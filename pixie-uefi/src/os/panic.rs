use core::fmt::Write;
use core::panic::PanicInfo;

use crate::os::arch::io;
use crate::os::logger::SERIAL;
use crate::os::raw_fb;
use crate::os::timer::Timer;
use crate::os::{input, ui};
use crate::power_control;

struct StackBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> StackBuf<N> {
    const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> Write for StackBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let avail = N.saturating_sub(self.len);
        let to_copy = bytes.len().min(avail);
        self.buf[self.len..self.len + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}

pub fn handle_fault(reason: &str) -> ! {
    io::cli();

    let serial = SERIAL.lock();
    serial.write_str("\n\x1b[1;31m======================= SYSTEM FAULT DETECTED =======================\x1b[0m\n");
    serial.write_str("\x1b[1;31m");
    serial.write_str(reason);
    serial.write_str("\x1b[0m\n");
    serial.write_str("\x1b[1;31m=====================================================================\x1b[0m\n\n");
    drop(serial);

    if let Some(st_ptr) = uefi::table::system_table_raw() {
        unsafe {
            let st = st_ptr.as_ref();
            if !st.boot_services.is_null() && !st.stdout.is_null() {
                uefi::println!("\n\n======================= SYSTEM FAULT DETECTED =======================");
                uefi::println!("{}", reason);
                uefi::println!("=====================================================================\n");
            }
        }
    }

    // Direct hardware fallback: write fault directly to physical framebuffer
    raw_fb::draw_fault_banner(reason);
    ui::display_fault_screen(reason);

    for sec in (1..=30).rev() {
        ui::update_fault_countdown(sec);
        let mut cdown = raw_fb::StackWriter::<128>::new();
        let _ = write!(
            cdown,
            "Rebooting in {sec:2} seconds... (press any key to reboot immediately)"
        );
        raw_fb::print_at(2, 6, cdown.as_str(), raw_fb::COLOR_YELLOW, raw_fb::COLOR_BG_RED);

        let serial = SERIAL.lock();
        let mut msg_buf = StackBuf::<128>::new();
        let _ = writeln!(
            msg_buf,
            "Rebooting in {sec:2} seconds... (press any key to reboot immediately)"
        );
        serial.write_str(msg_buf.as_str());
        drop(serial);

        let deadline = Timer::micros() + 1_000_000;
        while Timer::micros() < deadline {
            if input::check_key_pressed() {
                power_control::reset();
            }
            io::pause();
        }
    }

    power_control::reset();
}

pub fn handle_exception(vector: u64, error_code: u64, rip: u64) -> ! {
    let name = match vector {
        0 => "Division by Zero (#DE)",
        1 => "Debug (#DB)",
        2 => "Non-Maskable Interrupt (#NMI)",
        3 => "Breakpoint (#BP)",
        4 => "Overflow (#OF)",
        5 => "Bound Range Exceeded (#BR)",
        6 => "Invalid Opcode (#UD)",
        7 => "Device Not Available (#NM)",
        8 => "Double Fault (#DF)",
        10 => "Invalid TSS (#TS)",
        11 => "Segment Not Present (#NP)",
        12 => "Stack-Segment Fault (#SS)",
        13 => "General Protection Fault (#GP)",
        14 => "Page Fault (#PF)",
        16 => "x87 Floating-Point Exception (#MF)",
        17 => "Alignment Check (#AC)",
        18 => "Machine Check (#MC)",
        19 => "SIMD Floating-Point Exception (#XM)",
        _ => "Unknown CPU Exception",
    };

    let mut msg_buf = StackBuf::<256>::new();
    let _ = core::write!(
        msg_buf,
        "CPU Exception {vector} ({name}), error code: 0x{error_code:X}, RIP: 0x{rip:016X}"
    );
    handle_fault(msg_buf.as_str());
}

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    let mut msg_buf = StackBuf::<512>::new();
    let _ = core::write!(msg_buf, "{info}");
    handle_fault(msg_buf.as_str());
}
