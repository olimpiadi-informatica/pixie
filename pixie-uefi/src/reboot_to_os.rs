use crate::os::{MessageKind, UefiOS};

pub async fn reboot_to_os(os: UefiOS) -> ! {
    let bo = os.boot_options();
    let next = bo.reboot_target();
    if let Some(next) = next {
        // Reboot to next boot option.
        bo.set_next(next);
    } else {
        os.append_message(
            format!(
                "Did not find a valid boot order entry! current: {}",
                bo.current()
            ),
            MessageKind::Warn,
        );
        os.append_message(format!("{:?}", bo.order()), MessageKind::Warn);
        os.sleep_us(100_000_000).await;
    }
    os.reset();
}
