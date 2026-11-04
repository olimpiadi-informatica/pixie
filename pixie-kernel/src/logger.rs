use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};
use log::{Level, LevelFilter, Metadata, Record};
use spin::Mutex;
use uefi::proto::console::text::Color;

use crate::hal::NativeHal;
use crate::raw_fb;
use crate::timer::Timer;
use crate::ui::{self, DrawArea};
use pixie_drivers::input::serial::{COM1_PORT, SerialPort};

struct Logger;

static LOGGER: Logger = Logger;

struct LogEntry {
    tenths: u64,
    level: Level,
    target: String,
    msg: String,
}

static EARLY_LOGS: Mutex<Vec<LogEntry>> = Mutex::new(Vec::new());
static LOG_DRAW_AREA: Mutex<DrawArea> = Mutex::new(DrawArea::invalid());
static UI_INITIALIZED: AtomicBool = AtomicBool::new(false);
static SERIAL_PORT: Mutex<Option<SerialPort<NativeHal>>> = Mutex::new(None);

fn serial_write_str(s: &str) {
    if let Some(serial) = SERIAL_PORT.lock().as_ref() {
        serial.write_str(s);
    }
}

struct SerialWriter;

impl Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        serial_write_str(s);
        Ok(())
    }
}

fn write_log(area: &mut DrawArea, tenths: u64, level: Level, target: &str, msg: &str) {
    let color = match level {
        Level::Error => Color::Red,
        Level::Warn => Color::Yellow,
        Level::Info => Color::Green,
        Level::Debug => Color::Blue,
        Level::Trace => Color::Cyan,
    };
    let _ = write!(area, "[{}.{}s ", tenths / 10, tenths % 10);
    area.write_with_color(&alloc::format!("{level:5} "), color, Color::Black);
    let _ = writeln!(area, "{target}] {msg}");
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let tenths = (Timer::micros() as u64 + 50_000) / 100_000;
            let msg = alloc::format!("{}", record.args());
            let target = record.target();
            let _ = writeln!(
                SerialWriter,
                "[{}.{}s {:5} {}] {}",
                tenths / 10,
                tenths % 10,
                record.level().as_str(),
                target,
                msg
            );
            if !UI_INITIALIZED.load(Ordering::Relaxed) {
                let fg = match record.level() {
                    Level::Error => raw_fb::COLOR_RED,
                    Level::Warn => raw_fb::COLOR_YELLOW,
                    Level::Info => raw_fb::COLOR_GREEN,
                    Level::Debug => raw_fb::COLOR_CYAN,
                    Level::Trace => raw_fb::COLOR_WHITE,
                };
                raw_fb::print("[", raw_fb::COLOR_WHITE, raw_fb::COLOR_DARK_BLUE);
                raw_fb::print(record.level().as_str(), fg, raw_fb::COLOR_DARK_BLUE);
                raw_fb::print("] ", raw_fb::COLOR_WHITE, raw_fb::COLOR_DARK_BLUE);
                raw_fb::print(&msg, raw_fb::COLOR_WHITE, raw_fb::COLOR_DARK_BLUE);
                raw_fb::print("\n", raw_fb::COLOR_WHITE, raw_fb::COLOR_DARK_BLUE);
                EARLY_LOGS.lock().push(LogEntry {
                    tenths,
                    level: record.level(),
                    target: String::from(target),
                    msg,
                });
            } else if let Some(mut logs) = LOG_DRAW_AREA.try_lock()
                && logs.size.0 > 0
                && logs.size.1 > 0
            {
                write_log(&mut logs, tenths, record.level(), target, &msg);
                ui::flush();
            }
        }
    }

    fn flush(&self) {}
}

pub fn on_ui_init() {
    let mut logs = DrawArea::logs();
    logs.clear();
    for entry in EARLY_LOGS.lock().drain(..) {
        write_log(
            &mut logs,
            entry.tenths,
            entry.level,
            &entry.target,
            &entry.msg,
        );
    }
    *LOG_DRAW_AREA.lock() = logs;
    UI_INITIALIZED.store(true, Ordering::Relaxed);
    ui::flush();
}

pub fn init() {
    *SERIAL_PORT.lock() = Some(SerialPort::new(COM1_PORT, NativeHal));
    let _ = writeln!(
        SerialWriter,
        "\n--- Pixie Native Kernel Serial Log Initialized ---"
    );
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(LevelFilter::Info));
}
