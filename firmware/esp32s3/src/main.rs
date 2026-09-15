//! GUD (Generic USB Display) firmware for the Waveshare ESP32-S3-Touch-LCD-1.69.
//!
//! Enumerates as a GUD device (1d50:614d) on the ESP32-S3's USB OTG port and
//! streams the host's framebuffer updates straight into the board's ST7789V2
//! panel.
//!
//! The board's CST816 touch controller is exposed as a USB HID touch screen
//! interface (see `touch.rs`), so the host gets touch through its own HID
//! stack with nothing to install.
//!
//! A CDC-ACM serial port sits beside the GUD interface. It prints a status
//! line once a second while open, and writing `gud-reflash` to it reboots
//! the chip into the ROM download mode so espflash can reflash it without
//! touching the BOOT button.

#![no_std]
#![no_main]

mod gud;
mod panel;
mod touch;

use core::fmt::Write;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_futures::join::{join, join5};
use embassy_time::{with_timeout, Duration, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver, Sender, State};
use embassy_usb::class::hid;
use embassy_usb::Builder;
use esp_hal::clock::CpuClock;
use embedded_hal::delay::DelayNs;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::ledc::channel::ChannelIFace;
use esp_hal::ledc::timer::TimerIFace;
use esp_hal::ledc::{channel, timer, LSGlobalClkSource, Ledc, LowSpeed};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_hal::time::Duration as HalDuration;
use esp_hal::timer::timg::{MwdtStage, MwdtStageAction, TimerGroup, Wdt};
use esp_hal::usb::otg::embassy_usb_device::{Config as DriverConfig, Driver};
use esp_hal::usb::otg::Usb;
use esp_hal::system::Stack;
use gud_panel::{strip, St7789, HEIGHT, WIDTH};
use gud_protocol::{USB_PID, USB_VID};
use gud_touch::hid::{POLL_MS, TOUCH_REPORT_LEN};
use gud_touch::{Cst816, TouchHidState};
use static_cell::{ConstStaticCell, StaticCell};

use gud::{
    receive_task, Backlight, Band, CommandChannel, FreeChannel, GudHandler, JobChannel,
    BULK_PACKET_SIZE, NUM_BANDS, STATS,
};
use panel::PanelWorker;

esp_bootloader_esp_idf::esp_app_desc!();

/// Crash record kept in RTC RAM so it survives the reset that follows a
/// panic or a watchdog bite, and can be read off the console afterwards.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut CRASH: CrashRecord = CrashRecord::EMPTY;

#[derive(Clone, Copy)]
#[repr(C)]
struct CrashRecord {
    magic: u32,
    boots: u32,
    /// Last boot stage reached (see `stage()` calls in `main`).
    stage: u32,
    panic_len: u32,
    panic: [u8; 200],
    /// Set when the control handler found the console task stalled:
    /// (console tick, uptime) at detection.
    stall_tick: u32,
    stall_now: u32,
}

unsafe impl esp_hal::Persistable for CrashRecord {}

impl CrashRecord {
    const MAGIC: u32 = 0x6775_6421;
    const EMPTY: Self = Self {
        magic: 0,
        boots: 0,
        stage: 0,
        panic_len: 0,
        panic: [0; 200],
        stall_tick: 0,
        stall_now: 0,
    };
}

fn crash() -> &'static mut CrashRecord {
    // SAFETY: single-threaded access from main/console on core 0 and the
    // panic handler, which never returns.
    unsafe { &mut *core::ptr::addr_of_mut!(CRASH) }
}

fn stage(n: u32) {
    crash().stage = n;
}

/// The bulk receiver saw a payload stall: the host began an update and then
/// stopped sending partway through. The unframed bulk stream can only be
/// realigned by re-enumerating, so reboot. Fast boot brings the display back
/// in about a second and the host reattaches.
pub fn payload_stall() -> ! {
    esp_println::println!("gud: payload stall, resetting to realign bulk stream");
    esp_hal::system::software_reset()
}

/// Called from the USB control handler when the console task has stopped
/// ticking: record what we know and start over.
pub fn stall_detected(tick: u32, now: u32) -> ! {
    let record = crash();
    record.stall_tick = tick;
    record.stall_now = now;
    esp_hal::system::software_reset()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let record = crash();
    let mut line = Line::new();
    let _ = write!(line, "{}", info);
    let n = line.len.min(record.panic.len());
    record.panic[..n].copy_from_slice(&line.as_bytes()[..n]);
    record.panic_len = n as u32;
    esp_hal::system::software_reset()
}

/// Shows up in the serial port's device name, so scripts can find it.
const USB_SERIAL: &str = "GUDESP32S3";

/// Writing this to the serial console reboots into download mode.
const REFLASH_MAGIC: &[u8] = b"gud-reflash";
const CONSOLE_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const WATCHDOG_TIMEOUT_S: u64 = 5;

/// The panel's SPI write cycle bottoms out at 16 ns. The pins go through the
/// GPIO matrix rather than IOMUX, which is fine for an output-only bus.
const SPI_FREQUENCY_MHZ: u32 = 40;
/// The CST816 is rated for 10 to 400 kHz.
const TOUCH_I2C_KHZ: u32 = 400;

static COMMANDS: CommandChannel = CommandChannel::new();
static JOBS: JobChannel = JobChannel::new();
static FREE: FreeChannel = FreeChannel::new();
static HANDLER: StaticCell<GudHandler> = StaticCell::new();
// Const-initialised so the buffer lives in zeroed RAM from the start; a
// `StaticCell::init` would build it on the stack first, which overflows it.
// The (larger) payload buffers live in PSRAM.
static DECODED: ConstStaticCell<Band> = ConstStaticCell::new([0; gud::BAND_BYTES]);
const STATUS_SCRATCH_BYTES: usize = WIDTH as usize * strip::STRIP_H * 2;
static STATUS_SCRATCH: ConstStaticCell<[u8; STATUS_SCRATCH_BYTES]> = ConstStaticCell::new([0; STATUS_SCRATCH_BYTES]);
static CORE1_STACK: ConstStaticCell<Stack<16384>> = ConstStaticCell::new(Stack::new());

struct BacklightPwm(channel::Channel<'static, LowSpeed>);

impl Backlight for BacklightPwm {
    fn set_brightness(&mut self, percent: u8) {
        let _ = self.0.set_duty(percent.min(100));
    }
}

/// Fixed-size line buffer so status lines can be formatted without alloc.
struct Line {
    buf: [u8; 512],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self { buf: [0; 512], len: 0 }
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

/// Reboot into the ROM's USB download mode, where espflash can reach us.
fn reboot_to_download_mode() -> ! {
    // Disconnect while OTG still owns the PHY. Switching straight to the
    // ROM's USB-Serial-JTAG controller can leave D+ asserted throughout the
    // reset: macOS then keeps the old GUD address/descriptors and cannot talk
    // to the bootloader until a host USB reset or a physical replug.
    // No USB task is polled again on this path; mask the OTG interrupt before
    // detaching and leave enough time for the host to remove the old device.
    let otg = unsafe { &*esp32s3::USB_FS::ptr() };
    otg.gahbcfg().modify(|_, w| w.glbllntrmsk().clear_bit());
    otg.dctl().modify(|_, w| w.sftdiscon().set_bit());
    Delay::new().delay_ms(250);

    let rtc = unsafe { &*esp32s3::RTC_CNTL::ptr() };
    // The USB configuration lives in the RTC domain and survives every reset
    // but a power cycle. The OTG driver pointed the single PHY at itself;
    // put the whole register back to its power-on value (PHY follows the
    // eFuse default, USB-serial-JTAG, no pad or pull-up overrides) so the
    // ROM enumerates as the download port espflash expects. Left as is, the
    // ROM's download mode has no PHY and the board vanishes from the bus,
    // and BOOT+RESET can't fix that either since the register persists.
    rtc.usb_conf().reset();
    while rtc.usb_conf().read().bits() != 0 {}
    rtc.option1().modify(|_, w| w.force_download_boot().set_bit());
    while rtc.option1().read().force_download_boot().bit_is_clear() {}
    esp_hal::system::software_reset()
}

/// Reflash trigger: reboot into download mode when the host writes the
/// magic string to the console. (A 1200-baud "touch" was tried first, but
/// macOS caches a tty's line coding per device node and replays it on every
/// open, so one such touch kept rebooting the board forever after.)
async fn reflash_trigger_task<'d>(mut receiver: Receiver<'d, Driver<'d>>) -> ! {
    let mut buf = [0u8; 64];
    loop {
        receiver.wait_connection().await;
        while let Ok(n) = receiver.read_packet(&mut buf).await {
            STATS.console_rx_bytes.fetch_add(n as u32, Ordering::Relaxed);
            if buf[..n].windows(REFLASH_MAGIC.len()).any(|w| w == REFLASH_MAGIC) {
                reboot_to_download_mode();
            }
        }
    }
}

/// Holding BOOT for a second while the app runs reboots into download mode,
/// the same way the console's reflash string does. The button alone can't:
/// BOOT+RESET lands in the ROM with the USB PHY still pointed at the OTG
/// controller, so the download port never appears (see
/// `reboot_to_download_mode`). This path restores the PHY first.
async fn boot_button_task(button: Input<'static>) -> ! {
    let mut held_ms: u32 = 0;
    loop {
        Timer::after_millis(50).await;
        if button.is_low() {
            held_ms += 50;
            if held_ms >= 1000 {
                reboot_to_download_mode();
            }
        } else {
            held_ms = 0;
        }
    }
}

/// Debug console: one status line a second while the port is open. Writes
/// are bounded so a host that stops draining the port can't wedge the task.
/// Also feeds the watchdog, so a stalled executor reboots into the app.
async fn console_task<'d>(
    mut sender: Sender<'d, Driver<'d>>,
    mut watchdog: Wdt<esp_hal::peripherals::TIMG1<'static>>,
    reset_reason: Option<esp_hal::rtc_cntl::SocResetReason>,
    previous_stage: u32,
) -> ! {
    let mut uptime_s: u32 = 0;
    esp_println::println!("gud: console task running");
    loop {
        Timer::after_millis(1000).await;
        watchdog.feed();
        stage(5);
        gud::CONSOLE_TICK.store(embassy_time::Instant::now().as_secs() as u32, Ordering::Relaxed);
        uptime_s = uptime_s.wrapping_add(1);
        STATS.dtr.store(sender.dtr() as u32, Ordering::Relaxed);
        if !sender.dtr() {
            continue;
        }

        if uptime_s % 5 == 1 {
            let record = crash();
            let mut line = Line::new();
            let _ = write!(
                line,
                "boot #{} previous_stage={} last_stall={}/{} ",
                record.boots, previous_stage, record.stall_tick, record.stall_now
            );
            if record.panic_len > 0 {
                let n = record.panic_len as usize;
                let _ = write!(line, "last_panic={:?}", core::str::from_utf8(&record.panic[..n]).unwrap_or("?"));
            } else {
                let _ = write!(line, "last_panic=none");
            }
            let _ = write!(line, "\r\n");
            for chunk in line.as_bytes().chunks(63) {
                if with_timeout(CONSOLE_WRITE_TIMEOUT, sender.write_packet(chunk)).await.is_err() {
                    break;
                }
            }
        }

        let mut line = Line::new();
        let _ = write!(
            line,
            "gud {}x{} up={}s reset={:?} updates={} bytes={} wire={} rejected={} last_rejected=0x{:02x} abandoned={} rx_disabled={} rx_overflow={} decode_errors={} rx_us={} rx_pkts={} decode_us={} swap_us={} spi_us={} idle_us={} starve_us={} bands_written={} pixels_written={} panel_state=0x{:x} console_rx={} console_tx_ok={} console_tx_timeout={} touch={} touch_dropped={} touch_errors={}\r\n",
            WIDTH,
            HEIGHT,
            uptime_s,
            reset_reason,
            STATS.updates.load(Ordering::Relaxed),
            STATS.bytes.load(Ordering::Relaxed),
            STATS.wire_bytes.load(Ordering::Relaxed),
            STATS.rejected.load(Ordering::Relaxed),
            STATS.last_rejected_request.load(Ordering::Relaxed),
            STATS.abandoned.load(Ordering::Relaxed),
            STATS.rx_disabled.load(Ordering::Relaxed),
            STATS.rx_overflow.load(Ordering::Relaxed),
            STATS.decode_errors.load(Ordering::Relaxed),
            STATS.rx_us.load(Ordering::Relaxed),
            STATS.rx_packets.load(Ordering::Relaxed),
            STATS.decode_us.load(Ordering::Relaxed),
            STATS.swap_us.load(Ordering::Relaxed),
            STATS.spi_us.load(Ordering::Relaxed),
            STATS.idle_us.load(Ordering::Relaxed),
            STATS.starve_us.load(Ordering::Relaxed),
            STATS.bands_written.load(Ordering::Relaxed),
            STATS.pixels_written.load(Ordering::Relaxed),
            STATS.panel_state.load(Ordering::Relaxed),
            STATS.console_rx_bytes.load(Ordering::Relaxed),
            STATS.console_tx_ok.load(Ordering::Relaxed),
            STATS.console_tx_timeout.load(Ordering::Relaxed),
            STATS.touch_reports.load(Ordering::Relaxed),
            STATS.touch_dropped.load(Ordering::Relaxed),
            STATS.touch_errors.load(Ordering::Relaxed),
        );
        for chunk in line.as_bytes().chunks(63) {
            match with_timeout(CONSOLE_WRITE_TIMEOUT, sender.write_packet(chunk)).await {
                Ok(Ok(())) => {
                    STATS.console_tx_ok.fetch_add(1, Ordering::Relaxed);
                }
                _ => {
                    STATS.console_tx_timeout.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
    }
}

#[esp_hal::main]
async fn main(_spawner: Spawner) {
    let reset_reason = esp_hal::system::reset_reason();
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_println::println!("gud: boot");

    // Payload buffers live in PSRAM: they only ever hold what came off USB,
    // and the decoder reads them through the cache. PSRAM is brought up
    // first, before the scheduler and peripherals, because its cache
    // reconfiguration must not interrupt anything.
    let psram = esp_hal::psram::Psram::new(peripherals.PSRAM, esp_hal::psram::PsramConfig::default());
    let (psram_start, psram_size) = psram.raw_parts();
    assert!(psram_size >= NUM_BANDS * gud::BAND_BYTES, "PSRAM missing or too small");
    // SAFETY: PSRAM is otherwise unused; the region is carved once, here.
    let bands: &'static mut [Band] = unsafe { core::slice::from_raw_parts_mut(psram_start as *mut Band, NUM_BANDS) };
    let previous_stage = {
        let record = crash();
        if record.magic != CrashRecord::MAGIC {
            *record = CrashRecord::EMPTY;
            record.magic = CrashRecord::MAGIC;
        }
        record.boots = record.boots.wrapping_add(1);
        if record.panic_len > 0 {
            let n = record.panic_len as usize;
            esp_println::println!(
                "gud: previous run reached stage {} and panicked: {}",
                record.stage,
                core::str::from_utf8(&record.panic[..n]).unwrap_or("?")
            );
        }
        record.stage
    };
    stage(1);

    // If anything ever stalls the executor, come back as a working display
    // rather than a dead USB device.
    let mut watchdog = TimerGroup::new(peripherals.TIMG1).wdt;
    watchdog.set_timeout(MwdtStage::Stage0, HalDuration::from_secs(WATCHDOG_TIMEOUT_S));
    watchdog.set_stage_action(MwdtStage::Stage0, MwdtStageAction::ResetSystem);
    watchdog.enable();

    esp_println::println!("gud: stage 1 (watchdog armed)");
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // LCD wiring per Waveshare's pin_config.h: DC 4, CS 5, SCK 6, MOSI 7,
    // RST 8, backlight 15.
    let spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(SPI_FREQUENCY_MHZ))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(peripherals.GPIO6)
    .with_mosi(peripherals.GPIO7);
    // Scratch DMA buffers let the DMA handle carry small, unaligned slices
    // (panel commands); without them those writes fail silently.
    let mut spi = spi.with_dma(peripherals.DMA_CH0).with_buffers(
        esp_hal::dma_rx_buffer!(256).unwrap(),
        esp_hal::dma_tx_buffer!(256).unwrap(),
    );
    let dc = Output::new(peripherals.GPIO4, Level::Low, OutputConfig::default());
    let cs = Output::new(peripherals.GPIO5, Level::High, OutputConfig::default());
    let rst = Output::new(peripherals.GPIO8, Level::High, OutputConfig::default());
    let boot_button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));

    // Touch controller per the same pin table: SCL 10, SDA 11, RST 13, INT 14.
    // The board has pull-ups on the bus and on INT.
    let touch_i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(TOUCH_I2C_KHZ)),
    )
    .unwrap()
    .with_sda(peripherals.GPIO11)
    .with_scl(peripherals.GPIO10);
    let touch_rst = Output::new(peripherals.GPIO13, Level::High, OutputConfig::default());
    let touch_int = Input::new(peripherals.GPIO14, InputConfig::default().with_pull(Pull::Up));
    let mut touch_controller = Cst816::new(touch_i2c, touch_rst);
    let touch_present = match touch_controller.init(&mut Delay::new()) {
        Ok(id) => {
            esp_println::println!("gud: touch controller 0x{:02x} ready", id);
            true
        }
        Err(e) => {
            esp_println::println!("gud: no touch controller: {:?}", e);
            false
        }
    };

    // ~25 kHz backlight PWM, duty in percent.
    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);
    static BACKLIGHT_TIMER: StaticCell<timer::Timer<'static, LowSpeed>> = StaticCell::new();
    let backlight_timer = BACKLIGHT_TIMER.init(ledc.timer::<LowSpeed>(timer::Number::Timer0));
    backlight_timer
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_khz(25),
        })
        .unwrap();
    let mut backlight = ledc.channel(channel::Number::Channel0, peripherals.GPIO15);
    backlight
        .configure(channel::config::Config {
            timer: backlight_timer,
            duty_pct: 0,
            drive_mode: esp_hal::gpio::DriveMode::PushPull,
        })
        .unwrap();

    let mut lcd = St7789::new(dc, cs, rst);
    lcd.init(&mut spi, &mut Delay::new());
    esp_println::println!("gud: lcd init done");
    lcd.fill(&mut spi, 0x0000);
    esp_println::println!("gud: lcd cleared");
    let dma_buffers = [
        esp_hal::dma_tx_buffer!(panel::DMA_CHUNK).unwrap(),
        esp_hal::dma_tx_buffer!(panel::DMA_CHUNK).unwrap(),
    ];
    let mut board = PanelWorker::new(lcd, spi, dma_buffers);
    esp_println::println!("gud: dma buffers ready");
    let decoded = DECODED.take();
    board.show_boot_logo(&mut decoded[..]);
    let _ = backlight.set_duty(100);
    let backlight = BacklightPwm(backlight);
    {
        let record = crash();
        if record.panic_len > 0 {
            // Leave the last panic on the glass long enough to read it.
            let n = record.panic_len as usize;
            let mut message = [0u8; 200];
            message[..n].copy_from_slice(&record.panic[..n]);
            board.show_message(&message[..n], &mut decoded[..]);
            Delay::new().delay_ms(4000);
        } else {
            Delay::new().delay_ms(500);
        }
    }
    esp_println::println!("gud: boot logo shown");

    // USB OTG on the native pins (shared with the ROM's USB-Serial-JTAG).
    let usb = Usb::new_fs(peripherals.USB_FS, peripherals.GPIO20, peripherals.GPIO19);
    // The patched driver reserves 128 packets for every bulk OUT endpoint
    // (GUD and CDC data), plus endpoint 0 (see vendor/README.md).
    const EP_OUT_BUFFER_SIZE: usize = 2 * 128 * 64 + 64;
    static EP_OUT_BUFFER: ConstStaticCell<[u8; EP_OUT_BUFFER_SIZE]> = ConstStaticCell::new([0; EP_OUT_BUFFER_SIZE]);
    let driver = Driver::new(usb, EP_OUT_BUFFER.take(), DriverConfig::default());

    let mut config = embassy_usb::Config::new(USB_VID, USB_PID);
    config.manufacturer = Some("Waveshare");
    config.product = Some("ESP32-S3-Touch-LCD-1.69");
    config.serial_number = Some(USB_SERIAL);
    config.max_packet_size_0 = 64;
    // Composite (0xEF/2/1) with IADs so macOS configures the device and
    // binds its CDC driver without any host-side help.
    config.composite_with_iads = true;
    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;

    static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESCRIPTOR: StaticCell<[u8; 32]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESCRIPTOR.init([0; 256]),
        BOS_DESCRIPTOR.init([0; 32]),
        &mut [],
        CONTROL_BUF.init([0; 128]),
    );

    // The GUD interface goes first so hosts that take the first vendor-class
    // interface find the right one. Vendor-specific class/subclass is how the
    // Linux driver matches us.
    let bulk_out = {
        let mut function = builder.function(0xff, 0xff, 0);
        let mut interface = function.interface();
        let interface_number = interface.interface_number();
        let mut alt = interface.alt_setting(0xff, 0xff, 0, None);
        let bulk_out = alt.endpoint_bulk_out(None, BULK_PACKET_SIZE);
        drop(function);
        let handler = HANDLER.init(GudHandler::new(interface_number, &COMMANDS));
        builder.handler(handler);
        bulk_out
    };

    static SERIAL_STATE: StaticCell<State> = StaticCell::new();
    let serial = CdcAcmClass::new(&mut builder, SERIAL_STATE.init(State::new()), 64);
    let (sender, receiver) = serial.split();

    // HID touch screen, last so the GUD and console interfaces keep their
    // numbers, and only when there is a controller to report for.
    static HID_STATE: StaticCell<hid::State> = StaticCell::new();
    static TOUCH_HID: StaticCell<TouchHidState> = StaticCell::new();
    let touch_writer = touch_present.then(|| {
        hid::HidWriter::<_, TOUCH_REPORT_LEN>::new(
            &mut builder,
            HID_STATE.init(hid::State::new()),
            hid::Config {
                report_descriptor: &touch::REPORT_DESCRIPTOR,
                request_handler: Some(TOUCH_HID.init(TouchHidState::new(&touch::LAST_REPORT))),
                poll_ms: POLL_MS,
                max_packet_size: 64,
                hid_subclass: hid::HidSubclass::No,
                hid_boot_protocol: hid::HidBootProtocol::None,
            },
        )
    });

    let mut usb = builder.build();
    esp_println::println!("gud: usb ready, switching PHY to OTG");
    watchdog.feed();
    stage(3);

    for band in bands.iter_mut() {
        FREE.try_send(band).ok().unwrap();
    }
    // Everything that touches the panel runs on the second core, so USB
    // reception on this core never waits for decoding or SPI.
    esp_rtos::start_second_core(
        peripherals.CPU_CTRL,
        peripherals.FROM_CPU_INTR1,
        CORE1_STACK.take(),
        move || {
            let mut idle = Delay::new();
            board.run(&JOBS, &FREE, decoded, STATUS_SCRATCH.take(), || idle.delay_us(20));
        },
    );
    stage(4);
    esp_println::println!("gud: core 1 started");

    join(
        join5(
            usb.run(),
            receive_task(bulk_out, backlight, &COMMANDS, &JOBS, &FREE),
            reflash_trigger_task(receiver),
            boot_button_task(boot_button),
            console_task(sender, watchdog, reset_reason, previous_stage),
        ),
        async {
            match touch_writer {
                Some(writer) => touch::touch_task(writer, touch_controller, touch_int).await,
                None => core::future::pending().await,
            }
        },
    )
    .await;
}
