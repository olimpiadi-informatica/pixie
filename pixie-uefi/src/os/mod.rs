use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefMut;
use core::ffi::c_void;
use core::fmt::Write;
use core::future::{poll_fn, Future};
use core::ptr::NonNull;
use core::task::Poll;

use pixie_shared::util::BytesFmt;
use uefi::boot::{EventType, ScopedProtocol, Tpl};
use uefi::proto::console::serial::Serial;
use uefi::proto::console::text::{Color, Input, Key, Output};
use uefi::{Event, Status};

use self::error::Result;
use self::executor::Executor;
use self::sync::SyncRefCell;
use self::timer::Timer;

pub mod boot_options;
pub mod disk;
pub mod error;
pub mod executor;
pub mod memory;
pub mod net;
pub mod sync;
mod timer;

struct UefiOSImpl {
    input: ScopedProtocol<Input>,
    vga: ScopedProtocol<Output>,
    serial: Option<ScopedProtocol<Serial>>,
    messages: VecDeque<(f64, log::Level, String, String)>,
    ui_buf: Vec<(String, Color, Color)>,
    ui_pos: usize,
    ui_drawer: Option<Box<dyn Fn(UefiOS) + 'static>>,
}

impl UefiOSImpl {
    fn cols(&mut self) -> usize {
        let mode = self.vga.current_mode().unwrap().unwrap();
        mode.columns()
    }

    pub fn write_with_color(&mut self, msg: &str, fg: Color, bg: Color) {
        let lines: Vec<_> = msg.split('\n').collect();
        for (n, line) in lines.iter().enumerate() {
            self.ui_buf.push((line.to_string(), fg, bg));
            self.ui_pos += line.len();
            if n != lines.len() - 1 {
                let cols = self.cols();
                let colp = self.ui_pos % cols;
                let n = cols - colp;
                self.ui_buf
                    .push((String::from_utf8(vec![0x20; n]).unwrap(), fg, bg));
                self.ui_pos += n;
            }
        }
    }

    pub fn maybe_advance_to_col(&mut self, col: usize) {
        let (fg, bg) = if let Some((_, f, b)) = self.ui_buf[..].last() {
            (*f, *b)
        } else {
            (Color::White, Color::Black)
        };
        let cols = self.cols();
        let colp = self.ui_pos % cols;
        let n = col - colp;
        if colp < col {
            self.ui_buf
                .push((String::from_utf8(vec![0x20; n]).unwrap(), fg, bg));
            self.ui_pos += n;
        }
    }

    pub fn flush_ui_buf(&mut self) {
        self.vga.set_cursor_position(0, 0).unwrap();
        let mode = self.vga.current_mode().unwrap().unwrap();
        let (cols, rows) = (mode.columns(), mode.rows());
        for (msg, fg, bg) in self.ui_buf.drain(..) {
            self.vga.set_color(fg, bg).unwrap();
            write!(self.vga, "{msg}").unwrap();
        }
        self.vga.set_color(Color::White, Color::Black).unwrap();
        if self.ui_pos + 1 < cols * rows {
            // Clear any remaining chars.
            let n = cols * rows - self.ui_pos - 1;
            write!(self.vga, "{}", String::from_utf8(vec![0x20; n]).unwrap()).unwrap();
        }
        self.ui_pos = 0;
    }
}

static OS: SyncRefCell<Option<UefiOSImpl>> = SyncRefCell::new(None);

#[non_exhaustive]
#[derive(Clone, Copy)]
pub struct UefiOS {
    #[allow(dead_code)]
    cant_build: (),
}

unsafe extern "efiapi" fn exit_boot_services(_e: Event, _ctx: Option<NonNull<c_void>>) {
    panic!("You must never exit boot services");
}

impl UefiOS {
    pub fn start<F, Fut>(mut f: F) -> !
    where
        F: FnMut(UefiOS) -> Fut + 'static,
        Fut: Future<Output = Result<()>>,
    {
        // Never call this function twice.
        assert!(OS.borrow().is_none());

        uefi::helpers::init().unwrap();

        // Ensure we never exit boot services.
        // SAFETY: the callback panics on exit from boot services, and thus handles exit from boot
        // services correctly by definition.
        unsafe {
            uefi::boot::create_event(
                EventType::SIGNAL_EXIT_BOOT_SERVICES,
                Tpl::NOTIFY,
                Some(exit_boot_services),
                None,
            )
            .unwrap();
        }

        Timer::ensure_init();

        let input_handles = uefi::boot::find_handles::<Input>().unwrap();
        let input = uefi::boot::open_protocol_exclusive::<Input>(input_handles[0]).unwrap();

        let serial = uefi::boot::find_handles::<Serial>()
            .ok()
            .map(|handles| uefi::boot::open_protocol_exclusive::<Serial>(handles[0]).unwrap());

        let vga_handles = uefi::boot::find_handles::<Output>().unwrap();
        let mut vga = uefi::boot::open_protocol_exclusive::<Output>(vga_handles[0]).unwrap();

        vga.clear().unwrap();

        *OS.borrow_mut() = Some(UefiOSImpl {
            input,
            vga,
            serial,
            messages: VecDeque::new(),
            ui_buf: vec![],
            ui_pos: 0,
            ui_drawer: None,
        });

        let os = UefiOS { cant_build: () };

        log::set_logger(&UefiOS { cant_build: () }).unwrap();
        log::set_max_level(log::LevelFilter::Trace);

        net::init();

        Executor::spawn("init", async move {
            loop {
                if let Err(err) = f(os).await {
                    log::error!("Error: {err:?}");
                }
            }
        });

        Executor::spawn("[watchdog]", async move {
            loop {
                let err = uefi::boot::set_watchdog_timer(300, 0x10000, None);

                if let Err(err) = err {
                    if err.status() != Status::UNSUPPORTED {
                        log::error!("Error disabling watchdog: {err:?}");
                    }

                    break;
                }

                Executor::sleep_us(30_000_000).await;
            }
        });

        Executor::spawn("[draw_ui]", async move {
            loop {
                os.draw_ui();
                Executor::sleep_us(1_000_000).await;
            }
        });

        Executor::run()
    }

    fn borrow_mut(&self) -> RefMut<'static, UefiOSImpl> {
        RefMut::map(OS.borrow_mut(), |f| f.as_mut().unwrap())
    }

    pub fn read_key(&self) -> impl Future<Output = Result<Key>> + '_ {
        poll_fn(move |cx| {
            let key = self.borrow_mut().input.read_key();
            if let Err(e) = key {
                return Poll::Ready(Err(e.into()));
            }
            let key = key.unwrap();
            if let Some(key) = key {
                return Poll::Ready(Ok(key));
            }
            cx.waker().wake_by_ref();
            Poll::Pending
        })
    }

    pub fn write_with_color(&self, msg: &str, fg: Color, bg: Color) {
        self.borrow_mut().write_with_color(msg, fg, bg);
    }

    fn draw_ui(&self) {
        // Write the header.
        {
            let time = Timer::micros() as f32 * 0.000_001;
            let ip = net::ip();
            let mut os = self.borrow_mut();

            let mode = os.vga.current_mode().unwrap().unwrap();
            let cols = mode.columns();

            os.write_with_color(&format!("uptime: {time:10.1}s"), Color::White, Color::Black);
            os.maybe_advance_to_col(cols / 3);

            if let Some(ip) = ip {
                os.write_with_color(&format!("IP: {ip}"), Color::White, Color::Black);
            } else {
                os.write_with_color("DHCP...", Color::Yellow, Color::Black);
            }

            os.maybe_advance_to_col(3 * cols / 5);

            let (vtx, vrx) = net::speed();
            os.write_with_color(
                &format!("rx: {}/s tx: {}/s\n\n", BytesFmt(vrx), BytesFmt(vtx)),
                Color::White,
                Color::Black,
            );

            for (micros, name) in Executor::top_tasks(7) {
                os.write_with_color(name, Color::White, Color::Black);
                os.maybe_advance_to_col(cols / 4);
                os.write_with_color(
                    &format!("{:7.3}s\n", micros as f64 * 0.000_001),
                    Color::White,
                    Color::Black,
                );
            }

            os.maybe_advance_to_col(cols);

            // TODO(veluca): find a better solution.
            let messages: Vec<_> = os.messages.iter().cloned().collect();

            for (time, level, target, msg) in messages {
                let fg_color = match level {
                    log::Level::Trace => Color::Cyan,
                    log::Level::Debug => Color::Blue,
                    log::Level::Info => Color::Green,
                    log::Level::Warn => Color::Yellow,
                    log::Level::Error => Color::Red,
                };
                os.write_with_color(&format!("[{time:.1}s "), Color::White, Color::Black);
                os.write_with_color(&format!("{level:5}"), fg_color, Color::Black);
                os.write_with_color(&format!(" {target}] {msg}\n"), Color::White, Color::Black);
            }
            os.write_with_color("\n", Color::Black, Color::Black);
        }
        {
            let ui = self.borrow_mut().ui_drawer.take();
            if let Some(ui) = &ui {
                ui(*self);
            }
            self.borrow_mut().ui_drawer = ui;
        }
        // Actually draw the changes.
        self.borrow_mut().flush_ui_buf();
    }

    pub fn force_ui_redraw(&self) {
        self.draw_ui()
    }

    pub fn set_ui_drawer<F: Fn(UefiOS) + 'static>(&self, f: F) {
        self.borrow_mut().ui_drawer = Some(Box::new(f));
    }

    fn append_message(&self, time: f64, level: log::Level, target: &str, msg: String) {
        {
            let mut os = self.borrow_mut();

            if let Some(serial) = &mut os.serial {
                let style = match level {
                    log::Level::Trace => anstyle::AnsiColor::Cyan.on_default(),
                    log::Level::Debug => anstyle::AnsiColor::Blue.on_default(),
                    log::Level::Info => anstyle::AnsiColor::Green.on_default(),
                    log::Level::Warn => anstyle::AnsiColor::Yellow.on_default(),
                    log::Level::Error => anstyle::AnsiColor::Red.on_default().bold(),
                };
                write!(
                    serial,
                    "[{time:.1}s {style}{level:5}{style:#} {target}] {msg}\r\n"
                )
                .unwrap();
            }

            os.messages.push_back((time, level, target.into(), msg));
            const MAX_MESSAGES: usize = 10;
            if os.messages.len() > MAX_MESSAGES {
                os.messages.pop_front();
            }
        }
        self.force_ui_redraw();
    }
}

impl log::Log for UefiOS {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let now = Timer::micros() as f64 * 0.000_001;
        self.append_message(
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
