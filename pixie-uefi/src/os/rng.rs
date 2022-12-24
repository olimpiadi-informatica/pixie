use core::arch::x86_64::{_rdseed64_step, _rdtsc};

use rand::{distributions::Uniform, prelude::Distribution, SeedableRng};
use rand_xoshiro::Xoshiro256StarStar;

pub struct Rng {
    rng: Xoshiro256StarStar,
}

impl Rng {
    pub fn new() -> Rng {
        // Try to generate a random number with rdseed up to 10 times, but if that fails, use
        // the timestamp counter.
        let mut seed = 0;
        // SAFETY: modern x86 CPUs have _rdtsc. We use feature detection for rdseed.
        unsafe {
            if core_detect::is_x86_feature_detected!("rdseed") {
                for _i in 0..10 {
                    if _rdseed64_step(&mut seed) == 1 {
                        break;
                    } else {
                        seed = _rdtsc();
                    }
                }
            } else {
                seed = _rdtsc();
            }
        }
        Rng {
            rng: Xoshiro256StarStar::seed_from_u64(seed),
        }
    }

    pub fn rand<T, D: Distribution<T>>(&mut self, d: &D) -> T {
        d.sample(&mut self.rng)
    }

    pub fn rand_u64(&mut self) -> u64 {
        self.rand(&Uniform::new_inclusive(0, u64::MAX))
    }
}
