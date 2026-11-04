use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pixie_drivers::hal::{DmaBuffer, Hal, PAGE_SIZE};
use pixie_drivers::net::e1000::{self, E1000Device, NUM_RX_DESC, NUM_TX_DESC, RxDesc, TxDesc};
use pixie_drivers::pci::PciDevice;

const MMIO_BASE: u64 = 0xFEB8_0000;
const DMA_PHYS_BASE: u64 = 0x3000_0000;
const DMA_POOL_PAGES: usize = 2048; // 8 MiB
const DMA_POOL_SIZE: usize = DMA_POOL_PAGES * PAGE_SIZE;

const CTRL_RST: u32 = 1 << 26;
const RCTL_EN: u32 = 1 << 1;
const TCTL_EN: u32 = 1 << 1;
const RXDCTL_ENABLE: u32 = 1 << 25;
const RFCTL_EXSTEN: u32 = 1 << 15;
const TX_STATUS_DD: u8 = 1 << 0;
const RX_STATUS_DD: u8 = 1 << 0;
const RX_STATUS_EOP: u8 = 1 << 1;

struct E1000SimState {
    dma_pool: Box<[u8]>,
    next_dma_page: usize,

    // Registers
    ctrl: u32,
    status: u32,
    eerd: u32,
    imc: u32,
    rctl: u32,
    tctl: u32,
    rdbal: u32,
    rdbah: u32,
    rdlen: u32,
    rdh: u32,
    rdt: u32,
    rxdctl: u32,
    tdbal: u32,
    tdbah: u32,
    tdlen: u32,
    tdh: u32,
    tdt: u32,
    txdctl: u32,
    rfctl: u32,
    ral0: u32,
    rah0: u32,
    mrqc: u32,
    manc: u32,
    fwsm: u32,
    fextnvm11: u32,

    eeprom_words: [u16; 64],

    // Quirk config & tracking
    pch_stuck_tdh: Option<u32>,
    auto_complete_tx: bool,
    ctrl_rst_asserted: bool,
    rxdctl_enabled_before_rctl_en: bool,
    tctl_disabled_after_enable: bool,
    tctl_was_enabled: bool,
    transmitted_packets: Vec<(usize, Vec<u8>)>, // (descriptor index, packet bytes)
    time_us: i64,
}

impl E1000SimState {
    fn new(ral0: u32, rah0: u32, pch_stuck_tdh: Option<u32>, auto_complete_tx: bool) -> Self {
        let initial_tdh = pch_stuck_tdh.unwrap_or(0);
        Self {
            dma_pool: vec![0u8; DMA_POOL_SIZE].into_boxed_slice(),
            next_dma_page: 0,

            ctrl: 1 << 6,   // SLU
            status: 1 << 1, // Link Up (LU)
            eerd: 0,
            imc: 0,
            rctl: 0,
            tctl: 0,
            rdbal: 0,
            rdbah: 0,
            rdlen: 0,
            rdh: 0,
            rdt: 0,
            rxdctl: 0,
            tdbal: 0,
            tdbah: 0,
            tdlen: 0,
            tdh: initial_tdh,
            tdt: initial_tdh,
            txdctl: 0,
            rfctl: RFCTL_EXSTEN, // Pre-set EXSTEN (bit 15) as UEFI UNDI often leaves it
            ral0,
            rah0,
            mrqc: 0x0000_0001, // Pre-set RSS bit
            manc: 1 << 13,     // Pre-set ARP_EN
            fwsm: 0,
            fextnvm11: 0,

            eeprom_words: [0xFFFF; 64],

            pch_stuck_tdh,
            auto_complete_tx,
            ctrl_rst_asserted: false,
            rxdctl_enabled_before_rctl_en: false,
            tctl_disabled_after_enable: false,
            tctl_was_enabled: false,
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

    fn tx_ring_base(&self) -> u64 {
        (self.tdbal as u64) | ((self.tdbah as u64) << 32)
    }

    fn rx_ring_base(&self) -> u64 {
        (self.rdbal as u64) | ((self.rdbah as u64) << 32)
    }

    fn read_tx_desc(&self, idx: usize) -> TxDesc {
        let addr = self.tx_ring_base() + (idx * core::mem::size_of::<TxDesc>()) as u64;
        let b = self.dma_slice(addr, 16);
        TxDesc {
            buffer_addr: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            length: u16::from_le_bytes(b[8..10].try_into().unwrap()),
            cso: b[10],
            cmd: b[11],
            status: b[12],
            css: b[13],
            special: u16::from_le_bytes(b[14..16].try_into().unwrap()),
        }
    }

    fn write_tx_desc_status(&mut self, idx: usize, status: u8) {
        let addr = self.tx_ring_base() + (idx * core::mem::size_of::<TxDesc>()) as u64;
        let b = self.dma_slice_mut(addr, 16);
        b[12] = status;
    }

    fn inject_rx_packet(&mut self, frame: &[u8]) {
        assert!(
            (self.rctl & RCTL_EN) != 0,
            "Cannot inject RX packet when RCTL.EN == 0"
        );
        let idx = self.rdh as usize;
        let desc_addr = self.rx_ring_base() + (idx * core::mem::size_of::<RxDesc>()) as u64;
        let mut raw = [0u8; 16];
        raw.copy_from_slice(self.dma_slice(desc_addr, 16));
        let buf_phys = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        assert_ne!(buf_phys, 0, "RX descriptor {} buffer_addr is 0", idx);

        self.dma_slice_mut(buf_phys, frame.len())
            .copy_from_slice(frame);
        let desc_mut = self.dma_slice_mut(desc_addr, 16);
        desc_mut[8..10].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        desc_mut[12] = RX_STATUS_DD | RX_STATUS_EOP;
        desc_mut[13] = 0; // errors = 0

        self.rdh = ((idx + 1) % NUM_RX_DESC) as u32;
    }

    fn process_tx_tail_write(&mut self, new_tdt: u32) {
        while self.tdh != new_tdt {
            let idx = self.tdh as usize;
            let desc = self.read_tx_desc(idx);
            assert_ne!(
                desc.buffer_addr, 0,
                "TX descriptor {} buffer_addr is 0",
                idx
            );
            assert!(desc.length > 0, "TX descriptor {} length is 0", idx);
            let pkt = self
                .dma_slice(desc.buffer_addr, desc.length as usize)
                .to_vec();
            self.transmitted_packets.push((idx, pkt));

            if self.auto_complete_tx {
                self.write_tx_desc_status(idx, desc.status | TX_STATUS_DD);
                self.tdh = ((idx + 1) % NUM_TX_DESC) as u32;
            } else {
                // Leave TDH where it is and do NOT set TX_STATUS_DD to simulate ring backpressure!
                break;
            }
        }
        self.tdt = new_tdt;
    }

    fn complete_one_pending_tx(&mut self) {
        let idx = self.tdh as usize;
        let desc = self.read_tx_desc(idx);
        let pkt = self
            .dma_slice(desc.buffer_addr, desc.length as usize)
            .to_vec();
        self.transmitted_packets.push((idx, pkt));
        self.write_tx_desc_status(idx, desc.status | TX_STATUS_DD);
        self.tdh = ((idx + 1) % NUM_TX_DESC) as u32;
    }

    fn mmio_read32(&self, addr: u64) -> u32 {
        let off = addr
            .checked_sub(MMIO_BASE)
            .expect("MMIO read below MMIO_BASE");
        match off {
            0x0000 => self.ctrl,
            0x0008 => self.status,
            0x0014 => self.eerd,
            0x00D8 => self.imc,
            0x0100 => self.rctl,
            0x0400 => self.tctl,
            0x2800 => self.rdbal,
            0x2804 => self.rdbah,
            0x2808 => self.rdlen,
            0x2810 => self.rdh,
            0x2818 => self.rdt,
            0x2828 => self.rxdctl,
            0x3800 => self.tdbal,
            0x3804 => self.tdbah,
            0x3808 => self.tdlen,
            0x3810 => self.tdh,
            0x3818 => self.tdt,
            0x3828 => self.txdctl,
            0x5008 => self.rfctl,
            0x5400 => self.ral0,
            0x5404 => self.rah0,
            0x5818 => self.mrqc,
            0x5820 => self.manc,
            0x5B54 => self.fwsm,
            0x5BBC => self.fextnvm11,
            _ => 0,
        }
    }

    fn mmio_write32(&mut self, addr: u64, val: u32) {
        let off = addr
            .checked_sub(MMIO_BASE)
            .expect("MMIO write below MMIO_BASE");
        match off {
            0x0000 => {
                if (val & CTRL_RST) != 0 {
                    self.ctrl_rst_asserted = true;
                }
                self.ctrl = val & !CTRL_RST;
            }
            0x0014 => {
                // EERD: bit 0 = START
                if (val & 1) != 0 {
                    let word_idx = if ((val >> 8) & 0xFF) != 0 {
                        ((val >> 8) & 0xFF) as usize
                    } else {
                        ((val >> 2) & 0x3F) as usize
                    };
                    let data = self.eeprom_words.get(word_idx).copied().unwrap_or(0xFFFF);
                    // Set both DONE bit 4 (8254x) and bit 1 (e1000e) so either poll mask succeeds
                    self.eerd = ((data as u32) << 16) | (1 << 4) | (1 << 1);
                } else {
                    self.eerd = val;
                }
            }
            0x00D8 => self.imc = val,
            0x0100 => {
                if (val & RCTL_EN) != 0
                    && (self.rctl & RCTL_EN) == 0
                    && (self.rxdctl & RXDCTL_ENABLE) != 0
                {
                    self.rxdctl_enabled_before_rctl_en = true;
                }
                self.rctl = val;
            }
            0x0400 => {
                if (val & TCTL_EN) != 0 {
                    self.tctl_was_enabled = true;
                } else if self.tctl_was_enabled {
                    self.tctl_disabled_after_enable = true;
                }
                self.tctl = val;
            }
            0x2800 => self.rdbal = val,
            0x2804 => self.rdbah = val,
            0x2808 => self.rdlen = val,
            0x2810 => self.rdh = val,
            0x2818 => {
                // Quirk 1: Real e1000e/I219 ignores non-zero RDT writes while RCTL.EN == 0!
                if val == 0 || (self.rctl & RCTL_EN) != 0 {
                    self.rdt = val;
                }
            }
            0x2828 => self.rxdctl = val,
            0x3800 => self.tdbal = val,
            0x3804 => self.tdbah = val,
            0x3808 => self.tdlen = val,
            0x3810 => {
                // Quirk 3: PCH I217/I219 ignores writes to TDH!
                if self.pch_stuck_tdh.is_none() {
                    self.tdh = val;
                }
            }
            0x3818 => {
                // Quirk 3: PCH ignores writes to TDT while TCTL.EN == 0!
                if self.pch_stuck_tdh.is_some() && (self.tctl & TCTL_EN) == 0 {
                    // ignored while TCTL.EN == 0
                } else if (self.tctl & TCTL_EN) != 0 {
                    self.process_tx_tail_write(val);
                } else {
                    self.tdt = val;
                }
            }
            0x3828 => self.txdctl = val,
            0x5008 => self.rfctl = val,
            0x5400 => self.ral0 = val,
            0x5404 => self.rah0 = val,
            0x5818 => self.mrqc = val,
            0x5820 => self.manc = val,
            0x5BBC => self.fextnvm11 = val,
            _ => {}
        }
    }
}

#[derive(Clone)]
struct MockE1000Hal {
    state: Arc<Mutex<E1000SimState>>,
}

impl MockE1000Hal {
    fn new(ral0: u32, rah0: u32, pch_stuck_tdh: Option<u32>, auto_complete_tx: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(E1000SimState::new(
                ral0,
                rah0,
                pch_stuck_tdh,
                auto_complete_tx,
            ))),
        }
    }
}

impl Hal for MockE1000Hal {
    fn mmio_read8(&self, addr: u64) -> u8 {
        let aligned = addr & !3;
        let shift = ((addr & 3) * 8) as u32;
        ((self.mmio_read32(aligned) >> shift) & 0xFF) as u8
    }
    fn mmio_write8(&self, _addr: u64, _val: u8) {}
    fn mmio_read16(&self, addr: u64) -> u16 {
        let aligned = addr & !3;
        let shift = ((addr & 2) * 8) as u32;
        ((self.mmio_read32(aligned) >> shift) & 0xFFFF) as u16
    }
    fn mmio_write16(&self, _addr: u64, _val: u16) {}
    fn mmio_read32(&self, addr: u64) -> u32 {
        self.state.lock().unwrap().mmio_read32(addr)
    }
    fn mmio_write32(&self, addr: u64, val: u32) {
        self.state.lock().unwrap().mmio_write32(addr, val);
    }
    fn mmio_read64(&self, addr: u64) -> u64 {
        let st = self.state.lock().unwrap();
        (st.mmio_read32(addr) as u64) | ((st.mmio_read32(addr + 4) as u64) << 32)
    }
    fn mmio_write64(&self, addr: u64, val: u64) {
        let mut st = self.state.lock().unwrap();
        st.mmio_write32(addr, (val & 0xFFFF_FFFF) as u32);
        st.mmio_write32(addr + 4, (val >> 32) as u32);
    }
    fn io_read8(&self, _port: u16) -> u8 {
        0
    }
    fn io_write8(&self, _port: u16, _val: u8) {}
    fn io_read16(&self, _port: u16) -> u16 {
        0
    }
    fn io_write16(&self, _port: u16, _val: u16) {}
    fn io_read32(&self, _port: u16) -> u32 {
        0
    }
    fn io_write32(&self, _port: u16, _val: u32) {}

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

fn make_pci(device_id: u16) -> PciDevice {
    PciDevice {
        bus: 0,
        dev: 25,
        func: 0,
        vendor_id: 0x8086,
        device_id,
        class_code: 0x02,
        subclass: 0x00,
        prog_if: 0x00,
    }
}

#[test]
fn test_e1000_probe_supported_devices() {
    assert!(e1000::probe(&make_pci(0x100E))); // 82540EM
    assert!(e1000::probe(&make_pci(0x10D3))); // 82574L
    assert!(e1000::probe(&make_pci(0x153A))); // I217-LM
    assert!(e1000::probe(&make_pci(0x15B8))); // I219-V
    assert!(!e1000::probe(&PciDevice {
        vendor_id: 0x10EC,
        device_id: 0x8168,
        ..make_pci(0x8168)
    }));
}

#[test]
fn test_e1000_quirk1_and_quirk2_rx_ordering_thresholds_and_rfctl_exsten() {
    // MAC 52:54:00:12:34:56 in RAL0/RAH0
    let ral0 = 0x1200_5452;
    let rah0 = 0x8000_5634;
    let hal = MockE1000Hal::new(ral0, rah0, None, true);

    let mut dev = E1000Device::new(make_pci(0x10D3), MMIO_BASE, None, hal.clone())
        .expect("E1000Device::new should succeed");

    assert_eq!(dev.mac_address(), [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    assert!(dev.is_link_up());

    {
        let st = hal.state.lock().unwrap();
        // Quirk 2: RFCTL.EXSTEN (bit 15) and MRQC must be cleared, and CTRL.RST must NOT be asserted
        assert_eq!(
            st.rfctl & RFCTL_EXSTEN,
            0,
            "Driver must clear RFCTL.EXSTEN (bit 15) left set by UEFI UNDI"
        );
        assert_eq!(
            st.mrqc, 0,
            "Driver must clear MRQC multi-queue RSS register"
        );
        assert!(
            !st.ctrl_rst_asserted,
            "Driver must not assert destructive CTRL.RST during init"
        );

        // Quirk 1: RXDCTL.ENABLE + non-zero PTHRESH/WTHRESH, RCTL.EN set, and RDT == NUM_RX_DESC - 1
        // (Remember: MockE1000Hal ignores writes to RDT > 0 while RCTL.EN == 0!)
        assert_ne!(
            st.rxdctl & RXDCTL_ENABLE,
            0,
            "RXDCTL.ENABLE (bit 25) must be set"
        );
        let pthresh = st.rxdctl & 0x3F;
        let wthresh = (st.rxdctl >> 16) & 0x3F;
        assert!(pthresh > 0, "RXDCTL.PTHRESH must be non-zero");
        assert!(wthresh > 0, "RXDCTL.WTHRESH must be non-zero");
        assert_ne!(st.rctl & RCTL_EN, 0, "RCTL.EN must be set");
        assert_eq!(
            st.rdt,
            (NUM_RX_DESC - 1) as u32,
            "RDT must be written to NUM_RX_DESC - 1 AFTER RCTL.EN is enabled"
        );
    }

    // Verify RX packet delivery
    let rx_frame = b"\xff\xff\xff\xff\xff\xff\x52\x54\x00\x12\x34\x56\x08\x06hello-e1000-rx-test";
    hal.state.lock().unwrap().inject_rx_packet(rx_frame);
    assert!(dev.has_packets());

    let mut buf = [0u8; 256];
    let n = dev.receive(&mut buf).expect("Expected RX packet");
    assert_eq!(&buf[..n], rx_frame);
    assert!(!dev.has_packets());
}

#[test]
fn test_e1000_quirk3_pch_nonzero_tdh_synced_after_tctl_en() {
    // Simulate Intel PCH I217-LM (0x153A) where TDH is stuck at 7 while TCTL.EN == 0
    let hal = MockE1000Hal::new(0x1200_5452, 0x8000_5634, Some(7), true);
    let mut dev = E1000Device::new(make_pci(0x153A), MMIO_BASE, None, hal.clone())
        .expect("E1000Device::new should succeed on PCH with non-zero TDH");

    let pkt = b"pch-tx-sync-at-descriptor-7";
    dev.transmit(pkt).expect("TX on PCH should succeed");

    let st = hal.state.lock().unwrap();
    assert_eq!(st.transmitted_packets.len(), 1);
    let (desc_idx, ref data) = st.transmitted_packets[0];
    assert_eq!(
        desc_idx, 7,
        "First transmitted packet on PCH with TDH=7 must use TX descriptor 7"
    );
    assert_eq!(data.as_slice(), pkt);
    assert_eq!(st.tdt, 8, "TDT should advance from 7 to 8");
}

#[test]
fn test_e1000_quirk4_tx_backpressure_without_destructive_reset() {
    // Disable auto_complete_tx so hardware does not set TX_STATUS_DD automatically
    let hal = MockE1000Hal::new(0x1200_5452, 0x8000_5634, None, false);
    let mut dev = E1000Device::new(make_pci(0x100E), MMIO_BASE, None, hal.clone())
        .expect("E1000Device::new should succeed");

    // Fill the TX ring until can_transmit() becomes false
    let mut sent = 0usize;
    for i in 0..(NUM_TX_DESC + 4) {
        if !dev.can_transmit() {
            break;
        }
        let pkt = [0xA0u8.wrapping_add(i as u8); 64];
        if dev.transmit(&pkt).is_err() {
            break;
        }
        sent += 1;
    }

    assert!(
        sent > 0 && sent <= NUM_TX_DESC,
        "Expected TX ring to accept up to NUM_TX_DESC packets before backpressure, sent={}",
        sent
    );
    assert!(
        !dev.can_transmit(),
        "can_transmit() must return false when all TX descriptors lack TX_STATUS_DD"
    );
    assert!(
        dev.transmit(b"should-fail-with-backpressure").is_err(),
        "transmit() must return Err when TX ring is full"
    );

    {
        let st = hal.state.lock().unwrap();
        assert!(
            !st.tctl_disabled_after_enable,
            "Driver MUST NOT disable TCTL.EN or reset the TX ring on backpressure"
        );
    }

    // Now complete one pending descriptor in hardware and verify transmission resumes
    hal.state.lock().unwrap().complete_one_pending_tx();
    assert!(
        dev.can_transmit(),
        "can_transmit() must return true once hardware sets TX_STATUS_DD on the oldest descriptor"
    );
    dev.transmit(b"resumed-after-backpressure")
        .expect("transmit() should succeed after descriptor completion");
}

#[test]
fn test_e1000_eeprom_mac_and_fallback_mac() {
    // RAL0 = 0, RAH0 = 0 -> read MAC 02:11:22:33:44:55 from EERD words 0..2
    let hal = MockE1000Hal::new(0, 0, None, true);
    {
        let mut st = hal.state.lock().unwrap();
        st.eeprom_words[0] = 0x1102;
        st.eeprom_words[1] = 0x3322;
        st.eeprom_words[2] = 0x5544;
    }
    let dev = E1000Device::new(make_pci(0x100E), MMIO_BASE, None, hal)
        .expect("E1000Device::new with EEPROM MAC should succeed");
    assert_eq!(dev.mac_address(), [0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);

    // Both RAL0/RAH0 == 0 and EEPROM == 0xFFFF -> use fallback_mac
    let hal_fb = MockE1000Hal::new(0, 0, None, true);
    let fb_mac = [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
    let dev_fb = E1000Device::new(make_pci(0x100E), MMIO_BASE, Some(fb_mac), hal_fb)
        .expect("E1000Device::new with fallback MAC should succeed");
    assert_eq!(dev_fb.mac_address(), fb_mac);
}
