use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, alloc_dma_buffers};
use crate::input::KeyEvent;
use crate::xhci::{
    CTRL_BUF_SIZE, CTRL_DMA_PAGES, EventRing, IN_CTX_MAX_BYTES, OFF_CMD_RING, OFF_EP0_RING,
    OFF_IN_CTX, OFF_XFER_IN_RING, RING_SIZE, TRB_CONFIGURE_ENDPOINT, TRB_NORMAL,
    TRB_TRANSFER_EVENT, TrbRing, XhciCaps, XhciIo, XhciScratchpad, bios_handoff_and_reset,
    control_in, disable_slot, enable_and_address_slot, halt_controller, is_transfer_cc_ok,
    reset_and_probe_port, setup_and_start_controller, submit_command,
};

const OFF_REPORTS: usize = 0x2A00;
const REPORT_BUF_COUNT: u8 = 64;

/// Decodes a pair of 8-byte USB HID Boot Protocol keyboard reports (`prev` and `curr`),
/// returning any newly pressed `KeyEvent`s (including `KeyEvent::CtrlAltDel` if Left/Right Ctrl +
/// Left/Right Alt + Delete (`0x4C`) or Keypad-Delete (`0x63`) are held simultaneously).
pub fn decode_hid_boot_report(prev: &[u8; 8], curr: &[u8; 8]) -> Vec<KeyEvent> {
    let mut events = Vec::new();

    // Check for Phantom / ErrorRollOver state
    if curr[2..8].contains(&0x01) {
        return events;
    }

    let modifiers = curr[0];
    let ctrl = (modifiers & 0x11) != 0; // Left Ctrl (0x01) or Right Ctrl (0x10)
    let shift = (modifiers & 0x22) != 0; // Left Shift (0x02) or Right Shift (0x20)
    let alt = (modifiers & 0x44) != 0; // Left Alt (0x04) or Right Alt (0x40)

    let prev_ctrl = (prev[0] & 0x11) != 0;
    let prev_alt = (prev[0] & 0x44) != 0;
    let curr_has_del = curr[2..8].iter().any(|&k| k == 0x4C || k == 0x63);
    let prev_has_del = prev[2..8].iter().any(|&k| k == 0x4C || k == 0x63);

    if ctrl && alt && curr_has_del && !(prev_ctrl && prev_alt && prev_has_del) {
        events.push(KeyEvent::CtrlAltDel);
        return events;
    }

    for &kc in &curr[2..8] {
        if kc <= 0x03 || prev[2..8].contains(&kc) {
            continue;
        }
        if let Some(ev) = decode_hid_usage(kc, shift) {
            events.push(ev);
        }
    }

    events
}

fn decode_hid_usage(usage: u8, shift: bool) -> Option<KeyEvent> {
    match usage {
        0x04..=0x1D => {
            let base = if shift { b'A' } else { b'a' };
            Some(KeyEvent::Char((base + (usage - 0x04)) as char))
        }
        0x1E..=0x27 => {
            const DIGITS: &[u8; 10] = b"1234567890";
            const SYMBOLS: &[u8; 10] = b"!@#$%^&*()";
            let idx = (usage - 0x1E) as usize;
            let b = if shift { SYMBOLS[idx] } else { DIGITS[idx] };
            Some(KeyEvent::Char(b as char))
        }
        0x28 | 0x58 => Some(KeyEvent::Enter),
        0x29 => Some(KeyEvent::Char('\x1b')),
        0x2A => Some(KeyEvent::Char('\x08')),
        0x2B => Some(KeyEvent::Char('\t')),
        0x2C => Some(KeyEvent::Char(' ')),
        0x2D => Some(KeyEvent::Char(if shift { '_' } else { '-' })),
        0x2E => Some(KeyEvent::Char(if shift { '+' } else { '=' })),
        0x2F => Some(KeyEvent::Char(if shift { '{' } else { '[' })),
        0x30 => Some(KeyEvent::Char(if shift { '}' } else { ']' })),
        0x31 | 0x32 => Some(KeyEvent::Char(if shift { '|' } else { '\\' })),
        0x33 => Some(KeyEvent::Char(if shift { ':' } else { ';' })),
        0x34 => Some(KeyEvent::Char(if shift { '"' } else { '\'' })),
        0x35 => Some(KeyEvent::Char(if shift { '~' } else { '`' })),
        0x36 => Some(KeyEvent::Char(if shift { '<' } else { ',' })),
        0x37 => Some(KeyEvent::Char(if shift { '>' } else { '.' })),
        0x38 => Some(KeyEvent::Char(if shift { '?' } else { '/' })),
        0x4C | 0x63 => Some(KeyEvent::Delete),
        0x4F => Some(KeyEvent::Right),
        0x50 => Some(KeyEvent::Left),
        0x51 => Some(KeyEvent::Down),
        0x52 => Some(KeyEvent::Up),
        _ => None,
    }
}

/// Parses a USB Configuration Descriptor to locate an Interrupt-IN endpoint belonging to a
/// HID Boot Keyboard (`class == 0x03`, `protocol == 0x01`) or fallback non-mouse HID interface
/// (`class == 0x03`, `protocol != 0x02`), returning `(dci, max_packet_size, interval)`.
fn parse_hid_keyboard_endpoint(cfg_desc: &[u8]) -> Option<(u8, u16, u8)> {
    if cfg_desc.len() < 9 || cfg_desc[1] != 2 {
        return None;
    }
    let total_len = (u16::from_le_bytes([cfg_desc[2], cfg_desc[3]]) as usize).min(cfg_desc.len());
    let mut off = 0usize;
    let mut cur_class = 0u8;
    let mut cur_proto = 0u8;
    let mut fallback_hid: Option<(u8, u16, u8)> = None;

    while off + 2 <= total_len {
        let b_len = cfg_desc[off] as usize;
        let b_type = cfg_desc[off + 1];
        if b_len < 2 || off + b_len > total_len {
            break;
        }
        if b_type == 4 && b_len >= 9 {
            cur_class = cfg_desc[off + 5];
            cur_proto = cfg_desc[off + 7];
        } else if b_type == 5 && b_len >= 7 && cur_class == 0x03 && cur_proto != 0x02 {
            let ep_addr = cfg_desc[off + 2];
            let attr = cfg_desc[off + 3] & 0x03;
            let ep_num = ep_addr & 0x0F;
            let is_in = (ep_addr & 0x80) != 0;
            if is_in && attr == 0x03 && ep_num != 0 {
                let dci = (ep_num * 2 + 1).clamp(3, 31);
                let raw_mps = u16::from_le_bytes([cfg_desc[off + 4], cfg_desc[off + 5]]) & 0x07FF;
                let mps = raw_mps.clamp(8, 64);
                let interval = cfg_desc[off + 6].clamp(1, 16);
                if cur_proto == 0x01 {
                    return Some((dci, mps, interval));
                }
                if fallback_hid.is_none() {
                    fallback_hid = Some((dci, mps, interval));
                }
            }
        }
        off += b_len;
    }

    fallback_hid
}

pub struct XhciKeyboard<H: Hal> {
    pub mmio_base: u64,
    pub hal: H,
    op_off: u32,
    rt_off: u32,
    db_off: u32,
    ctrl_dma: DmaBuffer,
    scratchpad: Option<Box<XhciScratchpad>>,
    cmd_ring: TrbRing,
    ep0_ring: TrbRing,
    int_ring: TrbRing,
    event_ring: EventRing,
    report_buf_idx: u8,
    active_slot: u8,
    int_dci: u8,
    prev_report: [u8; 8],
    pending_keys: VecDeque<KeyEvent>,
}

impl<H: Hal> XhciKeyboard<H> {
    /// Initializes the xHCI controller at `mmio_base`:
    /// - Performs BIOS-to-OS handoff via the `USBLEGSUP` extended capability (`id == 1`).
    /// - Resets (`USBCMD.HCRST`) and starts the controller with `DCBAA` (+ scratchpad buffers if requested),
    ///   Command Ring, and Event Ring.
    /// - Scans root hub ports, reads each device's Configuration Descriptor via `EP0` to verify a
    ///   HID Keyboard Interrupt-IN interface, configures that endpoint, and queues 8-byte Interrupt-IN TRBs.
    pub async fn new(mmio_base: u64, hal: H) -> Result<Self> {
        let caps = XhciCaps::read_and_map(mmio_base, &hal)?;
        bios_handoff_and_reset(mmio_base, &caps, &hal)?;

        let [ctrl_dma] =
            alloc_dma_buffers(&hal, [CTRL_DMA_PAGES], "xHCI: failed to allocate DMA pages")?;
        unsafe {
            ptr::write_bytes(ctrl_dma.virt, 0, ctrl_dma.size);
        }

        let mut cmd_ring = TrbRing::new(OFF_CMD_RING);
        let mut ep0_ring = TrbRing::new(OFF_EP0_RING);
        let mut int_ring = TrbRing::new(OFF_XFER_IN_RING);
        cmd_ring.init(&ctrl_dma);
        ep0_ring.init(&ctrl_dma);
        int_ring.init(&ctrl_dma);

        let scratchpad = setup_and_start_controller(mmio_base, &caps, &ctrl_dma, &hal)?;

        let mut kbd = Self {
            mmio_base,
            hal,
            op_off: caps.op_off,
            rt_off: caps.rt_off,
            db_off: caps.db_off,
            ctrl_dma,
            scratchpad,
            cmd_ring,
            ep0_ring,
            int_ring,
            event_ring: EventRing::new(),
            report_buf_idx: 0,
            active_slot: 0,
            int_dci: 3,
            prev_report: [0u8; 8],
            pending_keys: VecDeque::new(),
        };

        let op_base = mmio_base + caps.op_off as u64;
        for port_idx in 0..(caps.max_ports as usize) {
            let Some(speed) = reset_and_probe_port(op_base, port_idx, &kbd.hal) else {
                continue;
            };
            if kbd
                .init_port_device(port_idx + 1, speed, caps.ctx_size as usize)
                .is_ok()
            {
                break;
            }
        }

        Ok(kbd)
    }

    #[inline]
    fn op_base(&self) -> u64 {
        self.mmio_base + self.op_off as u64
    }

    #[inline]
    fn rt_base(&self) -> u64 {
        self.mmio_base + self.rt_off as u64
    }

    #[inline]
    fn db_base(&self) -> u64 {
        self.mmio_base + self.db_off as u64
    }

    fn init_port_device(&mut self, port_num: usize, speed: u8, ctx_size: usize) -> Result<()> {
        let io = XhciIo {
            ctrl_dma: &self.ctrl_dma,
            rt_base: self.rt_base(),
            db_base: self.db_base(),
            hal: &self.hal,
        };

        let slot_id = enable_and_address_slot(
            &io,
            &mut self.cmd_ring,
            &mut self.ep0_ring,
            &mut self.event_ring,
            port_num,
            speed,
            ctx_size,
        )?;

        // Read Configuration Descriptor via EP0 GET_DESCRIPTOR to verify HID Keyboard interface
        let mut cfg_desc = [0u8; CTRL_BUF_SIZE];
        let len_le = (CTRL_BUF_SIZE as u16).to_le_bytes();
        let got_cfg = control_in(
            &io,
            &mut self.ep0_ring,
            &mut self.event_ring,
            slot_id,
            [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, len_le[0], len_le[1]],
            &mut cfg_desc,
        );
        if !got_cfg {
            disable_slot(&io, &mut self.cmd_ring, &mut self.event_ring, slot_id);
            return Err(Error::msg(
                "xHCI failed to read USB Configuration Descriptor",
            ));
        }

        let Some((int_dci, int_mps, _interval)) = parse_hid_keyboard_endpoint(&cfg_desc) else {
            disable_slot(&io, &mut self.cmd_ring, &mut self.event_ring, slot_id);
            return Err(Error::msg(
                "USB device does not expose a HID Keyboard endpoint",
            ));
        };

        self.active_slot = slot_id;
        self.int_dci = int_dci;
        self.int_ring.init(&self.ctrl_dma);

        // Configure Interrupt-IN endpoint (`int_dci`)
        let in_ctx_virt = unsafe { self.ctrl_dma.virt.add(OFF_IN_CTX) };
        let in_ctx_phys = self.ctrl_dma.phys + OFF_IN_CTX as u64;
        unsafe {
            ptr::write_bytes(in_ctx_virt, 0, IN_CTX_MAX_BYTES);
            ptr::write_volatile(
                in_ctx_virt.add(4) as *mut u32,
                (1u32 << 0) | (1u32 << int_dci),
            );

            let slot_ctx = in_ctx_virt.add(ctx_size) as *mut u32;
            ptr::write_volatile(slot_ctx, ((int_dci as u32) << 27) | ((speed as u32) << 20));
            ptr::write_volatile(slot_ctx.add(1), (port_num as u32) << 16);

            let ep_in_ctx = in_ctx_virt.add((int_dci as usize + 1) * ctx_size) as *mut u32;
            let int_ring_phys = self.ctrl_dma.phys + OFF_XFER_IN_RING as u64;
            ptr::write_volatile(ep_in_ctx, 6 << 16);
            ptr::write_volatile(
                ep_in_ctx.add(1),
                (7 << 3) | (3 << 1) | ((int_mps as u32) << 16),
            );
            ptr::write_volatile(ep_in_ctx.add(2), (int_ring_phys as u32) | 1);
            ptr::write_volatile(ep_in_ctx.add(3), (int_ring_phys >> 32) as u32);
            ptr::write_volatile(ep_in_ctx.add(4), (8 << 16) | 8);
        }
        fence(Ordering::SeqCst);

        let io = XhciIo {
            ctrl_dma: &self.ctrl_dma,
            rt_base: self.rt_base(),
            db_base: self.db_base(),
            hal: &self.hal,
        };
        match submit_command(
            &io,
            &mut self.cmd_ring,
            &mut self.event_ring,
            in_ctx_phys,
            (TRB_CONFIGURE_ENDPOINT << 10) | ((slot_id as u32) << 24),
        ) {
            Some((1, _)) => {}
            _ => {
                self.active_slot = 0;
                disable_slot(&io, &mut self.cmd_ring, &mut self.event_ring, slot_id);
                return Err(Error::msg("xHCI Configure Endpoint failed"));
            }
        }

        // Queue initial Interrupt-IN Normal TRBs for 8-byte HID Boot reports
        for _ in 0..8 {
            self.queue_interrupt_in_trb();
        }
        self.ring_ep_doorbell();

        Ok(())
    }

    fn queue_interrupt_in_trb(&mut self) {
        let buf_idx = self.report_buf_idx as usize;
        self.report_buf_idx = (self.report_buf_idx + 1) % REPORT_BUF_COUNT;
        let report_phys = self.ctrl_dma.phys + (OFF_REPORTS + buf_idx * 8) as u64;
        self.int_ring.enqueue(
            &self.ctrl_dma,
            report_phys,
            8,
            (TRB_NORMAL << 10) | (1 << 5) | (1 << 2), // IOC = 1, ISP = 1
        );
    }

    fn ring_ep_doorbell(&self) {
        if self.active_slot == 0 {
            return;
        }
        let slot = self.active_slot as u64;
        self.hal
            .mmio_write32(self.db_base() + slot * 4, self.int_dci as u32);
    }

    /// Polls the xHCI Event Ring for completed Interrupt-IN transfer events and returns the next `KeyEvent` if any.
    pub fn poll_key(&mut self) -> Option<KeyEvent> {
        if let Some(ev) = self.pending_keys.pop_front() {
            return Some(ev);
        }
        if self.active_slot == 0 {
            return None;
        }

        let rt_base = self.rt_base();
        let int_ring_phys = self.ctrl_dma.phys + OFF_XFER_IN_RING as u64;
        let reports_phys = self.ctrl_dma.phys + OFF_REPORTS as u64;

        for _ in 0..RING_SIZE {
            let (trb_type, param, status, control) =
                self.event_ring.pop(&self.ctrl_dma, rt_base, &self.hal)?;
            if trb_type != TRB_TRANSFER_EVENT {
                continue;
            }

            let cc = ((status >> 24) & 0xFF) as u8;
            if !is_transfer_cc_ok(cc) {
                self.queue_interrupt_in_trb();
                self.ring_ep_doorbell();
                continue;
            }

            let report: [u8; 8] = if (control & (1 << 2)) != 0 {
                param.to_le_bytes()
            } else if param >= int_ring_phys && param < int_ring_phys + (RING_SIZE * 16) as u64 {
                let trb_idx = ((param - int_ring_phys) / 16) as usize;
                let trb_virt = unsafe { self.ctrl_dma.virt.add(OFF_XFER_IN_RING + trb_idx * 16) };
                let buf_phys = unsafe { ptr::read_volatile(trb_virt as *const u64) };
                let buf_virt = if buf_phys >= self.ctrl_dma.phys
                    && buf_phys + 8 <= self.ctrl_dma.phys + self.ctrl_dma.size as u64
                {
                    unsafe {
                        self.ctrl_dma
                            .virt
                            .add((buf_phys - self.ctrl_dma.phys) as usize)
                    }
                } else {
                    self.hal.phys_to_virt(buf_phys)
                };
                let mut r = [0u8; 8];
                unsafe {
                    ptr::copy_nonoverlapping(buf_virt as *const u8, r.as_mut_ptr(), 8);
                }
                r
            } else if param >= reports_phys
                && param + 8 <= reports_phys + (REPORT_BUF_COUNT as usize * 8) as u64
            {
                let buf_virt = unsafe {
                    self.ctrl_dma
                        .virt
                        .add(OFF_REPORTS + (param - reports_phys) as usize)
                };
                let mut r = [0u8; 8];
                unsafe {
                    ptr::copy_nonoverlapping(buf_virt as *const u8, r.as_mut_ptr(), 8);
                }
                r
            } else {
                let mut r = [0u8; 8];
                unsafe {
                    ptr::copy_nonoverlapping(
                        self.ctrl_dma.virt.add(OFF_REPORTS) as *const u8,
                        r.as_mut_ptr(),
                        8,
                    );
                }
                r
            };

            let decoded = decode_hid_boot_report(&self.prev_report, &report);
            self.prev_report = report;

            self.queue_interrupt_in_trb();
            self.ring_ep_doorbell();

            for ev in decoded {
                self.pending_keys.push_back(ev);
            }
            if let Some(ev) = self.pending_keys.pop_front() {
                return Some(ev);
            }
        }

        None
    }
}

impl<H: Hal> Drop for XhciKeyboard<H> {
    fn drop(&mut self) {
        halt_controller(self.op_base(), &self.hal);
        if let Some(sp) = self.scratchpad.take() {
            sp.free(&self.hal);
        }
        self.hal.free_dma(self.ctrl_dma);
    }
}
