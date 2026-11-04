use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE};
use crate::pci::PciDevice;

pub const NVME_QUEUE_ENTRIES: usize = 64;
pub const NVME_MAX_DMA_PAGES: usize = 64; // 256 KiB per I/O command via PRP list

const NVME_REG_CAP: u64 = 0x0000;
const NVME_REG_CC: u64 = 0x0014;
const NVME_REG_CSTS: u64 = 0x001C;
const NVME_REG_AQA: u64 = 0x0024;
const NVME_REG_ASQ: u64 = 0x0028;
const NVME_REG_ACQ: u64 = 0x0030;
const NVME_REG_DOORBELL: u64 = 0x1000;
const COMMAND_TIMEOUT_MICROS: i64 = 10_000_000;
const MAX_POLL_YIELDS: usize = 10_000_000;

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

pub struct NvmeDisk<H: Hal> {
    pub pci: PciDevice,
    pub bar0: u64,
    pub hal: H,
    admin_sq: DmaBuffer,
    admin_cq: DmaBuffer,
    io_sq: DmaBuffer,
    io_cq: DmaBuffer,
    prp_list: DmaBuffer,
    data: DmaBuffer,
    queue_entries: u16,
    namespace: u32,
    block_size: u64,
    blocks: u64,
    max_transfer_bytes: usize,
    doorbell_stride: u64,
    admin_sq_tail: u16,
    admin_cq_head: u16,
    admin_cq_phase: bool,
    io_sq_tail: u16,
    io_cq_head: u16,
    io_cq_phase: bool,
}

impl<H: Hal> NvmeDisk<H> {
    fn read32(&self, offset: u64) -> u32 {
        self.hal.mmio_read32(self.bar0 + offset)
    }

    fn write32(&self, offset: u64, value: u32) {
        self.hal.mmio_write32(self.bar0 + offset, value);
    }

    /// Initializes the NVMe controller at `bar0`, sets up Admin and I/O queues,
    /// identifies the controller (`CNS=1`, reading `MDTS` and `NN`), and identifies
    /// the first valid namespace (`CNS=2` active list with fallback to `CNS=0` on `1..=NN`).
    pub async fn open(pci: PciDevice, bar0: u64, hal: H) -> Result<Self> {
        if bar0 == 0 {
            return Err(Error::msg("NVMe BAR0 is 0"));
        }
        pci.enable_bus_mastering(&hal);
        hal.map_mmio(bar0, 0x4000);

        let admin_sq = hal
            .alloc_dma(1)
            .ok_or_else(|| Error::msg("Out of DMA memory for NVMe admin_sq"))?;
        let Some(admin_cq) = hal.alloc_dma(1) else {
            hal.free_dma(admin_sq);
            return Err(Error::msg("Out of DMA memory for NVMe admin_cq"));
        };
        let Some(io_sq) = hal.alloc_dma(1) else {
            hal.free_dma(admin_sq);
            hal.free_dma(admin_cq);
            return Err(Error::msg("Out of DMA memory for NVMe io_sq"));
        };
        let Some(io_cq) = hal.alloc_dma(1) else {
            hal.free_dma(admin_sq);
            hal.free_dma(admin_cq);
            hal.free_dma(io_sq);
            return Err(Error::msg("Out of DMA memory for NVMe io_cq"));
        };
        let Some(prp_list) = hal.alloc_dma(1) else {
            hal.free_dma(admin_sq);
            hal.free_dma(admin_cq);
            hal.free_dma(io_sq);
            hal.free_dma(io_cq);
            return Err(Error::msg("Out of DMA memory for NVMe prp_list"));
        };
        let Some(data) = hal.alloc_dma(NVME_MAX_DMA_PAGES) else {
            hal.free_dma(admin_sq);
            hal.free_dma(admin_cq);
            hal.free_dma(io_sq);
            hal.free_dma(io_cq);
            hal.free_dma(prp_list);
            return Err(Error::msg("Out of DMA memory for NVMe data buffer"));
        };

        let mut disk = Self {
            pci,
            bar0,
            hal,
            admin_sq,
            admin_cq,
            io_sq,
            io_cq,
            prp_list,
            data,
            queue_entries: NVME_QUEUE_ENTRIES as u16,
            namespace: 0,
            block_size: 512,
            blocks: 0,
            max_transfer_bytes: NVME_MAX_DMA_PAGES * PAGE_SIZE,
            doorbell_stride: 4,
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
        let start = self.hal.micros();
        for _ in 0..MAX_POLL_YIELDS {
            let csts = self.read32(NVME_REG_CSTS);
            if (csts & 0x02) != 0 {
                return Err(Error::msg("NVMe CSTS.CFS fatal error"));
            }
            if ((csts & 0x01) != 0) == wanted {
                return Ok(());
            }
            if self.hal.micros().saturating_sub(start) > COMMAND_TIMEOUT_MICROS {
                return Err(Error::msg("NVMe controller ready state timeout"));
            }
            self.hal.yield_now().await;
        }
        Err(Error::msg("NVMe controller ready state timeout"))
    }

    async fn initialize(&mut self) -> Result<()> {
        let cap_lo = self.read32(NVME_REG_CAP);
        let cap_hi = self.read32(NVME_REG_CAP + 4);
        let mqes = ((cap_lo & 0xFFFF) as usize) + 1;
        if mqes < 2 {
            return Err(Error::msg("NVMe queue depth too small"));
        }
        let mpsmin = (cap_hi >> 16) & 0x0F;
        if mpsmin > 0 {
            return Err(Error::msg("NVMe controller requires page size > 4 KiB"));
        }
        self.queue_entries = NVME_QUEUE_ENTRIES.min(mqes) as u16;
        self.doorbell_stride = 4u64 << (cap_hi & 0x0F);

        self.write32(NVME_REG_CC, 0);
        self.wait_ready(false).await?;

        let qm1 = (self.queue_entries as u32) - 1;
        let aqa = (qm1 << 16) | qm1;
        self.write32(NVME_REG_AQA, aqa);
        self.write32(NVME_REG_ASQ, self.admin_sq.phys as u32);
        self.write32(NVME_REG_ASQ + 4, (self.admin_sq.phys >> 32) as u32);
        self.write32(NVME_REG_ACQ, self.admin_cq.phys as u32);
        self.write32(NVME_REG_ACQ + 4, (self.admin_cq.phys >> 32) as u32);

        self.write32(NVME_REG_CC, (4 << 20) | (6 << 16) | 1);
        self.wait_ready(true).await?;

        // Identify Controller (CNS = 1)
        let (mdts, nn) = self.identify_controller().await?;
        let max_pages = if mdts == 0 {
            NVME_MAX_DMA_PAGES
        } else {
            (1usize << (mdts as usize).min(20)).min(NVME_MAX_DMA_PAGES)
        };
        self.max_transfer_bytes = (max_pages * PAGE_SIZE).max(PAGE_SIZE);

        // Create I/O Completion Queue (QID = 1)
        self.submit_admin(NvmeCommand {
            cdw0: 0x05,
            prp1: self.io_cq.phys,
            cdw10: (qm1 << 16) | 1,
            cdw11: 1, // Physically contiguous (PC=1)
            ..Default::default()
        })
        .await?;

        // Create I/O Submission Queue (QID = 1, CQID = 1)
        self.submit_admin(NvmeCommand {
            cdw0: 0x01,
            prp1: self.io_sq.phys,
            cdw10: (qm1 << 16) | 1,
            cdw11: (1 << 16) | 1, // CQID=1, PC=1
            ..Default::default()
        })
        .await?;

        // Enumerate namespaces via CNS=2 active list with fallback to CNS=0 on 1..=NN
        let mut found_ns = None;
        if let Ok(ns_list) = self.active_namespaces().await {
            for ns in ns_list {
                if let Ok((blocks, block_size)) = self.identify_namespace(ns).await {
                    found_ns = Some((ns, blocks, block_size));
                    break;
                }
            }
        }

        if found_ns.is_none() {
            let max_nn = if nn == 0 { 1 } else { nn.min(16) };
            for ns in 1..=max_nn {
                if let Ok((blocks, block_size)) = self.identify_namespace(ns).await {
                    found_ns = Some((ns, blocks, block_size));
                    break;
                }
            }
        }

        if let Some((ns, blocks, block_size)) = found_ns {
            self.namespace = ns;
            self.blocks = blocks;
            self.block_size = block_size;
            Ok(())
        } else {
            Err(Error::msg("No active NVMe namespace found"))
        }
    }

    async fn identify_controller(&mut self) -> Result<(u8, u32)> {
        unsafe {
            self.data.as_mut_slice()[..PAGE_SIZE].fill(0);
        }
        self.submit_admin(NvmeCommand {
            cdw0: 0x06,
            nsid: 0,
            prp1: self.data.phys,
            cdw10: 0x01, // CNS = 1 (Identify Controller)
            ..Default::default()
        })
        .await?;

        let identify = unsafe { &self.data.as_slice()[..PAGE_SIZE] };
        let mdts = identify[77];
        let nn = u32::from_le_bytes(identify[516..520].try_into().unwrap());
        Ok((mdts, nn))
    }

    async fn active_namespaces(&mut self) -> Result<Vec<u32>> {
        unsafe {
            self.data.as_mut_slice()[..PAGE_SIZE].fill(0);
        }
        self.submit_admin(NvmeCommand {
            cdw0: 0x06,
            nsid: 0,
            prp1: self.data.phys,
            cdw10: 0x02, // CNS = 2 (Active Namespace ID List)
            ..Default::default()
        })
        .await?;

        let bytes = unsafe { &self.data.as_slice()[..PAGE_SIZE] };
        let mut ns_list = Vec::new();
        for &chunk in bytes.as_chunks::<4>().0 {
            let ns = u32::from_le_bytes(chunk);
            if ns == 0 {
                break;
            }
            ns_list.push(ns);
        }
        Ok(ns_list)
    }

    async fn identify_namespace(&mut self, namespace: u32) -> Result<(u64, u64)> {
        unsafe {
            self.data.as_mut_slice()[..PAGE_SIZE].fill(0);
        }
        self.submit_admin(NvmeCommand {
            cdw0: 0x06,
            nsid: namespace,
            prp1: self.data.phys,
            cdw10: 0x00, // CNS = 0 (Identify Namespace)
            ..Default::default()
        })
        .await?;

        let identify = unsafe { &self.data.as_slice()[..PAGE_SIZE] };
        let blocks = u64::from_le_bytes(identify[0..8].try_into().unwrap());
        if blocks == 0 {
            return Err(Error::msg("NVMe namespace has 0 blocks"));
        }
        let format = (identify[26] & 0x0F) as usize;
        let lbads = identify[128 + format * 4 + 2];
        let block_size = if lbads == 0 {
            512
        } else {
            1u64.checked_shl(lbads as u32)
                .ok_or_else(|| Error::msg("Invalid NVMe namespace block size"))?
        };
        if !(512..=PAGE_SIZE as u64).contains(&block_size) || !block_size.is_power_of_two() {
            return Err(Error::msg("Unsupported NVMe namespace block size"));
        }
        Ok((blocks, block_size))
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit_queue(
        hal: &H,
        bar0: u64,
        sq: &DmaBuffer,
        cq: &DmaBuffer,
        queue_entries: u16,
        doorbell_stride: u64,
        queue: u16,
        sq_tail: &mut u16,
        cq_head: &mut u16,
        cq_phase: &mut bool,
        mut command: NvmeCommand,
    ) -> Result<()> {
        let cid = *sq_tail;
        command.cdw0 |= (cid as u32) << 16;
        unsafe {
            ptr::write_volatile((sq.virt as *mut NvmeCommand).add(cid as usize), command);
        }
        fence(Ordering::SeqCst);
        *sq_tail = (cid + 1) % queue_entries;

        hal.mmio_write32(
            bar0 + NVME_REG_DOORBELL + (2 * queue as u64) * doorbell_stride,
            *sq_tail as u32,
        );
        if queue != 0 {
            hal.yield_now().await;
        }

        let start = hal.micros();
        for _ in 0..MAX_POLL_YIELDS {
            let completion = unsafe {
                ptr::read_volatile((cq.virt as *const NvmeCompletion).add(*cq_head as usize))
            };
            if ((completion.status & 1) != 0) == *cq_phase {
                fence(Ordering::SeqCst);
                let status = completion.status;
                *cq_head += 1;
                if *cq_head >= queue_entries {
                    *cq_head = 0;
                    *cq_phase = !*cq_phase;
                }
                hal.mmio_write32(
                    bar0 + NVME_REG_DOORBELL + (2 * queue as u64 + 1) * doorbell_stride,
                    *cq_head as u32,
                );
                if (status & 0xFFFE) != 0 {
                    return Err(Error::msg("NVMe command completed with error status"));
                }
                return Ok(());
            }
            if hal.micros().saturating_sub(start) > COMMAND_TIMEOUT_MICROS {
                return Err(Error::msg("NVMe command timed out"));
            }
            hal.yield_now().await;
        }
        Err(Error::msg("NVMe command timed out"))
    }

    async fn submit_admin(&mut self, command: NvmeCommand) -> Result<()> {
        Self::submit_queue(
            &self.hal,
            self.bar0,
            &self.admin_sq,
            &self.admin_cq,
            self.queue_entries,
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
        Self::submit_queue(
            &self.hal,
            self.bar0,
            &self.io_sq,
            &self.io_cq,
            self.queue_entries,
            self.doorbell_stride,
            1,
            &mut self.io_sq_tail,
            &mut self.io_cq_head,
            &mut self.io_cq_phase,
            command,
        )
        .await
    }

    async fn transfer(
        &mut self,
        write: bool,
        lba: u64,
        blocks: u16,
        transfer_bytes: usize,
    ) -> Result<()> {
        let num_pages = transfer_bytes.div_ceil(PAGE_SIZE).max(1);
        let prp1 = self.data.phys;
        let prp2 = if num_pages == 1 {
            0
        } else if num_pages == 2 {
            self.data.phys + PAGE_SIZE as u64
        } else {
            let prp_entries = self.prp_list.virt as *mut u64;
            for i in 0..(num_pages - 1) {
                let page_phys = self.data.phys + ((i + 1) * PAGE_SIZE) as u64;
                unsafe {
                    ptr::write_volatile(prp_entries.add(i), page_phys);
                }
            }
            fence(Ordering::SeqCst);
            self.prp_list.phys
        };

        self.submit_io(NvmeCommand {
            cdw0: if write { 0x01 } else { 0x02 },
            nsid: self.namespace,
            prp1,
            prp2,
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: (blocks as u32) - 1,
            ..Default::default()
        })
        .await
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

    pub fn namespace(&self) -> u32 {
        self.namespace
    }

    pub fn max_transfer_bytes(&self) -> usize {
        self.max_transfer_bytes
    }

    /// Asynchronously reads `buf.len()` bytes starting at byte `offset` on the disk.
    /// Supports arbitrary byte offsets/lengths and multi-page transfers up to `max_transfer_bytes` (256 KiB) per command using PRP1 + PRP2 / PRP lists.
    pub async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| Error::msg("NVMe read offset overflow"))?;
        if end > self.size() {
            return Err(Error::msg("NVMe read out of bounds"));
        }

        let bs = self.block_size as usize;
        let max_bytes = self
            .max_transfer_bytes
            .min(NVME_MAX_DMA_PAGES * PAGE_SIZE)
            .max(bs);
        let max_blocks = (max_bytes / bs).max(1);

        let mut cur_offset = offset;
        let mut remaining = buf;
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = (in_block + remaining.len()).div_ceil(bs).min(max_blocks);
            let transfer_bytes = blocks * bs;

            self.transfer(false, lba, blocks as u16, transfer_bytes)
                .await?;
            let chunk_bytes = (transfer_bytes - in_block).min(remaining.len());
            let data_slice = unsafe { self.data.as_slice() };
            remaining[..chunk_bytes].copy_from_slice(&data_slice[in_block..in_block + chunk_bytes]);

            cur_offset += chunk_bytes as u64;
            remaining = &mut remaining[chunk_bytes..];
        }
        Ok(())
    }

    /// Asynchronously writes `buf.len()` bytes starting at byte `offset` on the disk.
    /// Handles unaligned head/tail blocks via read-modify-write and transfers up to `max_transfer_bytes` (256 KiB) per command.
    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| Error::msg("NVMe write offset overflow"))?;
        if end > self.size() {
            return Err(Error::msg("NVMe write out of bounds"));
        }

        let bs = self.block_size as usize;
        let max_bytes = self
            .max_transfer_bytes
            .min(NVME_MAX_DMA_PAGES * PAGE_SIZE)
            .max(bs);
        let max_blocks = (max_bytes / bs).max(1);

        let mut cur_offset = offset;
        let mut remaining = buf;
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = (in_block + remaining.len()).div_ceil(bs).min(max_blocks);
            let transfer_bytes = blocks * bs;
            let chunk_bytes = (transfer_bytes - in_block).min(remaining.len());

            if in_block != 0 || chunk_bytes < transfer_bytes {
                self.transfer(false, lba, blocks as u16, transfer_bytes)
                    .await?;
            }

            unsafe {
                self.data.as_mut_slice()[in_block..in_block + chunk_bytes]
                    .copy_from_slice(&remaining[..chunk_bytes]);
            }

            self.transfer(true, lba, blocks as u16, transfer_bytes)
                .await?;

            cur_offset += chunk_bytes as u64;
            remaining = &remaining[chunk_bytes..];
        }
        Ok(())
    }

    /// Issues an NVMe Flush command (`0x00`) on the I/O queue.
    pub async fn flush(&mut self) -> Result<()> {
        self.submit_io(NvmeCommand {
            cdw0: 0x00,
            nsid: self.namespace,
            ..Default::default()
        })
        .await
    }
}

impl<H: Hal> Drop for NvmeDisk<H> {
    fn drop(&mut self) {
        self.hal.free_dma(self.admin_sq);
        self.hal.free_dma(self.admin_cq);
        self.hal.free_dma(self.io_sq);
        self.hal.free_dma(self.io_cq);
        self.hal.free_dma(self.prp_list);
        self.hal.free_dma(self.data);
    }
}
