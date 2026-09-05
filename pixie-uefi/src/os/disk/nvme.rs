//! A deliberately small NVMe driver used while boot services are available.
//!
//! The firmware NVMe driver is disconnected while an [`NvmeDisk`] exists.
//! This is necessary because both it and this driver would otherwise program
//! the same controller queues. Dropping `NvmeDisk` resets the controller and
//! reconnects the firmware driver.
//!
//! MMIO and DMA-buffer allocation go through `EFI_PCI_IO_PROTOCOL`, wrapped
//! safely by [`uefi::proto::pci::io::PciIo`]. Boot-time UEFI has no active
//! DMA remapping for the app's own bus-master transfers, so a DMA buffer's
//! host physical address doubles as the device (bus) address the controller
//! uses for PRP entries; there is no separate mapping step.

use alloc::vec::Vec;
use core::ptr::{self, NonNull};
use core::sync::atomic::{Ordering, fence};
use core::time::Duration;

use uefi::Handle;
use uefi::boot::{
    AllocateType, MemoryType, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol,
};
use uefi::proto::nvme::pass_thru::NvmePassThru;
use uefi::proto::pci::io::{PciIo, PciIoProtocolAttributes};
use uefi_raw::protocol::nvme::NvmExpressPassThruAttributes;

use crate::os::error::{Error, Result};
use crate::os::executor::Executor;
use crate::os::timer::Timer;

const PAGE_SIZE: usize = 4096;
const QUEUE_ENTRIES: usize = 64;
const NVME_REG_CAP: u64 = 0x0000;
const NVME_REG_CC: u64 = 0x0014;
const NVME_REG_CSTS: u64 = 0x001c;
const NVME_REG_AQA: u64 = 0x0024;
const NVME_REG_ASQ: u64 = 0x0028;
const NVME_REG_ACQ: u64 = 0x0030;
const NVME_REG_DOORBELL: u64 = 0x1000;
const PCI_BAR0: u8 = 0;
/// Wall-clock budget for a cooperative wait loop, matching the original
/// 1,000,000-iteration * 10us busy-wait budget. Bounded by real elapsed time
/// (via [`Timer::micros`], a plain TSC read) rather than an iteration count,
/// since a `sched_yield`-based loop's iteration cost isn't a fixed duration.
const COMMAND_TIMEOUT_MICROS: i64 = 10_000_000;

/// A single page allocated as `BOOT_SERVICES_DATA`, used for NVMe queues and
/// I/O data. Freed automatically on drop.
struct DmaPage {
    ptr: NonNull<u8>,
}

impl DmaPage {
    fn new() -> Result<Self> {
        let ptr =
            uefi::boot::allocate_pages(AllocateType::AnyPages, MemoryType::BOOT_SERVICES_DATA, 1)?;
        unsafe { ptr::write_bytes(ptr.as_ptr(), 0, PAGE_SIZE) };
        Ok(Self { ptr })
    }

    fn as_ptr<T>(&self) -> *mut T {
        self.ptr.as_ptr().cast()
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), PAGE_SIZE) }
    }

    fn device_addr(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }
}

impl Drop for DmaPage {
    fn drop(&mut self) {
        let _ = unsafe { uefi::boot::free_pages(self.ptr, 1) };
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmeCommand {
    cdw0: u32,
    nsid: u32,
    reserved: [u32; 2],
    mptr: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmeCompletion {
    _dw0: u32,
    _dw1: u32,
    _dw2: u32,
    cid: u16,
    status: u16,
}

pub struct NvmeDisk {
    controller: Handle,
    pci: ScopedProtocol<PciIo>,
    admin_sq: DmaPage,
    admin_cq: DmaPage,
    io_sq: DmaPage,
    io_cq: DmaPage,
    data: DmaPage,
    namespace: u32,
    block_size: u64,
    blocks: u64,
    doorbell_stride: u64,
    admin_sq_tail: u16,
    admin_cq_head: u16,
    admin_cq_phase: bool,
    io_sq_tail: u16,
    io_cq_head: u16,
    io_cq_phase: bool,
}

impl NvmeDisk {
    fn open_pci(controller: Handle) -> Result<ScopedProtocol<PciIo>> {
        let image_handle = uefi::boot::image_handle();
        Ok(unsafe {
            uefi::boot::open_protocol::<PciIo>(
                OpenProtocolParams {
                    agent: image_handle,
                    controller: None,
                    handle: controller,
                },
                OpenProtocolAttributes::GetProtocol,
            )?
        })
    }

    fn enable_dma(pci: &mut PciIo) -> Result<()> {
        // SAFETY: enabling memory decode and bus mastering is required for MMIO
        // register access and DMA, and is safe for any PCI function.
        unsafe {
            pci.enable_attributes(
                PciIoProtocolAttributes::EFI_PCI_IO_ATTRIBUTE_MEMORY
                    | PciIoProtocolAttributes::EFI_PCI_IO_ATTRIBUTE_BUS_MASTER,
            )?
        };
        Ok(())
    }

    fn read32(pci: &mut PciIo, offset: u64) -> Result<u32> {
        Ok(pci.memory(PCI_BAR0).read_one::<u32>(offset)?)
    }

    fn write32(pci: &mut PciIo, offset: u64, value: u32) -> Result<()> {
        Ok(pci.memory(PCI_BAR0).write_one::<u32>(offset, value)?)
    }

    async fn open(controller: Handle) -> Result<Self> {
        let mut pci = Self::open_pci(controller)?;
        Self::enable_dma(&mut pci)?;
        let admin_sq = DmaPage::new()?;
        let admin_cq = DmaPage::new()?;
        let io_sq = DmaPage::new()?;
        let io_cq = DmaPage::new()?;
        let data = DmaPage::new()?;
        let mut disk = Self {
            controller,
            pci,
            admin_sq,
            admin_cq,
            io_sq,
            io_cq,
            data,
            namespace: 0,
            block_size: 0,
            blocks: 0,
            doorbell_stride: 0,
            admin_sq_tail: 0,
            admin_cq_head: 0,
            admin_cq_phase: true,
            io_sq_tail: 0,
            io_cq_head: 0,
            io_cq_phase: true,
        };
        disk.initialize().await?;
        Ok(disk)
    }

    /// Waits for the controller ready bit to reach `wanted`, cooperatively
    /// yielding to the executor between polls instead of blocking it. Used
    /// during controller bring-up, which happens off the hot I/O path.
    ///
    /// Yields via `sched_yield` rather than sleeping/waiting for an
    /// interrupt: nothing wires up a real interrupt for this controller, and
    /// `Executor::sleep` only re-checks its deadline when the executor wakes
    /// from `hlt` for some unrelated interrupt, so it gives no real latency
    /// bound. `sched_yield` re-polls on the very next executor pass, keeping
    /// this about as fast as a busy-wait while still letting other ready
    /// tasks run in between.
    async fn wait_ready(&mut self, wanted: bool) -> Result<()> {
        let deadline = Timer::micros() + COMMAND_TIMEOUT_MICROS;
        loop {
            if (Self::read32(&mut self.pci, NVME_REG_CSTS)? & 1 != 0) == wanted {
                return Ok(());
            }
            if Timer::micros() > deadline {
                return Err(Error::msg("NVMe controller did not change ready state"));
            }
            Executor::sched_yield().await;
        }
    }

    /// Synchronous counterpart of [`Self::wait_ready`]. `Drop::drop` cannot
    /// be async, so controller teardown busy-waits here instead; this only
    /// runs once, off the executor, when the disk itself is going away.
    fn wait_ready_sync(&mut self, wanted: bool) -> Result<()> {
        for _ in 0..1_000_000 {
            if (Self::read32(&mut self.pci, NVME_REG_CSTS)? & 1 != 0) == wanted {
                return Ok(());
            }
            uefi::boot::stall(Duration::from_micros(10));
        }
        Err(Error::msg("NVMe controller did not change ready state"))
    }

    async fn initialize(&mut self) -> Result<()> {
        let cap_lo = Self::read32(&mut self.pci, NVME_REG_CAP)?;
        let cap_hi = Self::read32(&mut self.pci, NVME_REG_CAP + 4)?;
        if (cap_lo as usize & 0xffff) + 1 < QUEUE_ENTRIES {
            return Err(Error::msg("NVMe controller queue depth is too small"));
        }
        // CAP.DSTRD is bits 35:32, therefore bits 3:0 of cap_hi.
        self.doorbell_stride = 4_u64 << (cap_hi & 0xf);

        Self::write32(&mut self.pci, NVME_REG_CC, 0)?;
        self.wait_ready(false).await?;
        Self::write32(
            &mut self.pci,
            NVME_REG_AQA,
            ((QUEUE_ENTRIES as u32 - 1) << 16) | (QUEUE_ENTRIES as u32 - 1),
        )?;
        let admin_sq_addr = self.admin_sq.device_addr();
        Self::write32(&mut self.pci, NVME_REG_ASQ, admin_sq_addr as u32)?;
        Self::write32(
            &mut self.pci,
            NVME_REG_ASQ + 4,
            (admin_sq_addr >> 32) as u32,
        )?;
        let admin_cq_addr = self.admin_cq.device_addr();
        Self::write32(&mut self.pci, NVME_REG_ACQ, admin_cq_addr as u32)?;
        Self::write32(
            &mut self.pci,
            NVME_REG_ACQ + 4,
            (admin_cq_addr >> 32) as u32,
        )?;
        // EN | CSS=NVM | MPS=0 | IOSQES=64 bytes | IOCQES=16 bytes.
        Self::write32(&mut self.pci, NVME_REG_CC, 1 | (6 << 16) | (4 << 20))?;
        self.wait_ready(true).await?;

        self.submit_admin(NvmeCommand {
            cdw0: 0x05, // Create I/O completion queue
            prp1: self.io_cq.device_addr(),
            cdw10: 1 | ((QUEUE_ENTRIES as u32 - 1) << 16),
            cdw11: 1, // physically contiguous
            ..Default::default()
        })
        .await?;
        self.submit_admin(NvmeCommand {
            cdw0: 0x01, // Create I/O submission queue
            prp1: self.io_sq.device_addr(),
            cdw10: 1 | ((QUEUE_ENTRIES as u32 - 1) << 16),
            cdw11: 1 | (1 << 16), // physically contiguous, completion queue 1
            ..Default::default()
        })
        .await?;
        Ok(())
    }

    /// Writes `command` into `sq`, rings the doorbell, and cooperatively
    /// waits for its completion on `cq`, yielding to the executor between
    /// polls instead of blocking it. Used on the async read/write/admin
    /// path; see [`Self::submit_sync`] for the blocking counterpart kept for
    /// the synchronous GPT-parsing path and controller teardown.
    #[allow(clippy::too_many_arguments)]
    async fn submit(
        pci: &mut PciIo,
        sq: &DmaPage,
        cq: &DmaPage,
        doorbell_stride: u64,
        queue: u16,
        sq_tail: &mut u16,
        cq_head: &mut u16,
        cq_phase: &mut bool,
        mut command: NvmeCommand,
    ) -> Result<()> {
        let cid = *sq_tail;
        command.cdw0 |= (cid as u32) << 16;
        unsafe { ptr::write_volatile(sq.as_ptr::<NvmeCommand>().add(cid as usize), command) };
        fence(Ordering::Release);
        *sq_tail = (cid + 1) % QUEUE_ENTRIES as u16;
        Self::write32(
            pci,
            NVME_REG_DOORBELL + 2 * queue as u64 * doorbell_stride,
            *sq_tail as u32,
        )?;

        // Yield via `sched_yield` rather than sleeping/waiting for an
        // interrupt: see the comment on `wait_ready` for why. The deadline
        // is checked via `Timer::micros` (a plain TSC read, no interrupt
        // needed) rather than an iteration count, since each `sched_yield`
        // round-trip isn't a fixed duration.
        let deadline = Timer::micros() + COMMAND_TIMEOUT_MICROS;
        loop {
            let completion =
                unsafe { ptr::read_volatile(cq.as_ptr::<NvmeCompletion>().add(*cq_head as usize)) };
            if (completion.status & 1 != 0) == *cq_phase {
                fence(Ordering::Acquire);
                if completion.status & 0xfffe != 0 {
                    return Err(Error::msg("NVMe command completed with an error"));
                }
                *cq_head += 1;
                if *cq_head == QUEUE_ENTRIES as u16 {
                    *cq_head = 0;
                    *cq_phase = !*cq_phase;
                }
                Self::write32(
                    pci,
                    NVME_REG_DOORBELL + (2 * queue as u64 + 1) * doorbell_stride,
                    *cq_head as u32,
                )?;
                return Ok(());
            }
            if Timer::micros() > deadline {
                return Err(Error::msg("NVMe command timed out"));
            }
            Executor::sched_yield().await;
        }
    }

    /// Synchronous counterpart of [`Self::submit`], used only by the sync
    /// GPT-parsing path ([`Self::transfer_sync`]).
    #[allow(clippy::too_many_arguments)]
    fn submit_sync(
        pci: &mut PciIo,
        sq: &DmaPage,
        cq: &DmaPage,
        doorbell_stride: u64,
        queue: u16,
        sq_tail: &mut u16,
        cq_head: &mut u16,
        cq_phase: &mut bool,
        mut command: NvmeCommand,
    ) -> Result<()> {
        let cid = *sq_tail;
        command.cdw0 |= (cid as u32) << 16;
        unsafe { ptr::write_volatile(sq.as_ptr::<NvmeCommand>().add(cid as usize), command) };
        fence(Ordering::Release);
        *sq_tail = (cid + 1) % QUEUE_ENTRIES as u16;
        Self::write32(
            pci,
            NVME_REG_DOORBELL + 2 * queue as u64 * doorbell_stride,
            *sq_tail as u32,
        )?;

        for _ in 0..1_000_000 {
            let completion =
                unsafe { ptr::read_volatile(cq.as_ptr::<NvmeCompletion>().add(*cq_head as usize)) };
            if (completion.status & 1 != 0) == *cq_phase {
                fence(Ordering::Acquire);
                if completion.status & 0xfffe != 0 {
                    return Err(Error::msg("NVMe command completed with an error"));
                }
                *cq_head += 1;
                if *cq_head == QUEUE_ENTRIES as u16 {
                    *cq_head = 0;
                    *cq_phase = !*cq_phase;
                }
                Self::write32(
                    pci,
                    NVME_REG_DOORBELL + (2 * queue as u64 + 1) * doorbell_stride,
                    *cq_head as u32,
                )?;
                return Ok(());
            }
            uefi::boot::stall(Duration::from_micros(10));
        }
        Err(Error::msg("NVMe command timed out"))
    }

    async fn submit_admin(&mut self, command: NvmeCommand) -> Result<()> {
        Self::submit(
            &mut self.pci,
            &self.admin_sq,
            &self.admin_cq,
            self.doorbell_stride,
            0,
            &mut self.admin_sq_tail,
            &mut self.admin_cq_head,
            &mut self.admin_cq_phase,
            command,
        )
        .await
    }

    async fn submit_io(&mut self, command: NvmeCommand) -> Result<()> {
        Self::submit(
            &mut self.pci,
            &self.io_sq,
            &self.io_cq,
            self.doorbell_stride,
            1,
            &mut self.io_sq_tail,
            &mut self.io_cq_head,
            &mut self.io_cq_phase,
            command,
        )
        .await
    }

    fn submit_io_sync(&mut self, command: NvmeCommand) -> Result<()> {
        Self::submit_sync(
            &mut self.pci,
            &self.io_sq,
            &self.io_cq,
            self.doorbell_stride,
            1,
            &mut self.io_sq_tail,
            &mut self.io_cq_head,
            &mut self.io_cq_phase,
            command,
        )
    }

    async fn identify_namespace(&mut self, namespace: u32) -> Result<(u64, u64)> {
        self.submit_admin(NvmeCommand {
            cdw0: 0x06, // Identify
            nsid: namespace,
            prp1: self.data.device_addr(),
            ..Default::default()
        })
        .await?;
        let identify = self.data.as_bytes();
        let blocks = u64::from_le_bytes(identify[0..8].try_into().unwrap());
        let format = identify[26] & 0x0f;
        let lbads = identify[128 + format as usize * 4 + 2];
        let block_size = 1_u64
            .checked_shl(lbads as u32)
            .ok_or_else(|| Error::msg("invalid NVMe namespace block size"))?;
        if blocks == 0 || block_size == 0 || block_size > PAGE_SIZE as u64 {
            return Err(Error::msg("unsupported NVMe namespace geometry"));
        }
        Ok((blocks, block_size))
    }

    fn set_namespace(&mut self, namespace: u32, blocks: u64, block_size: u64) {
        self.namespace = namespace;
        self.blocks = blocks;
        self.block_size = block_size;
    }

    async fn transfer(&mut self, write: bool, lba: u64, blocks: u16) -> Result<()> {
        self.submit_io(NvmeCommand {
            cdw0: if write { 0x01 } else { 0x02 }, // Write / Read
            nsid: self.namespace,
            prp1: self.data.device_addr(),
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: blocks as u32 - 1,
            ..Default::default()
        })
        .await
    }

    fn transfer_sync(&mut self, write: bool, lba: u64, blocks: u16) -> Result<()> {
        self.submit_io_sync(NvmeCommand {
            cdw0: if write { 0x01 } else { 0x02 }, // Write / Read
            nsid: self.namespace,
            prp1: self.data.device_addr(),
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: blocks as u32 - 1,
            ..Default::default()
        })
    }

    fn check_range(&self, offset: u64, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| Error::msg("disk range overflow"))?;
        if end > self.size() {
            return Err(Error::msg("disk range is outside the namespace"));
        }
        Ok(())
    }

    /// Finds handles backed by a physical (non-RAID) NVMe controller, using
    /// the firmware's own pass-thru driver only to tell physical controllers
    /// apart from logical/RAID volumes. Namespace discovery happens later,
    /// over our own admin queue: the firmware's `GetNextNamespace` walks
    /// every allocated namespace ID up to the controller's maximum rather
    /// than only the active ones, which is needlessly slow.
    fn probe_controllers() -> Vec<Handle> {
        let image_handle = uefi::boot::image_handle();
        uefi::boot::find_handles::<NvmePassThru>()
            .unwrap_or_default()
            .into_iter()
            .filter(|&handle| {
                let Ok(protocol) = (unsafe {
                    uefi::boot::open_protocol::<NvmePassThru>(
                        OpenProtocolParams {
                            agent: image_handle,
                            controller: None,
                            handle,
                        },
                        OpenProtocolAttributes::GetProtocol,
                    )
                }) else {
                    return false;
                };
                protocol
                    .mode()
                    .attributes
                    .contains(NvmExpressPassThruAttributes::PHYSICAL)
            })
            .collect()
    }

    /// Lists active namespace IDs directly from the controller (Identify,
    /// CNS=Active Namespace ID List), in one round trip.
    async fn active_namespaces(&mut self) -> Result<Vec<u32>> {
        self.submit_admin(NvmeCommand {
            cdw0: 0x06,  // Identify
            cdw10: 0x02, // CNS: active namespace ID list
            prp1: self.data.device_addr(),
            ..Default::default()
        })
        .await?;
        Ok(self
            .data
            .as_bytes()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|entry| u32::from_le_bytes(*entry))
            .take_while(|&namespace| namespace != 0)
            .collect())
    }

    /// Opens the NVMe namespace with the lowest `score(size)` among all
    /// physical NVMe controllers, or `None` if none exposes a usable
    /// namespace. Returns the winning score alongside the disk so callers
    /// can compare it against candidates from other backends; the selected
    /// controller remains disconnected from its firmware driver until drop.
    pub(super) async fn choose(score: impl Fn(u64) -> u128) -> Option<(u128, NvmeDisk)> {
        let mut selected: Option<(u128, NvmeDisk)> = None;
        for controller in Self::probe_controllers() {
            // A failed disconnect can mean that no firmware driver was bound;
            // the PCI I/O protocol may still be usable, so continue probing.
            if let Err(error) = uefi::boot::disconnect_controller(controller, None, None) {
                log::debug!("NVMe {controller:?}: disconnect: {error}");
            }
            let mut disk = match Self::open(controller).await {
                Ok(disk) => disk,
                Err(error) => {
                    log::debug!("NVMe {controller:?}: {error}");
                    let _ = uefi::boot::connect_controller(controller, &[], None, true);
                    continue;
                }
            };
            let namespaces = match disk.active_namespaces().await {
                Ok(namespaces) => namespaces,
                Err(error) => {
                    // `disk` is dropped here, which reconnects the firmware driver.
                    log::debug!("NVMe {controller:?}: active namespaces: {error}");
                    continue;
                }
            };
            // Same semantics as `.filter_map(..).min_by_key(..)` (first
            // minimum wins on a tie), rewritten as an explicit loop since the
            // per-namespace identify is now async.
            let mut best_for_ctrl: Option<(u128, u32, u64, u64)> = None;
            for namespace in namespaces {
                match disk.identify_namespace(namespace).await {
                    Ok((blocks, block_size)) => {
                        let size = block_size.saturating_mul(blocks);
                        let ns_score = score(size);
                        if best_for_ctrl
                            .as_ref()
                            .is_none_or(|(best, ..)| ns_score < *best)
                        {
                            best_for_ctrl = Some((ns_score, namespace, blocks, block_size));
                        }
                    }
                    Err(error) => {
                        log::debug!("NVMe {controller:?} namespace {namespace}: {error}");
                    }
                }
            }
            if let Some((ctrl_score, ns, blocks, block_size)) = best_for_ctrl
                && selected
                    .as_ref()
                    .is_none_or(|(best_score, _)| ctrl_score < *best_score)
            {
                disk.set_namespace(ns, blocks, block_size);
                selected = Some((ctrl_score, disk));
                continue;
            }
            // Losing controller or controller with no supported namespaces is dropped here
            // and its firmware driver is restored.
        }
        selected
    }

    pub(super) fn size(&self) -> u64 {
        self.block_size.saturating_mul(self.blocks)
    }

    pub(super) fn block_size(&self) -> u32 {
        self.block_size as u32
    }

    pub(super) fn num_blocks(&self) -> u64 {
        self.blocks
    }

    pub(super) async fn flush(&mut self) -> Result<()> {
        self.submit_io(NvmeCommand {
            cdw0: 0x00, // Flush
            nsid: self.namespace,
            ..Default::default()
        })
        .await
    }

    /// Synchronous read, used only for GPT parsing (via the sync
    /// `gpt_disk_io::BlockIo` trait) before the async disk API is available.
    pub(super) fn read_sync(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check_range(offset, buf.len())?;
        let mut offset = offset;
        let mut buf = buf;
        while !buf.is_empty() {
            let lba = offset / self.block_size;
            let in_block = (offset % self.block_size) as usize;
            let blocks = ((in_block + buf.len()).div_ceil(self.block_size as usize))
                .min(PAGE_SIZE / self.block_size as usize);
            self.transfer_sync(false, lba, blocks as u16)?;
            let bytes = blocks * self.block_size as usize;
            let copied = (bytes - in_block).min(buf.len());
            unsafe {
                ptr::copy_nonoverlapping(
                    self.data.as_ptr::<u8>().add(in_block),
                    buf.as_mut_ptr(),
                    copied,
                )
            };
            offset += copied as u64;
            buf = &mut buf[copied..];
        }
        Ok(())
    }

    pub(super) async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        Executor::sched_yield().await;
        self.check_range(offset, buf.len())?;
        let mut offset = offset;
        let mut buf = buf;
        while !buf.is_empty() {
            let lba = offset / self.block_size;
            let in_block = (offset % self.block_size) as usize;
            let blocks = ((in_block + buf.len()).div_ceil(self.block_size as usize))
                .min(PAGE_SIZE / self.block_size as usize);
            self.transfer(false, lba, blocks as u16).await?;
            let bytes = blocks * self.block_size as usize;
            let copied = (bytes - in_block).min(buf.len());
            unsafe {
                ptr::copy_nonoverlapping(
                    self.data.as_ptr::<u8>().add(in_block),
                    buf.as_mut_ptr(),
                    copied,
                )
            };
            offset += copied as u64;
            buf = &mut buf[copied..];
        }
        Ok(())
    }

    /// Synchronous write, used only for GPT parsing (via the sync
    /// `gpt_disk_io::BlockIo` trait) before the async disk API is available.
    pub(super) fn write_sync(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.check_range(offset, buf.len())?;
        let mut offset = offset;
        let mut buf = buf;
        while !buf.is_empty() {
            let lba = offset / self.block_size;
            let in_block = (offset % self.block_size) as usize;
            let blocks = ((in_block + buf.len()).div_ceil(self.block_size as usize))
                .min(PAGE_SIZE / self.block_size as usize);
            // Read-modify-write keeps the public byte-granular API correct.
            self.transfer_sync(false, lba, blocks as u16)?;
            let bytes = blocks * self.block_size as usize;
            let copied = (bytes - in_block).min(buf.len());
            unsafe {
                ptr::copy_nonoverlapping(
                    buf.as_ptr(),
                    self.data.as_ptr::<u8>().add(in_block),
                    copied,
                )
            };
            self.transfer_sync(true, lba, blocks as u16)?;
            offset += copied as u64;
            buf = &buf[copied..];
        }
        Ok(())
    }

    pub(super) async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        Executor::sched_yield().await;
        self.check_range(offset, buf.len())?;
        let mut offset = offset;
        let mut buf = buf;
        while !buf.is_empty() {
            let lba = offset / self.block_size;
            let in_block = (offset % self.block_size) as usize;
            let blocks = ((in_block + buf.len()).div_ceil(self.block_size as usize))
                .min(PAGE_SIZE / self.block_size as usize);
            // Read-modify-write keeps the public byte-granular API correct.
            self.transfer(false, lba, blocks as u16).await?;
            let bytes = blocks * self.block_size as usize;
            let copied = (bytes - in_block).min(buf.len());
            unsafe {
                ptr::copy_nonoverlapping(
                    buf.as_ptr(),
                    self.data.as_ptr::<u8>().add(in_block),
                    copied,
                )
            };
            self.transfer(true, lba, blocks as u16).await?;
            offset += copied as u64;
            buf = &buf[copied..];
        }
        Ok(())
    }
}

impl Drop for NvmeDisk {
    fn drop(&mut self) {
        let _ = Self::write32(&mut self.pci, NVME_REG_CC, 0);
        let _ = self.wait_ready_sync(false);
        let _ = uefi::boot::connect_controller(self.controller, &[], None, true);
    }
}
