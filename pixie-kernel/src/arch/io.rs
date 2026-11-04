#![allow(dead_code)]

#[inline(always)]
pub unsafe fn outb(port: u16, val: u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
    }
}

#[inline(always)]
pub unsafe fn inb(port: u16) -> u8 {
    let ret: u8;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("in al, dx", out("al") ret, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = port;
        ret = 0;
    }
    ret
}

#[inline(always)]
pub unsafe fn outw(port: u16, val: u16) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("out dx, ax", in("dx") port, in("ax") val, options(nomem, nostack, preserves_flags));
    }
}

#[inline(always)]
pub unsafe fn inw(port: u16) -> u16 {
    let ret: u16;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("in ax, dx", out("ax") ret, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = port;
        ret = 0;
    }
    ret
}

#[inline(always)]
pub unsafe fn outl(port: u16, val: u32) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") port, in("eax") val, options(nomem, nostack, preserves_flags));
    }
}

#[inline(always)]
pub unsafe fn inl(port: u16) -> u32 {
    let ret: u32;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("in eax, dx", out("eax") ret, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = port;
        ret = 0;
    }
    ret
}

#[inline(always)]
pub fn pause() {
    core::hint::spin_loop();
}

#[inline(always)]
pub unsafe fn cli() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
}

#[inline(always)]
pub unsafe fn sti() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
}

#[inline(always)]
pub unsafe fn hlt() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("hlt", options(nomem, nostack));
    }
}

#[inline(always)]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let low: u32;
        let high: u32;
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
        ((high as u64) << 32) | (low as u64)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = msr;
        0
    }
}

#[inline(always)]
pub unsafe fn wrmsr(msr: u32, val: u64) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let low = val as u32;
        let high = (val >> 32) as u32;
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") low,
            in("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (msr, val);
    }
}
