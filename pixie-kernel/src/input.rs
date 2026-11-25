use alloc::vec::Vec;
use core::time::Duration;
use spin::Mutex;
use uefi::Char16;
use uefi::proto::console::text::{Key, ScanCode};

use crate::arch::io::{inb, pause};
use crate::error::Result;
use crate::executor::Executor;
use crate::hal::NativeHal;
use pixie_drivers::input::KeyEvent;
use pixie_drivers::input::ps2::Ps2Keyboard;
use pixie_drivers::input::serial::{COM1_PORT, SerialPort};
use pixie_drivers::input::xhci::XhciKeyboard;

struct InputDevices {
    ps2: Ps2Keyboard<NativeHal>,
    serial: SerialPort<NativeHal>,
    xhci: Vec<XhciKeyboard<NativeHal>>,
}

static INPUT_DEVICES: Mutex<Option<InputDevices>> = Mutex::new(None);

pub async fn init() {
    let ps2 = Ps2Keyboard::new(NativeHal);
    let serial = SerialPort::attach(COM1_PORT, crate::logger::serial_present(), NativeHal);
    let mut xhci = Vec::new();

    for ctrl in crate::usb::controllers() {
        if let Ok(kbd) = XhciKeyboard::attach(ctrl.clone()).await {
            xhci.push(kbd);
        }
    }

    *INPUT_DEVICES.lock() = Some(InputDevices { ps2, serial, xhci });
}

fn translate_event(ev: KeyEvent) -> Option<Key> {
    match ev {
        KeyEvent::CtrlAltDel => {
            crate::power_control::emergency_reset();
        }
        KeyEvent::Up => Some(Key::Special(ScanCode::UP)),
        KeyEvent::Down => Some(Key::Special(ScanCode::DOWN)),
        KeyEvent::Left => Some(Key::Special(ScanCode::LEFT)),
        KeyEvent::Right => Some(Key::Special(ScanCode::RIGHT)),
        KeyEvent::Delete => Some(Key::Special(ScanCode::DELETE)),
        KeyEvent::Enter => Some(Key::Printable(Char16::try_from('\r').unwrap())),
        KeyEvent::Char('\x1b') => Some(Key::Special(ScanCode::ESCAPE)),
        KeyEvent::Char(ch) => Char16::try_from(ch).ok().map(Key::Printable),
    }
}

fn poll_devs(devs: &mut InputDevices) -> Option<Key> {
    if let Some(ev) = devs.ps2.poll_key()
        && let Some(k) = translate_event(ev)
    {
        return Some(k);
    }
    if let Some(ev) = devs.serial.poll_key()
        && let Some(k) = translate_event(ev)
    {
        return Some(k);
    }
    for xhci in &mut devs.xhci {
        if let Some(ev) = xhci.poll_key()
            && let Some(k) = translate_event(ev)
        {
            return Some(k);
        }
    }
    None
}

fn poll_key_once() -> Option<Key> {
    let mut guard = INPUT_DEVICES.lock();
    poll_devs(guard.as_mut()?)
}

pub async fn read_key() -> Result<Key> {
    loop {
        if let Some(k) = poll_key_once() {
            return Ok(k);
        }
        Executor::sleep(Duration::from_millis(10)).await;
    }
}

/// Allocation-free synchronous wait for a keypress (safe to call from panic handlers / OOM paths).
pub fn wait_for_key() -> Result<()> {
    let mut fallback_ps2 = Ps2Keyboard::new(NativeHal);
    loop {
        if let Some(mut guard) = INPUT_DEVICES.try_lock()
            && let Some(devs) = guard.as_mut()
        {
            if poll_devs(devs).is_some() {
                return Ok(());
            }
        } else {
            if let Some(ev) = fallback_ps2.poll_key() {
                let _ = translate_event(ev);
                return Ok(());
            }
            unsafe {
                let lsr = inb(COM1_PORT + 5);
                if lsr != 0xFF && (lsr & 0x01) != 0 {
                    let b = inb(COM1_PORT);
                    if b == b'\r' || b == b'\n' || (0x20..=0x7E).contains(&b) || b == 0x1B {
                        return Ok(());
                    }
                }
            }
        }
        pause();
    }
}
