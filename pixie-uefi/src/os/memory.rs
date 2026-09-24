use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
use talc::{OomHandler, Span, Talc, Talck};
use uefi::boot::MemoryType;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned};

pub const PAGE_SIZE: u64 = 4096;
const MAX_PAGES: usize = 262144 * 64; // 16,777,216 pages = 64 GiB of RAM
const BITMAP_WORDS: usize = MAX_PAGES / 64;

// Bitmap: 1 = reserved/used, 0 = free
struct FrameBitmap {
    bitmap: [u64; BITMAP_WORDS],
    total_usable_pages: u64,
    allocated_pages: u64,
    max_page: usize,
}

impl FrameBitmap {
    const fn new() -> Self {
        Self {
            bitmap: [!0u64; BITMAP_WORDS], // start all reserved
            total_usable_pages: 0,
            allocated_pages: 0,
            max_page: 0,
        }
    }

    fn mark_free(&mut self, start_page: usize, num_pages: usize) {
        for p in start_page..start_page + num_pages {
            if p < MAX_PAGES {
                let word = p / 64;
                let bit = p % 64;
                if (self.bitmap[word] & (1 << bit)) != 0 {
                    self.bitmap[word] &= !(1 << bit);
                    self.total_usable_pages += 1;
                }
                if p > self.max_page {
                    self.max_page = p;
                }
            }
        }
    }

    fn mark_used(&mut self, start_page: usize, num_pages: usize) {
        for p in start_page..start_page + num_pages {
            if p < MAX_PAGES {
                let word = p / 64;
                let bit = p % 64;
                if (self.bitmap[word] & (1 << bit)) == 0 {
                    self.bitmap[word] |= 1 << bit;
                    self.total_usable_pages = self.total_usable_pages.saturating_sub(1);
                }
            }
        }
    }

    fn alloc_contiguous(&mut self, num_pages: usize) -> Option<u64> {
        if num_pages == 0 {
            return None;
        }

        let mut consecutive = 0;
        let mut start_page = 0;

        for p in 0..=self.max_page {
            let word = p / 64;
            let bit = p % 64;
            if (self.bitmap[word] & (1 << bit)) == 0 {
                if consecutive == 0 {
                    start_page = p;
                }
                consecutive += 1;
                if consecutive == num_pages {
                    for i in start_page..start_page + num_pages {
                        let w = i / 64;
                        let b = i % 64;
                        self.bitmap[w] |= 1 << b;
                    }
                    self.allocated_pages += num_pages as u64;
                    return Some((start_page as u64) * PAGE_SIZE);
                }
            } else {
                consecutive = 0;
            }
        }
        None
    }

    fn free_contiguous(&mut self, phys: u64, num_pages: usize) {
        let start_page = (phys / PAGE_SIZE) as usize;
        for p in start_page..start_page + num_pages {
            if p < MAX_PAGES {
                let word = p / 64;
                let bit = p % 64;
                if (self.bitmap[word] & (1 << bit)) != 0 {
                    self.bitmap[word] &= !(1 << bit);
                    self.allocated_pages = self.allocated_pages.saturating_sub(1);
                }
            }
        }
    }
}

static FRAME_ALLOCATOR: Mutex<FrameBitmap> = Mutex::new(FrameBitmap::new());

pub fn alloc_page() -> Option<u64> {
    FRAME_ALLOCATOR.lock().alloc_contiguous(1)
}

pub fn alloc_contiguous(num_pages: usize) -> Option<u64> {
    FRAME_ALLOCATOR.lock().alloc_contiguous(num_pages)
}

pub fn free_page(phys: u64) {
    FRAME_ALLOCATOR.lock().free_contiguous(phys, 1);
}

pub fn free_contiguous(phys: u64, num_pages: usize) {
    FRAME_ALLOCATOR.lock().free_contiguous(phys, num_pages);
}

const MAX_BLOCK_SIZE: usize = 32 << 20; // 32 MB maximum chunk to avoid physical fragmentation failure

pub struct AllocOnOom {
    next_block_size: usize,
}

impl OomHandler for AllocOnOom {
    fn handle_oom(talc: &mut Talc<Self>, layout: core::alloc::Layout) -> Result<(), ()> {
        let min_bytes = layout.size().max(PAGE_SIZE as usize);
        let mut target_bytes = talc.oom_handler.next_block_size.max(min_bytes);

        // Attempt to allocate target_bytes. If large contiguous physical frames
        // are unavailable due to fragmentation, back off by halving down to min_bytes.
        let mut chosen_pages = 0;
        let mut phys = None;

        while target_bytes >= min_bytes {
            let num_pages = target_bytes.div_ceil(PAGE_SIZE as usize);
            let p = if TOTAL_RAM.load(Ordering::Relaxed) == 0 {
                uefi::boot::allocate_pages(
                    uefi::boot::AllocateType::AnyPages,
                    uefi::boot::MemoryType::LOADER_DATA,
                    num_pages,
                )
                .ok()
                .map(|ptr| ptr.as_ptr() as u64)
            } else {
                alloc_contiguous(num_pages)
            };

            if let Some(addr) = p {
                phys = Some(addr);
                chosen_pages = num_pages;
                // Target doubling for subsequent OOMs, capped at MAX_BLOCK_SIZE
                talc.oom_handler.next_block_size = (target_bytes * 2).min(MAX_BLOCK_SIZE);
                break;
            }

            if target_bytes <= min_bytes {
                break;
            }
            target_bytes = (target_bytes / 2).max(min_bytes);
        }

        match phys {
            Some(phys) => {
                let span = Span::from_base_size(phys as *mut u8, chosen_pages * PAGE_SIZE as usize);
                unsafe { talc.claim(span) }?;
                Ok(())
            }
            None => {
                log::error!(
                    "OOM: Could not allocate physical memory for layout size {} (min_bytes: {})",
                    layout.size(),
                    min_bytes
                );
                Err(())
            }
        }
    }
}

#[global_allocator]
static ALLOCATOR: Talck<spin::Mutex<()>, AllocOnOom> = Talc::new(AllocOnOom {
    next_block_size: 16 << 20, // 16 MB initial block
})
.lock();

#[derive(Debug, Clone, Copy)]
pub struct MemoryStats {
    pub used: u64,
    pub free: u64,
    pub other: u64,
}

static TOTAL_RAM: AtomicU64 = AtomicU64::new(0);

pub fn init(memory_map: &MemoryMapOwned) {
    let mut total_ram = 0u64;

    {
        let mut fa = FRAME_ALLOCATOR.lock();

        for entry in memory_map.entries() {
            let sz = entry.page_count * PAGE_SIZE;
            total_ram += sz;
            let start_page = (entry.phys_start / PAGE_SIZE) as usize;
            let num_pages = entry.page_count as usize;

            match entry.ty {
                MemoryType::CONVENTIONAL | MemoryType::BOOT_SERVICES_CODE => {
                    fa.mark_free(start_page, num_pages);
                }
                _ => {
                    // Reserved / Runtime / LOADER_CODE / LOADER_DATA / BOOT_SERVICES_DATA
                }
            }
        }

        // Always protect the first 1MB (BIOS IVT, BDA, EBDA, VGA buffers)
        fa.mark_used(0, (0x100000 / PAGE_SIZE) as usize);
    }

    TOTAL_RAM.store(total_ram, Ordering::Relaxed);

    // Prime the heap with an initial 16MB allocation
    let initial_pages = (16 << 20) / PAGE_SIZE as usize;
    if let Some(phys) = alloc_contiguous(initial_pages) {
        let span = Span::from_base_size(phys as *mut u8, initial_pages * PAGE_SIZE as usize);
        unsafe {
            ALLOCATOR
                .lock()
                .claim(span)
                .expect("Failed to claim initial heap span");
        }
    }
}

pub fn stats() -> MemoryStats {
    let fa = FRAME_ALLOCATOR.lock();
    let total = TOTAL_RAM.load(Ordering::Relaxed);
    let used = fa.allocated_pages * PAGE_SIZE;
    let free = fa.total_usable_pages.saturating_sub(fa.allocated_pages) * PAGE_SIZE;
    let other = total.saturating_sub(used + free);

    MemoryStats { used, free, other }
}
