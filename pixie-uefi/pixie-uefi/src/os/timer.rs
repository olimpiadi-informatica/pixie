use core::{arch::x86_64::_rdtsc};


static mut TICKS_AT_START: i64 = 0;
static mut TICKS_PER_MICRO: i64 = 0;



use super::UefiOS;

pub fn start_timer(os: UefiOS) {
    unsafe {
        // Read timer clock & wait to stabilize the counter.
        _rdtsc();
        os.boot_services().stall(20000);
        let tsc_before = _rdtsc() as i64;
        os.boot_services().stall(20000);
        let tsc_after = _rdtsc() as i64;
        TICKS_AT_START = tsc_after;
        // TICKS_PER_MICRO is a multiple of 10 on every reasonable system.
        TICKS_PER_MICRO = (tsc_after - tsc_before) / (20000 * 10) * 10;
    }
}

pub fn get_time_micros() -> i64 {
    unsafe { (_rdtsc() as i64 - TICKS_AT_START) / TICKS_PER_MICRO }
}
