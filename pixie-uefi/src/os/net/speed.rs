use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use pixie_shared::util::BytesFmt;
use uefi::proto::console::text::Color;

use crate::os::executor::Executor;
use crate::os::timer::Timer;
use crate::os::ui;

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

pub(super) fn spawn_network_speed_task() {
    Executor::spawn("[net_speed]", async move {
        let mut draw_area = ui::DrawArea::net_speed();
        loop {
            TX_SPEED.update_speed();
            RX_SPEED.update_speed();
            draw_area.clear();
            let w = draw_area.size().0;
            let vtx = TX_SPEED.bytes_per_second();
            let vrx = RX_SPEED.bytes_per_second();
            draw_area.write_with_color(
                &format!("Network \u{2193}:{0:1$}", "", w - 22),
                Color::Green,
                Color::Black,
            );
            writeln!(draw_area, "{:10.1}/s", BytesFmt(vrx)).unwrap();
            draw_area.write_with_color(
                &format!("Network \u{2191}:{0:1$}", "", w - 22),
                Color::Cyan,
                Color::Black,
            );
            writeln!(draw_area, "{:10.1}/s", BytesFmt(vtx)).unwrap();
            Executor::sleep(Duration::from_secs(1)).await;
        }
    });
}
