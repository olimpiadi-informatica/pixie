mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{DmaPool, block_on};
use pixie_drivers::disk::nvme::{NVME_QUEUE_ENTRIES, NvmeDisk};
use pixie_drivers::hal::{DmaBuffer, Hal, PAGE_SIZE};
use pixie_drivers::pci::PciDevice;

const BAR0: u64 = 0xFEB0_0000;
const DMA_PHYS_BASE: u64 = 0x1000_0000;
const DMA_POOL_PAGES: usize = 4096; // 16 MiB

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cns2Behavior {
    Normal,
    RejectStatus,
    ReturnAllZeros,
}

#[derive(Clone, Copy, Debug)]
struct PendingCqe {
    cq_base: u64,
    cq_size: usize,
    is_admin: bool,
    dw0: u32,
    sq_id: u16,
    sq_head: u16,
    cid: u16,
    status: u16,
    remaining_yields: usize,
}

struct NvmeSimState {
    dma: DmaPool,
    disk_storage: Vec<u8>,
    block_size: u32,
    num_blocks: u64,
    active_nsid: u32,
    nn: u32,
    mdts: u8,
    dstrd: u8,
    cns2_behavior: Cns2Behavior,
    delay_cqe_yields: usize,

    // MMIO register state
    cc: u32,
    csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    intms: u32,
    intmc: u32,

    // Queue tracking
    asq_head: u16,
    acq_tail: usize,
    acq_head: u16,
    acq_phase: u16,

    iosq_base: u64,
    iosq_size: usize,
    iosq_head: u16,

    iocq_base: u64,
    iocq_size: usize,
    iocq_tail: usize,
    iocq_head: u16,
    iocq_phase: u16,
    iocq_phase_flips: usize,

    pending_cqes: Vec<PendingCqe>,

    // Diagnostics / assertions
    doorbell_offsets_written: Vec<u64>,
    identify_cns_seen: Vec<u8>,
    saw_single_page_prp: bool,
    saw_two_page_direct_prp2: bool,
    saw_prp_list: bool,
    max_pages_per_cmd: usize,
    flush_count: usize,
    yield_count: usize,
    time_us: i64,

    // PCI config space
    pci_addr_latch: u32,
    pci_command_reg: u16,
}

impl NvmeSimState {
    fn new(
        num_blocks: u64,
        dstrd: u8,
        cns2_behavior: Cns2Behavior,
        delay_cqe_yields: usize,
    ) -> Self {
        let block_size = 512u32;
        Self {
            dma: DmaPool::new(DMA_PHYS_BASE, DMA_POOL_PAGES),
            disk_storage: vec![0u8; (num_blocks as usize) * (block_size as usize)],
            block_size,
            num_blocks,
            active_nsid: 1,
            nn: 1,
            mdts: 6, // 2^6 * 4 KiB = 256 KiB
            dstrd,
            cns2_behavior,
            delay_cqe_yields,

            cc: 0,
            csts: 0,
            aqa: 0,
            asq: 0,
            acq: 0,
            intms: 0,
            intmc: 0,

            asq_head: 0,
            acq_tail: 0,
            acq_head: 0,
            acq_phase: 1,

            iosq_base: 0,
            iosq_size: NVME_QUEUE_ENTRIES,
            iosq_head: 0,

            iocq_base: 0,
            iocq_size: NVME_QUEUE_ENTRIES,
            iocq_tail: 0,
            iocq_head: 0,
            iocq_phase: 1,
            iocq_phase_flips: 0,

            pending_cqes: Vec::new(),

            doorbell_offsets_written: Vec::new(),
            identify_cns_seen: Vec::new(),
            saw_single_page_prp: false,
            saw_two_page_direct_prp2: false,
            saw_prp_list: false,
            max_pages_per_cmd: 0,
            flush_count: 0,
            yield_count: 0,
            time_us: 1_000,

            pci_addr_latch: 0,
            pci_command_reg: 0,
        }
    }

    fn cap_reg(&self) -> u64 {
        let mqes = (NVME_QUEUE_ENTRIES as u64 - 1) & 0xFFFF;
        let cqr = 1u64 << 16;
        let to = 20u64 << 24;
        let dstrd = ((self.dstrd as u64) & 0xF) << 32;
        let css = 1u64 << 37;
        mqes | cqr | to | dstrd | css
    }

    fn read_u64_phys(&self, phys: u64) -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(self.dma.slice(phys, 8));
        u64::from_le_bytes(buf)
    }

    fn commit_cqe(&mut self, cqe: PendingCqe) {
        let (tail, phase) = if cqe.is_admin {
            (self.acq_tail, self.acq_phase)
        } else {
            (self.iocq_tail, self.iocq_phase)
        };
        let entry_phys = cqe.cq_base + (tail * 16) as u64;
        let dw2 = ((cqe.sq_id as u32) << 16) | (cqe.sq_head as u32);
        let dw3 = ((cqe.status as u32) << 17) | ((phase as u32) << 16) | (cqe.cid as u32);

        let slot = self.dma.slice_mut(entry_phys, 16);
        slot[0..4].copy_from_slice(&cqe.dw0.to_le_bytes());
        slot[4..8].copy_from_slice(&0u32.to_le_bytes());
        slot[8..12].copy_from_slice(&dw2.to_le_bytes());
        slot[12..16].copy_from_slice(&dw3.to_le_bytes());

        if cqe.is_admin {
            self.acq_tail += 1;
            if self.acq_tail >= cqe.cq_size {
                self.acq_tail = 0;
                self.acq_phase ^= 1;
            }
        } else {
            self.iocq_tail += 1;
            if self.iocq_tail >= cqe.cq_size {
                self.iocq_tail = 0;
                self.iocq_phase ^= 1;
                self.iocq_phase_flips += 1;
            }
        }
    }

    fn inject_stale_io_cqe(&mut self, stale_cid: u16, status: u16) {
        let cqe = PendingCqe {
            cq_base: self.iocq_base,
            cq_size: self.iocq_size,
            is_admin: false,
            dw0: 0,
            sq_id: 1,
            sq_head: self.iosq_head,
            cid: stale_cid,
            status,
            remaining_yields: 0,
        };
        self.commit_cqe(cqe);
    }

    fn enqueue_or_commit_cqe(&mut self, cqe: PendingCqe) {
        if cqe.remaining_yields == 0 {
            self.commit_cqe(cqe);
        } else {
            self.pending_cqes.push(cqe);
        }
    }

    fn advance_pending_cqes(&mut self) {
        let mut ready = Vec::new();
        for item in &mut self.pending_cqes {
            if item.remaining_yields > 0 {
                item.remaining_yields -= 1;
            }
            if item.remaining_yields == 0 {
                ready.push(*item);
            }
        }
        self.pending_cqes.retain(|item| item.remaining_yields > 0);
        for cqe in ready {
            self.commit_cqe(cqe);
        }
    }

    fn process_admin_sq(&mut self, new_tail: u16) {
        assert_eq!(self.asq % PAGE_SIZE as u64, 0, "ASQ must be 4 KiB-aligned");
        assert_eq!(self.acq % PAGE_SIZE as u64, 0, "ACQ must be 4 KiB-aligned");
        let asq_size = ((self.aqa & 0xFFF) as usize) + 1;
        let acq_size = (((self.aqa >> 16) & 0xFFF) as usize) + 1;

        while self.asq_head != new_tail {
            let idx = self.asq_head as usize;
            self.asq_head = ((idx + 1) % asq_size) as u16;

            let sqe_phys = self.asq + (idx * 64) as u64;
            let mut sqe = [0u8; 64];
            sqe.copy_from_slice(self.dma.slice(sqe_phys, 64));

            let cdw0 = u32::from_le_bytes(sqe[0..4].try_into().unwrap());
            let opcode = (cdw0 & 0xFF) as u8;
            let cid = ((cdw0 >> 16) & 0xFFFF) as u16;
            let nsid = u32::from_le_bytes(sqe[4..8].try_into().unwrap());
            let prp1 = u64::from_le_bytes(sqe[24..32].try_into().unwrap());
            let cdw10 = u32::from_le_bytes(sqe[40..44].try_into().unwrap());
            let cdw11 = u32::from_le_bytes(sqe[44..48].try_into().unwrap());

            let mut status = 0u16;
            let mut dw0 = 0u32;

            match opcode {
                // Delete I/O SQ (0x00) or Delete I/O CQ (0x04)
                0x00 | 0x04 => {}
                // Create I/O SQ (0x01)
                0x01 => {
                    assert_eq!(
                        prp1 % PAGE_SIZE as u64,
                        0,
                        "IOSQ PRP1 must be 4 KiB-aligned"
                    );
                    let qsize = ((cdw10 >> 16) & 0xFFFF) as usize + 1;
                    assert!(
                        (cdw11 & 1) != 0,
                        "Create I/O SQ must set PC (Physically Contiguous)"
                    );
                    self.iosq_base = prp1;
                    self.iosq_size = qsize;
                    self.iosq_head = 0;
                }
                // Create I/O CQ (0x05)
                0x05 => {
                    assert_eq!(
                        prp1 % PAGE_SIZE as u64,
                        0,
                        "IOCQ PRP1 must be 4 KiB-aligned"
                    );
                    let qsize = ((cdw10 >> 16) & 0xFFFF) as usize + 1;
                    assert!(
                        (cdw11 & 1) != 0,
                        "Create I/O CQ must set PC (Physically Contiguous)"
                    );
                    self.iocq_base = prp1;
                    self.iocq_size = qsize;
                    self.iocq_tail = 0;
                    self.iocq_head = 0;
                    self.iocq_phase = 1;
                }
                // Identify (0x06)
                0x06 => {
                    assert_eq!(
                        prp1 % PAGE_SIZE as u64,
                        0,
                        "Identify PRP1 must be 4 KiB-aligned"
                    );
                    let cns = (cdw10 & 0xFF) as u8;
                    self.identify_cns_seen.push(cns);
                    match cns {
                        // Identify Namespace
                        0x00 => {
                            let num_blocks = self.num_blocks;
                            let lbads = self.block_size.trailing_zeros() as u8;
                            let active_nsid = self.active_nsid;
                            let buf = self.dma.slice_mut(prp1, PAGE_SIZE);
                            buf.fill(0);
                            if nsid == active_nsid {
                                buf[0..8].copy_from_slice(&num_blocks.to_le_bytes()); // NSZE
                                buf[8..16].copy_from_slice(&num_blocks.to_le_bytes()); // NCAP
                                buf[16..24].copy_from_slice(&num_blocks.to_le_bytes()); // NUSE
                                buf[26] = 0; // FLBAS -> format 0
                                buf[130] = lbads; // LBAF0.LBADS = 9 (512B)
                            }
                        }
                        // Identify Controller
                        0x01 => {
                            let mdts = self.mdts;
                            let nn = self.nn;
                            let buf = self.dma.slice_mut(prp1, PAGE_SIZE);
                            buf.fill(0);
                            buf[4..24].copy_from_slice(b"PIXIE-NVME-ADV-00001");
                            buf[24..44].copy_from_slice(b"PIXIE NVME SIMULATOR");
                            buf[77] = mdts;
                            buf[516..520].copy_from_slice(&nn.to_le_bytes());
                        }
                        // Active Namespace ID list
                        0x02 => match self.cns2_behavior {
                            Cns2Behavior::Normal => {
                                let active_nsid = self.active_nsid;
                                let buf = self.dma.slice_mut(prp1, PAGE_SIZE);
                                buf.fill(0);
                                buf[0..4].copy_from_slice(&active_nsid.to_le_bytes());
                            }
                            Cns2Behavior::RejectStatus => {
                                status = 0x0002; // Invalid Field in Command
                            }
                            Cns2Behavior::ReturnAllZeros => {
                                let buf = self.dma.slice_mut(prp1, PAGE_SIZE);
                                buf.fill(0);
                            }
                        },
                        _ => {
                            status = 0x0002;
                        }
                    }
                }
                // Set Features (0x09) / Get Features (0x08)
                0x08 | 0x09 => {
                    dw0 = 0x0001_0001;
                }
                _ => {
                    status = 0x0001; // Invalid Command Opcode
                }
            }

            let cqe = PendingCqe {
                cq_base: self.acq,
                cq_size: acq_size,
                is_admin: true,
                dw0,
                sq_id: 0,
                sq_head: self.asq_head,
                cid,
                status,
                remaining_yields: self.delay_cqe_yields,
            };
            self.enqueue_or_commit_cqe(cqe);
        }
    }

    fn resolve_prp_pages(&mut self, prp1: u64, prp2: u64, total_bytes: usize) -> Vec<(u64, usize)> {
        assert_eq!(
            prp1 % PAGE_SIZE as u64,
            0,
            "I/O PRP1 {:#x} must be 4 KiB-aligned",
            prp1
        );
        let num_pages = total_bytes.div_ceil(PAGE_SIZE);
        assert!(num_pages >= 1, "Transfer must span at least 1 page");
        self.max_pages_per_cmd = self.max_pages_per_cmd.max(num_pages);

        let mut pages = Vec::with_capacity(num_pages);
        let first_len = total_bytes.min(PAGE_SIZE);
        pages.push((prp1, first_len));

        if num_pages == 1 {
            self.saw_single_page_prp = true;
            return pages;
        }

        if num_pages == 2 {
            assert_ne!(prp2, 0, "2-page transfer requires non-zero PRP2");
            assert_eq!(
                prp2 % PAGE_SIZE as u64,
                0,
                "Direct PRP2 {:#x} must be 4 KiB-aligned",
                prp2
            );
            self.saw_two_page_direct_prp2 = true;
            pages.push((prp2, total_bytes - PAGE_SIZE));
            return pages;
        }

        // 3 or more pages: PRP2 points to a PRP list of (num_pages - 1) u64 physical page addresses
        assert_ne!(
            prp2, 0,
            "Multi-page transfer requires non-zero PRP2 list pointer"
        );
        assert_eq!(
            prp2 % PAGE_SIZE as u64,
            0,
            "PRP list pointer {:#x} must be 4 KiB-aligned",
            prp2
        );
        self.saw_prp_list = true;

        let mut remaining = total_bytes - PAGE_SIZE;
        for i in 0..(num_pages - 1) {
            let entry_phys = self.read_u64_phys(prp2 + (i * 8) as u64);
            assert_ne!(entry_phys, 0, "PRP list entry {} is zero", i);
            assert_eq!(
                entry_phys % PAGE_SIZE as u64,
                0,
                "PRP list entry {} ({:#x}) must be 4 KiB-aligned",
                i,
                entry_phys
            );
            let chunk = remaining.min(PAGE_SIZE);
            pages.push((entry_phys, chunk));
            remaining -= chunk;
        }
        pages
    }

    fn process_io_sq(&mut self, new_tail: u16) {
        let iosq_size = self.iosq_size;
        let iocq_base = self.iocq_base;
        let iocq_size = self.iocq_size;

        while self.iosq_head != new_tail {
            let idx = self.iosq_head as usize;
            self.iosq_head = ((idx + 1) % iosq_size) as u16;

            let sqe_phys = self.iosq_base + (idx * 64) as u64;
            let mut sqe = [0u8; 64];
            sqe.copy_from_slice(self.dma.slice(sqe_phys, 64));

            let cdw0 = u32::from_le_bytes(sqe[0..4].try_into().unwrap());
            let opcode = (cdw0 & 0xFF) as u8;
            let cid = ((cdw0 >> 16) & 0xFFFF) as u16;
            let nsid = u32::from_le_bytes(sqe[4..8].try_into().unwrap());
            let prp1 = u64::from_le_bytes(sqe[24..32].try_into().unwrap());
            let prp2 = u64::from_le_bytes(sqe[32..40].try_into().unwrap());
            let cdw10 = u32::from_le_bytes(sqe[40..44].try_into().unwrap());
            let cdw11 = u32::from_le_bytes(sqe[44..48].try_into().unwrap());
            let cdw12 = u32::from_le_bytes(sqe[48..52].try_into().unwrap());

            assert_eq!(
                nsid, self.active_nsid,
                "I/O command sent to unexpected NSID"
            );
            let mut status = 0u16;

            match opcode {
                // Flush (0x00)
                0x00 => {
                    self.flush_count += 1;
                }
                // Write (0x01)
                0x01 => {
                    let slba = (cdw10 as u64) | ((cdw11 as u64) << 32);
                    let nlb = ((cdw12 & 0xFFFF) as usize) + 1;
                    let total_bytes = nlb * (self.block_size as usize);
                    let disk_offset = (slba as usize) * (self.block_size as usize);
                    let pages = self.resolve_prp_pages(prp1, prp2, total_bytes);

                    let mut cur = disk_offset;
                    for (page_phys, len) in pages {
                        let mut tmp = vec![0u8; len];
                        tmp.copy_from_slice(self.dma.slice(page_phys, len));
                        self.disk_storage[cur..cur + len].copy_from_slice(&tmp);
                        cur += len;
                    }
                }
                // Read (0x02)
                0x02 => {
                    let slba = (cdw10 as u64) | ((cdw11 as u64) << 32);
                    let nlb = ((cdw12 & 0xFFFF) as usize) + 1;
                    let total_bytes = nlb * (self.block_size as usize);
                    let disk_offset = (slba as usize) * (self.block_size as usize);
                    let pages = self.resolve_prp_pages(prp1, prp2, total_bytes);

                    let mut cur = disk_offset;
                    for (page_phys, len) in pages {
                        let tmp = self.disk_storage[cur..cur + len].to_vec();
                        self.dma.slice_mut(page_phys, len).copy_from_slice(&tmp);
                        cur += len;
                    }
                }
                _ => {
                    status = 0x0001;
                }
            }

            let cqe = PendingCqe {
                cq_base: iocq_base,
                cq_size: iocq_size,
                is_admin: false,
                dw0: 0,
                sq_id: 1,
                sq_head: self.iosq_head,
                cid,
                status,
                remaining_yields: self.delay_cqe_yields,
            };
            self.enqueue_or_commit_cqe(cqe);
        }
    }

    fn mmio_read32(&self, addr: u64) -> u32 {
        let off = addr.checked_sub(BAR0).expect("MMIO read below BAR0");
        match off {
            0x0000 => (self.cap_reg() & 0xFFFF_FFFF) as u32,
            0x0004 => (self.cap_reg() >> 32) as u32,
            0x0008 => 0x0001_0400, // VS = 1.4
            0x000C => self.intms,
            0x0010 => self.intmc,
            0x0014 => self.cc,
            0x001C => self.csts,
            0x0024 => self.aqa,
            0x0028 => (self.asq & 0xFFFF_FFFF) as u32,
            0x002C => (self.asq >> 32) as u32,
            0x0030 => (self.acq & 0xFFFF_FFFF) as u32,
            0x0034 => (self.acq >> 32) as u32,
            _ => 0,
        }
    }

    fn mmio_write32(&mut self, addr: u64, val: u32) {
        let off = addr.checked_sub(BAR0).expect("MMIO write below BAR0");
        match off {
            0x000C => self.intms |= val,
            0x0010 => self.intmc |= val,
            0x0014 => {
                self.cc = val;
                if (val & 1) == 0 {
                    // Controller reset / disable
                    self.csts = 0;
                    self.asq_head = 0;
                    self.acq_tail = 0;
                    self.acq_head = 0;
                    self.acq_phase = 1;
                } else {
                    // Controller enabled
                    self.csts = 1;
                }
            }
            0x0024 => self.aqa = val,
            0x0028 => self.asq = (self.asq & 0xFFFF_FFFF_0000_0000) | (val as u64),
            0x002C => self.asq = (self.asq & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32),
            0x0030 => self.acq = (self.acq & 0xFFFF_FFFF_0000_0000) | (val as u64),
            0x0034 => self.acq = (self.acq & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32),
            db if db >= 0x1000 => {
                self.doorbell_offsets_written.push(db);
                let stride = 4u64 << (self.dstrd as u64);
                let rel = db - 0x1000;
                assert_eq!(
                    rel % stride,
                    0,
                    "Doorbell write at offset {:#x} violates DSTRD={} (stride={})",
                    db,
                    self.dstrd,
                    stride
                );
                let db_idx = rel / stride;
                match db_idx {
                    0 => self.process_admin_sq((val & 0xFFFF) as u16),
                    1 => self.acq_head = (val & 0xFFFF) as u16,
                    2 => self.process_io_sq((val & 0xFFFF) as u16),
                    3 => self.iocq_head = (val & 0xFFFF) as u16,
                    _ => panic!("Unexpected doorbell index {} at offset {:#x}", db_idx, db),
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
struct MockNvmeHal {
    state: Arc<Mutex<NvmeSimState>>,
}

impl MockNvmeHal {
    fn new(
        num_blocks: u64,
        dstrd: u8,
        cns2_behavior: Cns2Behavior,
        delay_cqe_yields: usize,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(NvmeSimState::new(
                num_blocks,
                dstrd,
                cns2_behavior,
                delay_cqe_yields,
            ))),
        }
    }
}

impl Hal for MockNvmeHal {
    fn mmio_read8(&self, addr: u64) -> u8 {
        let aligned = addr & !3;
        let shift = ((addr & 3) * 8) as u32;
        ((self.mmio_read32(aligned) >> shift) & 0xFF) as u8
    }

    fn mmio_write8(&self, _addr: u64, _val: u8) {}

    fn mmio_read16(&self, addr: u64) -> u16 {
        let aligned = addr & !3;
        let shift = ((addr & 2) * 8) as u32;
        ((self.mmio_read32(aligned) >> shift) & 0xFFFF) as u16
    }

    fn mmio_write16(&self, _addr: u64, _val: u16) {}

    fn mmio_read32(&self, addr: u64) -> u32 {
        self.state.lock().unwrap().mmio_read32(addr)
    }

    fn mmio_write32(&self, addr: u64, val: u32) {
        self.state.lock().unwrap().mmio_write32(addr, val);
    }

    fn mmio_read64(&self, addr: u64) -> u64 {
        let st = self.state.lock().unwrap();
        let lo = st.mmio_read32(addr) as u64;
        let hi = st.mmio_read32(addr + 4) as u64;
        lo | (hi << 32)
    }

    fn mmio_write64(&self, addr: u64, val: u64) {
        let mut st = self.state.lock().unwrap();
        st.mmio_write32(addr, (val & 0xFFFF_FFFF) as u32);
        st.mmio_write32(addr + 4, (val >> 32) as u32);
    }

    fn io_read8(&self, _port: u16) -> u8 {
        0
    }
    fn io_write8(&self, _port: u16, _val: u8) {}
    fn io_read16(&self, _port: u16) -> u16 {
        0
    }
    fn io_write16(&self, _port: u16, _val: u16) {}

    fn io_read32(&self, port: u16) -> u32 {
        let st = self.state.lock().unwrap();
        if port == 0xCFC && (st.pci_addr_latch & 0xFC) == 0x04 {
            return st.pci_command_reg as u32;
        }
        0
    }

    fn io_write32(&self, port: u16, val: u32) {
        let mut st = self.state.lock().unwrap();
        if port == 0xCF8 {
            st.pci_addr_latch = val;
        } else if port == 0xCFC && (st.pci_addr_latch & 0xFC) == 0x04 {
            st.pci_command_reg = (val & 0xFFFF) as u16;
        }
    }

    fn alloc_dma(&self, pages: usize) -> Option<DmaBuffer> {
        self.state.lock().unwrap().dma.alloc(pages)
    }

    fn free_dma(&self, buf: DmaBuffer) {
        let mut st = self.state.lock().unwrap();
        assert_eq!(
            st.cc & 1,
            0,
            "NvmeDisk must clear CC.EN before freeing DMA buffers"
        );
        st.dma.free(buf);
    }

    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        self.state.lock().unwrap().dma.phys_to_virt(phys)
    }

    fn micros(&self) -> i64 {
        let mut st = self.state.lock().unwrap();
        st.time_us += 10;
        st.time_us
    }

    async fn yield_now(&self) {
        let mut st = self.state.lock().unwrap();
        st.yield_count += 1;
        st.time_us += 50;
        st.advance_pending_cqes();
    }

    async fn sleep(&self, duration: Duration) {
        let mut st = self.state.lock().unwrap();
        st.yield_count += 1;
        st.time_us += duration.as_micros() as i64;
        st.advance_pending_cqes();
    }
}

fn dummy_nvme_pci() -> PciDevice {
    PciDevice {
        bus: 0,
        dev: 4,
        func: 0,
        vendor_id: 0x1B36,
        device_id: 0x0010,
        class_code: 0x01,
        subclass: 0x08,
        prog_if: 0x02,
    }
}

#[test]
fn test_nvme_multi_page_prp_read_write_and_flush() {
    // 8192 blocks = 4 MiB disk
    let hal = MockNvmeHal::new(8192, 0, Cns2Behavior::Normal, 0);
    let mut disk = block_on(NvmeDisk::open(dummy_nvme_pci(), BAR0, hal.clone()))
        .expect("NvmeDisk::open should succeed");

    assert_eq!(disk.block_size(), 512);
    assert_eq!(disk.num_blocks(), 8192);
    assert_eq!(disk.size(), 8192 * 512);
    assert_eq!(disk.namespace(), 1);
    assert!(disk.max_transfer_bytes() >= 16 * 1024);

    // 1. 4 KiB (1 page -> PRP1 only)
    let pat_4k: Vec<u8> = (0..4096).map(|i| ((i * 7 + 3) & 0xFF) as u8).collect();
    block_on(disk.write(0, &pat_4k)).expect("4 KiB write should succeed");
    let mut read_4k = vec![0u8; 4096];
    block_on(disk.read(0, &mut read_4k)).expect("4 KiB read should succeed");
    assert_eq!(read_4k, pat_4k);

    // 2. 8 KiB (2 pages -> PRP1 + direct PRP2)
    let pat_8k: Vec<u8> = (0..8192).map(|i| ((i * 13 + 9) & 0xFF) as u8).collect();
    block_on(disk.write(16 * 1024, &pat_8k)).expect("8 KiB write should succeed");
    let mut read_8k = vec![0u8; 8192];
    block_on(disk.read(16 * 1024, &mut read_8k)).expect("8 KiB read should succeed");
    assert_eq!(read_8k, pat_8k);

    // 3. 16 KiB (4 pages -> PRP1 + PRP2 PRP list of 3 page pointers)
    let pat_16k: Vec<u8> = (0..16384).map(|i| ((i * 31 + 17) & 0xFF) as u8).collect();
    block_on(disk.write(64 * 1024, &pat_16k)).expect("16 KiB write should succeed");
    let mut read_16k = vec![0u8; 16384];
    block_on(disk.read(64 * 1024, &mut read_16k)).expect("16 KiB read should succeed");
    assert_eq!(read_16k, pat_16k);

    // 4. 1 MiB transfer (multiple 256 KiB / 64-page commands with 63-entry PRP lists)
    let one_mib = 1024 * 1024;
    let pat_1m: Vec<u8> = (0..one_mib)
        .map(|i| ((i ^ (i >> 8) ^ 0x5A) & 0xFF) as u8)
        .collect();
    block_on(disk.write(512 * 1024, &pat_1m)).expect("1 MiB write should succeed");
    let mut read_1m = vec![0u8; one_mib];
    block_on(disk.read(512 * 1024, &mut read_1m)).expect("1 MiB read should succeed");
    assert_eq!(read_1m, pat_1m);

    // 5. Unaligned byte offset and length read-modify-write
    let unaligned_off = 512 * 1024 + 137;
    let unaligned_data: Vec<u8> = (0..5000).map(|i| ((i * 19 + 0xA5) & 0xFF) as u8).collect();
    block_on(disk.write(unaligned_off, &unaligned_data)).expect("Unaligned write should succeed");
    let mut unaligned_read = vec![0u8; unaligned_data.len()];
    block_on(disk.read(unaligned_off, &mut unaligned_read)).expect("Unaligned read should succeed");
    assert_eq!(unaligned_read, unaligned_data);

    // Verify surrounding bytes in the 1 MiB region were preserved by read-modify-write
    let mut check_prefix = vec![0u8; 137];
    block_on(disk.read(512 * 1024, &mut check_prefix)).unwrap();
    assert_eq!(&check_prefix[..], &pat_1m[..137]);

    // 6. Stale CQE draining: inject a stale CQE with mismatched CID (0xBEEF, non-zero status)
    // and verify the next I/O command skips the stale CQE and matches its own CID cleanly.
    hal.state
        .lock()
        .unwrap()
        .inject_stale_io_cqe(0xBEEF, 0x0007);
    let mut post_stale_read = vec![0u8; 4096];
    block_on(disk.read(0, &mut post_stale_read))
        .expect("Read after stale CQE should skip mismatched CID and succeed");
    assert_eq!(post_stale_read, pat_4k);

    // 7. Flush
    block_on(disk.flush()).expect("Flush should succeed");

    {
        let st = hal.state.lock().unwrap();
        assert_eq!(
            st.intms, 0xFFFF_FFFF,
            "NvmeDisk::open must mask interrupts via REG_INTMS"
        );
        assert!(st.saw_single_page_prp, "Expected 1-page PRP1 transfer");
        assert!(
            st.saw_two_page_direct_prp2,
            "Expected 2-page direct PRP2 transfer"
        );
        assert!(st.saw_prp_list, "Expected >=3-page PRP list transfer");
        assert!(
            st.max_pages_per_cmd >= 4,
            "Expected multi-page PRP list transfer of at least 4 pages, got {}",
            st.max_pages_per_cmd
        );
        assert!(
            st.flush_count >= 1,
            "Expected at least 1 NVMe Flush command"
        );
    }

    drop(disk);
    let st = hal.state.lock().unwrap();
    assert_eq!(st.cc & 1, 0, "Drop must disable CC.EN");
    assert!(st.dma.freed_pages() > 0);
}

#[test]
fn test_nvme_quirk_cns2_unsupported_or_empty_fallback() {
    // Subcase A: Controller rejects CNS=2 with non-zero status code
    {
        let hal = MockNvmeHal::new(2048, 0, Cns2Behavior::RejectStatus, 0);
        {
            let mut st = hal.state.lock().unwrap();
            st.nn = 4;
            st.active_nsid = 2; // Namespace 1 is inactive (0 blocks), Namespace 2 is active!
        }
        let mut disk = block_on(NvmeDisk::open(dummy_nvme_pci(), BAR0, hal.clone()))
            .expect("NvmeDisk::open should fall back from rejected CNS=2 to CNS=0 scan");
        assert_eq!(disk.namespace(), 2);
        assert_eq!(disk.num_blocks(), 2048);

        let payload = [0x42u8; 1024];
        block_on(disk.write(4096, &payload)).unwrap();
        let mut readback = [0u8; 1024];
        block_on(disk.read(4096, &mut readback)).unwrap();
        assert_eq!(readback, payload);
    }

    // Subcase B: Controller returns all zeros for CNS=2
    {
        let hal = MockNvmeHal::new(2048, 0, Cns2Behavior::ReturnAllZeros, 0);
        let mut disk = block_on(NvmeDisk::open(dummy_nvme_pci(), BAR0, hal.clone()))
            .expect("NvmeDisk::open should fall back from empty CNS=2 to CNS=0 scan");
        assert_eq!(disk.namespace(), 1);
        assert_eq!(disk.num_blocks(), 2048);

        let payload = [0x99u8; 512];
        block_on(disk.write(0, &payload)).unwrap();
        let mut readback = [0u8; 512];
        block_on(disk.read(0, &mut readback)).unwrap();
        assert_eq!(readback, payload);
    }
}

#[test]
fn test_nvme_quirk_non_default_doorbell_stride_dstrd2() {
    // CAP.DSTRD = 2 -> stride = 4 << 2 = 16 bytes
    // Admin SQ0 Tail = 0x1000, Admin CQ0 Head = 0x1010, I/O SQ1 Tail = 0x1020, I/O CQ1 Head = 0x1030
    let hal = MockNvmeHal::new(1024, 2, Cns2Behavior::Normal, 0);
    let mut disk = block_on(NvmeDisk::open(dummy_nvme_pci(), BAR0, hal.clone()))
        .expect("NvmeDisk::open should succeed with CAP.DSTRD = 2");

    let data = [0xABu8; 4096];
    block_on(disk.write(0, &data)).expect("Write with DSTRD=2 should succeed");
    let mut out = [0u8; 4096];
    block_on(disk.read(0, &mut out)).expect("Read with DSTRD=2 should succeed");
    assert_eq!(out, data);

    let st = hal.state.lock().unwrap();
    assert!(
        st.doorbell_offsets_written.contains(&0x1000),
        "Missing Admin SQ0 doorbell at 0x1000"
    );
    assert!(
        st.doorbell_offsets_written.contains(&0x1010),
        "Missing Admin CQ0 doorbell at 0x1010"
    );
    assert!(
        st.doorbell_offsets_written.contains(&0x1020),
        "Missing I/O SQ1 doorbell at 0x1020"
    );
    assert!(
        st.doorbell_offsets_written.contains(&0x1030),
        "Missing I/O CQ1 doorbell at 0x1030"
    );
}

#[test]
fn test_nvme_quirk_delayed_cqe_and_phase_tag_wraparound() {
    // Delay every CQE by 3 yields and execute > 2 * NVME_QUEUE_ENTRIES (150) I/O commands
    let hal = MockNvmeHal::new(1024, 0, Cns2Behavior::Normal, 3);
    let mut disk = block_on(NvmeDisk::open(dummy_nvme_pci(), BAR0, hal.clone()))
        .expect("NvmeDisk::open should succeed with delayed CQEs");

    let total_cmds = NVME_QUEUE_ENTRIES * 2 + 22; // 150 commands -> wraps 64-entry CQ twice
    for i in 0..total_cmds {
        let block_off = ((i % 128) * 512) as u64;
        let pat = [(i & 0xFF) as u8; 512];
        block_on(disk.write(block_off, &pat)).expect("Delayed write should succeed");
        let mut buf = [0u8; 512];
        block_on(disk.read(block_off, &mut buf)).expect("Delayed read should succeed");
        assert_eq!(buf, pat);
    }

    let st = hal.state.lock().unwrap();
    assert!(
        st.iocq_phase_flips >= 4,
        "Expected I/O CQ phase bit to flip at least 4 times across {} commands, got {}",
        total_cmds * 2,
        st.iocq_phase_flips
    );
    assert!(
        st.yield_count >= total_cmds * 2 * 3,
        "Expected yield_now() to be polled during delayed CQE waits, got {}",
        st.yield_count
    );
}
