use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::io::{inb, outb, rdmsr, wrmsr};

const IA32_APIC_BASE_MSR: u32 = 0x1B;
const APIC_BASE_ENABLE: u64 = 1 << 11;

const REG_EOI: usize = 0x0B0;
const REG_SVR: usize = 0x0F0;
const REG_TIMER_LVT: usize = 0x320;
const REG_TIMER_INIT_CNT: usize = 0x380;
const REG_TIMER_CURR_CNT: usize = 0x390;
const REG_TIMER_DIV: usize = 0x3E0;

static LAPIC_BASE: AtomicUsize = AtomicUsize::new(0xFEE0_0000);
static APIC_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[inline(always)]
unsafe fn lapic_read(reg: usize) -> u32 {
    let base = LAPIC_BASE.load(Ordering::Relaxed);
    unsafe { core::ptr::read_volatile((base + reg) as *const u32) }
}

#[inline(always)]
unsafe fn lapic_write(reg: usize, val: u32) {
    let base = LAPIC_BASE.load(Ordering::Relaxed);
    unsafe { core::ptr::write_volatile((base + reg) as *mut u32, val) }
}

pub unsafe fn init() {
    unsafe {
        // Mask all legacy 8259 PIC interrupts
        outb(0x21, 0xFF);
        outb(0xA1, 0xFF);

        // Enable Local APIC via MSR
        let msr = rdmsr(IA32_APIC_BASE_MSR);
        let base = (msr & 0xFFFF_F000) as usize;
        if base != 0 {
            LAPIC_BASE.store(base, Ordering::Relaxed);
            super::paging::map_mmio(base as u64, 0x1000);
        }
        wrmsr(IA32_APIC_BASE_MSR, msr | APIC_BASE_ENABLE);

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

        // Mask timer one-shot for calibration
        lapic_write(REG_TIMER_LVT, 1 << 16);
        lapic_write(REG_TIMER_INIT_CNT, 0xFFFF_FFFF);

        // Use PIT channel 2 for ~10ms calibration (1193182 Hz / 100 = 11932 ticks)
        let ch2 = inb(0x61);
        outb(0x61, (ch2 & 0xFD) | 1);
        outb(0x43, 0xB0);
        let pit_count: u16 = 11932;
        outb(0x42, (pit_count & 0xFF) as u8);
        outb(0x42, (pit_count >> 8) as u8);

        // Reset PIT gate
        let gate = inb(0x61) & 0xFE;
        outb(0x61, gate);
        outb(0x61, gate | 1);

        let mut spins = 0u32;
        while (inb(0x61) & 0x20) == 0 {
            spins += 1;
            if spins > 2_000_000 {
                break;
            }
        }

        let curr = lapic_read(REG_TIMER_CURR_CNT);
        let ticks_in_10ms = (0xFFFF_FFFFu32 - curr).max(10_000);
        let ticks_per_us = (ticks_in_10ms as u64 / 10_000).max(1);
        let init_count = (ticks_per_us * interval_us) as u32;

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
