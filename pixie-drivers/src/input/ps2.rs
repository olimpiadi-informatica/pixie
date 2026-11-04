use crate::hal::Hal;
use crate::input::KeyEvent;

const PORT_DATA: u16 = 0x60;
const PORT_STATUS: u16 = 0x64;

const SET1_NORMAL: &[u8; 0x3A] =
    b"\0\x1b1234567890-=\x08\tqwertyuiop[]\0\0asdfghjkl;'`\0\\zxcvbnm,./\0\0\0 ";
const SET1_SHIFT: &[u8; 0x3A] =
    b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\0\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0\0\0 ";

pub struct Ps2Keyboard<H: Hal> {
    pub hal: H,
    e0_prefix: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl<H: Hal> Ps2Keyboard<H> {
    pub fn new(hal: H) -> Self {
        Self {
            hal,
            e0_prefix: false,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// Polls PS/2 ports `0x64` / `0x60` (ignoring mouse auxiliary bytes with bit 5 `0x20` set in status),
    /// tracks Set-1 scancodes (`0xE0` extended prefix, ignoring `0xE0 0x2A`/`0xE0 0x36` Fake Shift prefixes,
    /// Ctrl/Alt/Shift modifiers), and returns the next `KeyEvent` if any.
    pub fn poll_key(&mut self) -> Option<KeyEvent> {
        for _ in 0..64 {
            let status = self.hal.io_read8(PORT_STATUS);
            if (status & 0x01) == 0 {
                return None;
            }
            let scancode = self.hal.io_read8(PORT_DATA);
            // Ignore auxiliary (PS/2 mouse) data bytes
            if (status & 0x20) != 0 {
                continue;
            }

            if scancode == 0xE0 {
                self.e0_prefix = true;
                continue;
            }

            let was_e0 = self.e0_prefix;
            self.e0_prefix = false;

            let released = (scancode & 0x80) != 0;
            let code = scancode & 0x7F;

            match code {
                0x1D => {
                    self.ctrl = !released;
                    continue;
                }
                0x38 => {
                    self.alt = !released;
                    continue;
                }
                0x2A | 0x36 => {
                    // Ignore PS/2 Set-1 Fake Shift prefixes (`0xE0 0x2A`/`0xAA`, `0xE0 0x36`/`0xB6`)
                    if !was_e0 {
                        self.shift = !released;
                    }
                    continue;
                }
                _ => {}
            }

            if released {
                continue;
            }

            if code == 0x53 {
                if self.ctrl && self.alt {
                    return Some(KeyEvent::CtrlAltDel);
                }
                return Some(KeyEvent::Delete);
            }

            match code {
                0x48 => return Some(KeyEvent::Up),
                0x50 => return Some(KeyEvent::Down),
                0x4B => return Some(KeyEvent::Left),
                0x4D => return Some(KeyEvent::Right),
                0x1C => return Some(KeyEvent::Enter),
                0x35 if was_e0 => return Some(KeyEvent::Char('/')),
                _ if was_e0 => continue,
                _ => {
                    if let Some(ch) = decode_set1_char(code, self.shift) {
                        return Some(KeyEvent::Char(ch));
                    }
                }
            }
        }
        None
    }
}

fn decode_set1_char(code: u8, shift: bool) -> Option<char> {
    let table = if shift { SET1_SHIFT } else { SET1_NORMAL };
    let b = *table.get(code as usize)?;
    (b != 0).then_some(b as char)
}
