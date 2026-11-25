#![allow(clippy::missing_safety_doc)]

pub mod apic;
pub mod gdt;
pub mod idt;
pub mod io;
pub mod paging;

/// Enables hardware SSE/SSE2/SSE4.1 execution in `CR0` and `CR4` and unmasks SIMD exceptions.
pub unsafe fn enable_sse() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let mut cr0: u64;
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        cr0 &= !((1 << 2) | (1 << 3)); // Clear EM (emulation) and TS (task switched)
        cr0 |= 1 << 1; // Set MP (monitor co-processor)
        core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack, preserves_flags));

        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        cr4 |= (1 << 9) | (1 << 10); // Set OSFXSR and OSXMMEXCPT
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags));
    }
}
