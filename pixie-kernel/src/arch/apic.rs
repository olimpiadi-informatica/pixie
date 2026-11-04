use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::io::{inb, outb, pause, rdmsr, wrmsr};

const IA32_APIC_BASE_MSR: u32 = 0x1B;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_PHYS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const REG_EOI: usize = 0x0B0;
const REG_SVR: usize = 0x0F0;
const REG_TIMER_LVT: usize = 0x320;
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
        while crate::timer::Timer::micros() - start_us < 10_000 {
            pause();
        }

        let curr = lapic_read(REG_TIMER_CURR_CNT);
        let ticks_in_10ms = (0xFFFF_FFFFu32 - curr).max(10_000);
        let ticks_per_us = (ticks_in_10ms as u64 / 10_000).max(1);
        let init_count = ticks_per_us
            .saturating_mul(interval_us)
            .min(u32::MAX as u64) as u32;

        // Configure periodic mode (bit 17) on `vector`
        lapic_write(REG_TIMER_LVT, (1 << 17) | (vector as u32));
        lapic_write(REG_TIMER_DIV, 0x03);
        lapic_write(REG_TIMER_INIT_CNT, init_count.max(100));
    }
}

pub unsafe fn set_timer_masked(masked: bool) {
    if !APIC_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        let lvt = lapic_read(REG_TIMER_LVT);
        if masked {
            lapic_write(REG_TIMER_LVT, lvt | (1 << 16));
        } else {
            lapic_write(REG_TIMER_LVT, lvt & !(1 << 16));
        }
    }
}
