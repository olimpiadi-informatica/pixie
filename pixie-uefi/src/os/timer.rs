use core::{
    arch::x86_64::_rdtsc,
    sync::atomic::{AtomicBool, AtomicI64, Ordering},
};
use smoltcp::time::Instant;

static TICKS_AT_START: AtomicI64 = AtomicI64::new(0);
static TICKS_PER_MICRO: AtomicI64 = AtomicI64::new(0);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
pub struct Timer {}

fn rdtsc() -> i64 {
    // SAFETY: modern x86 CPUs have this instruction.
    unsafe { _rdtsc() as i64 }
}

fn init() {
    if INITIALIZED.load(Ordering::Relaxed) {
        return;
    }
    // Read timer clock & wait to stabilize the counter.
    rdtsc();
    uefi::boot::stall(20000);
    let tsc_before = rdtsc();
    uefi::boot::stall(20000);
    let tsc_after = rdtsc();

    TICKS_AT_START.store(tsc_after, Ordering::Relaxed);
    // TICKS_PER_MICRO is a multiple of 10 on every reasonable system.
    TICKS_PER_MICRO.store(
        (tsc_after - tsc_before) / (20000 * 10) * 10,
        Ordering::Relaxed,
    );
    INITIALIZED.store(true, Ordering::Relaxed);
}

impl Timer {
    pub fn new() -> Self {
        init();
        Self {}
    }

    pub fn micros(&self) -> i64 {
        let ticks_at_start = TICKS_AT_START.load(Ordering::Relaxed);
        let ticks_per_micro = TICKS_PER_MICRO.load(Ordering::Relaxed);
        (rdtsc() - ticks_at_start) / ticks_per_micro
    }

    pub fn instant(&self) -> Instant {
        Instant::from_micros(self.micros())
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}
