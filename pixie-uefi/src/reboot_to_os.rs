use crate::os::{MessageKind, UefiOS};

pub async fn reboot_to_os(os: UefiOS) -> ! {
    let bo = os.boot_options();
    let order = bo.order();
    let num_skip = order
        .iter()
        .cloned()
        .position(|x| x == bo.current())
        .map(|x| x + 1)
        .unwrap_or(0);
    let next = order.iter().cloned().skip(num_skip).find(|x| *x < 0x2000);
    if let Some(next) = next {
        // Reboot to next boot option.
        bo.set_next(next);
    } else {
        os.append_message(
            format!(
                "Did not find a valid boot order entry! current: {}",
                bo.current()
            ),
            MessageKind::Warning,
        );
        os.append_message(format!("{:?}", order), MessageKind::Warning);
        os.sleep_us(100_000_000).await;
    }
    os.reset();
}
