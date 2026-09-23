// Basic 8x16 bitmap font covering ASCII 0x20..0x7E and common box-drawing characters
// Each character is 16 bytes, 1 byte per row, MSB is leftmost pixel.

pub const FONT_WIDTH: usize = 8;
pub const FONT_HEIGHT: usize = 16;

include!("font_data.rs");

pub fn get_glyph(c: char) -> &'static [u8; 16] {
    let cp = c as usize;
    if cp < 256 {
        &FONT_DATA[cp]
    } else {
        // Fallback for box drawing characters:
        match c {
            '\u{2500}' => &GLYPH_HLINE,
            '\u{2502}' => &GLYPH_VLINE,
            '\u{250C}' => &GLYPH_CORNER_TL,
            '\u{2510}' => &GLYPH_CORNER_TR,
            '\u{2514}' => &GLYPH_CORNER_BL,
            '\u{2518}' => &GLYPH_CORNER_BR,
            '\u{252C}' => &GLYPH_TEE_TOP,
            '\u{2534}' => &GLYPH_TEE_BOTTOM,
            '\u{25ba}' => &GLYPH_ARROW_RIGHT,
            _ => &FONT_DATA[b'?' as usize],
        }
    }
}
