use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

const IA32_APIC_BASE_MSR: u32 = 0x1B;
const APIC_DEFAULT_BASE: u64 = 0xFEE00000;

const REG_EOI: usize = 0x00B0;
const REG_SVR: usize = 0x00F0;
const REG_LVT_TIMER: usize = 0x0320;
const REG_TIMER_INIT_COUNT: usize = 0x0380;
const REG_TIMER_DIV_CONFIG: usize = 0x03E0;

static APIC_BASE: AtomicU64 = AtomicU64::new(APIC_DEFAULT_BASE);

unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | (low as u64)
}

unsafe fn wrmsr(msr: u32, val: u64) {
    let low = val as u32;
    let high = (val >> 32) as u32;
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high, options(nomem, nostack, preserves_flags));
    }
}

#[allow(dead_code)]
unsafe fn apic_read(offset: usize) -> u32 {
    let base = APIC_BASE.load(Ordering::Relaxed);
    unsafe { core::ptr::read_volatile((base + offset as u64) as *const u32) }
}

unsafe fn apic_write(offset: usize, val: u32) {
    let base = APIC_BASE.load(Ordering::Relaxed);
    unsafe { core::ptr::write_volatile((base + offset as u64) as *mut u32, val) }
}

pub unsafe fn init() {
    let apic_msr = unsafe { rdmsr(IA32_APIC_BASE_MSR) };
    let base = apic_msr & 0xFFFF_F000;
    let base = if base == 0 { APIC_DEFAULT_BASE } else { base };
    APIC_BASE.store(base, Ordering::Relaxed);

    // Enable APIC globally via MSR bit 11 if not set
    if (apic_msr & (1 << 11)) == 0 {
        unsafe { wrmsr(IA32_APIC_BASE_MSR, apic_msr | (1 << 11)) };
    }

    // Enable Software APIC and set Spurious Vector to 0xFF
    unsafe {
        apic_write(REG_SVR, 0x1FF);
    }
}

#[allow(dead_code)]
pub fn get_apic_base() -> u64 {
    APIC_BASE.load(Ordering::Relaxed)
}

pub fn send_eoi() {
    unsafe {
        apic_write(REG_EOI, 0);
    }
}

pub unsafe fn start_timer(vector: u8, initial_count: u32) {
    unsafe {
        // Divide by 16 (0b0011)
        apic_write(REG_TIMER_DIV_CONFIG, 0x03);
        // Periodic mode (bit 17) + vector
        apic_write(REG_LVT_TIMER, (1 << 17) | (vector as u32));
        apic_write(REG_TIMER_INIT_COUNT, initial_count);
    }
}
