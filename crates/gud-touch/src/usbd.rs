//! HID touch screen class for the usb-device stack: one interface, one
//! interrupt IN endpoint, and the class requests of HID 1.11 section 7.
//!
//! Written here rather than taken from usbd-hid because that crate stalls
//! GET_REPORT, and a touch screen has to answer it for the Contact Count
//! Maximum feature report.

use usb_device::class_prelude::*;
use usb_device::control::{Recipient, Request, RequestType};

use crate::hid::{ReportCell, ReportKind, TouchHidState, TouchReport, POLL_MS, TOUCH_REPORT_LEN};

const CLASS_HID: u8 = 0x03;
const DESCRIPTOR_HID: u8 = 0x21;
const DESCRIPTOR_REPORT: u8 = 0x22;
const REQ_GET_REPORT: u8 = 0x01;
const REQ_GET_IDLE: u8 = 0x02;
const REQ_SET_IDLE: u8 = 0x0A;

pub struct TouchHidClass<'a, B: UsbBus> {
    interface: InterfaceNumber,
    ep_in: EndpointIn<'a, B>,
    report_descriptor: &'static [u8],
    last: &'static ReportCell,
    state: TouchHidState,
}

impl<'a, B: UsbBus> TouchHidClass<'a, B> {
    pub fn new(alloc: &'a UsbBusAllocator<B>, report_descriptor: &'static [u8], last: &'static ReportCell) -> Self {
        Self {
            interface: alloc.interface(),
            ep_in: alloc.interrupt(TOUCH_REPORT_LEN as u16, POLL_MS),
            report_descriptor,
            last,
            state: TouchHidState::new(last),
        }
    }

    /// Queues a report on the interrupt endpoint. `WouldBlock` means the
    /// previous one has not been picked up yet; try again next poll.
    pub fn push(&mut self, report: &TouchReport) -> usb_device::Result<()> {
        self.ep_in.write(&report.to_bytes())?;
        self.last.store(report);
        Ok(())
    }

    fn is_ours(&self, req: &Request) -> bool {
        req.recipient == Recipient::Interface && req.index == u8::from(self.interface) as u16
    }
}

impl<B: UsbBus> UsbClass<B> for TouchHidClass<'_, B> {
    fn get_configuration_descriptors(&self, writer: &mut DescriptorWriter) -> usb_device::Result<()> {
        writer.iad(self.interface, 1, CLASS_HID, 0, 0, None)?;
        writer.interface(self.interface, CLASS_HID, 0, 0)?;
        let len = self.report_descriptor.len() as u16;
        writer.write(
            DESCRIPTOR_HID,
            &[
                0x11, 0x01, // bcdHID 1.11
                0x00, // bCountryCode: not localised
                0x01, // bNumDescriptors
                DESCRIPTOR_REPORT,
                len as u8,
                (len >> 8) as u8,
            ],
        )?;
        writer.endpoint(&self.ep_in)
    }

    fn control_in(&mut self, xfer: ControlIn<B>) {
        let req = *xfer.request();
        if !self.is_ours(&req) {
            return;
        }
        match (req.request_type, req.request) {
            (RequestType::Standard, Request::GET_DESCRIPTOR) => {
                let result = match (req.value >> 8) as u8 {
                    DESCRIPTOR_REPORT => xfer.accept_with_static(self.report_descriptor),
                    DESCRIPTOR_HID => {
                        let len = self.report_descriptor.len() as u16;
                        xfer.accept_with(&[
                            9,
                            DESCRIPTOR_HID,
                            0x11,
                            0x01,
                            0x00,
                            0x01,
                            DESCRIPTOR_REPORT,
                            len as u8,
                            (len >> 8) as u8,
                        ])
                    }
                    _ => xfer.reject(),
                };
                let _ = result;
            }
            (RequestType::Class, REQ_GET_REPORT) => {
                let mut buf = [0u8; TOUCH_REPORT_LEN];
                let served = ReportKind::from_wvalue(req.value)
                    .and_then(|kind| self.state.get_report(kind, req.value as u8, &mut buf));
                let _ = match served {
                    Some(len) => xfer.accept_with(&buf[..len]),
                    None => xfer.reject(),
                };
            }
            (RequestType::Class, REQ_GET_IDLE) => {
                // Idle rate is reported in 4 ms units; 0 is indefinite.
                let _ = xfer.accept_with(&[(self.state.idle_ms() / 4) as u8]);
            }
            (RequestType::Class, _) => {
                let _ = xfer.reject();
            }
            _ => {}
        }
    }

    fn control_out(&mut self, xfer: ControlOut<B>) {
        let req = *xfer.request();
        if !self.is_ours(&req) || req.request_type != RequestType::Class {
            return;
        }
        match req.request {
            REQ_SET_IDLE => {
                self.state.set_idle(u32::from(req.value >> 8) * 4);
                let _ = xfer.accept();
            }
            // No output or writable feature reports (SET_REPORT); boot
            // protocol is not offered, so SET_PROTOCOL is rejected too.
            _ => {
                let _ = xfer.reject();
            }
        }
    }
}
