use alloc::string::String;
use core::fmt::Write;

use log::Level;
use spin::Mutex;
use uefi::boot::ScopedProtocol;
use uefi::proto::console::serial::Serial;
use uefi::proto::console::text::Color;

use crate::os::send_wrapper::SendWrapper;
use crate::os::timer::Timer;
use crate::os::ui::{self, DrawArea};

static SERIAL: Mutex<Option<SendWrapper<ScopedProtocol<Serial>>>> = Mutex::new(None);
static DRAW_AREA: Mutex<DrawArea> = Mutex::new(DrawArea::invalid());

struct Logger {}

pub(super) fn init() {
    let serial = uefi::boot::find_handles::<Serial>()
        .ok()
        .map(|handles| uefi::boot::open_protocol_exclusive::<Serial>(handles[0]).unwrap());

    *SERIAL.lock() = serial.map(SendWrapper);

    log::set_logger(&Logger {}).unwrap();
    log::set_max_level(log::LevelFilter::Trace);

    *DRAW_AREA.lock() = DrawArea::logs();
    DRAW_AREA.lock().clear();
}

fn append_message(tenths: u64, level: log::Level, target: &str, msg: String) {
    let secs = tenths / 10;
    let dec = tenths % 10;
    if let Some(serial) = &mut *SERIAL.lock() {
        let style = match level {
            Level::Trace => anstyle::AnsiColor::Cyan.on_default(),
            Level::Debug => anstyle::AnsiColor::Blue.on_default(),
            Level::Info => anstyle::AnsiColor::Green.on_default(),
            Level::Warn => anstyle::AnsiColor::Yellow.on_default(),
            Level::Error => anstyle::AnsiColor::Red.on_default().bold(),
        };
        write!(
            serial.0,
            "[{secs}.{dec}s {style}{level:5}{style:#} {target}] {msg}\r\n"
        )
        .unwrap();
    }

    {
        let col = match level {
            Level::Trace => Color::Cyan,
            Level::Debug => Color::Blue,
            Level::Info => Color::Green,
            Level::Warn => Color::Yellow,
            Level::Error => Color::Red,
        };
        let mut draw_area = DRAW_AREA.lock();
        write!(draw_area, "[{secs}.{dec}s ").unwrap();
        draw_area.write_with_color(&format!("{level:5} "), col, Color::Black);
        writeln!(draw_area, "{target}] {msg}").unwrap();
    }

    if level <= Level::Warn {
        ui::flush();
    }
}

impl log::Log for Logger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let tenths = (Timer::micros() as u64 + 50_000) / 100_000;
        append_message(
            tenths,
            record.level(),
            record.target(),
            format!("{}", record.args()),
        );
    }

    fn flush(&self) {
        // no-op
    }
}
