use core::arch::x86_64::{_mm_lfence, _rdtsc};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;

static TICKS_AT_START: AtomicU64 = AtomicU64::new(0);
static US_PER_TICK_Q32: AtomicU64 = AtomicU64::new(1 << 21); // ~2 GHz fallback until calibrated
static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub struct Timer {}

#[inline(always)]
pub fn rdtsc() -> i64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        _mm_lfence();
        _rdtsc() as i64
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

#[inline(always)]
fn rdtsc_u64() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        _mm_lfence();
        _rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

impl Timer {
    /// Calibrate TSC frequency using the median of 5 UEFI `stall(4 ms)` intervals
    /// and store fixed-point `us_per_tick << 32` (Q32).
    pub fn ensure_init() {
        if INITIALIZED.load(Ordering::Acquire) {
            return;
        }
        const SAMPLE_US: u64 = 4_000;
        let _ = rdtsc_u64();
        uefi::boot::stall(Duration::from_micros(SAMPLE_US));

        let mut deltas = [0u64; 5];
        for slot in &mut deltas {
            let t0 = rdtsc_u64();
            uefi::boot::stall(Duration::from_micros(SAMPLE_US));
            let t1 = rdtsc_u64();
            *slot = t1.wrapping_sub(t0).max(1);
        }
        deltas.sort_unstable();
        let median_ticks = deltas[2].max(1);
        let us_per_tick_q32 = ((SAMPLE_US as u128) << 32) / (median_ticks as u128);

        TICKS_AT_START.store(rdtsc_u64(), Ordering::Release);
        US_PER_TICK_Q32.store((us_per_tick_q32 as u64).max(1), Ordering::Release);
        INITIALIZED.store(true, Ordering::Release);
    }

    pub fn init_with_ticks(ticks_per_micro: i64) {
        let tpm = ticks_per_micro.max(1) as u64;
        let us_per_tick_q32 = ((1u64 << 32) / tpm).max(1);
        TICKS_AT_START.store(rdtsc_u64(), Ordering::Release);
        US_PER_TICK_Q32.store(us_per_tick_q32, Ordering::Release);
        INITIALIZED.store(true, Ordering::Release);
    }

    #[inline]
    pub fn micros() -> i64 {
        let start = TICKS_AT_START.load(Ordering::Relaxed);
        let ticks = rdtsc_u64().wrapping_sub(start) as u128;
        let scale = US_PER_TICK_Q32.load(Ordering::Relaxed) as u128;
        ((ticks * scale) >> 32).min(i64::MAX as u128) as i64
    }

    #[inline]
    pub fn now() -> Duration {
        Duration::from_micros(Self::micros().max(0) as u64)
    }

    pub async fn sleep(duration: Duration) {
        crate::executor::Executor::sleep(duration).await;
    }
}
