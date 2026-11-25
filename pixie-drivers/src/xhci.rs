//! Shared xHCI USB 3.x host controller driver for cooperative async / polling use.
//!
//! Multiple USB function drivers (e.g. `UsbEthernetDevice` and `XhciKeyboard`)
//! attach to a single [`SharedXhci`] controller handle so they share one
//! DCBAA, Command Ring, Event Ring, and scratchpad allocation while isolating
//! per-device Device Contexts and Transfer Rings cleanly.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering, fence};
use core::task::{Context, Poll};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal};

pub const RING_SIZE: usize = 64;
pub const CTRL_DMA_PAGES: usize = 4; // 16 KiB
pub const DEV_DMA_PAGES: usize = 1; // 4 KiB per device slot base

// Controller DMA layout (`ctrl_dma`: 4 pages = 16 KiB)
pub const OFF_DCBAA: usize = 0x0000; // 256 * 8 = 2048 B
pub const OFF_ERST: usize = 0x0800; // 16 B (1 segment)
pub const OFF_CMD_RING: usize = 0x1000; // 64 * 16 = 1024 B
pub const OFF_EVENT_RING: usize = 0x1400; // 64 * 16 = 1024 B
pub const OFF_IN_CTX: usize = 0x2000; // up to 33 * 64 = 2112 B
pub const IN_CTX_MAX_BYTES: usize = 33 * 64;
pub const OFF_CTRL_BUF: usize = 0x3000; // 4096 B EP0 control transfer staging
pub const CTRL_BUF_SIZE: usize = 4096;

// Per-device slot DMA layout (`slot_dma`: 1 page = 4 KiB)
pub const DEV_OFF_OUT_CTX: usize = 0x0000; // 2048 B Output Device Context (4K aligned)
pub const DEV_OFF_EP0_RING: usize = 0x0800; // 1024 B EP0 Transfer Ring
pub const DEV_OFF_EP_RING: usize = 0x0C00; // 1024 B Primary Data/Interrupt IN Ring

// Capability registers
const CAP_CAPLENGTH: u64 = 0x00;
const CAP_HCSPARAMS1: u64 = 0x04;
const CAP_HCSPARAMS2: u64 = 0x08;
const CAP_HCCPARAMS1: u64 = 0x10;
const CAP_DBOFF: u64 = 0x14;
const CAP_RTSOFF: u64 = 0x18;

// Operational registers (relative to op_base = mmio + cap_len)
const OP_USBCMD: u64 = 0x00;
const OP_USBSTS: u64 = 0x04;
const OP_PAGESIZE: u64 = 0x08;
const OP_CRCR: u64 = 0x18;
const OP_DCBAAP: u64 = 0x30;
const OP_CONFIG: u64 = 0x38;
const OP_PORTSC_BASE: u64 = 0x400;

// USBCMD bits
const CMD_RS: u32 = 1 << 0;
const CMD_HCRST: u32 = 1 << 1;

// USBSTS bits
const STS_HCH: u32 = 1 << 0;
const STS_CNR: u32 = 1 << 11;

// PORTSC bits
pub const PORTSC_CCS: u32 = 1 << 0;
pub const PORTSC_PED: u32 = 1 << 1;
pub const PORTSC_PR: u32 = 1 << 4;
pub const PORTSC_PP: u32 = 1 << 9;
pub const PORTSC_CSC: u32 = 1 << 17;
pub const PORTSC_PEC: u32 = 1 << 18;
pub const PORTSC_WRC: u32 = 1 << 19;
pub const PORTSC_OCC: u32 = 1 << 20;
pub const PORTSC_PRC: u32 = 1 << 21;
pub const PORTSC_PLC: u32 = 1 << 22;
pub const PORTSC_CEC: u32 = 1 << 23;
const PORTSC_CHANGE_BITS: u32 =
    PORTSC_CSC | PORTSC_PEC | PORTSC_WRC | PORTSC_OCC | PORTSC_PRC | PORTSC_PLC | PORTSC_CEC;

// Runtime interrupter 0 registers (relative to rt_base + 0x20)
const IR_IMAN: u64 = 0x00;
const IR_ERSTSZ: u64 = 0x08;
const IR_ERSTBA: u64 = 0x10;
const IR_ERDP: u64 = 0x18;

// TRB types
pub const TRB_NORMAL: u32 = 1;
pub const TRB_SETUP_STAGE: u32 = 2;
pub const TRB_DATA_STAGE: u32 = 3;
pub const TRB_STATUS_STAGE: u32 = 4;
pub const TRB_LINK: u32 = 6;
pub const TRB_ENABLE_SLOT: u32 = 9;
pub const TRB_DISABLE_SLOT: u32 = 10;
pub const TRB_ADDRESS_DEVICE: u32 = 11;
pub const TRB_CONFIGURE_ENDPOINT: u32 = 12;
pub const TRB_EVALUATE_CONTEXT: u32 = 13;
pub const TRB_RESET_ENDPOINT: u32 = 14;
pub const TRB_SET_TR_DEQUEUE: u32 = 16;
pub const TRB_TRANSFER_EVENT: u32 = 32;
pub const TRB_CMD_COMPLETION: u32 = 33;

// Completion codes
pub const CC_SUCCESS: u8 = 1;
pub const CC_DATA_BUFFER_ERROR: u8 = 2;
pub const CC_BABBLE_DETECTED: u8 = 3;
pub const CC_USB_TX_ERROR: u8 = 4;
pub const CC_TRB_ERROR: u8 = 5;
pub const CC_STALL_ERROR: u8 = 6;
pub const CC_SHORT_PACKET: u8 = 13;

// Endpoint types
pub const EP_TYPE_BULK_OUT: u8 = 2;
pub const EP_TYPE_INT_OUT: u8 = 3;
pub const EP_TYPE_CONTROL: u8 = 4;
pub const EP_TYPE_BULK_IN: u8 = 6;
pub const EP_TYPE_INT_IN: u8 = 7;

const MAX_PENDING_EVENTS: usize = 256;
const STUCK_CLOCK_SPIN_LIMIT: u32 = 200_000;

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
pub struct Trb {
    pub param: u64,
    pub status: u32,
    pub control: u32,
}

#[derive(Clone, Copy)]
pub struct RingState {
    pub index: u16,
    pub cycle: bool,
}

impl Default for RingState {
    fn default() -> Self {
        Self::new()
    }
}

impl RingState {
    pub const fn new() -> Self {
        Self {
            index: 0,
            cycle: true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct XhciCaps {
    pub cap_len: u8,
    pub max_slots: u8,
    pub max_ports: u8,
    pub rt_off: u32,
    pub db_off: u32,
    pub csz_64: bool,
    pub ac64: bool,
    pub max_scratchpad_bufs: usize,
    pub page_size: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct EndpointConfig {
    pub ep_dci: u8,
    pub ep_type: u8,
    pub max_packet_size: u16,
    pub max_burst: u8,
    pub interval: u8,
    pub avg_trb_len: u16,
    pub max_esit_payload_lo: u16,
    pub ring_phys: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct CmdCompletion {
    pub trb_ptr: u64,
    pub cc: u8,
    pub slot_id: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct TransferEvent {
    pub slot_id: u8,
    pub ep_dci: u8,
    pub trb_ptr: u64,
    pub status: u32,
    pub control: u32,
}

impl TransferEvent {
    #[inline]
    pub const fn cc(&self) -> u8 {
        ((self.status >> 24) & 0xFF) as u8
    }

    #[inline]
    pub const fn residual(&self) -> u32 {
        self.status & 0x00FF_FFFF
    }
}

/// Convert USB descriptor `bEndpointAddress` (`0x81`, `0x02`, ...) to xHCI DCI (`1..=31`).
#[inline]
pub const fn endpoint_dci(ep_addr: u8) -> u8 {
    let ep_num = ep_addr & 0x0F;
    let dir_in = (ep_addr & 0x80) != 0;
    if ep_num == 0 {
        1
    } else {
        ep_num * 2 + if dir_in { 1 } else { 0 }
    }
}

/// Convert xHCI DCI (`2..=31`) back to USB `bEndpointAddress`.
#[inline]
pub const fn dci_to_ep_addr(ep_dci: u8) -> u8 {
    let ep_num = ep_dci >> 1;
    let dir_bit = if (ep_dci & 1) != 0 { 0x80 } else { 0x00 };
    ep_num | dir_bit
}

/// Compute xHCI Endpoint Context `Interval` field per xHCI Table 6-12.
pub fn compute_xhci_interval(speed: u8, b_interval: u8, is_interrupt: bool) -> u8 {
    if !is_interrupt {
        // Bulk / Control endpoints use NAK max rate (0 is valid when not HS/SS NAK).
        return 0;
    }
    if speed >= 3 {
        // High-Speed / SuperSpeed: bInterval is 1..=16 representing 2^(bInterval-1)*125us
        b_interval.clamp(1, 16) - 1
    } else {
        // Low-Speed / Full-Speed Interrupt: bInterval is in 1ms frames -> floor(log2(ms * 8)) in 3..=10
        let units_125us = (b_interval.max(1) as u32) * 8;
        let exp = (31 - units_125us.leading_zeros()) as u8;
        exp.clamp(3, 10)
    }
}

/// Sleep/spin for `us` microseconds using `hal.micros()`, with a bounded stuck-clock guard
/// so static-clock test HALs never spin indefinitely.
pub(crate) fn spin_wait_us<H: Hal>(hal: &H, us: u64) {
    let start = hal.micros();
    let deadline = start.saturating_add(us.min(i64::MAX as u64) as i64);
    let mut last = start;
    let mut stuck = 0u32;
    while hal.micros() < deadline {
        let now = hal.micros();
        if now == last {
            stuck += 1;
            if stuck >= 2000 {
                break;
            }
        } else {
            last = now;
            stuck = 0;
        }
        core::hint::spin_loop();
    }
}

struct YieldOnce {
    yielded: bool,
}

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// Shared synchronization wrapper around a single xHCI host controller.
pub type SharedXhci<H> = Arc<XhciHostLock<H>>;

/// Cooperative + non-blocking lock protecting [`XhciController`].
pub struct XhciHostLock<H: Hal> {
    locked: AtomicBool,
    inner: UnsafeCell<ManuallyDrop<XhciController<H>>>,
}

// Safety: all access to `inner` is serialized by the `locked` Acquire/Release flag.
unsafe impl<H: Hal + Send> Send for XhciHostLock<H> {}
unsafe impl<H: Hal + Send> Sync for XhciHostLock<H> {}

pub struct XhciGuard<'a, H: Hal> {
    lock: &'a XhciHostLock<H>,
}

impl<H: Hal> Deref for XhciGuard<'_, H> {
    type Target = XhciController<H>;

    fn deref(&self) -> &Self::Target {
        // Safety: guard holds exclusive access via `locked`.
        unsafe { &*self.lock.inner.get() }
    }
}

impl<H: Hal> DerefMut for XhciGuard<'_, H> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Safety: guard holds exclusive access via `locked`.
        unsafe { &mut *self.lock.inner.get() }
    }
}

impl<H: Hal> Drop for XhciGuard<'_, H> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

impl<H: Hal> XhciHostLock<H> {
    pub fn new(ctrl: XhciController<H>) -> Self {
        Self {
            locked: AtomicBool::new(false),
            inner: UnsafeCell::new(ManuallyDrop::new(ctrl)),
        }
    }

    pub fn try_lock(&self) -> Option<XhciGuard<'_, H>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| XhciGuard { lock: self })
    }

    pub async fn lock(&self) -> XhciGuard<'_, H> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            YieldOnce { yielded: false }.await;
        }
    }
}

impl<H: Hal> Drop for XhciHostLock<H> {
    fn drop(&mut self) {
        let ctrl = unsafe { &mut **self.inner.get() };
        ctrl.shutdown_and_free();
    }
}

/// Initialized xHCI host controller owning global rings, DCBAA, scratchpads, and port claims.
pub struct XhciController<H: Hal> {
    hal: H,
    mmio_base: u64,
    caps: XhciCaps,
    ctrl_dma: DmaBuffer,
    scratchpad_array: Option<DmaBuffer>,
    scratchpad_pages: Vec<DmaBuffer>,
    cmd_ring: RingState,
    evt_ring: RingState,
    pending_cmds: VecDeque<CmdCompletion>,
    pending_transfers: VecDeque<TransferEvent>,
    claimed_ports: [bool; 256],
    port_addressed_once: [bool; 256],
    halted: bool,
    dma_freed: bool,
}

impl<H: Hal> XhciController<H> {
    /// Initialize an xHCI host controller at `mmio_base` and return a shareable handle.
    pub fn new_shared(mmio_base: u64, hal: H) -> Result<SharedXhci<H>> {
        Ok(Arc::new(XhciHostLock::new(Self::new(mmio_base, hal)?)))
    }

    /// Initialize an xHCI host controller at `mmio_base`.
    pub fn new(mmio_base: u64, hal: H) -> Result<Self> {
        hal.map_mmio(mmio_base, 0x10000);

        let cap_len = (hal.mmio_read32(mmio_base + CAP_CAPLENGTH) & 0xFF) as u8;
        if cap_len < 0x20 {
            return Err(Error::msg("Invalid xHCI CAPLENGTH"));
        }
        let hcsparams1 = hal.mmio_read32(mmio_base + CAP_HCSPARAMS1);
        let hcsparams2 = hal.mmio_read32(mmio_base + CAP_HCSPARAMS2);
        let hccparams1 = hal.mmio_read32(mmio_base + CAP_HCCPARAMS1);
        let db_off = hal.mmio_read32(mmio_base + CAP_DBOFF) & !0x3;
        let rt_off = hal.mmio_read32(mmio_base + CAP_RTSOFF) & !0x1F;

        let max_slots = (hcsparams1 & 0xFF) as u8;
        let max_ports = ((hcsparams1 >> 24) & 0xFF) as u8;
        if max_slots == 0 || max_ports == 0 {
            return Err(Error::msg("xHCI reports zero slots or ports"));
        }

        let max_scratchpad_bufs =
            ((((hcsparams2 >> 21) & 0x1F) << 5) | ((hcsparams2 >> 27) & 0x1F)) as usize;
        let op_base = mmio_base + cap_len as u64;
        let ps_reg = hal.mmio_read32(op_base + OP_PAGESIZE) & 0xFFFF;
        let page_size = if ps_reg != 0 {
            let lsb = ps_reg.trailing_zeros();
            1usize << (12 + lsb)
        } else {
            4096
        };

        let caps = XhciCaps {
            cap_len,
            max_slots,
            max_ports,
            rt_off,
            db_off,
            csz_64: (hccparams1 & (1 << 2)) != 0,
            ac64: (hccparams1 & 1) != 0,
            max_scratchpad_bufs,
            page_size,
        };

        Self::bios_handoff(&hal, mmio_base, hccparams1);

        // Wait for Controller Not Ready (CNR) == 0 before halting/resetting.
        Self::wait_mmio_mask(&hal, op_base + OP_USBSTS, STS_CNR, 0, 1_000_000)
            .map_err(|_| Error::msg("xHCI CNR did not clear"))?;

        // Halt controller and wait for HCH == 1 before asserting HCRST.
        let cmd = hal.mmio_read32(op_base + OP_USBCMD);
        if (cmd & CMD_RS) != 0 {
            hal.mmio_write32(op_base + OP_USBCMD, cmd & !CMD_RS);
        }
        Self::wait_mmio_mask(&hal, op_base + OP_USBSTS, STS_HCH, STS_HCH, 200_000)
            .map_err(|_| Error::msg("xHCI did not halt before reset"))?;

        // Reset controller and wait for HCRST == 0 and CNR == 0.
        hal.mmio_write32(op_base + OP_USBCMD, CMD_HCRST);
        Self::wait_mmio_mask(&hal, op_base + OP_USBCMD, CMD_HCRST, 0, 1_000_000)
            .map_err(|_| Error::msg("xHCI HCRST timeout"))?;
        Self::wait_mmio_mask(&hal, op_base + OP_USBSTS, STS_CNR, 0, 1_000_000)
            .map_err(|_| Error::msg("xHCI post-reset CNR timeout"))?;

        // Configure max slots enabled.
        hal.mmio_write32(op_base + OP_CONFIG, max_slots as u32);

        // Allocate controller DMA region and check AC64 constraint.
        let ctrl_dma = hal
            .alloc_dma(CTRL_DMA_PAGES)
            .ok_or(Error::msg("Failed to allocate xHCI controller DMA"))?;
        if !caps.ac64
            && ctrl_dma
                .phys
                .saturating_add((CTRL_DMA_PAGES * 4096) as u64 - 1)
                > u32::MAX as u64
        {
            hal.free_dma(ctrl_dma);
            return Err(Error::msg(
                "xHCI controller lacks AC64 for high DMA address",
            ));
        }
        unsafe {
            core::ptr::write_bytes(ctrl_dma.virt, 0, CTRL_DMA_PAGES * 4096);
        }

        // Allocate scratchpad buffers if required by HCSPARAMS2.
        let mut scratchpad_array: Option<DmaBuffer> = None;
        let mut scratchpad_pages: Vec<DmaBuffer> = Vec::new();
        if caps.max_scratchpad_bufs > 0 {
            let array_bytes = caps.max_scratchpad_bufs * 8;
            let array_pages = array_bytes.div_ceil(4096).max(1);
            let Some(arr) = hal.alloc_dma(array_pages) else {
                hal.free_dma(ctrl_dma);
                return Err(Error::msg("Failed to allocate xHCI scratchpad array"));
            };
            if !caps.ac64
                && arr.phys.saturating_add((array_pages * 4096) as u64 - 1) > u32::MAX as u64
            {
                hal.free_dma(arr);
                hal.free_dma(ctrl_dma);
                return Err(Error::msg("xHCI scratchpad array exceeds 32-bit DMA"));
            }
            unsafe {
                core::ptr::write_bytes(arr.virt, 0, array_pages * 4096);
            }
            let pages_per_buf = caps.page_size.div_ceil(4096).max(1);
            let arr_ptrs = arr.virt as *mut u64;
            for i in 0..caps.max_scratchpad_bufs {
                let Some(sp) = hal.alloc_dma(pages_per_buf) else {
                    for prev in scratchpad_pages.drain(..) {
                        hal.free_dma(prev);
                    }
                    hal.free_dma(arr);
                    hal.free_dma(ctrl_dma);
                    return Err(Error::msg("Failed to allocate xHCI scratchpad page"));
                };
                if !caps.ac64
                    && sp.phys.saturating_add((pages_per_buf * 4096) as u64 - 1) > u32::MAX as u64
                {
                    hal.free_dma(sp);
                    for prev in scratchpad_pages.drain(..) {
                        hal.free_dma(prev);
                    }
                    hal.free_dma(arr);
                    hal.free_dma(ctrl_dma);
                    return Err(Error::msg("xHCI scratchpad buffer exceeds 32-bit DMA"));
                }
                unsafe {
                    core::ptr::write_bytes(sp.virt, 0, pages_per_buf * 4096);
                    core::ptr::write_volatile(arr_ptrs.add(i), sp.phys);
                }
                scratchpad_pages.push(sp);
            }
            // Program DCBAA[0] = scratchpad array physical address.
            unsafe {
                core::ptr::write_volatile(ctrl_dma.virt.add(OFF_DCBAA) as *mut u64, arr.phys);
            }
            scratchpad_array = Some(arr);
        }

        // Program DCBAAP.
        hal.mmio_write64(op_base + OP_DCBAAP, ctrl_dma.phys + OFF_DCBAA as u64);

        // Initialize Command Ring Link TRB and CRCR (RCS=1).
        let cmd_ring_phys = ctrl_dma.phys + OFF_CMD_RING as u64;
        unsafe {
            let link = &mut *(ctrl_dma.virt.add(OFF_CMD_RING) as *mut Trb).add(RING_SIZE - 1);
            link.param = cmd_ring_phys;
            link.status = 0;
            link.control = (TRB_LINK << 10) | (1 << 1) | 1; // Toggle Cycle + Cycle=1
        }
        hal.mmio_write64(op_base + OP_CRCR, cmd_ring_phys | 1);

        // Initialize Event Ring and ERST (polling only: IMAN.IE stays 0, USBCMD.INTE stays 0).
        let evt_ring_phys = ctrl_dma.phys + OFF_EVENT_RING as u64;
        let erst_phys = ctrl_dma.phys + OFF_ERST as u64;
        unsafe {
            let erst_ptr = ctrl_dma.virt.add(OFF_ERST) as *mut u64;
            core::ptr::write_volatile(erst_ptr, evt_ring_phys);
            core::ptr::write_volatile(erst_ptr.add(1), RING_SIZE as u64);
        }

        let ir0 = mmio_base + rt_off as u64 + 0x20;
        hal.mmio_write32(ir0 + IR_ERSTSZ, 1);
        hal.mmio_write64(ir0 + IR_ERDP, evt_ring_phys);
        hal.mmio_write64(ir0 + IR_ERSTBA, erst_phys);
        hal.mmio_write32(ir0 + IR_IMAN, 0);

        // Start controller in pure polling mode (CMD_RS only, no CMD_INTE).
        fence(Ordering::SeqCst);
        hal.mmio_write32(op_base + OP_USBCMD, CMD_RS);
        Self::wait_mmio_mask(&hal, op_base + OP_USBSTS, STS_HCH, 0, 200_000)
            .map_err(|_| Error::msg("xHCI failed to start (HCH stayed set)"))?;

        // Allow root hub port power to stabilize before scanning ports.
        spin_wait_us(&hal, 50_000);

        Ok(Self {
            hal,
            mmio_base,
            caps,
            ctrl_dma,
            scratchpad_array,
            scratchpad_pages,
            cmd_ring: RingState::new(),
            evt_ring: RingState::new(),
            pending_cmds: VecDeque::new(),
            pending_transfers: VecDeque::new(),
            claimed_ports: [false; 256],
            port_addressed_once: [false; 256],
            halted: false,
            dma_freed: false,
        })
    }

    pub fn hal(&self) -> &H {
        &self.hal
    }

    pub fn caps(&self) -> XhciCaps {
        self.caps
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    pub fn is_port_claimed(&self, port_num: u8) -> bool {
        port_num == 0 || self.claimed_ports[port_num as usize]
    }

    pub fn claim_port(&mut self, port_num: u8) -> bool {
        if port_num == 0 || port_num > self.caps.max_ports || self.claimed_ports[port_num as usize]
        {
            return false;
        }
        self.claimed_ports[port_num as usize] = true;
        true
    }

    pub fn release_port(&mut self, port_num: u8) {
        if port_num > 0 {
            self.claimed_ports[port_num as usize] = false;
        }
    }

    /// Check if `slot_dma` respects controller 32/64-bit DMA limits.
    pub fn alloc_slot_dma(&self) -> Result<DmaBuffer> {
        let dma = self
            .hal
            .alloc_dma(DEV_DMA_PAGES)
            .ok_or(Error::msg("Failed to allocate xHCI device slot DMA"))?;
        if !self.check_dma_bounds(&dma, DEV_DMA_PAGES) {
            self.hal.free_dma(dma);
            return Err(Error::msg("xHCI device slot DMA exceeds 32-bit limit"));
        }
        unsafe {
            core::ptr::write_bytes(dma.virt, 0, DEV_DMA_PAGES * 4096);
        }
        Ok(dma)
    }

    pub fn check_dma_bounds(&self, dma: &DmaBuffer, pages: usize) -> bool {
        self.caps.ac64 || dma.phys.saturating_add((pages * 4096) as u64 - 1) <= u32::MAX as u64
    }

    fn op_base(&self) -> u64 {
        self.mmio_base + self.caps.cap_len as u64
    }

    fn portsc_addr(&self, port_num: u8) -> u64 {
        self.op_base() + OP_PORTSC_BASE + (port_num as u64 - 1) * 0x10
    }

    #[inline]
    fn doorbell_addr(&self, slot_id: u8) -> u64 {
        self.mmio_base + self.caps.db_off as u64 + (slot_id as u64) * 4
    }

    pub fn ring_doorbell(&self, slot_id: u8, target: u32) {
        fence(Ordering::SeqCst);
        self.hal.mmio_write32(self.doorbell_addr(slot_id), target);
    }

    pub fn read_portsc(&self, port_num: u8) -> u32 {
        if port_num == 0 || port_num > self.caps.max_ports {
            return 0;
        }
        self.hal.mmio_read32(self.portsc_addr(port_num))
    }

    /// Ensure `port_num` (1-based) is connected, enabled, and freshly at USB Address 0.
    /// Returns the non-zero USB speed ID (`1=Full, 2=Low, 3=High, 4+=SuperSpeed`).
    pub fn ensure_port_ready(&mut self, port_num: u8) -> Result<u8> {
        if port_num == 0 || port_num > self.caps.max_ports {
            return Err(Error::msg("Invalid xHCI port number"));
        }
        let reg = self.portsc_addr(port_num);
        let mut portsc = self.hal.mmio_read32(reg);
        if (portsc & PORTSC_CCS) == 0 {
            return Err(Error::msg("xHCI port not connected"));
        }

        // If the port is disabled OR previously had Address Device issued on it (and was disabled
        // without disconnecting), issue a Port Reset so the device resets back to USB address 0.
        if (portsc & PORTSC_PED) == 0 || self.port_addressed_once[port_num as usize] {
            // Preserve only PP (1<<9); never write 1 to RW1CS PED or RW1C change bits.
            let pp = if (portsc & PORTSC_PP) != 0 {
                PORTSC_PP
            } else {
                0
            };
            self.hal.mmio_write32(reg, pp | PORTSC_PR);

            let start = self.hal.micros();
            let mut last = start;
            let mut stuck = 0u32;
            loop {
                portsc = self.hal.mmio_read32(reg);
                if (portsc & PORTSC_PRC) != 0
                    || ((portsc & PORTSC_PR) == 0 && (portsc & PORTSC_PED) != 0)
                {
                    break;
                }
                let now = self.hal.micros();
                if now.saturating_sub(start) > 250_000 {
                    return Err(Error::msg("xHCI port reset timeout"));
                }
                if now == last {
                    stuck += 1;
                    if stuck >= STUCK_CLOCK_SPIN_LIMIT {
                        return Err(Error::msg("xHCI port reset timeout"));
                    }
                } else {
                    last = now;
                    stuck = 0;
                }
                core::hint::spin_loop();
            }

            if (portsc & PORTSC_CHANGE_BITS) != 0 {
                let pp = if (portsc & PORTSC_PP) != 0 {
                    PORTSC_PP
                } else {
                    0
                };
                self.hal.mmio_write32(reg, pp | PORTSC_CHANGE_BITS);
            }
            self.port_addressed_once[port_num as usize] = false;
            spin_wait_us(&self.hal, 20_000);
            portsc = self.hal.mmio_read32(reg);
        }

        if (portsc & PORTSC_CCS) == 0 || (portsc & PORTSC_PED) == 0 {
            return Err(Error::msg("xHCI port failed to enable after reset"));
        }
        let speed = ((portsc >> 10) & 0xF) as u8;
        if speed == 0 {
            return Err(Error::msg("xHCI port speed is 0"));
        }
        Ok(speed)
    }

    pub fn enable_slot(&mut self) -> Result<u8> {
        let cmd_phys = self.enqueue_cmd(0, 0, TRB_ENABLE_SLOT << 10);
        self.ring_doorbell(0, 0);
        let ev = self.wait_for_cmd(cmd_phys, 500_000)?;
        if ev.cc != CC_SUCCESS || ev.slot_id == 0 {
            return Err(Error::msg("xHCI Enable Slot failed"));
        }
        Ok(ev.slot_id)
    }

    pub fn disable_slot(&mut self, slot_id: u8) -> Result<()> {
        if slot_id == 0 {
            return Ok(());
        }
        let cmd_phys = self.enqueue_cmd(0, 0, (TRB_DISABLE_SLOT << 10) | ((slot_id as u32) << 24));
        self.ring_doorbell(0, 0);
        let res = self.wait_for_cmd(cmd_phys, 200_000);
        unsafe {
            let dcbaa = self.ctrl_dma.virt.add(OFF_DCBAA) as *mut u64;
            core::ptr::write_volatile(dcbaa.add(slot_id as usize), 0);
        }
        self.pending_transfers.retain(|ev| ev.slot_id != slot_id);
        match res {
            Ok(ev) if ev.cc == CC_SUCCESS => Ok(()),
            Ok(_) => Err(Error::msg("xHCI Disable Slot rejected")),
            Err(e) => Err(e),
        }
    }

    pub fn address_device(
        &mut self,
        slot_id: u8,
        port_num: u8,
        speed: u8,
        slot_dma: &mut DmaBuffer,
        ep0_ring: &mut RingState,
    ) -> Result<()> {
        *ep0_ring = RingState::new();
        let ep0_phys = slot_dma.phys + DEV_OFF_EP0_RING as u64;
        unsafe {
            core::ptr::write_bytes(slot_dma.virt, 0, DEV_DMA_PAGES * 4096);
            let link = &mut *(slot_dma.virt.add(DEV_OFF_EP0_RING) as *mut Trb).add(RING_SIZE - 1);
            link.param = ep0_phys;
            link.status = 0;
            link.control = (TRB_LINK << 10) | (1 << 1) | 1;

            let dcbaa = self.ctrl_dma.virt.add(OFF_DCBAA) as *mut u64;
            core::ptr::write_volatile(
                dcbaa.add(slot_id as usize),
                slot_dma.phys + DEV_OFF_OUT_CTX as u64,
            );

            core::ptr::write_bytes(self.ctrl_dma.virt.add(OFF_IN_CTX), 0, IN_CTX_MAX_BYTES);
            let stride = self.ctx_stride();
            let in_ctx = self.ctrl_dma.virt.add(OFF_IN_CTX);

            // Input Control Context: Add Slot (bit 0) + EP0 (bit 1)
            core::ptr::write_volatile(in_ctx.add(4) as *mut u32, 0b11);

            // Slot Context (index 1)
            let slot_ctx = in_ctx.add(stride) as *mut u32;
            core::ptr::write_volatile(slot_ctx, ((speed as u32) << 20) | (1 << 27));
            core::ptr::write_volatile(slot_ctx.add(1), (port_num as u32) << 16);

            // Initial EP0 Max Packet Size per USB 2/3 & xHCI spec
            let init_mps: u32 = match speed {
                1 | 2 => 8,
                3 => 64,
                _ => 512,
            };

            // EP0 Context (index 2)
            let ep0_ctx = in_ctx.add(2 * stride) as *mut u32;
            core::ptr::write_volatile(
                ep0_ctx.add(1),
                (3 << 1) | ((EP_TYPE_CONTROL as u32) << 3) | (init_mps << 16),
            );
            core::ptr::write_volatile(ep0_ctx.add(2), ((ep0_phys | 1) & 0xFFFF_FFFF) as u32);
            core::ptr::write_volatile(ep0_ctx.add(3), ((ep0_phys | 1) >> 32) as u32);
            core::ptr::write_volatile(ep0_ctx.add(4), 8); // Avg TRB Len = 8
        }

        let in_ctx_phys = self.ctrl_dma.phys + OFF_IN_CTX as u64;
        let cmd_phys = self.enqueue_cmd(
            in_ctx_phys,
            0,
            (TRB_ADDRESS_DEVICE << 10) | ((slot_id as u32) << 24),
        );
        self.ring_doorbell(0, 0);
        let ev = self.wait_for_cmd(cmd_phys, 500_000)?;
        self.port_addressed_once[port_num as usize] = true;
        if ev.cc != CC_SUCCESS {
            return Err(Error::msg("xHCI Address Device failed"));
        }
        Ok(())
    }

    pub fn evaluate_ep0_mps(
        &mut self,
        slot_id: u8,
        new_mps: u16,
        slot_dma: &DmaBuffer,
    ) -> Result<()> {
        let stride = self.ctx_stride();
        unsafe {
            core::ptr::write_bytes(self.ctrl_dma.virt.add(OFF_IN_CTX), 0, IN_CTX_MAX_BYTES);
            let in_ctx = self.ctrl_dma.virt.add(OFF_IN_CTX);
            let out_ctx = slot_dma.virt.add(DEV_OFF_OUT_CTX);
            core::ptr::write_volatile(in_ctx.add(4) as *mut u32, 0b10); // Evaluate EP0
            core::ptr::copy_nonoverlapping(out_ctx, in_ctx.add(stride), stride);
            core::ptr::copy_nonoverlapping(out_ctx.add(stride), in_ctx.add(2 * stride), stride);
            let ep0_ctx = in_ctx.add(2 * stride) as *mut u32;
            let dw1 = core::ptr::read_volatile(ep0_ctx.add(1));
            let updated = (dw1 & 0x0000_FFFF) | ((new_mps as u32) << 16);
            core::ptr::write_volatile(ep0_ctx.add(1), updated);
        }

        let in_ctx_phys = self.ctrl_dma.phys + OFF_IN_CTX as u64;
        let cmd_phys = self.enqueue_cmd(
            in_ctx_phys,
            0,
            (TRB_EVALUATE_CONTEXT << 10) | ((slot_id as u32) << 24),
        );
        self.ring_doorbell(0, 0);
        let ev = self.wait_for_cmd(cmd_phys, 500_000)?;
        if ev.cc != CC_SUCCESS {
            return Err(Error::msg("xHCI Evaluate Context failed"));
        }
        Ok(())
    }

    pub fn configure_endpoints(
        &mut self,
        slot_id: u8,
        slot_dma: &DmaBuffer,
        eps: &[EndpointConfig],
    ) -> Result<()> {
        if eps.is_empty() {
            return Err(Error::msg("No endpoints to configure"));
        }
        let stride = self.ctx_stride();
        let max_dci = eps.iter().map(|e| e.ep_dci).max().unwrap_or(1);

        unsafe {
            core::ptr::write_bytes(self.ctrl_dma.virt.add(OFF_IN_CTX), 0, IN_CTX_MAX_BYTES);
            let in_ctx = self.ctrl_dma.virt.add(OFF_IN_CTX);
            let out_ctx = slot_dma.virt.add(DEV_OFF_OUT_CTX);

            // Copy Output Slot Context into Input Slot Context so hub/route/speed/port fields stay valid.
            core::ptr::copy_nonoverlapping(out_ctx, in_ctx.add(stride), stride);

            let slot_dw0_ptr = in_ctx.add(stride) as *mut u32;
            let slot_dw0 = core::ptr::read_volatile(slot_dw0_ptr);
            let prev_entries = ((slot_dw0 >> 27) & 0x1F) as u8;
            let entries = prev_entries.max(max_dci) as u32;
            core::ptr::write_volatile(slot_dw0_ptr, (slot_dw0 & !(0x1F << 27)) | (entries << 27));

            let mut add_flags: u32 = 1; // Slot Context
            for ep in eps {
                if ep.ep_dci < 2 || ep.ep_dci > 31 {
                    return Err(Error::msg("Invalid endpoint DCI"));
                }
                add_flags |= 1u32 << ep.ep_dci;
                let ep_ctx = in_ctx.add((1 + ep.ep_dci as usize) * stride) as *mut u32;
                core::ptr::write_volatile(ep_ctx, (ep.interval as u32) << 16);
                core::ptr::write_volatile(
                    ep_ctx.add(1),
                    (3 << 1)
                        | ((ep.ep_type as u32) << 3)
                        | ((ep.max_burst as u32) << 8)
                        | ((ep.max_packet_size as u32) << 16),
                );
                let deq = ep.ring_phys | 1;
                core::ptr::write_volatile(ep_ctx.add(2), (deq & 0xFFFF_FFFF) as u32);
                core::ptr::write_volatile(ep_ctx.add(3), (deq >> 32) as u32);
                core::ptr::write_volatile(
                    ep_ctx.add(4),
                    (ep.avg_trb_len as u32) | ((ep.max_esit_payload_lo as u32) << 16),
                );
            }
            core::ptr::write_volatile(in_ctx.add(4) as *mut u32, add_flags);
        }

        let in_ctx_phys = self.ctrl_dma.phys + OFF_IN_CTX as u64;
        let cmd_phys = self.enqueue_cmd(
            in_ctx_phys,
            0,
            (TRB_CONFIGURE_ENDPOINT << 10) | ((slot_id as u32) << 24),
        );
        self.ring_doorbell(0, 0);
        let ev = self.wait_for_cmd(cmd_phys, 500_000)?;
        if ev.cc != CC_SUCCESS {
            return Err(Error::msg("xHCI Configure Endpoint failed"));
        }
        Ok(())
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub(crate) fn init_transfer_ring(
        &self,
        ring_virt: *mut u8,
        ring_phys: u64,
        ring_state: &mut RingState,
    ) {
        *ring_state = RingState::new();
        unsafe {
            core::ptr::write_bytes(ring_virt, 0, RING_SIZE * core::mem::size_of::<Trb>());
            let link = &mut *(ring_virt as *mut Trb).add(RING_SIZE - 1);
            link.param = ring_phys;
            link.status = 0;
            link.control = (TRB_LINK << 10) | (1 << 1) | 1;
        }
    }

    /// Enqueue a Normal TRB on an arbitrary transfer ring, returning its TRB physical address.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue_normal(
        &self,
        ring_virt: *mut u8,
        ring_phys: u64,
        ring_state: &mut RingState,
        buf_phys: u64,
        len: u32,
        ioc: bool,
        isp: bool,
    ) -> u64 {
        let mut ctrl = TRB_NORMAL << 10;
        if ioc {
            ctrl |= 1 << 5;
        }
        if isp {
            ctrl |= 1 << 2;
        }
        Self::enqueue_trb_raw(ring_virt, ring_phys, ring_state, buf_phys, len, ctrl)
    }

    /// Perform a Control IN transfer (`setup` + optional Data Stage + Status Stage).
    /// Returns the actual number of bytes received into `out`.
    pub fn control_in(
        &mut self,
        slot_id: u8,
        slot_dma: &mut DmaBuffer,
        ep0_ring: &mut RingState,
        setup: [u8; 8],
        out: &mut [u8],
    ) -> Result<usize> {
        if out.len() > CTRL_BUF_SIZE {
            return Err(Error::msg("Control IN buffer exceeds CTRL_BUF_SIZE"));
        }
        self.control_in_inner(slot_id, slot_dma, ep0_ring, setup, out, true)
    }

    fn control_in_inner(
        &mut self,
        slot_id: u8,
        slot_dma: &mut DmaBuffer,
        ep0_ring: &mut RingState,
        setup: [u8; 8],
        out: &mut [u8],
        allow_recover: bool,
    ) -> Result<usize> {
        let ep0_virt = unsafe { slot_dma.virt.add(DEV_OFF_EP0_RING) };
        let ep0_phys = slot_dma.phys + DEV_OFF_EP0_RING as u64;
        let ctrl_buf_phys = self.ctrl_dma.phys + OFF_CTRL_BUF as u64;
        let ctrl_buf_virt = unsafe { self.ctrl_dma.virt.add(OFF_CTRL_BUF) };
        if !out.is_empty() {
            unsafe {
                core::ptr::write_bytes(ctrl_buf_virt, 0, out.len());
            }
        }

        let setup_u64 = u64::from_le_bytes(setup);
        let trt = if out.is_empty() { 0u32 } else { 3u32 };
        Self::enqueue_trb_raw(
            ep0_virt,
            ep0_phys,
            ep0_ring,
            setup_u64,
            8,
            (TRB_SETUP_STAGE << 10) | (1 << 6) | (trt << 16), // IDT=1, TRT
        );

        let mut data_trb_phys = 0u64;
        if !out.is_empty() {
            data_trb_phys = Self::enqueue_trb_raw(
                ep0_virt,
                ep0_phys,
                ep0_ring,
                ctrl_buf_phys,
                out.len() as u32,
                (TRB_DATA_STAGE << 10) | (1 << 16), // DIR=1 (IN)
            );
        }

        let status_trb_phys = Self::enqueue_trb_raw(
            ep0_virt,
            ep0_phys,
            ep0_ring,
            0,
            0,
            (TRB_STATUS_STAGE << 10) | (1 << 5), // IOC=1, DIR=0 (OUT)
        );
        self.ring_doorbell(slot_id, 1);

        let ev = match self.wait_for_transfer_event(slot_id, 1, 500_000) {
            Ok(ev) => ev,
            Err(e) => {
                if allow_recover {
                    let _ = self.recover_ep0(slot_id, ep0_ring, ep0_phys);
                }
                return Err(e);
            }
        };

        let cc = ev.cc();
        let mut received = out.len();
        if !out.is_empty() && ev.trb_ptr == data_trb_phys {
            if cc != CC_SUCCESS && cc != CC_SHORT_PACKET {
                if allow_recover {
                    let _ = self.recover_ep0(slot_id, ep0_ring, ep0_phys);
                }
                return Err(Error::msg("xHCI Control IN Data Stage failed"));
            }
            received = out.len().saturating_sub(ev.residual() as usize);
            // Drain the subsequent Status Stage completion TRB so it never leaks into the queue.
            let st_ev = self.wait_for_transfer_event(slot_id, 1, 500_000)?;
            if st_ev.cc() != CC_SUCCESS && st_ev.cc() != CC_SHORT_PACKET {
                return Err(Error::msg("xHCI Control IN Status Stage failed"));
            }
        } else if cc != CC_SUCCESS && cc != CC_SHORT_PACKET {
            if allow_recover {
                let _ = self.recover_ep0(slot_id, ep0_ring, ep0_phys);
            }
            return Err(Error::msg("xHCI Control IN failed"));
        } else if ev.trb_ptr == status_trb_phys && ev.residual() != 0 && !out.is_empty() {
            // Some minimal simulators put residual on Status TRB
            received = out.len().saturating_sub(ev.residual() as usize);
        }

        fence(Ordering::Acquire);
        if received > 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(ctrl_buf_virt, out.as_mut_ptr(), received);
            }
        }
        Ok(received)
    }

    /// Perform a Control OUT transfer (`setup` + optional Data Stage + Status Stage).
    pub fn control_out(
        &mut self,
        slot_id: u8,
        slot_dma: &mut DmaBuffer,
        ep0_ring: &mut RingState,
        setup: [u8; 8],
        data: &[u8],
    ) -> Result<()> {
        if data.len() > CTRL_BUF_SIZE {
            return Err(Error::msg("Control OUT payload exceeds CTRL_BUF_SIZE"));
        }
        self.control_out_inner(slot_id, slot_dma, ep0_ring, setup, data, true)
    }

    fn control_out_inner(
        &mut self,
        slot_id: u8,
        slot_dma: &mut DmaBuffer,
        ep0_ring: &mut RingState,
        setup: [u8; 8],
        data: &[u8],
        allow_recover: bool,
    ) -> Result<()> {
        let ep0_virt = unsafe { slot_dma.virt.add(DEV_OFF_EP0_RING) };
        let ep0_phys = slot_dma.phys + DEV_OFF_EP0_RING as u64;
        let ctrl_buf_phys = self.ctrl_dma.phys + OFF_CTRL_BUF as u64;
        let ctrl_buf_virt = unsafe { self.ctrl_dma.virt.add(OFF_CTRL_BUF) };
        if !data.is_empty() {
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr(), ctrl_buf_virt, data.len());
            }
        }

        let setup_u64 = u64::from_le_bytes(setup);
        let trt = if data.is_empty() { 0u32 } else { 2u32 };
        Self::enqueue_trb_raw(
            ep0_virt,
            ep0_phys,
            ep0_ring,
            setup_u64,
            8,
            (TRB_SETUP_STAGE << 10) | (1 << 6) | (trt << 16), // IDT=1, TRT
        );

        let mut data_trb_phys = 0u64;
        if !data.is_empty() {
            data_trb_phys = Self::enqueue_trb_raw(
                ep0_virt,
                ep0_phys,
                ep0_ring,
                ctrl_buf_phys,
                data.len() as u32,
                TRB_DATA_STAGE << 10, // DIR=0 (OUT)
            );
        }

        // No-data status stage uses DIR=1 (IN); OUT data status stage uses DIR=1 (IN)
        Self::enqueue_trb_raw(
            ep0_virt,
            ep0_phys,
            ep0_ring,
            0,
            0,
            (TRB_STATUS_STAGE << 10) | (1 << 5) | (1 << 16), // IOC=1, DIR=1 (IN)
        );
        self.ring_doorbell(slot_id, 1);

        let ev = match self.wait_for_transfer_event(slot_id, 1, 500_000) {
            Ok(ev) => ev,
            Err(e) => {
                if allow_recover {
                    let _ = self.recover_ep0(slot_id, ep0_ring, ep0_phys);
                }
                return Err(e);
            }
        };

        let cc = ev.cc();
        if !data.is_empty() && ev.trb_ptr == data_trb_phys {
            if cc != CC_SUCCESS {
                if allow_recover {
                    let _ = self.recover_ep0(slot_id, ep0_ring, ep0_phys);
                }
                return Err(Error::msg("xHCI Control OUT Data Stage failed"));
            }
            let st_ev = self.wait_for_transfer_event(slot_id, 1, 500_000)?;
            if st_ev.cc() != CC_SUCCESS && st_ev.cc() != CC_SHORT_PACKET {
                return Err(Error::msg("xHCI Control OUT Status Stage failed"));
            }
        } else if cc != CC_SUCCESS && cc != CC_SHORT_PACKET {
            if allow_recover {
                let _ = self.recover_ep0(slot_id, ep0_ring, ep0_phys);
            }
            return Err(Error::msg("xHCI Control OUT failed"));
        }
        Ok(())
    }

    fn recover_ep0(&mut self, slot_id: u8, ep0_ring: &mut RingState, ep0_phys: u64) -> Result<()> {
        let cmd_reset = self.enqueue_cmd(
            0,
            0,
            (TRB_RESET_ENDPOINT << 10) | ((slot_id as u32) << 24) | (1u32 << 16),
        );
        self.ring_doorbell(0, 0);
        let _ = self.wait_for_cmd(cmd_reset, 200_000);

        let deq_ptr = (ep0_phys + (ep0_ring.index as u64) * 16) | (ep0_ring.cycle as u64);
        let cmd_deq = self.enqueue_cmd(
            deq_ptr,
            0,
            (TRB_SET_TR_DEQUEUE << 10) | ((slot_id as u32) << 24) | (1u32 << 16),
        );
        self.ring_doorbell(0, 0);
        let _ = self.wait_for_cmd(cmd_deq, 200_000);
        self.pending_transfers
            .retain(|ev| !(ev.slot_id == slot_id && ev.ep_dci == 1));
        Ok(())
    }

    /// Recover a halted Bulk/Interrupt/Control endpoint and reset its TR Dequeue Pointer.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn recover_halted_endpoint(
        &mut self,
        slot_id: u8,
        ep_dci: u8,
        ring_state: &mut RingState,
        ring_phys: u64,
        was_stalled: bool,
        slot_dma: &mut DmaBuffer,
        ep0_ring: &mut RingState,
    ) -> Result<()> {
        self.recover_halted_endpoint_inner(
            slot_id,
            ep_dci,
            ring_state,
            ring_phys,
            was_stalled,
            slot_dma,
            ep0_ring,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn recover_halted_endpoint_inner(
        &mut self,
        slot_id: u8,
        ep_dci: u8,
        ring_state: &mut RingState,
        ring_phys: u64,
        was_stalled: bool,
        slot_dma: &mut DmaBuffer,
        ep0_ring: &mut RingState,
    ) -> Result<()> {
        // 1. Reset Endpoint (TSP=0) -> transitions endpoint from Halted to Stopped
        let cmd_reset = self.enqueue_cmd(
            0,
            0,
            (TRB_RESET_ENDPOINT << 10) | ((slot_id as u32) << 24) | ((ep_dci as u32) << 16),
        );
        self.ring_doorbell(0, 0);
        let _ = self.wait_for_cmd(cmd_reset, 200_000);

        // 2. If device STALLed a non-EP0 endpoint, clear HALT feature on the USB device
        if was_stalled && ep_dci > 1 {
            let ep_addr = dci_to_ep_addr(ep_dci);
            let setup = [0x02, 0x01, 0x00, 0x00, ep_addr, 0x00, 0x00, 0x00];
            let _ = self.control_out_inner(slot_id, slot_dma, ep0_ring, setup, &[], false);
        }

        // 3. Synchronize hardware dequeue pointer with software ring state
        let deq_ptr = (ring_phys + (ring_state.index as u64) * 16) | (ring_state.cycle as u64);
        let cmd_deq = self.enqueue_cmd(
            deq_ptr,
            0,
            (TRB_SET_TR_DEQUEUE << 10) | ((slot_id as u32) << 24) | ((ep_dci as u32) << 16),
        );
        self.ring_doorbell(0, 0);
        let _ = self.wait_for_cmd(cmd_deq, 200_000);
        self.pending_transfers
            .retain(|ev| !(ev.slot_id == slot_id && ev.ep_dci == ep_dci));
        Ok(())
    }

    /// Pop the next queued Transfer Event matching `(slot_id, ep_dci)`, draining HW events first.
    pub fn pop_transfer_event(&mut self, slot_id: u8, ep_dci: u8) -> Option<TransferEvent> {
        self.drain_hw_events();
        let pos = self
            .pending_transfers
            .iter()
            .position(|ev| ev.slot_id == slot_id && ev.ep_dci == ep_dci)?;
        self.pending_transfers.remove(pos)
    }

    pub fn wait_for_transfer_event(
        &mut self,
        slot_id: u8,
        ep_dci: u8,
        timeout_us: u64,
    ) -> Result<TransferEvent> {
        let start = self.hal.micros();
        let mut last = start;
        let mut stuck = 0u32;
        loop {
            if let Some(ev) = self.pop_transfer_event(slot_id, ep_dci) {
                return Ok(ev);
            }
            let now = self.hal.micros();
            if now.saturating_sub(start) > timeout_us as i64 {
                return Err(Error::msg("xHCI transfer event timeout"));
            }
            if now == last {
                stuck += 1;
                if stuck >= STUCK_CLOCK_SPIN_LIMIT {
                    return Err(Error::msg("xHCI transfer event timeout"));
                }
            } else {
                last = now;
                stuck = 0;
            }
            core::hint::spin_loop();
        }
    }

    fn enqueue_cmd(&mut self, param: u64, status: u32, control_base: u32) -> u64 {
        let virt = unsafe { self.ctrl_dma.virt.add(OFF_CMD_RING) };
        let phys = self.ctrl_dma.phys + OFF_CMD_RING as u64;
        Self::enqueue_trb_raw(virt, phys, &mut self.cmd_ring, param, status, control_base)
    }

    fn enqueue_trb_raw(
        ring_virt: *mut u8,
        ring_phys: u64,
        ring_state: &mut RingState,
        param: u64,
        status: u32,
        control_base: u32,
    ) -> u64 {
        if ring_state.index as usize == RING_SIZE - 1 {
            unsafe {
                let link = &mut *(ring_virt as *mut Trb).add(RING_SIZE - 1);
                link.param = ring_phys;
                link.status = 0;
                link.control = (TRB_LINK << 10) | (1 << 1) | (ring_state.cycle as u32);
            }
            ring_state.index = 0;
            ring_state.cycle = !ring_state.cycle;
        }
        let idx = ring_state.index as usize;
        let trb_phys = ring_phys + (idx as u64) * 16;
        unsafe {
            let slot = &mut *(ring_virt as *mut Trb).add(idx);
            slot.param = param;
            slot.status = status;
            fence(Ordering::Release);
            slot.control = (control_base & !1) | (ring_state.cycle as u32);
        }
        ring_state.index += 1;
        if ring_state.index as usize == RING_SIZE - 1 {
            unsafe {
                let link = &mut *(ring_virt as *mut Trb).add(RING_SIZE - 1);
                link.param = ring_phys;
                link.status = 0;
                fence(Ordering::Release);
                link.control = (TRB_LINK << 10) | (1 << 1) | (ring_state.cycle as u32);
            }
            ring_state.index = 0;
            ring_state.cycle = !ring_state.cycle;
        }
        trb_phys
    }

    pub fn drain_hw_events(&mut self) {
        let evt_virt = unsafe { self.ctrl_dma.virt.add(OFF_EVENT_RING) as *const Trb };
        let evt_phys = self.ctrl_dma.phys + OFF_EVENT_RING as u64;
        let ir0 = self.mmio_base + self.caps.rt_off as u64 + 0x20;
        let mut drained = false;

        for _ in 0..RING_SIZE {
            fence(Ordering::Acquire);
            let idx = self.evt_ring.index as usize;
            let trb = unsafe { core::ptr::read_volatile(evt_virt.add(idx)) };
            if (trb.control & 1) != (self.evt_ring.cycle as u32) {
                break;
            }
            drained = true;
            let trb_type = (trb.control >> 10) & 0x3F;
            match trb_type {
                TRB_CMD_COMPLETION => {
                    if self.pending_cmds.len() >= MAX_PENDING_EVENTS {
                        self.pending_cmds.pop_front();
                    }
                    self.pending_cmds.push_back(CmdCompletion {
                        trb_ptr: trb.param,
                        cc: ((trb.status >> 24) & 0xFF) as u8,
                        slot_id: ((trb.control >> 24) & 0xFF) as u8,
                    });
                }
                TRB_TRANSFER_EVENT => {
                    if self.pending_transfers.len() >= MAX_PENDING_EVENTS {
                        self.pending_transfers.pop_front();
                    }
                    self.pending_transfers.push_back(TransferEvent {
                        slot_id: ((trb.control >> 24) & 0xFF) as u8,
                        ep_dci: ((trb.control >> 16) & 0x1F) as u8,
                        trb_ptr: trb.param,
                        status: trb.status,
                        control: trb.control,
                    });
                }
                _ => {}
            }

            self.evt_ring.index += 1;
            if self.evt_ring.index as usize >= RING_SIZE {
                self.evt_ring.index = 0;
                self.evt_ring.cycle = !self.evt_ring.cycle;
            }
        }

        if drained {
            let new_erdp = evt_phys + (self.evt_ring.index as u64) * 16;
            self.hal.mmio_write64(ir0 + IR_ERDP, new_erdp | (1 << 3));
        }
    }

    fn wait_for_cmd(&mut self, cmd_phys: u64, timeout_us: u64) -> Result<CmdCompletion> {
        let start = self.hal.micros();
        let mut last = start;
        let mut stuck = 0u32;
        loop {
            self.drain_hw_events();
            if let Some(pos) = self.pending_cmds.iter().position(|c| c.trb_ptr == cmd_phys) {
                return Ok(self.pending_cmds.remove(pos).unwrap());
            }
            // Also accept simulators that leave cmd trb_ptr == 0 if no exact match
            if let Some(pos) = self.pending_cmds.iter().position(|c| c.trb_ptr == 0) {
                return Ok(self.pending_cmds.remove(pos).unwrap());
            }
            let now = self.hal.micros();
            if now.saturating_sub(start) > timeout_us as i64 {
                return Err(Error::msg("xHCI command completion timeout"));
            }
            if now == last {
                stuck += 1;
                if stuck >= STUCK_CLOCK_SPIN_LIMIT {
                    return Err(Error::msg("xHCI command completion timeout"));
                }
            } else {
                last = now;
                stuck = 0;
            }
            core::hint::spin_loop();
        }
    }

    #[inline]
    fn ctx_stride(&self) -> usize {
        if self.caps.csz_64 { 64 } else { 32 }
    }

    fn bios_handoff(hal: &H, mmio_base: u64, hccparams1: u32) {
        let mut xecp_off = ((hccparams1 >> 16) & 0xFFFF) as u64 * 4;
        for _ in 0..64 {
            if xecp_off == 0 {
                break;
            }
            let reg_addr = mmio_base + xecp_off;
            hal.map_mmio(reg_addr & !0xFFF, 4096);
            let cap = hal.mmio_read32(reg_addr);
            if cap == 0 || cap == u32::MAX {
                break;
            }
            if (cap & 0xFF) == 1 && (cap & (1 << 16)) != 0 {
                hal.mmio_write32(reg_addr, cap | (1 << 24));
                let _ = Self::wait_mmio_mask(hal, reg_addr, 1 << 16, 0, 1_000_000);
            }
            let next = ((cap >> 8) & 0xFF) as u64 * 4;
            if next == 0 {
                break;
            }
            xecp_off += next;
        }
    }

    fn wait_mmio_mask(hal: &H, addr: u64, mask: u32, want: u32, timeout_us: u64) -> Result<()> {
        let start = hal.micros();
        let mut last = start;
        let mut stuck = 0u32;
        loop {
            if (hal.mmio_read32(addr) & mask) == want {
                return Ok(());
            }
            let now = hal.micros();
            if now.saturating_sub(start) > timeout_us as i64 {
                return Err(Error::msg("xHCI register wait timeout"));
            }
            if now == last {
                stuck += 1;
                if stuck >= STUCK_CLOCK_SPIN_LIMIT {
                    return Err(Error::msg("xHCI register wait timeout"));
                }
            } else {
                last = now;
                stuck = 0;
            }
            core::hint::spin_loop();
        }
    }

    /// Halt the xHCI controller cleanly and free controller DMA only once HCH == 1.
    fn shutdown_and_free(&mut self) {
        if self.dma_freed {
            return;
        }
        let op = self.op_base();
        let cmd = self.hal.mmio_read32(op + OP_USBCMD);
        if (cmd & CMD_RS) != 0 {
            self.hal.mmio_write32(op + OP_USBCMD, cmd & !CMD_RS);
        }
        let halted_ok =
            Self::wait_mmio_mask(&self.hal, op + OP_USBSTS, STS_HCH, STS_HCH, 100_000).is_ok();
        self.halted = halted_ok;
        self.dma_freed = true;

        if halted_ok {
            self.hal.mmio_write64(op + OP_DCBAAP, 0);
            self.hal.free_dma(self.ctrl_dma);
            if let Some(arr) = self.scratchpad_array.take() {
                self.hal.free_dma(arr);
            }
            for sp in self.scratchpad_pages.drain(..) {
                self.hal.free_dma(sp);
            }
        } else {
            // Controller refused to halt: leak scratchpad/ctrl DMA instead of causing DMA UAF.
            self.scratchpad_array.take();
            self.scratchpad_pages.clear();
        }
    }
}
