use core::{
    cell::{Ref, RefCell, RefMut},
    ffi::c_void,
    future::{poll_fn, Future, PollFn},
    mem::transmute,
    ptr::NonNull,
    sync::atomic::AtomicBool,
    task::{Context, Poll},
};

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use uefi::{
    prelude::{BootServices, RuntimeServices},
    proto::{
        device_path::{
            build::DevicePathBuilder,
            text::{AllowShortcuts, DevicePathToText, DisplayOnly},
            DevicePath,
        },
        Protocol,
    },
    table::{
        boot::{EventType, ScopedProtocol, Tpl},
        runtime::{VariableAttributes, VariableVendor},
        Boot, SystemTable,
    },
    CStr16, Event, Handle, Status,
};

use self::{
    boot_options::BootOptions,
    disk::Disk,
    error::Error,
    executor::Executor,
    net::{NetworkInterface, TcpStream, UdpHandle},
    rng::Rng,
    timer::Timer,
};

mod boot_options;
pub mod disk;
pub mod error;
mod executor;
mod net;
mod rng;
mod timer;

use error::Result;

pub use net::{HttpMethod, PACKET_SIZE};

struct UefiOSImpl {
    boot_services: &'static BootServices,
    runtime_services: &'static RuntimeServices,
    timer: Timer,
    rng: Rng,
    net: Option<NetworkInterface>,
}

static mut OS: Option<RefCell<UefiOSImpl>> = None;
static OS_CONSTRUCTED: AtomicBool = AtomicBool::new(false);

#[non_exhaustive]
#[derive(Clone, Copy)]
pub struct UefiOS {}

impl !Send for UefiOS {}
impl !Sync for UefiOS {}

unsafe extern "efiapi" fn exit_boot_services(_e: Event, _ctx: Option<NonNull<c_void>>) {
    panic!("You must never exit boot services");
}

impl UefiOS {
    pub fn start<F, Fut>(mut system_table: SystemTable<Boot>, f: F) -> !
    where
        F: FnOnce(UefiOS) -> Fut + 'static,
        Fut: Future<Output = Result<()>>,
    {
        // Never call this function twice.
        assert_eq!(
            OS_CONSTRUCTED.load(core::sync::atomic::Ordering::Relaxed),
            false
        );

        uefi_services::init(&mut system_table).unwrap();

        // Ensure we never exit boot services.
        // SAFETY: the callback panics on exit from boot services, and thus handles exit from boot
        // services correctly by definition.
        unsafe {
            system_table
                .boot_services()
                .create_event(
                    EventType::SIGNAL_EXIT_BOOT_SERVICES,
                    Tpl::NOTIFY,
                    Some(exit_boot_services),
                    None,
                )
                .unwrap();
        }

        // SAFETY: it is now safe to assume that boot and runtime services will be available forever.
        let boot_services = unsafe { transmute(system_table.boot_services()) };
        let runtime_services = unsafe { transmute(system_table.runtime_services()) };

        let timer = Timer::new(boot_services);
        let rng = Rng::new();

        OS_CONSTRUCTED.store(true, core::sync::atomic::Ordering::Relaxed);

        // SAFETY: we guarantee this is the only place we could be modifying OS from, and that
        // nothing can read it until we do so.
        unsafe {
            OS = Some(RefCell::new(UefiOSImpl {
                boot_services,
                runtime_services,
                timer,
                rng,
                net: None,
            }))
        }

        let net = NetworkInterface::new(UefiOS {});

        unsafe {
            OS.as_mut().unwrap().borrow_mut().net = Some(net);
        }

        Executor::init();

        let os = UefiOS {};

        os.spawn(async { f(UefiOS {}).await.unwrap() });

        os.spawn(poll_fn(|cx| {
            let os = UefiOS {}.os().borrow_mut();
            let (mut net, timer) = RefMut::map_split(os, |os| (&mut os.net, &mut os.timer));
            net.as_mut().unwrap().poll(&timer);
            // TODO(veluca): figure out whether we can suspend the task.
            cx.waker().wake_by_ref();
            Poll::Pending
        }));

        Executor::run()
    }

    fn os(&self) -> &'static RefCell<UefiOSImpl> {
        // SAFETY: OS is only modified during construction of UefiOS; moreover, it is guaranteed
        // not to be None.
        // No concurrent modifications are possible, as `UefiOS` cannot be constructed in another
        // thread.
        unsafe { OS.as_ref().unwrap_unchecked() }
    }

    pub fn timer(&self) -> Ref<'static, Timer> {
        Ref::map(self.os().borrow(), |f| &f.timer)
    }

    pub fn rng(&self) -> RefMut<'static, Rng> {
        RefMut::map(self.os().borrow_mut(), |f| &mut f.rng)
    }

    pub fn net(&self) -> RefMut<'static, NetworkInterface> {
        RefMut::map(self.os().borrow_mut(), |f| f.net.as_mut().unwrap())
    }

    pub fn wait_for_ip(&self) -> PollFn<impl FnMut(&mut Context<'_>) -> Poll<()>> {
        poll_fn(move |cx| {
            let os = UefiOS {};
            if os.net().has_ip() {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    pub fn sleep_us(&self, us: u64) -> PollFn<impl FnMut(&mut Context<'_>) -> Poll<()>> {
        let tgt = self.timer().micros() as u64 + us;
        poll_fn(move |cx| {
            let os = UefiOS {};
            let now = os.timer().micros() as u64;
            if now >= tgt {
                Poll::Ready(())
            } else {
                // TODO(veluca): actually suspend the task.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    pub fn get_variable(
        &self,
        name: &str,
        vendor: &VariableVendor,
    ) -> Result<(Vec<u8>, VariableAttributes)> {
        // name.len() should be enough, but...
        let mut name_buf = vec![0u16; name.len() * 2 + 16];
        let name = CStr16::from_str_with_buf(name, &mut name_buf).unwrap();
        let size = self
            .os()
            .borrow_mut()
            .runtime_services
            .get_variable_size(name, vendor)
            .map_err(|e| Error::Generic(format!("Error getting variable: {:?}", e)))?;

        let mut var_buf = vec![0u8; size];
        let (var, attrs) = self
            .os()
            .borrow_mut()
            .runtime_services
            .get_variable(name, vendor, &mut var_buf)
            .map_err(|e| Error::Generic(format!("Error getting variable: {:?}", e)))?;
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
        self.os()
            .borrow_mut()
            .runtime_services
            .set_variable(name, vendor, attrs, data)
            .map_err(|e| Error::Generic(format!("Error setting variable: {:?}", e)))?;
        Ok(())
    }

    pub fn boot_options(&self) -> BootOptions {
        BootOptions { os: *self }
    }

    pub fn device_path_to_string(&self, device: &DevicePath) -> String {
        let os = self.os().borrow();
        let handle = os
            .boot_services
            .get_handle_for_protocol::<DevicePathToText>()
            .unwrap();
        let device_path_to_text = os
            .boot_services
            .open_protocol_exclusive::<DevicePathToText>(handle)
            .unwrap();
        device_path_to_text
            .convert_device_path_to_text(
                os.boot_services,
                device,
                DisplayOnly(true),
                AllowShortcuts(true),
            )
            .unwrap()
            .to_string()
    }

    /// Find the topmost device that implements this protocol.
    fn handle_on_device<P: Protocol>(&self, device: &DevicePath) -> Handle {
        let os = self.os().borrow();
        for i in 0..device.node_iter().count() {
            let mut buf = vec![];
            let mut dev = DevicePathBuilder::with_vec(&mut buf);
            for node in device.node_iter().take(i + 1) {
                dev = dev.push(&node).unwrap();
            }
            let mut dev = dev.finalize().unwrap();
            if let Ok(h) = os.boot_services.locate_device_path::<P>(&mut dev) {
                return h;
            }
        }
        // TODO(veluca): bubble up errors.
        panic!("handle not found");
    }

    fn all_handles<P: Protocol>(&self) -> Result<Vec<Handle>> {
        Ok(self.os().borrow().boot_services.find_handles::<P>()?)
    }

    fn open_handle<P: Protocol>(&self, handle: Handle) -> Result<ScopedProtocol<'static, P>> {
        Ok(self
            .os()
            .borrow()
            .boot_services
            .open_protocol_exclusive::<P>(handle)?)
    }

    fn open_protocol_on_device<P: Protocol>(
        &self,
        device: &DevicePath,
    ) -> Result<ScopedProtocol<'static, P>> {
        self.open_handle::<P>(self.handle_on_device::<P>(device))
    }

    pub fn open_first_disk(&self) -> Disk {
        Disk::new(*self)
    }

    pub async fn connect(&self, ip: (u8, u8, u8, u8), port: u16) -> Result<TcpStream> {
        TcpStream::new(*self, ip, port).await
    }

    pub async fn udp_bind(&self, port: Option<u16>) -> Result<UdpHandle> {
        UdpHandle::new(*self, port).await
    }

    pub async fn http(
        &self,
        ip: (u8, u8, u8, u8),
        port: u16,
        method: HttpMethod<'_>,
        path: &[u8],
    ) -> Result<Vec<u8>> {
        net::http(*self, ip, port, method, path).await
    }

    /// Spawn a new task.
    pub fn spawn<Fut>(&self, f: Fut)
    where
        Fut: Future<Output = ()> + 'static,
    {
        Executor::spawn(f)
    }

    pub fn reset(&self) -> ! {
        self.os().borrow().runtime_services.reset(
            uefi::table::runtime::ResetType::Warm,
            Status::SUCCESS,
            None,
        )
    }
}
