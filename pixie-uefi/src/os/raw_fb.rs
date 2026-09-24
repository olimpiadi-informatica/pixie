#![allow(dead_code)]

use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, AtomicU64, Ordering};

use super::boot_info::BootInfo;
use super::font::{FONT_HEIGHT, FONT_WIDTH, get_glyph};

static FB_BASE: AtomicU64 = AtomicU64::new(0);
static FB_STRIDE: AtomicUsize = AtomicUsize::new(0);
static FB_WIDTH: AtomicUsize = AtomicUsize::new(0);
static FB_HEIGHT: AtomicUsize = AtomicUsize::new(0);
static CURRENT_ROW: AtomicUsize = AtomicUsize::new(0);

pub const COLOR_BLACK: u32 = 0x0000_0000;
pub const COLOR_DARK_BLUE: u32 = 0x0000_0033;
pub const COLOR_WHITE: u32 = 0x00FF_FFFF;
pub const COLOR_YELLOW: u32 = 0x00FF_FF55;
pub const COLOR_GREEN: u32 = 0x0055_FF55;
pub const COLOR_CYAN: u32 = 0x0055_FFFF;
pub const COLOR_RED: u32 = 0x00FF_3333;
pub const COLOR_BG_RED: u32 = 0x0088_0000;

pub struct StackWriter<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> StackWriter<N> {
    pub const fn new() -> Self {
        Self { buf: [0; N], len: 0 }
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl<const N: usize> Write for StackWriter<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let avail = N.saturating_sub(self.len);
        let copy_len = bytes.len().min(avail);
        self.buf[self.len..self.len + copy_len].copy_from_slice(&bytes[..copy_len]);
        self.len += copy_len;
        Ok(())
    }
}

pub fn init(info: &BootInfo) {
    if info.framebuffer_base != 0 && info.framebuffer_base != 0xB8000 {
        FB_BASE.store(info.framebuffer_base, Ordering::SeqCst);
        FB_STRIDE.store(info.fb_stride as usize, Ordering::SeqCst);
        FB_WIDTH.store(info.fb_width as usize, Ordering::SeqCst);
        FB_HEIGHT.store(info.fb_height as usize, Ordering::SeqCst);
        CURRENT_ROW.store(0, Ordering::SeqCst);
    }
}

pub fn is_ready() -> bool {
    FB_BASE.load(Ordering::Relaxed) != 0
}

pub fn draw_char(col: usize, row: usize, c: char, fg: u32, bg: u32) {
    let base = FB_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let stride = FB_STRIDE.load(Ordering::Relaxed);
    let width = FB_WIDTH.load(Ordering::Relaxed);
    let height = FB_HEIGHT.load(Ordering::Relaxed);

    let fb = base as *mut u32;
    let glyph = get_glyph(c);
    let px = col * FONT_WIDTH;
    let py = row * FONT_HEIGHT;

    for (y, &row_byte) in glyph.iter().enumerate().take(FONT_HEIGHT) {
        let fb_y = py + y;
        if fb_y >= height {
            break;
        }
        let row_offset = fb_y * stride;
        for x in 0..FONT_WIDTH {
            let fb_x = px + x;
            if fb_x >= width {
                break;
            }
            let is_fg = (row_byte & (0x80 >> x)) != 0;
            let color = if is_fg { fg } else { bg };
            unsafe {
                core::ptr::write_volatile(fb.add(row_offset + fb_x), color);
            }
        }
    }
}

pub fn print_at(mut col: usize, row: usize, s: &str, fg: u32, bg: u32) {
    let width = FB_WIDTH.load(Ordering::Relaxed);
    let max_cols = if width > 0 { width / FONT_WIDTH } else { 80 };
    for c in s.chars() {
        if c == '\n' {
            break;
        }
        if col >= max_cols {
            break;
        }
        draw_char(col, row, c, fg, bg);
        col += 1;
    }
}

pub fn print(s: &str, fg: u32, bg: u32) {
    let row = CURRENT_ROW.fetch_add(1, Ordering::SeqCst);
    let height = FB_HEIGHT.load(Ordering::Relaxed);
    let max_rows = if height > 0 { height / FONT_HEIGHT } else { 25 };
    if row < max_rows {
        // Clear row background
        let width = FB_WIDTH.load(Ordering::Relaxed);
        let max_cols = if width > 0 { width / FONT_WIDTH } else { 80 };
        for col in 0..max_cols {
            draw_char(col, row, ' ', fg, bg);
        }
        print_at(0, row, s, fg, bg);
    }
}

pub fn clear_screen(bg: u32) {
    let base = FB_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let stride = FB_STRIDE.load(Ordering::Relaxed);
    let width = FB_WIDTH.load(Ordering::Relaxed);
    let height = FB_HEIGHT.load(Ordering::Relaxed);
    let fb = base as *mut u32;

    for y in 0..height {
        let offset = y * stride;
        for x in 0..width {
            unsafe {
                core::ptr::write_volatile(fb.add(offset + x), bg);
            }
        }
    }
    CURRENT_ROW.store(0, Ordering::SeqCst);
}

pub fn draw_fault_banner(reason: &str) {
    let base = FB_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let width = FB_WIDTH.load(Ordering::Relaxed);
    let height = FB_HEIGHT.load(Ordering::Relaxed);
    let max_cols = if width > 0 { width / FONT_WIDTH } else { 80 };

    // Fill top 12 rows with red background
    for row in 0..12.min(height / FONT_HEIGHT) {
        for col in 0..max_cols {
            draw_char(col, row, ' ', COLOR_WHITE, COLOR_BG_RED);
        }
    }

    let banner = "======================= SYSTEM FAULT DETECTED =======================";
    let b_col = max_cols.saturating_sub(banner.len()) / 2;
    print_at(b_col, 1, banner, COLOR_YELLOW, COLOR_BG_RED);

    print_at(2, 3, "REASON:", COLOR_YELLOW, COLOR_BG_RED);
    print_at(10, 3, reason, COLOR_WHITE, COLOR_BG_RED);

    print_at(2, 5, "System halted. Check diagnostic information above.", COLOR_WHITE, COLOR_BG_RED);
    print_at(b_col, 7, banner, COLOR_YELLOW, COLOR_BG_RED);
}
