mod common;

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{DmaPool, block_on};
use pixie_drivers::acpi::AcpiInfo;
use pixie_drivers::hal::{DmaBuffer, Hal};
use pixie_drivers::input::KeyEvent;
use pixie_drivers::input::ps2::Ps2Keyboard;
use pixie_drivers::input::serial::{COM1_PORT, SerialPort};
use pixie_drivers::input::xhci::{XhciKeyboard, decode_hid_boot_report};
use pixie_drivers::pci::{Bar, PciDevice, scan_pci};

// ============================================================================
// 1. USB HID Boot Protocol Report Decoder Tests
// ============================================================================

#[test]
fn test_xhci_decode_hid_boot_report_modifiers_rollover_and_special_keys() {
    let empty = [0u8; 8];

    // Simple lowercase 'a' (0x04)
    let r1 = [0x00, 0x00, 0x04, 0, 0, 0, 0, 0];
    assert_eq!(
        decode_hid_boot_report(&empty, &r1),
        vec![KeyEvent::Char('a')]
    );

    // Holding 'a' in repeated report -> no duplicate event
    assert_eq!(decode_hid_boot_report(&r1, &r1), vec![]);

    // Rollover: holding 'a' (0x04) while pressing 'b' (0x05) -> only 'b' emitted
    let r2 = [0x00, 0x00, 0x04, 0x05, 0, 0, 0, 0];
    assert_eq!(decode_hid_boot_report(&r1, &r2), vec![KeyEvent::Char('b')]);

    // Left Shift (0x02) + 'a' (0x04) and '1' (0x1E) -> 'A' and '!'
    let r_shift = [0x02, 0x00, 0x04, 0x1E, 0, 0, 0, 0];
    assert_eq!(
        decode_hid_boot_report(&empty, &r_shift),
        vec![KeyEvent::Char('A'), KeyEvent::Char('!')]
    );

    // Right Shift (0x20) + '-' (0x2D) and '/' (0x38) -> '_' and '?'
    let r_rshift = [0x20, 0x00, 0x2D, 0x38, 0, 0, 0, 0];
    assert_eq!(
        decode_hid_boot_report(&empty, &r_rshift),
        vec![KeyEvent::Char('_'), KeyEvent::Char('?')]
    );

    // Special keys: Enter (0x28), Delete (0x4C), Arrow keys (0x4F..=0x52)
    let r_nav = [0x00, 0x00, 0x28, 0x4C, 0x4F, 0x50, 0x51, 0x52];
    assert_eq!(
        decode_hid_boot_report(&empty, &r_nav),
        vec![
            KeyEvent::Enter,
            KeyEvent::Delete,
            KeyEvent::Right,
            KeyEvent::Left,
            KeyEvent::Down,
            KeyEvent::Up,
        ]
    );

    // Backspace (0x2A -> '\x08') and Space (0x2C -> ' ')
    let r_edit = [0x00, 0x00, 0x2A, 0x2C, 0, 0, 0, 0];
    assert_eq!(
        decode_hid_boot_report(&empty, &r_edit),
        vec![KeyEvent::Char('\x08'), KeyEvent::Char(' ')]
    );

    // ErrorRollOver (0x01) must be ignored
    let r_rollover_err = [0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01];
    assert_eq!(decode_hid_boot_report(&empty, &r_rollover_err), vec![]);

    // Ctrl (0x01) + Alt (0x04) + Delete (0x4C) -> CtrlAltDel
    let r_cad = [0x01 | 0x04, 0x00, 0x4C, 0, 0, 0, 0, 0];
    assert_eq!(
        decode_hid_boot_report(&empty, &r_cad),
        vec![KeyEvent::CtrlAltDel]
    );

    // Right Ctrl (0x10) + Right Alt (0x40) + Keypad Delete (0x63) -> CtrlAltDel
    let r_cad_kp = [0x10 | 0x40, 0x00, 0x63, 0, 0, 0, 0, 0];
    assert_eq!(
        decode_hid_boot_report(&empty, &r_cad_kp),
        vec![KeyEvent::CtrlAltDel]
    );
}

// ============================================================================
// 2. xHCI Controller BIOS Handoff, Enumeration & Interrupt-IN Polling Test
// ============================================================================

const XHCI_BASE: u64 = 0xFEBF_0000;
const XHCI_DMA_BASE: u64 = 0x5000_0000;
const XHCI_DMA_PAGES: usize = 512;

#[derive(Clone, Copy)]
enum PortDevice {
    Disconnected,
    HidMouse,
    HidKeyboard { ep_addr: u8 },
}

struct XhciSimState {
    dma: DmaPool,

    // Capability & operational state
    hcsparams2: u32,
    usblegsup: u32,
    bios_handoff_completed: bool,
    usbcmd: u32,
    usbsts: u32,
    crcr: u64,
    dcbaap: u64,
    config: u32,
    ports: [PortDevice; 4],
    portsc: [u32; 4],

    // Interrupter 0
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    evt_enqueue_idx: usize,
    evt_cycle: u32,

    // Command & slot tracking
    cmd_dequeue_idx: usize,
    cmd_cycle: u32,
    next_slot_id: u8,
    disabled_slots: Vec<u8>,
    slot_port: [usize; 16],

    ep0_ring_phys: u64,
    ep0_deq_idx: usize,
    ep0_cycle: u32,

    int_ep_ring_phys: u64,
    int_ep_deq_idx: usize,
    int_ep_cycle: u32,
    configured_slot: u8,
    configured_int_dci: u8,
    time_us: i64,
}

impl XhciSimState {
    fn new_with_ports(ports: [PortDevice; 4], scratchpad_count: u32) -> Self {
        let mut portsc = [0u32; 4];
        for (i, dev) in ports.iter().enumerate() {
            if !matches!(dev, PortDevice::Disconnected) {
                portsc[i] = (1 << 0) | (1 << 1) | (1 << 9) | (3 << 10);
            }
        }
        Self {
            dma: DmaPool::new(XHCI_DMA_BASE, XHCI_DMA_PAGES),
            hcsparams2: (scratchpad_count & 0x1F) << 27,
            usblegsup: 0x0001_0001,
            bios_handoff_completed: false,
            usbcmd: 0,
            usbsts: 1, // HCH (Halted) = 1
            crcr: 0,
            dcbaap: 0,
            config: 0,
            ports,
            portsc,
            erstsz: 0,
            erstba: 0,
            erdp: 0,
            evt_enqueue_idx: 0,
            evt_cycle: 1,
            cmd_dequeue_idx: 0,
            cmd_cycle: 1,
            next_slot_id: 0,
            disabled_slots: Vec::new(),
            slot_port: [0; 16],
            ep0_ring_phys: 0,
            ep0_deq_idx: 0,
            ep0_cycle: 1,
            int_ep_ring_phys: 0,
            int_ep_deq_idx: 0,
            int_ep_cycle: 1,
            configured_slot: 1,
            configured_int_dci: 3,
            time_us: 1_000,
        }
    }

    fn new() -> Self {
        Self::new_with_ports(
            [
                PortDevice::HidKeyboard { ep_addr: 0x81 },
                PortDevice::Disconnected,
                PortDevice::Disconnected,
                PortDevice::Disconnected,
            ],
            0,
        )
    }

    fn event_ring_base_and_size(&self) -> (u64, usize) {
        let erst_phys = self.erstba & !0x3F;
        let seg = self.dma.slice(erst_phys, 16);
        let ring_base = u64::from_le_bytes(seg[0..8].try_into().unwrap());
        let ring_size = u32::from_le_bytes(seg[8..12].try_into().unwrap()) as usize;
        (ring_base, ring_size.max(16))
    }

    fn post_event_trb(&mut self, param: u64, status: u32, control_without_cycle: u32) {
        let (ring_base, ring_size) = self.event_ring_base_and_size();
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

            let mut slot_id = ((ctrl >> 24) & 0xFF) as u8;
            match trb_type {
                9 => {
                    // Enable Slot
                    self.next_slot_id += 1;
                    slot_id = self.next_slot_id;
                }
                10 => {
                    // Disable Slot
                    self.disabled_slots.push(slot_id);
                }
                11 | 12 if param >= XHCI_DMA_BASE => {
                    assert_eq!(
                        param & 0xFFF,
                        0,
                        "Input Context {:#x} must be 4 KiB page-aligned",
                        param
                    );
                    // Read Root Hub Port Number from Slot Context (offset 32B, dword 1 bits 23:16)
                    let slot_dw1 =
                        u32::from_le_bytes(self.dma.slice(param + 36, 4).try_into().unwrap());
                    let port_num = ((slot_dw1 >> 16) & 0xFF) as usize;
                    if (slot_id as usize) < self.slot_port.len() && port_num > 0 {
                        self.slot_port[slot_id as usize] = port_num;
                    }
                    for ctx_idx in 2..32 {
                        let ep_ctx_phys = param + (ctx_idx * 32) as u64;
                        if (ep_ctx_phys - XHCI_DMA_BASE) as usize + 16 <= self.dma.len() {
                            let s = self.dma.slice(ep_ctx_phys, 16);
                            let dw1 = u32::from_le_bytes(s[4..8].try_into().unwrap());
                            let ep_type = (dw1 >> 3) & 0x7;
                            let tr_raw = u64::from_le_bytes(s[8..16].try_into().unwrap());
                            let tr_deq = tr_raw & !0xF;
                            let dcs = (tr_raw & 1) as u32;
                            if tr_deq >= XHCI_DMA_BASE {
                                if ep_type == 4 {
                                    self.ep0_ring_phys = tr_deq;
                                    self.ep0_deq_idx = 0;
                                    self.ep0_cycle = dcs;
                                } else if ep_type == 7 {
                                    self.int_ep_ring_phys = tr_deq;
                                    self.int_ep_deq_idx = 0;
                                    self.int_ep_cycle = dcs;
                                    self.configured_slot = slot_id;
                                    self.configured_int_dci = (ctx_idx - 1) as u8;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }

            let comp_status = 1u32 << 24;
            let comp_ctrl = ((slot_id as u32) << 24) | (33u32 << 10);
            self.post_event_trb(trb_phys, comp_status, comp_ctrl);
        }
    }

    fn process_slot_doorbell(&mut self, slot_id: u8, dci: u32) {
        if dci == 1 {
            self.complete_control_transfers(slot_id);
        }
    }

    fn complete_control_transfers(&mut self, slot_id: u8) {
        let ring = self.ep0_ring_phys;
        let mut idx = self.ep0_deq_idx;
        let mut cyc = self.ep0_cycle;
        if let Some((_, setup_trb)) = self.next_ring_trb(ring, &mut idx, &mut cyc) {
            let setup: [u8; 8] = setup_trb[0..8].try_into().unwrap();
            let (_, data_trb) = self
                .next_ring_trb(ring, &mut idx, &mut cyc)
                .expect("Missing EP0 Data Stage TRB");
            let data_buf_phys = u64::from_le_bytes(data_trb[0..8].try_into().unwrap());
            let port_num = self
                .slot_port
                .get(slot_id as usize)
                .copied()
                .unwrap_or(1)
                .max(1);
            let port_dev = self.ports[port_num - 1];
            self.fill_usb_descriptor(port_dev, setup, data_buf_phys);
            let (st_phys, _) = self
                .next_ring_trb(ring, &mut idx, &mut cyc)
                .expect("Missing EP0 Status Stage TRB");
            self.ep0_deq_idx = idx;
            self.ep0_cycle = cyc;
            let comp_status = 1u32 << 24;
            let comp_ctrl = ((slot_id as u32) << 24) | (1u32 << 16) | (32u32 << 10);
            self.post_event_trb(st_phys, comp_status, comp_ctrl);
        }
    }

    fn fill_usb_descriptor(&mut self, port_dev: PortDevice, setup: [u8; 8], data_buf_phys: u64) {
        let b_request = setup[1];
        let w_value = u16::from_le_bytes([setup[2], setup[3]]);
        let w_len = u16::from_le_bytes([setup[6], setup[7]]) as usize;
        let desc_type = (w_value >> 8) as u8;
        if b_request == 0x06 {
            match desc_type {
                1 => {
                    let dev_desc: [u8; 18] = [
                        18, 1, 0x00, 0x02, 0x00, 0x00, 0x00, 8, 0x34, 0x12, 0x01, 0x00, 0x00, 0x01,
                        1, 2, 0, 1,
                    ];
                    let n = w_len.min(18);
                    self.dma
                        .slice_mut(data_buf_phys, n)
                        .copy_from_slice(&dev_desc[..n]);
                }
                2 => {
                    let (proto, ep_addr) = match port_dev {
                        PortDevice::HidMouse => (0x02u8, 0x81u8),
                        PortDevice::HidKeyboard { ep_addr } => (0x01u8, ep_addr),
                        PortDevice::Disconnected => (0x00u8, 0x81u8),
                    };
                    let cfg_desc: [u8; 34] = [
                        // Config (9B)
                        9, 2, 34, 0, 1, 1, 0, 0x80, 50,
                        // Interface (9B): Class=3 (HID), SubClass=1 (Boot), Protocol=proto
                        9, 4, 0, 0, 1, 0x03, 0x01, proto, 0, // HID (9B)
                        9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0,
                        // Endpoint (7B): Interrupt IN, wMaxPacketSize=8, bInterval=10
                        7, 5, ep_addr, 0x03, 8, 0, 10,
                    ];
                    let n = w_len.min(34);
                    self.dma
                        .slice_mut(data_buf_phys, n)
                        .copy_from_slice(&cfg_desc[..n]);
                }
                _ => {}
            }
        }
    }

    fn inject_hid_report_with_cc(&mut self, report: [u8; 8], cc: u32) {
        let ring = self.int_ep_ring_phys;
        let mut idx = self.int_ep_deq_idx;
        let mut cyc = self.int_ep_cycle;
        let (trb_phys, trb) = self
            .next_ring_trb(ring, &mut idx, &mut cyc)
            .expect("Driver did not queue an 8-byte Interrupt-IN Normal TRB");
        self.int_ep_deq_idx = idx;
        self.int_ep_cycle = cyc;

        let buf_phys = u64::from_le_bytes(trb[0..8].try_into().unwrap());
        self.dma.slice_mut(buf_phys, 8).copy_from_slice(&report);

        let comp_status = cc << 24;
        let comp_ctrl = ((self.configured_slot as u32) << 24)
            | ((self.configured_int_dci as u32) << 16)
            | (32u32 << 10);
        self.post_event_trb(trb_phys, comp_status, comp_ctrl);
    }

    fn inject_hid_report(&mut self, report: [u8; 8]) {
        self.inject_hid_report_with_cc(report, 1);
    }

    fn mmio_read32(&self, addr: u64) -> u32 {
        let off = addr - XHCI_BASE;
        match off {
            0x000 => 0x0100_0020,              // CAPLENGTH = 0x20, HCIVERSION = 0x0100
            0x004 => (4 << 24) | (1 << 8) | 8, // HCSPARAMS1: 4 ports, 1 intr, 8 slots
            0x008 => self.hcsparams2,          // HCSPARAMS2: scratchpad buffers
            0x010 => 0x0080_0000, // HCCPARAMS1: xECP = 0x80 dwords (0x200 bytes), CSZ = 0 (32B)
            0x014 => 0x0000_0300, // DBOFF = 0x300
            0x018 => 0x0000_0600, // RTSOFF = 0x600
            // Operational registers (base 0x020)
            0x020 => self.usbcmd,
            0x024 => self.usbsts,
            0x028 => 0x0000_0001, // PAGESIZE = 4 KiB
            0x038 => (self.crcr & 0xFFFF_FFFF) as u32,
            0x03C => (self.crcr >> 32) as u32,
            0x050 => (self.dcbaap & 0xFFFF_FFFF) as u32,
            0x054 => (self.dcbaap >> 32) as u32,
            0x058 => self.config,
            0x200 => self.usblegsup,
            0x420 => self.portsc[0],
            0x430 => self.portsc[1],
            0x440 => self.portsc[2],
            0x450 => self.portsc[3],
            // Runtime Interrupter 0 (RTSOFF = 0x600 -> IR0 at 0x620)
            0x628 => self.erstsz,
            0x630 => (self.erstba & 0xFFFF_FFFF) as u32,
            0x634 => (self.erstba >> 32) as u32,
            0x638 => (self.erdp & 0xFFFF_FFFF) as u32,
            0x63C => (self.erdp >> 32) as u32,
            _ => 0,
        }
    }

    fn mmio_write32(&mut self, addr: u64, val: u32) {
        let off = addr - XHCI_BASE;
        match off {
            0x020 => {
                // USBCMD
                if (val & (1 << 1)) != 0 {
                    // HCRST: auto-clear HCRST, set HCH = 1, CNR = 0
                    self.usbcmd = val & !(1 << 1);
                    self.usbsts = 1;
                } else {
                    self.usbcmd = val;
                    if (val & 1) != 0 {
                        self.usbsts &= !1; // RS = 1 -> clear HCH
                    } else {
                        self.usbsts |= 1; // RS = 0 -> set HCH
                    }
                }
            }
            0x024 => self.usbsts &= !val,
            0x038 => self.crcr = (self.crcr & 0xFFFF_FFFF_0000_0000) | (val as u64),
            0x03C => self.crcr = (self.crcr & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32),
            0x050 => self.dcbaap = (self.dcbaap & 0xFFFF_FFFF_0000_0000) | (val as u64),
            0x054 => self.dcbaap = (self.dcbaap & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32),
            0x058 => self.config = val,
            0x200 => {
                // USBLEGSUP BIOS-to-OS handoff: setting bit 24 (OS Owned) clears bit 16 (BIOS Owned)
                self.usblegsup = val;
                if (val & (1 << 24)) != 0 {
                    self.usblegsup &= !(1 << 16);
                    self.bios_handoff_completed = true;
                }
            }
            0x420 | 0x430 | 0x440 | 0x450 => {
                let idx = ((off - 0x420) / 0x10) as usize;
                if (val & (1 << 4)) != 0 && !matches!(self.ports[idx], PortDevice::Disconnected) {
                    self.portsc[idx] = (1 << 0) | (1 << 1) | (1 << 9) | (3 << 10) | (1 << 21);
                }
            }
            // Doorbells (DBOFF = 0x300)
            0x300 => self.process_command_ring(),
            db if (0x304..0x340).contains(&db) && (db & 3) == 0 => {
                let slot_id = ((db - 0x300) / 4) as u8;
                self.process_slot_doorbell(slot_id, val & 0xFF);
            }
            // Runtime Interrupter 0 (RTSOFF = 0x600 -> IR0 at 0x620)
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
struct MockXhciHal {
    state: Arc<Mutex<XhciSimState>>,
}

impl MockXhciHal {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(XhciSimState::new())),
        }
    }

    fn with_ports(ports: [PortDevice; 4], scratchpad_count: u32) -> Self {
        Self {
            state: Arc::new(Mutex::new(XhciSimState::new_with_ports(
                ports,
                scratchpad_count,
            ))),
        }
    }
}

impl Hal for MockXhciHal {
    fn mmio_read8(&self, addr: u64) -> u8 {
        let aligned = addr & !3;
        let shift = ((addr & 3) * 8) as u32;
        ((self.mmio_read32(aligned) >> shift) & 0xFF) as u8
    }

    fn mmio_write8(&self, addr: u64, val: u8) {
        let aligned = addr & !3;
        let shift = ((addr & 3) * 8) as u32;
        let cur = self.mmio_read32(aligned);
        let next = (cur & !(0xFF << shift)) | ((val as u32) << shift);
        self.mmio_write32(aligned, next);
    }

    fn mmio_read16(&self, addr: u64) -> u16 {
        let aligned = addr & !3;
        let shift = ((addr & 2) * 8) as u32;
        ((self.mmio_read32(aligned) >> shift) & 0xFFFF) as u16
    }

    fn mmio_write16(&self, addr: u64, val: u16) {
        let aligned = addr & !3;
        let shift = ((addr & 2) * 8) as u32;
        let cur = self.mmio_read32(aligned);
        let next = (cur & !(0xFFFF << shift)) | ((val as u32) << shift);
        self.mmio_write32(aligned, next);
    }

    fn mmio_read32(&self, addr: u64) -> u32 {
        self.state.lock().unwrap().mmio_read32(addr)
    }

    fn mmio_write32(&self, addr: u64, val: u32) {
        self.state.lock().unwrap().mmio_write32(addr, val);
    }

    fn mmio_read64(&self, addr: u64) -> u64 {
        let lo = self.mmio_read32(addr) as u64;
        let hi = self.mmio_read32(addr + 4) as u64;
        lo | (hi << 32)
    }

    fn mmio_write64(&self, addr: u64, val: u64) {
        self.mmio_write32(addr, (val & 0xFFFF_FFFF) as u32);
        self.mmio_write32(addr + 4, (val >> 32) as u32);
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

#[test]
fn test_xhci_keyboard_bios_handoff_and_interrupt_polling() {
    let hal = MockXhciHal::new();
    let mut kbd = block_on(XhciKeyboard::new(XHCI_BASE, hal.clone()))
        .expect("XhciKeyboard::new should succeed");

    {
        let st = hal.state.lock().unwrap();
        assert!(
            st.bios_handoff_completed,
            "XhciKeyboard::new must perform USBLEGSUP BIOS-to-OS handoff (setting OS Owned bit 24)"
        );
    }

    assert_eq!(kbd.poll_key(), None);

    // Inject HID Boot report for 'x' (usage 0x1B)
    hal.state
        .lock()
        .unwrap()
        .inject_hid_report([0x00, 0x00, 0x1B, 0, 0, 0, 0, 0]);
    assert_eq!(kbd.poll_key(), Some(KeyEvent::Char('x')));
    assert_eq!(kbd.poll_key(), None);
}

#[test]
fn test_xhci_keyboard_skips_mouse_on_port1_scratchpads_and_custom_dci() {
    // Port 1 = HID Mouse (Protocol 2), Port 2 = HID Boot Keyboard on EP2 IN (0x82 -> DCI 5),
    // plus HCSPARAMS2 requesting 2 Scratchpad Buffers!
    let hal = MockXhciHal::with_ports(
        [
            PortDevice::HidMouse,
            PortDevice::HidKeyboard { ep_addr: 0x82 },
            PortDevice::Disconnected,
            PortDevice::Disconnected,
        ],
        2,
    );

    let mut kbd = block_on(XhciKeyboard::new(XHCI_BASE, hal.clone()))
        .expect("XhciKeyboard::new should skip mouse on Port 1 and bind keyboard on Port 2");

    let alloc_pages = {
        let st = hal.state.lock().unwrap();
        assert_eq!(
            st.disabled_slots,
            vec![1],
            "Mouse slot on Port 1 must be disabled when rejected"
        );
        assert_eq!(st.configured_slot, 2);
        assert_eq!(st.configured_int_dci, 5, "EP2 IN (0x82) must map to DCI 5");
        let dcbaa0 = u64::from_le_bytes(st.dma.slice(st.dcbaap, 8).try_into().unwrap());
        assert!(
            dcbaa0 >= XHCI_DMA_BASE,
            "DCBAA[0] must point to the Scratchpad Buffer Array when HCSPARAMS2 > 0"
        );
        // 4 control pages + 1 scratchpad array page + 2 scratchpad pages = 7 pages
        assert_eq!(st.dma.allocated_pages(), 7);
        st.dma.allocated_pages()
    };

    // Errored Transfer Event (CC = 4) must be ignored without delivering stale report bytes
    hal.state
        .lock()
        .unwrap()
        .inject_hid_report_with_cc([0x00, 0x00, 0x1D, 0, 0, 0, 0, 0], 4);
    assert_eq!(kbd.poll_key(), None);

    // Valid Transfer Event (CC = 1) delivers 'z' (usage 0x1D)
    hal.state
        .lock()
        .unwrap()
        .inject_hid_report([0x00, 0x00, 0x1D, 0, 0, 0, 0, 0]);
    assert_eq!(kbd.poll_key(), Some(KeyEvent::Char('z')));

    drop(kbd);
    let st = hal.state.lock().unwrap();
    assert_eq!(
        st.dma.freed_pages(),
        alloc_pages,
        "Drop must free all control and scratchpad DMA pages"
    );
}

// ============================================================================
// 3. PS/2 Keyboard Scancode Set 1 + Serial Console ANSI Escape Tests
// ============================================================================

struct IoSimState {
    // PS/2 queue: pairs of (status_0x64, data_0x60)
    ps2_queue: VecDeque<(u8, u8)>,

    // 16550 UART state
    uart_ier: u8,
    uart_lcr: u8,
    uart_mcr: u8,
    uart_loopback_byte: u8,
    uart_rx_queue: VecDeque<u8>,
    uart_tx_bytes: Vec<u8>,

    // ACPI & PCI I/O tracking
    io_writes_8: Vec<(u16, u8)>,
    io_writes_16: Vec<(u16, u16)>,
    acpi_mem: Vec<u8>,
    acpi_base: u64,

    // PCI config space: (bus, dev, func, reg_aligned) -> u32
    pci_addr_latch: u32,
    pci_config: std::collections::HashMap<(u8, u8, u8, u8), u32>,
}

impl IoSimState {
    fn new() -> Self {
        Self {
            ps2_queue: VecDeque::new(),
            uart_ier: 0,
            uart_lcr: 0,
            uart_mcr: 0,
            uart_loopback_byte: 0,
            uart_rx_queue: VecDeque::new(),
            uart_tx_bytes: Vec::new(),
            io_writes_8: Vec::new(),
            io_writes_16: Vec::new(),
            acpi_mem: vec![0u8; 64 * 1024],
            acpi_base: 0x000E_0000,
            pci_addr_latch: 0,
            pci_config: std::collections::HashMap::new(),
        }
    }
}

#[derive(Clone)]
struct MockIoHal {
    state: Arc<Mutex<IoSimState>>,
}

impl MockIoHal {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(IoSimState::new())),
        }
    }
}

impl Hal for MockIoHal {
    fn mmio_read8(&self, addr: u64) -> u8 {
        let st = self.state.lock().unwrap();
        let off = (addr - st.acpi_base) as usize;
        st.acpi_mem[off]
    }
    fn mmio_write8(&self, addr: u64, val: u8) {
        let mut st = self.state.lock().unwrap();
        let off = (addr - st.acpi_base) as usize;
        st.acpi_mem[off] = val;
    }
    fn mmio_read16(&self, addr: u64) -> u16 {
        let lo = self.mmio_read8(addr) as u16;
        let hi = self.mmio_read8(addr + 1) as u16;
        lo | (hi << 8)
    }
    fn mmio_write16(&self, _addr: u64, _val: u16) {}
    fn mmio_read32(&self, addr: u64) -> u32 {
        let b0 = self.mmio_read8(addr) as u32;
        let b1 = self.mmio_read8(addr + 1) as u32;
        let b2 = self.mmio_read8(addr + 2) as u32;
        let b3 = self.mmio_read8(addr + 3) as u32;
        b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
    }
    fn mmio_write32(&self, _addr: u64, _val: u32) {}
    fn mmio_read64(&self, addr: u64) -> u64 {
        (self.mmio_read32(addr) as u64) | ((self.mmio_read32(addr + 4) as u64) << 32)
    }
    fn mmio_write64(&self, _addr: u64, _val: u64) {}

    fn io_read8(&self, port: u16) -> u8 {
        let mut st = self.state.lock().unwrap();
        match port {
            0x64 => st
                .ps2_queue
                .front()
                .map(|&(status, _)| status)
                .unwrap_or(0x00),
            0x60 => st
                .ps2_queue
                .pop_front()
                .map(|(_, data)| data)
                .unwrap_or(0x00),
            p if p == COM1_PORT => {
                if (st.uart_mcr & 0x10) != 0 {
                    st.uart_loopback_byte
                } else {
                    st.uart_rx_queue.pop_front().unwrap_or(0)
                }
            }
            p if p == COM1_PORT + 5 => {
                // LSR: bit 5 (THRE = 0x20) | bit 6 (TEMT = 0x40), plus bit 0 (DR = 0x01) if RX available
                if !st.uart_rx_queue.is_empty() || (st.uart_mcr & 0x10) != 0 {
                    0x61
                } else {
                    0x60
                }
            }
            _ => 0,
        }
    }

    fn io_write8(&self, port: u16, val: u8) {
        let mut st = self.state.lock().unwrap();
        st.io_writes_8.push((port, val));
        match port {
            p if p == COM1_PORT => {
                if (st.uart_lcr & 0x80) == 0 {
                    if (st.uart_mcr & 0x10) != 0 {
                        st.uart_loopback_byte = val;
                    } else {
                        st.uart_tx_bytes.push(val);
                    }
                }
            }
            p if p == COM1_PORT + 1 => st.uart_ier = val,
            p if p == COM1_PORT + 3 => st.uart_lcr = val,
            p if p == COM1_PORT + 4 => st.uart_mcr = val,
            _ => {}
        }
    }

    fn io_read16(&self, _port: u16) -> u16 {
        0
    }

    fn io_write16(&self, port: u16, val: u16) {
        self.state.lock().unwrap().io_writes_16.push((port, val));
    }

    fn io_read32(&self, port: u16) -> u32 {
        let st = self.state.lock().unwrap();
        if port == 0xCFC && (st.pci_addr_latch & 0x8000_0000) != 0 {
            let bus = ((st.pci_addr_latch >> 16) & 0xFF) as u8;
            let dev = ((st.pci_addr_latch >> 11) & 0x1F) as u8;
            let func = ((st.pci_addr_latch >> 8) & 0x07) as u8;
            let offset = (st.pci_addr_latch & 0xFC) as u8;
            return st
                .pci_config
                .get(&(bus, dev, func, offset))
                .copied()
                .unwrap_or(0xFFFF_FFFF);
        }
        0xFFFF_FFFF
    }

    fn io_write32(&self, port: u16, val: u32) {
        let mut st = self.state.lock().unwrap();
        if port == 0xCF8 {
            st.pci_addr_latch = val;
        } else if port == 0xCFC && (st.pci_addr_latch & 0x8000_0000) != 0 {
            let bus = ((st.pci_addr_latch >> 16) & 0xFF) as u8;
            let dev = ((st.pci_addr_latch >> 11) & 0x1F) as u8;
            let func = ((st.pci_addr_latch >> 8) & 0x07) as u8;
            let offset = (st.pci_addr_latch & 0xFC) as u8;
            st.pci_config.insert((bus, dev, func, offset), val);
        }
    }

    fn alloc_dma(&self, _pages: usize) -> Option<DmaBuffer> {
        None
    }
    fn free_dma(&self, _buf: DmaBuffer) {}

    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        let mut st = self.state.lock().unwrap();
        let off = (phys - st.acpi_base) as usize;
        unsafe { st.acpi_mem.as_mut_ptr().add(off) }
    }

    fn micros(&self) -> i64 {
        0
    }
    fn yield_now(&self) -> impl Future<Output = ()> {
        core::future::ready(())
    }
    fn sleep(&self, _duration: Duration) -> impl Future<Output = ()> {
        core::future::ready(())
    }
}

#[test]
fn test_ps2_keyboard_scancodes_extended_arrows_shift_and_mouse_filter() {
    let hal = MockIoHal::new();
    {
        let mut st = hal.state.lock().unwrap();
        // 1. Auxiliary PS/2 mouse byte (status bit 5 = 0x20 set) -> must be discarded!
        st.ps2_queue.push_back((0x21, 0x1E));
        // 2. Extended arrow keys: Up (E0 48), Down (E0 50), Left (E0 4B), Right (E0 4D), Delete (E0 53)
        for &code in &[0xE0, 0x48, 0xE0, 0x50, 0xE0, 0x4B, 0xE0, 0x4D, 0xE0, 0x53] {
            st.ps2_queue.push_back((0x01, code));
        }
        // 3. Left Shift press (0x2A), 'a' (0x1E) -> 'A', Left Shift release (0xAA), 'a' (0x1E) -> 'a', Enter (0x1C)
        for &code in &[0x2A, 0x1E, 0xAA, 0x1E, 0x1C] {
            st.ps2_queue.push_back((0x01, code));
        }
        // 4. Ctrl (0x1D) + Alt (0x38) + Extended Delete (0xE0, 0x53) -> CtrlAltDel
        for &code in &[0x1D, 0x38, 0xE0, 0x53] {
            st.ps2_queue.push_back((0x01, code));
        }
    }

    let mut ps2 = Ps2Keyboard::new(hal);
    let mut events = Vec::new();
    for _ in 0..32 {
        if let Some(ev) = ps2.poll_key() {
            events.push(ev);
        }
    }

    assert_eq!(
        events,
        vec![
            KeyEvent::Up,
            KeyEvent::Down,
            KeyEvent::Left,
            KeyEvent::Right,
            KeyEvent::Delete,
            KeyEvent::Char('A'),
            KeyEvent::Char('a'),
            KeyEvent::Enter,
            KeyEvent::CtrlAltDel,
        ]
    );
}

#[test]
fn test_ps2_fake_shift_ignored_and_serial_standalone_escape() {
    let hal = MockIoHal::new();
    {
        let mut st = hal.state.lock().unwrap();
        // PS/2 Set-1 Fake Left Shift (E0 2A / E0 AA) and Fake Right Shift (E0 36 / E0 B6)
        // emitted by 8042 / USB legacy firmware around extended navigation keys:
        // even when 'a' (0x1E) arrives while Fake Shift is asserted (before E0 AA / E0 B6),
        // `shift` must NOT be set!
        for &code in &[
            0xE0, 0x2A, 0xE0, 0x48, 0x1E, 0xE0, 0xC8, 0xE0, 0xAA, // Fake LShift + Up + 'a'
            0xE0, 0x36, 0xE0, 0x53, 0x1E, 0xE0, 0xD3, 0xE0, 0xB6, // Fake RShift + Del + 'a'
            0x36, 0x1E, 0xB6, 0x1E, // Real Right Shift + 'a' -> 'A', release -> 'a'
        ] {
            st.ps2_queue.push_back((0x01, code));
        }
    }

    let mut ps2 = Ps2Keyboard::new(hal.clone());
    let mut ps2_events = Vec::new();
    for _ in 0..32 {
        if let Some(ev) = ps2.poll_key() {
            ps2_events.push(ev);
        }
    }
    assert_eq!(
        ps2_events,
        vec![
            KeyEvent::Up,
            KeyEvent::Char('a'),
            KeyEvent::Delete,
            KeyEvent::Char('a'),
            KeyEvent::Char('A'),
            KeyEvent::Char('a'),
        ]
    );

    // Serial standalone / consecutive ESC handling:
    // - `\x1bq` emits standalone `Char('\x1b')` AND preserves `Char('q')` without swallowing 'q'!
    // - `\x1b\x1b[A` emits standalone `Char('\x1b')` followed by CSI `Up`!
    // - `\x1bOB` decodes SS3 `Down`, and trailing `\x1b` emits standalone `Char('\x1b')`!
    let mut serial = SerialPort::new(COM1_PORT, hal.clone());
    {
        let mut st = hal.state.lock().unwrap();
        for &b in b"\x1bq\x1b\x1b[A\x1bOBx\x1b" {
            st.uart_rx_queue.push_back(b);
        }
    }
    let mut serial_events = Vec::new();
    while let Some(ev) = serial.poll_key() {
        serial_events.push(ev);
    }
    assert_eq!(
        serial_events,
        vec![
            KeyEvent::Char('\x1b'),
            KeyEvent::Char('q'),
            KeyEvent::Char('\x1b'),
            KeyEvent::Up,
            KeyEvent::Down,
            KeyEvent::Char('x'),
            KeyEvent::Char('\x1b'),
        ]
    );
}

#[test]
fn test_serial_port_loopback_ansi_escapes_and_ascii() {
    let hal = MockIoHal::new();
    let mut serial = SerialPort::new(COM1_PORT, hal.clone());
    assert!(
        serial.present,
        "SerialPort loopback probe should detect 16550 UART"
    );

    serial.write_str("HI");
    assert_eq!(hal.state.lock().unwrap().uart_tx_bytes, b"HI");

    // Feed ANSI sequences: Up (\x1b[A), Down (\x1b[B), Right (\x1b[C), Left (\x1b[D), Delete (\x1b[3~), Enter (\r), and 'q'
    {
        let mut st = hal.state.lock().unwrap();
        for &b in b"\x1b[A\x1b[B\x1b[C\x1b[D\x1b[3~\rq" {
            st.uart_rx_queue.push_back(b);
        }
    }

    let mut events = Vec::new();
    while let Some(ev) = serial.poll_key() {
        events.push(ev);
    }

    assert_eq!(
        events,
        vec![
            KeyEvent::Up,
            KeyEvent::Down,
            KeyEvent::Right,
            KeyEvent::Left,
            KeyEvent::Delete,
            KeyEvent::Enter,
            KeyEvent::Char('q'),
        ]
    );
}

// ============================================================================
// 4. ACPI Table Parsing (RSDP v2 -> XSDT -> FADT + DSDT _S5_) & Power Control
// ============================================================================

fn set_acpi_checksum(table: &mut [u8], checksum_idx: usize) {
    table[checksum_idx] = 0;
    let sum: u8 = table.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    table[checksum_idx] = 0u8.wrapping_sub(sum);
}

#[test]
fn test_acpi_parse_from_rsdp_shutdown_and_reset() {
    let hal = MockIoHal::new();
    let rsdp_phys = 0x000E_0000u64;
    let xsdt_phys = 0x000E_1000u64;
    let fadt_phys = 0x000E_2000u64;
    let dsdt_phys = 0x000E_3000u64;

    {
        let mut st = hal.state.lock().unwrap();
        let base = st.acpi_base;

        // 1. RSDP v2 at 0x000E_0000 (36 bytes)
        let rsdp_off = (rsdp_phys - base) as usize;
        let rsdp = &mut st.acpi_mem[rsdp_off..rsdp_off + 36];
        rsdp[0..8].copy_from_slice(b"RSD PTR ");
        rsdp[9..15].copy_from_slice(b"PIXIE ");
        rsdp[15] = 2; // Revision 2 (XSDT)
        rsdp[20..24].copy_from_slice(&36u32.to_le_bytes());
        rsdp[24..32].copy_from_slice(&xsdt_phys.to_le_bytes());
        set_acpi_checksum(&mut rsdp[0..20], 8);
        set_acpi_checksum(rsdp, 32);

        // 2. XSDT at 0x000E_1000 (36-byte header + 8-byte entry = 44 bytes)
        let xsdt_off = (xsdt_phys - base) as usize;
        let xsdt = &mut st.acpi_mem[xsdt_off..xsdt_off + 44];
        xsdt[0..4].copy_from_slice(b"XSDT");
        xsdt[4..8].copy_from_slice(&44u32.to_le_bytes());
        xsdt[8] = 1;
        xsdt[36..44].copy_from_slice(&fadt_phys.to_le_bytes());
        set_acpi_checksum(xsdt, 9);

        // 3. FADT (FACP) at 0x000E_2000 (244 bytes)
        let fadt_off = (fadt_phys - base) as usize;
        let fadt = &mut st.acpi_mem[fadt_off..fadt_off + 244];
        fadt[0..4].copy_from_slice(b"FACP");
        fadt[4..8].copy_from_slice(&244u32.to_le_bytes());
        fadt[8] = 3; // Revision 3
        fadt[40..44].copy_from_slice(&(dsdt_phys as u32).to_le_bytes()); // DSDT (32-bit)
        fadt[64..68].copy_from_slice(&0x0604u32.to_le_bytes()); // PM1a_CNT_BLK
        fadt[68..72].copy_from_slice(&0x0608u32.to_le_bytes()); // PM1b_CNT_BLK
        fadt[112..116].copy_from_slice(&(1u32 << 10).to_le_bytes()); // Flags: RESET_REG_SUP
        // RESET_REG GAS at 116..128: AddressSpaceId = 1 (System I/O), BitWidth = 8, BitOffset = 0, AccessSize = 1, Address = 0x0CF9
        fadt[116] = 1;
        fadt[117] = 8;
        fadt[118] = 0;
        fadt[119] = 1;
        fadt[120..128].copy_from_slice(&0x0CF9u64.to_le_bytes());
        fadt[128] = 0x06; // RESET_VALUE = 0x06
        fadt[140..148].copy_from_slice(&dsdt_phys.to_le_bytes()); // X_DSDT (64-bit)
        set_acpi_checksum(fadt, 9);

        // 4. DSDT at 0x000E_3000 with decoy string "_S5_" (without NameOp 0x08) and
        // non-Package _S5_ reference before the real NameOp(0x08) "_S5_" PackageOp(0x12)
        let aml: &[u8] = &[
            0x0D, b'_', b'S', b'5', b'_',
            0x00, // StringPrefix "_S5_" decoy (not NameOp 0x08!)
            0x08, b'_', b'S', b'5', b'_',
            0xFF, // Malformed non-PackageOp -> must `continue`, not abort!
            0x08, b'_', b'S', b'5', b'_', // Real NameOp "_S5_"
            0x12, 0x08, 0x04, // PackageOp, PkgLength=8, NumElements=4
            0x0A, 0x05, // BytePrefix 5 (SLP_TYPa)
            0x0A, 0x07, // BytePrefix 7 (SLP_TYPb)
            0x00, 0x00,
        ];
        let dsdt_len = 36 + aml.len();
        let dsdt_off = (dsdt_phys - base) as usize;
        let dsdt = &mut st.acpi_mem[dsdt_off..dsdt_off + dsdt_len];
        dsdt[0..4].copy_from_slice(b"DSDT");
        dsdt[4..8].copy_from_slice(&(dsdt_len as u32).to_le_bytes());
        dsdt[8] = 2;
        dsdt[36..36 + aml.len()].copy_from_slice(aml);
        set_acpi_checksum(dsdt, 9);
    }

    let info = AcpiInfo::parse_from_rsdp(rsdp_phys, &hal)
        .expect("AcpiInfo::parse_from_rsdp should parse synthetic RSDP -> XSDT -> FADT + DSDT");

    assert_eq!(info.pm1a_cnt_blk, 0x0604);
    assert_eq!(info.pm1b_cnt_blk, Some(0x0608));
    assert_eq!(info.slp_typa, 5);
    assert_eq!(info.slp_typb, 7);
    assert_eq!(info.reset_io_port, Some(0x0CF9));
    assert_eq!(info.reset_value, 0x06);

    // Verify shutdown writes (SLP_TYP << 10) | (1 << 13) to PM1a_CNT_BLK and PM1b_CNT_BLK
    info.shutdown(&hal);
    {
        let st = hal.state.lock().unwrap();
        let expected_pm1a = (5u16 << 10) | (1u16 << 13);
        let expected_pm1b = (7u16 << 10) | (1u16 << 13);
        assert!(
            st.io_writes_16.contains(&(0x0604, expected_pm1a)),
            "Expected PM1a_CNT_BLK write ({:#x}, {:#x}), got {:?}",
            0x0604,
            expected_pm1a,
            st.io_writes_16
        );
        assert!(
            st.io_writes_16.contains(&(0x0608, expected_pm1b)),
            "Expected PM1b_CNT_BLK write ({:#x}, {:#x}), got {:?}",
            0x0608,
            expected_pm1b,
            st.io_writes_16
        );
    }

    // Verify reset writes RESET_VALUE (0x06) to RESET_REG port (0x0CF9)
    assert!(info.reset(&hal));
    {
        let st = hal.state.lock().unwrap();
        assert!(
            st.io_writes_8.contains(&(0x0CF9, 0x06)),
            "Expected ACPI reset write (0x0CF9, 0x06), got {:?}",
            st.io_writes_8
        );
    }

    // Corrupt RSDP 20-byte checksum -> parse_from_rsdp must reject it!
    hal.state.lock().unwrap().acpi_mem[8] ^= 0xFF;
    assert!(AcpiInfo::parse_from_rsdp(rsdp_phys, &hal).is_none());
}

// ============================================================================
// 5. PCI Bus Enumeration, 64-bit BAR Decoding, and Bus Mastering Test
// ============================================================================

#[test]
fn test_pci_scan_64bit_bar_and_enable_bus_mastering() {
    let hal = MockIoHal::new();
    {
        let mut st = hal.state.lock().unwrap();
        // Device 00:02.0 (multi-function header_type = 0x80 -> Type 0x00) and 00:02.1
        st.pci_config.insert((0, 2, 0, 0x00), 0x100E_8086); // vendor=0x8086, device=0x100E
        st.pci_config.insert((0, 2, 0, 0x04), 0x0010_0000); // status/command = 0
        st.pci_config.insert((0, 2, 0, 0x08), 0x0200_0000); // class=0x02, subclass=0x00
        st.pci_config.insert((0, 2, 0, 0x0C), 0x0080_0000); // header_type = 0x80 (multi-function)
        // 64-bit MMIO BAR0+BAR1 at 0x0000_0001_FEB0_0000 (bits 2:1 = 0b10 for 64-bit memory BAR)
        // Note: BAR1 high dword is 0x0000_0001 (bit 0 == 1!), which would misdecode as Bar::Io if not skipped!
        st.pci_config.insert((0, 2, 0, 0x10), 0xFEB0_0004);
        st.pci_config.insert((0, 2, 0, 0x14), 0x0000_0001);
        // I/O BAR2 at port 0xD000 (bit 0 = 1)
        st.pci_config.insert((0, 2, 0, 0x18), 0x0000_D001);

        // Second function 00:02.1 (NVMe controller, header_type defaults to 0xFFFF_FFFF if absent -> set 0x0C = 0)
        st.pci_config.insert((0, 2, 1, 0x00), 0x0010_1B36);
        st.pci_config.insert((0, 2, 1, 0x08), 0x0108_0200);
        st.pci_config.insert((0, 2, 1, 0x0C), 0x0000_0000);

        // PCI-to-PCI Bridge 00:03.0 (header_type = 0x01: only BAR0 and BAR1 exist; 0x18 is bus numbers!)
        st.pci_config.insert((0, 3, 0, 0x00), 0x244E_8086);
        st.pci_config.insert((0, 3, 0, 0x08), 0x0604_0000);
        st.pci_config.insert((0, 3, 0, 0x0C), 0x0001_0000); // header_type = 0x01
        st.pci_config.insert((0, 3, 0, 0x10), 0xFE00_0000); // 32-bit MMIO BAR0
        st.pci_config.insert((0, 3, 0, 0x18), 0x0002_0100); // Primary/Secondary/Subordinate Bus != BAR2!
    }

    let devices = scan_pci(&hal);
    assert_eq!(devices.len(), 3);
    assert_eq!(
        devices[0],
        PciDevice {
            bus: 0,
            dev: 2,
            func: 0,
            vendor_id: 0x8086,
            device_id: 0x100E,
            class_code: 0x02,
            subclass: 0x00,
            prog_if: 0x00,
        }
    );
    assert_eq!(devices[1].func, 1);
    assert_eq!(devices[1].vendor_id, 0x1B36);
    assert_eq!(devices[2].dev, 3);

    // 64-bit BAR0 decoding
    assert_eq!(
        devices[0].read_bar(&hal, 0),
        Some(Bar::Memory {
            base: 0x0000_0001_FEB0_0000,
            size: 0,
            is_64bit: true,
        })
    );
    // High 32 bits of 64-bit BAR0 at index 1 MUST be skipped and return None!
    assert_eq!(devices[0].read_bar(&hal, 1), None);
    // I/O BAR2 decoding
    assert_eq!(devices[0].read_bar(&hal, 2), Some(Bar::Io { port: 0xD000 }));
    // Out-of-bounds BAR6 on Type 0x00 header MUST return None
    assert_eq!(devices[0].read_bar(&hal, 6), None);

    // Type 0x01 PCI-to-PCI Bridge: BAR0 valid, BAR2 (bus number register 0x18) MUST return None!
    assert_eq!(
        devices[2].read_bar(&hal, 0),
        Some(Bar::Memory {
            base: 0xFE00_0000,
            size: 0,
            is_64bit: false,
        })
    );
    assert_eq!(devices[2].read_bar(&hal, 2), None);

    // Enable Bus Mastering
    devices[0].enable_bus_mastering(&hal);
    assert_eq!(devices[0].read_u16(&hal, 0x04) & 0x0007, 0x0007);
}
