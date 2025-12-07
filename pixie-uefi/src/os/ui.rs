use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use pixie_shared::util::BytesFmt;
use spin::lazy::Lazy;
use spin::Mutex;
use uefi::boot::ScopedProtocol;
use uefi::proto::console::text::{Color, Output};
use uefi::{CStr16, Char16};

use super::executor::{Executor, TASK_LEN};
use super::memory;
use super::send_wrapper::SendWrapper;
use super::timer::Timer;

const TASK_HEIGHT: usize = 7; // includes box
const LOG_HEIGHT: usize = 10;

#[derive(Clone, Copy)]
struct ScreenChar {
    c: Char16,
    fg: Color,
    bg: Color,
}

impl Default for ScreenChar {
    fn default() -> Self {
        Self {
            c: Char16::try_from(0x20).unwrap(),
            fg: Color::White,
            bg: Color::Black,
        }
    }
}

impl PartialEq for ScreenChar {
    fn eq(&self, other: &Self) -> bool {
        other.c == self.c && other.fg as u8 == self.fg as u8 && other.bg as u8 == self.bg as u8
    }
}

pub struct Screen {
    vga: SendWrapper<ScopedProtocol<Output>>,
    front_buffer: Vec<ScreenChar>,
    back_buffer: Vec<ScreenChar>,
}

static WIDTH: AtomicUsize = AtomicUsize::new(0);
static HEIGHT: AtomicUsize = AtomicUsize::new(0);

fn w() -> usize {
    WIDTH.load(Ordering::Relaxed)
}

fn h() -> usize {
    HEIGHT.load(Ordering::Relaxed)
}

static SCREEN: Lazy<Mutex<Screen>> = Lazy::new(|| {
    let vga_handles = uefi::boot::find_handles::<Output>().unwrap();
    let vga = uefi::boot::open_protocol_exclusive::<Output>(vga_handles[0]).unwrap();
    let mode = vga.current_mode().unwrap().unwrap();
    let (w, h) = (mode.columns(), mode.rows());
    WIDTH.store(w, Ordering::Relaxed);
    HEIGHT.store(h, Ordering::Relaxed);
    Mutex::new(Screen {
        vga: SendWrapper(vga),
        front_buffer: vec![ScreenChar::default(); w * h],
        back_buffer: vec![ScreenChar::default(); w * h],
    })
});

static CONTENT_DRAW_AREA: Mutex<DrawArea> = Mutex::new(DrawArea::invalid());

pub(super) fn init() {
    let mut screen = SCREEN.lock();
    screen.vga.clear().unwrap();
    let _ = screen.vga.enable_cursor(false);

    Executor::spawn("[flush_ui]", async move {
        loop {
            SCREEN.lock().flush();
            Executor::sleep_us(100_000).await;
        }
    });

    *CONTENT_DRAW_AREA.lock() = DrawArea::content();

    Executor::spawn("[show_timer]", async move {
        let mut draw_area = DrawArea::time();
        loop {
            draw_area.clear();
            let time = Timer::micros() as f32 * 0.000_001;
            let w = draw_area.size().0;
            write!(draw_area, "uptime:{0:1$}{time:12.1}s", "", w - 20).unwrap();
            Executor::sleep_us(50_000).await;
        }
    });

    Executor::spawn("[show_memory]", async {
        let mut draw_area = DrawArea::memory();
        loop {
            draw_area.clear();
            let memory = memory::stats();
            let w = draw_area.size().0;
            write!(
                draw_area,
                "RAM:{2:3$}{:10.1} / {:10.1}",
                BytesFmt(memory.used),
                BytesFmt(memory.free + memory.used),
                "",
                w - 27,
            )
            .unwrap();
            Executor::sleep_us(1_000_000).await;
        }
    });

    let logs_area = DrawArea::logs();
    for y in 0..logs_area.size.1 {
        screen.back_buffer[(logs_area.offset.1 + y) * w()].c = Char16::try_from(0x25ba).unwrap();
    }
}

impl Screen {
    fn flush(&mut self) {
        let mut i = 0;
        while i < w() * h() {
            if self.back_buffer[i] == self.front_buffer[i] {
                i += 1;
                continue;
            }

            let start = i;
            let fg = self.back_buffer[i].fg;
            let bg = self.back_buffer[i].bg;

            let mut run = Vec::new();
            while i < w() * h()
                && self.back_buffer[i].fg as u8 == fg as u8
                && self.back_buffer[i].bg as u8 == bg as u8
                && self.back_buffer[i] != self.front_buffer[i]
            {
                if i > start && i % w() == 0 {
                    break;
                }
                run.push(self.back_buffer[i].c);
                self.front_buffer[i] = self.back_buffer[i];
                i += 1;
            }

            run.push(Char16::try_from(0).unwrap());

            let x = start % w();
            let y = start / w();
            self.vga.set_cursor_position(x, y).unwrap();
            self.vga.set_color(fg, bg).unwrap();
            self.vga
                .output_string(CStr16::from_char16_until_nul(&run).unwrap())
                .unwrap();
        }
    }
}

const STATUS_WIDTH: usize = 32;

pub struct DrawArea {
    offset: (usize, usize),
    size: (usize, usize),
    pos: (usize, usize),
    scroll: bool,
}

impl DrawArea {
    pub(super) const fn invalid() -> Self {
        Self::new((0, 0), (0, 0), false)
    }

    const fn new(offset: (usize, usize), size: (usize, usize), scroll: bool) -> Self {
        Self {
            offset,
            size,
            pos: (0, 0),
            scroll,
        }
    }

    fn task_columns() -> usize {
        (w() - STATUS_WIDTH - 1) / (TASK_LEN + 1)
    }

    fn task_width() -> usize {
        Self::task_columns() * (TASK_LEN + 1) + 1
    }

    fn task_side(col: usize, num_cols: usize) -> DrawArea {
        Self::new(
            (Self::task_width() + 1, col),
            (w() - Self::task_width() - 1, num_cols),
            false,
        )
    }

    pub(super) fn ip() -> DrawArea {
        Self::task_side(1, 1)
    }

    pub(super) fn net_speed() -> DrawArea {
        Self::task_side(2, 2)
    }

    fn time() -> DrawArea {
        Self::task_side(4, 1)
    }

    pub(super) fn memory() -> DrawArea {
        Self::task_side(5, 1)
    }

    pub(super) fn tasks() -> DrawArea {
        Self::new((0, 0), (Self::task_width(), TASK_HEIGHT), false)
    }

    pub(super) fn logs() -> DrawArea {
        Self::new((2, h() - LOG_HEIGHT), (w() - 3, LOG_HEIGHT), true)
    }

    pub(super) fn content() -> DrawArea {
        let offset = 1 + TASK_HEIGHT;
        Self::new((0, offset), (w(), h() - offset - LOG_HEIGHT), false)
    }

    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    fn idx(&self, p: (usize, usize)) -> usize {
        (self.offset.1 + p.1) * w() + self.offset.0 + p.0
    }

    pub fn clear(&mut self) {
        self.pos = (0, 0);
        let mut screen = SCREEN.lock();
        for y in 0..self.size.1 {
            for x in 0..self.size.0 {
                screen.back_buffer[self.idx((x, y))] = ScreenChar::default();
            }
        }
    }

    pub fn write_with_color(&mut self, msg: &str, fg: Color, bg: Color) {
        let mut screen = SCREEN.lock();
        for c in msg.chars() {
            if c == '\n' {
                self.newline();
                continue;
            }
            if self.pos.0 == self.size.0 {
                self.newline();
            }
            while self.scroll && self.pos.1 >= self.size.1 {
                self.pos.1 -= 1;
                for y in 0..self.size.1 - 1 {
                    for x in 0..self.size.0 {
                        screen.back_buffer[self.idx((x, y))] =
                            screen.back_buffer[self.idx((x, y + 1))];
                    }
                }
                for x in 0..self.size.0 {
                    screen.back_buffer[self.idx((x, self.size.1 - 1))] = ScreenChar::default();
                }
            }
            if self.pos.1 >= self.size.1 {
                continue;
            }
            if let Ok(c16) = Char16::try_from(c) {
                screen.back_buffer[self.idx(self.pos)] = ScreenChar { c: c16, fg, bg };
            }
            self.pos.0 += 1;
        }
    }

    pub fn newline(&mut self) {
        self.pos.0 = 0;
        self.pos.1 += 1;
    }
    pub fn advance(&mut self, n: usize) {
        self.pos.0 += n;
    }
}

impl Write for DrawArea {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_with_color(s, Color::White, Color::Black);
        Ok(())
    }
}

pub fn flush() {
    SCREEN.lock().flush();
}

pub fn update_content<F: Fn(&mut DrawArea)>(f: F) {
    f(&mut CONTENT_DRAW_AREA
        .try_lock()
        .expect("content draw area is locked"));
}
