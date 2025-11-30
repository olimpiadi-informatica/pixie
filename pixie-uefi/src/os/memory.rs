use uefi::boot::MemoryType;
use uefi::mem::memory_map::MemoryMap;

#[derive(Debug)]
pub struct MemoryStats {
    pub used: u64,
    pub free: u64,
    pub other: u64,
}

pub fn stats() -> MemoryStats {
    let mut stats = MemoryStats {
        used: 0,
        free: 0,
        other: 0,
    };
    const PAGE_SIZE: u64 = 4096;
    uefi::boot::memory_map(MemoryType::LOADER_DATA)
        .expect("Failed to get memory map")
        .entries()
        .for_each(|entry| {
            let sz = entry.page_count * PAGE_SIZE;
            match entry.ty {
                MemoryType::LOADER_DATA | MemoryType::LOADER_CODE => {
                    stats.used += sz;
                }
                MemoryType::CONVENTIONAL => {
                    stats.free += sz;
                }
                _ => {
                    stats.other += sz;
                }
            }
        });
    stats
}
