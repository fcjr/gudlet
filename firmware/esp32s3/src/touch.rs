//! Touch input: the CST816 on I2C, reported to the host as a USB HID touch
//! screen (see the `gud-touch` crate for the descriptor and the driver).
//!
//! The controller pulses INT on every change and every scan while a finger
//! is down. The task waits for that edge, with a short timeout while
//! touching so a missed pulse never sticks a finger, and a long one while
//! idle so a wedged INT line still gets noticed.

use core::sync::atomic::Ordering;

use embassy_time::{with_timeout, Duration, Instant};
use embassy_usb::class::hid::HidWriter;
use esp_hal::gpio::{Input, Output};
use esp_hal::i2c::master::I2c;
use esp_hal::usb::otg::embassy_usb_device::Driver;
use esp_hal::Blocking;
use gud_panel::{HEIGHT, HEIGHT_TENTH_MM, WIDTH, WIDTH_TENTH_MM};
use gud_touch::hid::{report_descriptor, REPORT_DESCRIPTOR_LEN, TOUCH_REPORT_LEN};
use gud_touch::{Cst816, Panel, ReportCell, TouchTracker};

use crate::gud::STATS;

pub type Controller = Cst816<I2c<'static, Blocking>, Output<'static>>;
pub type Writer = HidWriter<'static, Driver<'static>, TOUCH_REPORT_LEN>;

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

const TOUCHING_POLL: Duration = Duration::from_millis(10);
const IDLE_POLL: Duration = Duration::from_millis(250);

pub async fn touch_task(mut hid: Writer, mut controller: Controller, mut int: Input<'static>) -> ! {
    let mut tracker = TouchTracker::new();
    loop {
        let wait = if tracker.touching() { TOUCHING_POLL } else { IDLE_POLL };
        let _ = with_timeout(wait, int.wait_for_falling_edge()).await;
        let point = match controller.read() {
            Ok(point) => point,
            Err(_) => {
                STATS.touch_errors.fetch_add(1, Ordering::Relaxed);
                None
            }
        };
        let point = point.map(|p| (p.x.min(WIDTH - 1), p.y.min(HEIGHT - 1)));
        let scan_time = (Instant::now().as_micros() / 100) as u16;
        if let Some(report) = tracker.update(point, scan_time) {
            // A host that stopped polling the endpoint (suspend, no driver)
            // must not hold the reader; drop the report rather than wait.
            match with_timeout(Duration::from_millis(100), hid.write(&report.to_bytes())).await {
                Ok(Ok(())) => {
                    LAST_REPORT.store(&report);
                    STATS.touch_reports.fetch_add(1, Ordering::Relaxed);
                }
                _ => {
                    STATS.touch_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}
