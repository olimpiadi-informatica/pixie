use core::{
    arch::x86_64::{_rdseed64_step, _rdtsc},
    ffi::c_void,
    ptr::NonNull,
};

use alloc::boxed::Box;

use rand::{distributions::Uniform, prelude::Distribution, SeedableRng};
use rand_xoshiro::Xoshiro256StarStar;
use uefi::{
    prelude::{BootServices, RuntimeServices},
    proto::network::snp::SimpleNetwork,
    table::{
        boot::{EventType, ScopedProtocol, Tpl, TplGuard},
        Boot, SystemTable,
    },
    Event, Status,
};

pub mod net;
pub mod timer;

static mut BOOT_SERVICES: Option<NonNull<BootServices>> = None;
static mut RUNTIME_SERVICES: Option<NonNull<RuntimeServices>> = None;
static mut SIMPLE_NETWORK: Option<&'static ScopedProtocol<'static, SimpleNetwork>> = None;
static mut RNG: Option<Xoshiro256StarStar> = None;

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

            // Try to generate a random number with rdseed up to 10 times, but if that fails, use
            // the timestamp counter.
            let mut seed = 0;
            if core_detect::is_x86_feature_detected!("rdseed") {
                for _i in 0..10 {
                    if _rdseed64_step(&mut seed) == 1 {
                        break;
                    } else {
                        seed = _rdtsc();
                    }
                }
            } else {
                seed = _rdtsc();
            }
            RNG = Some(Xoshiro256StarStar::seed_from_u64(seed));

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

    pub fn mask_interrupts(&self) -> TplGuard {
        unsafe {
            BOOT_SERVICES
                .unwrap_unchecked()
                .as_ref()
                .raise_tpl(Tpl::HIGH_LEVEL)
        }
    }

    pub fn rand<T, D: Distribution<T>>(&self, d: &D) -> T {
        // Temporarily mask all interrupts to guarantee that this function is not re-entered.
        let _tpl_guard = self.mask_interrupts();
        unsafe { d.sample(RNG.as_mut().unwrap_unchecked()) }
    }

    pub fn rand_u64(&self) -> u64 {
        self.rand(&Uniform::new_inclusive(0, u64::MAX))
    }

    pub fn reset(&self) -> ! {
        self.runtime_services()
            .reset(uefi::table::runtime::ResetType::Warm, Status::SUCCESS, None)
    }
}

impl !Send for UefiOS {}
impl !Sync for UefiOS {}
