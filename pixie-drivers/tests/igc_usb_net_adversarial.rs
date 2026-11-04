mod common;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::DmaPool;
use pixie_drivers::hal::{DmaBuffer, Hal};
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
    dma: DmaPool,

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
            dma: DmaPool::new(IGC_DMA_BASE, IGC_DMA_PAGES),
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
        raw.copy_from_slice(self.dma.slice(desc_addr, 16));
        let buf_phys = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        self.dma
            .slice_mut(buf_phys, frame.len())
            .copy_from_slice(frame);
        let desc_mut = self.dma.slice_mut(desc_addr, 16);
        desc_mut[8..10].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        desc_mut[12] = 0x03; // DD | EOP
        desc_mut[13] = 0;
        self.rdh0 = ((idx + 1) % NUM_RX_DESC) as u32;
    }

    fn process_tx_tail(&mut self, new_tdt: u32) {
        while self.tdh0 != new_tdt {
            let idx = self.tdh0 as usize;
            let desc_addr = self.tx_ring_base() + (idx * core::mem::size_of::<TxDesc>()) as u64;
            let b = self.dma.slice(desc_addr, 16);
            let buf_phys = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let len = u16::from_le_bytes(b[8..10].try_into().unwrap()) as usize;
            let pkt = self.dma.slice(buf_phys, len).to_vec();
            self.transmitted_packets.push(pkt);
            self.dma.slice_mut(desc_addr, 16)[12] |= 1; // Set DD
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
        self.state.lock().unwrap().dma.alloc(pages)
    }
    fn free_dma(&self, buf: DmaBuffer) {
        self.state.lock().unwrap().dma.free(buf);
    }
    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        self.state.lock().unwrap().dma.phys_to_virt(phys)
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum UsbDongleKind {
    Rtl8153,
    CdcEcm,
    CompositeHidAndCdcNcm,
}

struct UsbNetXhciSim {
    dma: DmaPool,
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
    cmd_cycle: u32,
    kind: UsbDongleKind,

    ep0_ring_phys: u64,
    ep0_deq_idx: usize,
    ep0_cycle: u32,

    bulk_in_ring_phys: u64,
    bulk_in_deq_idx: usize,
    bulk_in_cycle: u32,

    bulk_out_ring_phys: u64,
    bulk_out_deq_idx: usize,
    bulk_out_cycle: u32,
    hold_bulk_out: bool,

    transmitted_usb_transfers: Vec<Vec<u8>>,
}

impl UsbNetXhciSim {
    fn new(kind: UsbDongleKind) -> Self {
        Self {
            dma: DmaPool::new(USB_DMA_BASE, USB_DMA_PAGES),
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
            cmd_cycle: 1,
            kind,

            ep0_ring_phys: 0,
            ep0_deq_idx: 0,
            ep0_cycle: 1,

            bulk_in_ring_phys: 0,
            bulk_in_deq_idx: 0,
            bulk_in_cycle: 1,

            bulk_out_ring_phys: 0,
            bulk_out_deq_idx: 0,
            bulk_out_cycle: 1,
            hold_bulk_out: false,

            transmitted_usb_transfers: Vec::new(),
        }
    }

    fn post_event_trb(&mut self, param: u64, status: u32, control_without_cycle: u32) {
        let erst_phys = self.erstba & !0x3F;
        let seg = self.dma.slice(erst_phys, 16);
        let ring_base = u64::from_le_bytes(seg[0..8].try_into().unwrap());
        let ring_size = (u32::from_le_bytes(seg[8..12].try_into().unwrap()) as usize).max(16);

        let trb_phys = ring_base + (self.evt_enqueue_idx * 16) as u64;
        let ctrl = (control_without_cycle & !1) | (self.evt_cycle & 1);
        let dst = self.dma.slice_mut(trb_phys, 16);
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
            trb.copy_from_slice(self.dma.slice(trb_phys, 16));
            let param = u64::from_le_bytes(trb[0..8].try_into().unwrap());
            let ctrl = u32::from_le_bytes(trb[12..16].try_into().unwrap());
            if (ctrl & 1) != self.cmd_cycle {
                break;
            }
            let trb_type = (ctrl >> 10) & 0x3F;
            if trb_type == 6 {
                self.cmd_dequeue_idx = 0;
                if (ctrl & (1 << 1)) != 0 {
                    self.cmd_cycle ^= 1;
                }
                continue;
            }
            self.cmd_dequeue_idx += 1;

            // Inspect Input Context on Address Device (11) or Configure Endpoint (12)
            if (trb_type == 11 || trb_type == 12) && param >= USB_DMA_BASE {
                assert_eq!(
                    param & 0xFFF,
                    0,
                    "Input Context {:#x} must be 4 KiB page-aligned",
                    param
                );
                for ctx_idx in 2..32 {
                    for stride in [32usize, 64usize] {
                        let ep_ctx_phys = param + (ctx_idx * stride) as u64;
                        if (ep_ctx_phys - USB_DMA_BASE) as usize + 16 <= self.dma.len() {
                            let s = self.dma.slice(ep_ctx_phys, 16);
                            let dw1 = u32::from_le_bytes(s[4..8].try_into().unwrap());
                            let ep_type = (dw1 >> 3) & 0x7;
                            let tr_raw = u64::from_le_bytes(s[8..16].try_into().unwrap());
                            let tr_deq = tr_raw & !0xF;
                            let dcs = (tr_raw & 1) as u32;
                            if tr_deq >= USB_DMA_BASE {
                                match ep_type {
                                    4 => {
                                        self.ep0_ring_phys = tr_deq;
                                        self.ep0_deq_idx = 0;
                                        self.ep0_cycle = dcs;
                                    }
                                    6 => {
                                        self.bulk_in_ring_phys = tr_deq;
                                        self.bulk_in_deq_idx = 0;
                                        self.bulk_in_cycle = dcs;
                                    }
                                    2 => {
                                        self.bulk_out_ring_phys = tr_deq;
                                        self.bulk_out_deq_idx = 0;
                                        self.bulk_out_cycle = dcs;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }

            self.post_event_trb(trb_phys, 1u32 << 24, (1u32 << 24) | (33u32 << 10));
        }
    }

    fn process_slot_doorbell(&mut self, dci: u32) {
        if dci == 1 {
            self.complete_ep0_control();
        } else if dci == 4 && !self.hold_bulk_out {
            self.flush_in_flight_bulk_out();
        }
    }

    fn next_ring_trb(
        &mut self,
        ring_phys: u64,
        deq_idx: &mut usize,
        cycle: &mut u32,
    ) -> Option<(u64, [u8; 16])> {
        if ring_phys == 0 {
            return None;
        }
        for _ in 0..8 {
            let trb_phys = ring_phys + (*deq_idx * 16) as u64;
            let mut trb = [0u8; 16];
            trb.copy_from_slice(self.dma.slice(trb_phys, 16));
            let ctrl = u32::from_le_bytes(trb[12..16].try_into().unwrap());
            if (ctrl & 1) != *cycle {
                return None;
            }
            let trb_type = (ctrl >> 10) & 0x3F;
            if trb_type == 6 {
                *deq_idx = 0;
                if (ctrl & (1 << 1)) != 0 {
                    *cycle ^= 1;
                }
                continue;
            }
            *deq_idx += 1;
            return Some((trb_phys, trb));
        }
        None
    }

    fn complete_ep0_control(&mut self) {
        let mut ep0_idx = self.ep0_deq_idx;
        let mut ep0_cyc = self.ep0_cycle;
        let ring = self.ep0_ring_phys;
        if let Some((_, setup_trb)) = self.next_ring_trb(ring, &mut ep0_idx, &mut ep0_cyc) {
            let setup: [u8; 8] = setup_trb[0..8].try_into().unwrap();
            let (_, data_trb) = self
                .next_ring_trb(ring, &mut ep0_idx, &mut ep0_cyc)
                .expect("Missing Data Stage TRB");
            let data_buf_phys = u64::from_le_bytes(data_trb[0..8].try_into().unwrap());
            self.fill_control_data(setup, data_buf_phys);
            let (st_phys, _) = self
                .next_ring_trb(ring, &mut ep0_idx, &mut ep0_cyc)
                .expect("Missing Status Stage TRB");
            self.ep0_deq_idx = ep0_idx;
            self.ep0_cycle = ep0_cyc;
            self.post_event_trb(
                st_phys,
                1u32 << 24,
                (1u32 << 24) | (1u32 << 16) | (32u32 << 10),
            );
        }
    }

    fn fill_control_data(&mut self, setup: [u8; 8], data_buf_phys: u64) {
        let bm_req = setup[0];
        let b_req = setup[1];
        let w_val = u16::from_le_bytes([setup[2], setup[3]]);
        let w_len = u16::from_le_bytes([setup[6], setup[7]]) as usize;
        if bm_req == 0x80 && b_req == 0x06 {
            let desc_type = (w_val >> 8) as u8;
            if desc_type == 1 {
                let (dev_class, vid, pid): (u8, u16, u16) = match self.kind {
                    UsbDongleKind::Rtl8153 => (0x00, 0x0BDA, 0x8153),
                    UsbDongleKind::CdcEcm => (0x02, 0x1D6B, 0x0104),
                    UsbDongleKind::CompositeHidAndCdcNcm => (0x00, 0x17EF, 0x3082),
                };
                let mut dev_desc = [
                    18, 1, 0x00, 0x03, dev_class, 0x00, 0x00, 9, 0, 0, 0, 0, 0x00, 0x30, 1, 2, 3, 1,
                ];
                dev_desc[8..10].copy_from_slice(&vid.to_le_bytes());
                dev_desc[10..12].copy_from_slice(&pid.to_le_bytes());
                let n = w_len.min(18);
                self.dma
                    .slice_mut(data_buf_phys, n)
                    .copy_from_slice(&dev_desc[..n]);
            } else if desc_type == 2 {
                let cfg_desc: Vec<u8> = match self.kind {
                    UsbDongleKind::Rtl8153 | UsbDongleKind::CdcEcm => {
                        let (class, subclass) = if self.kind == UsbDongleKind::Rtl8153 {
                            (0xFF, 0xFF)
                        } else {
                            (0x02, 0x06) // CDC-ECM
                        };
                        vec![
                            9, 2, 32, 0, 1, 1, 0, 0x80, 50, // Config (32B)
                            9, 4, 0, 0, 2, class, subclass, 0, 0, // Interface 0
                            7, 5, 0x81, 0x02, 0x00, 0x04, 0, // EP1 IN Bulk (DCI 3)
                            7, 5, 0x02, 0x02, 0x00, 0x04, 0, // EP2 OUT Bulk (DCI 4)
                        ]
                    }
                    UsbDongleKind::CompositeHidAndCdcNcm => {
                        // Composite USB-C dock:
                        // - Interface 0: HID (0x03) with EP3 IN Interrupt (0x83) -> must NOT abort NIC search!
                        // - Interface 1: CDC Communication (0x02), SubClass 0x0D (CDC-NCM)
                        // - Interface 2: CDC Data (0x0A) with EP1 IN Bulk (0x81, DCI 3) & EP2 OUT Bulk (0x02, DCI 4)
                        let total: u16 = 9 + 9 + 7 + 9 + 9 + 7 + 7; // 57 bytes
                        let [lo, hi] = total.to_le_bytes();
                        vec![
                            9, 2, lo, hi, 3, 1, 0, 0x80, 50, // Config (57B, 3 interfaces)
                            9, 4, 0, 0, 1, 0x03, 0x01, 0x01, 0, // IF0: HID Keyboard
                            7, 5, 0x83, 0x03, 0x08, 0x00, 10, // EP3 IN Interrupt
                            9, 4, 1, 0, 0, 0x02, 0x0D, 0x00, 0, // IF1: CDC-NCM Control
                            9, 4, 2, 0, 2, 0x0A, 0x00, 0x01, 0, // IF2: CDC-NCM Data
                            7, 5, 0x81, 0x02, 0x00, 0x04, 0, // EP1 IN Bulk (DCI 3)
                            7, 5, 0x02, 0x02, 0x00, 0x04, 0, // EP2 OUT Bulk (DCI 4)
                        ]
                    }
                };
                let n = w_len.min(cfg_desc.len());
                self.dma
                    .slice_mut(data_buf_phys, n)
                    .copy_from_slice(&cfg_desc[..n]);
            }
        } else if bm_req == 0xC0 && b_req == 0x05 && w_val == 0xC000 {
            // Realtek RTL8153 PLA IDR MAC read
            let mac = [0x00, 0xE0, 0x4C, 0x68, 0x01, 0x02, 0x00, 0x00];
            self.dma.slice_mut(data_buf_phys, 8).copy_from_slice(&mac);
        }
    }

    fn flush_in_flight_bulk_out(&mut self) {
        let ring = self.bulk_out_ring_phys;
        let mut idx = self.bulk_out_deq_idx;
        let mut cyc = self.bulk_out_cycle;
        let mut completed = Vec::new();
        while let Some((trb_phys, trb)) = self.next_ring_trb(ring, &mut idx, &mut cyc) {
            let param = u64::from_le_bytes(trb[0..8].try_into().unwrap());
            let status = u32::from_le_bytes(trb[8..12].try_into().unwrap());
            let ctrl = u32::from_le_bytes(trb[12..16].try_into().unwrap());
            let trb_type = (ctrl >> 10) & 0x3F;
            assert_eq!(trb_type, 1, "Expected Normal TRB on Bulk-OUT ring");
            let xfer_len = (status & 0x1FFFF) as usize;
            let data = self.dma.slice(param, xfer_len).to_vec();
            completed.push((trb_phys, data));
        }
        self.bulk_out_deq_idx = idx;
        self.bulk_out_cycle = cyc;
        for (trb_phys, data) in completed {
            self.transmitted_usb_transfers.push(data);
            self.post_event_trb(
                trb_phys,
                1u32 << 24,
                (1u32 << 24) | (4u32 << 16) | (32u32 << 10),
            );
        }
    }

    fn inject_usb_rx_transfer_with_cc(&mut self, transfer: &[u8], cc: u32) {
        let ring = self.bulk_in_ring_phys;
        let mut idx = self.bulk_in_deq_idx;
        let mut cyc = self.bulk_in_cycle;
        let (trb_phys, trb) = self
            .next_ring_trb(ring, &mut idx, &mut cyc)
            .expect("No Bulk-IN TRB queued");
        self.bulk_in_deq_idx = idx;
        self.bulk_in_cycle = cyc;

        let buf_phys = u64::from_le_bytes(trb[0..8].try_into().unwrap());
        self.dma
            .slice_mut(buf_phys, transfer.len())
            .copy_from_slice(transfer);

        let residual = (BULK_BUFFER_SIZE - transfer.len()) as u32;
        let comp_status = (cc << 24) | (residual & 0x00FF_FFFF);
        let comp_ctrl = (1u32 << 24) | (3u32 << 16) | (32u32 << 10); // DCI = 3 (Bulk IN)
        self.post_event_trb(trb_phys, comp_status, comp_ctrl);
    }

    fn inject_usb_rx_transfer(&mut self, transfer: &[u8]) {
        self.inject_usb_rx_transfer_with_cc(transfer, 13); // CC = 13 Short Packet
    }

    fn mmio_read32(&self, addr: u64) -> u32 {
        let off = addr - USB_XHCI_BASE;
        match off {
            0x000 => 0x0100_0020,
            0x004 => (4 << 24) | (1 << 8) | 8,
            0x008 => 0, // HCSPARAMS2: 0 scratchpad buffers
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
    fn new(kind: UsbDongleKind) -> Self {
        Self {
            state: Arc::new(Mutex::new(UsbNetXhciSim::new(kind))),
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
        self.state.lock().unwrap().dma.alloc(pages)
    }
    fn free_dma(&self, buf: DmaBuffer) {
        self.state.lock().unwrap().dma.free(buf);
    }
    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        self.state.lock().unwrap().dma.phys_to_virt(phys)
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
    let hal = MockUsbNetHal::new(UsbDongleKind::Rtl8153);
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
    let hal = MockUsbNetHal::new(UsbDongleKind::CdcEcm);
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

#[test]
fn test_usb_ethernet_tx_link_trb_wraparound_32_in_flight_no_aliasing() {
    assert!(
        core::mem::size_of::<UsbEthernetDevice<MockUsbNetHal>>() < 200,
        "UsbEthernetDevice size should stay under 200 bytes"
    );

    let hal = MockUsbNetHal::new(UsbDongleKind::CdcEcm);
    let mut dev = UsbEthernetDevice::new(USB_XHCI_BASE, None, hal.clone())
        .expect("UsbEthernetDevice::new should succeed");
    assert_eq!(dev.mac_address(), [0x02, b'P', b'I', b'X', b'I', b'E']);

    // Hold Bulk-OUT completions in hardware so 32 packets stay simultaneously in-flight in DMA!
    hal.state.lock().unwrap().hold_bulk_out = true;

    let mut expected_packets: Vec<Vec<u8>> = Vec::new();
    // 4 batches * 32 in-flight = 128 packets (> 64 TRBs, crossing the 63-slot Link TRB wrap twice!)
    for batch in 0..4usize {
        for slot in 0..32usize {
            let seq = batch * 32 + slot;
            assert!(
                dev.can_transmit(),
                "Expected can_transmit() == true for packet {} (slot {}/32)",
                seq,
                slot
            );
            let mut pkt = vec![0u8; 64];
            pkt[0..6].copy_from_slice(&[0xFF; 6]);
            pkt[6..12].copy_from_slice(&dev.mac_address());
            pkt[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            pkt[14..18].copy_from_slice(&(seq as u32).to_be_bytes());
            for (i, b) in pkt[18..].iter_mut().enumerate() {
                *b = ((seq * 31 + i * 7) & 0xFF) as u8;
            }
            dev.transmit(&pkt)
                .expect("transmit() with <32 in-flight packets should succeed");
            expected_packets.push(pkt);
        }

        // At 32 packets in flight, backpressure MUST engage
        assert!(
            !dev.can_transmit(),
            "can_transmit() must be false when 32 Bulk-OUT buffers are in flight"
        );
        assert!(dev.transmit(&[0xAA; 60]).is_err());

        // Now complete all 32 in-flight TRBs from DMA at once — if any two in-flight TRBs
        // aliased the same DMA buffer (e.g. ring slot 32 vs wrapped ring slot 0 at packet 63!),
        // the earlier packet's payload would have been overwritten before completion!
        hal.state.lock().unwrap().flush_in_flight_bulk_out();
    }

    // Drain final completion events
    assert!(dev.can_transmit());

    let st = hal.state.lock().unwrap();
    assert_eq!(st.transmitted_usb_transfers.len(), expected_packets.len());
    for (idx, (got, want)) in st
        .transmitted_usb_transfers
        .iter()
        .zip(expected_packets.iter())
        .enumerate()
    {
        assert_eq!(
            got, want,
            "DMA buffer aliasing / payload corruption detected on packet {}",
            idx
        );
    }
}

#[test]
fn test_usb_ethernet_composite_hid_cdc_ncm_multi_datagram_and_cc_filter() {
    let hal = MockUsbNetHal::new(UsbDongleKind::CompositeHidAndCdcNcm);
    let mut dev = UsbEthernetDevice::new(USB_XHCI_BASE, None, hal.clone())
        .expect("Composite USB dock with HID + CDC-NCM interfaces must enumerate NIC");
    assert_eq!(dev.protocol(), UsbNetProtocol::CdcNcm);

    let frame1 = b"\xff\xff\xff\xff\xff\xff\x02PIXIE\x08\x06ncm-multi-datagram-one";
    let frame2 = b"\xff\xff\xff\xff\xff\xff\x02PIXIE\x08\x00ncm-multi-datagram-two-payload";
    let frame3 = b"\xff\xff\xff\xff\xff\xff\x02PIXIE\x86\xddncm-multi-datagram-three-ipv6";

    // Build a single NTB16 transfer containing 3 Ethernet datagrams in one NDP16 table:
    // NTH16 (12B at 0..12), NDP16 (8B header + 4 * 4B entries = 24B at 12..36), followed by frames
    let ndp_offset: u16 = 12;
    let ndp_len: u16 = 8 + 4 * 4; // 3 datagrams + terminating (0, 0) entry
    let off1 = (ndp_offset + ndp_len) as usize;
    let off2 = off1 + frame1.len();
    let off3 = off2 + frame2.len();
    let total_len = off3 + frame3.len();

    let mut ntb = vec![0u8; total_len];
    ntb[0..4].copy_from_slice(b"NCMH");
    ntb[4..6].copy_from_slice(&12u16.to_le_bytes()); // wHeaderLength
    ntb[6..8].copy_from_slice(&1u16.to_le_bytes()); // wSequence
    ntb[8..10].copy_from_slice(&(total_len as u16).to_le_bytes()); // wBlockLength
    ntb[10..12].copy_from_slice(&ndp_offset.to_le_bytes()); // wNdpIndex

    ntb[12..16].copy_from_slice(b"NCM0");
    ntb[16..18].copy_from_slice(&ndp_len.to_le_bytes());
    ntb[18..20].copy_from_slice(&0u16.to_le_bytes()); // wNextNdpIndex = 0
    ntb[20..22].copy_from_slice(&(off1 as u16).to_le_bytes());
    ntb[22..24].copy_from_slice(&(frame1.len() as u16).to_le_bytes());
    ntb[24..26].copy_from_slice(&(off2 as u16).to_le_bytes());
    ntb[26..28].copy_from_slice(&(frame2.len() as u16).to_le_bytes());
    ntb[28..30].copy_from_slice(&(off3 as u16).to_le_bytes());
    ntb[30..32].copy_from_slice(&(frame3.len() as u16).to_le_bytes());
    // 32..36 is terminating (0, 0)

    ntb[off1..off1 + frame1.len()].copy_from_slice(frame1);
    ntb[off2..off2 + frame2.len()].copy_from_slice(frame2);
    ntb[off3..off3 + frame3.len()].copy_from_slice(frame3);

    // 1. Inject an errored Transfer Event (CC = 4 USB Transaction Error) first: must be ignored!
    hal.state
        .lock()
        .unwrap()
        .inject_usb_rx_transfer_with_cc(&ntb, 4);
    assert!(
        !dev.has_packets(),
        "Errored USB Transfer Event (CC=4) must not deliver packets"
    );

    // 2. Inject the valid multi-datagram NTB16 transfer (CC = 13 Short Packet)
    hal.state.lock().unwrap().inject_usb_rx_transfer(&ntb);
    assert!(dev.has_packets());

    let mut rx_buf = [0u8; 256];
    let n1 = dev.receive(&mut rx_buf).expect("Expected datagram 1");
    assert_eq!(&rx_buf[..n1], frame1);
    let n2 = dev.receive(&mut rx_buf).expect("Expected datagram 2");
    assert_eq!(&rx_buf[..n2], frame2);
    let n3 = dev.receive(&mut rx_buf).expect("Expected datagram 3");
    assert_eq!(&rx_buf[..n3], frame3);
    assert!(!dev.has_packets());
}
