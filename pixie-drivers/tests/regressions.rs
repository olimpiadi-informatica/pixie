mod common;

use common::{DmaPool, block_on};
use core::future::Future;
use core::time::Duration;
use pixie_drivers::acpi::AcpiInfo;
use pixie_drivers::disk::ahci::AhciDisk;
use pixie_drivers::disk::nvme::NvmeDisk;
use pixie_drivers::hal::{DmaBuffer, Hal};
use pixie_drivers::input::KeyEvent;
use pixie_drivers::input::ps2::Ps2Keyboard;
use pixie_drivers::input::serial::SerialPort;
use pixie_drivers::input::xhci::XhciKeyboard;
use pixie_drivers::net::rtl::RtlDevice;
use pixie_drivers::net::usb_net::{
    RTL_RX_DESC_LEN, UsbEthernetDevice, UsbNetProtocol, decode_rtl8153_rx_frame,
};
use pixie_drivers::pci::PciDevice;
use pixie_drivers::xhci::{PORTSC_CCS, PORTSC_PED, PORTSC_PP, XhciController};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ============================================================================
// 1. Shared xHCI Controller: Simultaneous CDC-ECM NIC + HID Boot Keyboard
// ============================================================================

const SHARED_XHCI_BASE: u64 = 0xFEBE_0000;
const SHARED_DMA_BASE: u64 = 0x7000_0000;

struct SlotRings {
    ep0_phys: u64,
    ep0_idx: usize,
    ep0_cyc: u32,
    data_in_phys: u64,
    data_in_idx: usize,
    data_in_cyc: u32,
    data_in_dci: u8,
    data_out_phys: u64,
    data_out_idx: usize,
    data_out_cyc: u32,
    port_num: u8,
}

impl SlotRings {
    fn new() -> Self {
        Self {
            ep0_phys: 0,
            ep0_idx: 0,
            ep0_cyc: 1,
            data_in_phys: 0,
            data_in_idx: 0,
            data_in_cyc: 1,
            data_in_dci: 0,
            data_out_phys: 0,
            data_out_idx: 0,
            data_out_cyc: 1,
            port_num: 0,
        }
    }
}

struct SharedXhciSim {
    dma: DmaPool,
    usbcmd: u32,
    usbsts: u32,
    crcr: u64,
    dcbaap: u64,
    erstba: u64,
    erstsz: u32,
    erdp: u64,
    cmd_idx: usize,
    cmd_cyc: u32,
    evt_idx: usize,
    evt_cyc: u32,
    next_slot: u8,
    slots: [SlotRings; 4],
    portsc: [u32; 2],
    tx_packets: Vec<Vec<u8>>,
    time_us: i64,
}

impl SharedXhciSim {
    fn new() -> Self {
        let up = PORTSC_CCS | PORTSC_PED | PORTSC_PP | (3 << 10);
        Self {
            dma: DmaPool::new(SHARED_DMA_BASE, 512),
            usbcmd: 0,
            usbsts: 1,
            crcr: 0,
            dcbaap: 0,
            erstba: 0,
            erstsz: 0,
            erdp: 0,
            cmd_idx: 0,
            cmd_cyc: 1,
            evt_idx: 0,
            evt_cyc: 1,
            next_slot: 0,
            slots: [
                SlotRings::new(),
                SlotRings::new(),
                SlotRings::new(),
                SlotRings::new(),
            ],
            portsc: [up, up],
            tx_packets: Vec::new(),
            time_us: 0,
        }
    }

    fn post_event(&mut self, param: u64, status: u32, ctrl_base: u32) {
        let seg = self.dma.slice(self.erstba & !0x3F, 16);
        let ring = u64::from_le_bytes(seg[0..8].try_into().unwrap());
        let size = (u32::from_le_bytes(seg[8..12].try_into().unwrap()) as usize).max(16);
        let addr = ring + (self.evt_idx * 16) as u64;
        let dst = self.dma.slice_mut(addr, 16);
        dst[0..8].copy_from_slice(&param.to_le_bytes());
        dst[8..12].copy_from_slice(&status.to_le_bytes());
        dst[12..16].copy_from_slice(&((ctrl_base & !1) | (self.evt_cyc & 1)).to_le_bytes());
        self.evt_idx += 1;
        if self.evt_idx >= size {
            self.evt_idx = 0;
            self.evt_cyc ^= 1;
        }
    }

    fn next_trb(&self, ring: u64, idx: &mut usize, cyc: &mut u32) -> Option<(u64, [u8; 16])> {
        if ring == 0 {
            return None;
        }
        for _ in 0..8 {
            let addr = ring + (*idx * 16) as u64;
            let trb: [u8; 16] = self.dma.slice(addr, 16).try_into().unwrap();
            let ctrl = u32::from_le_bytes(trb[12..16].try_into().unwrap());
            if (ctrl & 1) != *cyc {
                return None;
            }
            if ((ctrl >> 10) & 0x3F) == 6 {
                *idx = 0;
                if (ctrl & (1 << 1)) != 0 {
                    *cyc ^= 1;
                }
                continue;
            }
            *idx += 1;
            return Some((addr, trb));
        }
        None
    }

    fn process_cmd_ring(&mut self) {
        let base = self.crcr & !0x3F;
        if base == 0 {
            return;
        }
        while let Some((trb_phys, trb)) =
            self.next_trb(base, &mut self.cmd_idx.clone(), &mut self.cmd_cyc.clone())
        {
            // Advance actual cmd cursor cleanly
            let mut idx = self.cmd_idx;
            let mut cyc = self.cmd_cyc;
            self.next_trb(base, &mut idx, &mut cyc);
            self.cmd_idx = idx;
            self.cmd_cyc = cyc;
            let _ = trb_phys;

            let param = u64::from_le_bytes(trb[0..8].try_into().unwrap());
            let ctrl = u32::from_le_bytes(trb[12..16].try_into().unwrap());
            let ty = (ctrl >> 10) & 0x3F;
            let mut slot_id = ((ctrl >> 24) & 0xFF) as u8;
            match ty {
                9 => {
                    self.next_slot += 1;
                    slot_id = self.next_slot;
                }
                11 | 12 if param >= SHARED_DMA_BASE => {
                    let s = slot_id as usize;
                    let slot_dw1 =
                        u32::from_le_bytes(self.dma.slice(param + 36, 4).try_into().unwrap());
                    let port_num = ((slot_dw1 >> 16) & 0xFF) as u8;
                    if port_num > 0 {
                        self.slots[s].port_num = port_num;
                    }
                    for ctx in 2..32 {
                        let ep = self.dma.slice(param + (ctx * 32) as u64, 16);
                        let dw1 = u32::from_le_bytes(ep[4..8].try_into().unwrap());
                        let ep_ty = (dw1 >> 3) & 7;
                        let deq_raw = u64::from_le_bytes(ep[8..16].try_into().unwrap());
                        let deq = deq_raw & !0xF;
                        let dcs = (deq_raw & 1) as u32;
                        if deq >= SHARED_DMA_BASE {
                            match ep_ty {
                                4 => {
                                    self.slots[s].ep0_phys = deq;
                                    self.slots[s].ep0_idx = 0;
                                    self.slots[s].ep0_cyc = dcs;
                                }
                                6 | 7 => {
                                    self.slots[s].data_in_phys = deq;
                                    self.slots[s].data_in_idx = 0;
                                    self.slots[s].data_in_cyc = dcs;
                                    self.slots[s].data_in_dci = (ctx - 1) as u8;
                                }
                                2 => {
                                    self.slots[s].data_out_phys = deq;
                                    self.slots[s].data_out_idx = 0;
                                    self.slots[s].data_out_cyc = dcs;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
            self.post_event(
                trb_phys,
                1u32 << 24,
                ((slot_id as u32) << 24) | (33u32 << 10),
            );
        }
    }

    fn process_doorbell(&mut self, slot_id: u8, dci: u32) {
        let s = slot_id as usize;
        if dci == 1 {
            let ring = self.slots[s].ep0_phys;
            let mut idx = self.slots[s].ep0_idx;
            let mut cyc = self.slots[s].ep0_cyc;
            while let Some((_, setup_trb)) = self.next_trb(ring, &mut idx, &mut cyc) {
                let setup: [u8; 8] = setup_trb[0..8].try_into().unwrap();
                let w_len = u16::from_le_bytes([setup[6], setup[7]]) as usize;
                if w_len > 0 {
                    let (_, data_trb) = self.next_trb(ring, &mut idx, &mut cyc).unwrap();
                    let buf = u64::from_le_bytes(data_trb[0..8].try_into().unwrap());
                    if (setup[0] & 0x80) != 0 && setup[1] == 0x06 {
                        let dtype = setup[3];
                        if dtype == 1 {
                            let dev =
                                [18, 1, 0, 2, 0, 0, 0, 64, 0x6B, 0x1D, 4, 1, 0, 1, 1, 2, 0, 1];
                            let n = w_len.min(dev.len());
                            self.dma.slice_mut(buf, n).copy_from_slice(&dev[..n]);
                        } else if dtype == 2 {
                            let port = self.slots[s].port_num;
                            let cfg: &[u8] = if port == 1 {
                                &[
                                    9, 2, 32, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 2, 0x02, 0x06, 0,
                                    0, // CDC-ECM
                                    7, 5, 0x81, 0x02, 0x00, 0x02, 0, 7, 5, 0x02, 0x02, 0x00, 0x02,
                                    0,
                                ]
                            } else {
                                &[
                                    9, 2, 34, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 1, 0x03, 0x01,
                                    0x01, 0, // HID Boot Keyboard
                                    9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0, 7, 5, 0x82, 0x03, 8, 0,
                                    10, // EP2 IN (DCI 5)
                                ]
                            };
                            let n = w_len.min(cfg.len());
                            self.dma.slice_mut(buf, n).copy_from_slice(&cfg[..n]);
                        }
                    }
                }
                let (st_phys, _) = self.next_trb(ring, &mut idx, &mut cyc).unwrap();
                self.slots[s].ep0_idx = idx;
                self.slots[s].ep0_cyc = cyc;
                self.post_event(
                    st_phys,
                    1u32 << 24,
                    ((slot_id as u32) << 24) | (1u32 << 16) | (32u32 << 10),
                );
            }
        } else if dci == 4 && self.slots[s].data_out_phys != 0 {
            let ring = self.slots[s].data_out_phys;
            let mut idx = self.slots[s].data_out_idx;
            let mut cyc = self.slots[s].data_out_cyc;
            while let Some((trb_phys, trb)) = self.next_trb(ring, &mut idx, &mut cyc) {
                let ptr = u64::from_le_bytes(trb[0..8].try_into().unwrap());
                let len = (u32::from_le_bytes(trb[8..12].try_into().unwrap()) & 0x1FFFF) as usize;
                self.tx_packets.push(self.dma.slice(ptr, len).to_vec());
                self.post_event(
                    trb_phys,
                    1u32 << 24,
                    ((slot_id as u32) << 24) | (4u32 << 16) | (32u32 << 10),
                );
            }
            self.slots[s].data_out_idx = idx;
            self.slots[s].data_out_cyc = cyc;
        }
    }

    fn inject_port_in(&mut self, port_num: u8, payload: &[u8]) {
        let s = (1..=3)
            .find(|&i| self.slots[i].port_num == port_num)
            .unwrap();
        let ring = self.slots[s].data_in_phys;
        let mut idx = self.slots[s].data_in_idx;
        let mut cyc = self.slots[s].data_in_cyc;
        let (trb_phys, trb) = self.next_trb(ring, &mut idx, &mut cyc).unwrap();
        self.slots[s].data_in_idx = idx;
        self.slots[s].data_in_cyc = cyc;
        let buf = u64::from_le_bytes(trb[0..8].try_into().unwrap());
        let len = (u32::from_le_bytes(trb[8..12].try_into().unwrap()) & 0x1FFFF) as usize;
        self.dma
            .slice_mut(buf, payload.len())
            .copy_from_slice(payload);
        let residual = (len - payload.len()) as u32;
        let dci = self.slots[s].data_in_dci as u32;
        self.post_event(
            trb_phys,
            (13u32 << 24) | (residual & 0x00FF_FFFF),
            ((s as u32) << 24) | (dci << 16) | (32u32 << 10),
        );
    }
}

#[derive(Clone)]
struct SharedHal(Arc<Mutex<SharedXhciSim>>);

impl Hal for SharedHal {
    fn mmio_read8(&self, a: u64) -> u8 {
        ((self.mmio_read32(a & !3) >> ((a & 3) * 8)) & 0xFF) as u8
    }
    fn mmio_write8(&self, _: u64, _: u8) {}
    fn mmio_read16(&self, a: u64) -> u16 {
        ((self.mmio_read32(a & !3) >> ((a & 2) * 8)) & 0xFFFF) as u16
    }
    fn mmio_write16(&self, _: u64, _: u16) {}
    fn mmio_read32(&self, addr: u64) -> u32 {
        let st = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match addr - SHARED_XHCI_BASE {
            0x000 => 0x0100_0020,
            0x004 => (2 << 24) | (1 << 8) | 8,
            0x014 => 0x0000_0300,
            0x018 => 0x0000_0600,
            0x020 => st.usbcmd,
            0x024 => st.usbsts,
            0x028 => 1,
            0x420 => st.portsc[0],
            0x430 => st.portsc[1],
            0x628 => st.erstsz,
            _ => 0,
        }
    }
    fn mmio_write32(&self, addr: u64, val: u32) {
        let mut st = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match addr - SHARED_XHCI_BASE {
            0x020 => {
                if (val & 2) != 0 {
                    st.usbcmd = val & !2;
                    st.usbsts = 1;
                } else {
                    st.usbcmd = val;
                    if (val & 1) != 0 {
                        st.usbsts &= !1;
                    } else {
                        st.usbsts |= 1;
                    }
                }
            }
            0x038 => st.crcr = (st.crcr & !0xFFFF_FFFF) | val as u64,
            0x03C => st.crcr = (st.crcr & 0xFFFF_FFFF) | ((val as u64) << 32),
            0x050 => st.dcbaap = (st.dcbaap & !0xFFFF_FFFF) | val as u64,
            0x054 => st.dcbaap = (st.dcbaap & 0xFFFF_FFFF) | ((val as u64) << 32),
            0x300 => st.process_cmd_ring(),
            db if (0x304..0x320).contains(&db) => {
                st.process_doorbell(((db - 0x300) / 4) as u8, val & 0xFF);
            }
            0x628 => st.erstsz = val,
            0x630 => st.erstba = (st.erstba & !0xFFFF_FFFF) | val as u64,
            0x634 => st.erstba = (st.erstba & 0xFFFF_FFFF) | ((val as u64) << 32),
            0x638 => st.erdp = (st.erdp & !0xFFFF_FFFF) | val as u64,
            0x63C => st.erdp = (st.erdp & 0xFFFF_FFFF) | ((val as u64) << 32),
            _ => {}
        }
    }
    fn mmio_read64(&self, a: u64) -> u64 {
        (self.mmio_read32(a) as u64) | ((self.mmio_read32(a + 4) as u64) << 32)
    }
    fn mmio_write64(&self, a: u64, v: u64) {
        self.mmio_write32(a, v as u32);
        self.mmio_write32(a + 4, (v >> 32) as u32);
    }
    fn io_read8(&self, _: u16) -> u8 {
        0
    }
    fn io_write8(&self, _: u16, _: u8) {}
    fn io_read16(&self, _: u16) -> u16 {
        0
    }
    fn io_write16(&self, _: u16, _: u16) {}
    fn io_read32(&self, _: u16) -> u32 {
        0
    }
    fn io_write32(&self, _: u16, _: u32) {}
    fn alloc_dma(&self, pages: usize) -> Option<DmaBuffer> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dma
            .alloc(pages)
    }
    fn free_dma(&self, buf: DmaBuffer) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dma
            .free(buf);
    }
    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dma
            .phys_to_virt(phys)
    }
    fn micros(&self) -> i64 {
        let mut st = self.0.lock().unwrap_or_else(|e| e.into_inner());
        st.time_us += 10;
        st.time_us
    }
    fn yield_now(&self) -> impl Future<Output = ()> {
        core::future::ready(())
    }
    fn sleep(&self, _: Duration) -> impl Future<Output = ()> {
        core::future::ready(())
    }
}

#[test]
fn test_shared_xhci_ethernet_and_keyboard_simultaneous() {
    let sim = Arc::new(Mutex::new(SharedXhciSim::new()));
    let hal = SharedHal(sim.clone());
    let ctrl = XhciController::new_shared(SHARED_XHCI_BASE, hal.clone()).expect("Shared xHCI init");

    let mut nic = UsbEthernetDevice::attach(ctrl.clone(), Some([0x02, 1, 2, 3, 4, 5]))
        .expect("Attach USB NIC on Port 1");
    let mut kbd =
        block_on(XhciKeyboard::attach(ctrl.clone())).expect("Attach USB Keyboard on Port 2");

    assert_eq!(nic.protocol(), UsbNetProtocol::CdcEcm);

    // Interleave events from BOTH devices on the shared Event Ring before polling either!
    let eth_pkt = b"\xff\xff\xff\xff\xff\xff\x02\x01\x02\x03\x04\x05\x08\x00hello-shared-xhci";
    sim.lock()
        .unwrap()
        .inject_port_in(2, &[0, 0, 0x04, 0, 0, 0, 0, 0]); // 'a'
    sim.lock().unwrap().inject_port_in(1, eth_pkt);

    // NIC receives only its Ethernet packet (keyboard events buffered cleanly for kbd)
    let mut rx_buf = [0u8; 128];
    let n = nic.receive(&mut rx_buf).expect("NIC must receive eth_pkt");
    assert_eq!(&rx_buf[..n], eth_pkt);
    assert_eq!(nic.receive(&mut rx_buf), None);

    // Keyboard receives 'a' (re-arming Interrupt-IN TRB), then receives 'B' after next report
    assert_eq!(kbd.poll_key(), Some(KeyEvent::Char('a')));
    sim.lock()
        .unwrap()
        .inject_port_in(2, &[0x02, 0, 0x05, 0, 0, 0, 0, 0]); // 'B'
    assert_eq!(kbd.poll_key(), Some(KeyEvent::Char('B')));
    assert_eq!(kbd.poll_key(), None);

    nic.transmit(eth_pkt).expect("NIC TX works alongside kbd");
    assert!(nic.can_transmit());
    assert_eq!(sim.lock().unwrap().tx_packets, vec![eth_pkt.to_vec()]);

    drop(kbd);
    drop(nic);
    drop(ctrl);
    let st = sim.lock().unwrap();
    assert_eq!(st.dma.freed_pages(), st.dma.allocated_pages());
}

// ============================================================================
// 2. RTL8153 24-byte Multi-Frame Aggregation & OOB Rejection
// ============================================================================

#[test]
fn test_rtl8153_24byte_oob_and_truncation_guards() {
    // Valid 24B header + frame + 4B FCS
    let frame = b"\xff\xff\xff\xff\xff\xff\x00\x11\x22\x33\x44\x55\x08\x00test-frame";
    let mut good = vec![0u8; RTL_RX_DESC_LEN];
    good[0..4].copy_from_slice(&((frame.len() + 4) as u32).to_le_bytes());
    good.extend_from_slice(frame);
    good.extend_from_slice(&[1, 2, 3, 4]);
    assert_eq!(decode_rtl8153_rx_frame(&good), Some(frame.as_slice()));

    // OOB raw_len > available transfer bytes -> None
    let mut bad_oob = vec![0u8; RTL_RX_DESC_LEN + 20];
    bad_oob[0..4].copy_from_slice(&500u32.to_le_bytes());
    assert_eq!(decode_rtl8153_rx_frame(&bad_oob), None);

    // Short transfer (< 24 + 4 + 1) -> None
    assert_eq!(decode_rtl8153_rx_frame(&[0u8; RTL_RX_DESC_LEN + 4]), None);
}

// ============================================================================
// 3. PS/2 L+R Shift, Pause (0xE1), and Serial Multi-Byte CSI + CRLF
// ============================================================================

struct InputMockHal {
    ps2: Mutex<VecDeque<(u8, u8)>>,
    uart_rx: Mutex<VecDeque<u8>>,
    mcr: Mutex<u8>,
    loopback: Mutex<u8>,
    time_us: Mutex<i64>,
}

impl InputMockHal {
    fn new(ps2: Vec<(u8, u8)>, uart_rx: Vec<u8>) -> Self {
        Self {
            ps2: Mutex::new(VecDeque::from(ps2)),
            uart_rx: Mutex::new(VecDeque::from(uart_rx)),
            mcr: Mutex::new(0),
            loopback: Mutex::new(0),
            time_us: Mutex::new(0),
        }
    }
}

impl Hal for InputMockHal {
    fn mmio_read8(&self, _: u64) -> u8 {
        0
    }
    fn mmio_write8(&self, _: u64, _: u8) {}
    fn mmio_read16(&self, _: u64) -> u16 {
        0
    }
    fn mmio_write16(&self, _: u64, _: u16) {}
    fn mmio_read32(&self, _: u64) -> u32 {
        0
    }
    fn mmio_write32(&self, _: u64, _: u32) {}
    fn mmio_read64(&self, _: u64) -> u64 {
        0
    }
    fn mmio_write64(&self, _: u64, _: u64) {}
    fn io_read8(&self, port: u16) -> u8 {
        match port {
            0x64 => self.ps2.lock().unwrap().front().map(|x| x.0).unwrap_or(0),
            0x60 => self
                .ps2
                .lock()
                .unwrap()
                .pop_front()
                .map(|x| x.1)
                .unwrap_or(0),
            0x3F8 => {
                if (*self.mcr.lock().unwrap() & 0x10) != 0 {
                    *self.loopback.lock().unwrap()
                } else {
                    self.uart_rx.lock().unwrap().pop_front().unwrap_or(0)
                }
            }
            0x3FD => {
                if (*self.mcr.lock().unwrap() & 0x10) != 0
                    || !self.uart_rx.lock().unwrap().is_empty()
                {
                    0x61
                } else {
                    0x60
                }
            }
            _ => 0,
        }
    }
    fn io_write8(&self, port: u16, val: u8) {
        if port == 0x3FC {
            *self.mcr.lock().unwrap() = val;
        } else if port == 0x3F8 && (*self.mcr.lock().unwrap() & 0x10) != 0 {
            *self.loopback.lock().unwrap() = val;
        }
    }
    fn io_read16(&self, _: u16) -> u16 {
        0
    }
    fn io_write16(&self, _: u16, _: u16) {}
    fn io_read32(&self, _: u16) -> u32 {
        0
    }
    fn io_write32(&self, _: u16, _: u32) {}
    fn alloc_dma(&self, _: usize) -> Option<DmaBuffer> {
        None
    }
    fn free_dma(&self, _: DmaBuffer) {}
    fn micros(&self) -> i64 {
        let mut t = self.time_us.lock().unwrap();
        *t += 1000;
        *t
    }
    fn yield_now(&self) -> impl Future<Output = ()> {
        core::future::ready(())
    }
    fn sleep(&self, _: Duration) -> impl Future<Output = ()> {
        core::future::ready(())
    }
}

#[test]
fn test_ps2_lr_shift_e1_pause_and_serial_csi_crlf() {
    // Press LShift (0x2A), RShift (0x36), Release LShift (0xAA), Press 'a' (0x1E) -> still 'A'
    // Plus Pause/Break sequence 0xE1 0x1D 0x45 followed by Del (0xE0 0x53) -> NO bogus Ctrl+Del!
    let ps2_events = vec![
        (0x01, 0x2A),
        (0x01, 0x36),
        (0x01, 0xAA),
        (0x01, 0x1E), // should be 'A' because RShift still held
        (0x01, 0xB6), // release RShift
        (0x01, 0xE1),
        (0x01, 0x1D),
        (0x01, 0x45), // Pause 2 bytes after E1
        (0x01, 0x1D), // Ctrl? Wait: release 0xE1 0x9D 0xC5
        (0x01, 0xE1),
        (0x01, 0x9D),
        (0x01, 0xC5),
        (0x01, 0xE0),
        (0x01, 0x53), // Del
    ];
    let uart_bytes = b"\x1b[1;5A\r\nx".to_vec();
    let hal = InputMockHal::new(ps2_events, uart_bytes);

    let mut ps2 = Ps2Keyboard::new(hal);
    let mut keys = Vec::new();
    while let Some(k) = ps2.poll_key() {
        keys.push(k);
    }
    assert_eq!(keys, vec![KeyEvent::Char('A'), KeyEvent::Delete]);

    let mut serial = SerialPort::attach(
        0x3F8,
        true,
        InputMockHal::new(vec![], b"\x1b[1;5A\r\nx".to_vec()),
    );
    let mut skeys = Vec::new();
    while let Some(k) = serial.poll_key() {
        skeys.push(k);
    }
    assert_eq!(
        skeys,
        vec![KeyEvent::Up, KeyEvent::Enter, KeyEvent::Char('x')]
    );
}

// ============================================================================
// 4. ACPI Checksum, RESET_REG_SUP=0, and Truncated _S5_ Bounds
// ============================================================================

struct AcpiMemHal(Vec<u8>, u64);
impl Hal for AcpiMemHal {
    fn mmio_read8(&self, addr: u64) -> u8 {
        self.0[(addr - self.1) as usize]
    }
    fn mmio_write8(&self, _: u64, _: u8) {}
    fn mmio_read16(&self, _: u64) -> u16 {
        0
    }
    fn mmio_write16(&self, _: u64, _: u16) {}
    fn mmio_read32(&self, _: u64) -> u32 {
        0
    }
    fn mmio_write32(&self, _: u64, _: u32) {}
    fn mmio_read64(&self, _: u64) -> u64 {
        0
    }
    fn mmio_write64(&self, _: u64, _: u64) {}
    fn io_read8(&self, _: u16) -> u8 {
        0
    }
    fn io_write8(&self, _: u16, _: u8) {}
    fn io_read16(&self, _: u16) -> u16 {
        0
    }
    fn io_write16(&self, _: u16, _: u16) {}
    fn io_read32(&self, _: u16) -> u32 {
        0
    }
    fn io_write32(&self, _: u16, _: u32) {}
    fn alloc_dma(&self, _: usize) -> Option<DmaBuffer> {
        None
    }
    fn free_dma(&self, _: DmaBuffer) {}
    fn micros(&self) -> i64 {
        0
    }
    fn yield_now(&self) -> impl Future<Output = ()> {
        core::future::ready(())
    }
    fn sleep(&self, _: Duration) -> impl Future<Output = ()> {
        core::future::ready(())
    }
}

fn fix_checksum(slice: &mut [u8]) {
    slice[9] = 0;
    let sum: u8 = slice.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    slice[9] = 0u8.wrapping_sub(sum);
}

#[test]
fn test_acpi_reset_reg_sup_required_and_truncated_aml_rejected() {
    let base = 0x9000_0000u64;
    let mut mem = vec![0u8; 4096];

    // XSDT at 0x200 pointing to FADT at 0x400
    let xsdt_off = 0x200usize;
    mem[xsdt_off..xsdt_off + 4].copy_from_slice(b"XSDT");
    mem[xsdt_off + 4..xsdt_off + 8].copy_from_slice(&44u32.to_le_bytes());
    mem[xsdt_off + 36..xsdt_off + 44].copy_from_slice(&(base + 0x400).to_le_bytes());
    fix_checksum(&mut mem[xsdt_off..xsdt_off + 44]);

    // 1. Valid DSDT _S5_ + FADT with RESET_REG_SUP=0: parses cleanly and ignores ResetReg
    let dsdt_off = 0x600usize;
    let mut aml = vec![0u8; 47]; // 36 header + 11 bytes NameOp _S5_
    aml[0..4].copy_from_slice(b"DSDT");
    let aml_len = aml.len() as u32;
    aml[4..8].copy_from_slice(&aml_len.to_le_bytes());
    aml[36..47].copy_from_slice(b"\x08_S5_\x12\x05\x02\x0A\x05\x00");
    fix_checksum(&mut aml);
    mem[dsdt_off..dsdt_off + aml.len()].copy_from_slice(&aml);

    let fadt_off = 0x400usize;
    mem[fadt_off..fadt_off + 4].copy_from_slice(b"FACP");
    mem[fadt_off + 4..fadt_off + 8].copy_from_slice(&132u32.to_le_bytes());
    mem[fadt_off + 40..fadt_off + 44]
        .copy_from_slice(&(base as u32 + dsdt_off as u32).to_le_bytes());
    mem[fadt_off + 64..fadt_off + 68].copy_from_slice(&0x604u32.to_le_bytes());
    mem[fadt_off + 112..fadt_off + 116].copy_from_slice(&0u32.to_le_bytes()); // RESET_REG_SUP=0
    mem[fadt_off + 116] = 1; // System I/O
    mem[fadt_off + 120..fadt_off + 128].copy_from_slice(&0xCF9u64.to_le_bytes());
    mem[fadt_off + 128] = 0x06;
    fix_checksum(&mut mem[fadt_off..fadt_off + 132]);

    // RSDP at 0
    mem[0..8].copy_from_slice(b"RSD PTR ");
    mem[15] = 2;
    mem[20..24].copy_from_slice(&36u32.to_le_bytes());
    mem[24..32].copy_from_slice(&(base + xsdt_off as u64).to_le_bytes());
    let s1: u8 = mem[0..20].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    mem[8] = 0u8.wrapping_sub(s1);
    let s2: u8 = mem[0..36].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    mem[32] = 0u8.wrapping_sub(s2);

    let info = AcpiInfo::parse_from_rsdp(base, &AcpiMemHal(mem.clone(), base))
        .expect("Valid FADT + DSDT parses");
    assert!(
        info.reset_io_port.is_none(),
        "Must ignore ResetReg when RESET_REG_SUP=0"
    );
    assert_eq!(info.slp_typa, 5);

    // 2. Truncated DSDT _S5_ PkgLength (claims 0x07 bytes when only 5 remain) -> rejected cleanly
    aml[36..47].copy_from_slice(b"\x08_S5_\x12\x07\x02\x0A\x07\x00");
    fix_checksum(&mut aml);
    mem[dsdt_off..dsdt_off + aml.len()].copy_from_slice(&aml);
    assert!(
        AcpiInfo::parse_from_rsdp(base, &AcpiMemHal(mem, base)).is_none(),
        "Truncated _S5_ PkgLength must be rejected cleanly"
    );
}

// ============================================================================
// 5. AHCI & NVMe Stuck Hardware Drop Leaks DMA Safely (No UAF) & Rtl8125 Regs
// ============================================================================

struct StuckAhciHal {
    dma: Mutex<DmaPool>,
}
impl Hal for StuckAhciHal {
    fn mmio_read8(&self, _: u64) -> u8 {
        0
    }
    fn mmio_write8(&self, _: u64, _: u8) {}
    fn mmio_read16(&self, _: u64) -> u16 {
        0
    }
    fn mmio_write16(&self, _: u64, _: u16) {}
    fn mmio_read32(&self, addr: u64) -> u32 {
        match addr - 0xFEBF_0000 {
            0x000 => 1 << 31,                              // S64A=1
            0x00C => 1,                                    // PI port 0
            0x118 => (1 << 15) | (1 << 14) | (1 << 4) | 1, // CR & FR permanently stuck 1!
            0x120 => 0,                                    // TFD = 0 (no ERR/BSY/DRQ)
            0x128 => 0x113,                                // DET=3, IPM=1
            _ => 0,
        }
    }
    fn mmio_write32(&self, _: u64, _: u32) {}
    fn mmio_read64(&self, _: u64) -> u64 {
        0
    }
    fn mmio_write64(&self, _: u64, _: u64) {}
    fn io_read8(&self, _: u16) -> u8 {
        0
    }
    fn io_write8(&self, _: u16, _: u8) {}
    fn io_read16(&self, _: u16) -> u16 {
        0
    }
    fn io_write16(&self, _: u16, _: u16) {}
    fn io_read32(&self, _: u16) -> u32 {
        0
    }
    fn io_write32(&self, _: u16, _: u32) {}
    fn alloc_dma(&self, pages: usize) -> Option<DmaBuffer> {
        self.dma.lock().unwrap().alloc(pages)
    }
    fn free_dma(&self, buf: DmaBuffer) {
        self.dma.lock().unwrap().free(buf);
    }
    fn micros(&self) -> i64 {
        0
    }
    fn yield_now(&self) -> impl Future<Output = ()> {
        core::future::ready(())
    }
    fn sleep(&self, _: Duration) -> impl Future<Output = ()> {
        core::future::ready(())
    }
}

#[test]
fn test_ahci_stuck_port_returns_err_or_leaks_without_uaf() {
    let hal = StuckAhciHal {
        dma: Mutex::new(DmaPool::new(0x8000_0000, 128)),
    };
    let pci = PciDevice {
        bus: 0,
        dev: 1,
        func: 0,
        vendor_id: 0x8086,
        device_id: 0x2922,
        class_code: 0x01,
        subclass: 0x06,
        prog_if: 0x01,
    };
    // Because PxCMD.CR refuses to clear in stop_port(), open() must fail cleanly and NOT double-free/UAF!
    let res = block_on(AhciDisk::open(pci, 0xFEBF_0000, 0, &hal));
    assert!(res.is_err());
    // Ensure allocated DMA pages are leaked rather than freed while CR=1
    assert_eq!(hal.dma.lock().unwrap().freed_pages(), 0);
}

#[test]
fn test_rtl_nvme_compile_presence_checks() {
    let _ = core::mem::size_of::<RtlDevice<StuckAhciHal>>();
    let _ = core::mem::size_of::<NvmeDisk<StuckAhciHal>>();
}
