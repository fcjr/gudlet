//! The HID touch screen report descriptor and the reports it describes.
//!
//! One digitizer application collection, one finger, laid out the way the
//! Windows touch requirements and Linux's hid-multitouch both expect: Tip
//! Switch, Contact Identifier, X and Y with physical units, Scan Time in
//! 100 us units, and a Contact Count per input report, plus a Contact Count
//! Maximum feature report the host reads once with GET_REPORT.

use core::sync::atomic::{AtomicU32, Ordering};

/// Report ID of the touch input report.
pub const TOUCH_REPORT_ID: u8 = 1;
/// Report ID of the Contact Count Maximum feature report.
pub const MAX_CONTACTS_REPORT_ID: u8 = 2;
/// The CST816 tracks one finger.
pub const MAX_CONTACTS: u8 = 1;
/// Bytes in an input report, report ID included.
pub const TOUCH_REPORT_LEN: usize = 10;
/// Bytes in the feature report, report ID included.
pub const FEATURE_REPORT_LEN: usize = 2;
/// Bytes in the report descriptor.
pub const REPORT_DESCRIPTOR_LEN: usize = 124;

/// `bInterval` for the interrupt IN endpoint: full-speed frames are 1 ms,
/// and the controller scans every 10 ms.
pub const POLL_MS: u8 = 10;

/// Report types in the high byte of `wValue` for GET_REPORT / SET_REPORT
/// (HID 1.11 section 7.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportKind {
    Input = 1,
    Output = 2,
    Feature = 3,
}

impl ReportKind {
    pub const fn from_wvalue(value: u16) -> Option<Self> {
        match value >> 8 {
            1 => Some(Self::Input),
            2 => Some(Self::Output),
            3 => Some(Self::Feature),
            _ => None,
        }
    }
}

/// Touch surface geometry the descriptor advertises: logical range in
/// panel pixels and physical size in tenths of a millimetre.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Panel {
    pub width: u16,
    pub height: u16,
    pub width_tenth_mm: u16,
    pub height_tenth_mm: u16,
}

/// Builds the report descriptor for a panel. The layout is fixed; only the
/// logical and physical maxima change with the panel.
pub const fn report_descriptor(panel: Panel) -> [u8; REPORT_DESCRIPTOR_LEN] {
    let [xl, xh] = (panel.width - 1).to_le_bytes();
    let [yl, yh] = (panel.height - 1).to_le_bytes();
    let [pxl, pxh] = panel.width_tenth_mm.to_le_bytes();
    let [pyl, pyh] = panel.height_tenth_mm.to_le_bytes();
    [
        0x05, 0x0D, // Usage Page (Digitizer)
        0x09, 0x04, // Usage (Touch Screen)
        0xA1, 0x01, // Collection (Application)
        0x85, TOUCH_REPORT_ID, //   Report ID
        0x09, 0x22, //   Usage (Finger)
        0xA1, 0x02, //   Collection (Logical)
        0x09, 0x42, //     Usage (Tip Switch)
        0x15, 0x00, //     Logical Minimum (0)
        0x25, 0x01, //     Logical Maximum (1)
        0x75, 0x01, //     Report Size (1)
        0x95, 0x01, //     Report Count (1)
        0x81, 0x02, //     Input (Data, Variable, Absolute)
        0x95, 0x07, //     Report Count (7)
        0x81, 0x03, //     Input (Constant, Variable, Absolute): pad to a byte
        0x09, 0x51, //     Usage (Contact Identifier)
        0x25, 0x7F, //     Logical Maximum (127)
        0x75, 0x08, //     Report Size (8)
        0x95, 0x01, //     Report Count (1)
        0x81, 0x02, //     Input (Data, Variable, Absolute)
        0x05, 0x01, //     Usage Page (Generic Desktop)
        0x09, 0x30, //     Usage (X)
        0x15, 0x00, //     Logical Minimum (0)
        0x26, xl, xh, //   Logical Maximum (width - 1)
        0x35, 0x00, //     Physical Minimum (0)
        0x46, pxl, pxh, // Physical Maximum (width)
        0x55, 0x0E, //     Unit Exponent (-2)
        0x65, 0x11, //     Unit (SI Linear, centimetre): 0.01 cm steps
        0x75, 0x10, //     Report Size (16)
        0x95, 0x01, //     Report Count (1)
        0x81, 0x02, //     Input (Data, Variable, Absolute)
        0x09, 0x31, //     Usage (Y)
        0x26, yl, yh, //   Logical Maximum (height - 1)
        0x46, pyl, pyh, // Physical Maximum (height)
        0x81, 0x02, //     Input (Data, Variable, Absolute)
        0x55, 0x00, //     Unit Exponent (0)
        0x65, 0x00, //     Unit (None)
        0xC0, //         End Collection
        0x05, 0x0D, //   Usage Page (Digitizer)
        0x09, 0x56, //   Usage (Scan Time)
        0x27, 0xFF, 0xFF, 0x00, 0x00, // Logical Maximum (65535)
        0x55, 0x0C, //   Unit Exponent (-4)
        0x66, 0x01, 0x10, // Unit (SI Linear, second): 100 us steps
        0x75, 0x10, //   Report Size (16)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x02, //   Input (Data, Variable, Absolute)
        0x55, 0x00, //   Unit Exponent (0)
        0x65, 0x00, //   Unit (None)
        0x09, 0x54, //   Usage (Contact Count)
        0x25, 0x7F, //   Logical Maximum (127)
        0x75, 0x08, //   Report Size (8)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x02, //   Input (Data, Variable, Absolute)
        0x85, MAX_CONTACTS_REPORT_ID, // Report ID
        0x09, 0x55, //   Usage (Contact Count Maximum)
        0x25, MAX_CONTACTS, // Logical Maximum
        0x75, 0x08, //   Report Size (8)
        0x95, 0x01, //   Report Count (1)
        0xB1, 0x02, //   Feature (Data, Variable, Absolute)
        0xC0, // End Collection
    ]
}

/// The Contact Count Maximum feature report.
pub const fn feature_report() -> [u8; FEATURE_REPORT_LEN] {
    [MAX_CONTACTS_REPORT_ID, MAX_CONTACTS]
}

/// One touch input report: the finger's state in panel pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TouchReport {
    /// Finger on the glass.
    pub tip: bool,
    pub x: u16,
    pub y: u16,
    /// Free-running time in 100 us units; wraps.
    pub scan_time: u16,
}

impl TouchReport {
    /// Wire layout, report ID first.
    pub fn to_bytes(&self) -> [u8; TOUCH_REPORT_LEN] {
        let [xl, xh] = self.x.to_le_bytes();
        let [yl, yh] = self.y.to_le_bytes();
        let [tl, th] = self.scan_time.to_le_bytes();
        // A lift-off is a report for the same contact with the tip switch
        // cleared, so the contact count stays 1 there too.
        [TOUCH_REPORT_ID, self.tip as u8, 0, xl, xh, yl, yh, tl, th, 1]
    }
}

/// Turns controller reads into the reports worth sending: every change
/// while the finger is down, and exactly one lift-off.
#[derive(Debug, Default)]
pub struct TouchTracker {
    last: Option<(u16, u16)>,
}

impl TouchTracker {
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Finger currently down, as last reported.
    pub fn touching(&self) -> bool {
        self.last.is_some()
    }

    pub fn update(&mut self, point: Option<(u16, u16)>, scan_time: u16) -> Option<TouchReport> {
        match (self.last, point) {
            (Some(prev), Some(now)) if prev == now => None,
            (_, Some((x, y))) => {
                self.last = Some((x, y));
                Some(TouchReport { tip: true, x, y, scan_time })
            }
            (Some((x, y)), None) => {
                self.last = None;
                Some(TouchReport { tip: false, x, y, scan_time })
            }
            (None, None) => None,
        }
    }
}

/// The report most recently sent on the interrupt endpoint, shared between
/// whoever reads the controller and the control request handler that
/// answers GET_REPORT for it. Two words with plain loads and stores, so it
/// works on cores without compare-and-swap; a torn read can only mix two
/// consecutive reports.
#[derive(Debug)]
pub struct ReportCell {
    xy: AtomicU32,
    rest: AtomicU32,
}

impl Default for ReportCell {
    fn default() -> Self {
        Self::new()
    }
}

impl ReportCell {
    pub const fn new() -> Self {
        Self { xy: AtomicU32::new(0), rest: AtomicU32::new(0) }
    }

    pub fn store(&self, report: &TouchReport) {
        self.xy.store(u32::from(report.x) << 16 | u32::from(report.y), Ordering::Relaxed);
        self.rest.store(u32::from(report.tip) << 16 | u32::from(report.scan_time), Ordering::Relaxed);
    }

    pub fn load(&self) -> TouchReport {
        let xy = self.xy.load(Ordering::Relaxed);
        let rest = self.rest.load(Ordering::Relaxed);
        TouchReport {
            tip: rest >> 16 != 0,
            x: (xy >> 16) as u16,
            y: xy as u16,
            scan_time: rest as u16,
        }
    }
}

/// Everything the class requests need to know: the report the host would
/// get from GET_REPORT, and the idle rate it last set.
#[derive(Debug)]
pub struct TouchHidState {
    last: &'static ReportCell,
    idle_ms: u32,
}

impl TouchHidState {
    pub const fn new(last: &'static ReportCell) -> Self {
        Self { last, idle_ms: 0 }
    }

    /// GET_REPORT: copies the requested report into `buf`, or `None` to stall.
    pub fn get_report(&self, kind: ReportKind, id: u8, buf: &mut [u8]) -> Option<usize> {
        let (bytes, len): (&[u8], usize) = match (kind, id) {
            (ReportKind::Input, TOUCH_REPORT_ID) => (&self.last.load().to_bytes(), TOUCH_REPORT_LEN),
            (ReportKind::Feature, MAX_CONTACTS_REPORT_ID) => (&feature_report(), FEATURE_REPORT_LEN),
            _ => return None,
        };
        if buf.len() < len {
            return None;
        }
        buf[..len].copy_from_slice(&bytes[..len]);
        Some(len)
    }

    /// SET_IDLE: `duration_ms` is 0 for "only when the report changes",
    /// which is how the device reports anyway.
    pub fn set_idle(&mut self, duration_ms: u32) {
        self.idle_ms = duration_ms;
    }

    /// GET_IDLE, in milliseconds.
    pub fn idle_ms(&self) -> u32 {
        self.idle_ms
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    const PANEL: Panel = Panel { width: 240, height: 280, width_tenth_mm: 280, height_tenth_mm: 326 };

    /// Minimal HID item walker: (tag byte, data) pairs, enough to check the
    /// descriptor is well formed and to find the fields the host relies on.
    fn items(desc: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < desc.len() {
            let prefix = desc[i];
            let size = match prefix & 0x03 {
                3 => 4,
                n => n as usize,
            };
            out.push((prefix & 0xFC, desc[i + 1..i + 1 + size].to_vec()));
            i += 1 + size;
        }
        out
    }

    #[test]
    fn descriptor_is_well_formed_and_balanced() {
        let desc = report_descriptor(PANEL);
        let items = items(&desc);
        let opens = items.iter().filter(|(tag, _)| *tag == 0xA0).count();
        let closes = items.iter().filter(|(tag, _)| *tag == 0xC0).count();
        assert_eq!(opens, 2);
        assert_eq!(closes, 2);
        // Ends exactly at the last byte.
        assert_eq!(items.iter().map(|(_, d)| 1 + d.len()).sum::<usize>(), REPORT_DESCRIPTOR_LEN);
    }

    #[test]
    fn descriptor_carries_the_panel_geometry() {
        let desc = report_descriptor(PANEL);
        let items = items(&desc);
        let logical_max: Vec<Vec<u8>> = items.iter().filter(|(t, _)| *t == 0x24).map(|(_, d)| d.clone()).collect();
        assert!(logical_max.contains(&vec![239, 0]));
        assert!(logical_max.contains(&vec![(279 & 0xFF) as u8, 1]));
        let physical_max: Vec<Vec<u8>> = items.iter().filter(|(t, _)| *t == 0x44).map(|(_, d)| d.clone()).collect();
        assert_eq!(physical_max, vec![vec![(280 & 0xFF) as u8, 1], vec![(326 & 0xFF) as u8, 1]]);
    }

    #[test]
    fn descriptor_declares_the_usages_hosts_require() {
        let desc = report_descriptor(PANEL);
        let items = items(&desc);
        let mut page = 0u8;
        let mut usages = Vec::new();
        for (tag, data) in &items {
            match tag {
                0x04 => page = data[0],
                0x08 => usages.push((page, data[0])),
                _ => {}
            }
        }
        for required in [(0x0D, 0x04), (0x0D, 0x22), (0x0D, 0x42), (0x0D, 0x51), (0x01, 0x30), (0x01, 0x31),
                         (0x0D, 0x56), (0x0D, 0x54), (0x0D, 0x55)] {
            assert!(usages.contains(&required), "missing usage {required:?}");
        }
    }

    #[test]
    fn input_report_bit_layout_matches_the_descriptor() {
        let desc = report_descriptor(PANEL);
        // Sum input bits under report ID 1: 1 + 7 + 8 + 16 + 16 + 16 + 8 = 72 = 9 bytes + ID.
        let mut size = 0u32;
        let mut count = 0u32;
        let mut bits = 0u32;
        for (tag, data) in items(&desc) {
            match tag {
                0x74 => size = data[0] as u32,
                0x94 => count = data[0] as u32,
                0x80 => bits += size * count,
                _ => {}
            }
        }
        assert_eq!(bits, (TOUCH_REPORT_LEN as u32 - 1) * 8);
    }

    #[test]
    fn reports_encode_little_endian_with_ids() {
        let report = TouchReport { tip: true, x: 0x0102, y: 0x0116, scan_time: 0xBEEF };
        assert_eq!(report.to_bytes(), [1, 1, 0, 0x02, 0x01, 0x16, 0x01, 0xEF, 0xBE, 1]);
        let up = TouchReport { tip: false, x: 5, y: 6, scan_time: 0 };
        assert_eq!(up.to_bytes(), [1, 0, 0, 5, 0, 6, 0, 0, 0, 1]);
        assert_eq!(feature_report(), [2, 1]);
    }

    #[test]
    fn tracker_reports_changes_and_one_lift_off() {
        let mut tracker = TouchTracker::new();
        assert_eq!(tracker.update(None, 1), None);
        assert_eq!(tracker.update(Some((10, 20)), 2), Some(TouchReport { tip: true, x: 10, y: 20, scan_time: 2 }));
        assert_eq!(tracker.update(Some((10, 20)), 3), None);
        assert!(tracker.touching());
        assert_eq!(tracker.update(Some((11, 20)), 4), Some(TouchReport { tip: true, x: 11, y: 20, scan_time: 4 }));
        assert_eq!(tracker.update(None, 5), Some(TouchReport { tip: false, x: 11, y: 20, scan_time: 5 }));
        assert_eq!(tracker.update(None, 6), None);
        assert!(!tracker.touching());
    }

    #[test]
    fn get_report_serves_input_and_feature_reports() {
        static LAST: ReportCell = ReportCell::new();
        let state = TouchHidState::new(&LAST);
        let mut buf = [0u8; 16];
        assert_eq!(state.get_report(ReportKind::Feature, MAX_CONTACTS_REPORT_ID, &mut buf), Some(2));
        assert_eq!(&buf[..2], &[2, 1]);
        let report = TouchReport { tip: true, x: 1, y: 2, scan_time: 3 };
        LAST.store(&report);
        assert_eq!(LAST.load(), report);
        assert_eq!(state.get_report(ReportKind::Input, TOUCH_REPORT_ID, &mut buf), Some(TOUCH_REPORT_LEN));
        assert_eq!(&buf[..TOUCH_REPORT_LEN], &report.to_bytes());
        assert_eq!(state.get_report(ReportKind::Output, 1, &mut buf), None);
        assert_eq!(state.get_report(ReportKind::Feature, 9, &mut buf), None);
        assert_eq!(state.get_report(ReportKind::Input, TOUCH_REPORT_ID, &mut buf[..4]), None);
        assert_eq!(ReportKind::from_wvalue(0x0302), Some(ReportKind::Feature));
        assert_eq!(ReportKind::from_wvalue(0x0401), None);
    }
}
