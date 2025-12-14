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
    uefi::runtime::reset(uefi::runtime::ResetType::WARM, Status::SUCCESS, None)
}

pub fn shutdown() -> ! {
    uefi::runtime::reset(uefi::runtime::ResetType::SHUTDOWN, Status::SUCCESS, None)
}
