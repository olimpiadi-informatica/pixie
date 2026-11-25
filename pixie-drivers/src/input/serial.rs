use crate::hal::Hal;
use crate::input::KeyEvent;

pub const COM1_PORT: u16 = 0x3F8;

const ESC_TIMEOUT_US: i64 = 30_000;

pub struct SerialPort<H: Hal> {
    pub port: u16,
    pub present: bool,
    pub hal: H,
    esc_state: u8,
    esc_started_us: i64,
    pending_byte: Option<u8>,
    last_was_cr: bool,
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
        // Drain any stale RX byte before writing the loopback probe byte.
        for _ in 0..16 {
            let lsr = hal.io_read8(port + 5);
            if lsr == 0xFF || (lsr & 0x01) == 0 {
                break;
            }
            let _ = hal.io_read8(port);
        }
        hal.io_write8(port, 0xAE); // Send test byte

        let start = hal.micros();
        let mut dr_ready = false;
        for _ in 0..2_000 {
            let lsr = hal.io_read8(port + 5);
            if lsr == 0xFF {
                break;
            }
            if (lsr & 0x01) != 0 {
                dr_ready = true;
                break;
            }
            if hal.micros().saturating_sub(start) >= 1_000 {
                break;
            }
            core::hint::spin_loop();
        }
        let present = if dr_ready && hal.io_read8(port) == 0xAE {
            hal.io_write8(port + 4, 0x0F); // Normal operation (DTR, RTS, OUT1, OUT2)
            hal.io_write8(port + 2, 0xC7); // Clear FIFOs after loopback test
            true
        } else {
            hal.io_write8(port + 4, 0x0F);
            false
        };

        Self {
            port,
            present,
            hal,
            esc_state: 0,
            esc_started_us: 0,
            pending_byte: None,
            last_was_cr: false,
        }
    }

    /// Attaches to an already-configured 16550 UART without resetting FIFOs or entering loopback mode.
    pub fn attach(port: u16, present: bool, hal: H) -> Self {
        Self {
            port,
            present,
            hal,
            esc_state: 0,
            esc_started_us: 0,
            pending_byte: None,
            last_was_cr: false,
        }
    }

    #[inline]
    pub fn is_present(&self) -> bool {
        self.present
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
                self.hal.io_write8(self.port, byte);
                return;
            }
        }
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
    /// and ANSI arrow/delete escape sequences (`\x1b[A` Up, `\x1b[B` Down, `\x1b[C` Right,
    /// `\x1b[D` Left, `\x1b[3~` Delete, `\r`/`\n` Enter).
    pub fn poll_key(&mut self) -> Option<KeyEvent> {
        if !self.present {
            return None;
        }

        for _ in 0..32 {
            let Some(b) = self.read_rx_byte() else {
                if self.esc_state != 0 {
                    let elapsed = self.hal.micros().saturating_sub(self.esc_started_us);
                    if elapsed == 0 || elapsed >= ESC_TIMEOUT_US {
                        let was_esc = self.esc_state == 1;
                        self.esc_state = 0;
                        if was_esc {
                            return Some(KeyEvent::Char('\x1b'));
                        }
                    }
                }
                return None;
            };

            match self.esc_state {
                0 => {
                    if b == b'\n' && self.last_was_cr {
                        self.last_was_cr = false;
                        continue;
                    }
                    self.last_was_cr = b == b'\r';
                    match b {
                        0x1B => {
                            self.esc_state = 1;
                            self.esc_started_us = self.hal.micros();
                        }
                        b'\r' | b'\n' => return Some(KeyEvent::Enter),
                        0x7F => return Some(KeyEvent::Delete),
                        0x08 => return Some(KeyEvent::Char('\x08')),
                        b'\t' => return Some(KeyEvent::Char('\t')),
                        0x20..=0x7E => return Some(KeyEvent::Char(b as char)),
                        _ => {}
                    }
                }
                1 => {
                    self.last_was_cr = false;
                    match b {
                        b'[' => {
                            self.esc_state = 2;
                            self.esc_started_us = self.hal.micros();
                        }
                        b'O' => {
                            self.esc_state = 3;
                            self.esc_started_us = self.hal.micros();
                        }
                        0x1B => {
                            // First 0x1B was a standalone ESC; keep esc_state = 1 for this second 0x1B.
                            self.esc_started_us = self.hal.micros();
                            return Some(KeyEvent::Char('\x1b'));
                        }
                        other => {
                            self.esc_state = 0;
                            self.pending_byte = Some(other);
                            return Some(KeyEvent::Char('\x1b'));
                        }
                    }
                }
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
                        self.esc_state = 4;
                        self.esc_started_us = self.hal.micros();
                    }
                    0x20..=0x3F => {
                        // CSI parameter/intermediate bytes: drain until final byte 0x40..=0x7E.
                        self.esc_state = 5;
                        self.esc_started_us = self.hal.micros();
                    }
                    0x1B => {
                        self.esc_state = 1;
                        self.esc_started_us = self.hal.micros();
                    }
                    _ => {
                        self.esc_state = 0;
                    }
                },
                3 => match b {
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
                    0x1B => {
                        self.esc_state = 1;
                        self.esc_started_us = self.hal.micros();
                    }
                    _ => {
                        self.esc_state = 0;
                    }
                },
                4 => match b {
                    b'~' => {
                        self.esc_state = 0;
                        return Some(KeyEvent::Delete);
                    }
                    0x20..=0x3F => {
                        self.esc_state = 5;
                        self.esc_started_us = self.hal.micros();
                    }
                    0x1B => {
                        self.esc_state = 1;
                        self.esc_started_us = self.hal.micros();
                    }
                    _ => {
                        self.esc_state = 0;
                    }
                },
                5 => match b {
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
                    0x20..=0x3F => {
                        self.esc_started_us = self.hal.micros();
                    }
                    0x1B => {
                        self.esc_state = 1;
                        self.esc_started_us = self.hal.micros();
                    }
                    _ => {
                        self.esc_state = 0;
                    }
                },
                _ => {
                    self.esc_state = 0;
                }
            }
        }
        None
    }
}
