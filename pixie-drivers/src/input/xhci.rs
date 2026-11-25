//! xHCI USB HID Boot-Protocol Keyboard driver.

use alloc::vec::Vec;
use core::mem::ManuallyDrop;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal};
use crate::input::KeyEvent;
use crate::xhci::{
    CC_BABBLE_DETECTED, CC_DATA_BUFFER_ERROR, CC_SHORT_PACKET, CC_STALL_ERROR, CC_SUCCESS,
    CC_USB_TX_ERROR, CTRL_BUF_SIZE, DEV_OFF_EP_RING, DEV_OFF_OUT_CTX, EP_TYPE_INT_IN,
    EndpointConfig, RING_SIZE, RingState, SharedXhci, XhciController, compute_xhci_interval,
    endpoint_dci,
};

const HID_MOD_LCTRL: u8 = 1 << 0;
const HID_MOD_LSHIFT: u8 = 1 << 1;
const HID_MOD_LALT: u8 = 1 << 2;
const HID_MOD_RCTRL: u8 = 1 << 4;
const HID_MOD_RSHIFT: u8 = 1 << 5;
const HID_MOD_RALT: u8 = 1 << 6;

const REPORT_DMA_PAGES: usize = 1;
const MAX_HID_REPORT_LEN: usize = 64;

/// Decode newly-pressed keys comparing an 8-byte HID boot keyboard `curr` report against `prev`
/// into `out` without any heap allocation. Returns the number of events written.
pub fn decode_hid_boot_report_into(
    prev: &[u8; 8],
    curr: &[u8; 8],
    out: &mut [Option<KeyEvent>; 8],
) -> usize {
    *out = [None; 8];
    // USB HID ErrorRollOver (0x01) means key matrix overflow; ignore entire report cleanly.
    if curr[2..8].contains(&0x01) {
        return 0;
    }

    let mods = curr[0];
    let shift = (mods & (HID_MOD_LSHIFT | HID_MOD_RSHIFT)) != 0;
    let ctrl = (mods & (HID_MOD_LCTRL | HID_MOD_RCTRL)) != 0;
    let alt = (mods & (HID_MOD_LALT | HID_MOD_RALT)) != 0;

    let mut count = 0usize;
    for &code in &curr[2..8] {
        if code == 0 || prev[2..8].contains(&code) {
            continue;
        }
        let ev = match code {
            0x04..=0x1D => {
                let offset = code - 0x04;
                if ctrl && alt {
                    None
                } else {
                    let base = if shift { b'A' } else { b'a' };
                    Some(KeyEvent::Char((base + offset) as char))
                }
            }
            0x1E..=0x26 => {
                let normal = b"123456789";
                let shifted = b"!@#$%^&*(";
                let idx = (code - 0x1E) as usize;
                Some(KeyEvent::Char(
                    if shift { shifted[idx] } else { normal[idx] } as char,
                ))
            }
            0x27 => Some(KeyEvent::Char(if shift { ')' } else { '0' })),
            0x28 | 0x58 => Some(KeyEvent::Enter),
            0x29 => Some(KeyEvent::Char('\x1B')),
            0x2A => Some(KeyEvent::Char('\x08')),
            0x2B => Some(KeyEvent::Char('\t')),
            0x2C => Some(KeyEvent::Char(' ')),
            0x2D => Some(KeyEvent::Char(if shift { '_' } else { '-' })),
            0x2E => Some(KeyEvent::Char(if shift { '+' } else { '=' })),
            0x2F => Some(KeyEvent::Char(if shift { '{' } else { '[' })),
            0x30 => Some(KeyEvent::Char(if shift { '}' } else { ']' })),
            0x31 => Some(KeyEvent::Char(if shift { '|' } else { '\\' })),
            0x33 => Some(KeyEvent::Char(if shift { ':' } else { ';' })),
            0x34 => Some(KeyEvent::Char(if shift { '"' } else { '\'' })),
            0x35 => Some(KeyEvent::Char(if shift { '~' } else { '`' })),
            0x36 => Some(KeyEvent::Char(if shift { '<' } else { ',' })),
            0x37 => Some(KeyEvent::Char(if shift { '>' } else { '.' })),
            0x38 => Some(KeyEvent::Char(if shift { '?' } else { '/' })),
            0x4C | 0x63 => {
                if ctrl && alt {
                    Some(KeyEvent::CtrlAltDel)
                } else {
                    Some(KeyEvent::Delete)
                }
            }
            0x4F => Some(KeyEvent::Right),
            0x50 => Some(KeyEvent::Left),
            0x51 => Some(KeyEvent::Down),
            0x52 => Some(KeyEvent::Up),
            _ => None,
        };
        if let Some(e) = ev
            && count < out.len()
        {
            out[count] = Some(e);
            count += 1;
        }
    }
    count
}

/// Decode newly-pressed keys comparing an 8-byte HID boot keyboard `curr` report against `prev`.
pub fn decode_hid_boot_report(prev: &[u8; 8], curr: &[u8; 8]) -> Vec<KeyEvent> {
    let mut out = [None; 8];
    let n = decode_hid_boot_report_into(prev, curr, &mut out);
    let mut vec = Vec::with_capacity(n);
    for slot in out.iter().take(n).flatten() {
        vec.push(*slot);
    }
    vec
}

#[derive(Clone, Copy)]
struct HidMatch {
    config_val: u8,
    iface_num: u8,
    ep_dci: u8,
    int_mps: u16,
    b_interval: u8,
    is_boot_kbd: bool,
}

fn find_hid_keyboard_interface(cfg_buf: &[u8], allow_fallback_hid: bool) -> Option<HidMatch> {
    if cfg_buf.len() < 9 || cfg_buf[1] != 2 {
        return None;
    }
    let total_len = (u16::from_le_bytes([cfg_buf[2], cfg_buf[3]]) as usize).min(cfg_buf.len());
    let config_val = cfg_buf[5].max(1);

    let mut cur_iface: Option<(u8, bool, bool)> = None; // (iface_num, is_kbd_candidate, is_boot_kbd)
    let mut best_fallback: Option<HidMatch> = None;

    let mut off = 0usize;
    while off + 2 <= total_len {
        let len = cfg_buf[off] as usize;
        let dtype = cfg_buf[off + 1];
        if len < 2 || off + len > total_len {
            break;
        }
        match dtype {
            4 if len >= 9 => {
                let iface_num = cfg_buf[off + 2];
                let class = cfg_buf[off + 5];
                let subclass = cfg_buf[off + 6];
                let proto = cfg_buf[off + 7];
                let is_boot_kbd = class == 0x03 && subclass == 0x01 && proto == 0x01;
                let is_mouse = class == 0x03 && subclass == 0x01 && proto == 0x02;
                let is_kbd_candidate =
                    is_boot_kbd || (allow_fallback_hid && class == 0x03 && !is_mouse);
                cur_iface = Some((iface_num, is_kbd_candidate, is_boot_kbd));
            }
            5 if len >= 7 => {
                if let Some((iface_num, true, is_boot_kbd)) = cur_iface {
                    let ep_addr = cfg_buf[off + 2];
                    let bm_attr = cfg_buf[off + 3];
                    let mps = (u16::from_le_bytes([cfg_buf[off + 4], cfg_buf[off + 5]]) & 0x07FF)
                        .clamp(8, MAX_HID_REPORT_LEN as u16);
                    let b_interval = cfg_buf[off + 6];
                    if (ep_addr & 0x80) != 0 && (bm_attr & 0x03) == 3 {
                        let m = HidMatch {
                            config_val,
                            iface_num,
                            ep_dci: endpoint_dci(ep_addr),
                            int_mps: mps,
                            b_interval,
                            is_boot_kbd,
                        };
                        if is_boot_kbd {
                            return Some(m);
                        }
                        if best_fallback.is_none() {
                            best_fallback = Some(m);
                        }
                    }
                }
            }
            _ => {}
        }
        off += len;
    }

    best_fallback
}

struct XhciKeyboardInner {
    slot_dma: ManuallyDrop<DmaBuffer>,
    report_dma: ManuallyDrop<DmaBuffer>,
    ep0_ring: RingState,
    int_ring: RingState,
    expected_trb_phys: u64,
    report_len: usize,
    port_num: u8,
    slot_id: u8,
    int_dci: u8,
    prev_report: [u8; 8],
    pending_events: [Option<KeyEvent>; 8],
    pending_head: usize,
    pending_len: usize,
}

impl XhciKeyboardInner {
    fn rearm_interrupt<H: Hal>(&mut self, g: &XhciController<H>) {
        let int_ring_virt = unsafe { self.slot_dma.virt.add(DEV_OFF_EP_RING) };
        let int_ring_phys = self.slot_dma.phys + DEV_OFF_EP_RING as u64;
        self.expected_trb_phys = g.enqueue_normal(
            int_ring_virt,
            int_ring_phys,
            &mut self.int_ring,
            self.report_dma.phys,
            self.report_len as u32,
            true,
            true,
        );
        g.ring_doorbell(self.slot_id, self.int_dci as u32);
        let _ = RING_SIZE;
    }
}

/// Polling-mode xHCI USB HID keyboard driver.
pub struct XhciKeyboard<H: Hal> {
    ctrl: SharedXhci<H>,
    inner: XhciKeyboardInner,
}

unsafe impl<H: Hal + Send> Send for XhciKeyboard<H> {}

impl<H: Hal> XhciKeyboard<H> {
    /// Initialize a standalone xHCI host controller at `mmio_base` and probe for a USB keyboard.
    pub async fn new(mmio_base: u64, hal: H) -> Result<Self> {
        let ctrl = XhciController::new_shared(mmio_base, hal)?;
        Self::attach(ctrl).await
    }

    /// Attach a USB HID Boot Keyboard on a shared xHCI host controller.
    pub async fn attach(ctrl: SharedXhci<H>) -> Result<Self> {
        // Two-pass scan: Pass 1 looks strictly for Boot Keyboard (03/01/01),
        // Pass 2 allows non-mouse HID keyboards on any unclaimed port.
        for allow_fallback_hid in [false, true] {
            let max_ports = {
                let g = ctrl.lock().await;
                g.caps().max_ports
            };
            for port_num in 1..=max_ports {
                let mut g = ctrl.lock().await;
                if g.is_port_claimed(port_num) {
                    continue;
                }
                if let Some(kbd) =
                    Self::try_probe_port(&mut g, &ctrl, port_num, allow_fallback_hid)?
                {
                    return Ok(kbd);
                }
            }
        }
        Err(Error::msg("No USB HID keyboard found on xHCI ports"))
    }

    fn try_probe_port(
        g: &mut XhciController<H>,
        ctrl_arc: &SharedXhci<H>,
        port_num: u8,
        allow_fallback_hid: bool,
    ) -> Result<Option<Self>> {
        let Ok(speed) = g.ensure_port_ready(port_num) else {
            return Ok(None);
        };
        let Ok(slot_id) = g.enable_slot() else {
            return Ok(None);
        };
        let mut slot_dma = match g.alloc_slot_dma() {
            Ok(d) => d,
            Err(e) => {
                let _ = g.disable_slot(slot_id);
                return Err(e);
            }
        };
        let mut ep0_ring = RingState::new();
        if g.address_device(slot_id, port_num, speed, &mut slot_dma, &mut ep0_ring)
            .is_err()
        {
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Ok(None);
        }

        // For Full-Speed devices, read 8 bytes of Device Descriptor to check bMaxPacketSize0.
        if speed == 1 {
            let mut dev8 = [0u8; 8];
            let setup_dev8 = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x08, 0x00];
            if let Ok(n) =
                g.control_in(slot_id, &mut slot_dma, &mut ep0_ring, setup_dev8, &mut dev8)
                && n >= 8
                && dev8[7] >= 8
                && dev8[7] != 8
            {
                let _ = g.evaluate_ep0_mps(slot_id, dev8[7] as u16, &slot_dma);
            }
        }

        // Read Device Descriptor (for bNumConfigurations)
        let mut dev_desc = [0u8; 18];
        let setup_dev = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0x00];
        let num_cfgs = match g.control_in(
            slot_id,
            &mut slot_dma,
            &mut ep0_ring,
            setup_dev,
            &mut dev_desc,
        ) {
            Ok(n) if n >= 18 && dev_desc[1] == 1 => dev_desc[17].clamp(1, 8),
            _ => 1,
        };

        let mut matched: Option<HidMatch> = None;
        let mut cfg_buf = [0u8; CTRL_BUF_SIZE];
        for cfg_idx in 0..num_cfgs {
            let setup_hdr = [0x80, 0x06, cfg_idx, 0x02, 0x00, 0x00, 9, 0x00];
            let Ok(n) = g.control_in(
                slot_id,
                &mut slot_dma,
                &mut ep0_ring,
                setup_hdr,
                &mut cfg_buf[..9],
            ) else {
                continue;
            };
            if n < 9 || cfg_buf[1] != 2 {
                continue;
            }
            let total_len =
                (u16::from_le_bytes([cfg_buf[2], cfg_buf[3]]) as usize).clamp(9, CTRL_BUF_SIZE);
            if total_len > 9 {
                let len_le = (total_len as u16).to_le_bytes();
                let setup_full = [0x80, 0x06, cfg_idx, 0x02, 0x00, 0x00, len_le[0], len_le[1]];
                let Ok(full_n) = g.control_in(
                    slot_id,
                    &mut slot_dma,
                    &mut ep0_ring,
                    setup_full,
                    &mut cfg_buf[..total_len],
                ) else {
                    continue;
                };
                if let Some(m) = find_hid_keyboard_interface(
                    &cfg_buf[..full_n.min(total_len)],
                    allow_fallback_hid,
                ) {
                    matched = Some(m);
                    break;
                }
            } else if let Some(m) = find_hid_keyboard_interface(&cfg_buf[..n], allow_fallback_hid) {
                matched = Some(m);
                break;
            }
        }

        let Some(hid) = matched else {
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Ok(None);
        };

        // SET_CONFIGURATION(config_val)
        let setup_set_cfg = [0x00, 0x09, hid.config_val, 0x00, 0x00, 0x00, 0x00, 0x00];
        if g.control_out(slot_id, &mut slot_dma, &mut ep0_ring, setup_set_cfg, &[])
            .is_err()
        {
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Ok(None);
        }

        // HID SET_PROTOCOL(Boot = 0)
        let setup_set_proto = [0x21, 0x0B, 0x00, 0x00, hid.iface_num, 0x00, 0x00, 0x00];
        let proto_res = g.control_out(slot_id, &mut slot_dma, &mut ep0_ring, setup_set_proto, &[]);
        if proto_res.is_err() && hid.is_boot_kbd {
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Ok(None);
        }

        // HID SET_IDLE(0) — optional per HID spec (some keyboards STALL it cleanly)
        let setup_set_idle = [0x21, 0x0A, 0x00, 0x00, hid.iface_num, 0x00, 0x00, 0x00];
        let _ = g.control_out(slot_id, &mut slot_dma, &mut ep0_ring, setup_set_idle, &[]);

        // Allocate Interrupt IN report buffer DMA
        let Some(report_dma) = g.hal().alloc_dma(REPORT_DMA_PAGES) else {
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Err(Error::msg("Failed to allocate HID report DMA"));
        };
        if !g.check_dma_bounds(&report_dma, REPORT_DMA_PAGES) {
            g.hal().free_dma(report_dma);
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Err(Error::msg("HID report DMA exceeds 32-bit limit"));
        }
        unsafe {
            core::ptr::write_bytes(report_dma.virt, 0, REPORT_DMA_PAGES * 4096);
        }

        // Configure Interrupt IN Endpoint
        let mut int_ring = RingState::new();
        let int_ring_virt = unsafe { slot_dma.virt.add(DEV_OFF_EP_RING) };
        let int_ring_phys = slot_dma.phys + DEV_OFF_EP_RING as u64;
        g.init_transfer_ring(int_ring_virt, int_ring_phys, &mut int_ring);

        let ep_cfg = EndpointConfig {
            ep_dci: hid.ep_dci,
            ep_type: EP_TYPE_INT_IN,
            max_packet_size: hid.int_mps,
            max_burst: 0,
            interval: compute_xhci_interval(speed, hid.b_interval, true),
            avg_trb_len: hid.int_mps,
            max_esit_payload_lo: hid.int_mps,
            ring_phys: int_ring_phys,
        };
        if g.configure_endpoints(slot_id, &slot_dma, &[ep_cfg])
            .is_err()
        {
            g.hal().free_dma(report_dma);
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Ok(None);
        }

        let report_len = (hid.int_mps as usize).clamp(8, MAX_HID_REPORT_LEN);
        let trb_phys = g.enqueue_normal(
            int_ring_virt,
            int_ring_phys,
            &mut int_ring,
            report_dma.phys,
            report_len as u32,
            true,
            true,
        );
        g.ring_doorbell(slot_id, hid.ep_dci as u32);
        let _ = g.claim_port(port_num);

        // Sanity zero of Output Context isn't overwritten later
        let _ = DEV_OFF_OUT_CTX;

        Ok(Some(Self {
            ctrl: ctrl_arc.clone(),
            inner: XhciKeyboardInner {
                slot_dma: ManuallyDrop::new(slot_dma),
                report_dma: ManuallyDrop::new(report_dma),
                ep0_ring,
                int_ring,
                expected_trb_phys: trb_phys,
                report_len,
                port_num,
                slot_id,
                int_dci: hid.ep_dci,
                prev_report: [0; 8],
                pending_events: [None; 8],
                pending_head: 0,
                pending_len: 0,
            },
        }))
    }

    /// Non-blocking poll for the next keyboard event (zero heap allocation).
    pub fn poll_key(&mut self) -> Option<KeyEvent> {
        let inner = &mut self.inner;
        if inner.pending_head < inner.pending_len {
            let ev = inner.pending_events[inner.pending_head].take();
            inner.pending_head += 1;
            if inner.pending_head >= inner.pending_len {
                inner.pending_head = 0;
                inner.pending_len = 0;
            }
            if ev.is_some() {
                return ev;
            }
        }

        let mut g = self.ctrl.try_lock()?;
        while let Some(ev) = g.pop_transfer_event(inner.slot_id, inner.int_dci) {
            if ev.trb_ptr != 0 && ev.trb_ptr != inner.expected_trb_phys {
                continue;
            }
            let cc = ev.cc();
            match cc {
                CC_SUCCESS | CC_SHORT_PACKET => {
                    let received = inner.report_len.saturating_sub(ev.residual() as usize);
                    if received >= 8 {
                        fence(Ordering::Acquire);
                        let mut report = [0u8; 8];
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                inner.report_dma.virt,
                                report.as_mut_ptr(),
                                8,
                            );
                        }
                        if !report[2..8].contains(&0x01) {
                            let n = decode_hid_boot_report_into(
                                &inner.prev_report,
                                &report,
                                &mut inner.pending_events,
                            );
                            inner.prev_report = report;
                            inner.pending_head = 0;
                            inner.pending_len = n;
                        }
                    }
                    inner.rearm_interrupt(&g);
                }
                CC_STALL_ERROR | CC_USB_TX_ERROR | CC_DATA_BUFFER_ERROR | CC_BABBLE_DETECTED => {
                    let int_ring_phys = inner.slot_dma.phys + DEV_OFF_EP_RING as u64;
                    let _ = g.recover_halted_endpoint(
                        inner.slot_id,
                        inner.int_dci,
                        &mut inner.int_ring,
                        int_ring_phys,
                        cc == CC_STALL_ERROR,
                        &mut inner.slot_dma,
                        &mut inner.ep0_ring,
                    );
                    inner.rearm_interrupt(&g);
                }
                _ => {
                    inner.rearm_interrupt(&g);
                }
            }

            if inner.pending_head < inner.pending_len {
                let key = inner.pending_events[inner.pending_head].take();
                inner.pending_head += 1;
                if inner.pending_head >= inner.pending_len {
                    inner.pending_head = 0;
                    inner.pending_len = 0;
                }
                if key.is_some() {
                    return key;
                }
            }
        }
        None
    }
}

impl<H: Hal> Drop for XhciKeyboard<H> {
    fn drop(&mut self) {
        if let Some(mut g) = self.ctrl.try_lock() {
            let disabled = g.disable_slot(self.inner.slot_id).is_ok() || g.is_halted();
            g.release_port(self.inner.port_num);
            if disabled {
                let slot_dma = unsafe { ManuallyDrop::take(&mut self.inner.slot_dma) };
                let report_dma = unsafe { ManuallyDrop::take(&mut self.inner.report_dma) };
                g.hal().free_dma(report_dma);
                g.hal().free_dma(slot_dma);
            }
        }
        // If controller lock couldn't be taken (e.g. panic inside lock), leave DMA leaked rather than UAF.
    }
}
