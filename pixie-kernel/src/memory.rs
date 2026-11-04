use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
#[cfg(target_os = "uefi")]
use talc::{OomHandler, Span, Talc, Talck};
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};

pub const PAGE_SIZE: u64 = 4096;

const MAX_PHYS_PAGES: usize = 16 * 1024 * 1024; // 64 GiB
const BITMAP_WORDS: usize = MAX_PHYS_PAGES / 64;
const LOW_4G_FRAMES: usize = (4 * 1024 * 1024 * 1024usize) / 4096; // 1_048_576 frames

struct FrameBitmap {
    words: [u64; BITMAP_WORDS],
    free_pages: u64,
    used_pages: u64,
    next_search_idx: usize,
}

impl FrameBitmap {
    const fn new() -> Self {
        Self {
            words: [0; BITMAP_WORDS],
            free_pages: 0,
            used_pages: 0,
            next_search_idx: 256, // Start at 1 MiB
        }
    }

    fn mark_free_range(&mut self, start_frame: usize, count: usize) {
        let end = (start_frame + count).min(MAX_PHYS_PAGES);
        for frame in start_frame.max(256)..end {
            let word = frame / 64;
            let bit = frame % 64;
            let mask = 1u64 << bit;
            if (self.words[word] & mask) == 0 {
                self.words[word] |= mask;
                self.free_pages += 1;
            }
        }
    }

    fn alloc_one(&mut self) -> Option<u64> {
        let start_word = self.next_search_idx / 64;
        for offset in 0..BITMAP_WORDS {
            let word_idx = (start_word + offset) % BITMAP_WORDS;
            let w = self.words[word_idx];
            if w != 0 {
                let bit = w.trailing_zeros() as usize;
                let frame = word_idx * 64 + bit;
                if frame < 256 {
                    continue;
                }
                self.words[word_idx] &= !(1u64 << bit);
                self.free_pages = self.free_pages.saturating_sub(1);
                self.used_pages += 1;
                self.next_search_idx = frame + 1;
                return Some((frame as u64) * PAGE_SIZE);
            }
        }
        None
    }

    fn alloc_contiguous_in_range(
        &mut self,
        num_pages: usize,
        min_frame: usize,
        max_frame: usize,
    ) -> Option<u64> {
        if num_pages == 0 {
            return None;
        }
        let limit = max_frame.min(MAX_PHYS_PAGES);
        let mut run = 0usize;
        let mut run_start = min_frame.max(256);

        let start_word = run_start / 64;
        let end_word = limit.div_ceil(64).min(BITMAP_WORDS);

        for word_idx in start_word..end_word {
            let w = self.words[word_idx];
            if w == 0 {
                run = 0;
                continue;
            }
            if w == u64::MAX && run > 0 && run + 64 <= num_pages {
                run += 64;
                if run == num_pages {
                    return Some(self.claim_run(run_start, num_pages));
                }
                continue;
            }
            for bit in 0..64 {
                let frame = word_idx * 64 + bit;
                if frame < min_frame.max(256) {
                    continue;
                }
                if frame >= limit {
                    return None;
                }
                if (w & (1u64 << bit)) != 0 {
                    if run == 0 {
                        run_start = frame;
                    }
                    run += 1;
                    if run == num_pages {
                        return Some(self.claim_run(run_start, num_pages));
                    }
                } else {
                    run = 0;
                }
            }
        }
        None
    }

    fn claim_run(&mut self, run_start: usize, num_pages: usize) -> u64 {
        for f in run_start..(run_start + num_pages) {
            self.words[f / 64] &= !(1u64 << (f % 64));
        }
        self.free_pages = self.free_pages.saturating_sub(num_pages as u64);
        self.used_pages += num_pages as u64;
        (run_start as u64) * PAGE_SIZE
    }

    fn free_range(&mut self, start_frame: usize, num_pages: usize) {
        let end = (start_frame + num_pages).min(MAX_PHYS_PAGES);
        for frame in start_frame..end {
            let word = frame / 64;
            let bit = frame % 64;
            let mask = 1u64 << bit;
            if (self.words[word] & mask) == 0 {
                self.words[word] |= mask;
                self.free_pages += 1;
                self.used_pages = self.used_pages.saturating_sub(1);
            }
        }
    }
}

static FRAME_ALLOCATOR: Mutex<FrameBitmap> = Mutex::new(FrameBitmap::new());
static TOTAL_RAM: AtomicU64 = AtomicU64::new(0);
static MAX_PHYS_ADDR: AtomicU64 = AtomicU64::new(4 * 1024 * 1024 * 1024);

#[cfg(target_os = "uefi")]
#[repr(C, align(4096))]
struct BootstrapHeap([u8; 2 * 1024 * 1024]);

#[cfg(target_os = "uefi")]
static mut BOOTSTRAP_HEAP: BootstrapHeap = BootstrapHeap([0; 2 * 1024 * 1024]);

#[cfg(target_os = "uefi")]
struct KernelOomHandler {
    bootstrap_claimed: bool,
}

#[cfg(target_os = "uefi")]
impl OomHandler for KernelOomHandler {
    fn handle_oom(talc: &mut Talc<Self>, layout: core::alloc::Layout) -> Result<(), ()> {
        if !talc.oom_handler.bootstrap_claimed {
            talc.oom_handler.bootstrap_claimed = true;
            unsafe {
                let ptr = (&raw mut BOOTSTRAP_HEAP.0) as *mut u8;
                let span = Span::from_base_size(ptr, 2 * 1024 * 1024);
                let _ = talc.claim(span);
            }
            if layout.size() <= 1024 * 1024 {
                return Ok(());
            }
        }

        let needed_bytes = (layout.size() + layout.align() + 65536).max(4 * 1024 * 1024);
        let pages = needed_bytes.div_ceil(PAGE_SIZE as usize);
        if let Some(phys) = alloc_contiguous(pages) {
            unsafe {
                let span = Span::from_base_size(phys as *mut u8, pages * (PAGE_SIZE as usize));
                talc.claim(span).map(|_| ())
            }
        } else {
            Err(())
        }
    }
}

#[cfg(target_os = "uefi")]
#[global_allocator]
static ALLOCATOR: Talck<Mutex<()>, KernelOomHandler> = Talc::new(KernelOomHandler {
    bootstrap_claimed: false,
})
.lock();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryStats {
    pub used: u64,
    pub free: u64,
    pub other: u64,
}

fn is_ram_type(ty: MemoryType) -> bool {
    matches!(
        ty,
        MemoryType::CONVENTIONAL
            | MemoryType::BOOT_SERVICES_CODE
            | MemoryType::BOOT_SERVICES_DATA
            | MemoryType::LOADER_CODE
            | MemoryType::LOADER_DATA
            | MemoryType::ACPI_RECLAIM
            | MemoryType::ACPI_NON_VOLATILE
            | MemoryType::RUNTIME_SERVICES_CODE
            | MemoryType::RUNTIME_SERVICES_DATA
    )
}

pub fn init(memory_map: &MemoryMapOwned) {
    let mut total_ram = 0u64;
    let mut max_phys = 4 * 1024 * 1024 * 1024u64;

    {
        let mut fa = FRAME_ALLOCATOR.lock();
        for desc in memory_map.entries() {
            let bytes = desc.page_count * PAGE_SIZE;
            let end_addr = desc.phys_start.saturating_add(bytes);
            if end_addr > max_phys {
                max_phys = end_addr;
            }
            if is_ram_type(desc.ty) {
                total_ram = total_ram.saturating_add(bytes);
            }
            if matches!(desc.ty, MemoryType::CONVENTIONAL) {
                let start_frame = (desc.phys_start / PAGE_SIZE) as usize;
                let count = desc.page_count as usize;
                fa.mark_free_range(start_frame, count);
            }
        }
    }

    TOTAL_RAM.store(total_ram, Ordering::Relaxed);
    MAX_PHYS_ADDR.store(max_phys, Ordering::Relaxed);

    // Pre-seed the global heap with a 32 MiB chunk if available
    #[cfg(target_os = "uefi")]
    if let Some(phys) = alloc_contiguous(8192) {
        let mut talc = ALLOCATOR.lock();
        unsafe {
            let span = Span::from_base_size(phys as *mut u8, 8192 * (PAGE_SIZE as usize));
            let _ = talc.claim(span);
        }
    }
}

pub fn max_phys_addr() -> u64 {
    MAX_PHYS_ADDR.load(Ordering::Relaxed)
}

pub fn alloc_page() -> Option<u64> {
    FRAME_ALLOCATOR.lock().alloc_one()
}

pub fn alloc_contiguous(num_pages: usize) -> Option<u64> {
    if num_pages == 1 {
        return alloc_page();
    }
    FRAME_ALLOCATOR
        .lock()
        .alloc_contiguous_in_range(num_pages, 256, MAX_PHYS_PAGES)
}

/// Allocates `num_pages` contiguous physical pages, preferring addresses below 4 GiB for 32-bit DMA safety.
pub fn alloc_dma_pages(num_pages: usize) -> Option<u64> {
    if num_pages == 0 {
        return None;
    }
    let phys = {
        let mut fa = FRAME_ALLOCATOR.lock();
        fa.alloc_contiguous_in_range(num_pages, 256, LOW_4G_FRAMES)
            .or_else(|| fa.alloc_contiguous_in_range(num_pages, LOW_4G_FRAMES, MAX_PHYS_PAGES))
    }?;
    unsafe {
        core::ptr::write_bytes(phys as *mut u8, 0, num_pages * (PAGE_SIZE as usize));
    }
    Some(phys)
}

pub fn free_page(phys: u64) {
    free_contiguous(phys, 1);
}

pub fn free_contiguous(phys: u64, num_pages: usize) {
    if phys == 0 || num_pages == 0 {
        return;
    }
    let start_frame = (phys / PAGE_SIZE) as usize;
    FRAME_ALLOCATOR.lock().free_range(start_frame, num_pages);
}

pub fn stats() -> MemoryStats {
    let fa = FRAME_ALLOCATOR.lock();
    let free = fa.free_pages * PAGE_SIZE;
    let used = fa.used_pages * PAGE_SIZE;
    let total = TOTAL_RAM.load(Ordering::Relaxed);
    let other = total.saturating_sub(free + used);
    MemoryStats { used, free, other }
}
