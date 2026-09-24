use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use uefi::proto::console::text::{Key, ScanCode};

use crate::os::arch::io;
use crate::os::error::Result;
use crate::os::executor::Executor;
use crate::os::logger::SERIAL;

const PS2_STATUS: u16 = 0x64;
const PS2_DATA: u16 = 0x60;

static EXTENDED_SCANCODE: AtomicBool = AtomicBool::new(false);
static ESCAPE_SEQ_STATE: AtomicBool = AtomicBool::new(false);
static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
static ALT_DOWN: AtomicBool = AtomicBool::new(false);
static KEY_QUEUE: Mutex<VecDeque<Key>> = Mutex::new(VecDeque::new());

fn poll_ps2() -> Option<Key> {
    unsafe {
        let status = io::inb(PS2_STATUS);
        if (status & 0x01) == 0 {
            return None;
        }
        if (status & 0x20) != 0 {
            let _ = io::inb(PS2_DATA); // Discard mouse byte
            return None;
        }
        let scancode = io::inb(PS2_DATA);

        if scancode == 0xE0 {
            EXTENDED_SCANCODE.store(true, Ordering::Relaxed);
            return None;
        }

        let is_extended = EXTENDED_SCANCODE.swap(false, Ordering::Relaxed);
        let is_release = (scancode & 0x80) != 0;
        let make_code = scancode & 0x7F;

        // Track Ctrl and Alt keys
        if make_code == 0x1D {
            // Left Ctrl or Right Ctrl (if extended)
            CTRL_DOWN.store(!is_release, Ordering::Relaxed);
            return None;
        } else if make_code == 0x38 {
            // Left Alt or Right Alt / AltGr (if extended)
            ALT_DOWN.store(!is_release, Ordering::Relaxed);
            return None;
        }

        if !is_release {
            // Check for Delete key (make code 0x53): extended Delete or keypad Delete
            if make_code == 0x53 && CTRL_DOWN.load(Ordering::Relaxed) && ALT_DOWN.load(Ordering::Relaxed) {
                log::info!("Ctrl-Alt-Del detected via PS/2 keyboard. Rebooting system...");
                crate::power_control::reset();
            }

            if is_extended {
                match make_code {
                    0x48 => Some(Key::Special(ScanCode::UP)),
                    0x50 => Some(Key::Special(ScanCode::DOWN)),
                    0x4B => Some(Key::Special(ScanCode::LEFT)),
                    0x4D => Some(Key::Special(ScanCode::RIGHT)),
                    _ => None,
                }
            } else {
                match make_code {
                    0x1C => Some(Key::Printable('\r'.try_into().unwrap())),
                    _ => None,
                }
            }
        } else {
            None
        }
    }
}

fn poll_serial() -> Option<Key> {
    let serial = SERIAL.lock();
    let byte = serial.read_byte()?;

    if byte == 0x1B {
        ESCAPE_SEQ_STATE.store(true, Ordering::Relaxed);
        return None;
    }

    if ESCAPE_SEQ_STATE.load(Ordering::Relaxed) {
        if byte == b'[' {
            return None;
        }
        ESCAPE_SEQ_STATE.store(false, Ordering::Relaxed);
        match byte {
            b'A' => return Some(Key::Special(ScanCode::UP)),
            b'B' => return Some(Key::Special(ScanCode::DOWN)),
            b'D' => return Some(Key::Special(ScanCode::LEFT)),
            b'C' => return Some(Key::Special(ScanCode::RIGHT)),
            _ => return None,
        }
    }

    if byte == b'\r' || byte == b'\n' {
        Some(Key::Printable('\r'.try_into().unwrap()))
    } else {
        None
    }
}

pub fn poll_background() {
    while let Some(key) = poll_ps2().or_else(poll_serial) {
        KEY_QUEUE.lock().push_back(key);
    }
}

pub fn try_read_key() -> Option<Key> {
    if let Some(key) = KEY_QUEUE.lock().pop_front() {
        return Some(key);
    }
    poll_ps2().or_else(poll_serial)
}

pub fn check_key_pressed() -> bool {
    try_read_key().is_some()
}

pub async fn read_key() -> Result<Key> {
    loop {
        if let Some(key) = try_read_key() {
            return Ok(key);
        }
        Executor::wait_for_interrupt().await;
    }
}

#[allow(dead_code)]
pub fn wait_for_key() -> Result<()> {
    loop {
        io::hlt();
        if try_read_key().is_some() {
            return Ok(());
        }
    }
}
