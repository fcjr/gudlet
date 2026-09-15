//! Touch input for the gudlet boards, shared between microcontrollers.
//!
//! GUD carries pixels only, so touch goes to the host the standard way: a
//! USB HID touch screen interface (HID 1.11, Usage Tables 1.4 Digitizer
//! page) beside the GUD interface on the same composite device. Linux binds
//! hid-multitouch to it and macOS binds its HID stack, with nothing to
//! install on either.
//!
//! [`hid`] holds the report descriptor and the report encoding, [`cst816`]
//! the I2C driver for the CST816 controller on the Waveshare 1.69" boards,
//! and the `usb-device` / `embassy-usb` features add the glue that answers
//! the class requests each stack routes to the interface.

#![no_std]

pub mod cst816;
pub mod hid;

#[cfg(feature = "embassy-usb")]
pub mod embassy;
#[cfg(feature = "usb-device")]
pub mod usbd;

pub use cst816::{Cst816, Point};
pub use hid::{Panel, ReportCell, TouchHidState, TouchReport, TouchTracker};
