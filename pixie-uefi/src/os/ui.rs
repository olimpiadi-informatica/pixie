#![allow(clippy::collapsible_if)]

use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;

use pixie_shared::util::BytesFmt;
use spin::Mutex;
use uefi::proto::console::text::Color;

use super::boot_info::BootInfo;
use super::executor::{Executor, TASK_LEN};
use super::font::{FONT_HEIGHT, FONT_WIDTH, get_glyph};
use super::memory;
use super::timer::Timer;

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
    cols: usize,
    rows: usize,
    is_vga_text: bool,
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

fn color_to_vga(c: Color) -> u8 {
    match c {
        Color::Black => 0,
        Color::Blue => 1,
        Color::Green => 2,
        Color::Cyan => 3,
        Color::Red => 4,
        Color::Magenta => 5,
        Color::Brown => 6,
        Color::LightGray => 7,
        Color::DarkGray => 8,
        Color::LightBlue => 9,
        Color::LightGreen => 10,
        Color::LightCyan => 11,
        Color::LightRed => 12,
        Color::LightMagenta => 13,
        Color::Yellow => 14,
        Color::White => 15,
    }
}

fn rgb_to_vga(rgb: u32) -> u8 {
    match rgb {
        0x00000000 => 0,
        0x000000AA => 1,
        0x0000AA00 => 2,
        0x0000AAAA => 3,
        0x00AA0000 => 4,
        0x00AA00AA => 5,
        0x00AA5500 => 6,
        0x00AAAAAA => 7,
        0x00555555 => 8,
        0x005555FF => 9,
        0x0055FF55 => 10,
        0x0055FFFF => 11,
        0x00FF5555 => 12,
        0x00FF55FF => 13,
        0x00FFFF55 => 14,
        0x00FFFFFF => 15,
        _ => 7,
    }
}

fn color_to_rgb(c: Color) -> u32 {
    match c {
        Color::Black => 0x00000000,
        Color::Blue => 0x000000AA,
        Color::Green => 0x0000AA00,
        Color::Cyan => 0x0000AAAA,
        Color::Red => 0x00AA0000,
        Color::Magenta => 0x00AA00AA,
        Color::Brown => 0x00AA5500,
        Color::LightGray => 0x00AAAAAA,
        Color::DarkGray => 0x00555555,
        Color::LightBlue => 0x005555FF,
        Color::LightGreen => 0x0055FF55,
        Color::LightCyan => 0x0055FFFF,
        Color::LightRed => 0x00FF5555,
        Color::LightMagenta => 0x00FF55FF,
        Color::Yellow => 0x00FFFF55,
        Color::White => 0x00FFFFFF,
    }
}

impl Screen {
    pub fn new(boot_info: BootInfo) -> Self {
        let is_vga_text = boot_info.framebuffer_base == 0xB8000;
        let (cols, rows) = if is_vga_text {
            (80, 25)
        } else {
            (
                (boot_info.fb_width as usize) / FONT_WIDTH,
                (boot_info.fb_height as usize) / FONT_HEIGHT,
            )
        };
        COLS.store(cols, Ordering::Relaxed);
        ROWS.store(rows, Ordering::Relaxed);

        let total_chars = cols * rows;

        Self {
            framebuffer_base: boot_info.framebuffer_base as *mut u32,
            fb_stride: boot_info.fb_stride as usize,
            fb_width: boot_info.fb_width as usize,
            fb_height: boot_info.fb_height as usize,
            cols,
            rows,
            is_vga_text,
            front_buffer: vec![ScreenChar::default(); total_chars],
            back_buffer: vec![ScreenChar::default(); total_chars],
        }
    }

    fn draw_char_raw(&mut self, col: usize, row: usize, c: char, fg: u32, bg: u32) {
        if self.framebuffer_base.is_null() || col >= self.cols || row >= self.rows {
            return;
        }

        if self.is_vga_text {
            let vga_ptr = 0xB8000 as *mut u16;
            let ascii = if (c as u32) < 128 { c as u8 } else { b'?' };
            let vga_fg = rgb_to_vga(fg);
            let vga_bg = rgb_to_vga(bg);
            let attr = (vga_bg << 4) | (vga_fg & 0x0F);
            let cell = (ascii as u16) | ((attr as u16) << 8);
            unsafe {
                core::ptr::write_volatile(vga_ptr.add(row * 80 + col), cell);
            }
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
        for i in 0..total {
            if self.front_buffer[i] != self.back_buffer[i] {
                let cell = self.back_buffer[i];
                let col = i % self.cols;
                let row = i / self.cols;
                self.draw_char_raw(
                    col,
                    row,
                    cell.c,
                    color_to_rgb(cell.fg),
                    color_to_rgb(cell.bg),
                );
                self.front_buffer[i] = cell;
            }
        }
    }

    pub fn clear_all(&mut self, bg: Color) {
        if self.is_vga_text {
            let vga_ptr = 0xB8000 as *mut u16;
            let vga_bg = color_to_vga(bg);
            let attr = (vga_bg << 4) | 7;
            let cell = (b' ' as u16) | ((attr as u16) << 8);
            for i in 0..(80 * 25) {
                unsafe {
                    core::ptr::write_volatile(vga_ptr.add(i), cell);
                }
            }
        } else if !self.framebuffer_base.is_null() {
            let bg_rgb = color_to_rgb(bg);
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
            if boot_info.framebuffer_base == 0xB8000 {
                write!(
                    draw_area,
                    "DISP:{0:1$}VGA Text (80x25)",
                    "",
                    width.saturating_sub(22)
                )
                .unwrap();
            } else {
                let label = alloc::format!("GOP {w}x{h}", w = boot_info.fb_width, h = boot_info.fb_height);
                write!(
                    draw_area,
                    "DISP:{0:1$}{label}",
                    "",
                    width.saturating_sub(5 + label.len())
                )
                .unwrap();
            }
            Executor::sleep(Duration::from_secs(10)).await;
        }
    });

    Executor::spawn("[show_timer]", async move {
        let mut draw_area = DrawArea::time();
        loop {
            draw_area.clear();
            let time = Timer::micros() as f32 * 0.000_001;
            let width = draw_area.size().0;
            write!(
                draw_area,
                "uptime:{0:1$}{time:12.1}s",
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

    crate::os::logger::on_ui_init();
}

pub fn flush() {
    if let Some(mut s) = SCREEN.try_lock() {
        if let Some(screen) = s.as_mut() {
            screen.flush();
        }
    }
}

#[allow(dead_code)]
pub fn red_screen() {
    if let Some(mut s) = SCREEN.try_lock() {
        if let Some(screen) = s.as_mut() {
            screen.clear_all(Color::Red);
        }
    }
}

pub fn display_fault_screen(reason: &str) {
    let mut guard = SCREEN.try_lock();
    if guard.is_none() {
        for _ in 0..10_000 {
            if let Some(g) = SCREEN.try_lock() {
                guard = Some(g);
                break;
            }
            core::hint::spin_loop();
        }
        if guard.is_none() {
            unsafe {
                SCREEN.force_unlock();
            }
            guard = SCREEN.try_lock();
        }
    }

    let mut displayed = false;
    if let Some(mut s) = guard {
        if let Some(screen) = s.as_mut() {
            screen.clear_all(Color::Red);
            let banner = "======================= SYSTEM FAULT DETECTED =======================";
            let x = screen.cols.saturating_sub(banner.len()) / 2;
            for (i, c) in banner.chars().enumerate() {
                screen.draw_char_raw(x + i, 4, c, 0xFFFFFFFF, 0x00AA0000);
            }

            let rx = screen.cols.saturating_sub(reason.len()) / 2;
            for (i, c) in reason.chars().enumerate() {
                screen.draw_char_raw(rx + i, 8, c, 0xFFFFFFFF, 0x00AA0000);
            }
            displayed = true;
        }
    }

    if !displayed {
        let vga_ptr = 0xB8000 as *mut u16;
        let attr = (4 << 4) | 15; // White on red
        for i in 0..(80 * 25) {
            unsafe {
                core::ptr::write_volatile(vga_ptr.add(i), (b' ' as u16) | ((attr as u16) << 8));
            }
        }
        let banner = "======================= SYSTEM FAULT DETECTED =======================";
        let x = 80usize.saturating_sub(banner.len()) / 2;
        for (i, c) in banner.chars().enumerate() {
            unsafe {
                core::ptr::write_volatile(
                    vga_ptr.add(4 * 80 + x + i),
                    (c as u8 as u16) | ((attr as u16) << 8),
                );
            }
        }
        let rx = 80usize.saturating_sub(reason.len()) / 2;
        for (i, c) in reason.chars().enumerate().take(80) {
            let ch = if (c as u32) < 128 { c as u8 } else { b'?' };
            unsafe {
                core::ptr::write_volatile(
                    vga_ptr.add(8 * 80 + rx + i),
                    (ch as u16) | ((attr as u16) << 8),
                );
            }
        }
    }
}

pub fn update_fault_countdown(seconds: usize) {
    if let Some(mut s) = SCREEN.try_lock() {
        if let Some(screen) = s.as_mut() {
            let mut buf = *b"Rebooting in XX seconds... (press any key to reboot immediately)";
            let tens = ((seconds / 10) % 10) as u8;
            let ones = (seconds % 10) as u8;
            buf[13] = if tens > 0 { b'0' + tens } else { b' ' };
            buf[14] = b'0' + ones;
            let s = core::str::from_utf8(&buf).unwrap_or("");
            let x = screen.cols.saturating_sub(s.len()) / 2;
            for (i, c) in s.chars().enumerate() {
                screen.draw_char_raw(x + i, 12, c, 0xFFFFFFFF, 0x00AA0000);
            }
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

    fn time() -> DrawArea {
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
