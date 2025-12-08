use core::ffi::c_void;
use core::future::Future;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use uefi::boot::{EventType, Tpl};
use uefi::{Event, Status};

use self::error::Result;
use self::executor::Executor;
use self::timer::Timer;

pub mod boot_options;
pub mod disk;
pub mod error;
pub mod executor;
pub mod input;
mod logger;
pub mod memory;
pub mod net;
mod send_wrapper;
mod timer;
pub mod ui;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub fn start<F, Fut>(mut f: F) -> !
where
    F: FnMut() -> Fut + 'static,
    Fut: Future<Output = Result<()>>,
{
    assert!(!INITIALIZED.swap(true, Ordering::Relaxed));

    uefi::helpers::init().unwrap();

    unsafe extern "efiapi" fn exit_boot_services(_e: Event, _ctx: Option<NonNull<c_void>>) {
        panic!("You must never exit boot services");
    }

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
    ui::init();
    logger::init();
    net::init();

    Executor::spawn("init", async move {
        loop {
            if let Err(err) = f().await {
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

            Executor::sleep(Duration::from_secs(30)).await;
        }
    });

    Executor::run()
}
