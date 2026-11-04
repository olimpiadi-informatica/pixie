use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned};

const PAGE_SIZE: usize = 4096;
const HUGE_PAGE_SIZE: u64 = 2 * 1024 * 1024; // 2 MiB

const FLAG_PRESENT: u64 = 1 << 0;
const FLAG_RW: u64 = 1 << 1;
const FLAG_PWT: u64 = 1 << 3;
const FLAG_PCD: u64 = 1 << 4;
const FLAG_HUGE: u64 = 1 << 7;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const MAX_RAM_RANGES: usize = 128;

struct RamRanges {
    entries: [(u64, u64); MAX_RAM_RANGES],
    len: usize,
}

impl RamRanges {
    const fn new() -> Self {
        Self {
            entries: [(0, 0); MAX_RAM_RANGES],
            len: 0,
        }
    }

    fn insert(&mut self, mut start: u64, mut end: u64) {
        if start >= end {
            return;
        }
        let mut i = 0;
        while i < self.len {
            let (rs, re) = self.entries[i];
            if start <= re && rs <= end {
                start = start.min(rs);
                end = end.max(re);
                self.len -= 1;
                self.entries[i] = self.entries[self.len];
            } else {
                i += 1;
            }
        }
        if self.len < MAX_RAM_RANGES {
            self.entries[self.len] = (start, end);
            self.len += 1;
        }
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.entries[..self.len]
            .iter()
            .any(|&(rs, re)| rs < end && start < re)
    }
}

static PML4_PHYS: AtomicU64 = AtomicU64::new(0);
static RAM_RANGES: Mutex<RamRanges> = Mutex::new(RamRanges::new());
static PAGING_LOCK: Mutex<()> = Mutex::new(());

#[inline]
unsafe fn invlpg(addr: u64) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = addr;
}

unsafe fn alloc_zeroed_table() -> Option<*mut u64> {
    let phys = crate::memory::alloc_page()?;
    let ptr = phys as *mut u8;
    unsafe {
        core::ptr::write_bytes(ptr, 0, PAGE_SIZE);
    }
    Some(ptr as *mut u64)
}

unsafe fn get_or_alloc_pd(pml4: *mut u64, phys: u64) -> Option<*mut u64> {
    let pml4_idx = ((phys >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((phys >> 30) & 0x1FF) as usize;

    unsafe {
        let mut pml4e = core::ptr::read_volatile(pml4.add(pml4_idx));
        let pdpt = if (pml4e & FLAG_PRESENT) == 0 {
            let new_pdpt = alloc_zeroed_table()?;
            pml4e = (new_pdpt as u64) | FLAG_PRESENT | FLAG_RW;
            core::ptr::write_volatile(pml4.add(pml4_idx), pml4e);
            new_pdpt
        } else {
            (pml4e & ADDR_MASK) as *mut u64
        };

        let mut pdpte = core::ptr::read_volatile(pdpt.add(pdpt_idx));
        if (pdpte & FLAG_PRESENT) == 0 {
            let new_pd = alloc_zeroed_table()?;
            pdpte = (new_pd as u64) | FLAG_PRESENT | FLAG_RW;
            core::ptr::write_volatile(pdpt.add(pdpt_idx), pdpte);
            Some(new_pd)
        } else if (pdpte & FLAG_HUGE) != 0 {
            None
        } else {
            Some((pdpte & ADDR_MASK) as *mut u64)
        }
    }
}

unsafe fn map_2m_page(pml4: *mut u64, phys_2m: u64, uncacheable: bool) -> bool {
    let pd_idx = ((phys_2m >> 21) & 0x1FF) as usize;
    unsafe {
        let Some(pd) = get_or_alloc_pd(pml4, phys_2m) else {
            return false;
        };
        let mut flags = FLAG_PRESENT | FLAG_RW | FLAG_HUGE;
        if uncacheable {
            flags |= FLAG_PWT | FLAG_PCD;
        }
        core::ptr::write_volatile(pd.add(pd_idx), (phys_2m & !(HUGE_PAGE_SIZE - 1)) | flags);
    }
    true
}

pub unsafe fn init(memory_map: &MemoryMapOwned) {
    {
        let mut ram = RAM_RANGES.lock();
        *ram = RamRanges::new();
        for desc in memory_map.entries() {
            if crate::memory::is_ram_type(desc.ty) && desc.page_count > 0 {
                let start = desc.phys_start;
                let end = start.saturating_add(desc.page_count * crate::memory::PAGE_SIZE);
                ram.insert(start, end);
            }
        }
    }

    let Some(pml4) = (unsafe { alloc_zeroed_table() }) else {
        log::error!("paging::init: failed to allocate PML4");
        return;
    };

    let max_phys = crate::memory::max_phys_addr().max(4 * 1024 * 1024 * 1024);
    let end = (max_phys + HUGE_PAGE_SIZE - 1) & !(HUGE_PAGE_SIZE - 1);

    {
        let ram = RAM_RANGES.lock();
        let mut addr = 0u64;
        while addr < end {
            let has_ram = ram.overlaps(addr, addr + HUGE_PAGE_SIZE);
            if !unsafe { map_2m_page(pml4, addr, !has_ram) } {
                log::error!("paging::init: failed to map 2 MiB page at {:#x}", addr);
                break;
            }
            addr += HUGE_PAGE_SIZE;
        }
    }

    let pml4_phys = pml4 as u64;
    PML4_PHYS.store(pml4_phys, Ordering::SeqCst);

    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) pml4_phys, options(nostack, preserves_flags));
    }
}

pub fn map_mmio(phys: u64, size: usize) {
    if size == 0 {
        return;
    }
    let pml4_phys = PML4_PHYS.load(Ordering::SeqCst);
    if pml4_phys == 0 {
        return;
    }
    let _guard = PAGING_LOCK.lock();
    let ram = RAM_RANGES.lock();
    let pml4 = pml4_phys as *mut u64;

    let page_mask = PAGE_SIZE as u64 - 1;
    let start_4k = phys & !page_mask;
    let end_4k = phys.saturating_add(size as u64).saturating_add(page_mask) & !page_mask;

    let mut cur = start_4k;
    while cur < end_4k {
        let base_2m = cur & !(HUGE_PAGE_SIZE - 1);
        let next_2m = base_2m.saturating_add(HUGE_PAGE_SIZE);
        let chunk_end = end_4k.min(next_2m);
        let pd_idx = ((base_2m >> 21) & 0x1FF) as usize;

        unsafe {
            let Some(pd) = get_or_alloc_pd(pml4, base_2m) else {
                log::error!("paging::map_mmio: failed to allocate PD at {:#x}", base_2m);
                return;
            };

            let pde = core::ptr::read_volatile(pd.add(pd_idx));

            // Check if every requested 4 KiB page in this 2 MiB window overlaps RAM
            let mut all_ram = true;
            let mut p = cur;
            while p < chunk_end {
                if !ram.overlaps(p, p + PAGE_SIZE as u64) {
                    all_ram = false;
                    break;
                }
                p += PAGE_SIZE as u64;
            }

            if all_ram
                && (pde & FLAG_PRESENT) != 0
                && (pde & FLAG_HUGE) != 0
                && (pde & (FLAG_PWT | FLAG_PCD)) == 0
            {
                // Already mapped as WB 2 MiB page; never flip RAM to UC!
                cur = chunk_end;
                continue;
            }

            if cur == base_2m
                && chunk_end == next_2m
                && !ram.overlaps(base_2m, next_2m)
                && ((pde & FLAG_PRESENT) == 0 || (pde & FLAG_HUGE) != 0)
            {
                let flags = FLAG_PRESENT | FLAG_RW | FLAG_HUGE | FLAG_PWT | FLAG_PCD;
                core::ptr::write_volatile(pd.add(pd_idx), base_2m | flags);
                invlpg(base_2m);
                cur = chunk_end;
                continue;
            }

            // Ensure a 512-entry 4 KiB Page Table exists for `base_2m`
            let pt = if (pde & FLAG_PRESENT) == 0 {
                let Some(new_pt) = alloc_zeroed_table() else {
                    log::error!("paging::map_mmio: failed to allocate PT at {:#x}", base_2m);
                    return;
                };
                core::ptr::write_volatile(pd.add(pd_idx), (new_pt as u64) | FLAG_PRESENT | FLAG_RW);
                new_pt
            } else if (pde & FLAG_HUGE) != 0 {
                let Some(new_pt) = alloc_zeroed_table() else {
                    log::error!(
                        "paging::map_mmio: failed to split 2 MiB PDE at {:#x}",
                        base_2m
                    );
                    return;
                };
                let old_cache_flags = pde & (FLAG_PWT | FLAG_PCD);
                for i in 0..512usize {
                    let sub_phys = base_2m + (i * PAGE_SIZE) as u64;
                    core::ptr::write_volatile(
                        new_pt.add(i),
                        sub_phys | FLAG_PRESENT | FLAG_RW | old_cache_flags,
                    );
                }
                core::ptr::write_volatile(pd.add(pd_idx), (new_pt as u64) | FLAG_PRESENT | FLAG_RW);
                invlpg(base_2m);
                new_pt
            } else {
                (pde & ADDR_MASK) as *mut u64
            };

            let mut p = cur;
            while p < chunk_end {
                let pt_idx = ((p >> 12) & 0x1FF) as usize;
                let is_ram = ram.overlaps(p, p + PAGE_SIZE as u64);
                let mut flags = FLAG_PRESENT | FLAG_RW;
                if !is_ram {
                    flags |= FLAG_PWT | FLAG_PCD;
                }
                core::ptr::write_volatile(pt.add(pt_idx), p | flags);
                invlpg(p);
                p += PAGE_SIZE as u64;
            }
        }

        cur = chunk_end;
    }
}
