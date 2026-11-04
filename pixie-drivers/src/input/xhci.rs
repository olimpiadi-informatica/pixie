use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal};
use crate::input::KeyEvent;

const RING_SIZE: usize = 64;
const TRB_LINK: u32 = 6;
const TRB_NORMAL: u32 = 1;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_CMD_COMPLETION: u32 = 33;

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
        if kc <= 0x03 {
            continue;
        }
        if prev[2..8].contains(&kc) {
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
        0x1E => Some(KeyEvent::Char(if shift { '!' } else { '1' })),
        0x1F => Some(KeyEvent::Char(if shift { '@' } else { '2' })),
        0x20 => Some(KeyEvent::Char(if shift { '#' } else { '3' })),
        0x21 => Some(KeyEvent::Char(if shift { '$' } else { '4' })),
        0x22 => Some(KeyEvent::Char(if shift { '%' } else { '5' })),
        0x23 => Some(KeyEvent::Char(if shift { '^' } else { '6' })),
        0x24 => Some(KeyEvent::Char(if shift { '&' } else { '7' })),
        0x25 => Some(KeyEvent::Char(if shift { '*' } else { '8' })),
        0x26 => Some(KeyEvent::Char(if shift { '(' } else { '9' })),
        0x27 => Some(KeyEvent::Char(if shift { ')' } else { '0' })),
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

pub struct XhciKeyboard<H: Hal> {
    pub mmio_base: u64,
    pub hal: H,
    op_base: u64,
    rt_base: u64,
    db_base: u64,
    dma: Option<DmaBuffer>,
    cmd_enqueue: usize,
    cmd_cycle: bool,
    er_dequeue: usize,
    er_cycle: bool,
    int_enqueue: usize,
    int_cycle: bool,
    active_slot: u8,
    prev_report: [u8; 8],
    pending_keys: VecDeque<KeyEvent>,
}

const OFF_DCBAA: usize = 0x0000;
const OFF_ERST: usize = 0x0800;
const OFF_REPORTS: usize = 0x0900;
const OFF_CMD_RING: usize = 0x1000;
const OFF_EVENT_RING: usize = 0x1400;
const OFF_EP0_RING: usize = 0x1800;
const OFF_INT_RING: usize = 0x1C00;
const OFF_OUT_CTX: usize = 0x2000;
const OFF_IN_CTX: usize = 0x2800;

impl<H: Hal> XhciKeyboard<H> {
    /// Initializes the xHCI controller at `mmio_base`:
    /// - Performs BIOS-to-OS handoff via the `USBLEGSUP` extended capability (`id == 1`).
    /// - Resets (`USBCMD.HCRST`) and starts the controller with `DCBAA`, Command Ring, and Event Ring.
    /// - Scans root hub ports, addresses connected devices, configures HID Boot Keyboard endpoints,
    ///   and queues Interrupt-IN TRBs for 8-byte HID Boot reports.
    pub async fn new(mmio_base: u64, hal: H) -> Result<Self> {
        hal.map_mmio(mmio_base, 0x10000);
        let cap_hciver = hal.mmio_read32(mmio_base);
        if cap_hciver == 0xFFFF_FFFF || cap_hciver == 0 {
            return Err(Error::msg("xHCI controller not present"));
        }
        let cap_length = (cap_hciver & 0xFF) as u64;
        let hcsparams1 = hal.mmio_read32(mmio_base + 0x04);
        let hccparams1 = hal.mmio_read32(mmio_base + 0x10);
        let dboff = (hal.mmio_read32(mmio_base + 0x14) & !0x3) as u64;
        let rtsoff = (hal.mmio_read32(mmio_base + 0x18) & !0x1F) as u64;

        let max_slots = (hcsparams1 & 0xFF).clamp(1, 32);
        let max_ports = ((hcsparams1 >> 24) & 0xFF) as usize;
        let ctx_size = if (hccparams1 & (1 << 2)) != 0 { 64 } else { 32 };

        let op_base = mmio_base + cap_length;
        let rt_base = mmio_base + rtsoff;
        let db_base = mmio_base + dboff;

        // 1. BIOS-to-OS Handoff via Extended Capabilities (USBLEGSUP, id == 1)
        let mut ext_off = ((hccparams1 >> 16) as u64) * 4;
        for _ in 0..64 {
            if ext_off == 0 {
                break;
            }
            let cap = hal.mmio_read32(mmio_base + ext_off);
            if cap == 0 || cap == 0xFFFF_FFFF {
                break;
            }
            let cap_id = cap & 0xFF;
            if cap_id == 1 {
                // Set HC OS Owned Semaphore (bit 24)
                hal.mmio_write32(mmio_base + ext_off, cap | (1 << 24));
                for _ in 0..200 {
                    let v = hal.mmio_read32(mmio_base + ext_off);
                    if (v & (1 << 16)) == 0 {
                        break;
                    }
                    hal.yield_now().await;
                }
                // Clear SMI enable bits in USBLEGCTLSTS
                hal.mmio_write32(mmio_base + ext_off + 4, 0);
                break;
            }
            let next_dw = ((cap >> 8) & 0xFF) as u64;
            if next_dw == 0 {
                break;
            }
            ext_off += next_dw * 4;
        }

        // 2. Stop and Reset Controller (USBCMD.HCRST)
        let cmd = hal.mmio_read32(op_base);
        if (cmd & 1) != 0 {
            hal.mmio_write32(op_base, cmd & !1);
            for _ in 0..500 {
                if (hal.mmio_read32(op_base + 0x04) & 1) != 0 {
                    break;
                }
                hal.yield_now().await;
            }
        }
        hal.mmio_write32(op_base, 1 << 1); // HCRST

        let mut reset_done = false;
        for _ in 0..2000 {
            let cmd = hal.mmio_read32(op_base);
            let sts = hal.mmio_read32(op_base + 0x04);
            if (cmd & (1 << 1)) == 0 && (sts & (1 << 11)) == 0 {
                reset_done = true;
                break;
            }
            hal.yield_now().await;
        }
        if !reset_done {
            return Err(Error::msg("xHCI HCRST timeout"));
        }

        // 3. Allocate DMA structures
        let dma = hal
            .alloc_dma(3)
            .ok_or_else(|| Error::msg("xHCI: failed to allocate DMA pages"))?;
        unsafe {
            core::ptr::write_bytes(dma.virt, 0, dma.size);
        }

        // Initialize Link TRB at the end of Command Ring, EP0 Ring, and Interrupt Ring
        Self::write_trb_raw(
            unsafe { dma.virt.add(OFF_CMD_RING + (RING_SIZE - 1) * 16) },
            dma.phys + OFF_CMD_RING as u64,
            0,
            (TRB_LINK << 10) | (1 << 1) | 1,
        );
        Self::write_trb_raw(
            unsafe { dma.virt.add(OFF_EP0_RING + (RING_SIZE - 1) * 16) },
            dma.phys + OFF_EP0_RING as u64,
            0,
            (TRB_LINK << 10) | (1 << 1) | 1,
        );
        Self::write_trb_raw(
            unsafe { dma.virt.add(OFF_INT_RING + (RING_SIZE - 1) * 16) },
            dma.phys + OFF_INT_RING as u64,
            0,
            (TRB_LINK << 10) | (1 << 1) | 1,
        );

        // Initialize ERST entry 0 -> Event Ring
        unsafe {
            let erst_ptr = dma.virt.add(OFF_ERST) as *mut u32;
            let er_phys = dma.phys + OFF_EVENT_RING as u64;
            core::ptr::write_volatile(erst_ptr, er_phys as u32);
            core::ptr::write_volatile(erst_ptr.add(1), (er_phys >> 32) as u32);
            core::ptr::write_volatile(erst_ptr.add(2), RING_SIZE as u32);
            core::ptr::write_volatile(erst_ptr.add(3), 0);
        }

        // 4. Program Operational and Runtime registers
        let dcbaa_phys = dma.phys + OFF_DCBAA as u64;
        let cmd_ring_phys = dma.phys + OFF_CMD_RING as u64;
        let er_phys = dma.phys + OFF_EVENT_RING as u64;
        let erst_phys = dma.phys + OFF_ERST as u64;
        let ir0 = rt_base + 0x20;

        hal.mmio_write32(op_base + 0x38, max_slots);
        hal.mmio_write32(op_base + 0x30, dcbaa_phys as u32);
        hal.mmio_write32(op_base + 0x34, (dcbaa_phys >> 32) as u32);

        hal.mmio_write32(op_base + 0x18, (cmd_ring_phys as u32) | 1);
        hal.mmio_write32(op_base + 0x1C, (cmd_ring_phys >> 32) as u32);

        hal.mmio_write32(ir0 + 0x04, 0); // IMOD
        hal.mmio_write32(ir0 + 0x08, 1); // ERSTSZ = 1
        hal.mmio_write32(ir0 + 0x18, (er_phys as u32) | (1 << 3)); // ERDP
        hal.mmio_write32(ir0 + 0x1C, (er_phys >> 32) as u32);
        hal.mmio_write32(ir0 + 0x10, erst_phys as u32); // ERSTBA
        hal.mmio_write32(ir0 + 0x14, (erst_phys >> 32) as u32);
        hal.mmio_write32(ir0, 0x02); // IMAN.IE = 1

        // Start controller (USBCMD.RS | USBCMD.INTE)
        hal.mmio_write32(op_base, 1 | (1 << 2));

        for _ in 0..500 {
            let sts = hal.mmio_read32(op_base + 0x04);
            if (sts & 1) == 0 {
                break;
            }
            hal.yield_now().await;
        }

        let mut kbd = Self {
            mmio_base,
            hal,
            op_base,
            rt_base,
            db_base,
            dma: Some(dma),
            cmd_enqueue: 0,
            cmd_cycle: true,
            er_dequeue: 0,
            er_cycle: true,
            int_enqueue: 0,
            int_cycle: true,
            active_slot: 0,
            prev_report: [0u8; 8],
            pending_keys: VecDeque::new(),
        };

        // 5. Scan root hub ports and initialize connected keyboard device
        for port_idx in 0..max_ports {
            let portsc_addr = op_base + 0x400 + (port_idx as u64) * 0x10;
            let mut portsc = kbd.hal.mmio_read32(portsc_addr);
            if (portsc & 1) == 0 {
                continue;
            }
            if (portsc & (1 << 1)) == 0 {
                // Reset port to enable it
                kbd.hal
                    .mmio_write32(portsc_addr, (portsc & 0x0E01_C3E0) | (1 << 4));
                for _ in 0..200 {
                    let p = kbd.hal.mmio_read32(portsc_addr);
                    if (p & (1 << 4)) == 0 {
                        break;
                    }
                    kbd.hal.yield_now().await;
                }
                portsc = kbd.hal.mmio_read32(portsc_addr);
            }

            let speed = (((portsc >> 10) & 0xF) as u8).max(1);
            if kbd
                .init_port_device(port_idx + 1, speed, ctx_size)
                .await
                .is_ok()
            {
                break;
            }
        }

        Ok(kbd)
    }

    async fn init_port_device(
        &mut self,
        port_num: usize,
        speed: u8,
        ctx_size: usize,
    ) -> Result<()> {
        let dma = self.dma.as_ref().unwrap();
        let dma_virt = dma.virt;
        let dma_phys = dma.phys;

        // 1. Enable Slot Command
        self.enqueue_cmd(0, 0, TRB_ENABLE_SLOT << 10);
        let slot_id = match self.wait_cmd_completion().await {
            Some((cc, slot)) if (cc == 1 || cc == 0) && slot != 0 => slot,
            _ => 1,
        };
        self.active_slot = slot_id;

        // Point DCBAA[slot_id] -> Output Context
        let out_ctx_phys = dma_phys + OFF_OUT_CTX as u64;
        unsafe {
            let dcbaa_entry = dma_virt.add(OFF_DCBAA + (slot_id as usize) * 8) as *mut u64;
            core::ptr::write_volatile(dcbaa_entry, out_ctx_phys);
        }

        // 2. Build Input Context for Address Device
        let in_ctx_virt = unsafe { dma_virt.add(OFF_IN_CTX) };
        let in_ctx_phys = dma_phys + OFF_IN_CTX as u64;
        unsafe {
            core::ptr::write_bytes(in_ctx_virt, 0, 0x800);
            // Input Control Context: Add Slot Context (bit 0) + EP0 Context (bit 1)
            core::ptr::write_volatile(in_ctx_virt.add(4) as *mut u32, (1 << 0) | (1 << 1));

            // Slot Context (index 1)
            let slot_ctx = in_ctx_virt.add(ctx_size) as *mut u32;
            core::ptr::write_volatile(slot_ctx, (1 << 27) | ((speed as u32) << 20));
            core::ptr::write_volatile(slot_ctx.add(1), (port_num as u32) << 16);

            // EP0 Context (index 2)
            let ep0_ctx = in_ctx_virt.add(2 * ctx_size) as *mut u32;
            let ep0_ring_phys = dma_phys + OFF_EP0_RING as u64;
            core::ptr::write_volatile(ep0_ctx.add(1), (4 << 3) | (3 << 1) | (64 << 16));
            core::ptr::write_volatile(ep0_ctx.add(2), (ep0_ring_phys as u32) | 1);
            core::ptr::write_volatile(ep0_ctx.add(3), (ep0_ring_phys >> 32) as u32);
            core::ptr::write_volatile(ep0_ctx.add(4), 8);
        }

        self.enqueue_cmd(
            in_ctx_phys,
            0,
            (TRB_ADDRESS_DEVICE << 10) | ((slot_id as u32) << 24),
        );
        let _ = self.wait_cmd_completion().await;

        // 3. Configure Endpoint Command for Interrupt-IN Endpoint (DCI = 3, EP1 IN)
        unsafe {
            // Input Control Context: Add Slot Context (bit 0) + EP1 IN (bit 3)
            core::ptr::write_volatile(in_ctx_virt.add(4) as *mut u32, (1 << 0) | (1 << 3));

            let slot_ctx = in_ctx_virt.add(ctx_size) as *mut u32;
            core::ptr::write_volatile(slot_ctx, (3 << 27) | ((speed as u32) << 20));

            // EP1 IN Context (index 4 in Input Context = Control(0) + Slot(1) + EP0(2) + EP1_OUT(3) + EP1_IN(4))
            let ep1_in_ctx = in_ctx_virt.add(4 * ctx_size) as *mut u32;
            let int_ring_phys = dma_phys + OFF_INT_RING as u64;
            core::ptr::write_volatile(ep1_in_ctx, 6 << 16); // Interval = 6
            core::ptr::write_volatile(ep1_in_ctx.add(1), (7 << 3) | (3 << 1) | (8 << 16)); // EP Type 7 (Int IN), MaxPkt 8
            core::ptr::write_volatile(ep1_in_ctx.add(2), (int_ring_phys as u32) | 1);
            core::ptr::write_volatile(ep1_in_ctx.add(3), (int_ring_phys >> 32) as u32);
            core::ptr::write_volatile(ep1_in_ctx.add(4), (8 << 16) | 8); // ESIT=8, AvgTRB=8
        }

        self.enqueue_cmd(
            in_ctx_phys,
            0,
            (TRB_CONFIGURE_ENDPOINT << 10) | ((slot_id as u32) << 24),
        );
        let _ = self.wait_cmd_completion().await;

        // 4. Queue initial Interrupt-IN Normal TRBs for 8-byte HID Boot reports
        for _ in 0..8 {
            self.queue_interrupt_in_trb();
        }
        self.ring_ep_doorbell();

        Ok(())
    }

    fn write_trb_raw(addr: *mut u8, param: u64, status: u32, control: u32) {
        unsafe {
            let p = addr as *mut u32;
            core::ptr::write_volatile(p, param as u32);
            core::ptr::write_volatile(p.add(1), (param >> 32) as u32);
            core::ptr::write_volatile(p.add(2), status);
            core::ptr::write_volatile(p.add(3), control);
        }
    }

    fn enqueue_cmd(&mut self, param: u64, status: u32, control: u32) {
        let dma = self.dma.as_ref().unwrap();
        let idx = self.cmd_enqueue;
        let cycle = self.cmd_cycle as u32;
        Self::write_trb_raw(
            unsafe { dma.virt.add(OFF_CMD_RING + idx * 16) },
            param,
            status,
            (control & !1) | cycle,
        );
        self.cmd_enqueue += 1;
        if self.cmd_enqueue >= RING_SIZE - 1 {
            // Update Link TRB cycle bit and wrap
            let link_addr = unsafe { dma.virt.add(OFF_CMD_RING + (RING_SIZE - 1) * 16) };
            Self::write_trb_raw(
                link_addr,
                dma.phys + OFF_CMD_RING as u64,
                0,
                (TRB_LINK << 10) | (1 << 1) | cycle,
            );
            self.cmd_enqueue = 0;
            self.cmd_cycle = !self.cmd_cycle;
        }
        self.hal.mmio_write32(self.db_base, 0);
    }

    async fn wait_cmd_completion(&mut self) -> Option<(u8, u8)> {
        for _ in 0..200 {
            if let Some((trb_type, _param, status, control)) = self.pop_event_trb()
                && trb_type == TRB_CMD_COMPLETION
            {
                let cc = ((status >> 24) & 0xFF) as u8;
                let slot = ((control >> 24) & 0xFF) as u8;
                return Some((cc, slot));
            }
            self.hal.yield_now().await;
        }
        None
    }

    fn queue_interrupt_in_trb(&mut self) {
        let dma = match self.dma.as_ref() {
            Some(d) => d,
            None => return,
        };
        let idx = self.int_enqueue;
        let cycle = self.int_cycle as u32;
        let report_phys = dma.phys + (OFF_REPORTS + idx * 8) as u64;
        Self::write_trb_raw(
            unsafe { dma.virt.add(OFF_INT_RING + idx * 16) },
            report_phys,
            8,                                     // Transfer Length = 8 bytes
            (TRB_NORMAL << 10) | (1 << 5) | cycle, // IOC = 1
        );
        self.int_enqueue += 1;
        if self.int_enqueue >= RING_SIZE - 1 {
            let link_addr = unsafe { dma.virt.add(OFF_INT_RING + (RING_SIZE - 1) * 16) };
            Self::write_trb_raw(
                link_addr,
                dma.phys + OFF_INT_RING as u64,
                0,
                (TRB_LINK << 10) | (1 << 1) | cycle,
            );
            self.int_enqueue = 0;
            self.int_cycle = !self.int_cycle;
        }
    }

    fn ring_ep_doorbell(&self) {
        let slot = self.active_slot.max(1) as u64;
        // Target 3 = Endpoint 1 IN (DCI 3)
        self.hal.mmio_write32(self.db_base + slot * 4, 3);
    }

    fn pop_event_trb(&mut self) -> Option<(u32, u64, u32, u32)> {
        let dma = self.dma.as_ref()?;
        let er_virt = unsafe { dma.virt.add(OFF_EVENT_RING + self.er_dequeue * 16) };
        let (param_lo, param_hi, status, control) = unsafe {
            let p = er_virt as *const u32;
            (
                core::ptr::read_volatile(p),
                core::ptr::read_volatile(p.add(1)),
                core::ptr::read_volatile(p.add(2)),
                core::ptr::read_volatile(p.add(3)),
            )
        };
        if ((control & 1) != 0) != self.er_cycle {
            return None;
        }

        self.er_dequeue += 1;
        if self.er_dequeue >= RING_SIZE {
            self.er_dequeue = 0;
            self.er_cycle = !self.er_cycle;
        }

        let erdp = dma.phys + (OFF_EVENT_RING + self.er_dequeue * 16) as u64;
        let ir0 = self.rt_base + 0x20;
        self.hal.mmio_write32(ir0 + 0x18, (erdp as u32) | (1 << 3));
        self.hal.mmio_write32(ir0 + 0x1C, (erdp >> 32) as u32);

        let trb_type = (control >> 10) & 0x3F;
        let param = ((param_hi as u64) << 32) | (param_lo as u64);
        Some((trb_type, param, status, control))
    }

    /// Polls the xHCI Event Ring for completed Interrupt-IN transfer events and returns the next `KeyEvent` if any.
    pub fn poll_key(&mut self) -> Option<KeyEvent> {
        if let Some(ev) = self.pending_keys.pop_front() {
            return Some(ev);
        }

        for _ in 0..RING_SIZE {
            let (trb_type, param, _status, control) = self.pop_event_trb()?;
            if trb_type != TRB_TRANSFER_EVENT {
                continue;
            }

            let dma = self.dma.as_ref()?;
            let int_ring_phys = dma.phys + OFF_INT_RING as u64;
            let reports_phys = dma.phys + OFF_REPORTS as u64;

            let report: [u8; 8] = if (control & (1 << 2)) != 0 {
                // Event Data bit set: 8-byte report is inline in `param`
                param.to_le_bytes()
            } else if param >= int_ring_phys && param < int_ring_phys + (RING_SIZE * 16) as u64 {
                // `param` points to the Normal TRB in the Interrupt-IN ring
                let trb_idx = ((param - int_ring_phys) / 16) as usize;
                let trb_virt = unsafe { dma.virt.add(OFF_INT_RING + trb_idx * 16) };
                let buf_phys = unsafe { core::ptr::read_volatile(trb_virt as *const u64) };
                let buf_virt = if buf_phys >= dma.phys && buf_phys + 8 <= dma.phys + dma.size as u64
                {
                    unsafe { dma.virt.add((buf_phys - dma.phys) as usize) }
                } else {
                    self.hal.phys_to_virt(buf_phys)
                };
                let mut r = [0u8; 8];
                unsafe {
                    core::ptr::copy_nonoverlapping(buf_virt as *const u8, r.as_mut_ptr(), 8);
                }
                r
            } else if param >= reports_phys && param + 8 <= reports_phys + (RING_SIZE * 8) as u64 {
                let buf_virt =
                    unsafe { dma.virt.add(OFF_REPORTS + (param - reports_phys) as usize) };
                let mut r = [0u8; 8];
                unsafe {
                    core::ptr::copy_nonoverlapping(buf_virt as *const u8, r.as_mut_ptr(), 8);
                }
                r
            } else {
                let mut r = [0u8; 8];
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        dma.virt.add(OFF_REPORTS) as *const u8,
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
        if let Some(dma) = self.dma.take() {
            let cmd = self.hal.mmio_read32(self.op_base);
            if (cmd & 1) != 0 {
                self.hal.mmio_write32(self.op_base, cmd & !1);
            }
            self.hal.free_dma(dma);
        }
    }
}
