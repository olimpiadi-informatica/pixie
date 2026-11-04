use crate::hal::Hal;
use crate::input::KeyEvent;

pub const COM1_PORT: u16 = 0x3F8;

pub struct SerialPort<H: Hal> {
    pub port: u16,
    pub present: bool,
    pub hal: H,
    esc_state: u8,
    pending_byte: Option<u8>,
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
            pending_byte: None,
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

    fn read_rx_byte(&mut self) -> Option<u8> {
        if let Some(b) = self.pending_byte.take() {
            return Some(b);
        }
        let lsr = self.hal.io_read8(self.port + 5);
        if lsr != 0xFF && (lsr & 0x01) != 0 {
            return Some(self.hal.io_read8(self.port));
        }
        if self.esc_state != 0 {
            // Briefly wait (~200 us or 256 reads) for trailing ANSI sequence bytes at 115200 baud
            let start = self.hal.micros();
            for _ in 0..256 {
                let lsr = self.hal.io_read8(self.port + 5);
                if lsr == 0xFF {
                    break;
                }
                if (lsr & 0x01) != 0 {
                    return Some(self.hal.io_read8(self.port));
                }
                if self.hal.micros().saturating_sub(start) > 200 {
                    break;
                }
            }
        }
        None
    }

    /// Polls UART RX data (`LSR & 0x01`) and decodes ASCII characters, standalone `ESC`,
    /// and ANSI arrow escape sequences (`\x1b[A` Up, `\x1b[B` Down, `\x1b[C` Right,
    /// `\x1b[D` Left, `\x1b[3~` Delete, `\r`/`\n` Enter).
    pub fn poll_key(&mut self) -> Option<KeyEvent> {
        if !self.present {
            return None;
        }

        for _ in 0..32 {
            let Some(b) = self.read_rx_byte() else {
                if self.esc_state == 1 {
                    self.esc_state = 0;
                    return Some(KeyEvent::Char('\x1b'));
                }
                self.esc_state = 0;
                return None;
            };

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
                    0x1B => {
                        // First 0x1B was a standalone ESC; keep esc_state = 1 for this second 0x1B.
                        return Some(KeyEvent::Char('\x1b'));
                    }
                    other => {
                        self.esc_state = 0;
                        self.pending_byte = Some(other);
                        return Some(KeyEvent::Char('\x1b'));
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
                    0x1B => {
                        self.esc_state = 1;
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
                    if b == 0x1B {
                        self.esc_state = 1;
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
