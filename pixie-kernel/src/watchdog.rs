use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use core::time::Duration;

use crate::executor::Executor;
use crate::power_control;
use crate::timer::Timer;

const TIMEOUT_MICROS: i64 = 180 * 1_000_000;

static LAST_PET: AtomicI64 = AtomicI64::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn init() {
    LAST_PET.store(Timer::micros(), Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Executor::spawn("[watchdog]", async {
        loop {
            pet();
            Executor::sleep(Duration::from_secs(10)).await;
        }
    });
}

pub fn pet() {
    LAST_PET.store(Timer::micros(), Ordering::Release);
}

#[cold]
#[inline(never)]
fn trigger_watchdog_reset() -> ! {
    power_control::emergency_serial_write(
        b"\n[WATCHDOG] Software watchdog expired, resetting...\n",
    );
    power_control::emergency_reset();
}

pub fn check() {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    let elapsed = Timer::micros() - LAST_PET.load(Ordering::Acquire);
    if elapsed > TIMEOUT_MICROS {
        trigger_watchdog_reset();
    }
}
