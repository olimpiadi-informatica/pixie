use crate::os::boot_options::BootOptions;
use crate::os::UefiOS;

pub async fn reboot_to_os(os: UefiOS) -> ! {
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
        os.sleep_us(100_000_000).await;
    }
    os.reset();
}
