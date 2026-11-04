use crate::hal::Hal;
use crate::input::KeyEvent;

pub const COM1_PORT: u16 = 0x3F8;

pub struct SerialPort<H: Hal> {
    pub port: u16,
    pub present: bool,
    pub hal: H,
    esc_state: u8,
}

impl<H: Hal> SerialPort<H> {
    /// Probes the 16550 UART at `port` using loopback mode (`0xAE`) and configures 115200 8N1.
    pub fn new(port: u16, hal: H) -> Self {
        hal.io_write8(port + 1, 0x00); // Disable interrupts
        hal.io_write8(port + 3, 0x80); // Enable DLAB
        hal.io_write8(port, 0x01); // Divisor low byte (115200 baud)
        hal.io_write8(port + 1, 0x00); // Divisor high byte
        hal.io_write8(port + 3, 0x03); // 8 bits, no parity, one stop bit
        hal.io_write8(port + 2, 0xC7); // Enable FIFO, clear them, 14-byte threshold
        hal.io_write8(port + 4, 0x1E); // Loopback mode
        hal.io_write8(port, 0xAE); // Send test byte
        let present = if hal.io_read8(port) == 0xAE {
            hal.io_write8(port + 4, 0x0F); // Normal operation (DTR, RTS, OUT1, OUT2)
            true
        } else {
            false
        };

        Self {
            port,
            present,
            hal,
            esc_state: 0,
        }
    }

    pub fn write_byte(&self, byte: u8) {
        if !self.present {
            return;
        }
        for _ in 0..10_000 {
            let lsr = self.hal.io_read8(self.port + 5);
            if lsr == 0xFF {
                return;
            }
            if (lsr & 0x20) != 0 {
                break;
            }
        }
        self.hal.io_write8(self.port, byte);
    }

    pub fn write_str(&self, s: &str) {
        if !self.present {
            return;
        }
        for b in s.bytes() {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
    }

    /// Polls UART RX data (`LSR & 0x01`) and decodes ASCII characters and ANSI arrow escape sequences
    /// (`\x1b[A` Up, `\x1b[B` Down, `\x1b[C` Right, `\x1b[D` Left, `\x1b[3~` Delete, `\r`/`\n` Enter).
    pub fn poll_key(&mut self) -> Option<KeyEvent> {
        if !self.present {
            return None;
        }

        for _ in 0..32 {
            let lsr = self.hal.io_read8(self.port + 5);
            if lsr == 0xFF || (lsr & 0x01) == 0 {
                return None;
            }
            let b = self.hal.io_read8(self.port);

            match self.esc_state {
                0 => match b {
                    0x1B => {
                        self.esc_state = 1;
                    }
                    b'\r' | b'\n' => return Some(KeyEvent::Enter),
                    0x7F => return Some(KeyEvent::Delete),
                    0x08 => return Some(KeyEvent::Char('\x08')),
                    b'\t' => return Some(KeyEvent::Char('\t')),
                    0x20..=0x7E => return Some(KeyEvent::Char(b as char)),
                    _ => {}
                },
                1 => match b {
                    b'[' | b'O' => {
                        self.esc_state = 2;
                    }
                    _ => {
                        self.esc_state = 0;
                    }
                },
                2 => match b {
                    b'A' => {
                        self.esc_state = 0;
                        return Some(KeyEvent::Up);
                    }
                    b'B' => {
                        self.esc_state = 0;
                        return Some(KeyEvent::Down);
                    }
                    b'C' => {
                        self.esc_state = 0;
                        return Some(KeyEvent::Right);
                    }
                    b'D' => {
                        self.esc_state = 0;
                        return Some(KeyEvent::Left);
                    }
                    b'3' => {
                        self.esc_state = 3;
                    }
                    _ => {
                        self.esc_state = 0;
                    }
                },
                3 => {
                    self.esc_state = 0;
                    if b == b'~' {
                        return Some(KeyEvent::Delete);
                    }
                }
                _ => {
                    self.esc_state = 0;
                }
            }
        }
        None
    }
}
