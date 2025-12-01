use core::sync::atomic::{AtomicU64, Ordering};

use crate::os::timer::Timer;
use crate::os::UefiOS;

pub struct NetSpeed {
    total: AtomicU64,
    bytes_per_second: AtomicU64,
    last: AtomicU64,
    last_update_micros: AtomicU64,
}

impl NetSpeed {
    const fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            last: AtomicU64::new(0),
            bytes_per_second: AtomicU64::new(0),
            last_update_micros: AtomicU64::new(0),
        }
    }

    pub fn add_bytes(&self, count: usize) {
        self.total.fetch_add(count as u64, Ordering::Relaxed);
    }

    pub fn bytes_per_second(&self) -> u64 {
        self.bytes_per_second.load(Ordering::Relaxed)
    }

    pub fn update_speed(&self) {
        let micros = Timer::micros() as u64;
        let total = self.total.load(Ordering::Relaxed);
        let last_micros = self.last_update_micros.swap(micros, Ordering::Relaxed);
        let last = self.last.swap(total, Ordering::Relaxed);
        let elapsed = micros.saturating_sub(last_micros).max(1);
        let bytes = total.saturating_sub(last);
        let bytes_per_second = bytes * 1_000_000 / elapsed;
        self.bytes_per_second
            .store(bytes_per_second, Ordering::Relaxed);
    }
}

pub(super) static TX_SPEED: NetSpeed = NetSpeed::new();
pub(super) static RX_SPEED: NetSpeed = NetSpeed::new();

pub(super) fn spawn_update_network_speed_task(os: UefiOS) {
    os.spawn("[net_speed]", async move {
        loop {
            TX_SPEED.update_speed();
            RX_SPEED.update_speed();
            os.sleep_us(1_000_000).await;
        }
    });
}
