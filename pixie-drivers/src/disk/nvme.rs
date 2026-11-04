use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::disk::{BlockDeviceIo, read_bytes, write_bytes};
use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE, alloc_dma_buffers};
use crate::pci::PciDevice;

pub const NVME_QUEUE_ENTRIES: usize = 64;
pub const NVME_MAX_DMA_PAGES: usize = 64; // 256 KiB per I/O command via PRP list

const NVME_REG_CAP: u64 = 0x0000;
const NVME_REG_INTMS: u64 = 0x000C;
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

struct NvmeQueue {
    sq: DmaBuffer,
    cq: DmaBuffer,
    qid: u16,
    sq_tail: u16,
    cq_head: u16,
    cq_phase: bool,
    next_cid: u16,
}

impl NvmeQueue {
    fn new(sq: DmaBuffer, cq: DmaBuffer, qid: u16) -> Self {
        Self {
            sq,
            cq,
            qid,
            sq_tail: 0,
            cq_head: 0,
            cq_phase: true,
            next_cid: 0,
        }
    }
}

pub struct NvmeDisk<H: Hal> {
    pub pci: PciDevice,
    pub bar0: u64,
    pub hal: H,
    admin_q: NvmeQueue,
    io_q: NvmeQueue,
    prp_list: DmaBuffer,
    data: DmaBuffer,
    queue_entries: u16,
    namespace: u32,
    block_size: u64,
    blocks: u64,
    max_transfer_bytes: usize,
    doorbell_stride: u64,
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

        let [admin_sq, admin_cq, io_sq, io_cq, prp_list, data] = alloc_dma_buffers(
            &hal,
            [1, 1, 1, 1, 1, NVME_MAX_DMA_PAGES],
            "Out of DMA memory for NVMe",
        )?;

        let mut disk = Self {
            pci,
            bar0,
            hal,
            admin_q: NvmeQueue::new(admin_sq, admin_cq, 0),
            io_q: NvmeQueue::new(io_sq, io_cq, 1),
            prp_list,
            data,
            queue_entries: NVME_QUEUE_ENTRIES as u16,
            namespace: 0,
            block_size: 512,
            blocks: 0,
            max_transfer_bytes: NVME_MAX_DMA_PAGES * PAGE_SIZE,
            doorbell_stride: 4,
        };
        disk.initialize().await?;
        Ok(disk)
    }

    async fn wait_ready(&self, wanted: bool, timeout_us: i64) -> Result<()> {
        let start = self.hal.micros();
        for _ in 0..MAX_POLL_YIELDS {
            let csts = self.read32(NVME_REG_CSTS);
            if (csts & 0x02) != 0 {
                return Err(Error::msg("NVMe CSTS.CFS fatal error"));
            }
            if ((csts & 0x01) != 0) == wanted {
                return Ok(());
            }
            if self.hal.micros().saturating_sub(start) > timeout_us {
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

        let cap_to_us = (((cap_lo >> 24) & 0xFF) as i64) * 500_000;
        let ready_timeout_us = cap_to_us.max(COMMAND_TIMEOUT_MICROS);

        // Mask all legacy/MSI pin interrupts since this driver uses pure CQ polling.
        self.write32(NVME_REG_INTMS, 0xFFFF_FFFF);

        self.write32(NVME_REG_CC, 0);
        self.wait_ready(false, ready_timeout_us).await?;

        let qm1 = (self.queue_entries as u32) - 1;
        let aqa = (qm1 << 16) | qm1;
        self.write32(NVME_REG_AQA, aqa);
        self.write32(NVME_REG_ASQ, self.admin_q.sq.phys as u32);
        self.write32(NVME_REG_ASQ + 4, (self.admin_q.sq.phys >> 32) as u32);
        self.write32(NVME_REG_ACQ, self.admin_q.cq.phys as u32);
        self.write32(NVME_REG_ACQ + 4, (self.admin_q.cq.phys >> 32) as u32);

        self.write32(NVME_REG_CC, (4 << 20) | (6 << 16) | 1);
        self.wait_ready(true, ready_timeout_us).await?;

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
            prp1: self.io_q.cq.phys,
            cdw10: (qm1 << 16) | 1,
            cdw11: 1, // Physically contiguous (PC=1)
            ..Default::default()
        })
        .await?;

        // Create I/O Submission Queue (QID = 1, CQID = 1)
        self.submit_admin(NvmeCommand {
            cdw0: 0x01,
            prp1: self.io_q.sq.phys,
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

    async fn submit_queue(
        hal: &H,
        bar0: u64,
        queue_entries: u16,
        doorbell_stride: u64,
        q: &mut NvmeQueue,
        mut command: NvmeCommand,
    ) -> Result<()> {
        let cid = q.next_cid;
        q.next_cid = q.next_cid.wrapping_add(1);
        command.cdw0 = (command.cdw0 & 0xFFFF) | ((cid as u32) << 16);

        let slot = q.sq_tail as usize;
        unsafe {
            ptr::write_volatile((q.sq.virt as *mut NvmeCommand).add(slot), command);
        }
        fence(Ordering::SeqCst);
        q.sq_tail = (q.sq_tail + 1) % queue_entries;

        hal.mmio_write32(
            bar0 + NVME_REG_DOORBELL + (2 * q.qid as u64) * doorbell_stride,
            q.sq_tail as u32,
        );

        let cq_db = bar0 + NVME_REG_DOORBELL + (2 * q.qid as u64 + 1) * doorbell_stride;
        let start = hal.micros();
        for _ in 0..MAX_POLL_YIELDS {
            let mut drained_stale = false;
            loop {
                let completion = unsafe {
                    ptr::read_volatile((q.cq.virt as *const NvmeCompletion).add(q.cq_head as usize))
                };
                if ((completion.status & 1) != 0) != q.cq_phase {
                    break;
                }
                fence(Ordering::Acquire);
                q.cq_head += 1;
                if q.cq_head >= queue_entries {
                    q.cq_head = 0;
                    q.cq_phase = !q.cq_phase;
                }
                if completion.cid == cid {
                    hal.mmio_write32(cq_db, q.cq_head as u32);
                    if (completion.status & 0xFFFE) != 0 {
                        return Err(Error::msg("NVMe command completed with error status"));
                    }
                    return Ok(());
                }
                drained_stale = true;
            }
            if drained_stale {
                hal.mmio_write32(cq_db, q.cq_head as u32);
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
            self.queue_entries,
            self.doorbell_stride,
            &mut self.admin_q,
            command,
        )
        .await
    }

    async fn submit_io(&mut self, command: NvmeCommand) -> Result<()> {
        Self::submit_queue(
            &self.hal,
            self.bar0,
            self.queue_entries,
            self.doorbell_stride,
            &mut self.io_q,
            command,
        )
        .await
    }

    async fn transfer(
        &mut self,
        write: bool,
        lba: u64,
        blocks: u32,
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
            cdw12: blocks - 1,
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
        read_bytes(
            self,
            offset,
            buf,
            "NVMe read offset overflow",
            "NVMe read out of bounds",
        )
        .await
    }

    /// Asynchronously writes `buf.len()` bytes starting at byte `offset` on the disk.
    /// Handles unaligned head/tail blocks via read-modify-write and transfers up to `max_transfer_bytes` (256 KiB) per command.
    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        write_bytes(
            self,
            offset,
            buf,
            "NVMe write offset overflow",
            "NVMe write out of bounds",
        )
        .await
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

impl<H: Hal> BlockDeviceIo for NvmeDisk<H> {
    fn total_bytes(&self) -> u64 {
        self.size()
    }

    fn block_bytes(&self) -> usize {
        self.block_size as usize
    }

    fn max_transfer_blocks(&self) -> usize {
        let bs = self.block_size as usize;
        let max_bytes = self
            .max_transfer_bytes
            .min(NVME_MAX_DMA_PAGES * PAGE_SIZE)
            .max(bs);
        (max_bytes / bs).clamp(1, 65536)
    }

    fn dma_slice(&self) -> &[u8] {
        unsafe { self.data.as_slice() }
    }

    fn dma_mut_slice(&mut self) -> &mut [u8] {
        unsafe { self.data.as_mut_slice() }
    }

    async fn transfer_blocks(
        &mut self,
        write: bool,
        lba: u64,
        blocks: u32,
        transfer_bytes: usize,
    ) -> Result<()> {
        self.transfer(write, lba, blocks, transfer_bytes).await
    }
}

impl<H: Hal> Drop for NvmeDisk<H> {
    fn drop(&mut self) {
        // Clear CC.EN so the controller stops DMA into SQ/CQ pages before freeing them.
        if self.read32(NVME_REG_CSTS) != 0xFFFF_FFFF {
            self.write32(NVME_REG_CC, 0);
            let start = self.hal.micros();
            for _ in 0..2_000 {
                if (self.read32(NVME_REG_CSTS) & 0x01) == 0 {
                    break;
                }
                if self.hal.micros().saturating_sub(start) > 50_000 {
                    break;
                }
            }
        }
        fence(Ordering::SeqCst);
        self.hal.free_dma(self.admin_q.sq);
        self.hal.free_dma(self.admin_q.cq);
        self.hal.free_dma(self.io_q.sq);
        self.hal.free_dma(self.io_q.cq);
        self.hal.free_dma(self.prp_list);
        self.hal.free_dma(self.data);
    }
}
