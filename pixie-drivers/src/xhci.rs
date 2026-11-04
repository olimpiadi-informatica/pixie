use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE};

pub(crate) const RING_SIZE: usize = 64;
pub(crate) const CTRL_DMA_PAGES: usize = 4;

pub(crate) const TRB_NORMAL: u32 = 1;
pub(crate) const TRB_SETUP_STAGE: u32 = 2;
pub(crate) const TRB_DATA_STAGE: u32 = 3;
pub(crate) const TRB_STATUS_STAGE: u32 = 4;
pub(crate) const TRB_LINK: u32 = 6;
pub(crate) const TRB_ENABLE_SLOT: u32 = 9;
pub(crate) const TRB_DISABLE_SLOT: u32 = 10;
pub(crate) const TRB_ADDRESS_DEVICE: u32 = 11;
pub(crate) const TRB_CONFIGURE_ENDPOINT: u32 = 12;
pub(crate) const TRB_TRANSFER_EVENT: u32 = 32;
pub(crate) const TRB_CMD_COMPLETION: u32 = 33;

// Shared 4-page (16 KiB) xHCI control DMA layout:
// Page 0 (0x0000..0x1000):
//   0x0000..0x0800: DCBAA (256 * 8 B = 2048 B, 64-B aligned)
//   0x0800..0x0840: ERST (64 B, 64-B aligned)
//   0x0C00..0x1000: Driver-specific ring (e.g. Bulk-OUT ring in usb_net)
// Page 1 (0x1000..0x2000):
//   0x1000..0x1400: Command Ring (64 * 16 B = 1024 B)
//   0x1400..0x1800: Event Ring (64 * 16 B = 1024 B)
//   0x1800..0x1C00: EP0 Control Ring (64 * 16 B = 1024 B)
//   0x1C00..0x2000: Transfer-IN Ring (Interrupt-IN or Bulk-IN, 64 * 16 B = 1024 B)
// Page 2 (0x2000..0x3000):
//   0x2000..0x2800: Output Device Context (32 * 64 B = 2048 B, page-aligned)
//   0x2800..0x2A00: EP0 Control Data Buffer (512 B)
//   0x2A00..0x2C00: Driver-specific scratch (e.g. 64 * 8 B HID reports in input::xhci)
// Page 3 (0x3000..0x4000):
//   0x3000..0x3840: Input Context (33 * 64 B = 0x840 B, page-aligned, never crosses page boundary)
pub(crate) const OFF_DCBAA: usize = 0x0000;
pub(crate) const OFF_ERST: usize = 0x0800;
pub(crate) const OFF_CMD_RING: usize = 0x1000;
pub(crate) const OFF_EVENT_RING: usize = 0x1400;
pub(crate) const OFF_EP0_RING: usize = 0x1800;
pub(crate) const OFF_XFER_IN_RING: usize = 0x1C00;
pub(crate) const OFF_OUT_CTX: usize = 0x2000;
pub(crate) const OFF_CTRL_BUF: usize = 0x2800;
pub(crate) const CTRL_BUF_SIZE: usize = 512;
pub(crate) const OFF_IN_CTX: usize = 0x3000;
pub(crate) const IN_CTX_MAX_BYTES: usize = 0x0840;

const STATIC_CLOCK_SPIN_CAP: usize = 200_000;

#[inline]
pub(crate) fn is_transfer_cc_ok(cc: u8) -> bool {
    cc == 1 || cc == 13
}

/// Polls `cond()` until it returns `true`, `timeout_us` microseconds elapse on `hal.micros()`,
/// or `STATIC_CLOCK_SPIN_CAP` iterations pass without `hal.micros()` advancing (mocked static clock).
pub(crate) fn spin_wait<H: Hal, F: FnMut() -> bool>(hal: &H, timeout_us: i64, mut cond: F) -> bool {
    let start = hal.micros();
    let mut last = start;
    let mut stalled = 0usize;
    loop {
        if cond() {
            return true;
        }
        core::hint::spin_loop();
        let now = hal.micros();
        if now.saturating_sub(start) >= timeout_us {
            return cond();
        }
        if now == last {
            stalled += 1;
            if stalled >= STATIC_CLOCK_SPIN_CAP {
                return cond();
            }
        } else {
            last = now;
            stalled = 0;
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct XhciCaps {
    pub(crate) op_off: u32,
    pub(crate) rt_off: u32,
    pub(crate) db_off: u32,
    pub(crate) xecp_off: u32,
    pub(crate) max_slots: u8,
    pub(crate) max_ports: u8,
    pub(crate) ctx_size: u8,
    pub(crate) max_scratchpad: u16,
}

impl XhciCaps {
    pub(crate) fn read_and_map<H: Hal>(mmio_base: u64, hal: &H) -> Result<Self> {
        if mmio_base == 0 {
            return Err(Error::msg("xHCI MMIO base is 0"));
        }
        hal.map_mmio(mmio_base, PAGE_SIZE);
        let cap_hciver = hal.mmio_read32(mmio_base);
        if cap_hciver == 0 || cap_hciver == 0xFFFF_FFFF {
            return Err(Error::msg("xHCI controller not present"));
        }

        let op_off = cap_hciver & 0xFF;
        let hcsparams1 = hal.mmio_read32(mmio_base + 0x04);
        let hcsparams2 = hal.mmio_read32(mmio_base + 0x08);
        let hccparams1 = hal.mmio_read32(mmio_base + 0x10);
        let db_off = hal.mmio_read32(mmio_base + 0x14) & !0x3;
        let rt_off = hal.mmio_read32(mmio_base + 0x18) & !0x1F;

        let max_slots = (hcsparams1 & 0xFF).clamp(1, 64) as u8;
        let max_ports = ((hcsparams1 >> 24) & 0xFF) as u8;
        let ctx_size = if (hccparams1 & (1 << 2)) != 0 { 64 } else { 32 };
        let xecp_off = ((hccparams1 >> 16) & 0xFFFF) * 4;
        let max_scratchpad =
            ((((hcsparams2 >> 21) & 0x1F) << 5) | ((hcsparams2 >> 27) & 0x1F)) as u16;

        let op_end = (op_off as usize) + 0x400 + (max_ports as usize) * 0x10;
        let rt_end = (rt_off as usize) + 0x400;
        let db_end = (db_off as usize) + ((max_slots as usize) + 1) * 4;
        let xecp_end = if xecp_off != 0 {
            (xecp_off as usize) + 0x1000
        } else {
            0
        };
        let span = op_end
            .max(rt_end)
            .max(db_end)
            .max(xecp_end)
            .max(0x10000)
            .next_multiple_of(PAGE_SIZE);
        hal.map_mmio(mmio_base, span);

        Ok(Self {
            op_off,
            rt_off,
            db_off,
            xecp_off,
            max_slots,
            max_ports,
            ctx_size,
            max_scratchpad,
        })
    }
}

pub(crate) struct XhciScratchpad {
    array_dma: DmaBuffer,
    pages: Vec<DmaBuffer>,
}

impl XhciScratchpad {
    pub(crate) fn alloc<H: Hal>(
        hal: &H,
        count: u16,
        dcbaa_virt: *mut u8,
    ) -> Result<Option<Box<Self>>> {
        if count == 0 {
            return Ok(None);
        }
        let n = count as usize;
        let array_pages = (n * 8).div_ceil(PAGE_SIZE).max(1);
        let array_dma = hal
            .alloc_dma(array_pages)
            .ok_or_else(|| Error::msg("Out of DMA memory for xHCI scratchpad array"))?;
        unsafe {
            ptr::write_bytes(array_dma.virt, 0, array_dma.size);
        }

        let mut pages = Vec::with_capacity(n);
        for i in 0..n {
            let Some(page) = hal.alloc_dma(1) else {
                for prev in pages {
                    hal.free_dma(prev);
                }
                hal.free_dma(array_dma);
                return Err(Error::msg("Out of DMA memory for xHCI scratchpad buffer"));
            };
            unsafe {
                ptr::write_bytes(page.virt, 0, page.size);
                ptr::write_volatile((array_dma.virt as *mut u64).add(i), page.phys);
            }
            pages.push(page);
        }

        fence(Ordering::Release);
        unsafe {
            ptr::write_volatile(dcbaa_virt as *mut u64, array_dma.phys);
        }

        Ok(Some(Box::new(Self { array_dma, pages })))
    }

    pub(crate) fn free<H: Hal>(self, hal: &H) {
        for page in self.pages {
            hal.free_dma(page);
        }
        hal.free_dma(self.array_dma);
    }
}

pub(crate) fn bios_handoff_and_reset<H: Hal>(
    mmio_base: u64,
    caps: &XhciCaps,
    hal: &H,
) -> Result<()> {
    // 1. BIOS-to-OS Handoff via USBLEGSUP extended capability (Cap ID == 1)
    let mut ext_off = caps.xecp_off as u64;
    for _ in 0..64 {
        if ext_off == 0 {
            break;
        }
        let cap = hal.mmio_read32(mmio_base + ext_off);
        if cap == 0 || cap == 0xFFFF_FFFF {
            break;
        }
        if (cap & 0xFF) == 1 {
            hal.mmio_write32(mmio_base + ext_off, cap | (1 << 24));
            let _ = spin_wait(hal, 1_000_000, || {
                (hal.mmio_read32(mmio_base + ext_off) & (1 << 16)) == 0
            });
            hal.mmio_write32(mmio_base + ext_off + 4, 0);
            break;
        }
        let next_dw = ((cap >> 8) & 0xFF) as u64;
        if next_dw == 0 {
            break;
        }
        ext_off += next_dw * 4;
    }

    // 2. Halt and reset controller (USBCMD.HCRST)
    let op_base = mmio_base + caps.op_off as u64;
    let cmd = hal.mmio_read32(op_base);
    if (cmd & 1) != 0 {
        hal.mmio_write32(op_base, cmd & !1);
        let _ = spin_wait(hal, 100_000, || (hal.mmio_read32(op_base + 0x04) & 1) != 0);
    }

    hal.mmio_write32(op_base, 1 << 1);
    let reset_ok = spin_wait(hal, 1_000_000, || {
        let c = hal.mmio_read32(op_base);
        let s = hal.mmio_read32(op_base + 0x04);
        (c & (1 << 1)) == 0 && (s & (1 << 11)) == 0
    });
    if !reset_ok {
        return Err(Error::msg("xHCI HCRST timeout"));
    }
    Ok(())
}

pub(crate) fn setup_and_start_controller<H: Hal>(
    mmio_base: u64,
    caps: &XhciCaps,
    ctrl_dma: &DmaBuffer,
    hal: &H,
) -> Result<Option<Box<XhciScratchpad>>> {
    let scratchpad = XhciScratchpad::alloc(hal, caps.max_scratchpad, unsafe {
        ctrl_dma.virt.add(OFF_DCBAA)
    })?;

    unsafe {
        let erst_ptr = ctrl_dma.virt.add(OFF_ERST) as *mut u32;
        let er_phys = ctrl_dma.phys + OFF_EVENT_RING as u64;
        ptr::write_volatile(erst_ptr, er_phys as u32);
        ptr::write_volatile(erst_ptr.add(1), (er_phys >> 32) as u32);
        ptr::write_volatile(erst_ptr.add(2), RING_SIZE as u32);
        ptr::write_volatile(erst_ptr.add(3), 0);
    }
    fence(Ordering::SeqCst);

    let op_base = mmio_base + caps.op_off as u64;
    let rt_base = mmio_base + caps.rt_off as u64;
    let ir0 = rt_base + 0x20;

    let dcbaa_phys = ctrl_dma.phys + OFF_DCBAA as u64;
    let cmd_ring_phys = ctrl_dma.phys + OFF_CMD_RING as u64;
    let er_phys = ctrl_dma.phys + OFF_EVENT_RING as u64;
    let erst_phys = ctrl_dma.phys + OFF_ERST as u64;

    hal.mmio_write32(op_base + 0x38, caps.max_slots as u32);
    hal.mmio_write32(op_base + 0x30, dcbaa_phys as u32);
    hal.mmio_write32(op_base + 0x34, (dcbaa_phys >> 32) as u32);
    hal.mmio_write32(op_base + 0x18, (cmd_ring_phys as u32) | 1);
    hal.mmio_write32(op_base + 0x1C, (cmd_ring_phys >> 32) as u32);

    hal.mmio_write32(ir0 + 0x04, 0);
    hal.mmio_write32(ir0 + 0x08, 1);
    hal.mmio_write32(ir0 + 0x18, (er_phys as u32) | (1 << 3));
    hal.mmio_write32(ir0 + 0x1C, (er_phys >> 32) as u32);
    hal.mmio_write32(ir0 + 0x10, erst_phys as u32);
    hal.mmio_write32(ir0 + 0x14, (erst_phys >> 32) as u32);
    hal.mmio_write32(ir0, 0x02);

    hal.mmio_write32(op_base, 1 | (1 << 2));
    let _ = spin_wait(hal, 100_000, || (hal.mmio_read32(op_base + 0x04) & 1) == 0);

    Ok(scratchpad)
}

pub(crate) fn reset_and_probe_port<H: Hal>(op_base: u64, port_idx: usize, hal: &H) -> Option<u8> {
    let portsc_addr = op_base + 0x400 + (port_idx as u64) * 0x10;
    let mut portsc = hal.mmio_read32(portsc_addr);
    if portsc == 0xFFFF_FFFF || (portsc & 1) == 0 {
        return None;
    }
    if (portsc & (1 << 1)) == 0 {
        hal.mmio_write32(portsc_addr, (portsc & 0x0E01_C3E0) | (1 << 4));
        let _ = spin_wait(hal, 250_000, || {
            (hal.mmio_read32(portsc_addr) & (1 << 4)) == 0
        });
        portsc = hal.mmio_read32(portsc_addr);
        if (portsc & 1) == 0 {
            return None;
        }
    }
    Some((((portsc >> 10) & 0xF) as u8).max(1))
}

#[inline]
pub(crate) fn write_trb_ordered(addr: *mut u8, param: u64, status: u32, control: u32) {
    unsafe {
        let p = addr as *mut u32;
        ptr::write_volatile(p, param as u32);
        ptr::write_volatile(p.add(1), (param >> 32) as u32);
        ptr::write_volatile(p.add(2), status);
        fence(Ordering::Release);
        ptr::write_volatile(p.add(3), control);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TrbRing {
    off: u16,
    enqueue: u8,
    cycle: bool,
}

impl TrbRing {
    pub(crate) const fn new(off: usize) -> Self {
        Self {
            off: off as u16,
            enqueue: 0,
            cycle: true,
        }
    }

    pub(crate) fn init(&mut self, dma: &DmaBuffer) {
        self.enqueue = 0;
        self.cycle = true;
        let base_off = self.off as usize;
        unsafe {
            ptr::write_bytes(dma.virt.add(base_off), 0, RING_SIZE * 16);
            write_trb_ordered(
                dma.virt.add(base_off + (RING_SIZE - 1) * 16),
                dma.phys + base_off as u64,
                0,
                (TRB_LINK << 10) | (1 << 1),
            );
        }
        fence(Ordering::SeqCst);
    }

    pub(crate) fn enqueue(&mut self, dma: &DmaBuffer, param: u64, status: u32, control: u32) {
        let base_off = self.off as usize;
        let idx = self.enqueue as usize;
        let cycle = self.cycle as u32;
        unsafe {
            write_trb_ordered(
                dma.virt.add(base_off + idx * 16),
                param,
                status,
                (control & !1) | cycle,
            );
        }
        self.enqueue += 1;
        if (self.enqueue as usize) >= RING_SIZE - 1 {
            unsafe {
                write_trb_ordered(
                    dma.virt.add(base_off + (RING_SIZE - 1) * 16),
                    dma.phys + base_off as u64,
                    0,
                    (TRB_LINK << 10) | (1 << 1) | cycle,
                );
            }
            self.enqueue = 0;
            self.cycle = !self.cycle;
        }
        fence(Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct EventRing {
    dequeue: u8,
    cycle: bool,
}

impl EventRing {
    pub(crate) const fn new() -> Self {
        Self {
            dequeue: 0,
            cycle: true,
        }
    }

    pub(crate) fn pop<H: Hal>(
        &mut self,
        dma: &DmaBuffer,
        rt_base: u64,
        hal: &H,
    ) -> Option<(u32, u64, u32, u32)> {
        let er_virt = unsafe { dma.virt.add(OFF_EVENT_RING + (self.dequeue as usize) * 16) };
        let p = er_virt as *const u32;
        let control = unsafe { ptr::read_volatile(p.add(3)) };
        if ((control & 1) != 0) != self.cycle {
            return None;
        }
        fence(Ordering::Acquire);
        let (param_lo, param_hi, status) = unsafe {
            (
                ptr::read_volatile(p),
                ptr::read_volatile(p.add(1)),
                ptr::read_volatile(p.add(2)),
            )
        };

        self.dequeue += 1;
        if (self.dequeue as usize) >= RING_SIZE {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }

        let erdp = dma.phys + (OFF_EVENT_RING + (self.dequeue as usize) * 16) as u64;
        let ir0 = rt_base + 0x20;
        hal.mmio_write32(ir0 + 0x18, (erdp as u32) | (1 << 3));
        hal.mmio_write32(ir0 + 0x1C, (erdp >> 32) as u32);

        let trb_type = (control >> 10) & 0x3F;
        let param = ((param_hi as u64) << 32) | (param_lo as u64);
        Some((trb_type, param, status, control))
    }
}

pub(crate) struct XhciIo<'a, H: Hal> {
    pub(crate) ctrl_dma: &'a DmaBuffer,
    pub(crate) rt_base: u64,
    pub(crate) db_base: u64,
    pub(crate) hal: &'a H,
}

pub(crate) fn submit_command<H: Hal>(
    io: &XhciIo<'_, H>,
    cmd_ring: &mut TrbRing,
    event_ring: &mut EventRing,
    param: u64,
    control: u32,
) -> Option<(u8, u8)> {
    cmd_ring.enqueue(io.ctrl_dma, param, 0, control);
    io.hal.mmio_write32(io.db_base, 0);

    let mut result = None;
    let _ = spin_wait(io.hal, 250_000, || {
        while let Some((trb_type, _param, status, ctrl)) =
            event_ring.pop(io.ctrl_dma, io.rt_base, io.hal)
        {
            if trb_type == TRB_CMD_COMPLETION {
                let cc = ((status >> 24) & 0xFF) as u8;
                let slot = ((ctrl >> 24) & 0xFF) as u8;
                result = Some((cc, slot));
                return true;
            }
        }
        false
    });
    result
}

pub(crate) fn enable_and_address_slot<H: Hal>(
    io: &XhciIo<'_, H>,
    cmd_ring: &mut TrbRing,
    ep0_ring: &mut TrbRing,
    event_ring: &mut EventRing,
    port_num: usize,
    speed: u8,
    ctx_size: usize,
) -> Result<u8> {
    ep0_ring.init(io.ctrl_dma);

    let slot_id = match submit_command(io, cmd_ring, event_ring, 0, TRB_ENABLE_SLOT << 10) {
        Some((cc, slot)) if (cc == 1 || cc == 0) && slot != 0 => slot,
        _ => return Err(Error::msg("xHCI Enable Slot failed")),
    };

    let out_ctx_phys = io.ctrl_dma.phys + OFF_OUT_CTX as u64;
    unsafe {
        ptr::write_bytes(io.ctrl_dma.virt.add(OFF_OUT_CTX), 0, 0x800);
        let dcbaa_entry = io.ctrl_dma.virt.add(OFF_DCBAA + (slot_id as usize) * 8) as *mut u64;
        ptr::write_volatile(dcbaa_entry, out_ctx_phys);
    }

    let in_ctx_virt = unsafe { io.ctrl_dma.virt.add(OFF_IN_CTX) };
    let in_ctx_phys = io.ctrl_dma.phys + OFF_IN_CTX as u64;
    let ep0_max_pkt: u32 = match speed {
        4.. => 512,
        2 => 8,
        _ => 64,
    };
    unsafe {
        ptr::write_bytes(in_ctx_virt, 0, IN_CTX_MAX_BYTES);
        ptr::write_volatile(in_ctx_virt.add(4) as *mut u32, (1 << 0) | (1 << 1));

        let slot_ctx = in_ctx_virt.add(ctx_size) as *mut u32;
        ptr::write_volatile(slot_ctx, (1 << 27) | ((speed as u32) << 20));
        ptr::write_volatile(slot_ctx.add(1), (port_num as u32) << 16);

        let ep0_ctx = in_ctx_virt.add(2 * ctx_size) as *mut u32;
        let ep0_ring_phys = io.ctrl_dma.phys + OFF_EP0_RING as u64;
        ptr::write_volatile(ep0_ctx.add(1), (4 << 3) | (3 << 1) | (ep0_max_pkt << 16));
        ptr::write_volatile(ep0_ctx.add(2), (ep0_ring_phys as u32) | 1);
        ptr::write_volatile(ep0_ctx.add(3), (ep0_ring_phys >> 32) as u32);
        ptr::write_volatile(ep0_ctx.add(4), 8);
    }
    fence(Ordering::SeqCst);

    match submit_command(
        io,
        cmd_ring,
        event_ring,
        in_ctx_phys,
        (TRB_ADDRESS_DEVICE << 10) | ((slot_id as u32) << 24),
    ) {
        Some((1, _)) => Ok(slot_id),
        _ => {
            disable_slot(io, cmd_ring, event_ring, slot_id);
            Err(Error::msg("xHCI Address Device failed"))
        }
    }
}

pub(crate) fn disable_slot<H: Hal>(
    io: &XhciIo<'_, H>,
    cmd_ring: &mut TrbRing,
    event_ring: &mut EventRing,
    slot_id: u8,
) {
    if slot_id == 0 {
        return;
    }
    let _ = submit_command(
        io,
        cmd_ring,
        event_ring,
        0,
        (TRB_DISABLE_SLOT << 10) | ((slot_id as u32) << 24),
    );
    unsafe {
        let dcbaa_entry = io.ctrl_dma.virt.add(OFF_DCBAA + (slot_id as usize) * 8) as *mut u64;
        ptr::write_volatile(dcbaa_entry, 0);
    }
    fence(Ordering::Release);
}

pub(crate) fn control_in<H: Hal>(
    io: &XhciIo<'_, H>,
    ep0_ring: &mut TrbRing,
    event_ring: &mut EventRing,
    slot_id: u8,
    setup: [u8; 8],
    out: &mut [u8],
) -> bool {
    let req_len = out.len().min(CTRL_BUF_SIZE);
    let data_phys = io.ctrl_dma.phys + OFF_CTRL_BUF as u64;
    unsafe {
        ptr::write_bytes(io.ctrl_dma.virt.add(OFF_CTRL_BUF), 0, req_len);
    }

    let setup_param = u64::from_le_bytes(setup);
    ep0_ring.enqueue(
        io.ctrl_dma,
        setup_param,
        8,
        (TRB_SETUP_STAGE << 10) | (3 << 16) | (1 << 6),
    );
    ep0_ring.enqueue(
        io.ctrl_dma,
        data_phys,
        req_len as u32,
        (TRB_DATA_STAGE << 10) | (1 << 16),
    );
    ep0_ring.enqueue(io.ctrl_dma, 0, 0, (TRB_STATUS_STAGE << 10) | (1 << 5));

    io.hal
        .mmio_write32(io.db_base + (slot_id.max(1) as u64) * 4, 1);

    let mut ok = false;
    let _ = spin_wait(io.hal, 250_000, || {
        while let Some((trb_type, _param, status, _ctrl)) =
            event_ring.pop(io.ctrl_dma, io.rt_base, io.hal)
        {
            if trb_type == TRB_TRANSFER_EVENT {
                let cc = ((status >> 24) & 0xFF) as u8;
                if is_transfer_cc_ok(cc) {
                    fence(Ordering::Acquire);
                    unsafe {
                        ptr::copy_nonoverlapping(
                            io.ctrl_dma.virt.add(OFF_CTRL_BUF) as *const u8,
                            out.as_mut_ptr(),
                            req_len,
                        );
                    }
                    ok = true;
                }
                return true;
            }
        }
        false
    });
    ok
}

pub(crate) fn halt_controller<H: Hal>(op_base: u64, hal: &H) {
    let cmd = hal.mmio_read32(op_base);
    if cmd != 0xFFFF_FFFF && (cmd & 1) != 0 {
        hal.mmio_write32(op_base, cmd & !1);
        let _ = spin_wait(hal, 20_000, || (hal.mmio_read32(op_base + 0x04) & 1) != 0);
    }
    fence(Ordering::SeqCst);
}
