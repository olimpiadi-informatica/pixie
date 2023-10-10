use crate::os::UefiOS;

pub async fn reboot_to_os(os: UefiOS) -> ! {
    let bo = os.boot_options();
    let order = bo.order();
    let current_index = order
        .iter()
        .cloned()
        .enumerate()
        .find(|x| x.1 == bo.current())
        .unwrap_or((0, 0))
        .0;
    // Reboot to next boot option.
    bo.set_next(order[(current_index + 1).min(order.len() - 1)]);
    os.reset();
}
