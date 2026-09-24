use core::time::Duration;

use uefi::Status;

use crate::os::boot_options::BootOptions;
use crate::os::executor::Executor;

pub async fn reboot_to_os() -> ! {
    let next = BootOptions::reboot_target();
    if let Some(next) = next {
        // Reboot to next boot option.
        BootOptions::set_next(next);
    } else {
        log::warn!(
            "Did not find a valid boot order entry! current: {}",
            BootOptions::current()
        );
        log::warn!("{:?}", BootOptions::order());
        Executor::sleep(Duration::from_secs(100)).await;
    }
    reset();
}

pub fn reset() -> ! {
    unsafe {
        // Standard x86 hardware resets:
        // 1. PCI reset register 0xCF9 (System Reset / Full Reset)
        crate::os::arch::io::outb(0xCF9, 0x02);
        crate::os::arch::io::outb(0xCF9, 0x06);
        crate::os::arch::io::outb(0xCF9, 0x0E);
        // 2. 8042 PS/2 controller pulse CPU reset line
        crate::os::arch::io::outb(0x64, 0xFE);
    }
    uefi::runtime::reset(uefi::runtime::ResetType::WARM, Status::SUCCESS, None)
}

pub fn shutdown() -> ! {
    uefi::runtime::reset(uefi::runtime::ResetType::SHUTDOWN, Status::SUCCESS, None)
}
