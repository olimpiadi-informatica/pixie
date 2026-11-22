use core::arch::x86_64::_rdtsc;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use core::time::Duration;

use smoltcp::time::Instant;

static TICKS_AT_START: AtomicI64 = AtomicI64::new(0);
static US_PER_TICK_Q32: AtomicU64 = AtomicU64::new(0);
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
        // Read timer clock & wait to stabilize the counter.
        rdtsc();
        uefi::boot::stall(Duration::from_micros(20000));
        let tsc_before = rdtsc();
        uefi::boot::stall(Duration::from_micros(20000));
        let tsc_after = rdtsc();

        let dt_ticks = (tsc_after - tsc_before).max(1) as u64;
        let us_per_tick_q32 = (20_000u64 << 32) / dt_ticks;
        TICKS_AT_START.store(tsc_after, Ordering::Relaxed);
        US_PER_TICK_Q32.store(us_per_tick_q32, Ordering::Relaxed);
        INITIALIZED.store(true, Ordering::Relaxed);
    }

    pub fn micros() -> i64 {
        Self::ensure_init();
        // `ticks * scale == micros << 32` would overflow u64 after ~71.5 minutes.
        let ticks = (rdtsc() - TICKS_AT_START.load(Ordering::Relaxed)).max(0) as u128;
        let scale = US_PER_TICK_Q32.load(Ordering::Relaxed) as u128;
        ((ticks * scale) >> 32) as i64
    }

    pub fn instant() -> Instant {
        Instant::from_micros(Timer::micros())
    }
}
