use spin::Mutex;

use crate::boot_info::{BootInfo, FbPixelFormat};
use crate::font::{FONT_HEIGHT as CHAR_HEIGHT, FONT_WIDTH as CHAR_WIDTH, get_glyph};

pub const COLOR_DARK_BLUE: u32 = 0x0000_0040;
pub const COLOR_WHITE: u32 = 0x00FF_FFFF;
pub const COLOR_YELLOW: u32 = 0x00FF_FF55;
pub const COLOR_GREEN: u32 = 0x0055_FF55;
pub const COLOR_CYAN: u32 = 0x0055_FFFF;
pub const COLOR_RED: u32 = 0x00FF_5555;

struct RawFbState {
    fb_base: *mut u32,
    width: usize,
    height: usize,
    stride: usize,
    is_bgr: bool,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
}

unsafe impl Send for RawFbState {}

static RAW_FB: Mutex<Option<RawFbState>> = Mutex::new(None);

impl RawFbState {
    fn pack_color(&self, rgb: u32) -> u32 {
        if self.is_bgr {
            rgb
        } else {
            let r = (rgb >> 16) & 0xFF;
            let g = (rgb >> 8) & 0xFF;
            let b = rgb & 0xFF;
            (b << 16) | (g << 8) | r
        }
    }

    fn clear(&mut self, bg_rgb: u32) {
        let pixel = self.pack_color(bg_rgb);
        for y in 0..self.height {
            for x in 0..self.width {
                unsafe {
                    self.fb_base.add(y * self.stride + x).write_volatile(pixel);
                }
            }
        }
        self.col = 0;
        self.row = 0;
    }

    fn draw_char(&mut self, c: char, fg_rgb: u32, bg_rgb: u32) {
        if c == '\n' {
            self.col = 0;
            self.row += 1;
            if self.row >= self.rows {
                self.row = 0;
            }
            return;
        }
        if c == '\r' {
            self.col = 0;
            return;
        }

        if self.col >= self.cols {
            self.col = 0;
            self.row += 1;
            if self.row >= self.rows {
                self.row = 0;
            }
        }

        let fg = self.pack_color(fg_rgb);
        let bg = self.pack_color(bg_rgb);
        let glyph = get_glyph(c);
        let base_x = self.col * CHAR_WIDTH;
        let base_y = self.row * CHAR_HEIGHT;

        for (dy, &row_bits) in glyph.iter().enumerate() {
            let py = base_y + dy;
            if py >= self.height {
                break;
            }
            for dx in 0..CHAR_WIDTH {
                let px = base_x + dx;
                if px >= self.width {
                    break;
                }
                let bit = (row_bits >> (7 - dx)) & 1;
                let color = if bit != 0 { fg } else { bg };
                unsafe {
                    self.fb_base
                        .add(py * self.stride + px)
                        .write_volatile(color);
                }
            }
        }

        self.col += 1;
    }

    fn write_str(&mut self, s: &str, fg_rgb: u32, bg_rgb: u32) {
        for c in s.chars() {
            self.draw_char(c, fg_rgb, bg_rgb);
        }
    }
}

pub fn init(boot_info: &BootInfo) {
    let width = boot_info.fb_width as usize;
    let height = boot_info.fb_height as usize;
    let stride = boot_info.fb_stride as usize;
    let cols = (width / CHAR_WIDTH).max(1);
    let rows = (height / CHAR_HEIGHT).max(1);
    let mut state = RawFbState {
        fb_base: boot_info.framebuffer_base as *mut u32,
        width,
        height,
        stride,
        is_bgr: matches!(boot_info.pixel_format, FbPixelFormat::Bgr),
        cols,
        rows,
        col: 0,
        row: 0,
    };
    if !state.fb_base.is_null() {
        state.clear(COLOR_DARK_BLUE);
        state.write_str(
            "Pixie OS Kernel: Exited UEFI Boot Services\n",
            COLOR_YELLOW,
            COLOR_DARK_BLUE,
        );
    }
    *RAW_FB.lock() = Some(state);
}

pub fn clear_screen(bg: u32) {
    if let Some(fb) = RAW_FB.lock().as_mut() {
        fb.clear(bg);
    }
}

pub fn try_clear_screen(bg: u32) {
    if let Some(mut guard) = RAW_FB.try_lock()
        && let Some(fb) = guard.as_mut()
    {
        fb.clear(bg);
    }
}

pub fn print(s: &str, fg: u32, bg: u32) {
    if let Some(fb) = RAW_FB.lock().as_mut() {
        fb.write_str(s, fg, bg);
    }
}

pub fn try_print(s: &str, fg: u32, bg: u32) {
    if let Some(mut guard) = RAW_FB.try_lock()
        && let Some(fb) = guard.as_mut()
    {
        fb.write_str(s, fg, bg);
    }
}
