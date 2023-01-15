use core::arch::x86_64::_rdtsc;

use smoltcp::time::Instant;
use uefi::prelude::BootServices;

pub struct Timer {
    ticks_at_start: i64,
    ticks_per_micro: i64,
}

impl Timer {
    fn rdtsc() -> i64 {
        // SAFETY: modern x86 CPUs have this instruction.
        unsafe { _rdtsc() as i64 }
    }

    pub fn new(boot_services: &BootServices) -> Timer {
        // Read timer clock & wait to stabilize the counter.
        Self::rdtsc();
        boot_services.stall(20000);
        let tsc_before = Self::rdtsc();
        boot_services.stall(20000);
        let tsc_after = Self::rdtsc();

        Timer {
            ticks_at_start: tsc_after,
            // TICKS_PER_MICRO is a multiple of 10 on every reasonable system.
            ticks_per_micro: (tsc_after - tsc_before) / (20000 * 10) * 10,
        }
    }

    pub fn micros(&self) -> i64 {
        (Self::rdtsc() - self.ticks_at_start) / self.ticks_per_micro
    }

    pub fn instant(&self) -> Instant {
        Instant::from_micros(self.micros())
    }
}
