use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pixie_drivers::hal::{DmaBuffer, Hal, PAGE_SIZE};
use pixie_drivers::net::e1000;
use pixie_drivers::net::igc::{self, IgcDevice, NUM_RX_DESC, NUM_TX_DESC, RxDesc, TxDesc};
use pixie_drivers::net::usb_net::{
    BULK_BUFFER_SIZE, UsbEthernetDevice, UsbNetProtocol, decode_cdc_ncm_rx_frame,
    decode_rtl8153_rx_frame, encode_cdc_ncm_tx_frame, encode_rtl8153_tx_frame,
};
use pixie_drivers::pci::PciDevice;

// ============================================================================
// 1. Intel Foxville `igc` (I225 / I226) 2.5GbE Adversarial Simulator & Tests
// ============================================================================

const IGC_MMIO_BASE: u64 = 0xFEBC_0000;
const IGC_DMA_BASE: u64 = 0x4000_0000;
const IGC_DMA_PAGES: usize = 2048;

struct IgcSimState {
    dma_pool: Box<[u8]>,
    next_dma_page: usize,

    ctrl: u32,
    ctrl_ext: u32,
    status: u32,
    imc: u32,
    eimc: u32,
    rctl: u32,
    tctl: u32,
    rfctl: u32,
    mrqc: u32,
    ral0: u32,
    rah0: u32,

    // Queue 0 RX (0xC000) & TX (0xE000)
    rdbal0: u32,
    rdbah0: u32,
    rdlen0: u32,
    srrctl0: u32,
    rdh0: u32,
    rdt0: u32,
    rxdctl0: u32,

    tdbal0: u32,
    tdbah0: u32,
    tdlen0: u32,
    tdh0: u32,
    tdt0: u32,
    txdctl0: u32,

    ctrl_rst_or_dev_rst_asserted: bool,
    rxdctl0_enabled_before_rctl_en: bool,
    transmitted_packets: Vec<Vec<u8>>,
    time_us: i64,
}

impl IgcSimState {
    fn new(ral0: u32, rah0: u32) -> Self {
        Self {
            dma_pool: vec![0u8; IGC_DMA_PAGES * PAGE_SIZE].into_boxed_slice(),
            next_dma_page: 0,
            ctrl: 1 << 6, // SLU
            ctrl_ext: 0,
            status: 1 << 1, // LU
            imc: 0,
            eimc: 0,
            rctl: 0,
            tctl: 0,
            rfctl: 1 << 15, // EXSTEN pre-set by firmware
            mrqc: 1,        // RSS pre-set
            ral0,
            rah0,
            rdbal0: 0,
            rdbah0: 0,
            rdlen0: 0,
            srrctl0: 0,
            rdh0: 0,
            rdt0: 0,
            rxdctl0: 0,
            tdbal0: 0,
            tdbah0: 0,
            tdlen0: 0,
            tdh0: 0,
            tdt0: 0,
            txdctl0: 0,
            ctrl_rst_or_dev_rst_asserted: false,
            rxdctl0_enabled_before_rctl_en: false,
            transmitted_packets: Vec::new(),
            time_us: 1_000,
        }
    }

    fn dma_slice(&self, phys: u64, len: usize) -> &[u8] {
        let off = (phys - IGC_DMA_BASE) as usize;
        &self.dma_pool[off..off + len]
    }

    fn dma_slice_mut(&mut self, phys: u64, len: usize) -> &mut [u8] {
        let off = (phys - IGC_DMA_BASE) as usize;
        &mut self.dma_pool[off..off + len]
    }

    fn rx_ring_base(&self) -> u64 {
        (self.rdbal0 as u64) | ((self.rdbah0 as u64) << 32)
    }

    fn tx_ring_base(&self) -> u64 {
        (self.tdbal0 as u64) | ((self.tdbah0 as u64) << 32)
    }

    fn inject_rx_packet(&mut self, frame: &[u8]) {
        assert_ne!(self.rctl & (1 << 1), 0, "RCTL.EN must be set");
        let idx = self.rdh0 as usize;
        let desc_addr = self.rx_ring_base() + (idx * core::mem::size_of::<RxDesc>()) as u64;
        let mut raw = [0u8; 16];
        raw.copy_from_slice(self.dma_slice(desc_addr, 16));
        let buf_phys = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        self.dma_slice_mut(buf_phys, frame.len())
            .copy_from_slice(frame);
        let desc_mut = self.dma_slice_mut(desc_addr, 16);
        desc_mut[8..10].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        desc_mut[12] = 0x03; // DD | EOP
        desc_mut[13] = 0;
        self.rdh0 = ((idx + 1) % NUM_RX_DESC) as u32;
    }

    fn process_tx_tail(&mut self, new_tdt: u32) {
        while self.tdh0 != new_tdt {
            let idx = self.tdh0 as usize;
            let desc_addr = self.tx_ring_base() + (idx * core::mem::size_of::<TxDesc>()) as u64;
            let b = self.dma_slice(desc_addr, 16);
            let buf_phys = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let len = u16::from_le_bytes(b[8..10].try_into().unwrap()) as usize;
            let pkt = self.dma_slice(buf_phys, len).to_vec();
            self.transmitted_packets.push(pkt);
            self.dma_slice_mut(desc_addr, 16)[12] |= 1; // Set DD
            self.tdh0 = ((idx + 1) % NUM_TX_DESC) as u32;
        }
        self.tdt0 = new_tdt;
    }

    fn mmio_read32(&self, addr: u64) -> u32 {
        let off = addr
            .checked_sub(IGC_MMIO_BASE)
            .expect("IGC MMIO below base");
        match off {
            0x0000 => self.ctrl,
            0x0008 => self.status,
            0x0018 => self.ctrl_ext,
            0x0100 => self.rctl,
            0x0400 => self.tctl,
            0x1508 => self.imc,
            0x1528 => self.eimc,
            0x5008 => self.rfctl,
            0x5400 => self.ral0,
            0x5404 => self.rah0,
            0x5818 => self.mrqc,
            0xC000 => self.rdbal0,
            0xC004 => self.rdbah0,
            0xC008 => self.rdlen0,
            0xC00C => self.srrctl0,
            0xC010 => self.rdh0,
            0xC018 => self.rdt0,
            0xC028 => self.rxdctl0,
            0xE000 => self.tdbal0,
            0xE004 => self.tdbah0,
            0xE008 => self.tdlen0,
            0xE010 => self.tdh0,
            0xE018 => self.tdt0,
            0xE028 => self.txdctl0,
            _ => 0,
        }
    }

    fn mmio_write32(&mut self, addr: u64, val: u32) {
        let off = addr
            .checked_sub(IGC_MMIO_BASE)
            .expect("IGC MMIO below base");
        match off {
            0x0000 => {
                if (val & ((1 << 26) | (1 << 29))) != 0 {
                    self.ctrl_rst_or_dev_rst_asserted = true;
                }
                self.ctrl = val;
            }
            0x0018 => self.ctrl_ext = val,
            0x0100 => {
                if (val & (1 << 1)) != 0 && (self.rxdctl0 & (1 << 25)) != 0 {
                    self.rxdctl0_enabled_before_rctl_en = true;
                }
                self.rctl = val;
            }
            0x0400 => self.tctl = val,
            0x1508 => self.imc = val,
            0x1528 => self.eimc = val,
            0x5008 => self.rfctl = val,
            0x5400 => self.ral0 = val,
            0x5404 => self.rah0 = val,
            0x5818 => self.mrqc = val,
            0xC000 => self.rdbal0 = val,
            0xC004 => self.rdbah0 = val,
            0xC008 => self.rdlen0 = val,
            0xC00C => self.srrctl0 = val,
            0xC010 => self.rdh0 = val,
            0xC018 => {
                if val == 0 || (self.rctl & (1 << 1)) != 0 {
                    self.rdt0 = val;
                }
            }
            0xC028 => self.rxdctl0 = val,
            0xE000 => self.tdbal0 = val,
            0xE004 => self.tdbah0 = val,
            0xE008 => self.tdlen0 = val,
            0xE010 => self.tdh0 = val,
            0xE018 => {
                if (self.tctl & (1 << 1)) != 0 {
                    self.process_tx_tail(val);
                } else {
                    self.tdt0 = val;
                }
            }
            0xE028 => self.txdctl0 = val,
            _ => {}
        }
    }
}

#[derive(Clone)]
struct MockIgcHal {
    state: Arc<Mutex<IgcSimState>>,
}

impl MockIgcHal {
    fn new(ral0: u32, rah0: u32) -> Self {
        Self {
            state: Arc::new(Mutex::new(IgcSimState::new(ral0, rah0))),
        }
    }
}

impl Hal for MockIgcHal {
    fn mmio_read8(&self, _addr: u64) -> u8 {
        0
    }
    fn mmio_write8(&self, _addr: u64, _val: u8) {}
    fn mmio_read16(&self, _addr: u64) -> u16 {
        0
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
        st.mmio_write32(addr, val as u32);
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
        if st.next_dma_page + pages > IGC_DMA_PAGES {
            return None;
        }
        let off = st.next_dma_page * PAGE_SIZE;
        let size = pages * PAGE_SIZE;
        st.next_dma_page += pages;
        st.dma_pool[off..off + size].fill(0);
        let virt = unsafe { st.dma_pool.as_mut_ptr().add(off) };
        let phys = IGC_DMA_BASE + off as u64;
        Some(DmaBuffer { virt, phys, size })
    }
    fn free_dma(&self, _buf: DmaBuffer) {}
    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        let mut st = self.state.lock().unwrap();
        let off = (phys - IGC_DMA_BASE) as usize;
        unsafe { st.dma_pool.as_mut_ptr().add(off) }
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

fn make_intel_pci(device_id: u16) -> PciDevice {
    PciDevice {
        bus: 0,
        dev: 0,
        func: 0,
        vendor_id: 0x8086,
        device_id,
        class_code: 0x02,
        subclass: 0x00,
        prog_if: 0x00,
    }
}

#[test]
fn test_igc_probe_and_e1000_disjointness() {
    for &id in &[0x15F2, 0x15F3, 0x125B, 0x125C, 0x125D] {
        let pci = make_intel_pci(id);
        assert!(igc::probe(&pci), "igc::probe must match {:#06x}", id);
        assert!(
            !e1000::probe(&pci),
            "e1000::probe must not claim igc device {:#06x}",
            id
        );
    }
}

#[test]
fn test_igc_foxville_queue0_init_rx_and_tx() {
    let ral0 = 0x1200_5452;
    let rah0 = 0x8000_5634;
    let hal = MockIgcHal::new(ral0, rah0);
    let mut dev = IgcDevice::new(make_intel_pci(0x125C), IGC_MMIO_BASE, None, hal.clone())
        .expect("IgcDevice::new should succeed for I226-V");

    assert_eq!(dev.mac_address(), [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    assert!(dev.is_link_up());

    {
        let st = hal.state.lock().unwrap();
        assert!(
            !st.ctrl_rst_or_dev_rst_asserted,
            "IgcDevice must not assert destructive CTRL.RST or CTRL.DEV_RST"
        );
        assert_ne!(
            st.ctrl_ext & (1 << 28),
            0,
            "IgcDevice must set CTRL_EXT.DRV_LOAD"
        );
        assert_eq!(st.rfctl & (1 << 15), 0, "RFCTL.EXSTEN must be cleared");
        assert_eq!(st.mrqc, 0, "MRQC must be cleared");
        assert_eq!(
            st.srrctl0 & 0x7F,
            2,
            "SRRCTL0 BSIZEPKT must be set to 2 (2 KiB)"
        );
        assert!(
            st.rxdctl0_enabled_before_rctl_en,
            "RXDCTL0.ENABLE must be enabled before RCTL.EN"
        );
        assert_eq!(
            st.rdt0,
            (NUM_RX_DESC - 1) as u32,
            "RDT0 (0xC018) must be written to NUM_RX_DESC - 1 after RCTL.EN"
        );
    }

    // Test TX on Queue 0 (0xE018)
    let tx_pkt = b"hello-intel-i226-foxville-2.5gbe";
    dev.transmit(tx_pkt).expect("IgcDevice TX should succeed");
    {
        let st = hal.state.lock().unwrap();
        assert_eq!(st.transmitted_packets, vec![tx_pkt.to_vec()]);
    }

    // Test RX on Queue 0 (0xC010 / 0xC018)
    let rx_pkt = b"\xff\xff\xff\xff\xff\xff\x52\x54\x00\x12\x34\x56\x08\x00foxville-rx-frame";
    hal.state.lock().unwrap().inject_rx_packet(rx_pkt);
    assert!(dev.has_packets());
    let mut rx_buf = [0u8; 256];
    let n = dev.receive(&mut rx_buf).expect("Expected RX frame");
    assert_eq!(&rx_buf[..n], rx_pkt);
}

// ============================================================================
// 2. USB Ethernet Dongle (RTL8153 / CDC-ECM / CDC-NCM) Framing & xHCI Tests
// ============================================================================

#[test]
fn test_usb_ethernet_rtl8153_and_cdc_ncm_framing_codecs() {
    let eth_frame =
        b"\xff\xff\xff\xff\xff\xff\x00\xe0\x4c\x68\x01\x02\x08\x06arp-payload-test-1234";

    // 1. RTL8153 TX framing (8-byte header + frame)
    let mut rtl_tx = [0u8; 256];
    let tx_len = encode_rtl8153_tx_frame(eth_frame, &mut rtl_tx);
    assert_eq!(tx_len, eth_frame.len() + 8);
    let opts1 = u32::from_le_bytes(rtl_tx[0..4].try_into().unwrap());
    assert_eq!(opts1 & (1 << 31), 1 << 31, "TX_FS must be set");
    assert_eq!(opts1 & (1 << 30), 1 << 30, "TX_LS must be set");
    assert_eq!((opts1 & 0xFFFF) as usize, eth_frame.len());
    assert_eq!(&rtl_tx[8..tx_len], eth_frame);

    // 2. RTL8153 RX framing (8-byte header + frame + 4-byte FCS CRC)
    let mut rtl_rx = Vec::new();
    let rx_len_with_crc = (eth_frame.len() + 4) as u32;
    rtl_rx.extend_from_slice(&rx_len_with_crc.to_le_bytes());
    rtl_rx.extend_from_slice(&0u32.to_le_bytes());
    rtl_rx.extend_from_slice(eth_frame);
    rtl_rx.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // 4-byte FCS
    let decoded_rtl = decode_rtl8153_rx_frame(&rtl_rx).expect("Valid RTL8153 RX frame");
    assert_eq!(decoded_rtl, eth_frame, "4-byte FCS must be stripped");

    // 3. CDC-NCM TX and RX round-trip
    let mut ncm_buf = [0u8; 256];
    let ncm_len = encode_cdc_ncm_tx_frame(7, eth_frame, &mut ncm_buf);
    assert_eq!(ncm_len, 28 + eth_frame.len());
    let decoded_ncm =
        decode_cdc_ncm_rx_frame(&ncm_buf[..ncm_len]).expect("Valid CDC-NCM NTB16 frame");
    assert_eq!(decoded_ncm, eth_frame);
}

const USB_XHCI_BASE: u64 = 0xFEBD_0000;
const USB_DMA_BASE: u64 = 0x6000_0000;
const USB_DMA_PAGES: usize = 512;

struct UsbNetXhciSim {
    dma_pool: Box<[u8]>,
    next_dma_page: usize,
    usblegsup: u32,
    usbcmd: u32,
    usbsts: u32,
    crcr: u64,
    dcbaap: u64,
    config: u32,
    portsc0: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    evt_enqueue_idx: usize,
    evt_cycle: u32,
    cmd_dequeue_idx: usize,
    simulate_rtl8153: bool,
    bulk_in_trb_phys: u64,
    bulk_in_buf_phys: u64,
    transmitted_usb_transfers: Vec<Vec<u8>>,
}

impl UsbNetXhciSim {
    fn new(simulate_rtl8153: bool) -> Self {
        Self {
            dma_pool: vec![0u8; USB_DMA_PAGES * PAGE_SIZE].into_boxed_slice(),
            next_dma_page: 0,
            usblegsup: 0x0001_0001,
            usbcmd: 0,
            usbsts: 1,
            crcr: 0,
            dcbaap: 0,
            config: 0,
            portsc0: (1 << 0) | (1 << 1) | (1 << 9) | (4 << 10), // SuperSpeed USB 3.0 connected & enabled
            erstsz: 0,
            erstba: 0,
            erdp: 0,
            evt_enqueue_idx: 0,
            evt_cycle: 1,
            cmd_dequeue_idx: 0,
            simulate_rtl8153,
            bulk_in_trb_phys: 0,
            bulk_in_buf_phys: 0,
            transmitted_usb_transfers: Vec::new(),
        }
    }

    fn dma_slice(&self, phys: u64, len: usize) -> &[u8] {
        let off = (phys - USB_DMA_BASE) as usize;
        &self.dma_pool[off..off + len]
    }

    fn dma_slice_mut(&mut self, phys: u64, len: usize) -> &mut [u8] {
        let off = (phys - USB_DMA_BASE) as usize;
        &mut self.dma_pool[off..off + len]
    }

    fn post_event_trb(&mut self, param: u64, status: u32, control_without_cycle: u32) {
        let erst_phys = self.erstba & !0x3F;
        let seg = self.dma_slice(erst_phys, 16);
        let ring_base = u64::from_le_bytes(seg[0..8].try_into().unwrap());
        let ring_size = (u32::from_le_bytes(seg[8..12].try_into().unwrap()) as usize).max(16);

        let trb_phys = ring_base + (self.evt_enqueue_idx * 16) as u64;
        let ctrl = (control_without_cycle & !1) | (self.evt_cycle & 1);
        let dst = self.dma_slice_mut(trb_phys, 16);
        dst[0..8].copy_from_slice(&param.to_le_bytes());
        dst[8..12].copy_from_slice(&status.to_le_bytes());
        dst[12..16].copy_from_slice(&ctrl.to_le_bytes());

        self.evt_enqueue_idx += 1;
        if self.evt_enqueue_idx >= ring_size {
            self.evt_enqueue_idx = 0;
            self.evt_cycle ^= 1;
        }
    }

    fn process_command_ring(&mut self) {
        let cmd_ring_base = self.crcr & !0x3F;
        if cmd_ring_base == 0 {
            return;
        }
        for _ in 0..32 {
            let trb_phys = cmd_ring_base + (self.cmd_dequeue_idx * 16) as u64;
            let mut trb = [0u8; 16];
            trb.copy_from_slice(self.dma_slice(trb_phys, 16));
            let ctrl = u32::from_le_bytes(trb[12..16].try_into().unwrap());
            if (ctrl & 1) == 0 {
                break;
            }
            let trb_type = (ctrl >> 10) & 0x3F;
            if trb_type == 6 {
                self.cmd_dequeue_idx = 0;
                continue;
            }
            self.cmd_dequeue_idx += 1;
            self.post_event_trb(trb_phys, 1u32 << 24, (1u32 << 24) | (33u32 << 10));
        }
    }

    fn process_slot_doorbell(&mut self, dci: u32) {
        if dci == 1 {
            self.complete_ep0_control();
        } else if dci == 3 {
            self.scan_bulk_in_trbs();
        } else if dci == 4 {
            self.complete_bulk_out_trbs();
        }
    }

    fn complete_ep0_control(&mut self) {
        let max_off = self.next_dma_page * PAGE_SIZE;
        let mut off = 0usize;
        while off + 48 <= max_off {
            let ctrl = u32::from_le_bytes(self.dma_pool[off + 12..off + 16].try_into().unwrap());
            if (ctrl & 1) == 1 && ((ctrl >> 10) & 0x3F) == 2 {
                let setup: [u8; 8] = self.dma_pool[off..off + 8].try_into().unwrap();
                let data_buf_phys =
                    u64::from_le_bytes(self.dma_pool[off + 16..off + 24].try_into().unwrap());
                self.fill_control_data(setup, data_buf_phys);
                let st_off = off + 32;
                let st_phys = USB_DMA_BASE + st_off as u64;
                self.dma_pool[off + 12] &= !1;
                self.dma_pool[st_off + 12] &= !1;
                self.post_event_trb(
                    st_phys,
                    1u32 << 24,
                    (1u32 << 24) | (1u32 << 16) | (32u32 << 10),
                );
                break;
            }
            off += 16;
        }
    }

    fn fill_control_data(&mut self, setup: [u8; 8], data_buf_phys: u64) {
        let bm_req = setup[0];
        let b_req = setup[1];
        let w_val = u16::from_le_bytes([setup[2], setup[3]]);
        if bm_req == 0x80 && b_req == 0x06 {
            let desc_type = (w_val >> 8) as u8;
            if desc_type == 1 {
                // Device descriptor
                let (vid, pid): (u16, u16) = if self.simulate_rtl8153 {
                    (0x0BDA, 0x8153)
                } else {
                    (0x1D6B, 0x0104)
                };
                let mut dev_desc = [
                    18, 1, 0x00, 0x03, 0x02, 0x00, 0x00, 9, 0, 0, 0, 0, 0x00, 0x30, 1, 2, 3, 1,
                ];
                dev_desc[8..10].copy_from_slice(&vid.to_le_bytes());
                dev_desc[10..12].copy_from_slice(&pid.to_le_bytes());
                self.dma_slice_mut(data_buf_phys, 18)
                    .copy_from_slice(&dev_desc);
            } else if desc_type == 2 {
                // Configuration descriptor: 9B Config + 9B Interface + 7B EP1 IN Bulk (0x81) + 7B EP2 OUT Bulk (0x02) = 32B
                let (class, subclass) = if self.simulate_rtl8153 {
                    (0xFF, 0xFF)
                } else {
                    (0x02, 0x06) // CDC-ECM
                };
                let cfg_desc: [u8; 32] = [
                    9, 2, 32, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 2, class, subclass, 0, 0, 7, 5,
                    0x81, 0x02, 0x00, 0x04, 0, // EP1 IN Bulk (DCI 3)
                    7, 5, 0x02, 0x02, 0x00, 0x04, 0, // EP2 OUT Bulk (DCI 4)
                ];
                self.dma_slice_mut(data_buf_phys, 32)
                    .copy_from_slice(&cfg_desc);
            }
        } else if bm_req == 0xC0 && b_req == 0x05 && w_val == 0xC000 {
            // Realtek RTL8153 PLA IDR MAC read
            let mac = [0x00, 0xE0, 0x4C, 0x68, 0x01, 0x02, 0x00, 0x00];
            self.dma_slice_mut(data_buf_phys, 8).copy_from_slice(&mac);
        }
    }

    fn scan_bulk_in_trbs(&mut self) {
        let max_off = self.next_dma_page * PAGE_SIZE;
        let mut off = 0usize;
        while off + 16 <= max_off {
            let param = u64::from_le_bytes(self.dma_pool[off..off + 8].try_into().unwrap());
            let status = u32::from_le_bytes(self.dma_pool[off + 8..off + 12].try_into().unwrap());
            let ctrl = u32::from_le_bytes(self.dma_pool[off + 12..off + 16].try_into().unwrap());
            let cycle = ctrl & 1;
            let trb_type = (ctrl >> 10) & 0x3F;
            let xfer_len = (status & 0x1FFFF) as usize;
            if cycle == 1 && trb_type == 1 && xfer_len == BULK_BUFFER_SIZE && param >= USB_DMA_BASE
            {
                self.bulk_in_trb_phys = USB_DMA_BASE + off as u64;
                self.bulk_in_buf_phys = param;
                break;
            }
            off += 16;
        }
    }

    fn complete_bulk_out_trbs(&mut self) {
        let max_off = self.next_dma_page * PAGE_SIZE;
        let mut off = 0usize;
        while off + 16 <= max_off {
            let param = u64::from_le_bytes(self.dma_pool[off..off + 8].try_into().unwrap());
            let status = u32::from_le_bytes(self.dma_pool[off + 8..off + 12].try_into().unwrap());
            let ctrl = u32::from_le_bytes(self.dma_pool[off + 12..off + 16].try_into().unwrap());
            let cycle = ctrl & 1;
            let trb_type = (ctrl >> 10) & 0x3F;
            let xfer_len = (status & 0x1FFFF) as usize;
            if cycle == 1
                && trb_type == 1
                && xfer_len > 0
                && xfer_len < BULK_BUFFER_SIZE
                && param >= USB_DMA_BASE
            {
                let data = self.dma_slice(param, xfer_len).to_vec();
                self.transmitted_usb_transfers.push(data);
                self.dma_pool[off + 12] &= !1;
                let trb_phys = USB_DMA_BASE + off as u64;
                // Post Transfer Event for DCI = 4 (Bulk OUT)
                self.post_event_trb(
                    trb_phys,
                    1u32 << 24,
                    (1u32 << 24) | (4u32 << 16) | (32u32 << 10),
                );
            }
            off += 16;
        }
    }

    fn inject_usb_rx_transfer(&mut self, transfer: &[u8]) {
        self.scan_bulk_in_trbs();
        assert_ne!(self.bulk_in_buf_phys, 0, "No Bulk-IN TRB queued");
        self.dma_slice_mut(self.bulk_in_buf_phys, transfer.len())
            .copy_from_slice(transfer);
        let trb_off = (self.bulk_in_trb_phys - USB_DMA_BASE) as usize;
        self.dma_pool[trb_off + 12] &= !1;

        let residual = (BULK_BUFFER_SIZE - transfer.len()) as u32;
        let comp_status = (13u32 << 24) | (residual & 0x00FF_FFFF); // CC = 13 Short Packet
        let comp_ctrl = (1u32 << 24) | (3u32 << 16) | (32u32 << 10); // DCI = 3 (Bulk IN)
        self.post_event_trb(self.bulk_in_trb_phys, comp_status, comp_ctrl);
    }

    fn mmio_read32(&self, addr: u64) -> u32 {
        let off = addr - USB_XHCI_BASE;
        match off {
            0x000 => 0x0100_0020,
            0x004 => (4 << 24) | (1 << 8) | 8,
            0x010 => 0x0080_0000,
            0x014 => 0x0000_0300,
            0x018 => 0x0000_0600,
            0x020 => self.usbcmd,
            0x024 => self.usbsts,
            0x028 => 0x0000_0001,
            0x200 => self.usblegsup,
            0x420 => self.portsc0,
            0x628 => self.erstsz,
            0x630 => self.erstba as u32,
            0x634 => (self.erstba >> 32) as u32,
            0x638 => self.erdp as u32,
            0x63C => (self.erdp >> 32) as u32,
            _ => 0,
        }
    }

    fn mmio_write32(&mut self, addr: u64, val: u32) {
        let off = addr - USB_XHCI_BASE;
        match off {
            0x020 => {
                if (val & (1 << 1)) != 0 {
                    self.usbcmd = val & !(1 << 1);
                    self.usbsts = 1;
                } else {
                    self.usbcmd = val;
                    if (val & 1) != 0 {
                        self.usbsts &= !1;
                    } else {
                        self.usbsts |= 1;
                    }
                }
            }
            0x038 => self.crcr = (self.crcr & 0xFFFF_FFFF_0000_0000) | (val as u64),
            0x03C => self.crcr = (self.crcr & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32),
            0x050 => self.dcbaap = (self.dcbaap & 0xFFFF_FFFF_0000_0000) | (val as u64),
            0x054 => self.dcbaap = (self.dcbaap & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32),
            0x058 => self.config = val,
            0x200 => {
                self.usblegsup = val & !(1 << 16);
            }
            0x300 => self.process_command_ring(),
            0x304 => self.process_slot_doorbell(val & 0xFF),
            0x628 => self.erstsz = val,
            0x630 => self.erstba = (self.erstba & 0xFFFF_FFFF_0000_0000) | (val as u64),
            0x634 => self.erstba = (self.erstba & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32),
            0x638 => self.erdp = (self.erdp & 0xFFFF_FFFF_0000_0000) | (val as u64),
            0x63C => self.erdp = (self.erdp & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32),
            _ => {}
        }
    }
}

#[derive(Clone)]
struct MockUsbNetHal {
    state: Arc<Mutex<UsbNetXhciSim>>,
}

impl MockUsbNetHal {
    fn new(simulate_rtl8153: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(UsbNetXhciSim::new(simulate_rtl8153))),
        }
    }
}

impl Hal for MockUsbNetHal {
    fn mmio_read8(&self, _addr: u64) -> u8 {
        0
    }
    fn mmio_write8(&self, _addr: u64, _val: u8) {}
    fn mmio_read16(&self, _addr: u64) -> u16 {
        0
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
        st.mmio_write32(addr, val as u32);
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
        if st.next_dma_page + pages > USB_DMA_PAGES {
            return None;
        }
        let off = st.next_dma_page * PAGE_SIZE;
        let size = pages * PAGE_SIZE;
        st.next_dma_page += pages;
        st.dma_pool[off..off + size].fill(0);
        let virt = unsafe { st.dma_pool.as_mut_ptr().add(off) };
        let phys = USB_DMA_BASE + off as u64;
        Some(DmaBuffer { virt, phys, size })
    }
    fn free_dma(&self, _buf: DmaBuffer) {}
    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        let mut st = self.state.lock().unwrap();
        let off = (phys - USB_DMA_BASE) as usize;
        unsafe { st.dma_pool.as_mut_ptr().add(off) }
    }
    fn micros(&self) -> i64 {
        1_000
    }
    fn yield_now(&self) -> impl Future<Output = ()> {
        core::future::ready(())
    }
    fn sleep(&self, _duration: Duration) -> impl Future<Output = ()> {
        core::future::ready(())
    }
}

#[test]
fn test_usb_ethernet_rtl8153_dongle_enumeration_mac_and_bulk_io() {
    let hal = MockUsbNetHal::new(true);
    let mut dev = UsbEthernetDevice::new(USB_XHCI_BASE, None, hal.clone())
        .expect("UsbEthernetDevice::new should enumerate RTL8153 dongle");

    assert_eq!(dev.protocol(), UsbNetProtocol::Rtl8153);
    assert_eq!(dev.mac_address(), [0x00, 0xE0, 0x4C, 0x68, 0x01, 0x02]);

    // Transmit an Ethernet frame over Bulk-OUT and verify 8-byte RTL8153 header
    let tx_eth = b"\xff\xff\xff\xff\xff\xff\x00\xe0\x4c\x68\x01\x02\x08\x06rtl8153-usb-tx-frame";
    dev.transmit(tx_eth)
        .expect("RTL8153 Bulk-OUT TX should succeed");
    {
        let st = hal.state.lock().unwrap();
        assert_eq!(st.transmitted_usb_transfers.len(), 1);
        let raw_tx = &st.transmitted_usb_transfers[0];
        assert_eq!(raw_tx.len(), tx_eth.len() + 8);
        assert_eq!(&raw_tx[8..], tx_eth);
    }

    // Inject an RTL8153 Bulk-IN transfer (8-byte header + frame + 4-byte FCS) and verify receive()
    let rx_eth = b"\x00\xe0\x4c\x68\x01\x02\x52\x54\x00\x12\x34\x56\x08\x00rtl8153-usb-rx-frame";
    let mut usb_rx = Vec::new();
    usb_rx.extend_from_slice(&((rx_eth.len() + 4) as u32).to_le_bytes());
    usb_rx.extend_from_slice(&0u32.to_le_bytes());
    usb_rx.extend_from_slice(rx_eth);
    usb_rx.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // 4-byte FCS
    hal.state.lock().unwrap().inject_usb_rx_transfer(&usb_rx);

    assert!(dev.has_packets());
    let mut rx_buf = [0u8; 256];
    let n = dev
        .receive(&mut rx_buf)
        .expect("Expected RTL8153 RX packet");
    assert_eq!(&rx_buf[..n], rx_eth);
}

#[test]
fn test_usb_ethernet_cdc_ecm_dongle_enumeration_and_bulk_io() {
    let hal = MockUsbNetHal::new(false);
    let fb_mac = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
    let mut dev = UsbEthernetDevice::new(USB_XHCI_BASE, Some(fb_mac), hal.clone())
        .expect("UsbEthernetDevice::new should enumerate CDC-ECM dongle");

    assert_eq!(dev.protocol(), UsbNetProtocol::CdcEcm);
    assert_eq!(dev.mac_address(), fb_mac);

    let tx_eth = b"\xff\xff\xff\xff\xff\xff\x02\x11\x22\x33\x44\x55\x08\x06cdc-ecm-raw-frame";
    dev.transmit(tx_eth)
        .expect("CDC-ECM Bulk-OUT TX should succeed");
    {
        let st = hal.state.lock().unwrap();
        assert_eq!(st.transmitted_usb_transfers, vec![tx_eth.to_vec()]);
    }

    let rx_eth = b"\x02\x11\x22\x33\x44\x55\x52\x54\x00\x12\x34\x56\x08\x00cdc-ecm-rx-frame";
    hal.state.lock().unwrap().inject_usb_rx_transfer(rx_eth);
    assert!(dev.has_packets());
    let mut rx_buf = [0u8; 256];
    let n = dev
        .receive(&mut rx_buf)
        .expect("Expected CDC-ECM RX packet");
    assert_eq!(&rx_buf[..n], rx_eth);
}
