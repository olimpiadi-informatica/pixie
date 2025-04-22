use crate::os::UefiOS;

pub async fn reboot_to_os(os: UefiOS) -> ! {
    let bo = os.boot_options();
    let next = bo.reboot_target();
    if let Some(next) = next {
        // Reboot to next boot option.
        bo.set_next(next);
    } else {
        log::warn!(
            "Did not find a valid boot order entry! current: {}",
            bo.current()
        );
        log::warn!("{:?}", bo.order());
        os.sleep_us(100_000_000).await;
    }
    os.reset();
}
