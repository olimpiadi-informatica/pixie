use crate::hal::Hal;
use crate::input::KeyEvent;

const PORT_DATA: u16 = 0x60;
const PORT_STATUS: u16 = 0x64;

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
    /// tracks Set-1 scancodes (`0xE0` extended prefix, Ctrl/Alt modifiers), and returns the next `KeyEvent` if any.
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
                    self.shift = !released;
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

            if was_e0 {
                match code {
                    0x48 => return Some(KeyEvent::Up),
                    0x50 => return Some(KeyEvent::Down),
                    0x4B => return Some(KeyEvent::Left),
                    0x4D => return Some(KeyEvent::Right),
                    0x1C => return Some(KeyEvent::Enter),
                    0x35 => return Some(KeyEvent::Char('/')),
                    _ => continue,
                }
            }

            match code {
                0x48 => return Some(KeyEvent::Up),
                0x50 => return Some(KeyEvent::Down),
                0x4B => return Some(KeyEvent::Left),
                0x4D => return Some(KeyEvent::Right),
                0x1C => return Some(KeyEvent::Enter),
                0x01 => return Some(KeyEvent::Char('\x1b')),
                0x0E => return Some(KeyEvent::Char('\x08')),
                0x0F => return Some(KeyEvent::Char('\t')),
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
    let ch = match (code, shift) {
        (0x02, false) => '1',
        (0x02, true) => '!',
        (0x03, false) => '2',
        (0x03, true) => '@',
        (0x04, false) => '3',
        (0x04, true) => '#',
        (0x05, false) => '4',
        (0x05, true) => '$',
        (0x06, false) => '5',
        (0x06, true) => '%',
        (0x07, false) => '6',
        (0x07, true) => '^',
        (0x08, false) => '7',
        (0x08, true) => '&',
        (0x09, false) => '8',
        (0x09, true) => '*',
        (0x0A, false) => '9',
        (0x0A, true) => '(',
        (0x0B, false) => '0',
        (0x0B, true) => ')',
        (0x0C, false) => '-',
        (0x0C, true) => '_',
        (0x0D, false) => '=',
        (0x0D, true) => '+',
        (0x10, false) => 'q',
        (0x10, true) => 'Q',
        (0x11, false) => 'w',
        (0x11, true) => 'W',
        (0x12, false) => 'e',
        (0x12, true) => 'E',
        (0x13, false) => 'r',
        (0x13, true) => 'R',
        (0x14, false) => 't',
        (0x14, true) => 'T',
        (0x15, false) => 'y',
        (0x15, true) => 'Y',
        (0x16, false) => 'u',
        (0x16, true) => 'U',
        (0x17, false) => 'i',
        (0x17, true) => 'I',
        (0x18, false) => 'o',
        (0x18, true) => 'O',
        (0x19, false) => 'p',
        (0x19, true) => 'P',
        (0x1A, false) => '[',
        (0x1A, true) => '{',
        (0x1B, false) => ']',
        (0x1B, true) => '}',
        (0x1E, false) => 'a',
        (0x1E, true) => 'A',
        (0x1F, false) => 's',
        (0x1F, true) => 'S',
        (0x20, false) => 'd',
        (0x20, true) => 'D',
        (0x21, false) => 'f',
        (0x21, true) => 'F',
        (0x22, false) => 'g',
        (0x22, true) => 'G',
        (0x23, false) => 'h',
        (0x23, true) => 'H',
        (0x24, false) => 'j',
        (0x24, true) => 'J',
        (0x25, false) => 'k',
        (0x25, true) => 'K',
        (0x26, false) => 'l',
        (0x26, true) => 'L',
        (0x27, false) => ';',
        (0x27, true) => ':',
        (0x28, false) => '\'',
        (0x28, true) => '"',
        (0x29, false) => '`',
        (0x29, true) => '~',
        (0x2B, false) => '\\',
        (0x2B, true) => '|',
        (0x2C, false) => 'z',
        (0x2C, true) => 'Z',
        (0x2D, false) => 'x',
        (0x2D, true) => 'X',
        (0x2E, false) => 'c',
        (0x2E, true) => 'C',
        (0x2F, false) => 'v',
        (0x2F, true) => 'V',
        (0x30, false) => 'b',
        (0x30, true) => 'B',
        (0x31, false) => 'n',
        (0x31, true) => 'N',
        (0x32, false) => 'm',
        (0x32, true) => 'M',
        (0x33, false) => ',',
        (0x33, true) => '<',
        (0x34, false) => '.',
        (0x34, true) => '>',
        (0x35, false) => '/',
        (0x35, true) => '?',
        (0x39, _) => ' ',
        _ => return None,
    };
    Some(ch)
}
