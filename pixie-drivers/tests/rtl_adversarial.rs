use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pixie_drivers::hal::{DmaBuffer, Hal, PAGE_SIZE};
use pixie_drivers::net::rtl::{self, NUM_RX_DESC, NUM_TX_DESC, RtlDesc, RtlDevice, RtlVariant};
use pixie_drivers::pci::PciDevice;

const REG_BASE_MMIO: u64 = 0xFEBC_0000;
const REG_BASE_IO: u64 = 0xD000;
const DMA_PHYS_BASE: u64 = 0x4000_0000;
const DMA_POOL_PAGES: usize = 2048; // 8 MiB
const DMA_POOL_SIZE: usize = DMA_POOL_PAGES * PAGE_SIZE;

const DESC_OWN: u32 = 1 << 31;
const DESC_EOR: u32 = 1 << 30;
const DESC_FS: u32 = 1 << 29;
const DESC_LS: u32 = 1 << 28;

const CR_RST: u8 = 1 << 4;
const CR_RE: u8 = 1 << 3;
const CR_TE: u8 = 1 << 2;

struct RtlSimState {
    dma_pool: Box<[u8]>,
    next_dma_page: usize,
    reg_base: u64,

    // Register space (256 bytes)
    regs: [u8; 256],
    cr: u8,
    tcr: u32,
    rcr: u32,
    tnpds: u64,
    rdsar: u64,

    // Ring tracking
    tx_hw_idx: usize,
    rx_hw_idx: usize,

    // Quirk tracking
    tcr_ignored_before_cr_enable: usize,
    rcr_ignored_before_cr_enable: usize,
    doorbell_38_writes: Vec<u8>,
    doorbell_90_writes: Vec<u16>,
    transmitted_packets: Vec<Vec<u8>>,
    time_us: i64,
}

impl RtlSimState {
    fn new(reg_base: u64, mac: [u8; 6]) -> Self {
        let mut regs = [0u8; 256];
        regs[0..6].copy_from_slice(&mac);
        regs[0x6C] = 0x02; // PHYSTATUS: Link Up (bit 1)
        Self {
            dma_pool: vec![0u8; DMA_POOL_SIZE].into_boxed_slice(),
            next_dma_page: 0,
            reg_base,
            regs,
            cr: 0,
            tcr: 0,
            rcr: 0,
            tnpds: 0,
            rdsar: 0,
            tx_hw_idx: 0,
            rx_hw_idx: 0,
            tcr_ignored_before_cr_enable: 0,
            rcr_ignored_before_cr_enable: 0,
            doorbell_38_writes: Vec::new(),
            doorbell_90_writes: Vec::new(),
            transmitted_packets: Vec::new(),
            time_us: 1_000,
        }
    }

    fn dma_slice(&self, phys: u64, len: usize) -> &[u8] {
        assert!(phys >= DMA_PHYS_BASE, "DMA phys {:#x} below base", phys);
        let off = (phys - DMA_PHYS_BASE) as usize;
        assert!(off + len <= self.dma_pool.len(), "DMA range out of bounds");
        &self.dma_pool[off..off + len]
    }

    fn dma_slice_mut(&mut self, phys: u64, len: usize) -> &mut [u8] {
        assert!(phys >= DMA_PHYS_BASE, "DMA phys {:#x} below base", phys);
        let off = (phys - DMA_PHYS_BASE) as usize;
        assert!(off + len <= self.dma_pool.len(), "DMA range out of bounds");
        &mut self.dma_pool[off..off + len]
    }

    fn read_desc(&self, ring_base: u64, idx: usize) -> RtlDesc {
        let addr = ring_base + (idx * core::mem::size_of::<RtlDesc>()) as u64;
        let b = self.dma_slice(addr, 16);
        RtlDesc {
            opts1: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            opts2: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            buf_addr_low: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            buf_addr_high: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        }
    }

    fn write_desc_opts1(&mut self, ring_base: u64, idx: usize, opts1: u32) {
        let addr = ring_base + (idx * core::mem::size_of::<RtlDesc>()) as u64;
        let b = self.dma_slice_mut(addr, 16);
        b[0..4].copy_from_slice(&opts1.to_le_bytes());
    }

    fn process_tx_doorbell(&mut self) {
        assert_ne!(self.tnpds, 0, "TNPDS not programmed before TX doorbell");
        assert_eq!(
            self.tnpds & 0xFF,
            0,
            "TNPDS {:#x} must be 256-byte aligned",
            self.tnpds
        );

        for _ in 0..NUM_TX_DESC {
            let idx = self.tx_hw_idx;
            let desc = self.read_desc(self.tnpds, idx);
            if (desc.opts1 & DESC_OWN) == 0 {
                break;
            }
            if idx == NUM_TX_DESC - 1 {
                assert_ne!(
                    desc.opts1 & DESC_EOR,
                    0,
                    "TX descriptor {} (last entry) MUST preserve DESC_EOR (bit 30)",
                    idx
                );
            }
            let len = (desc.opts1 & 0x3FFF) as usize;
            let buf_phys = (desc.buf_addr_low as u64) | ((desc.buf_addr_high as u64) << 32);
            let pkt = self.dma_slice(buf_phys, len).to_vec();
            self.transmitted_packets.push(pkt);

            // Clear DESC_OWN while preserving DESC_EOR
            let new_opts1 = desc.opts1 & !DESC_OWN;
            self.write_desc_opts1(self.tnpds, idx, new_opts1);

            self.tx_hw_idx = if (desc.opts1 & DESC_EOR) != 0 {
                0
            } else {
                idx + 1
            };
        }
    }

    /// Delivers a frame into the next RX descriptor, appending a 4-byte Ethernet FCS
    /// just like real Realtek RTL8168/RTL8125 hardware does (`raw_len = frame.len() + 4`).
    fn inject_rx_frame_with_fcs(&mut self, frame: &[u8]) {
        assert_ne!(self.rdsar, 0, "RDSAR not programmed before RX injection");
        assert_eq!(
            self.rdsar & 0xFF,
            0,
            "RDSAR {:#x} must be 256-byte aligned",
            self.rdsar
        );

        let idx = self.rx_hw_idx;
        let desc = self.read_desc(self.rdsar, idx);
        assert_ne!(
            desc.opts1 & DESC_OWN,
            0,
            "RX descriptor {} must have DESC_OWN set before hardware can write into it",
            idx
        );
        if idx == NUM_RX_DESC - 1 {
            assert_ne!(
                desc.opts1 & DESC_EOR,
                0,
                "RX descriptor {} (last entry) MUST preserve DESC_EOR (bit 30)",
                idx
            );
        }

        let buf_phys = (desc.buf_addr_low as u64) | ((desc.buf_addr_high as u64) << 32);
        let raw_len = frame.len() + 4; // + 4-byte Ethernet CRC32 FCS
        {
            let dst = self.dma_slice_mut(buf_phys, raw_len);
            dst[..frame.len()].copy_from_slice(frame);
            dst[frame.len()..raw_len].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // synthetic FCS
        }

        let eor = desc.opts1 & DESC_EOR;
        let new_opts1 = eor | DESC_FS | DESC_LS | ((raw_len as u32) & 0x3FFF);
        self.write_desc_opts1(self.rdsar, idx, new_opts1);

        self.rx_hw_idx = if eor != 0 { 0 } else { idx + 1 };
    }

    fn reg_read8(&self, off: usize) -> u8 {
        match off {
            0x37 => self.cr,
            o if o < self.regs.len() => self.regs[o],
            _ => 0,
        }
    }

    fn reg_write8(&mut self, off: usize, val: u8) {
        if off < self.regs.len() {
            self.regs[off] = val;
        }
        match off {
            0x37 => {
                if (val & CR_RST) != 0 {
                    // Software reset clears CR_RST, CR_TE, CR_RE, TCR, and RCR
                    self.cr = 0;
                    self.tcr = 0;
                    self.rcr = 0;
                } else {
                    self.cr = val;
                }
            }
            0x38 => {
                // RTL8169/8168 TPPOLL doorbell
                self.doorbell_38_writes.push(val);
                if (val & 0x40) != 0 {
                    self.process_tx_doorbell();
                }
            }
            0x90 => {
                // RTL8125 TPPOLL doorbell (8-bit write)
                self.doorbell_90_writes.push(val as u16);
                if (val & 0x01) != 0 {
                    self.process_tx_doorbell();
                }
            }
            _ => {}
        }
    }

    fn reg_read16(&self, off: usize) -> u16 {
        (self.reg_read8(off) as u16) | ((self.reg_read8(off + 1) as u16) << 8)
    }

    fn reg_write16(&mut self, off: usize, val: u16) {
        if off == 0x90 {
            self.doorbell_90_writes.push(val);
            if (val & 0x0001) != 0 {
                self.process_tx_doorbell();
            }
            return;
        }
        self.reg_write8(off, (val & 0xFF) as u8);
        self.reg_write8(off + 1, (val >> 8) as u8);
    }

    fn reg_read32(&self, off: usize) -> u32 {
        match off {
            0x20 => (self.tnpds & 0xFFFF_FFFF) as u32,
            0x24 => (self.tnpds >> 32) as u32,
            0x40 => self.tcr,
            0x44 => self.rcr,
            0xE4 => (self.rdsar & 0xFFFF_FFFF) as u32,
            0xE8 => (self.rdsar >> 32) as u32,
            _ => {
                (self.reg_read8(off) as u32)
                    | ((self.reg_read8(off + 1) as u32) << 8)
                    | ((self.reg_read8(off + 2) as u32) << 16)
                    | ((self.reg_read8(off + 3) as u32) << 24)
            }
        }
    }

    fn reg_write32(&mut self, off: usize, val: u32) {
        match off {
            0x20 => self.tnpds = (self.tnpds & 0xFFFF_FFFF_0000_0000) | (val as u64),
            0x24 => self.tnpds = (self.tnpds & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32),
            0x40 => {
                // Quirk 1: Realtek hardware ignores writes to TCR while CR_TE | CR_RE is not enabled!
                if (self.cr & (CR_TE | CR_RE)) == (CR_TE | CR_RE) {
                    self.tcr = val;
                } else {
                    self.tcr_ignored_before_cr_enable += 1;
                }
            }
            0x44 => {
                // Quirk 1: Realtek hardware ignores writes to RCR while CR_TE | CR_RE is not enabled!
                if (self.cr & (CR_TE | CR_RE)) == (CR_TE | CR_RE) {
                    self.rcr = val;
                } else {
                    self.rcr_ignored_before_cr_enable += 1;
                }
            }
            0xE4 => self.rdsar = (self.rdsar & 0xFFFF_FFFF_0000_0000) | (val as u64),
            0xE8 => self.rdsar = (self.rdsar & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32),
            _ => {
                let bytes = val.to_le_bytes();
                for (i, b) in bytes.iter().enumerate() {
                    if off + i < self.regs.len() {
                        self.regs[off + i] = *b;
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct MockRtlHal {
    state: Arc<Mutex<RtlSimState>>,
}

impl MockRtlHal {
    fn new(reg_base: u64, mac: [u8; 6]) -> Self {
        Self {
            state: Arc::new(Mutex::new(RtlSimState::new(reg_base, mac))),
        }
    }
}

impl Hal for MockRtlHal {
    fn mmio_read8(&self, addr: u64) -> u8 {
        let st = self.state.lock().unwrap();
        st.reg_read8((addr - st.reg_base) as usize)
    }
    fn mmio_write8(&self, addr: u64, val: u8) {
        let mut st = self.state.lock().unwrap();
        let off = (addr - st.reg_base) as usize;
        st.reg_write8(off, val);
    }
    fn mmio_read16(&self, addr: u64) -> u16 {
        let st = self.state.lock().unwrap();
        st.reg_read16((addr - st.reg_base) as usize)
    }
    fn mmio_write16(&self, addr: u64, val: u16) {
        let mut st = self.state.lock().unwrap();
        let off = (addr - st.reg_base) as usize;
        st.reg_write16(off, val);
    }
    fn mmio_read32(&self, addr: u64) -> u32 {
        let st = self.state.lock().unwrap();
        st.reg_read32((addr - st.reg_base) as usize)
    }
    fn mmio_write32(&self, addr: u64, val: u32) {
        let mut st = self.state.lock().unwrap();
        let off = (addr - st.reg_base) as usize;
        st.reg_write32(off, val);
    }
    fn mmio_read64(&self, addr: u64) -> u64 {
        let st = self.state.lock().unwrap();
        let off = (addr - st.reg_base) as usize;
        (st.reg_read32(off) as u64) | ((st.reg_read32(off + 4) as u64) << 32)
    }
    fn mmio_write64(&self, addr: u64, val: u64) {
        let mut st = self.state.lock().unwrap();
        let off = (addr - st.reg_base) as usize;
        st.reg_write32(off, (val & 0xFFFF_FFFF) as u32);
        st.reg_write32(off + 4, (val >> 32) as u32);
    }

    fn io_read8(&self, port: u16) -> u8 {
        let st = self.state.lock().unwrap();
        let Some(off) = (port as u64).checked_sub(st.reg_base) else {
            return 0;
        };
        st.reg_read8(off as usize)
    }
    fn io_write8(&self, port: u16, val: u8) {
        let mut st = self.state.lock().unwrap();
        if let Some(off) = (port as u64).checked_sub(st.reg_base) {
            st.reg_write8(off as usize, val);
        }
    }
    fn io_read16(&self, port: u16) -> u16 {
        let st = self.state.lock().unwrap();
        let Some(off) = (port as u64).checked_sub(st.reg_base) else {
            return 0;
        };
        st.reg_read16(off as usize)
    }
    fn io_write16(&self, port: u16, val: u16) {
        let mut st = self.state.lock().unwrap();
        if let Some(off) = (port as u64).checked_sub(st.reg_base) {
            st.reg_write16(off as usize, val);
        }
    }
    fn io_read32(&self, port: u16) -> u32 {
        let st = self.state.lock().unwrap();
        let Some(off) = (port as u64).checked_sub(st.reg_base) else {
            return 0;
        };
        st.reg_read32(off as usize)
    }
    fn io_write32(&self, port: u16, val: u32) {
        let mut st = self.state.lock().unwrap();
        if let Some(off) = (port as u64).checked_sub(st.reg_base) {
            st.reg_write32(off as usize, val);
        }
    }

    fn alloc_dma(&self, pages: usize) -> Option<DmaBuffer> {
        let mut st = self.state.lock().unwrap();
        if st.next_dma_page + pages > DMA_POOL_PAGES {
            return None;
        }
        let offset = st.next_dma_page * PAGE_SIZE;
        let size = pages * PAGE_SIZE;
        st.next_dma_page += pages;
        st.dma_pool[offset..offset + size].fill(0);
        let virt = unsafe { st.dma_pool.as_mut_ptr().add(offset) };
        let phys = DMA_PHYS_BASE + offset as u64;
        Some(DmaBuffer { virt, phys, size })
    }

    fn free_dma(&self, _buf: DmaBuffer) {}

    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        let mut st = self.state.lock().unwrap();
        let offset = (phys - DMA_PHYS_BASE) as usize;
        unsafe { st.dma_pool.as_mut_ptr().add(offset) }
    }

    fn micros(&self) -> i64 {
        let mut st = self.state.lock().unwrap();
        st.time_us += 10;
        st.time_us
    }

    fn yield_now(&self) -> impl Future<Output = ()> {
        core::future::ready(())
    }

    fn sleep(&self, _duration: Duration) -> impl Future<Output = ()> {
        core::future::ready(())
    }
}

fn make_rtl_pci(device_id: u16) -> PciDevice {
    PciDevice {
        bus: 2,
        dev: 0,
        func: 0,
        vendor_id: 0x10EC,
        device_id,
        class_code: 0x02,
        subclass: 0x00,
        prog_if: 0x00,
    }
}

#[test]
fn test_rtl_probe_variants() {
    assert_eq!(rtl::probe(&make_rtl_pci(0x8168)), Some(RtlVariant::Rtl8169));
    assert_eq!(rtl::probe(&make_rtl_pci(0x8169)), Some(RtlVariant::Rtl8169));
    assert_eq!(rtl::probe(&make_rtl_pci(0x8136)), Some(RtlVariant::Rtl8169));
    assert_eq!(rtl::probe(&make_rtl_pci(0x8125)), Some(RtlVariant::Rtl8125));
    assert_eq!(rtl::probe(&make_rtl_pci(0x8126)), Some(RtlVariant::Rtl8125));
    assert_eq!(
        rtl::probe(&PciDevice {
            vendor_id: 0x8086,
            device_id: 0x100E,
            ..make_rtl_pci(0x100E)
        }),
        None
    );
}

#[test]
fn test_rtl_quirk1_cr_enable_before_tcr_rcr_and_quirk2_doorbells() {
    let mac_8168 = [0x00, 0xE0, 0x4C, 0x68, 0x00, 0x01];
    let hal_8168 = MockRtlHal::new(REG_BASE_MMIO, mac_8168);
    let mut dev_8168 = RtlDevice::new(
        make_rtl_pci(0x8168),
        REG_BASE_MMIO,
        false,
        None,
        hal_8168.clone(),
    )
    .expect("RtlDevice::new (RTL8168) should succeed");

    assert_eq!(dev_8168.variant(), RtlVariant::Rtl8169);
    assert_eq!(dev_8168.mac_address(), mac_8168);
    assert!(dev_8168.is_link_up());

    {
        let st = hal_8168.state.lock().unwrap();
        assert_eq!(
            st.cr & (CR_TE | CR_RE),
            CR_TE | CR_RE,
            "CR_TE | CR_RE (0x0C) must be enabled in REG_CR (0x37)"
        );
        assert_eq!(
            st.tcr_ignored_before_cr_enable, 0,
            "Driver must enable CR_TE | CR_RE BEFORE writing REG_TCR (0x40)"
        );
        assert_eq!(
            st.rcr_ignored_before_cr_enable, 0,
            "Driver must enable CR_TE | CR_RE BEFORE writing REG_RCR (0x44)"
        );
        assert_ne!(
            st.tcr, 0,
            "REG_TCR (0x40) must be configured after CR_TE | CR_RE"
        );
        assert_ne!(
            st.rcr & 0x0F,
            0,
            "REG_RCR (0x44) must have packet accept bits configured after CR_TE | CR_RE"
        );
    }

    // Quirk 2: RTL8168 must ring doorbell 0x38 (0x40) and NOT 0x90
    dev_8168
        .transmit(b"rtl8168-test-frame")
        .expect("RTL8168 transmit should succeed");
    {
        let st = hal_8168.state.lock().unwrap();
        assert_eq!(st.doorbell_38_writes, vec![0x40]);
        assert!(st.doorbell_90_writes.is_empty());
        assert_eq!(st.transmitted_packets.len(), 1);
        assert_eq!(st.transmitted_packets[0], b"rtl8168-test-frame");
    }

    // Quirk 2: RTL8125 (also testing Port I/O mode is_io = true) must ring doorbell 0x90 (0x0001) and NOT 0x38
    let mac_8125 = [0x00, 0xE0, 0x4C, 0x25, 0x00, 0x02];
    let hal_8125 = MockRtlHal::new(REG_BASE_IO, mac_8125);
    let mut dev_8125 = RtlDevice::new(
        make_rtl_pci(0x8125),
        REG_BASE_IO,
        true,
        None,
        hal_8125.clone(),
    )
    .expect("RtlDevice::new (RTL8125) should succeed");

    assert_eq!(dev_8125.variant(), RtlVariant::Rtl8125);
    assert_eq!(dev_8125.mac_address(), mac_8125);

    dev_8125
        .transmit(b"rtl8125-test-frame")
        .expect("RTL8125 transmit should succeed");
    {
        let st = hal_8125.state.lock().unwrap();
        assert_eq!(st.doorbell_90_writes, vec![0x0001]);
        assert!(st.doorbell_38_writes.is_empty());
        assert_eq!(st.transmitted_packets.len(), 1);
        assert_eq!(st.transmitted_packets[0], b"rtl8125-test-frame");
    }
}

#[test]
fn test_rtl_quirk3_fcs_stripping_and_ring_wraparound_desc_eor() {
    let mac = [0x00, 0xE0, 0x4C, 0x11, 0x22, 0x33];
    let hal = MockRtlHal::new(REG_BASE_MMIO, mac);
    let mut dev = RtlDevice::new(
        make_rtl_pci(0x8125),
        REG_BASE_MMIO,
        false,
        None,
        hal.clone(),
    )
    .expect("RtlDevice::new should succeed");

    // Verify initial DESC_EOR on final RX (255) and TX (127) descriptors
    {
        let st = hal.state.lock().unwrap();
        let last_rx = st.read_desc(st.rdsar, NUM_RX_DESC - 1);
        let last_tx = st.read_desc(st.tnpds, NUM_TX_DESC - 1);
        assert_ne!(
            last_rx.opts1 & DESC_EOR,
            0,
            "Initial RX descriptor {} must have DESC_EOR set",
            NUM_RX_DESC - 1
        );
        assert_ne!(
            last_tx.opts1 & DESC_EOR,
            0,
            "Initial TX descriptor {} must have DESC_EOR set",
            NUM_TX_DESC - 1
        );
    }

    // Quirk 3 + Ring Wrap-around: Transmit and receive 265 packets (> NUM_RX_DESC = 256 and > 2 * NUM_TX_DESC = 256)
    let total_packets = 265usize;
    for i in 0..total_packets {
        let mut frame = vec![0u8; 64];
        frame[0..6].copy_from_slice(&[0xFF; 6]);
        frame[6..12].copy_from_slice(&mac);
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        for (j, b) in frame[14..].iter_mut().enumerate() {
            *b = ((i * 17 + j * 3) & 0xFF) as u8;
        }

        // Transmit
        assert!(dev.can_transmit());
        dev.transmit(&frame)
            .expect("TX should succeed across ring wrap");

        // Inject RX with 4-byte FCS appended (raw_len = 68 in opts1)
        hal.state.lock().unwrap().inject_rx_frame_with_fcs(&frame);
        assert!(dev.has_packets());

        let mut rx_buf = [0u8; 128];
        let rx_len = dev.receive(&mut rx_buf).expect("Expected RX frame");
        // Quirk 3 assertion: 4-byte Ethernet FCS must be stripped (68 -> 64)!
        assert_eq!(
            rx_len, 64,
            "receive() must strip 4-byte Ethernet FCS on packet {}",
            i
        );
        assert_eq!(&rx_buf[..rx_len], &frame[..]);
        assert!(!dev.has_packets());
    }

    // Verify DESC_EOR is still preserved on the final descriptors after wrapping both rings!
    let st = hal.state.lock().unwrap();
    assert_eq!(st.transmitted_packets.len(), total_packets);
    let last_rx = st.read_desc(st.rdsar, NUM_RX_DESC - 1);
    let last_tx = st.read_desc(st.tnpds, NUM_TX_DESC - 1);
    assert_ne!(
        last_rx.opts1 & DESC_EOR,
        0,
        "RX descriptor {} must still have DESC_EOR set after ring wrap-around",
        NUM_RX_DESC - 1
    );
    assert_ne!(
        last_tx.opts1 & DESC_EOR,
        0,
        "TX descriptor {} must still have DESC_EOR set after ring wrap-around",
        NUM_TX_DESC - 1
    );
}
