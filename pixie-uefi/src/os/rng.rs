use core::arch::x86_64::_rdtsc;
use rand::{distributions::Uniform, prelude::Distribution, SeedableRng};
use rand_xoshiro::Xoshiro256StarStar;

pub struct Rng {
    rng: Xoshiro256StarStar,
}

impl Default for Rng {
    fn default() -> Self {
        Self::new()
    }
}

impl Rng {
    pub fn new() -> Rng {
        // SAFETY: modern x86 CPUs have _rdtsc.
        let seed = unsafe { _rdtsc() };
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
