use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::io::{inb, outb, pause, rdmsr, wrmsr};

const IA32_APIC_BASE_MSR: u32 = 0x1B;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_PHYS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const REG_VERSION: usize = 0x030;
const REG_TPR: usize = 0x080;
const REG_EOI: usize = 0x0B0;
const REG_SVR: usize = 0x0F0;
const REG_ESR: usize = 0x280;
const REG_LVT_CMCI: usize = 0x2F0;
const REG_TIMER_LVT: usize = 0x320;
const REG_LVT_THERMAL: usize = 0x330;
const REG_LVT_PERF: usize = 0x340;
const REG_LVT_LINT0: usize = 0x350;
const REG_LVT_LINT1: usize = 0x360;
const REG_LVT_ERROR: usize = 0x370;
const REG_TIMER_INIT_CNT: usize = 0x380;
const REG_TIMER_CURR_CNT: usize = 0x390;
const REG_TIMER_DIV: usize = 0x3E0;

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0xFEE0_0000);
static X2APIC_MODE: AtomicBool = AtomicBool::new(false);
static APIC_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[inline(always)]
unsafe fn io_wait() {
    unsafe {
        let _ = inb(0x80);
    }
}

#[inline(always)]
unsafe fn lapic_read(reg: usize) -> u32 {
    if X2APIC_MODE.load(Ordering::Relaxed) {
        unsafe { rdmsr(0x800 + (reg as u32 >> 4)) as u32 }
    } else {
        let base = LAPIC_BASE.load(Ordering::Relaxed) as usize;
        unsafe { core::ptr::read_volatile((base + reg) as *const u32) }
    }
}

#[inline(always)]
unsafe fn lapic_write(reg: usize, val: u32) {
    if X2APIC_MODE.load(Ordering::Relaxed) {
        unsafe { wrmsr(0x800 + (reg as u32 >> 4), val as u64) }
    } else {
        let base = LAPIC_BASE.load(Ordering::Relaxed) as usize;
        unsafe { core::ptr::write_volatile((base + reg) as *mut u32, val) }
    }
}

pub unsafe fn init() {
    unsafe {
        // Remap legacy 8259 PIC vectors to 0x20..0x2F before masking all lines so a stray
        // spurious 8259 IRQ7 fires vector 0x27 instead of CPU Exception #15.
        outb(0x20, 0x11);
        io_wait();
        outb(0xA0, 0x11);
        io_wait();
        outb(0x21, 0x20);
        io_wait();
        outb(0xA1, 0x28);
        io_wait();
        outb(0x21, 0x04);
        io_wait();
        outb(0xA1, 0x02);
        io_wait();
        outb(0x21, 0x01);
        io_wait();
        outb(0xA1, 0x01);
        io_wait();
        outb(0x21, 0xFF);
        outb(0xA1, 0xFF);

        // Enable Local APIC (supporting both xAPIC MMIO and x2APIC MSR modes)
        let msr = rdmsr(IA32_APIC_BASE_MSR);
        let x2apic = (msr & APIC_BASE_X2APIC) != 0;
        X2APIC_MODE.store(x2apic, Ordering::Relaxed);
        if x2apic {
            if (msr & APIC_BASE_ENABLE) == 0 {
                wrmsr(
                    IA32_APIC_BASE_MSR,
                    msr | APIC_BASE_ENABLE | APIC_BASE_X2APIC,
                );
            }
        } else {
            let base = msr & APIC_PHYS_MASK;
            if base != 0 {
                LAPIC_BASE.store(base, Ordering::Relaxed);
                super::paging::map_mmio(base, 0x1000);
            }
            wrmsr(IA32_APIC_BASE_MSR, msr | APIC_BASE_ENABLE);
        }

        // Clear Task Priority Register so all vectors are deliverable.
        lapic_write(REG_TPR, 0);

        // Mask unused LVT entries before enabling APIC.
        let max_lvt = ((lapic_read(REG_VERSION) >> 16) & 0xFF) as usize;
        if max_lvt >= 6 {
            lapic_write(REG_LVT_CMCI, 1 << 16);
        }
        if max_lvt >= 5 {
            lapic_write(REG_LVT_THERMAL, 1 << 16);
        }
        if max_lvt >= 4 {
            lapic_write(REG_LVT_PERF, 1 << 16);
        }
        lapic_write(REG_LVT_LINT0, 1 << 16);
        lapic_write(REG_LVT_LINT1, 1 << 16);
        lapic_write(REG_LVT_ERROR, 1 << 16);

        // Clear any pending APIC error bits (two writes per Intel SDM).
        lapic_write(REG_ESR, 0);
        lapic_write(REG_ESR, 0);

        // Set Spurious Interrupt Vector Register (enable APIC bit 8 + spurious vector 255)
        lapic_write(REG_SVR, 0x1FF);
        APIC_INITIALIZED.store(true, Ordering::SeqCst);
    }
}

#[inline(always)]
pub fn send_eoi() {
    if APIC_INITIALIZED.load(Ordering::Relaxed) {
        unsafe {
            lapic_write(REG_EOI, 0);
        }
    }
}

pub unsafe fn start_periodic_timer(vector: u8, interval_us: u64) {
    unsafe {
        // Set divider to 16
        lapic_write(REG_TIMER_DIV, 0x03);

        // Mask timer one-shot for 10ms calibration against already-calibrated TSC timer
        lapic_write(REG_TIMER_LVT, 1 << 16);
        lapic_write(REG_TIMER_INIT_CNT, 0xFFFF_FFFF);

        let start_us = crate::timer::Timer::micros();
        while crate::timer::Timer::micros().saturating_sub(start_us) < 10_000 {
            pause();
        }

        let curr = lapic_read(REG_TIMER_CURR_CNT);
        let ticks_in_10ms = (0xFFFF_FFFFu32 - curr).max(1);
        let init_count = (((ticks_in_10ms as u128) * (interval_us as u128)) / 10_000)
            .clamp(100, u32::MAX as u128) as u32;

        // Configure periodic mode (bit 17) on `vector`
        lapic_write(REG_TIMER_LVT, (1 << 17) | (vector as u32));
        lapic_write(REG_TIMER_DIV, 0x03);
        lapic_write(REG_TIMER_INIT_CNT, init_count);
    }
}

/// Set APIC timer mask state and return whether the timer was previously masked.
pub unsafe fn set_timer_masked(masked: bool) -> bool {
    if !APIC_INITIALIZED.load(Ordering::SeqCst) {
        return true;
    }
    unsafe {
        let lvt = lapic_read(REG_TIMER_LVT);
        let was_masked = (lvt & (1 << 16)) != 0;
        if masked {
            lapic_write(REG_TIMER_LVT, lvt | (1 << 16));
        } else {
            lapic_write(REG_TIMER_LVT, lvt & !(1 << 16));
        }
        was_masked
    }
}
