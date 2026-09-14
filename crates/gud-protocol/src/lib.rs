//! Device side of the GUD (Generic USB Display) protocol, version 1,
//! independent of any USB stack or panel.
//!
//! The Linux host driver (drivers/gpu/drm/gud) is the de facto spec. All
//! multi-byte fields are little-endian and packed. The device exposes one
//! vendor-class interface with a single bulk OUT endpoint; control requests
//! describe the display and announce each framebuffer update, and the pixel
//! data for that update follows on the bulk endpoint.
//!
//! [`Protocol`] answers the control requests. It knows nothing about the
//! transport: the firmware feeds it the request number, `wValue` and the
//! data stage, and gets back either the bytes to return or a status code to
//! stall with. Anything that touches the panel comes out as a [`Command`]
//! through a sink the firmware supplies, so it can be applied directly
//! (single-core, streaming) or queued to another task or core.
//!
//! [`stream`] holds the per-packet byte-swapping for firmware that streams
//! updates straight to the panel without buffering them.

#![no_std]

pub mod stream;

/// Vendor request numbers.
pub mod req {
    pub const GET_STATUS: u8 = 0x00;
    pub const GET_DESCRIPTOR: u8 = 0x01;
    pub const GET_FORMATS: u8 = 0x40;
    pub const GET_PROPERTIES: u8 = 0x41;
    pub const GET_CONNECTORS: u8 = 0x50;
    pub const GET_CONNECTOR_PROPERTIES: u8 = 0x51;
    pub const SET_CONNECTOR_FORCE_DETECT: u8 = 0x53;
    pub const GET_CONNECTOR_STATUS: u8 = 0x54;
    pub const GET_CONNECTOR_MODES: u8 = 0x55;
    pub const GET_CONNECTOR_EDID: u8 = 0x56;
    pub const SET_BUFFER: u8 = 0x60;
    pub const SET_STATE_CHECK: u8 = 0x61;
    pub const SET_STATE_COMMIT: u8 = 0x62;
    pub const SET_CONTROLLER_ENABLE: u8 = 0x63;
    pub const SET_DISPLAY_ENABLE: u8 = 0x64;
}

/// Status codes returned by `GET_STATUS`.
pub mod status {
    pub const OK: u8 = 0x00;
    pub const REQUEST_NOT_SUPPORTED: u8 = 0x02;
    pub const PROTOCOL_ERROR: u8 = 0x03;
    pub const INVALID_PARAMETER: u8 = 0x04;
}

/// VID/PID from the Linux driver's match table.
pub const USB_VID: u16 = 0x1d50;
pub const USB_PID: u16 = 0x614d;

pub const BULK_PACKET_SIZE: u16 = 64;
pub const BYTES_PER_PIXEL: u32 = 2;
pub const COMPRESSION_LZ4: u8 = 1 << 0;
/// Largest response `control_in` writes.
pub const MAX_IN_LEN: usize = 32;

const MAGIC: u32 = 0x1d50_614d;
const VERSION: u8 = 1;
const PIXEL_FORMAT_RGB565: u8 = 0x40;
const CONNECTOR_TYPE_PANEL: u8 = 0;
const CONNECTOR_STATUS_CONNECTED: u8 = 0x01;
const MODE_FLAG_PREFERRED: u32 = 1 << 10;
const PROPERTY_BACKLIGHT_BRIGHTNESS: u16 = 12;

/// What the device advertises about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    pub width: u16,
    pub height: u16,
    /// Bitmask of accepted compression types (`COMPRESSION_LZ4`).
    pub compression: u8,
    /// Largest uncompressed update the host may send, or 0 for unlimited.
    pub max_buffer_size: u32,
}

impl Display {
    /// A fixed-size RGB565 panel taking raw updates of any size.
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height, compression: 0, max_buffer_size: 0 }
    }

    /// Accept LZ4 block compressed updates of at most `max_buffer_size`
    /// uncompressed bytes.
    pub const fn with_lz4(self, max_buffer_size: u32) -> Self {
        Self { compression: COMPRESSION_LZ4, max_buffer_size, ..self }
    }

    pub const fn frame_bytes(&self) -> usize {
        self.width as usize * self.height as usize * BYTES_PER_PIXEL as usize
    }

    fn descriptor(&self, buf: &mut [u8]) -> usize {
        let d = &mut buf[..30];
        d[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        d[4] = VERSION;
        d[5..9].copy_from_slice(&0u32.to_le_bytes()); // flags
        d[9] = self.compression;
        d[10..14].copy_from_slice(&self.max_buffer_size.to_le_bytes());
        d[14..18].copy_from_slice(&(self.width as u32).to_le_bytes());
        d[18..22].copy_from_slice(&(self.width as u32).to_le_bytes());
        d[22..26].copy_from_slice(&(self.height as u32).to_le_bytes());
        d[26..30].copy_from_slice(&(self.height as u32).to_le_bytes());
        30
    }

    /// One preferred mode at about 60 Hz. Blanking is fictional (SPI panels
    /// have no sync signals) but hosts expect a mode that is electrically
    /// plausible and works out to a sane refresh rate.
    fn mode(&self, buf: &mut [u8]) -> usize {
        let (w, h) = (self.width, self.height);
        let htotal = w as u32 + 10;
        let vtotal = h as u32 + 10;
        let clock_khz = htotal * vtotal * 60 / 1000;
        let m = &mut buf[..24];
        m[0..4].copy_from_slice(&clock_khz.to_le_bytes());
        m[4..6].copy_from_slice(&w.to_le_bytes());
        m[6..8].copy_from_slice(&(w + 1).to_le_bytes());
        m[8..10].copy_from_slice(&(w + 2).to_le_bytes());
        m[10..12].copy_from_slice(&(htotal as u16).to_le_bytes());
        m[12..14].copy_from_slice(&h.to_le_bytes());
        m[14..16].copy_from_slice(&(h + 1).to_le_bytes());
        m[16..18].copy_from_slice(&(h + 2).to_le_bytes());
        m[18..20].copy_from_slice(&(vtotal as u16).to_le_bytes());
        m[20..24].copy_from_slice(&MODE_FLAG_PREFERRED.to_le_bytes());
        24
    }
}

/// One framebuffer update announced by `SET_BUFFER`, already validated
/// against the display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    /// Uncompressed pixel bytes.
    pub length: u32,
    /// Bytes actually sent on the bulk endpoint (== length when raw).
    pub payload: u32,
    pub compressed: bool,
}

/// Work for the panel, in the order it must be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// An update follows on the bulk endpoint.
    Update(Rect),
    /// Backlight, 0..=100.
    Brightness(u8),
    /// Display on/off.
    Enable(bool),
}

fn property(buf: &mut [u8], prop: u16, value: u64) -> usize {
    buf[0..2].copy_from_slice(&prop.to_le_bytes());
    buf[2..10].copy_from_slice(&value.to_le_bytes());
    10
}

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Control request handler and connector state.
pub struct Protocol {
    display: Display,
    status: u8,
    brightness: u8,
    pending_brightness: u8,
    enabled: bool,
}

impl Protocol {
    pub const fn new(display: Display) -> Self {
        Self {
            display,
            status: status::OK,
            brightness: 100,
            pending_brightness: 100,
            enabled: true,
        }
    }

    pub fn display(&self) -> &Display {
        &self.display
    }

    /// Status of the last request, as `GET_STATUS` reports it.
    pub fn status(&self) -> u8 {
        self.status
    }

    pub fn brightness(&self) -> u8 {
        self.brightness
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// USB bus reset.
    pub fn reset(&mut self) {
        self.status = status::OK;
    }

    /// Answer a vendor IN request. `Ok(n)` means reply with `buf[..n]`
    /// (`buf` must hold [`MAX_IN_LEN`] bytes); `Err(code)` means stall,
    /// with `code` recorded for `GET_STATUS`.
    pub fn control_in(&mut self, request: u8, value: u16, buf: &mut [u8]) -> Result<usize, u8> {
        let connector = value;
        let len = match request {
            // Reports the previous request's status; does not replace it.
            req::GET_STATUS => {
                buf[0] = self.status;
                return Ok(1);
            }
            req::GET_DESCRIPTOR => self.display.descriptor(buf),
            req::GET_FORMATS => {
                buf[0] = PIXEL_FORMAT_RGB565;
                1
            }
            req::GET_PROPERTIES => 0,
            req::GET_CONNECTORS => {
                buf[..5].copy_from_slice(&[CONNECTOR_TYPE_PANEL, 0, 0, 0, 0]);
                5
            }
            req::GET_CONNECTOR_PROPERTIES
            | req::GET_CONNECTOR_STATUS
            | req::GET_CONNECTOR_MODES
            | req::GET_CONNECTOR_EDID
                if connector != 0 =>
            {
                return Err(self.fail(status::INVALID_PARAMETER));
            }
            req::GET_CONNECTOR_PROPERTIES => {
                property(buf, PROPERTY_BACKLIGHT_BRIGHTNESS, self.brightness as u64)
            }
            req::GET_CONNECTOR_STATUS => {
                buf[0] = CONNECTOR_STATUS_CONNECTED;
                1
            }
            req::GET_CONNECTOR_MODES => self.display.mode(buf),
            // Fixed panel: no EDID, modes come from GET_CONNECTOR_MODES.
            req::GET_CONNECTOR_EDID => 0,
            // GET_CONNECTOR_TV_MODE_VALUES and anything unknown.
            _ => return Err(self.fail(status::REQUEST_NOT_SUPPORTED)),
        };
        self.status = status::OK;
        Ok(len)
    }

    /// Handle a vendor OUT request with its data stage. Panel work is
    /// handed to `sink` in order; if the sink fails, the request is
    /// rejected with the sink's status code. `Err(code)` means stall.
    pub fn control_out(
        &mut self,
        request: u8,
        data: &[u8],
        mut sink: impl FnMut(Command) -> Result<(), u8>,
    ) -> Result<(), u8> {
        let result = match request {
            req::SET_BUFFER => self.set_buffer(data).and_then(|rect| sink(Command::Update(rect))),
            req::SET_STATE_CHECK => self.state_check(data),
            req::SET_STATE_COMMIT => self.state_commit(&mut sink),
            req::SET_CONTROLLER_ENABLE => Ok(()),
            req::SET_DISPLAY_ENABLE => match data.first() {
                Some(&on) => self.set_display_enabled(on != 0, &mut sink),
                None => Err(status::PROTOCOL_ERROR),
            },
            req::SET_CONNECTOR_FORCE_DETECT => Ok(()),
            _ => Err(status::REQUEST_NOT_SUPPORTED),
        };
        match result {
            Ok(()) => {
                self.status = status::OK;
                Ok(())
            }
            Err(code) => Err(self.fail(code)),
        }
    }

    fn fail(&mut self, code: u8) -> u8 {
        self.status = code;
        code
    }

    fn set_buffer(&mut self, data: &[u8]) -> Result<Rect, u8> {
        if data.len() < 25 {
            return Err(status::PROTOCOL_ERROR);
        }
        let x = le_u32(&data[0..]);
        let y = le_u32(&data[4..]);
        let width = le_u32(&data[8..]);
        let height = le_u32(&data[12..]);
        let length = le_u32(&data[16..]);
        let compression = data[20];
        let compressed_length = le_u32(&data[21..]);

        let d = &self.display;
        if width == 0
            || height == 0
            || x + width > d.width as u32
            || y + height > d.height as u32
            || length != width * height * BYTES_PER_PIXEL
            || (d.max_buffer_size != 0 && length > d.max_buffer_size)
        {
            return Err(status::INVALID_PARAMETER);
        }
        let (compressed, payload) = match compression {
            0 => (false, length),
            c if c == COMPRESSION_LZ4
                && d.compression & COMPRESSION_LZ4 != 0
                && compressed_length > 0
                && compressed_length <= length =>
            {
                (true, compressed_length)
            }
            _ => return Err(status::INVALID_PARAMETER),
        };

        Ok(Rect {
            x: x as u16,
            y: y as u16,
            width: width as u16,
            height: height as u16,
            length,
            payload,
            compressed,
        })
    }

    fn state_check(&mut self, data: &[u8]) -> Result<(), u8> {
        if data.len() < 26 {
            return Err(status::PROTOCOL_ERROR);
        }
        let hdisplay = le_u16(&data[4..]);
        let vdisplay = le_u16(&data[12..]);
        let format = data[24];
        let connector = data[25];
        if hdisplay != self.display.width
            || vdisplay != self.display.height
            || format != PIXEL_FORMAT_RGB565
            || connector != 0
        {
            return Err(status::INVALID_PARAMETER);
        }

        self.pending_brightness = self.brightness;
        for prop in data[26..].chunks_exact(10) {
            let id = le_u16(prop);
            let value = le_u32(&prop[2..]);
            match id {
                PROPERTY_BACKLIGHT_BRIGHTNESS => {
                    if value > 100 {
                        return Err(status::INVALID_PARAMETER);
                    }
                    self.pending_brightness = value as u8;
                }
                // Unknown properties are ignored for forward compatibility.
                _ => {}
            }
        }
        Ok(())
    }

    fn state_commit(&mut self, sink: &mut impl FnMut(Command) -> Result<(), u8>) -> Result<(), u8> {
        if self.pending_brightness != self.brightness {
            self.brightness = self.pending_brightness;
            if self.enabled {
                sink(Command::Brightness(self.brightness))?;
            }
        }
        Ok(())
    }

    fn set_display_enabled(
        &mut self,
        on: bool,
        sink: &mut impl FnMut(Command) -> Result<(), u8>,
    ) -> Result<(), u8> {
        if on == self.enabled {
            return Ok(());
        }
        self.enabled = on;
        sink(Command::Enable(on))?;
        sink(Command::Brightness(if on { self.brightness } else { 0 }))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    const RAW: Display = Display::new(240, 280);
    const LZ4: Display = Display::new(240, 280).with_lz4(240 * 280 * 2);

    fn set_buffer(x: u32, y: u32, w: u32, h: u32, len: u32, compression: u8, clen: u32) -> [u8; 25] {
        let mut d = [0u8; 25];
        d[0..4].copy_from_slice(&x.to_le_bytes());
        d[4..8].copy_from_slice(&y.to_le_bytes());
        d[8..12].copy_from_slice(&w.to_le_bytes());
        d[12..16].copy_from_slice(&h.to_le_bytes());
        d[16..20].copy_from_slice(&len.to_le_bytes());
        d[20] = compression;
        d[21..25].copy_from_slice(&clen.to_le_bytes());
        d
    }

    fn state(props: &[(u16, u32)]) -> Vec<u8> {
        let mut d = std::vec![0u8; 26];
        d[4..6].copy_from_slice(&240u16.to_le_bytes());
        d[12..14].copy_from_slice(&280u16.to_le_bytes());
        d[24] = PIXEL_FORMAT_RGB565;
        for &(id, value) in props {
            d.extend_from_slice(&id.to_le_bytes());
            d.extend_from_slice(&(value as u64).to_le_bytes());
        }
        d
    }

    fn collect(p: &mut Protocol, request: u8, data: &[u8]) -> (Result<(), u8>, Vec<Command>) {
        let mut out = Vec::new();
        let r = p.control_out(request, data, |c| {
            out.push(c);
            Ok(())
        });
        (r, out)
    }

    #[test]
    fn descriptor_advertises_display() {
        let mut p = Protocol::new(LZ4);
        let mut buf = [0u8; MAX_IN_LEN];
        let n = p.control_in(req::GET_DESCRIPTOR, 0, &mut buf).unwrap();
        assert_eq!(n, 30);
        assert_eq!(le_u32(&buf[0..]), MAGIC);
        assert_eq!(buf[4], VERSION);
        assert_eq!(buf[9], COMPRESSION_LZ4);
        assert_eq!(le_u32(&buf[10..]), 240 * 280 * 2);
        assert_eq!(le_u32(&buf[14..]), 240);
        assert_eq!(le_u32(&buf[22..]), 280);

        let n = p.control_in(req::GET_CONNECTOR_MODES, 0, &mut buf).unwrap();
        assert_eq!(n, 24);
        assert_eq!(le_u16(&buf[4..]), 240);
        assert_eq!(le_u16(&buf[12..]), 280);
        assert_eq!(le_u32(&buf[20..]), MODE_FLAG_PREFERRED);
    }

    #[test]
    fn status_follows_last_request() {
        let mut p = Protocol::new(RAW);
        let mut buf = [0u8; MAX_IN_LEN];
        assert_eq!(p.control_in(0x7f, 0, &mut buf), Err(status::REQUEST_NOT_SUPPORTED));
        assert_eq!(p.control_in(req::GET_STATUS, 0, &mut buf), Ok(1));
        assert_eq!(buf[0], status::REQUEST_NOT_SUPPORTED);
        assert_eq!(p.control_in(req::GET_CONNECTOR_STATUS, 1, &mut buf), Err(status::INVALID_PARAMETER));
        assert_eq!(p.control_in(req::GET_FORMATS, 0, &mut buf), Ok(1));
        assert_eq!(buf[0], PIXEL_FORMAT_RGB565);
        p.control_in(req::GET_STATUS, 0, &mut buf).unwrap();
        assert_eq!(buf[0], status::OK);
    }

    #[test]
    fn set_buffer_validates_rectangle() {
        let mut p = Protocol::new(RAW);
        let (r, cmds) = collect(&mut p, req::SET_BUFFER, &set_buffer(10, 20, 100, 50, 100 * 50 * 2, 0, 0));
        assert_eq!(r, Ok(()));
        assert_eq!(
            cmds,
            [Command::Update(Rect {
                x: 10,
                y: 20,
                width: 100,
                height: 50,
                length: 10000,
                payload: 10000,
                compressed: false
            })]
        );

        let bad = [
            set_buffer(0, 0, 0, 10, 0, 0, 0),
            set_buffer(200, 0, 41, 10, 41 * 10 * 2, 0, 0),
            set_buffer(0, 271, 10, 10, 200, 0, 0),
            set_buffer(0, 0, 10, 10, 199, 0, 0),
            set_buffer(0, 0, 10, 10, 200, COMPRESSION_LZ4, 100),
        ];
        for data in &bad {
            let (r, cmds) = collect(&mut p, req::SET_BUFFER, data);
            assert_eq!(r, Err(status::INVALID_PARAMETER));
            assert!(cmds.is_empty());
        }
        assert_eq!(collect(&mut p, req::SET_BUFFER, &[0; 24]).0, Err(status::PROTOCOL_ERROR));
    }

    #[test]
    fn set_buffer_accepts_lz4_when_advertised() {
        let mut p = Protocol::new(LZ4);
        let (r, cmds) = collect(&mut p, req::SET_BUFFER, &set_buffer(0, 0, 240, 280, 134400, COMPRESSION_LZ4, 5000));
        assert_eq!(r, Ok(()));
        match cmds[0] {
            Command::Update(rect) => {
                assert!(rect.compressed);
                assert_eq!(rect.payload, 5000);
                assert_eq!(rect.length, 134400);
            }
            _ => panic!(),
        }
        let (r, _) = collect(&mut p, req::SET_BUFFER, &set_buffer(0, 0, 240, 280, 134400, COMPRESSION_LZ4, 134401));
        assert_eq!(r, Err(status::INVALID_PARAMETER));
        let (r, _) = collect(&mut p, req::SET_BUFFER, &set_buffer(0, 0, 240, 280, 134400, COMPRESSION_LZ4, 0));
        assert_eq!(r, Err(status::INVALID_PARAMETER));
    }

    #[test]
    fn sink_failure_rejects_request() {
        let mut p = Protocol::new(RAW);
        let r = p.control_out(req::SET_BUFFER, &set_buffer(0, 0, 10, 10, 200, 0, 0), |_| Err(status::PROTOCOL_ERROR));
        assert_eq!(r, Err(status::PROTOCOL_ERROR));
        assert_eq!(p.status(), status::PROTOCOL_ERROR);
    }

    #[test]
    fn brightness_is_staged_then_committed() {
        let mut p = Protocol::new(RAW);
        let (r, cmds) = collect(&mut p, req::SET_STATE_CHECK, &state(&[(PROPERTY_BACKLIGHT_BRIGHTNESS, 40)]));
        assert_eq!(r, Ok(()));
        assert!(cmds.is_empty());
        assert_eq!(p.brightness(), 100);
        let (r, cmds) = collect(&mut p, req::SET_STATE_COMMIT, &[]);
        assert_eq!(r, Ok(()));
        assert_eq!(cmds, [Command::Brightness(40)]);
        assert_eq!(p.brightness(), 40);
        // Unchanged brightness commits nothing.
        let _ = collect(&mut p, req::SET_STATE_CHECK, &state(&[(PROPERTY_BACKLIGHT_BRIGHTNESS, 40)]));
        assert!(collect(&mut p, req::SET_STATE_COMMIT, &[]).1.is_empty());
        // Unknown properties are ignored, out-of-range brightness is not.
        assert_eq!(collect(&mut p, req::SET_STATE_CHECK, &state(&[(99, 7)])).0, Ok(()));
        assert_eq!(
            collect(&mut p, req::SET_STATE_CHECK, &state(&[(PROPERTY_BACKLIGHT_BRIGHTNESS, 101)])).0,
            Err(status::INVALID_PARAMETER)
        );
        let mut wrong = state(&[]);
        wrong[4] = 0;
        assert_eq!(collect(&mut p, req::SET_STATE_CHECK, &wrong).0, Err(status::INVALID_PARAMETER));
    }

    #[test]
    fn display_enable_toggles_panel_and_backlight() {
        let mut p = Protocol::new(RAW);
        let _ = collect(&mut p, req::SET_STATE_CHECK, &state(&[(PROPERTY_BACKLIGHT_BRIGHTNESS, 30)]));
        let _ = collect(&mut p, req::SET_STATE_COMMIT, &[]);
        assert!(collect(&mut p, req::SET_DISPLAY_ENABLE, &[1]).1.is_empty());
        let (r, cmds) = collect(&mut p, req::SET_DISPLAY_ENABLE, &[0]);
        assert_eq!(r, Ok(()));
        assert_eq!(cmds, [Command::Enable(false), Command::Brightness(0)]);
        // Brightness changes while off are remembered, not applied.
        let _ = collect(&mut p, req::SET_STATE_CHECK, &state(&[(PROPERTY_BACKLIGHT_BRIGHTNESS, 60)]));
        assert!(collect(&mut p, req::SET_STATE_COMMIT, &[]).1.is_empty());
        let (_, cmds) = collect(&mut p, req::SET_DISPLAY_ENABLE, &[1]);
        assert_eq!(cmds, [Command::Enable(true), Command::Brightness(60)]);
        assert_eq!(collect(&mut p, req::SET_DISPLAY_ENABLE, &[]).0, Err(status::PROTOCOL_ERROR));
    }
}
