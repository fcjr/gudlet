//! Render the actual firmware logo as a PPM image for visual inspection.
//! cargo run -p gud-panel --example boot-logo > /tmp/gudlet-boot.ppm

use std::io::{self, Write};

fn main() -> io::Result<()> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    writeln!(out, "P6\n{} {}\n255", gud_panel::WIDTH, gud_panel::HEIGHT)?;
    let mut row = [0; gud_panel::WIDTH as usize * 2];
    for y in 0..gud_panel::HEIGHT {
        gud_panel::splash::render_rows(y, &mut row);
        for px in row.chunks_exact(2) {
            let rgb = u16::from_be_bytes([px[0], px[1]]);
            let r = ((rgb >> 11) & 31) as u8;
            let g = ((rgb >> 5) & 63) as u8;
            let b = (rgb & 31) as u8;
            out.write_all(&[
                (r << 3) | (r >> 2),
                (g << 2) | (g >> 4),
                (b << 3) | (b >> 2),
            ])?;
        }
    }
    out.flush()
}
