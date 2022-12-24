use crate::os::UefiOS;

pub async fn reboot_to_os(os: UefiOS) -> ! {
    let bo = os.boot_options();
    let order = bo.order();
    // Reboot to second boot option.
    bo.set_next(order[1]);
    os.reset();
}
