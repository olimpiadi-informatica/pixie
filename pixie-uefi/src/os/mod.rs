use self::{
    boot_options::BootOptions,
    disk::Disk,
    error::{Error, Result},
    executor::{Executor, Task},
    net::NetworkInterface,
    rng::Rng,
    sync::SyncRefCell,
    timer::Timer,
};
use alloc::{
    boxed::Box,
    collections::VecDeque,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{
    cell::{Ref, RefMut},
    ffi::c_void,
    fmt::{self, Display, Write},
    future::{poll_fn, Future, PollFn},
    net::SocketAddrV4,
    ptr::NonNull,
    task::{Context, Poll},
};
use uefi::{
    boot::{EventType, MemoryType, ScopedProtocol, TimerTrigger, Tpl},
    mem::memory_map::MemoryMap,
    proto::{
        console::{
            serial::Serial,
            text::{Color, Input, Key, Output},
        },
        device_path::{
            build::DevicePathBuilder,
            text::{AllowShortcuts, DevicePathToText, DisplayOnly},
            DevicePath,
        },
        Protocol,
    },
    runtime::{VariableAttributes, VariableVendor},
    CStr16, Event, Handle, Status,
};

mod boot_options;
pub mod disk;
pub mod error;
mod executor;
pub mod mpsc;
mod net;
mod rng;
mod sync;
mod timer;

pub use net::{TcpStream, UdpHandle, PACKET_SIZE};

struct BytesFmt(u64);

impl Display for BytesFmt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < (1 << 10) {
            write!(f, "{}B", self.0)
        } else if self.0 < (1 << 20) {
            write!(f, "{:.2}KiB", self.0 as f64 / 1024.0)
        } else if self.0 < (1 << 30) {
            write!(f, "{:.2}MiB", self.0 as f64 / (1 << 20) as f64)
        } else {
            write!(f, "{:.2}GiB", self.0 as f64 / (1 << 30) as f64)
        }
    }
}

struct UefiOSImpl {
    timer: Timer,
    rng: Rng,
    tasks: Vec<Arc<Task>>,
    input: ScopedProtocol<Input>,
    vga: ScopedProtocol<Output>,
    serial: Option<ScopedProtocol<Serial>>,
    net: Option<NetworkInterface>,
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

        let timer = Timer::new();
        let rng = Rng::new();

        let input_handles = uefi::boot::find_handles::<Input>().unwrap();
        let input = uefi::boot::open_protocol_exclusive::<Input>(input_handles[0]).unwrap();

        let serial = uefi::boot::find_handles::<Serial>()
            .ok()
            .map(|handles| uefi::boot::open_protocol_exclusive::<Serial>(handles[0]).unwrap());

        let vga_handles = uefi::boot::find_handles::<Output>().unwrap();
        let mut vga = uefi::boot::open_protocol_exclusive::<Output>(vga_handles[0]).unwrap();

        vga.clear().unwrap();

        *OS.borrow_mut() = Some(UefiOSImpl {
            timer,
            rng,
            tasks: Vec::new(),
            input,
            vga,
            serial,
            net: None,
            messages: VecDeque::new(),
            ui_buf: vec![],
            ui_pos: 0,
            ui_drawer: None,
        });

        let os = UefiOS { cant_build: () };

        log::set_logger(&UefiOS { cant_build: () }).unwrap();
        log::set_max_level(log::LevelFilter::Trace);

        let net = NetworkInterface::new(os);
        os.borrow_mut().net = Some(net);

        os.spawn("init", async move {
            loop {
                if let Err(err) = f(os).await {
                    log::error!("Error: {err:?}");
                }
            }
        });

        os.spawn("[watchdog]", async move {
            loop {
                let err = uefi::boot::set_watchdog_timer(300, 0x10000, None);

                if let Err(err) = err {
                    if err.status() != Status::UNSUPPORTED {
                        log::error!("Error disabling watchdog: {err:?}");
                    }

                    break;
                }

                os.sleep_us(30_000_000).await;
            }
        });

        os.spawn(
            "[net_poll]",
            poll_fn(move |cx| {
                let (mut net, timer) =
                    RefMut::map_split(os.borrow_mut(), |os| (&mut os.net, &mut os.timer));
                net.as_mut().unwrap().poll(&timer);
                // TODO(veluca): figure out whether we can suspend the task.
                cx.waker().wake_by_ref();
                Poll::Pending
            }),
        );

        os.spawn("[net_speed]", async move {
            let mut prx = 0;
            let mut ptx = 0;
            let mut ptm = os.timer().instant();
            loop {
                {
                    let now = os.timer().instant();
                    let dt = (now - ptm).total_micros() as f64 / 1_000_000.0;
                    ptm = now;

                    let mut net = os.net();
                    net.vrx = ((net.rx - prx) as f64 / dt) as u64;
                    prx = net.rx;
                    net.vtx = ((net.tx - ptx) as f64 / dt) as u64;
                    ptx = net.tx;
                }
                os.sleep_us(1_000_000).await;
            }
        });

        os.spawn("[draw_ui]", async move {
            loop {
                os.draw_ui();
                os.sleep_us(1_000_000).await;
            }
        });

        Executor::run(os)
    }

    fn borrow(&self) -> Ref<'static, UefiOSImpl> {
        Ref::map(OS.borrow(), |f| f.as_ref().unwrap())
    }

    fn borrow_mut(&self) -> RefMut<'static, UefiOSImpl> {
        RefMut::map(OS.borrow_mut(), |f| f.as_mut().unwrap())
    }

    pub fn timer(&self) -> Ref<'static, Timer> {
        Ref::map(self.borrow(), |f| &f.timer)
    }

    pub fn rng(&self) -> RefMut<'static, Rng> {
        RefMut::map(self.borrow_mut(), |f| &mut f.rng)
    }

    fn tasks(&self) -> RefMut<'static, Vec<Arc<Task>>> {
        RefMut::map(self.borrow_mut(), |f| &mut f.tasks)
    }

    pub fn net(&self) -> RefMut<'static, NetworkInterface> {
        RefMut::map(self.borrow_mut(), |f| f.net.as_mut().unwrap())
    }

    pub fn wait_for_ip(self) -> PollFn<impl FnMut(&mut Context<'_>) -> Poll<()>> {
        poll_fn(move |cx| {
            if self.net().has_ip() {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    /// Interrupt task execution.
    /// This is useful to yield the CPU to other tasks.
    pub fn schedule(&self) -> PollFn<impl FnMut(&mut Context<'_>) -> Poll<()>> {
        let mut ready = false;
        poll_fn(move |cx| {
            if ready {
                Poll::Ready(())
            } else {
                ready = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    pub fn sleep_us(self, us: u64) -> PollFn<impl FnMut(&mut Context<'_>) -> Poll<()>> {
        let tgt = self.timer().micros() as u64 + us;
        poll_fn(move |cx| {
            let now = self.timer().micros() as u64;
            if now >= tgt {
                Poll::Ready(())
            } else {
                // TODO(veluca): actually suspend the task.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    /// **WARNING**: this function halts all tasks
    pub fn deep_sleep_us(&self, us: u64) {
        // SAFETY: we are not using a callback
        let e =
            unsafe { uefi::boot::create_event(EventType::TIMER, Tpl::NOTIFY, None, None).unwrap() };
        uefi::boot::set_timer(&e, TimerTrigger::Relative(10 * us)).unwrap();
        uefi::boot::wait_for_event(&mut [e]).unwrap();
    }

    pub fn get_variable(
        &self,
        name: &str,
        vendor: &VariableVendor,
    ) -> Result<(Vec<u8>, VariableAttributes)> {
        // name.len() should be enough, but...
        let mut name_buf = vec![0u16; name.len() * 2 + 16];
        let name = CStr16::from_str_with_buf(name, &mut name_buf).unwrap();
        let (var, attrs) = uefi::runtime::get_variable_boxed(name, vendor)
            .map_err(|e| Error::Generic(format!("Error getting variable: {e:?}")))?;
        Ok((var.to_vec(), attrs))
    }

    pub fn set_variable(
        &self,
        name: &str,
        vendor: &VariableVendor,
        attrs: VariableAttributes,
        data: &[u8],
    ) -> Result<()> {
        // name.len() should be enough, but...
        let mut name_buf = vec![0u16; name.len() * 2 + 16];
        let name = CStr16::from_str_with_buf(name, &mut name_buf).unwrap();
        uefi::runtime::set_variable(name, vendor, attrs, data)
            .map_err(|e| Error::Generic(format!("Error setting variable: {e:?}")))?;
        Ok(())
    }

    pub fn boot_options(&self) -> BootOptions {
        BootOptions { os: *self }
    }

    pub fn device_path_to_string(&self, device: &DevicePath) -> String {
        let handle = uefi::boot::get_handle_for_protocol::<DevicePathToText>().unwrap();
        let device_path_to_text =
            uefi::boot::open_protocol_exclusive::<DevicePathToText>(handle).unwrap();
        device_path_to_text
            .convert_device_path_to_text(device, DisplayOnly(true), AllowShortcuts(true))
            .unwrap()
            .to_string()
    }

    /// Find the topmost device that implements this protocol.
    fn handle_on_device<P: Protocol>(&self, device: &DevicePath) -> Option<Handle> {
        for i in 0..device.node_iter().count() {
            let mut buf = vec![];
            let mut dev = DevicePathBuilder::with_vec(&mut buf);
            for node in device.node_iter().take(i + 1) {
                dev = dev.push(&node).unwrap();
            }
            let mut dev = dev.finalize().unwrap();
            if let Ok(h) = uefi::boot::locate_device_path::<P>(&mut dev) {
                return Some(h);
            }
        }
        None
    }

    pub fn open_first_disk(&self) -> Disk {
        Disk::new(*self)
    }

    pub async fn connect(&self, addr: SocketAddrV4) -> Result<TcpStream> {
        TcpStream::new(*self, addr).await
    }

    pub async fn udp_bind(&self, port: Option<u16>) -> Result<UdpHandle> {
        UdpHandle::new(*self, port).await
    }

    pub async fn read_key(&self) -> Result<Key> {
        Ok(poll_fn(move |cx| {
            let key = self.borrow_mut().input.read_key();
            if let Err(e) = key {
                return Poll::Ready(Err(e));
            }
            let key = key.unwrap();
            if let Some(key) = key {
                return Poll::Ready(Ok(key));
            }
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await?)
    }

    pub fn write_with_color(&self, msg: &str, fg: Color, bg: Color) {
        self.borrow_mut().write_with_color(msg, fg, bg);
    }

    fn draw_ui(&self) {
        // Write the header.
        {
            let time = self.timer().micros() as f32 * 0.000_001;
            let ip = self.net().ip();
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

            let vrx = os.net.as_ref().unwrap().vrx;
            let vtx = os.net.as_ref().unwrap().vtx;
            os.write_with_color(
                &format!("rx: {}/s tx: {}/s\n\n", BytesFmt(vrx), BytesFmt(vtx)),
                Color::White,
                Color::Black,
            );

            os.tasks.sort_by_key(|t| -t.micros());
            let tasks: Vec<_> = os.tasks.iter().take(7).cloned().collect();
            for task in tasks {
                os.write_with_color(task.name, Color::White, Color::Black);
                os.maybe_advance_to_col(cols / 4);
                os.write_with_color(
                    &format!("{:7.3}s\n", task.micros() as f64 * 0.000_001),
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
        // TODO(virv): during network initialization we already start logging
        if self.borrow().net.is_none() {
            return;
        }
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
            const MAX_MESSAGES: usize = 5;
            if os.messages.len() > MAX_MESSAGES {
                os.messages.pop_front();
            }
        }
        self.force_ui_redraw();
    }

    /// Spawn a new task.
    pub fn spawn<Fut>(&self, name: &'static str, f: Fut)
    where
        Fut: Future<Output = ()> + 'static,
    {
        let task = executor::Task::new(name, f);
        self.tasks().push(task.clone());
        Executor::spawn(task);
    }

    pub fn reset(&self) -> ! {
        uefi::runtime::reset(uefi::runtime::ResetType::WARM, Status::SUCCESS, None)
    }

    pub fn shutdown(&self) -> ! {
        uefi::runtime::reset(uefi::runtime::ResetType::SHUTDOWN, Status::SUCCESS, None)
    }

    pub fn get_total_mem(&self) -> u64 {
        uefi::boot::memory_map(MemoryType::LOADER_DATA)
            .expect("Failed to get memory map")
            .entries()
            .map(|entry| entry.page_count * 4096)
            .sum()
    }
}

impl log::Log for UefiOS {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let now = self.timer().micros() as f64 * 0.000_001;
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
