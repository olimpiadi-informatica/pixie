use talc::*;
use uefi::boot::MemoryType;

const MAX_BLOCK_SIZE: usize = 1 << 30;

struct AllocOnOom {
    next_block_size: usize,
}

impl OomHandler for AllocOnOom {
    fn handle_oom(talc: &mut Talc<Self>, layout: core::alloc::Layout) -> Result<(), ()> {
        let bs = talc.oom_handler.next_block_size;
        talc.oom_handler.next_block_size =
            (talc.oom_handler.next_block_size * 2).min(MAX_BLOCK_SIZE);
        match uefi::boot::allocate_pool(MemoryType::LOADER_DATA, bs) {
            Err(e) => {
                uefi::println!(
                    "Cannot allocate new block ({e}). Triggered by allocation {layout:?}"
                );
                Err(())
            }
            Ok(ptr) => {
                let span = talc::Span::from_base_size(ptr.as_ptr(), bs);
                // SAFETY: the memory was just allocated, so we have exclusive access to it
                // and we can transfer ownership.
                unsafe { talc.claim(span) }?;
                Ok(())
            }
        }
    }
}

#[global_allocator]
static ALLOCATOR: Talck<spin::Mutex<()>, AllocOnOom> = Talc::new(AllocOnOom {
    next_block_size: 1 << 20,
})
.lock();
