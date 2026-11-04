#![allow(clippy::collapsible_if)]

use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;

use pixie_shared::util::BytesFmt;
use spin::Mutex;
use uefi::proto::console::text::Color;

use crate::boot_info::{BootInfo, FbPixelFormat};
use crate::executor::{Executor, TASK_LEN};
use crate::font::{FONT_HEIGHT, FONT_WIDTH, get_glyph};
use crate::logger;
use crate::memory;
use crate::timer::Timer;

const TASK_HEIGHT: usize = 7;
const LOG_HEIGHT: usize = 10;
const STATUS_WIDTH: usize = 32;

#[derive(Clone, Copy)]
pub struct ScreenChar {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
}

impl PartialEq for ScreenChar {
    fn eq(&self, other: &Self) -> bool {
        self.c == other.c && self.fg as u8 == other.fg as u8 && self.bg as u8 == other.bg as u8
    }
}

impl Eq for ScreenChar {}

impl Default for ScreenChar {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::White,
            bg: Color::Black,
        }
    }
}

pub struct Screen {
    framebuffer_base: *mut u32,
    fb_stride: usize,
    fb_width: usize,
    fb_height: usize,
    pixel_format: FbPixelFormat,
    cols: usize,
    rows: usize,
    front_buffer: Vec<ScreenChar>,
    back_buffer: Vec<ScreenChar>,
}

unsafe impl Send for Screen {}
unsafe impl Sync for Screen {}

static SCREEN: Mutex<Option<Screen>> = Mutex::new(None);
static COLS: AtomicUsize = AtomicUsize::new(0);
static ROWS: AtomicUsize = AtomicUsize::new(0);

fn w() -> usize {
    COLS.load(Ordering::Relaxed)
}

fn h() -> usize {
    ROWS.load(Ordering::Relaxed)
}

fn color_to_rgb_tuple(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Black => (0x00, 0x00, 0x00),
        Color::Blue => (0x00, 0x00, 0xAA),
        Color::Green => (0x00, 0xAA, 0x00),
        Color::Cyan => (0x00, 0xAA, 0xAA),
        Color::Red => (0xAA, 0x00, 0x00),
        Color::Magenta => (0xAA, 0x00, 0xAA),
        Color::Brown => (0xAA, 0x55, 0x00),
        Color::LightGray => (0xAA, 0xAA, 0xAA),
        Color::DarkGray => (0x55, 0x55, 0x55),
        Color::LightBlue => (0x55, 0x55, 0xFF),
        Color::LightGreen => (0x55, 0xFF, 0x55),
        Color::LightCyan => (0x55, 0xFF, 0xFF),
        Color::LightRed => (0xFF, 0x55, 0x55),
        Color::LightMagenta => (0xFF, 0x55, 0xFF),
        Color::Yellow => (0xFF, 0xFF, 0x55),
        Color::White => (0xFF, 0xFF, 0xFF),
    }
}

fn pack_pixel(c: Color, format: FbPixelFormat) -> u32 {
    let (r, g, b) = color_to_rgb_tuple(c);
    match format {
        FbPixelFormat::Rgb => ((b as u32) << 16) | ((g as u32) << 8) | (r as u32),
        FbPixelFormat::Bgr => ((r as u32) << 16) | ((g as u32) << 8) | (b as u32),
    }
}

impl Screen {
    pub fn new(boot_info: BootInfo) -> Self {
        let cols = ((boot_info.fb_width as usize) / FONT_WIDTH).max(80);
        let rows = ((boot_info.fb_height as usize) / FONT_HEIGHT).max(25);
        COLS.store(cols, Ordering::Relaxed);
        ROWS.store(rows, Ordering::Relaxed);

        let total_chars = cols * rows;

        Self {
            framebuffer_base: boot_info.framebuffer_base as *mut u32,
            fb_stride: boot_info.fb_stride as usize,
            fb_width: boot_info.fb_width as usize,
            fb_height: boot_info.fb_height as usize,
            pixel_format: boot_info.pixel_format,
            cols,
            rows,
            front_buffer: vec![ScreenChar::default(); total_chars],
            back_buffer: vec![ScreenChar::default(); total_chars],
        }
    }

    fn draw_char_raw(&mut self, col: usize, row: usize, c: char, fg: u32, bg: u32) {
        if self.framebuffer_base.is_null() || col >= self.cols || row >= self.rows {
            return;
        }

        let glyph = get_glyph(c);
        let px = col * FONT_WIDTH;
        let py = row * FONT_HEIGHT;

        for (y, &row_byte) in glyph.iter().enumerate().take(FONT_HEIGHT) {
            let fb_y = py + y;
            if fb_y >= self.fb_height {
                break;
            }
            let row_offset = fb_y * self.fb_stride;

            for x in 0..FONT_WIDTH {
                let fb_x = px + x;
                if fb_x >= self.fb_width {
                    break;
                }
                let is_fg = (row_byte & (0x80 >> x)) != 0;
                let color = if is_fg { fg } else { bg };
                unsafe {
                    core::ptr::write_volatile(self.framebuffer_base.add(row_offset + fb_x), color);
                }
            }
        }
    }

    pub fn flush(&mut self) {
        let total = self.cols * self.rows;
        let fmt = self.pixel_format;
        for i in 0..total {
            if self.front_buffer[i] != self.back_buffer[i] {
                let cell = self.back_buffer[i];
                let col = i % self.cols;
                let row = i / self.cols;
                self.draw_char_raw(
                    col,
                    row,
                    cell.c,
                    pack_pixel(cell.fg, fmt),
                    pack_pixel(cell.bg, fmt),
                );
                self.front_buffer[i] = cell;
            }
        }
    }

    pub fn clear_all(&mut self, bg: Color) {
        if !self.framebuffer_base.is_null() {
            let bg_rgb = pack_pixel(bg, self.pixel_format);
            for y in 0..self.fb_height {
                let offset = y * self.fb_stride;
                for x in 0..self.fb_width {
                    unsafe {
                        core::ptr::write_volatile(self.framebuffer_base.add(offset + x), bg_rgb);
                    }
                }
            }
        }
        self.front_buffer.fill(ScreenChar {
            c: ' ',
            fg: Color::White,
            bg,
        });
        self.back_buffer.fill(ScreenChar {
            c: ' ',
            fg: Color::White,
            bg,
        });
    }
}

static CONTENT_DRAW_AREA: Mutex<DrawArea> = Mutex::new(DrawArea::invalid());

pub fn init(boot_info: BootInfo) {
    let mut screen = Screen::new(boot_info);
    screen.clear_all(Color::Black);
    *SCREEN.lock() = Some(screen);

    Executor::spawn("[flush_ui]", async move {
        loop {
            flush();
            Executor::sleep(Duration::from_millis(100)).await;
        }
    });

    *CONTENT_DRAW_AREA.lock() = DrawArea::content();

    Executor::spawn("[show_video_mode]", async move {
        let mut draw_area = DrawArea::video_mode();
        loop {
            draw_area.clear();
            let width = draw_area.size().0;
            let label = alloc::format!(
                "GOP {w}x{h}",
                w = boot_info.fb_width,
                h = boot_info.fb_height
            );
            write!(
                draw_area,
                "DISP:{0:1$}{label}",
                "",
                width.saturating_sub(5 + label.len())
            )
            .unwrap();
            Executor::sleep(Duration::from_secs(10)).await;
        }
    });

    Executor::spawn("[show_timer]", async move {
        let mut draw_area = DrawArea::time();
        loop {
            draw_area.clear();
            let tenths = (Timer::micros() as u64 + 50_000) / 100_000;
            let secs = tenths / 10;
            let dec = tenths % 10;
            let width = draw_area.size().0;
            write!(
                draw_area,
                "uptime:{0:1$}{secs:10}.{dec}s",
                "",
                width.saturating_sub(20)
            )
            .unwrap();
            Executor::sleep(Duration::from_millis(50)).await;
        }
    });

    Executor::spawn("[show_memory]", async {
        let mut draw_area = DrawArea::memory();
        loop {
            draw_area.clear();
            let mem = memory::stats();
            let width = draw_area.size().0;
            write!(
                draw_area,
                "RAM:{2:3$}{:10.1} / {:10.1}",
                BytesFmt(mem.used),
                BytesFmt(mem.free + mem.used),
                "",
                width.saturating_sub(27),
            )
            .unwrap();
            Executor::sleep(Duration::from_secs(1)).await;
        }
    });

    let logs_area = DrawArea::logs();
    if let Some(mut s) = SCREEN.try_lock() {
        if let Some(screen) = s.as_mut() {
            let width = screen.cols;
            for y in 0..logs_area.size.1 {
                let idx = (logs_area.offset.1 + y) * width;
                if idx < screen.back_buffer.len() {
                    screen.back_buffer[idx].c = '\u{25ba}';
                }
            }
        }
    }

    logger::on_ui_init();
}

pub fn flush() {
    if let Some(mut s) = SCREEN.try_lock() {
        if let Some(screen) = s.as_mut() {
            screen.flush();
        }
    }
}

pub fn red_screen() {
    if let Some(mut s) = SCREEN.try_lock() {
        if let Some(screen) = s.as_mut() {
            screen.clear_all(Color::Red);
        }
    }
}

#[derive(Clone, Copy)]
pub struct DrawArea {
    pub offset: (usize, usize),
    pub size: (usize, usize),
    pub pos: (usize, usize),
    pub scroll: bool,
}

impl DrawArea {
    pub const fn invalid() -> Self {
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
        (w().saturating_sub(STATUS_WIDTH + 1)) / (TASK_LEN + 1)
    }

    fn task_width() -> usize {
        Self::task_columns() * (TASK_LEN + 1) + 1
    }

    fn task_side(col: usize, num_cols: usize) -> DrawArea {
        let tw = Self::task_width();
        let width = w().saturating_sub(tw + 1);
        Self::new((tw + 1, col), (width, num_cols), false)
    }

    pub fn video_mode() -> DrawArea {
        Self::task_side(0, 1)
    }

    pub fn ip() -> DrawArea {
        Self::task_side(1, 1)
    }

    pub fn net_speed() -> DrawArea {
        Self::task_side(2, 2)
    }

    pub fn time() -> DrawArea {
        Self::task_side(4, 1)
    }

    pub fn memory() -> DrawArea {
        Self::task_side(5, 1)
    }

    pub fn tasks() -> DrawArea {
        Self::new((0, 0), (Self::task_width(), TASK_HEIGHT), false)
    }

    pub fn logs() -> DrawArea {
        Self::new(
            (2, h().saturating_sub(LOG_HEIGHT)),
            (w().saturating_sub(3), LOG_HEIGHT),
            true,
        )
    }

    pub fn content() -> DrawArea {
        let offset = 1 + TASK_HEIGHT;
        Self::new(
            (0, offset),
            (w(), h().saturating_sub(offset + LOG_HEIGHT)),
            false,
        )
    }

    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    fn idx(&self, p: (usize, usize)) -> usize {
        (self.offset.1 + p.1) * w() + self.offset.0 + p.0
    }

    pub fn clear(&mut self) {
        self.pos = (0, 0);
        let mut s = SCREEN.lock();
        if let Some(screen) = s.as_mut() {
            for y in 0..self.size.1 {
                for x in 0..self.size.0 {
                    let i = self.idx((x, y));
                    if i < screen.back_buffer.len() {
                        screen.back_buffer[i] = ScreenChar::default();
                    }
                }
            }
        }
    }

    pub fn write_with_color(&mut self, msg: &str, fg: Color, bg: Color) {
        let mut s = SCREEN.lock();
        if let Some(screen) = s.as_mut() {
            for c in msg.chars() {
                if c == '\n' {
                    self.newline();
                    continue;
                }
                if self.pos.0 >= self.size.0 {
                    self.newline();
                }
                while self.scroll && self.pos.1 >= self.size.1 {
                    self.pos.1 = self.pos.1.saturating_sub(1);
                    for y in 0..self.size.1.saturating_sub(1) {
                        for x in 0..self.size.0 {
                            let cur = self.idx((x, y));
                            let next = self.idx((x, y + 1));
                            if cur < screen.back_buffer.len() && next < screen.back_buffer.len() {
                                screen.back_buffer[cur] = screen.back_buffer[next];
                            }
                        }
                    }
                    for x in 0..self.size.0 {
                        let last = self.idx((x, self.size.1.saturating_sub(1)));
                        if last < screen.back_buffer.len() {
                            screen.back_buffer[last] = ScreenChar::default();
                        }
                    }
                }
                if self.pos.1 >= self.size.1 {
                    continue;
                }
                let idx = self.idx(self.pos);
                if idx < screen.back_buffer.len() {
                    screen.back_buffer[idx] = ScreenChar { c, fg, bg };
                }
                self.pos.0 += 1;
            }
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

pub fn update_content<F: Fn(&mut DrawArea)>(f: F) {
    if let Some(mut ca) = CONTENT_DRAW_AREA.try_lock() {
        f(&mut ca);
    }
}
