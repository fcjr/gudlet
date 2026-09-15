//! The 1.69" 240x280 ST7789V2 panel on Waveshare's Touch-LCD-1.69 boards:
//! geometry, a minimal command driver, and the on-glass diagnostics the
//! firmware draws with it.
//!
//! The driver only owns the control pins and issues commands over a borrowed
//! SPI bus, so the firmware can push pixel data on the same bus however it
//! likes (blocking writes, DMA). The controller's RAM is 240x320; the
//! visible 280 rows sit 20 rows in.

#![no_std]

pub mod splash;
pub mod strip;

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::SpiBus;

#[cfg(not(feature = "landscape"))]
mod geometry {
    pub const WIDTH: u16 = 240;
    pub const HEIGHT: u16 = 280;
    pub const X_OFFSET: u16 = 0;
    pub const Y_OFFSET: u16 = 20;
    pub const MADCTL: u8 = 0x00;
}

#[cfg(feature = "landscape")]
mod geometry {
    pub const WIDTH: u16 = 280;
    pub const HEIGHT: u16 = 240;
    pub const X_OFFSET: u16 = 20;
    pub const Y_OFFSET: u16 = 0;
    // MX | MV | ML: rotate 90 degrees, keep RGB order.
    pub const MADCTL: u8 = 0x70;
}

pub use geometry::{HEIGHT, WIDTH};
use geometry::{MADCTL, X_OFFSET, Y_OFFSET};

pub const BYTES_PER_PIXEL: usize = 2;
/// One full frame of RGB565.
pub const FRAME_BYTES: usize = WIDTH as usize * HEIGHT as usize * BYTES_PER_PIXEL;

const CMD_SLPOUT: u8 = 0x11;
const CMD_INVON: u8 = 0x21;
const CMD_DISPOFF: u8 = 0x28;
const CMD_DISPON: u8 = 0x29;
const CMD_CASET: u8 = 0x2A;
const CMD_RASET: u8 = 0x2B;
const CMD_RAMWR: u8 = 0x2C;
const CMD_MADCTL: u8 = 0x36;
const CMD_COLMOD: u8 = 0x3A;

pub struct St7789<DC, CS, RST> {
    dc: DC,
    cs: CS,
    rst: RST,
}

impl<DC, CS, RST> St7789<DC, CS, RST>
where
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
{
    pub fn new(dc: DC, cs: CS, rst: RST) -> Self {
        Self { dc, cs, rst }
    }

    /// Hardware reset followed by the panel vendor's register sequence.
    pub fn init(&mut self, spi: &mut impl SpiBus, delay: &mut impl DelayNs) {
        let _ = self.cs.set_high();
        let _ = self.rst.set_high();
        delay.delay_ms(100);
        let _ = self.rst.set_low();
        delay.delay_ms(100);
        let _ = self.rst.set_high();
        delay.delay_ms(100);

        self.command(spi, CMD_MADCTL, &[MADCTL]);
        self.command(spi, CMD_COLMOD, &[0x05]); // 16 bpp
        self.command(spi, 0xB2, &[0x0B, 0x0B, 0x00, 0x33, 0x35]); // porch control
        self.command(spi, 0xB7, &[0x11]); // gate control
        self.command(spi, 0xBB, &[0x35]); // VCOM
        self.command(spi, 0xC0, &[0x2C]); // LCM control
        self.command(spi, 0xC2, &[0x01]); // VDV/VRH enable
        self.command(spi, 0xC3, &[0x0D]); // VRH
        self.command(spi, 0xC4, &[0x20]); // VDV
        self.command(spi, 0xC6, &[0x13]); // frame rate
        self.command(spi, 0xD0, &[0xA4, 0xA1]); // power control
        self.command(spi, 0xD6, &[0xA1]);
        self.command(
            spi,
            0xE0,
            &[0xF0, 0x06, 0x0B, 0x0A, 0x09, 0x26, 0x29, 0x33, 0x41, 0x18, 0x16, 0x15, 0x29, 0x2D],
        );
        self.command(
            spi,
            0xE1,
            &[0xF0, 0x04, 0x08, 0x08, 0x07, 0x03, 0x28, 0x32, 0x40, 0x3B, 0x19, 0x18, 0x2A, 0x2E],
        );
        self.command(spi, 0xE4, &[0x25, 0x00, 0x00]); // gate scan (320 lines)
        self.command(spi, CMD_INVON, &[]);
        self.command(spi, CMD_SLPOUT, &[]);
        delay.delay_ms(120);
        self.command(spi, CMD_DISPON, &[]);
    }

    fn command(&mut self, spi: &mut impl SpiBus, cmd: u8, data: &[u8]) {
        let _ = self.cs.set_low();
        let _ = self.dc.set_low();
        spi.write(&[cmd]).ok().expect("panel command write failed");
        let _ = spi.flush();
        if !data.is_empty() {
            let _ = self.dc.set_high();
            spi.write(data).ok().expect("panel data write failed");
            let _ = spi.flush();
        }
        let _ = self.cs.set_high();
    }

    /// Select a rectangle and open a RAM write. Big-endian RGB565 pixel
    /// data written on the bus afterwards fills it row by row until
    /// `end_rect`.
    pub fn begin_rect(&mut self, spi: &mut impl SpiBus, x: u16, y: u16, width: u16, height: u16) {
        let x0 = x + X_OFFSET;
        let x1 = x0 + width - 1;
        let y0 = y + Y_OFFSET;
        let y1 = y0 + height - 1;
        self.command(spi, CMD_CASET, &[(x0 >> 8) as u8, x0 as u8, (x1 >> 8) as u8, x1 as u8]);
        self.command(spi, CMD_RASET, &[(y0 >> 8) as u8, y0 as u8, (y1 >> 8) as u8, y1 as u8]);

        let _ = self.cs.set_low();
        let _ = self.dc.set_low();
        let _ = spi.write(&[CMD_RAMWR]);
        let _ = spi.flush();
        let _ = self.dc.set_high();
    }

    /// Raise CS. The caller makes sure the last pixel has clocked out first.
    pub fn end_rect(&mut self) {
        let _ = self.cs.set_high();
    }

    pub fn fill(&mut self, spi: &mut impl SpiBus, color: u16) {
        let [hi, lo] = color.to_be_bytes();
        let mut chunk = [0u8; 64];
        for pair in chunk.chunks_exact_mut(2) {
            pair[0] = hi;
            pair[1] = lo;
        }
        self.begin_rect(spi, 0, 0, WIDTH, HEIGHT);
        let mut sent = 0;
        while sent < FRAME_BYTES {
            let n = (FRAME_BYTES - sent).min(chunk.len());
            let _ = spi.write(&chunk[..n]);
            sent += n;
        }
        let _ = spi.flush();
        self.end_rect();
    }

    pub fn set_display_on(&mut self, spi: &mut impl SpiBus, on: bool) {
        self.command(spi, if on { CMD_DISPON } else { CMD_DISPOFF }, &[]);
    }

    /// Draw the shared boot logo using only one row of scratch RAM.
    pub fn show_boot_logo(&mut self, spi: &mut impl SpiBus) {
        let mut row = [0; WIDTH as usize * 2];
        self.begin_rect(spi, 0, 0, WIDTH, HEIGHT);
        for y in 0..HEIGHT {
            splash::render_rows(y, &mut row);
            spi.write(&row).ok().expect("boot logo write failed");
        }
        let _ = spi.flush();
        self.end_rect();
    }
}

/// Fill `out` with full-width rows of eight vertical color bars, big-endian
/// RGB565, for testing panel initialization and the pixel path.
pub fn color_bars(out: &mut [u8]) {
    const COLORS: [u16; 8] = [0xF800, 0x07E0, 0x001F, 0xFFE0, 0xF81F, 0x07FF, 0xFFFF, 0x0000];
    let width = WIDTH as usize;
    for (i, px) in out.chunks_exact_mut(2).enumerate() {
        let x = i % width;
        px.copy_from_slice(&COLORS[x * 8 / width].to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_bars_are_vertical() {
        let mut out = [0u8; WIDTH as usize * 2 * 2];
        color_bars(&mut out);
        let row = WIDTH as usize * 2;
        assert_eq!(&out[..row], &out[row..]);
        assert_eq!(&out[..2], &[0xF8, 0x00]);
        assert_eq!(&out[row - 2..row], &[0x00, 0x00]);
    }
}
