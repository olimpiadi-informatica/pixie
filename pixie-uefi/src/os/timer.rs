use core::arch::x86_64::_rdtsc;
use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use core::time::Duration;

use smoltcp::time::Instant;

static TICKS_AT_START: AtomicI64 = AtomicI64::new(0);
static TICKS_PER_MICRO: AtomicI64 = AtomicI64::new(0);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub struct Timer {}

pub(super) fn rdtsc() -> i64 {
    // SAFETY: modern x86 CPUs have this instruction.
    unsafe { _rdtsc() as i64 }
}

impl Timer {
    pub(super) fn ensure_init() {
        if INITIALIZED.load(Ordering::Relaxed) {
            return;
        }

        // Try CPUID leaf 0x16 first (Processor Base Frequency in MHz, available on Intel Core 6th Gen+)
        let max_leaf = core::arch::x86_64::__get_cpuid_max(0).0;
        let cpuid_mhz = if max_leaf >= 0x16 {
            let res = core::arch::x86_64::__cpuid(0x16);
            res.eax as i64 // Base frequency in MHz (e.g. 2400 for 2.4GHz)
        } else {
            0
        };

        // Read timer clock & wait to stabilize the counter.
        rdtsc();
        uefi::boot::stall(Duration::from_micros(20000));
        let tsc_before = rdtsc();
        uefi::boot::stall(Duration::from_micros(20000));
        let tsc_after = rdtsc();

        TICKS_AT_START.store(tsc_after, Ordering::Relaxed);

        let calibrated = (tsc_after - tsc_before) / 20_000;
        let ticks_per_micro = if (500..=6000).contains(&cpuid_mhz) {
            cpuid_mhz
        } else if (500..=6000).contains(&calibrated) {
            (calibrated / 10) * 10
        } else {
            2500 // Sane 2.5 GHz fallback
        };

        TICKS_PER_MICRO.store(ticks_per_micro.max(1), Ordering::Relaxed);
        INITIALIZED.store(true, Ordering::Relaxed);
    }

    pub fn micros() -> i64 {
        Self::ensure_init();
        let ticks_at_start = TICKS_AT_START.load(Ordering::Relaxed);
        let ticks_per_micro = TICKS_PER_MICRO.load(Ordering::Relaxed);
        (rdtsc() - ticks_at_start) / ticks_per_micro
    }

    pub fn ticks_per_micro() -> i64 {
        Self::ensure_init();
        TICKS_PER_MICRO.load(Ordering::Relaxed)
    }

    pub fn instant() -> Instant {
        Instant::from_micros(Timer::micros())
    }
}
