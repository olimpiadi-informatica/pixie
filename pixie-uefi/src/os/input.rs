use core::sync::atomic::{AtomicBool, Ordering};

use uefi::proto::console::text::{Key, ScanCode};

use crate::os::arch::io;
use crate::os::error::Result;
use crate::os::executor::Executor;
use crate::os::logger::SERIAL;

const PS2_STATUS: u16 = 0x64;
const PS2_DATA: u16 = 0x60;

static EXTENDED_SCANCODE: AtomicBool = AtomicBool::new(false);
static ESCAPE_SEQ_STATE: AtomicBool = AtomicBool::new(false);

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

        if (scancode & 0x80) != 0 {
            // Key release (break code), ignore
            return None;
        }

        if is_extended {
            match scancode {
                0x48 => Some(Key::Special(ScanCode::UP)),
                0x50 => Some(Key::Special(ScanCode::DOWN)),
                0x4B => Some(Key::Special(ScanCode::LEFT)),
                0x4D => Some(Key::Special(ScanCode::RIGHT)),
                _ => None,
            }
        } else {
            match scancode {
                0x1C => Some(Key::Printable('\r'.try_into().unwrap())),
                _ => None,
            }
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

pub fn try_read_key() -> Option<Key> {
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
