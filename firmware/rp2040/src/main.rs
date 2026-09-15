//! GUD (Generic USB Display) firmware for the Waveshare RP2040-Touch-LCD-1.69.
//!
//! Enumerates as a GUD device (1d50:614d) and streams the host's framebuffer
//! updates straight into the board's ST7789V2 panel.
//!
//! A CDC-ACM serial port sits beside the GUD interface. It prints a status
//! line once a second while open, and opening it at 1200 baud reboots the
//! board into the USB bootloader (the Arduino convention) so the firmware
//! can be reflashed without touching the BOOT button.

#![no_std]
#![no_main]

mod gud;

use core::fmt::Write;

use embedded_hal::digital::OutputPin;
use embedded_hal::pwm::SetDutyCycle;
use embedded_hal::spi::SpiBus;
use panic_halt as _;
use rp2040_hal as hal;

use hal::fugit::RateExtU32;
use hal::gpio::{FunctionSpi, PinState};
use hal::pac;
use hal::usb::UsbBus;
use hal::Clock;
use usb_device::bus::UsbBusAllocator;
use usb_device::device::{StringDescriptors, UsbDeviceBuilder, UsbVidPid};
use usb_device::LangID;
use gud_panel::{St7789, HEIGHT, WIDTH};
use gud_protocol::{USB_PID, USB_VID};
use usbd_serial::SerialPort;

use gud::{GudDevice, Panel};

#[link_section = ".boot2"]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_GENERIC_03H;

const XTAL_FREQ_HZ: u32 = 12_000_000;

// The panel's SPI write cycle bottoms out at 16 ns; 31.25 MHz is what the
// vendor demo actually runs at and is far faster than full-speed USB can feed.
const SPI_BAUD_HZ: u32 = 31_250_000;

/// Shows up in the serial port's device name, so scripts can find it.
const USB_SERIAL: &str = "GUDRP2040";

/// Opening the serial port at this rate and closing it reboots to BOOTSEL.
const BOOTSEL_BAUD: u32 = 1200;
const STATUS_INTERVAL_MS: u64 = 1000;

/// Fixed-size line buffer so status lines can be formatted without alloc.
struct Line {
    buf: [u8; 96],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self { buf: [0; 96], len: 0 }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

struct Board<SPI, DC, CS, RST, BL> {
    spi: SPI,
    lcd: St7789<DC, CS, RST>,
    backlight: BL,
}

impl<SPI, DC, CS, RST, BL> Panel for Board<SPI, DC, CS, RST, BL>
where
    SPI: SpiBus,
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
    BL: SetDutyCycle,
{
    fn begin_rect(&mut self, x: u16, y: u16, width: u16, height: u16) {
        self.lcd.begin_rect(&mut self.spi, x, y, width, height);
    }

    fn write_pixels(&mut self, data: &[u8]) {
        let _ = self.spi.write(data);
    }

    fn end_rect(&mut self) {
        let _ = self.spi.flush();
        self.lcd.end_rect();
    }

    fn set_enabled(&mut self, on: bool) {
        self.lcd.set_display_on(&mut self.spi, on);
    }

    fn set_brightness(&mut self, percent: u8) {
        let _ = self.backlight.set_duty_cycle_percent(percent.min(100));
    }
}

#[hal::entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();
    let mut watchdog = hal::Watchdog::new(pac.WATCHDOG);
    let clocks = hal::clocks::init_clocks_and_plls(
        XTAL_FREQ_HZ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();
    let mut timer = hal::Timer::new(pac.TIMER, &mut pac.RESETS, &clocks);

    let sio = hal::Sio::new(pac.SIO);
    let pins = hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // LCD wiring per Waveshare's DEV_Config.h: SPI1 with DC 8, CS 9,
    // CLK 10, MOSI 11, MISO 12, RST 13, backlight PWM on 25.
    let sclk = pins.gpio10.into_function::<FunctionSpi>();
    let mosi = pins.gpio11.into_function::<FunctionSpi>();
    let miso = pins.gpio12.into_function::<FunctionSpi>();
    let mut spi = hal::Spi::<_, _, _, 8>::new(pac.SPI1, (mosi, miso, sclk)).init(
        &mut pac.RESETS,
        clocks.peripheral_clock.freq(),
        SPI_BAUD_HZ.Hz(),
        embedded_hal::spi::MODE_0,
    );
    let dc = pins.gpio8.into_push_pull_output();
    let cs = pins.gpio9.into_push_pull_output_in_state(PinState::High);
    let rst = pins.gpio13.into_push_pull_output_in_state(PinState::High);

    // ~25 kHz backlight PWM, duty in percent (matches the vendor demo).
    let pwm_slices = hal::pwm::Slices::new(pac.PWM, &mut pac.RESETS);
    let mut pwm = pwm_slices.pwm4;
    pwm.set_top(100);
    pwm.set_div_int(50);
    pwm.enable();
    let mut backlight = pwm.channel_b;
    backlight.output_to(pins.gpio25);
    let _ = backlight.set_duty_cycle_percent(0);

    let mut lcd = St7789::new(dc, cs, rst);
    lcd.init(&mut spi, &mut timer);
    lcd.show_boot_logo(&mut spi);
    let _ = backlight.set_duty_cycle_percent(100);

    let usb_bus = UsbBusAllocator::new(UsbBus::new(
        pac.USBCTRL_REGS,
        pac.USBCTRL_DPRAM,
        clocks.usb_clock,
        true,
        &mut pac.RESETS,
    ));

    // Interface order matters: the GUD interface must be allocated first so
    // hosts that take the first vendor-class interface find the right one.
    let mut gud = GudDevice::new(&usb_bus, Board { spi, lcd, backlight });
    let mut serial = SerialPort::new(&usb_bus);

    let strings = StringDescriptors::new(LangID::EN_US)
        .manufacturer("Waveshare")
        .product("RP2040-Touch-LCD-1.69")
        .serial_number(USB_SERIAL);
    let mut usb_dev = UsbDeviceBuilder::new(&usb_bus, UsbVidPid(USB_VID, USB_PID))
        // Composite (0xEF/2/1) with IADs so macOS configures the device and
        // binds its CDC driver without any host-side help.
        .composite_with_iads()
        .max_packet_size_0(64)
        .unwrap()
        .strings(&[strings])
        .unwrap()
        .build();

    let mut last_status = timer.get_counter();
    loop {
        usb_dev.poll(&mut [&mut gud, &mut serial]);

        if serial.line_coding().data_rate() == BOOTSEL_BAUD && !serial.dtr() {
            hal::rom_data::reset_to_usb_boot(0, 0);
        }

        // Discard anything typed at the console.
        let mut sink = [0u8; 64];
        let _ = serial.read(&mut sink);

        let now = timer.get_counter();
        if serial.dtr() && (now - last_status).to_millis() >= STATUS_INTERVAL_MS {
            last_status = now;
            let stats = gud.stats();
            let mut line = Line::new();
            let _ = write!(
                line,
                "gud {}x{} updates={} bytes={} rejected={} last_rejected=0x{:02x}\r\n",
                WIDTH,
                HEIGHT,
                stats.updates,
                stats.bytes,
                stats.rejected,
                stats.last_rejected_request
            );
            let _ = serial.write(line.as_bytes());
        }
    }
}
