use core::sync::atomic::{AtomicU64, Ordering};

const PAGE_SIZE: usize = 4096;
const HUGE_PAGE_SIZE: u64 = 2 * 1024 * 1024; // 2 MiB

const FLAG_PRESENT: u64 = 1 << 0;
const FLAG_RW: u64 = 1 << 1;
const FLAG_PWT: u64 = 1 << 3;
const FLAG_PCD: u64 = 1 << 4;
const FLAG_HUGE: u64 = 1 << 7;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

static PML4_PHYS: AtomicU64 = AtomicU64::new(0);

unsafe fn alloc_zeroed_table() -> Option<*mut u64> {
    let phys = crate::memory::alloc_page()?;
    let ptr = phys as *mut u8;
    unsafe {
        core::ptr::write_bytes(ptr, 0, PAGE_SIZE);
    }
    Some(ptr as *mut u64)
}

unsafe fn map_2m_page(pml4: *mut u64, phys_2m: u64, uncacheable: bool) -> bool {
    let pml4_idx = ((phys_2m >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((phys_2m >> 30) & 0x1FF) as usize;
    let pd_idx = ((phys_2m >> 21) & 0x1FF) as usize;

    unsafe {
        let mut pml4e = core::ptr::read_volatile(pml4.add(pml4_idx));
        let pdpt = if (pml4e & FLAG_PRESENT) == 0 {
            let Some(new_pdpt) = alloc_zeroed_table() else {
                return false;
            };
            pml4e = (new_pdpt as u64) | FLAG_PRESENT | FLAG_RW;
            core::ptr::write_volatile(pml4.add(pml4_idx), pml4e);
            new_pdpt
        } else {
            (pml4e & ADDR_MASK) as *mut u64
        };

        let mut pdpte = core::ptr::read_volatile(pdpt.add(pdpt_idx));
        let pd = if (pdpte & FLAG_PRESENT) == 0 {
            let Some(new_pd) = alloc_zeroed_table() else {
                return false;
            };
            pdpte = (new_pd as u64) | FLAG_PRESENT | FLAG_RW;
            core::ptr::write_volatile(pdpt.add(pdpt_idx), pdpte);
            new_pd
        } else if (pdpte & FLAG_HUGE) != 0 {
            // Already mapped as 1 GiB huge page
            return true;
        } else {
            (pdpte & ADDR_MASK) as *mut u64
        };

        let mut flags = FLAG_PRESENT | FLAG_RW | FLAG_HUGE;
        if uncacheable {
            flags |= FLAG_PWT | FLAG_PCD;
        }
        core::ptr::write_volatile(pd.add(pd_idx), (phys_2m & !(HUGE_PAGE_SIZE - 1)) | flags);
    }
    true
}

pub unsafe fn init() {
    let Some(pml4) = (unsafe { alloc_zeroed_table() }) else {
        log::error!("paging::init: failed to allocate PML4");
        return;
    };

    let max_phys = crate::memory::max_phys_addr().max(4 * 1024 * 1024 * 1024);
    let end = (max_phys + HUGE_PAGE_SIZE - 1) & !(HUGE_PAGE_SIZE - 1);

    let mut addr = 0u64;
    while addr < end {
        let is_low_mmio = (0xC000_0000..0x1_0000_0000).contains(&addr);
        unsafe {
            map_2m_page(pml4, addr, is_low_mmio);
        }
        addr += HUGE_PAGE_SIZE;
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
    let pml4 = pml4_phys as *mut u64;

    let start = phys & !(HUGE_PAGE_SIZE - 1);
    let end = (phys
        .saturating_add(size as u64)
        .saturating_add(HUGE_PAGE_SIZE - 1))
        & !(HUGE_PAGE_SIZE - 1);

    let mut addr = start;
    while addr < end {
        unsafe {
            map_2m_page(pml4, addr, true);
            #[cfg(target_arch = "x86_64")]
            core::arch::asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags));
        }
        addr += HUGE_PAGE_SIZE;
    }
}
