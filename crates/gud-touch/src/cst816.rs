//! Hynitron CST816 capacitive touch controller (CST816S/T/D), as fitted to
//! the Waveshare 1.69" boards: I2C address 0x15, one finger, coordinates in
//! the panel's native portrait frame.
//!
//! Register map from Hynitron's CST816T register description as shipped in
//! Waveshare's demos.

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use embedded_hal::i2c::I2c;

pub const ADDRESS: u8 = 0x15;

pub mod reg {
    pub const GESTURE_ID: u8 = 0x01;
    pub const FINGER_NUM: u8 = 0x02;
    pub const XPOS_H: u8 = 0x03;
    pub const XPOS_L: u8 = 0x04;
    pub const YPOS_H: u8 = 0x05;
    pub const YPOS_L: u8 = 0x06;
    pub const CHIP_ID: u8 = 0xA7;
    pub const FW_VERSION: u8 = 0xA9;
    /// Interrupt low pulse width in 0.1 ms, 1..=200, default 10.
    pub const IRQ_PULSE_WIDTH: u8 = 0xED;
    /// Scan period while touched in 10 ms units, 1..=30, default 1.
    pub const NOR_SCAN_PER: u8 = 0xEE;
    pub const IRQ_CTL: u8 = 0xFA;
    /// Non-zero keeps the chip out of its low-power mode, where it stops
    /// answering on I2C.
    pub const DIS_AUTO_SLEEP: u8 = 0xFE;
}

/// `IRQ_CTL` bits.
pub mod irq {
    /// Pulse INT every scan period while a finger is down.
    pub const EN_TOUCH: u8 = 0x40;
    /// Pulse INT when the touch state changes.
    pub const EN_CHANGE: u8 = 0x20;
    /// Pulse INT for recognised gestures.
    pub const EN_MOTION: u8 = 0x10;
}

/// Chip IDs seen in the `CHIP_ID` register.
pub mod chip {
    pub const CST816S: u8 = 0xB4;
    pub const CST816T: u8 = 0xB5;
    pub const CST816D: u8 = 0xB6;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    I2c(E),
    /// The chip ID register held something other than a CST816.
    UnknownChip(u8),
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Self {
        Self::I2c(e)
    }
}

/// Decodes `FINGER_NUM` through `YPOS_L` read as one block.
pub fn parse_point(regs: &[u8; 5]) -> Option<Point> {
    if regs[0] == 0 {
        return None;
    }
    // The high nibbles carry the 12-bit coordinate; the top bits of XPOS_H
    // are the event flag (press, lift, contact).
    Some(Point {
        x: u16::from(regs[1] & 0x0F) << 8 | u16::from(regs[2]),
        y: u16::from(regs[3] & 0x0F) << 8 | u16::from(regs[4]),
    })
}

pub struct Cst816<I2C, RST> {
    i2c: I2C,
    rst: RST,
}

impl<I2C: I2c, RST: OutputPin> Cst816<I2C, RST> {
    pub fn new(i2c: I2C, rst: RST) -> Self {
        Self { i2c, rst }
    }

    /// Hardware reset, identification, then point mode: INT pulses on every
    /// change and every scan while touched, auto-sleep off so the chip can
    /// always be read. Returns the chip ID.
    pub fn init(&mut self, delay: &mut impl DelayNs) -> Result<u8, Error<I2C::Error>> {
        let _ = self.rst.set_low();
        delay.delay_ms(10);
        let _ = self.rst.set_high();
        // The firmware needs a moment after reset before it answers.
        delay.delay_ms(100);

        let id = self.read_reg(reg::CHIP_ID)?;
        if !matches!(id, chip::CST816S | chip::CST816T | chip::CST816D) {
            return Err(Error::UnknownChip(id));
        }
        self.write_reg(reg::DIS_AUTO_SLEEP, 0x01)?;
        self.write_reg(reg::IRQ_CTL, irq::EN_TOUCH | irq::EN_CHANGE)?;
        Ok(id)
    }

    pub fn firmware_version(&mut self) -> Result<u8, I2C::Error> {
        self.read_reg(reg::FW_VERSION)
    }

    /// The finger's position, or `None` when nothing touches the glass.
    pub fn read(&mut self) -> Result<Option<Point>, I2C::Error> {
        let mut regs = [0u8; 5];
        self.i2c.write_read(ADDRESS, &[reg::FINGER_NUM], &mut regs)?;
        Ok(parse_point(&regs))
    }

    fn read_reg(&mut self, register: u8) -> Result<u8, I2C::Error> {
        let mut value = [0u8];
        self.i2c.write_read(ADDRESS, &[register], &mut value)?;
        Ok(value[0])
    }

    fn write_reg(&mut self, register: u8, value: u8) -> Result<(), I2C::Error> {
        self.i2c.write(ADDRESS, &[register, value])
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use embedded_hal::i2c::{ErrorType, Operation};
    use std::vec::Vec;

    #[test]
    fn parses_twelve_bit_coordinates_and_no_finger() {
        assert_eq!(parse_point(&[0, 0x80, 0x10, 0x01, 0x05]), None);
        assert_eq!(parse_point(&[1, 0x80, 0x10, 0x01, 0x05]), Some(Point { x: 0x010, y: 0x105 }));
        assert_eq!(parse_point(&[1, 0x4F, 0xFF, 0x0F, 0xFF]), Some(Point { x: 0xFFF, y: 0xFFF }));
    }

    /// Fake bus: answers register reads from a map and logs writes.
    #[derive(Default)]
    struct Bus {
        regs: std::collections::BTreeMap<u8, u8>,
        writes: Vec<Vec<u8>>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Nak;
    impl embedded_hal::i2c::Error for Nak {
        fn kind(&self) -> embedded_hal::i2c::ErrorKind {
            embedded_hal::i2c::ErrorKind::Other
        }
    }
    impl ErrorType for Bus {
        type Error = Nak;
    }
    impl I2c for Bus {
        fn transaction(&mut self, address: u8, ops: &mut [Operation<'_>]) -> Result<(), Nak> {
            assert_eq!(address, ADDRESS);
            let mut register = None;
            for op in ops {
                match op {
                    Operation::Write(bytes) => {
                        register = Some(bytes[0]);
                        if bytes.len() > 1 {
                            self.writes.push(bytes.to_vec());
                            self.regs.insert(bytes[0], bytes[1]);
                        }
                    }
                    Operation::Read(buf) => {
                        let start = register.ok_or(Nak)?;
                        for (i, b) in buf.iter_mut().enumerate() {
                            *b = *self.regs.get(&(start + i as u8)).ok_or(Nak)?;
                        }
                    }
                }
            }
            Ok(())
        }
    }

    struct Pin(Vec<bool>);
    impl embedded_hal::digital::ErrorType for Pin {
        type Error = core::convert::Infallible;
    }
    impl OutputPin for Pin {
        fn set_low(&mut self) -> Result<(), Self::Error> {
            self.0.push(false);
            Ok(())
        }
        fn set_high(&mut self) -> Result<(), Self::Error> {
            self.0.push(true);
            Ok(())
        }
    }

    struct NoDelay;
    impl DelayNs for NoDelay {
        fn delay_ns(&mut self, _: u32) {}
    }

    #[test]
    fn init_resets_identifies_and_configures_point_mode() {
        let mut bus = Bus::default();
        bus.regs.insert(reg::CHIP_ID, chip::CST816T);
        let mut ctp = Cst816::new(bus, Pin(Vec::new()));
        assert_eq!(ctp.init(&mut NoDelay), Ok(chip::CST816T));
        assert_eq!(ctp.rst.0, vec![false, true]);
        assert_eq!(ctp.i2c.writes, vec![vec![reg::DIS_AUTO_SLEEP, 0x01], vec![reg::IRQ_CTL, 0x60]]);
    }

    #[test]
    fn init_rejects_a_stranger() {
        let mut bus = Bus::default();
        bus.regs.insert(reg::CHIP_ID, 0x00);
        let mut ctp = Cst816::new(bus, Pin(Vec::new()));
        assert_eq!(ctp.init(&mut NoDelay), Err(Error::UnknownChip(0)));
        assert!(ctp.i2c.writes.is_empty());
    }

    #[test]
    fn read_returns_the_finger_position() {
        let mut bus = Bus::default();
        for (r, v) in [(reg::FINGER_NUM, 1), (reg::XPOS_H, 0), (reg::XPOS_L, 120), (reg::YPOS_H, 1), (reg::YPOS_L, 4)] {
            bus.regs.insert(r, v);
        }
        let mut ctp = Cst816::new(bus, Pin(Vec::new()));
        assert_eq!(ctp.read().unwrap(), Some(Point { x: 120, y: 260 }));
        ctp.i2c.regs.insert(reg::FINGER_NUM, 0);
        assert_eq!(ctp.read().unwrap(), None);
    }
}
