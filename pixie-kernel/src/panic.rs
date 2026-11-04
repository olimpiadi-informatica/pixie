use core::panic::PanicInfo;

use crate::{input, power_control, raw_fb, ui};

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    log::error!("PANIC: {info}");
    ui::red_screen();
    let msg = alloc::format!("\nPANIC: {info}\nPress any key to reset...\n");
    raw_fb::print(&msg, raw_fb::COLOR_WHITE, raw_fb::COLOR_RED);
    let _ = input::wait_for_key();
    power_control::reset();
}
