use core::{ffi::c_void, ptr::NonNull};

use alloc::boxed::Box;
use log::info;
use uefi::{
    prelude::{BootServices, RuntimeServices},
    proto::network::snp::SimpleNetwork,
    table::{
        boot::{EventType, ScopedProtocol, Tpl},
        Boot, SystemTable,
    },
    Event, Status,
};

pub mod net;
pub mod timer;

static mut BOOT_SERVICES: Option<NonNull<BootServices>> = None;
static mut RUNTIME_SERVICES: Option<NonNull<RuntimeServices>> = None;
static mut SIMPLE_NETWORK: Option<&'static ScopedProtocol<'static, SimpleNetwork>> = None;

#[non_exhaustive]
#[derive(Clone, Copy)]
pub struct UefiOS {}

unsafe extern "efiapi" fn exit_boot_services(_e: Event, _ctx: Option<NonNull<c_void>>) {
    panic!("You must never exit boot services");
}

impl UefiOS {
    pub fn new(mut system_table: SystemTable<Boot>) -> UefiOS {
        uefi_services::init(&mut system_table).unwrap();

        unsafe {
            BOOT_SERVICES = NonNull::new(system_table.boot_services() as *const _ as *mut _);
            RUNTIME_SERVICES = NonNull::new(system_table.runtime_services() as *const _ as *mut _);

            let snph = BOOT_SERVICES
                .unwrap()
                .as_ref()
                .find_handles::<SimpleNetwork>()
                .unwrap();

            SIMPLE_NETWORK = Some(Box::leak(Box::new(
                BOOT_SERVICES
                    .unwrap()
                    .as_ref()
                    .open_protocol_exclusive::<SimpleNetwork>(snph[0])
                    .unwrap(),
            )));

            BOOT_SERVICES
                .unwrap()
                .as_ref()
                .set_watchdog_timer(0, 0xFFFFFFFF, None)
                .unwrap();

            BOOT_SERVICES
                .unwrap()
                .as_ref()
                .create_event(
                    EventType::SIGNAL_EXIT_BOOT_SERVICES,
                    Tpl::NOTIFY,
                    Some(exit_boot_services),
                    None,
                )
                .map(|_| ())
                .unwrap();
        }

        UefiOS {}
    }

    pub fn boot_services(&self) -> &'static BootServices {
        unsafe { BOOT_SERVICES.unwrap_unchecked().as_ref() }
    }

    pub fn runtime_services(&self) -> &'static RuntimeServices {
        unsafe { RUNTIME_SERVICES.unwrap_unchecked().as_ref() }
    }

    pub fn simple_network(&self) -> &'static ScopedProtocol<'static, SimpleNetwork> {
        unsafe { SIMPLE_NETWORK.unwrap_unchecked() }
    }

    pub fn reset(&self) -> ! {
        self.runtime_services()
            .reset(uefi::table::runtime::ResetType::Warm, Status::SUCCESS, None)
    }
}
