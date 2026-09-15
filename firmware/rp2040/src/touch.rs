//! Touch input: the CST816 on I2C1, reported to the host as a USB HID touch
//! screen (see the `gud-touch` crate for the descriptor and the driver).
//!
//! Polled from the main loop like everything else on this board: the
//! controller is read when its INT line is low, every 10 ms while a finger
//! is down in case a pulse was missed, and every 250 ms while idle.

use gud_panel::{HEIGHT, HEIGHT_TENTH_MM, WIDTH, WIDTH_TENTH_MM};
use gud_touch::hid::{report_descriptor, REPORT_DESCRIPTOR_LEN};
use gud_touch::usbd::TouchHidClass;
use gud_touch::{Cst816, Panel, ReportCell, TouchReport, TouchTracker};
use embedded_hal::digital::InputPin;
use embedded_hal::digital::OutputPin;
use embedded_hal::i2c::I2c;
use usb_device::bus::UsbBus;
use usb_device::UsbError;

/// Touch is always reported in the glass's own portrait frame, whatever
/// rotation the host has set on the display: a rotated desktop is the
/// host's transform to apply, as it is for any touch screen.
const PANEL: Panel = Panel {
    width: WIDTH,
    height: HEIGHT,
    width_tenth_mm: WIDTH_TENTH_MM,
    height_tenth_mm: HEIGHT_TENTH_MM,
};

pub static REPORT_DESCRIPTOR: [u8; REPORT_DESCRIPTOR_LEN] = report_descriptor(PANEL);
/// Last report sent, for GET_REPORT.
pub static LAST_REPORT: ReportCell = ReportCell::new();

const TOUCHING_POLL_US: u64 = 10_000;
const IDLE_POLL_US: u64 = 250_000;

#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub reports: u32,
    pub errors: u32,
}

pub struct Touch<I2C, RST, INT> {
    controller: Cst816<I2C, RST>,
    int: INT,
    tracker: TouchTracker,
    last_read_us: u64,
    /// Report the endpoint could not take yet.
    pending: Option<TouchReport>,
    stats: Stats,
}

impl<I2C: I2c, RST: OutputPin, INT: InputPin> Touch<I2C, RST, INT> {
    pub fn new(controller: Cst816<I2C, RST>, int: INT) -> Self {
        Self {
            controller,
            int,
            tracker: TouchTracker::new(),
            last_read_us: 0,
            pending: None,
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// One turn of the main loop: flush a held report, then read the
    /// controller if it is due.
    pub fn service<B: UsbBus>(&mut self, now_us: u64, hid: &mut TouchHidClass<'_, B>) {
        if let Some(report) = self.pending {
            match hid.push(&report) {
                Ok(()) => {
                    self.pending = None;
                    self.stats.reports = self.stats.reports.wrapping_add(1);
                }
                Err(UsbError::WouldBlock) => return,
                Err(_) => self.pending = None,
            }
        }

        let interval = if self.tracker.touching() { TOUCHING_POLL_US } else { IDLE_POLL_US };
        let int_low = self.int.is_low().unwrap_or(false);
        if !int_low && now_us.wrapping_sub(self.last_read_us) < interval {
            return;
        }
        self.last_read_us = now_us;

        let point = match self.controller.read() {
            Ok(point) => point,
            Err(_) => {
                self.stats.errors = self.stats.errors.wrapping_add(1);
                None
            }
        };
        let point = point.map(|p| (p.x.min(WIDTH - 1), p.y.min(HEIGHT - 1)));
        let scan_time = (now_us / 100) as u16;
        if let Some(report) = self.tracker.update(point, scan_time) {
            self.pending = Some(report);
            match hid.push(&report) {
                Ok(()) => {
                    self.pending = None;
                    self.stats.reports = self.stats.reports.wrapping_add(1);
                }
                Err(UsbError::WouldBlock) => {}
                Err(_) => self.pending = None,
            }
        }
    }
}
