use alloc::string::String;
use core::fmt::Write;

use log::Level;
use spin::Mutex;
use uefi::proto::console::text::Color;

use crate::os::arch::io;
use crate::os::timer::Timer;
use crate::os::ui::{self, DrawArea};

const COM1: u16 = 0x3F8;

pub struct SerialPort {
    port: u16,
    present: bool,
}

impl SerialPort {
    pub const fn new(port: u16) -> Self {
        Self {
            port,
            present: false,
        }
    }

    pub unsafe fn init(&mut self) {
        unsafe {
            // Test if 16550 UART hardware is physically present using loopback mode
            io::outb(self.port + 1, 0x00); // Disable all interrupts
            io::outb(self.port + 4, 0x1E); // Loopback mode, RTS/DTR set
            io::outb(self.port, 0xAE);     // Write test byte
            let echo = io::inb(self.port);
            if echo != 0xAE {
                self.present = false;
                return;
            }
            self.present = true;

            // Initialize baud rate divisor to 1 (115200 baud)
            io::outb(self.port + 3, 0x80); // Enable DLAB
            io::outb(self.port, 0x01);     // Set divisor to 1 (lo byte)
            io::outb(self.port + 1, 0x00); //                  (hi byte)
            io::outb(self.port + 3, 0x03); // 8 bits, no parity, one stop bit
            io::outb(self.port + 2, 0xC7); // Enable FIFO, clear them, 14-byte threshold
            io::outb(self.port + 4, 0x0B); // Leave loopback, IRQs enabled, RTS/DSR set
        }
    }

    pub fn write_byte(&self, byte: u8) {
        if !self.present {
            return;
        }
        unsafe {
            for _ in 0..2_000 {
                if (io::inb(self.port + 5) & 0x20) != 0 {
                    io::outb(self.port, byte);
                    return;
                }
                io::pause();
            }
        }
    }

    pub fn write_str(&self, s: &str) {
        if !self.present {
            return;
        }
        for b in s.bytes() {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
    }

    pub fn has_data(&self) -> bool {
        if !self.present {
            return false;
        }
        unsafe { (io::inb(self.port + 5) & 0x01) != 0 }
    }

    pub fn read_byte(&self) -> Option<u8> {
        if !self.present {
            return None;
        }
        if self.has_data() {
            Some(unsafe { io::inb(self.port) })
        } else {
            None
        }
    }
}

pub static SERIAL: Mutex<SerialPort> = Mutex::new(SerialPort::new(COM1));
static DRAW_AREA: Mutex<DrawArea> = Mutex::new(DrawArea::invalid());

struct Logger;

pub fn init() {
    unsafe {
        SERIAL.lock().init();
    }

    let _ = log::set_logger(&Logger);
    log::set_max_level(log::LevelFilter::Trace);

    *DRAW_AREA.lock() = DrawArea::logs();
    DRAW_AREA.lock().clear();
}

fn append_message(time: f64, level: log::Level, target: &str, msg: String) {
    let style = match level {
        Level::Trace => anstyle::AnsiColor::Cyan.on_default(),
        Level::Debug => anstyle::AnsiColor::Blue.on_default(),
        Level::Info => anstyle::AnsiColor::Green.on_default(),
        Level::Warn => anstyle::AnsiColor::Yellow.on_default(),
        Level::Error => anstyle::AnsiColor::Red.on_default().bold(),
    };

    let log_line = format!("[{time:.1}s {style}{level:5}{style:#} {target}] {msg}\n");
    SERIAL.lock().write_str(&log_line);

    let col = match level {
        Level::Trace => Color::Cyan,
        Level::Debug => Color::Blue,
        Level::Info => Color::Green,
        Level::Warn => Color::Yellow,
        Level::Error => Color::Red,
    };

    if let Some(mut draw_area) = DRAW_AREA.try_lock() {
        write!(draw_area, "[{time:.1}s ").unwrap();
        draw_area.write_with_color(&format!("{level:5} "), col, Color::Black);
        writeln!(draw_area, "{target}] {msg}").unwrap();
    }

    // Always flush UI so log messages immediately appear on the screen
    ui::flush();
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
        // Serial FIFO is flushed as bytes are written
    }
}
