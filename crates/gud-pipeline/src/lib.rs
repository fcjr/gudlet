//! Shared bounded USB-to-panel pipeline. Buffers move between cores by ownership;
//! USB and DMA drivers remain board-specific.
#![no_std]

pub use embassy_sync::channel::TrySendError;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use gud_protocol::{Command, Rect};

pub enum PanelJob<const N: usize> {
    Band {
        payload: &'static mut [u8; N],
        rect: Rect,
    },
    Enable(bool),
}

pub type JobChannel<const N: usize> = Channel<CriticalSectionRawMutex, PanelJob<N>, 4>;
pub type FreeChannel<const N: usize, const COUNT: usize> =
    Channel<CriticalSectionRawMutex, &'static mut [u8; N], COUNT>;
pub type CommandChannel = Channel<CriticalSectionRawMutex, Command, 8>;

/// Tracks the exact payload boundary, including short and odd-sized USB packets.
/// An overrun leaves the cursor unchanged so callers can reset the unframed stream.
pub struct PayloadProgress {
    length: usize,
    filled: usize,
}

impl PayloadProgress {
    pub const fn new(length: usize) -> Self {
        Self { length, filled: 0 }
    }
    pub fn filled(&self) -> usize {
        self.filled
    }
    pub fn remaining(&self) -> usize {
        self.length - self.filled
    }
    pub fn complete(&self) -> bool {
        self.remaining() == 0
    }
    pub fn advance(&mut self, n: usize) -> Result<(), PayloadError> {
        if n > self.remaining() {
            return Err(PayloadError);
        }
        self.filled += n;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct PayloadError;

/// Validate/decode one complete band. Both outputs are still little-endian RGB565.
/// The caller swaps bytes only after decoding succeeds, before handing pixels to DMA.
pub fn decode_pixels<'a>(
    rect: &Rect,
    payload: &'a mut [u8],
    decoded: &'a mut [u8],
) -> Result<&'a mut [u8], PayloadError> {
    let length = rect.length as usize;
    let wire = rect.payload as usize;
    if length == 0 || length % 2 != 0 || wire == 0 || wire > length || wire > payload.len() {
        return Err(PayloadError);
    }
    if rect.compressed {
        let out = decoded.get_mut(..length).ok_or(PayloadError)?;
        match lz4_flex::block::decompress_into(&payload[..wire], out) {
            Ok(n) if n == length => Ok(out),
            _ => Err(PayloadError),
        }
    } else if wire == length {
        payload.get_mut(..length).ok_or(PayloadError)
    } else {
        Err(PayloadError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rect(length: u32, payload: u32, compressed: bool) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: (length / 2) as u16,
            height: 1,
            length,
            payload,
            compressed,
        }
    }
    #[test]
    fn packet_boundaries_and_overrun() {
        let mut p = PayloadProgress::new(130);
        for n in [1, 63, 0, 64] {
            p.advance(n).unwrap();
        }
        assert_eq!(p.remaining(), 2);
        assert_eq!(p.advance(3), Err(PayloadError));
        assert_eq!(p.filled(), 128);
        p.advance(2).unwrap();
        assert!(p.complete());
    }
    #[test]
    fn raw_does_not_touch_decode_buffer() {
        let mut raw = [1, 2, 3, 4];
        let mut scratch = [9; 4];
        assert_eq!(
            decode_pixels(&rect(4, 4, false), &mut raw, &mut scratch).unwrap(),
            [1, 2, 3, 4]
        );
        assert_eq!(scratch, [9; 4]);
        assert!(decode_pixels(&rect(4, 3, false), &mut raw, &mut scratch).is_err());
    }
    #[test]
    fn lz4_exact_length_and_malformed_input() {
        // Four literals, an overlapping 12-byte match, then six final literals.
        let mut block = [0x48, 1, 2, 3, 4, 4, 0, 0x60, 5, 6, 7, 8, 9, 10];
        let mut scratch = [0xa5; 24];
        let result = decode_pixels(&rect(22, 14, true), &mut block, &mut scratch).unwrap();
        assert_eq!(
            result,
            [1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        );
        assert_eq!(&scratch[22..], &[0xa5; 2]);
        assert!(decode_pixels(&rect(24, 14, true), &mut block, &mut scratch).is_err());
        assert!(decode_pixels(&rect(20, 14, true), &mut block, &mut scratch).is_err());
        block[5] = 0; // invalid zero offset
        assert!(decode_pixels(&rect(22, 14, true), &mut block, &mut scratch).is_err());
    }
}
