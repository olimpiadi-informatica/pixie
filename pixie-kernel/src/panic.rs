use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{arch::io::cli, input, power_control, raw_fb};

static PANICKING: AtomicBool = AtomicBool::new(false);

struct EmergencyPanicWriter;

impl Write for EmergencyPanicWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        power_control::emergency_serial_write(s.as_bytes());
        raw_fb::try_print(s, raw_fb::COLOR_WHITE, raw_fb::COLOR_RED);
        Ok(())
    }
}

#[cold]
#[inline(never)]
pub fn handle_panic(info: &PanicInfo) -> ! {
    unsafe {
        cli();
    }
    if PANICKING.swap(true, Ordering::SeqCst) {
        power_control::emergency_serial_write(b"\nDOUBLE PANIC! Resetting...\n");
        power_control::emergency_reset();
    }

    raw_fb::try_clear_screen(raw_fb::COLOR_RED);
    let _ = writeln!(
        EmergencyPanicWriter,
        "\nPANIC: {info}\nPress any key to reset..."
    );
    let _ = input::wait_for_key();
    power_control::emergency_reset();
}
