use crate::font_data::{
    FONT_DATA, GLYPH_ARROW_DOWN, GLYPH_ARROW_RIGHT, GLYPH_ARROW_UP, GLYPH_CORNER_BL,
    GLYPH_CORNER_BR, GLYPH_CORNER_TL, GLYPH_CORNER_TR, GLYPH_HLINE, GLYPH_TEE_BOTTOM,
    GLYPH_TEE_TOP, GLYPH_VLINE,
};

pub const FONT_WIDTH: usize = 8;
pub const FONT_HEIGHT: usize = 16;

pub fn get_glyph(c: char) -> &'static [u8; 16] {
    let cp = c as usize;
    if cp < 128 {
        &FONT_DATA[cp]
    } else {
        match c {
            '\u{2500}' => &GLYPH_HLINE,
            '\u{2502}' => &GLYPH_VLINE,
            '\u{250C}' => &GLYPH_CORNER_TL,
            '\u{2510}' => &GLYPH_CORNER_TR,
            '\u{2514}' => &GLYPH_CORNER_BL,
            '\u{2518}' => &GLYPH_CORNER_BR,
            '\u{252C}' => &GLYPH_TEE_TOP,
            '\u{2534}' => &GLYPH_TEE_BOTTOM,
            '\u{25BA}' | '\u{25B6}' => &GLYPH_ARROW_RIGHT,
            '\u{2191}' => &GLYPH_ARROW_UP,
            '\u{2193}' => &GLYPH_ARROW_DOWN,
            _ => &FONT_DATA[b'?' as usize],
        }
    }
}
