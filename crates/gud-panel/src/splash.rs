//! Boot logo, rendered a row at a time without a framebuffer or font dependency.

use crate::{HEIGHT, WIDTH};

const BACKGROUND: u16 = 0x10C4;
const MINT: u16 = 0x5F16;
const WHITE: u16 = 0xEF7D;

// Lowercase wordmark on a 5x9 grid, including the descender on the g.
const WORDMARK: [[u8; 9]; 6] = [
    [
        0, 0, 0b01111, 0b10001, 0b10001, 0b01111, 0b00001, 0b10001, 0b01110,
    ],
    [0, 0, 0b10001, 0b10001, 0b10001, 0b10001, 0b01111, 0, 0],
    [
        0b00001, 0b00001, 0b01111, 0b10001, 0b10001, 0b10001, 0b01111, 0, 0,
    ],
    [
        0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110, 0, 0,
    ],
    [0, 0, 0b01110, 0b10001, 0b11111, 0b10000, 0b01111, 0, 0],
    [
        0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00101, 0b00010, 0, 0,
    ],
];

fn rounded_rect(x: i32, y: i32, width: i32, height: i32, radius: i32) -> bool {
    if x < 0 || y < 0 || x >= width || y >= height {
        return false;
    }
    let dx = (radius - x).max(x - (width - 1 - radius)).max(0);
    let dy = (radius - y).max(y - (height - 1 - radius)).max(0);
    dx * dx + dy * dy <= radius * radius
}

fn pixel(x: i32, y: i32) -> u16 {
    let cx = WIDTH as i32 / 2;
    let top = HEIGHT as i32 / 2 - 54;
    let sx = x - (cx - 30);
    let sy = y - top;

    // A small monitor with rounded corners and a short pedestal.
    let bezel = rounded_rect(sx, sy, 60, 44, 7) && !rounded_rect(sx - 3, sy - 3, 54, 38, 4);
    let stand = (27..33).contains(&sx) && (44..51).contains(&sy)
        || rounded_rect(sx - 18, sy - 50, 24, 3, 1);
    if bezel || stand {
        return MINT;
    }

    // Two eyes and a quiet smile keep the little display recognizable at 1x.
    if rounded_rect(sx - 18, sy - 15, 4, 6, 1)
        || rounded_rect(sx - 38, sy - 15, 4, 6, 1)
        || (25..35).contains(&sx) && (28..31).contains(&sy)
        || (22..25).contains(&sx) && (25..28).contains(&sy)
        || (35..38).contains(&sx) && (25..28).contains(&sy)
    {
        return WHITE;
    }

    const SCALE: i32 = 4;
    const WORD_WIDTH: i32 = (6 * 6 - 1) * SCALE;
    let tx = x - (cx - WORD_WIDTH / 2);
    let ty = y - (top + 72);
    if (0..WORD_WIDTH).contains(&tx) && (0..9 * SCALE).contains(&ty) {
        let col = tx / SCALE;
        let gx = col % 6;
        if gx < 5 && WORDMARK[(col / 6) as usize][(ty / SCALE) as usize] & (0b10000 >> gx) != 0 {
            return WHITE;
        }
    }
    BACKGROUND
}

/// Render full-width rows starting at `y` into big-endian RGB565 pixels.
/// `out` must contain a whole number of rows within the visible panel.
pub fn render_rows(y: u16, out: &mut [u8]) {
    let row_bytes = WIDTH as usize * 2;
    assert_eq!(out.len() % row_bytes, 0);
    assert!(y as usize + out.len() / row_bytes <= HEIGHT as usize);
    for (i, px) in out.chunks_exact_mut(2).enumerate() {
        let x = i % WIDTH as usize;
        let row = y as usize + i / WIDTH as usize;
        px.copy_from_slice(&pixel(x as i32, row as i32).to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_in_bands_matches_a_full_frame() {
        let mut full = [0; crate::FRAME_BYTES];
        render_rows(0, &mut full);
        let mut row = [0; WIDTH as usize * 2];
        for y in 0..HEIGHT {
            render_rows(y, &mut row);
            let offset = y as usize * row.len();
            assert_eq!(&row[..], &full[offset..offset + row.len()]);
        }
        assert_eq!(&full[..2], &BACKGROUND.to_be_bytes());
        assert!(full.chunks_exact(2).any(|px| px == MINT.to_be_bytes()));
        assert!(full.chunks_exact(2).any(|px| px == WHITE.to_be_bytes()));
    }
}
