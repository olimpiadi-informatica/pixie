use alloc::{collections::vec_deque::VecDeque, string::String};
use core::fmt::Write;
use log::Level;
use spin::Mutex;
use uefi::{boot::ScopedProtocol, proto::console::serial::Serial};

use crate::os::{timer::Timer, UefiOS};

static MESSAGES: Mutex<VecDeque<LogEntry>> = Mutex::new(VecDeque::new());
static SERIAL: Mutex<Option<SerialWrapper>> = Mutex::new(None);

pub(super) struct LogEntry {
    pub time: f64,
    pub level: Level,
    pub target: String,
    pub msg: String,
}

struct SerialWrapper(ScopedProtocol<Serial>);

// SAFETY: There are no threads.
unsafe impl Send for SerialWrapper {}

struct Logger {}

pub(super) fn init() {
    let serial = uefi::boot::find_handles::<Serial>()
        .ok()
        .map(|handles| uefi::boot::open_protocol_exclusive::<Serial>(handles[0]).unwrap());

    *SERIAL.lock() = serial.map(SerialWrapper);

    log::set_logger(&Logger {}).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
}

fn append_message(time: f64, level: log::Level, target: &str, msg: String) {
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
            "[{time:.1}s {style}{level:5}{style:#} {target}] {msg}\r\n"
        )
        .unwrap();
    }

    {
        let mut logs = MESSAGES.try_lock().expect("messages are locked");
        logs.push_back(LogEntry {
            time,
            level,
            target: target.into(),
            msg,
        });
        const MAX_MESSAGES: usize = 10;
        if logs.len() > MAX_MESSAGES {
            logs.pop_front();
        }
    }

    if level <= Level::Warn {
        UefiOS { cant_build: () }.force_ui_redraw()
    }
}

pub(super) fn for_each_log<F: FnMut(&LogEntry)>(f: F) {
    MESSAGES
        .try_lock()
        .expect("messages are locked")
        .iter()
        .for_each(f);
}

impl log::Log for Logger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let now = Timer::micros() as f64 * 0.000_001;
        append_message(
            now,
            record.level(),
            record.target(),
            format!("{}", record.args()),
        );
    }

    fn flush(&self) {
        // no-op
    }
}
