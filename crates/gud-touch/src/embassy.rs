//! Class request handling for embassy-usb's HID class.

use embassy_usb::class::hid::{ReportId, RequestHandler};
use embassy_usb::control::OutResponse;

use crate::hid::{ReportKind, TouchHidState};

impl RequestHandler for TouchHidState {
    fn get_report(&mut self, id: ReportId, buf: &mut [u8]) -> Option<usize> {
        let (kind, id) = match id {
            ReportId::In(id) => (ReportKind::Input, id),
            ReportId::Out(id) => (ReportKind::Output, id),
            ReportId::Feature(id) => (ReportKind::Feature, id),
        };
        TouchHidState::get_report(self, kind, id, buf)
    }

    fn set_report(&mut self, _id: ReportId, _data: &[u8]) -> OutResponse {
        OutResponse::Rejected
    }

    fn get_idle_ms(&mut self, _id: Option<ReportId>) -> Option<u32> {
        Some(self.idle_ms())
    }

    fn set_idle_ms(&mut self, _id: Option<ReportId>, duration_ms: u32) {
        self.set_idle(if duration_ms == u32::MAX { 0 } else { duration_ms });
    }
}
