//! Pixel byte order. Hosts send little-endian RGB565; ST7789-class panels
//! want the high byte first.

/// Swap every pair in place: a whole decoded band, in one go.
pub fn swap_pairs(pixels: &mut [u8]) {
    for pair in pixels.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
}

/// Streams one update packet by packet, for firmware with no room to buffer
/// it: each call to [`feed`](Self::feed) byte-swaps what arrived and keeps
/// a dangling low byte for the next packet, so pixels split across packet
/// boundaries come out whole.
pub struct PixelStream {
    /// Bytes of the current update still expected on the bulk endpoint.
    remaining: u32,
    /// Low byte of a pixel whose high byte is in the next packet.
    carry: Option<u8>,
}

impl Default for PixelStream {
    fn default() -> Self {
        Self::new()
    }
}

impl PixelStream {
    pub const fn new() -> Self {
        Self { remaining: 0, carry: None }
    }

    /// Start a new update of `length` raw bytes.
    pub fn begin(&mut self, length: u32) {
        self.remaining = length;
        self.carry = None;
    }

    pub fn is_active(&self) -> bool {
        self.remaining != 0
    }

    /// Drop whatever is left of the current update. Returns whether one was
    /// in progress.
    pub fn abort(&mut self) -> bool {
        let active = self.is_active();
        self.remaining = 0;
        self.carry = None;
        active
    }

    /// Feed the next bytes off the bulk endpoint. Swapped pixels are written
    /// to `out`, which needs room for `data.len() + 1` bytes. Returns the
    /// number of bytes written and whether the update just completed.
    /// Bytes beyond the update's length are ignored, as is anything fed
    /// while no update is active.
    pub fn feed(&mut self, data: &[u8], out: &mut [u8]) -> (usize, bool) {
        if self.remaining == 0 {
            return (0, false);
        }
        let take = data.len().min(self.remaining as usize);
        self.remaining -= take as u32;

        let mut n = 0;
        let mut bytes = data[..take].iter().copied();
        if let Some(lo) = self.carry.take() {
            if let Some(hi) = bytes.next() {
                out[n] = hi;
                out[n + 1] = lo;
                n += 2;
            }
        }
        while let Some(lo) = bytes.next() {
            match bytes.next() {
                Some(hi) => {
                    out[n] = hi;
                    out[n + 1] = lo;
                    n += 2;
                }
                None => self.carry = Some(lo),
            }
        }

        let done = self.remaining == 0;
        if done {
            self.carry = None;
        }
        (n, done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_pairs() {
        let mut px = [1, 2, 3, 4, 5];
        swap_pairs(&mut px);
        assert_eq!(px, [2, 1, 4, 3, 5]);
    }

    #[test]
    fn stream_carries_split_pixels_across_packets() {
        let mut s = PixelStream::new();
        let mut out = [0u8; 8];
        assert_eq!(s.feed(&[1, 2], &mut out), (0, false));

        s.begin(6);
        assert!(s.is_active());
        // Three bytes: one whole pixel and a dangling low byte.
        assert_eq!(s.feed(&[0x11, 0x22, 0x33], &mut out), (2, false));
        assert_eq!(&out[..2], &[0x22, 0x11]);
        // The dangling byte pairs with the first byte here; extra bytes
        // past the update length are dropped.
        assert_eq!(s.feed(&[0x44, 0x55, 0x66, 0x77, 0x88], &mut out), (4, true));
        assert_eq!(&out[..4], &[0x44, 0x33, 0x66, 0x55]);
        assert!(!s.is_active());
        assert_eq!(s.feed(&[9, 9], &mut out), (0, false));
    }

    #[test]
    fn abort_discards_carry() {
        let mut s = PixelStream::new();
        let mut out = [0u8; 4];
        s.begin(4);
        s.feed(&[1], &mut out);
        assert!(s.abort());
        assert!(!s.abort());
        s.begin(2);
        assert_eq!(s.feed(&[2, 3], &mut out), (2, true));
        assert_eq!(&out[..2], &[3, 2]);
    }
}
