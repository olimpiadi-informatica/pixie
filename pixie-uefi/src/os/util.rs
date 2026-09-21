use alloc::string::{String, ToString};

pub fn hlt() {
    // SAFETY: hlt is available on all reasonable x86 processors and has no safety
    // requirements.
    unsafe {
        core::arch::asm!("hlt");
    }
}

pub fn get_cpu_model() -> String {
    let mut name = [0u8; 48];
    let mut i = 0;
    for &leaf in &[0x80000002, 0x80000003, 0x80000004] {
        let regs = core::arch::x86_64::__cpuid(leaf);
        for &reg in &[regs.eax, regs.ebx, regs.ecx, regs.edx] {
            name[i..i + 4].copy_from_slice(&reg.to_le_bytes());
            i += 4;
        }
    }
    String::from_utf8_lossy(&name)
        .trim_end_matches('\0')
        .to_string()
}
