use alloc::vec::Vec;
use spin::Mutex;
use uefi::Char16;
use uefi::proto::console::text::{Key, ScanCode};

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
    let serial = SerialPort::new(COM1_PORT, NativeHal);
    let mut xhci = Vec::new();

    for dev in pixie_drivers::pci::scan_pci(&NativeHal) {
        // xHCI USB 3.0 controller: class 0x0C, subclass 0x03, prog_if 0x30
        if dev.class_code == 0x0C && dev.subclass == 0x03 && dev.prog_if == 0x30 {
            dev.enable_bus_mastering(&NativeHal);
            if let Some(pixie_drivers::pci::Bar::Memory { base: bar0, .. }) =
                dev.read_bar(&NativeHal, 0)
            {
                crate::arch::paging::map_mmio(bar0, 0x10000);
                if let Ok(kbd) = XhciKeyboard::new(bar0, NativeHal).await {
                    xhci.push(kbd);
                }
            }
        }
    }

    *INPUT_DEVICES.lock() = Some(InputDevices { ps2, serial, xhci });
}

fn translate_event(ev: KeyEvent) -> Option<Key> {
    match ev {
        KeyEvent::CtrlAltDel => {
            crate::power_control::reset();
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

fn poll_key_once() -> Option<Key> {
    let mut guard = INPUT_DEVICES.lock();
    let devs = guard.as_mut()?;

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

pub fn poll_background() {
    let mut guard = INPUT_DEVICES.lock();
    if let Some(devs) = guard.as_mut() {
        if let Some(KeyEvent::CtrlAltDel) = devs.ps2.poll_key() {
            crate::power_control::reset();
        }
        for xhci in &mut devs.xhci {
            if let Some(KeyEvent::CtrlAltDel) = xhci.poll_key() {
                crate::power_control::reset();
            }
        }
    }
}

pub async fn read_key() -> Result<Key> {
    loop {
        if let Some(k) = poll_key_once() {
            return Ok(k);
        }
        Executor::sched_yield().await;
    }
}

pub fn wait_for_key() -> Result<()> {
    loop {
        if poll_key_once().is_some() {
            return Ok(());
        }
        crate::arch::io::pause();
    }
}
