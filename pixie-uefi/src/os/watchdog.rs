use core::sync::atomic::{AtomicI64, Ordering};

use crate::os::panic::handle_fault;
use crate::os::timer::Timer;

static LAST_HEARTBEAT: AtomicI64 = AtomicI64::new(0);
const HANG_TIMEOUT_US: i64 = 300_000_000; // 300 seconds (5 minutes)

pub fn init() {
    pet();
}

pub fn pet() {
    LAST_HEARTBEAT.store(Timer::micros(), Ordering::Relaxed);
}

pub fn check_hang() {
    let last = LAST_HEARTBEAT.load(Ordering::Relaxed);
    if last == 0 {
        return; // Watchdog not started yet
    }
    let elapsed = Timer::micros() - last;
    if elapsed > HANG_TIMEOUT_US {
        handle_fault("WATCHDOG TIMEOUT: Main task executor has stalled for > 300 seconds");
    }
}
