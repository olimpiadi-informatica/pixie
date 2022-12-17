use core::{
    cell::{Ref, RefCell, RefMut},
    ffi::c_void,
    future::{poll_fn, Future, PollFn},
    mem::transmute,
    ptr::NonNull,
    sync::atomic::AtomicBool,
    task::{Context, Poll},
};

use uefi::{
    prelude::{BootServices, RuntimeServices},
    table::{
        boot::{EventType, Tpl},
        Boot, SystemTable,
    },
    Event, Status,
};

use self::{
    executor::Executor,
    net::{NetworkInterface, TcpStream, UdpHandle},
    rng::Rng,
    timer::Timer,
};

pub mod error;
mod executor;
mod net;
mod rng;
mod timer;

use error::Result;

struct UefiOSImpl {
    boot_services: &'static BootServices,
    runtime_services: &'static RuntimeServices,
    timer: Timer,
    rng: Rng,
    net: NetworkInterface,
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
        let mut rng = Rng::new();
        let net = NetworkInterface::new(boot_services, &mut rng);

        OS_CONSTRUCTED.store(true, core::sync::atomic::Ordering::Relaxed);
        // SAFETY: we guarantee this is the only place we could be modifying OS from, and that
        // nothing can read it until we do so.
        unsafe {
            OS = Some(RefCell::new(UefiOSImpl {
                boot_services,
                runtime_services,
                timer,
                rng,
                net,
            }))
        }

        Executor::init();

        let os = UefiOS {};

        os.spawn(async { f(UefiOS {}).await.unwrap() });

        os.spawn(poll_fn(|cx| {
            let os = UefiOS {}.os().borrow_mut();
            let (mut net, timer) = RefMut::map_split(os, |os| (&mut os.net, &mut os.timer));
            net.poll(&timer);
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
        RefMut::map(self.os().borrow_mut(), |f| &mut f.net)
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

    pub async fn connect(&self, ip: (u8, u8, u8, u8), port: u16) -> Result<TcpStream> {
        TcpStream::new(*self, ip, port).await
    }

    pub async fn udp_bind(&self, port: Option<u16>) -> Result<UdpHandle> {
        UdpHandle::new(*self, port).await
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
