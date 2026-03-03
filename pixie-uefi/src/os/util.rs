pub fn hlt() {
    // SAFETY: hlt is available on all reasonable x86 processors and has no safety
    // requirements.
    unsafe {
        core::arch::asm!("hlt");
    }
}
