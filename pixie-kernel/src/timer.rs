use core::arch::x86_64::_rdtsc;
use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use core::time::Duration;

static TICKS_AT_START: AtomicI64 = AtomicI64::new(0);
static TICKS_PER_MICRO: AtomicI64 = AtomicI64::new(2000);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub struct Timer {}

pub fn rdtsc() -> i64 {
    unsafe { _rdtsc() as i64 }
}

impl Timer {
    pub fn ensure_init() {
        if INITIALIZED.load(Ordering::Relaxed) {
            return;
        }
        rdtsc();
        uefi::boot::stall(Duration::from_micros(20000));
        let tsc_before = rdtsc();
        uefi::boot::stall(Duration::from_micros(20000));
        let tsc_after = rdtsc();

        TICKS_AT_START.store(tsc_after, Ordering::Relaxed);
        let ticks = ((tsc_after - tsc_before) / (20000 * 10) * 10).max(10);
        TICKS_PER_MICRO.store(ticks, Ordering::Relaxed);
        INITIALIZED.store(true, Ordering::Relaxed);
    }

    pub fn init_with_ticks(ticks_per_micro: i64) {
        TICKS_AT_START.store(rdtsc(), Ordering::Relaxed);
        TICKS_PER_MICRO.store(ticks_per_micro.max(1), Ordering::Relaxed);
        INITIALIZED.store(true, Ordering::Relaxed);
    }

    pub fn ticks_per_micro() -> i64 {
        TICKS_PER_MICRO.load(Ordering::Relaxed).max(1)
    }

    pub fn micros() -> i64 {
        let ticks_at_start = TICKS_AT_START.load(Ordering::Relaxed);
        let ticks_per_micro = TICKS_PER_MICRO.load(Ordering::Relaxed).max(1);
        (rdtsc() - ticks_at_start) / ticks_per_micro
    }
}
