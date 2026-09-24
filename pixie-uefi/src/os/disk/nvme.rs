use alloc::vec::Vec;
use core::ptr::{self, NonNull};
use core::sync::atomic::{Ordering, fence};
use core::time::Duration;

use crate::os::error::{Error, Result};
use crate::os::executor::Executor;
use crate::os::memory::{self, PAGE_SIZE};
use crate::os::pci::{PciDevice, scan_pci};
use crate::os::timer::Timer;

const QUEUE_ENTRIES: usize = 64;
const NVME_REG_CAP: u64 = 0x0000;
const NVME_REG_CC: u64 = 0x0014;
const NVME_REG_CSTS: u64 = 0x001c;
const NVME_REG_AQA: u64 = 0x0024;
const NVME_REG_ASQ: u64 = 0x0028;
const NVME_REG_ACQ: u64 = 0x0030;
const NVME_REG_DOORBELL: u64 = 0x1000;
const COMMAND_TIMEOUT_MICROS: i64 = 10_000_000;

struct DmaPage {
    phys: u64,
    ptr: NonNull<u8>,
}

impl DmaPage {
    fn new() -> Result<Self> {
        let phys = memory::alloc_page()
            .ok_or_else(|| Error::msg("Out of physical memory for NVMe DMA"))?;
        let ptr = NonNull::new(phys as *mut u8).unwrap();
        unsafe { ptr::write_bytes(ptr.as_ptr(), 0, PAGE_SIZE as usize) };
        Ok(Self { phys, ptr })
    }

    fn as_ptr<T>(&self) -> *mut T {
        self.ptr.as_ptr().cast()
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), PAGE_SIZE as usize) }
    }

    fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), PAGE_SIZE as usize) }
    }

    fn device_addr(&self) -> u64 {
        self.phys
    }
}

impl Drop for DmaPage {
    fn drop(&mut self) {
        memory::free_page(self.phys);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmeCommand {
    cdw0: u32,
    nsid: u32,
    reserved: u64,
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
    dw0: u32,
    dw1: u32,
    sq_head: u16,
    sq_id: u16,
    cid: u16,
    status: u16,
}

pub struct NvmeDisk {
    pub pci: PciDevice,
    bar0: u64,
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
    fn read32(&self, offset: u64) -> u32 {
        unsafe { core::ptr::read_volatile((self.bar0 + offset) as *const u32) }
    }

    fn write32(&self, offset: u64, value: u32) {
        unsafe { core::ptr::write_volatile((self.bar0 + offset) as *mut u32, value) }
    }

    pub async fn open(pci: PciDevice) -> Result<Self> {
        pci.enable_bus_mastering();
        let bar0 = match pci.read_bar(0) {
            Some(crate::os::pci::Bar::Memory { base, .. }) => base,
            _ => return Err(Error::msg("NVMe BAR0 is not memory-mapped")),
        };

        let admin_sq = DmaPage::new()?;
        let admin_cq = DmaPage::new()?;
        let io_sq = DmaPage::new()?;
        let io_cq = DmaPage::new()?;
        let data = DmaPage::new()?;

        let mut disk = Self {
            pci,
            bar0,
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

    async fn wait_ready(&self, wanted: bool) -> Result<()> {
        let deadline = Timer::micros() + COMMAND_TIMEOUT_MICROS;
        loop {
            if (self.read32(NVME_REG_CSTS) & 1 != 0) == wanted {
                return Ok(());
            }
            if Timer::micros() > deadline {
                return Err(Error::msg("NVMe controller ready state timeout"));
            }
            Executor::sleep(Duration::from_millis(1)).await;
        }
    }

    fn wait_ready_sync(&self, wanted: bool) -> Result<()> {
        for _ in 0..1_000_000 {
            if (self.read32(NVME_REG_CSTS) & 1 != 0) == wanted {
                return Ok(());
            }
            crate::os::arch::io::pause();
        }
        Err(Error::msg("NVMe controller ready state timeout (sync)"))
    }

    async fn initialize(&mut self) -> Result<()> {
        let cap_lo = self.read32(NVME_REG_CAP);
        let cap_hi = self.read32(NVME_REG_CAP + 4);
        if (cap_lo as usize & 0xffff) + 1 < QUEUE_ENTRIES {
            return Err(Error::msg("NVMe queue depth too small"));
        }
        self.doorbell_stride = 4_u64 << (cap_hi & 0xf);

        self.write32(NVME_REG_CC, 0);
        self.wait_ready(false).await?;

        let aqa = ((QUEUE_ENTRIES as u32 - 1) << 16) | (QUEUE_ENTRIES as u32 - 1);
        self.write32(NVME_REG_AQA, aqa);
        self.write32(NVME_REG_ASQ, self.admin_sq.device_addr() as u32);
        self.write32(NVME_REG_ASQ + 4, (self.admin_sq.device_addr() >> 32) as u32);
        self.write32(NVME_REG_ACQ, self.admin_cq.device_addr() as u32);
        self.write32(NVME_REG_ACQ + 4, (self.admin_cq.device_addr() >> 32) as u32);

        // Enable controller with 4KB page size (MPS=0) and standard round robin (AMS=0)
        self.write32(NVME_REG_CC, (6 << 16) | (4 << 20) | 1);
        self.wait_ready(true).await?;

        // Create I/O CQ (QID=1)
        self.submit_admin(NvmeCommand {
            cdw0: 0x05,
            prp1: self.io_cq.device_addr(),
            cdw10: ((QUEUE_ENTRIES as u32 - 1) << 16) | 1,
            cdw11: 1, // Physically contiguous
            ..Default::default()
        })
        .await?;

        // Create I/O SQ (QID=1)
        self.submit_admin(NvmeCommand {
            cdw0: 0x01,
            prp1: self.io_sq.device_addr(),
            cdw10: ((QUEUE_ENTRIES as u32 - 1) << 16) | 1,
            cdw11: (1 << 16) | 1, // CQID=1, Physically contiguous
            ..Default::default()
        })
        .await?;

        // Discover active namespaces
        let namespaces = self.active_namespaces().await?;
        if let Some(&ns) = namespaces.first() {
            let (blocks, block_size) = self.identify_namespace(ns).await?;
            self.set_namespace(ns, blocks, block_size);
        } else {
            return Err(Error::msg("No active NVMe namespace found"));
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit(
        bar0: u64,
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

        unsafe {
            core::ptr::write_volatile(
                (bar0 + NVME_REG_DOORBELL + 2 * queue as u64 * doorbell_stride) as *mut u32,
                *sq_tail as u32,
            );
        }

        // Fast path: quick spin for immediate completions (< 20 us)
        for _ in 0..100 {
            let completion =
                unsafe { ptr::read_volatile(cq.as_ptr::<NvmeCompletion>().add(*cq_head as usize)) };
            if (completion.status & 1 != 0) == *cq_phase {
                fence(Ordering::Acquire);
                if completion.status & 0xfffe != 0 {
                    return Err(Error::msg("NVMe command completed with error"));
                }
                *cq_head += 1;
                if *cq_head == QUEUE_ENTRIES as u16 {
                    *cq_head = 0;
                    *cq_phase = !*cq_phase;
                }
                unsafe {
                    core::ptr::write_volatile(
                        (bar0 + NVME_REG_DOORBELL + (2 * queue as u64 + 1) * doorbell_stride)
                            as *mut u32,
                        *cq_head as u32,
                    );
                }
                return Ok(());
            }
            core::hint::spin_loop();
        }

        let deadline = Timer::micros() + COMMAND_TIMEOUT_MICROS;
        loop {
            let completion =
                unsafe { ptr::read_volatile(cq.as_ptr::<NvmeCompletion>().add(*cq_head as usize)) };
            if (completion.status & 1 != 0) == *cq_phase {
                fence(Ordering::Acquire);
                if completion.status & 0xfffe != 0 {
                    return Err(Error::msg("NVMe command completed with error"));
                }
                *cq_head += 1;
                if *cq_head == QUEUE_ENTRIES as u16 {
                    *cq_head = 0;
                    *cq_phase = !*cq_phase;
                }
                unsafe {
                    core::ptr::write_volatile(
                        (bar0 + NVME_REG_DOORBELL + (2 * queue as u64 + 1) * doorbell_stride)
                            as *mut u32,
                        *cq_head as u32,
                    );
                }
                return Ok(());
            }
            if Timer::micros() > deadline {
                return Err(Error::msg("NVMe command timed out"));
            }
            Executor::sleep(Duration::from_micros(50)).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_sync(
        bar0: u64,
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

        unsafe {
            core::ptr::write_volatile(
                (bar0 + NVME_REG_DOORBELL + 2 * queue as u64 * doorbell_stride) as *mut u32,
                *sq_tail as u32,
            );
        }

        let deadline = Timer::micros() + 10_000_000; // 10 seconds
        loop {
            let completion =
                unsafe { ptr::read_volatile(cq.as_ptr::<NvmeCompletion>().add(*cq_head as usize)) };
            if (completion.status & 1 != 0) == *cq_phase {
                fence(Ordering::Acquire);
                if completion.status & 0xfffe != 0 {
                    return Err(Error::msg("NVMe command completed with error"));
                }
                *cq_head += 1;
                if *cq_head == QUEUE_ENTRIES as u16 {
                    *cq_head = 0;
                    *cq_phase = !*cq_phase;
                }
                unsafe {
                    core::ptr::write_volatile(
                        (bar0 + NVME_REG_DOORBELL + (2 * queue as u64 + 1) * doorbell_stride)
                            as *mut u32,
                        *cq_head as u32,
                    );
                }
                return Ok(());
            }
            if Timer::micros() > deadline {
                return Err(Error::msg("NVMe command timed out (sync)"));
            }
            crate::os::arch::io::pause();
        }
    }

    async fn submit_admin(&mut self, command: NvmeCommand) -> Result<()> {
        Self::submit(
            self.bar0,
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
            self.bar0,
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
            self.bar0,
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
            cdw0: 0x06,
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
            .ok_or_else(|| Error::msg("Invalid NVMe namespace block size"))?;
        if blocks == 0 || block_size == 0 || block_size > PAGE_SIZE {
            return Err(Error::msg("Unsupported NVMe namespace geometry"));
        }
        Ok((blocks, block_size))
    }

    pub fn set_namespace(&mut self, namespace: u32, blocks: u64, block_size: u64) {
        self.namespace = namespace;
        self.blocks = blocks;
        self.block_size = block_size;
    }

    #[allow(clippy::chunks_exact_to_as_chunks)]
    async fn active_namespaces(&mut self) -> Result<Vec<u32>> {
        self.submit_admin(NvmeCommand {
            cdw0: 0x06,
            cdw10: 0x02,
            prp1: self.data.device_addr(),
            ..Default::default()
        })
        .await?;
        let bytes = self.data.as_bytes();
        let mut ns_list = Vec::new();
        for chunk in bytes.chunks_exact(4) {
            let ns = u32::from_le_bytes(chunk.try_into().unwrap());
            if ns == 0 {
                break;
            }
            ns_list.push(ns);
        }
        Ok(ns_list)
    }

    async fn transfer(&mut self, write: bool, lba: u64, blocks: u16) -> Result<()> {
        self.submit_io(NvmeCommand {
            cdw0: if write { 0x01 } else { 0x02 },
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
            cdw0: if write { 0x01 } else { 0x02 },
            nsid: self.namespace,
            prp1: self.data.device_addr(),
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: blocks as u32 - 1,
            ..Default::default()
        })
    }

    pub fn size(&self) -> u64 {
        self.block_size.saturating_mul(self.blocks)
    }

    pub fn block_size(&self) -> u32 {
        self.block_size as u32
    }

    pub fn num_blocks(&self) -> u64 {
        self.blocks
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.submit_io(NvmeCommand {
            cdw0: 0x00,
            nsid: self.namespace,
            ..Default::default()
        })
        .await
    }

    pub fn read_sync(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut cur_offset = offset;
        let mut remaining = buf;
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min((PAGE_SIZE / self.block_size) as usize);

            self.transfer_sync(false, lba, blocks as u16)?;
            let chunk_bytes = (blocks * self.block_size as usize - in_block).min(remaining.len());
            remaining[..chunk_bytes]
                .copy_from_slice(&self.data.as_bytes()[in_block..in_block + chunk_bytes]);

            cur_offset += chunk_bytes as u64;
            remaining = &mut remaining[chunk_bytes..];
        }
        Ok(())
    }

    pub async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut cur_offset = offset;
        let mut remaining = buf;
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min((PAGE_SIZE / self.block_size) as usize);

            self.transfer(false, lba, blocks as u16).await?;
            let chunk_bytes = (blocks * self.block_size as usize - in_block).min(remaining.len());
            remaining[..chunk_bytes]
                .copy_from_slice(&self.data.as_bytes()[in_block..in_block + chunk_bytes]);

            cur_offset += chunk_bytes as u64;
            remaining = &mut remaining[chunk_bytes..];
        }
        Ok(())
    }

    pub fn write_sync(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut cur_offset = offset;
        let mut remaining = buf;
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min((PAGE_SIZE / self.block_size) as usize);

            if in_block != 0 || remaining.len() < blocks * self.block_size as usize {
                self.transfer_sync(false, lba, blocks as u16)?;
            }

            let chunk_bytes = (blocks * self.block_size as usize - in_block).min(remaining.len());
            self.data.as_bytes_mut()[in_block..in_block + chunk_bytes]
                .copy_from_slice(&remaining[..chunk_bytes]);

            self.transfer_sync(true, lba, blocks as u16)?;

            cur_offset += chunk_bytes as u64;
            remaining = &remaining[chunk_bytes..];
        }
        Ok(())
    }

    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut cur_offset = offset;
        let mut remaining = buf;
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min((PAGE_SIZE / self.block_size) as usize);

            if in_block != 0 || remaining.len() < blocks * self.block_size as usize {
                self.transfer(false, lba, blocks as u16).await?;
            }

            let chunk_bytes = (blocks * self.block_size as usize - in_block).min(remaining.len());
            self.data.as_bytes_mut()[in_block..in_block + chunk_bytes]
                .copy_from_slice(&remaining[..chunk_bytes]);

            self.transfer(true, lba, blocks as u16).await?;

            cur_offset += chunk_bytes as u64;
            remaining = &remaining[chunk_bytes..];
        }
        Ok(())
    }

    pub async fn probe_all() -> Vec<NvmeDisk> {
        let mut disks = Vec::new();
        for dev in scan_pci() {
            if dev.class_code == 0x01 && dev.subclass == 0x08 && dev.prog_if == 0x02 {
                log::info!(
                    "Probing NVMe controller at {:02x}:{:02x}.{}",
                    dev.bus,
                    dev.dev,
                    dev.func
                );
                match Self::open(dev).await {
                    Ok(disk) => {
                        log::info!(
                            "NVMe drive initialized: {} blocks of {} bytes ({} GB)",
                            disk.blocks,
                            disk.block_size,
                            disk.size() / (1024 * 1024 * 1024)
                        );
                        disks.push(disk);
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to initialize NVMe controller at {:02x}:{:02x}.{}: {e}",
                            dev.bus,
                            dev.dev,
                            dev.func
                        );
                    }
                }
            }
        }
        disks
    }
}

impl Drop for NvmeDisk {
    fn drop(&mut self) {
        self.write32(NVME_REG_CC, 0);
        let _ = self.wait_ready_sync(false);
    }
}
