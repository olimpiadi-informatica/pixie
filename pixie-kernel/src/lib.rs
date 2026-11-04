#![no_std]
#![deny(unused_must_use)]

#[macro_use]
extern crate alloc;

use core::future::Future;

use crate::error::Result;

pub mod arch;
pub mod boot_info;
pub mod boot_options;
pub mod disk;
pub mod error;
pub mod executor;
pub mod font;
pub mod font_data;
pub mod hal;
pub mod input;
pub mod logger;
pub mod memory;
pub mod net;
#[cfg(target_os = "uefi")]
pub mod panic;
pub mod power_control;
pub mod raw_fb;
mod send_wrapper;
pub mod timer;
pub mod ui;
pub mod util;
pub mod watchdog;

pub fn start<F, Fut>(mut f: F) -> !
where
    F: FnMut() -> Fut + 'static,
    Fut: Future<Output = Result<()>>,
{
    let (boot_info, memory_map) = boot_info::collect_and_exit_boot_services();

    unsafe {
        arch::io::cli();
    }
    raw_fb::init(&boot_info);
    logger::init();
    memory::init(&memory_map);

    unsafe {
        arch::gdt::init();
        arch::paging::init();
        if boot_info.framebuffer_base != 0 && boot_info.framebuffer_size != 0 {
            arch::paging::map_mmio(boot_info.framebuffer_base, boot_info.framebuffer_size);
        }
        arch::idt::init();
        arch::apic::init();
        arch::apic::start_periodic_timer(32, 100);
    }

    power_control::init(boot_info.rsdp_addr);
    unsafe {
        arch::io::sti();
    }

    watchdog::init();
    ui::init(boot_info);
    net::init();

    executor::Executor::spawn("[main]", async move {
        input::init().await;
        disk::Disk::init().await;
        loop {
            match f().await {
                Ok(()) => {}
                Err(err) => {
                    log::error!("Error: {err:?}");
                    log::info!("Press any key to retry");
                    let _ = input::read_key().await;
                }
            }
        }
    });

    executor::Executor::run();
}
