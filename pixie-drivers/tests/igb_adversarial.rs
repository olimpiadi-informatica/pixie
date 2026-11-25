mod common;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::DmaPool;
use pixie_drivers::hal::{DmaBuffer, Hal};
use pixie_drivers::net::igb::{
    self, AdvRxDesc, IgbDevice, IgbVariant, NUM_RX_DESC, NUM_TX_DESC, TxDesc,
};
use pixie_drivers::net::{e1000, igc, rtl};
use pixie_drivers::pci::PciDevice;

const IGB_MMIO_BASE: u64 = 0xFEBA_0000;
const IGB_DMA_BASE: u64 = 0x5000_0000;
const IGB_DMA_PAGES: usize = 2048; // 8 MiB

const RXDCTL_QUEUE_ENABLE: u32 = 1 << 25;
const TXDCTL_QUEUE_ENABLE: u32 = 1 << 25;

#[derive(Default)]
struct QueueRegs {
    rdbal: u32,
    rdbah: u32,
    rdlen: u32,
    srrctl: u32,
    rdh: u32,
    rdt: u32,
    rxdctl: u32,
    tdbal: u32,
    tdbah: u32,
    tdlen: u32,
    tdh: u32,
    tdt: u32,
    txdctl: u32,
}

struct IgbSimState {
    dma: DmaPool,
    variant: IgbVariant,

    ctrl: u32,
    ctrl_ext: u32,
    status: u32,
    ims: u32,
    imc: u32,
    legacy_imc: u32,
    eimc: u32,
    icr_reads: usize,
    legacy_icr_reads: usize,
    eicr_reads: usize,
    rctl: u32,
    tctl: u32,
    tipg: u32,
    rxcsum: u32,
    rfctl: u32,
    mrqc: u32,
    ral0: u32,
    rah0: u32,

    queues: Vec<QueueRegs>,
    ctrl_rst_asserted: bool,
    rxdctl0_enabled_before_rctl_en: bool,
    txdctl0_enabled_before_tctl_en: bool,
    all_queues_disabled_before_q0_enable: bool,
    mdic_writes: usize,
    last_mdic: u32,
    set_status_lu_on_mdic: bool,
    transmitted_packets: Vec<Vec<u8>>,
    time_us: i64,
}

impl IgbSimState {
    fn new_dirty_handover(variant: IgbVariant, ral0: u32, rah0: u32) -> Self {
        let num_queues = match variant {
            IgbVariant::I82575 | IgbVariant::I210 => 4,
            IgbVariant::I82580 => 8,
            IgbVariant::I82576 => 16,
        };
        let mut queues = Vec::with_capacity(num_queues);
        for q in 0..num_queues {
            queues.push(QueueRegs {
                rxdctl: RXDCTL_QUEUE_ENABLE,
                txdctl: TXDCTL_QUEUE_ENABLE,
                tdh: if q == 0 { 5 } else { 0 },
                ..Default::default()
            });
        }
        Self {
            dma: DmaPool::new(IGB_DMA_BASE, IGB_DMA_PAGES),
            variant,
            ctrl: (1 << 3) | (1 << 6) | (1 << 30), // LRST + SLU + VME set
            ctrl_ext: 0,
            status: 1 << 1, // LU
            ims: 0,
            imc: 0,
            legacy_imc: 0,
            eimc: 0,
            icr_reads: 0,
            legacy_icr_reads: 0,
            eicr_reads: 0,
            rctl: 1 << 1,
            tctl: 1 << 1,
            tipg: 0,
            rxcsum: 0x0000_0700, // Dirty RXCSUM left by Linux igb
            rfctl: 1 << 15,      // EXSTEN left by Linux igb
            mrqc: 0x0000_0002,   // Dirty RSS left by Linux igb
            ral0,
            rah0,
            queues,
            ctrl_rst_asserted: false,
            rxdctl0_enabled_before_rctl_en: false,
            txdctl0_enabled_before_tctl_en: false,
            all_queues_disabled_before_q0_enable: false,
            mdic_writes: 0,
            last_mdic: 0,
            set_status_lu_on_mdic: false,
            transmitted_packets: Vec::new(),
            time_us: 1_000,
        }
    }

    fn decode_rx_queue_offset(&self, off: u64) -> Option<(usize, u64)> {
        match self.variant {
            IgbVariant::I82575 => {
                if (0x2800..0x2C00).contains(&off) {
                    Some((((off - 0x2800) / 0x100) as usize, (off - 0x2800) % 0x100))
                } else {
                    None
                }
            }
            IgbVariant::I82576 => {
                if (0x2800..0x2C00).contains(&off) {
                    Some((((off - 0x2800) / 0x100) as usize, (off - 0x2800) % 0x100))
                } else if (0xC100..0xC400).contains(&off) {
                    // Queues 4..15 at 0xC000 + q * 0x40 => 0xC100..0xC400
                    Some((((off - 0xC000) / 0x40) as usize, (off - 0xC000) % 0x40))
                } else {
                    None
                }
            }
            IgbVariant::I82580 | IgbVariant::I210 => {
                let max = 0xC000 + (self.queues.len() as u64) * 0x40;
                if (0xC000..max).contains(&off) {
                    Some((((off - 0xC000) / 0x40) as usize, (off - 0xC000) % 0x40))
                } else {
                    None
                }
            }
        }
    }

    fn decode_tx_queue_offset(&self, off: u64) -> Option<(usize, u64)> {
        match self.variant {
            IgbVariant::I82575 => {
                if (0x3800..0x3C00).contains(&off) {
                    Some((((off - 0x3800) / 0x100) as usize, (off - 0x3800) % 0x100))
                } else {
                    None
                }
            }
            IgbVariant::I82576 => {
                if (0x3800..0x3C00).contains(&off) {
                    Some((((off - 0x3800) / 0x100) as usize, (off - 0x3800) % 0x100))
                } else if (0xE100..0xE400).contains(&off) {
                    Some((((off - 0xE000) / 0x40) as usize, (off - 0xE000) % 0x40))
                } else {
                    None
                }
            }
            IgbVariant::I82580 | IgbVariant::I210 => {
                let max = 0xE000 + (self.queues.len() as u64) * 0x40;
                if (0xE000..max).contains(&off) {
                    Some((((off - 0xE000) / 0x40) as usize, (off - 0xE000) % 0x40))
                } else {
                    None
                }
            }
        }
    }

    fn rx_ring_base(&self) -> u64 {
        (self.queues[0].rdbal as u64) | ((self.queues[0].rdbah as u64) << 32)
    }

    fn tx_ring_base(&self) -> u64 {
        (self.queues[0].tdbal as u64) | ((self.queues[0].tdbah as u64) << 32)
    }

    fn inject_rx_packet(&mut self, frame: &[u8]) {
        assert_ne!(self.rctl & (1 << 1), 0, "RCTL.EN must be set");
        assert_ne!(
            self.queues[0].rxdctl & RXDCTL_QUEUE_ENABLE,
            0,
            "RXDCTL0.ENABLE must be set"
        );
        let idx = self.queues[0].rdh as usize;
        let desc_addr = self.rx_ring_base() + (idx * core::mem::size_of::<AdvRxDesc>()) as u64;
        let mut raw = [0u8; 16];
        raw.copy_from_slice(self.dma.slice(desc_addr, 16));
        let buf_phys = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        self.dma
            .slice_mut(buf_phys, frame.len())
            .copy_from_slice(frame);
        let desc_mut = self.dma.slice_mut(desc_addr, 16);
        desc_mut[0..8].fill(0);
        desc_mut[8..12].copy_from_slice(&0x0000_0003u32.to_le_bytes()); // DD | EOP in status_error
        desc_mut[12..14].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        desc_mut[14..16].fill(0);
        self.queues[0].rdh = ((idx + 1) % NUM_RX_DESC) as u32;
    }

    fn process_tx_tail(&mut self, new_tdt: u32) {
        while self.queues[0].tdh != new_tdt {
            let idx = self.queues[0].tdh as usize;
            let desc_addr = self.tx_ring_base() + (idx * core::mem::size_of::<TxDesc>()) as u64;
            let b = self.dma.slice(desc_addr, 16);
            let buf_phys = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let len = u16::from_le_bytes(b[8..10].try_into().unwrap()) as usize;
            let pkt = self.dma.slice(buf_phys, len).to_vec();
            self.transmitted_packets.push(pkt);
            self.dma.slice_mut(desc_addr, 16)[12] |= 1; // Set DD
            self.queues[0].tdh = ((idx + 1) % NUM_TX_DESC) as u32;
        }
        self.queues[0].tdt = new_tdt;
    }

    fn mmio_read32(&mut self, addr: u64) -> u32 {
        let off = addr
            .checked_sub(IGB_MMIO_BASE)
            .expect("IGB MMIO below base");
        if let Some((q, reg)) = self.decode_rx_queue_offset(off) {
            return match reg {
                0x00 => self.queues[q].rdbal,
                0x04 => self.queues[q].rdbah,
                0x08 => self.queues[q].rdlen,
                0x0C => self.queues[q].srrctl,
                0x10 => self.queues[q].rdh,
                0x18 => self.queues[q].rdt,
                0x28 => self.queues[q].rxdctl,
                _ => 0,
            };
        }
        if let Some((q, reg)) = self.decode_tx_queue_offset(off) {
            return match reg {
                0x00 => self.queues[q].tdbal,
                0x04 => self.queues[q].tdbah,
                0x08 => self.queues[q].tdlen,
                0x10 => self.queues[q].tdh,
                0x18 => self.queues[q].tdt,
                0x28 => self.queues[q].txdctl,
                _ => 0,
            };
        }
        match off {
            0x0000 => self.ctrl,
            0x0008 => self.status,
            0x0018 => self.ctrl_ext,
            0x0020 => 1 << 28, // MDIC.READY
            0x00C0 => {
                self.legacy_icr_reads += 1;
                0
            }
            0x00D8 => self.legacy_imc,
            0x0100 => self.rctl,
            0x0400 => self.tctl,
            0x0410 => self.tipg,
            0x1500 => {
                self.icr_reads += 1;
                0
            }
            0x1508 => self.ims,
            0x150C => self.imc,
            0x1528 => self.eimc,
            0x1580 => {
                self.eicr_reads += 1;
                0
            }
            0x5000 => self.rxcsum,
            0x5008 => self.rfctl,
            0x5400 => self.ral0,
            0x5404 => self.rah0,
            0x5818 => self.mrqc,
            _ => 0,
        }
    }

    fn mmio_write32(&mut self, addr: u64, val: u32) {
        let off = addr
            .checked_sub(IGB_MMIO_BASE)
            .expect("IGB MMIO below base");
        if let Some((q, reg)) = self.decode_rx_queue_offset(off) {
            match reg {
                0x00 => self.queues[q].rdbal = val,
                0x04 => self.queues[q].rdbah = val,
                0x08 => self.queues[q].rdlen = val,
                0x0C => self.queues[q].srrctl = val,
                0x10 => self.queues[q].rdh = val,
                0x18 => {
                    if val == 0 || (self.rctl & (1 << 1)) != 0 {
                        self.queues[q].rdt = val;
                    }
                }
                0x28 => {
                    if q == 0 && (val & RXDCTL_QUEUE_ENABLE) != 0 {
                        self.all_queues_disabled_before_q0_enable = self.queues.iter().all(|qr| {
                            (qr.rxdctl & RXDCTL_QUEUE_ENABLE) == 0
                                && (qr.txdctl & TXDCTL_QUEUE_ENABLE) == 0
                        });
                    }
                    self.queues[q].rxdctl = val;
                }
                _ => {}
            }
            return;
        }
        if let Some((q, reg)) = self.decode_tx_queue_offset(off) {
            match reg {
                0x00 => self.queues[q].tdbal = val,
                0x04 => self.queues[q].tdbah = val,
                0x08 => self.queues[q].tdlen = val,
                0x10 => self.queues[q].tdh = val,
                0x18 => {
                    if q == 0 && (self.tctl & (1 << 1)) != 0 {
                        self.process_tx_tail(val);
                    } else {
                        self.queues[q].tdt = val;
                    }
                }
                0x28 => self.queues[q].txdctl = val,
                _ => {}
            }
            return;
        }
        match off {
            0x0000 => {
                if (val & ((1 << 26) | (1 << 29) | (1 << 31))) != 0 {
                    self.ctrl_rst_asserted = true;
                }
                self.ctrl = val;
            }
            0x0018 => self.ctrl_ext = val,
            0x0020 => {
                self.mdic_writes += 1;
                self.last_mdic = val;
                if self.set_status_lu_on_mdic {
                    self.status |= 1 << 1;
                }
            }
            0x00D8 => self.legacy_imc = val,
            0x0100 => {
                if (val & (1 << 1)) != 0 && (self.queues[0].rxdctl & RXDCTL_QUEUE_ENABLE) != 0 {
                    self.rxdctl0_enabled_before_rctl_en = true;
                }
                self.rctl = val;
            }
            0x0400 => {
                if (val & (1 << 1)) != 0 && (self.queues[0].txdctl & TXDCTL_QUEUE_ENABLE) != 0 {
                    self.txdctl0_enabled_before_tctl_en = true;
                }
                self.tctl = val;
            }
            0x0410 => self.tipg = val,
            0x1508 => self.ims |= val,
            0x150C => self.imc = val,
            0x1528 => self.eimc = val,
            0x5000 => self.rxcsum = val,
            0x5008 => self.rfctl = val,
            0x5400 => self.ral0 = val,
            0x5404 => self.rah0 = val,
            0x5818 => self.mrqc = val,
            _ => {}
        }
    }
}

#[derive(Clone)]
struct MockIgbHal {
    state: Arc<Mutex<IgbSimState>>,
}

impl MockIgbHal {
    fn new(variant: IgbVariant, ral0: u32, rah0: u32) -> Self {
        Self {
            state: Arc::new(Mutex::new(IgbSimState::new_dirty_handover(
                variant, ral0, rah0,
            ))),
        }
    }
}

impl Hal for MockIgbHal {
    fn mmio_read8(&self, _addr: u64) -> u8 {
        0
    }
    fn mmio_write8(&self, _addr: u64, _val: u8) {}
    fn mmio_read16(&self, _addr: u64) -> u16 {
        0
    }
    fn mmio_write16(&self, _addr: u64, _val: u16) {}
    fn mmio_read32(&self, addr: u64) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mmio_read32(addr)
    }
    fn mmio_write32(&self, addr: u64, val: u32) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mmio_write32(addr, val);
    }
    fn mmio_read64(&self, addr: u64) -> u64 {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (st.mmio_read32(addr) as u64) | ((st.mmio_read32(addr + 4) as u64) << 32)
    }
    fn mmio_write64(&self, addr: u64, val: u64) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dma
            .alloc(pages)
    }
    fn free_dma(&self, buf: DmaBuffer) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dma
            .free(buf);
    }
    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dma
            .phys_to_virt(phys)
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
        bus: 1,
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
fn test_igb_probe_variants_and_disjointness() {
    let cases = [
        (0x10A7, IgbVariant::I82575),
        (0x10A9, IgbVariant::I82575),
        (0x10D6, IgbVariant::I82575),
        (0x10C9, IgbVariant::I82576),
        (0x10E6, IgbVariant::I82576),
        (0x1526, IgbVariant::I82576),
        (0x150E, IgbVariant::I82580),
        (0x1521, IgbVariant::I82580),
        (0x1546, IgbVariant::I82580),
        (0x1F41, IgbVariant::I82580),
        (0x0438, IgbVariant::I82580),
        (0x1533, IgbVariant::I210),
        (0x1539, IgbVariant::I210),
        (0x157B, IgbVariant::I210),
    ];

    for (id, expected) in cases {
        let pci = make_intel_pci(id);
        assert!(igb::probe(&pci), "igb::probe must claim {:#06x}", id);
        assert_eq!(igb::probe_variant(&pci), Some(expected));
        assert!(
            !e1000::probe(&pci),
            "e1000::probe must not claim igb device {:#06x}",
            id
        );
        assert!(
            !igc::probe(&pci),
            "igc::probe must not claim igb device {:#06x}",
            id
        );
        assert!(
            rtl::probe(&pci).is_none(),
            "rtl::probe must not claim igb device {:#06x}",
            id
        );
    }

    // Non-Ethernet class must be rejected
    assert!(!igb::probe(&PciDevice {
        class_code: 0x01,
        ..make_intel_pci(0x1533)
    }));
}

fn verify_dirty_handover_and_io(device_id: u16, variant: IgbVariant) {
    assert_eq!(NUM_RX_DESC, 1024);
    let ral0 = 0x1200_5452;
    let rah0 = 0x8000_5634;
    let hal = MockIgbHal::new(variant, ral0, rah0);
    let mut dev = IgbDevice::new(make_intel_pci(device_id), IGB_MMIO_BASE, None, hal.clone())
        .expect("IgbDevice::new should succeed");

    assert_eq!(dev.mac_address(), [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    assert!(dev.is_link_up());

    {
        let st = hal.state.lock().unwrap();
        assert!(
            !st.ctrl_rst_asserted,
            "IgbDevice must preserve UEFI/PHY link and NOT write CTRL.RST/DEV_RST/PHY_RST"
        );
        assert_eq!(st.ctrl & (1 << 3), 0, "CTRL.LRST must be cleared");
        assert_eq!(st.ctrl & (1 << 30), 0, "CTRL.VME must be cleared");
        assert_ne!(st.ctrl & (1 << 6), 0, "CTRL.SLU must be set");
        assert_eq!(
            st.mdic_writes, 0,
            "MDIC must not be touched when link is already up"
        );
        assert_ne!(st.ctrl_ext & (1 << 28), 0, "CTRL_EXT.DRV_LOAD must be set");
        assert_eq!(st.ims, 0, "IMS (0x1508) must never be written");
        assert_eq!(st.eimc, 0xFFFF_FFFF, "EIMC (0x1528) must be masked");
        match variant {
            IgbVariant::I82575 => {
                assert_eq!(st.legacy_imc, 0xFFFF_FFFF);
                assert!(st.legacy_icr_reads > 0);
            }
            IgbVariant::I82576 => {
                assert_eq!(st.imc, 0xFFFF_FFFF);
                assert_eq!(st.legacy_imc, 0xFFFF_FFFF);
                assert!(st.icr_reads > 0 && st.legacy_icr_reads > 0);
            }
            IgbVariant::I82580 | IgbVariant::I210 => {
                assert_eq!(st.imc, 0xFFFF_FFFF);
                assert!(st.icr_reads > 0);
            }
        }
        assert!(st.eicr_reads > 0);
        assert!(
            st.all_queues_disabled_before_q0_enable,
            "All queues (including non-zero queues left by Linux igb) must be disabled before enabling Q0"
        );
        for (q, qr) in st.queues.iter().enumerate().skip(1) {
            assert_eq!(
                qr.rxdctl & RXDCTL_QUEUE_ENABLE,
                0,
                "Secondary RX queue {} must stay disabled",
                q
            );
            assert_eq!(
                qr.txdctl & TXDCTL_QUEUE_ENABLE,
                0,
                "Secondary TX queue {} must stay disabled",
                q
            );
        }
        assert_eq!(st.mrqc, 0, "MRQC must be cleared");
        assert_eq!(st.rxcsum, 0, "RXCSUM must be cleared");
        assert_eq!(st.rfctl & (1 << 15), 0, "RFCTL.EXSTEN must be cleared");
        assert_eq!(
            st.queues[0].srrctl,
            2 | (1 << 25) | (1 << 31),
            "SRRCTL0 must be BSIZEPKT=2 | DESCTYPE_ADV_ONEBUF | DROP_EN"
        );
        assert!(st.rxdctl0_enabled_before_rctl_en);
        assert!(st.txdctl0_enabled_before_tctl_en);
        assert_eq!(
            st.queues[0].txdctl,
            0x1F | (1 << 8) | (1 << 16) | TXDCTL_QUEUE_ENABLE
        );
        assert_eq!(
            st.queues[0].rdt,
            (NUM_RX_DESC - 1) as u32,
            "RDT0 must be written to NUM_RX_DESC - 1 after RCTL.EN"
        );
    }

    // Exercise wraparound across > NUM_RX_DESC (1024) and > NUM_TX_DESC (128) packets
    for i in 0..1035usize {
        let mut frame = vec![0u8; 64];
        frame[0..6].copy_from_slice(&[0xFF; 6]);
        frame[6..12].copy_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[14] = (i & 0xFF) as u8;
        frame[15] = ((i >> 8) & 0xFF) as u8;

        assert!(dev.can_transmit());
        dev.transmit(&frame).expect("IgbDevice TX should succeed");

        hal.state.lock().unwrap().inject_rx_packet(&frame);
        assert!(dev.has_packets());
        let mut rx_buf = [0u8; 128];
        let n = dev.receive(&mut rx_buf).expect("Expected RX frame");
        assert_eq!(&rx_buf[..n], &frame[..]);
        assert!(!dev.has_packets());
    }

    drop(dev);
    let st = hal.state.lock().unwrap();
    assert_eq!(st.transmitted_packets.len(), 1035);
    assert_eq!(st.rctl, 0, "Drop must disable RCTL");
    assert_eq!(st.tctl, 0, "Drop must disable TCTL");
    assert_eq!(st.queues[0].rxdctl, 0, "Drop must disable RXDCTL0");
    assert_eq!(st.queues[0].txdctl, 0, "Drop must disable TXDCTL0");
}

#[test]
fn test_igb_82575_init_and_wraparound() {
    verify_dirty_handover_and_io(0x10A7, IgbVariant::I82575);
}

#[test]
fn test_igb_82576_16queues_init_and_wraparound() {
    verify_dirty_handover_and_io(0x10C9, IgbVariant::I82576);
}

#[test]
fn test_igb_i350_82580_layout_init_and_wraparound() {
    verify_dirty_handover_and_io(0x1521, IgbVariant::I82580);
}

#[test]
fn test_igb_i210_t1_layout_init_and_wraparound() {
    verify_dirty_handover_and_io(0x1533, IgbVariant::I210);
}

#[test]
fn test_igb_cold_boot_clears_lrst_and_restarts_autoneg() {
    let ral0 = 0x1200_5452;
    let rah0 = 0x8000_5634;
    let hal = MockIgbHal::new(IgbVariant::I82576, ral0, rah0);
    {
        let mut st = hal.state.lock().unwrap();
        st.status = 0x8000_0400; // Cold boot: STATUS.LU == 0
        st.ctrl = 0x0010_0209; // Power-on default with CTRL.LRST (bit 3) set
        st.set_status_lu_on_mdic = true;
    }

    let dev = IgbDevice::new(make_intel_pci(0x10C9), IGB_MMIO_BASE, None, hal.clone())
        .expect("IgbDevice::new should bring link up from cold boot");
    assert!(dev.is_link_up());

    let st = hal.state.lock().unwrap();
    assert_eq!(st.ctrl & (1 << 3), 0, "CTRL.LRST must be cleared");
    assert_ne!(st.ctrl & (1 << 6), 0, "CTRL.SLU must be set");
    assert_eq!(st.mdic_writes, 1, "MDIC should be written once on LU==0");
    assert_eq!(st.last_mdic, (1 << 26) | (1 << 21) | 0x1340);
}
