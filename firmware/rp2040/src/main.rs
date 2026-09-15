//! GUD (Generic USB Display) firmware for the Waveshare RP2040-Touch-LCD-1.69.
//!
//! Enumerates as a GUD device (1d50:614d) and streams the host's framebuffer
//! updates through a bounded LZ4 and DMA pipeline into the ST7789V2 panel.
//!
//! A CDC-ACM serial port sits beside the GUD interface. It prints a status
//! line once a second while open, and opening it at 1200 baud reboots the
//! board into the USB bootloader (the Arduino convention) so the firmware
//! can be reflashed without touching the BOOT button.

#![no_std]
#![no_main]

mod gud;
mod panel;

use core::fmt::Write;

use embedded_hal::pwm::SetDutyCycle;
use panic_halt as _;
use rp2040_hal as hal;

use gud_panel::{St7789, HEIGHT, WIDTH};
use gud_protocol::{USB_PID, USB_VID};
use hal::fugit::RateExtU32;
use hal::gpio::{FunctionSpi, PinState};
use hal::pac;
use hal::usb::UsbBus;
use hal::Clock;
use usb_device::bus::UsbBusAllocator;
use usb_device::device::{StringDescriptors, UsbDeviceBuilder, UsbVidPid};
use usb_device::LangID;
use usbd_serial::SerialPort;

use gud::{Band, FreeChannel, GudDevice, JobChannel, BAND_BYTES};
use hal::dma::DMAExt;
use hal::multicore::{Multicore, Stack};
use portable_atomic::Ordering;
use static_cell::ConstStaticCell;

static JOBS: JobChannel = JobChannel::new();
static FREE: FreeChannel = FreeChannel::new();
static INPUT: ConstStaticCell<[Band; gud::NUM_BANDS]> =
    ConstStaticCell::new([[0; BAND_BYTES]; gud::NUM_BANDS]);
static DECODED: ConstStaticCell<Band> = ConstStaticCell::new([0; BAND_BYTES]);
static DMA_BUFFERS: ConstStaticCell<[[u8; panel::DMA_CHUNK]; 2]> =
    ConstStaticCell::new([[0; panel::DMA_CHUNK]; 2]);
// Stack size is in 32-bit words: 8 KiB for core 1.
static CORE1_STACK: Stack<2048> = Stack::new();

#[link_section = ".boot2"]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_GENERIC_03H;

const XTAL_FREQ_HZ: u32 = 12_000_000;

// Keep the vendor-demo rate until faster SPI is visually validated on the
// panel. USB/LZ4 and DMA overlap remain independent of this clock.
const SPI_BAUD_HZ: u32 = 31_250_000;

/// Shows up in the serial port's device name, so scripts can find it.
const USB_SERIAL: &str = "GUDRP2040";

/// Opening the serial port at this rate and closing it reboots to BOOTSEL.
const BOOTSEL_BAUD: u32 = 1200;
const STATUS_INTERVAL_MS: u64 = 1000;

/// Fixed-size line buffer so status lines can be formatted without alloc.
struct Line {
    buf: [u8; 256],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self {
            buf: [0; 256],
            len: 0,
        }
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

fn wake_panel() {
    cortex_m::asm::sev();
}

fn payload_stall() -> ! {
    cortex_m::peripheral::SCB::sys_reset()
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

    let mut sio = hal::Sio::new(pac.SIO);
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

    for band in INPUT.take() {
        assert!(FREE.try_send(band).is_ok());
    }
    let [a, b] = DMA_BUFFERS.take();
    let dma = pac.DMA.split(&mut pac.RESETS);
    let worker = panel::PanelWorker::new(lcd, spi, dma.ch0, [a, b]);
    let mut multicore = Multicore::new(&mut pac.PSM, &mut pac.PPB, &mut sio.fifo);
    multicore.cores()[1]
        .spawn(CORE1_STACK.take().unwrap(), move || {
            worker.run(&JOBS, &FREE, DECODED.take());
        })
        .unwrap();

    let usb_bus = UsbBusAllocator::new(UsbBus::new(
        pac.USBCTRL_REGS,
        pac.USBCTRL_DPRAM,
        clocks.usb_clock,
        true,
        &mut pac.RESETS,
    ));

    // Interface order matters: the GUD interface must be allocated first so
    // hosts that take the first vendor-class interface find the right one.
    let mut gud = GudDevice::new(&usb_bus, backlight, &JOBS, &FREE);
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
    let mut pending_status: Option<Line> = None;
    let mut status_sent = 0;
    loop {
        gud.service(timer.get_counter().ticks());
        usb_dev.poll(&mut [&mut gud, &mut serial]);

        if serial.line_coding().data_rate() == BOOTSEL_BAUD && !serial.dtr() {
            hal::rom_data::reset_to_usb_boot(0, 0);
        }

        // Discard anything typed at the console.
        let mut sink = [0u8; 64];
        let _ = serial.read(&mut sink);

        // usbd-serial may accept only part of a status line. Retain the tail
        // and send it over later polls; otherwise long lines lose their newline.
        if !serial.dtr() {
            pending_status = None;
        } else if let Some(line) = pending_status.as_ref() {
            if let Ok(n) = serial.write(&line.as_bytes()[status_sent..]) {
                status_sent += n;
                if status_sent == line.as_bytes().len() {
                    pending_status = None;
                }
            }
        }

        let now = timer.get_counter();
        if serial.dtr()
            && pending_status.is_none()
            && (now - last_status).to_millis() >= STATUS_INTERVAL_MS
        {
            last_status = now;
            let stats = gud.stats();
            let mut line = Line::new();
            let _ = write!(
                line,
                "gud {}x{} rx={} bytes={} wire={} done={} written={} decode_errors={} rejected={} last=0x{:02x} bl={} on={} spi={} up={}\r\n",
                WIDTH,
                HEIGHT,
                stats.updates,
                stats.bytes,
                stats.wire_bytes,
                panel::COMPLETED.load(Ordering::Relaxed),
                panel::PIXEL_BYTES.load(Ordering::Relaxed),
                panel::DECODE_ERRORS.load(Ordering::Relaxed),
                stats.rejected,
                stats.last_rejected_request,
                stats.brightness,
                panel::ENABLED.load(Ordering::Relaxed),
                SPI_BAUD_HZ,
                now.ticks() / 1_000_000
            );
            pending_status = Some(line);
            status_sent = 0;
        }
    }
}
