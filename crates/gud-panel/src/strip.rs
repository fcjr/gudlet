//! On-panel text strip for bring-up: a 3x5 font scaled 2x, drawn by
//! the panel worker across the top rows once a second. It depends on nothing
//! but the panel, so it keeps working when USB or the console does not.

pub const ROWS: usize = 3;
pub const CHAR_W: usize = 8; // 3 columns * 2 + 2 spacing
pub const CHAR_H: usize = 12; // 5 rows * 2 + 2 spacing
pub const STRIP_H: usize = ROWS * CHAR_H;
pub const COLS: usize = crate::WIDTH as usize / CHAR_W;

/// 3x5 glyphs, one byte per row, low three bits used (MSB = left).
/// Lowercase maps to uppercase.
fn glyph(c: u8) -> [u8; 5] {
    match c.to_ascii_uppercase() {
        b'0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        b'1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        b'2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        b'3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        b'4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        b'5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        b'6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        b'7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        b'8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        b'9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        b'A' => [0b111, 0b101, 0b111, 0b101, 0b101],
        b'B' => [0b110, 0b101, 0b110, 0b101, 0b110],
        b'C' => [0b111, 0b100, 0b100, 0b100, 0b111],
        b'D' => [0b110, 0b101, 0b101, 0b101, 0b110],
        b'E' => [0b111, 0b100, 0b111, 0b100, 0b111],
        b'F' => [0b111, 0b100, 0b111, 0b100, 0b100],
        b'G' => [0b111, 0b100, 0b101, 0b101, 0b111],
        b'H' => [0b101, 0b101, 0b111, 0b101, 0b101],
        b'I' => [0b111, 0b010, 0b010, 0b010, 0b111],
        b'J' => [0b001, 0b001, 0b001, 0b101, 0b111],
        b'K' => [0b101, 0b101, 0b110, 0b101, 0b101],
        b'L' => [0b100, 0b100, 0b100, 0b100, 0b111],
        b'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
        b'N' => [0b110, 0b101, 0b101, 0b101, 0b101],
        b'O' => [0b111, 0b101, 0b101, 0b101, 0b111],
        b'P' => [0b111, 0b101, 0b111, 0b100, 0b100],
        b'Q' => [0b111, 0b101, 0b101, 0b111, 0b001],
        b'R' => [0b110, 0b101, 0b110, 0b101, 0b101],
        b'S' => [0b111, 0b100, 0b111, 0b001, 0b111],
        b'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        b'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        b'V' => [0b101, 0b101, 0b101, 0b101, 0b010],
        b'W' => [0b101, 0b101, 0b111, 0b111, 0b101],
        b'X' => [0b101, 0b101, 0b010, 0b101, 0b101],
        b'Y' => [0b101, 0b101, 0b010, 0b010, 0b010],
        b'Z' => [0b111, 0b001, 0b010, 0b100, 0b111],
        b':' => [0b000, 0b010, 0b000, 0b010, 0b000],
        b'.' => [0b000, 0b000, 0b000, 0b000, 0b010],
        b'/' => [0b001, 0b001, 0b010, 0b100, 0b100],
        b'-' => [0b000, 0b000, 0b111, 0b000, 0b000],
        b'_' => [0b000, 0b000, 0b000, 0b000, 0b111],
        b'(' => [0b010, 0b100, 0b100, 0b100, 0b010],
        b')' => [0b010, 0b001, 0b001, 0b001, 0b010],
        b'`' | b'\'' => [0b010, 0b001, 0b000, 0b000, 0b000],
        _ => [0; 5],
    }
}

/// Word-wrap plain text into ROWS lines of COLS chars, breaking at any
/// character. Returns the number of bytes written to `out` ('\n' separated).
pub fn wrap(text: &[u8], out: &mut [u8]) -> usize {
    let mut pos = 0;
    let mut col = 0;
    let mut row = 0;
    for &c in text {
        if row >= ROWS || pos + 2 > out.len() {
            break;
        }
        let c = if c == b'\n' { b' ' } else { c };
        if col == COLS {
            out[pos] = b'\n';
            pos += 1;
            col = 0;
            row += 1;
            if row >= ROWS {
                break;
            }
        }
        out[pos] = c;
        pos += 1;
        col += 1;
    }
    pos
}

/// Render `text` (ASCII, ROWS lines of COLS chars, '\n' separated) into a
/// big-endian RGB565 buffer of WIDTH x STRIP_H pixels: white on dark blue.
pub fn render(text: &[u8], out: &mut [u8]) {
    let width = crate::WIDTH as usize;
    let [bg_hi, bg_lo] = 0x0008u16.to_be_bytes();
    for px in out.chunks_exact_mut(2) {
        px[0] = bg_hi;
        px[1] = bg_lo;
    }
    let mut row = 0;
    let mut col = 0;
    for &c in text {
        if c == b'\n' {
            row += 1;
            col = 0;
            continue;
        }
        if row >= ROWS || col >= COLS {
            continue;
        }
        let g = glyph(c);
        for (gy, bits) in g.iter().enumerate() {
            for gx in 0..3 {
                if bits & (0b100 >> gx) == 0 {
                    continue;
                }
                for sy in 0..2 {
                    for sx in 0..2 {
                        let x = col * CHAR_W + gx * 2 + sx;
                        let y = row * CHAR_H + gy * 2 + sy;
                        let i = (y * width + x) * 2;
                        out[i] = 0xFF;
                        out[i + 1] = 0xFF;
                    }
                }
            }
        }
        col += 1;
    }
}

/// Write the low 24 bits of a u32 as 6 hex digits into `dst[..6]`.
pub fn hex(mut v: u32, dst: &mut [u8]) {
    for i in (0..6).rev() {
        dst[i] = b"0123456789ABCDEF"[(v & 0xf) as usize];
        v >>= 4;
    }
}
