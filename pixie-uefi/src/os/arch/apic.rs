use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::io;

const IA32_APIC_BASE_MSR: u32 = 0x1B;
const APIC_DEFAULT_BASE: u64 = 0xFEE00000;

const REG_EOI: usize = 0x00B0;
const REG_SVR: usize = 0x00F0;
const REG_LVT_TIMER: usize = 0x0320;
const REG_TIMER_INIT_COUNT: usize = 0x0380;
const REG_TIMER_CURRENT_COUNT: usize = 0x0390;
const REG_TIMER_DIV_CONFIG: usize = 0x03E0;

static APIC_BASE: AtomicU64 = AtomicU64::new(APIC_DEFAULT_BASE);
static IS_X2APIC: AtomicBool = AtomicBool::new(false);

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
    if IS_X2APIC.load(Ordering::Relaxed) {
        unsafe { rdmsr(0x800 + (offset as u32 >> 4)) as u32 }
    } else {
        let base = APIC_BASE.load(Ordering::Relaxed);
        unsafe { core::ptr::read_volatile((base + offset as u64) as *const u32) }
    }
}

unsafe fn apic_write(offset: usize, val: u32) {
    if IS_X2APIC.load(Ordering::Relaxed) {
        unsafe { wrmsr(0x800 + (offset as u32 >> 4), val as u64) }
    } else {
        let base = APIC_BASE.load(Ordering::Relaxed);
        unsafe { core::ptr::write_volatile((base + offset as u64) as *mut u32, val) }
    }
}

pub unsafe fn init() {
    // Mask legacy 8259 PIC to prevent PIT IRQ 0 from firing on CPU vector 8 (Double Fault)
    unsafe {
        io::outb(0x21, 0xFF);
        io::outb(0xA1, 0xFF);
    }

    let apic_msr = unsafe { rdmsr(IA32_APIC_BASE_MSR) };
    let is_x2apic = (apic_msr & (1 << 10)) != 0;
    IS_X2APIC.store(is_x2apic, Ordering::Relaxed);

    if !is_x2apic {
        let base = apic_msr & 0xFFFF_F000;
        let base = if base == 0 { APIC_DEFAULT_BASE } else { base };
        APIC_BASE.store(base, Ordering::Relaxed);

        // Enable APIC globally via MSR bit 11 if not set
        if (apic_msr & (1 << 11)) == 0 {
            unsafe { wrmsr(IA32_APIC_BASE_MSR, apic_msr | (1 << 11)) };
        }
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

#[allow(dead_code)]
pub unsafe fn start_timer(vector: u8, initial_count: u32) {
    unsafe {
        // Divide by 16 (0b0011)
        apic_write(REG_TIMER_DIV_CONFIG, 0x03);
        // Periodic mode (bit 17) + vector
        apic_write(REG_LVT_TIMER, (1 << 17) | (vector as u32));
        apic_write(REG_TIMER_INIT_COUNT, initial_count);
    }
}

/// Calibrates the Local APIC timer against the calibrated TSC and sets a periodic interrupt.
pub unsafe fn start_periodic_timer(vector: u8, interval_us: u64) {
    unsafe {
        // Divide by 16 (0b0011)
        apic_write(REG_TIMER_DIV_CONFIG, 0x03);

        // Mask timer and prepare calibration
        apic_write(REG_LVT_TIMER, 1 << 16);
        apic_write(REG_TIMER_INIT_COUNT, 0xFFFF_FFFF);

        // Measure ticks elapsed over 10,000 microseconds (10ms) using TSC timer
        let start_us = crate::os::timer::Timer::micros();
        while crate::os::timer::Timer::micros() - start_us < 10_000 {
            core::hint::spin_loop();
        }
        let current = apic_read(REG_TIMER_CURRENT_COUNT);
        let ticks_in_10ms = 0xFFFF_FFFF_u32.saturating_sub(current);

        // Fallback if timer didn't tick (e.g. broken virtual environment)
        let ticks_in_10ms = if ticks_in_10ms > 100 {
            ticks_in_10ms
        } else {
            // Assume 25 MHz bus clock / 16 * 10ms = ~15,625 ticks
            15_625
        };

        // Calculate count for interval_us (ticks_in_10ms is for 10_000 us)
        let count = ((ticks_in_10ms as u64 * interval_us) / 10_000).clamp(100, 0xFFFF_FFFF) as u32;

        // Periodic mode (bit 17) + vector
        apic_write(REG_LVT_TIMER, (1 << 17) | (vector as u32));
        apic_write(REG_TIMER_INIT_COUNT, count);
    }
}
