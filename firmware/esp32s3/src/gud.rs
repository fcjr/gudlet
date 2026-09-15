//! embassy-usb side of the GUD device: the vendor interface's control
//! handler and the bulk receiver.
//!
//! [`GudHandler`] answers control requests through [`gud_protocol::Protocol`]
//! and queues whatever touches the panel as a [`Command`].
//!
//! Updates arrive in bands of at most [`BAND_BYTES`] uncompressed, optionally
//! LZ4 block compressed. [`receive_task`] (core 0, async) pulls each band's
//! payload off the bulk endpoint into one of a few band buffers and hands it
//! to the panel worker (core 1, synchronous, see `panel.rs`), which
//! decompresses, byte-swaps and clocks it out over SPI by DMA while the next
//! band is already arriving. There is no framebuffer.

use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use embassy_time::{with_timeout, Duration, Instant};
use embassy_usb::control::{InResponse, OutResponse, Recipient, Request, RequestType};
use embassy_usb::driver::{EndpointError, EndpointOut};
use embassy_usb::types::InterfaceNumber;
use embassy_usb::Handler;
use gud_panel::{FRAME_BYTES, HEIGHT, WIDTH};
use gud_pipeline::PayloadProgress;
use gud_protocol::{status, Display, Protocol};
pub use gud_protocol::{Command, BULK_PACKET_SIZE};

/// Bytes requested per bulk read; the patched driver delivers up to this much
/// per wake-up (see vendor/README.md).
const BULK_READ_SIZE: usize = 8192;
/// Largest band (uncompressed) the host may send per update; advertised as
/// `max_buffer_size`. A whole frame fits, so each frame is one control
/// request plus one bulk transfer, which matters on full-speed USB where a
/// control round trip costs milliseconds.
pub const BAND_BYTES: usize = FRAME_BYTES;
/// What the host is told: fixed RGB565 panel, LZ4 accepted, one frame per update.
pub const DISPLAY: Display = Display::new(WIDTH, HEIGHT).with_lz4(BAND_BYTES as u32).with_rotation();
/// Band buffers (in PSRAM) in flight between USB reception and the panel.
pub const NUM_BANDS: usize = 3;

pub type Band = [u8; BAND_BYTES];

/// How long one payload read may stall before we treat the host as gone.
/// Bulk backpressure (NAK) paces a busy device without any timeout, so a gap
/// this long means the host stopped mid-transfer: the only way to realign the
/// unframed bulk stream is to re-enumerate, which a reset does cleanly.
const PAYLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Backlight control; stays on the USB core.
pub trait Backlight {
    /// 0..=100
    fn set_brightness(&mut self, percent: u8);
}

pub type PanelJob = gud_pipeline::PanelJob<BAND_BYTES>;
pub type JobChannel = gud_pipeline::JobChannel<BAND_BYTES>;
pub type FreeChannel = gud_pipeline::FreeChannel<BAND_BYTES, NUM_BANDS>;
pub use gud_pipeline::CommandChannel;

/// Counters for the debug console.
pub struct Stats {
    pub updates: AtomicU32,
    /// Uncompressed pixel bytes.
    pub bytes: AtomicU32,
    /// Bytes that actually crossed USB.
    pub wire_bytes: AtomicU32,
    pub rejected: AtomicU32,
    pub last_rejected_request: AtomicU8,
    pub abandoned: AtomicU32,
    /// Payload reads that ended early because the endpoint was disabled
    /// (bus reset or configuration change) or a packet overran the buffer.
    pub rx_disabled: AtomicU32,
    pub rx_overflow: AtomicU32,
    pub decode_errors: AtomicU32,
    /// Profiling: microseconds spent in each stage, and USB packets received.
    pub rx_us: AtomicU32,
    pub rx_packets: AtomicU32,
    pub decode_us: AtomicU32,
    pub swap_us: AtomicU32,
    pub spi_us: AtomicU32,
    /// Time the receiver spent waiting for the host's next command.
    pub idle_us: AtomicU32,
    /// Time the receiver spent waiting for a free band buffer.
    pub starve_us: AtomicU32,
    /// Bands the panel worker actually pushed to the glass, and pixel bytes.
    pub bands_written: AtomicU32,
    pub pixels_written: AtomicU32,
    /// Panel state as last commanded: bit0 display enabled, bits 8.. brightness.
    pub panel_state: AtomicU32,
    /// Bring-up counters: every control request seen (ours or not), DTR as
    /// last observed by the console task, bytes received on the console's
    /// OUT endpoint, console writes that completed / timed out.
    pub control_requests: AtomicU32,
    pub dtr: AtomicU32,
    pub console_rx_bytes: AtomicU32,
    pub console_tx_ok: AtomicU32,
    pub console_tx_timeout: AtomicU32,
    /// Touch reports sent, dropped because the host was not polling, and
    /// controller reads that failed.
    pub touch_reports: AtomicU32,
    pub touch_dropped: AtomicU32,
    pub touch_errors: AtomicU32,
}

impl Stats {
    pub const fn new() -> Self {
        Self {
            updates: AtomicU32::new(0),
            bytes: AtomicU32::new(0),
            wire_bytes: AtomicU32::new(0),
            rejected: AtomicU32::new(0),
            last_rejected_request: AtomicU8::new(0),
            abandoned: AtomicU32::new(0),
            rx_disabled: AtomicU32::new(0),
            rx_overflow: AtomicU32::new(0),
            decode_errors: AtomicU32::new(0),
            rx_us: AtomicU32::new(0),
            rx_packets: AtomicU32::new(0),
            decode_us: AtomicU32::new(0),
            swap_us: AtomicU32::new(0),
            spi_us: AtomicU32::new(0),
            idle_us: AtomicU32::new(0),
            starve_us: AtomicU32::new(0),
            bands_written: AtomicU32::new(0),
            pixels_written: AtomicU32::new(0),
            panel_state: AtomicU32::new(1 | (100 << 8)),
            control_requests: AtomicU32::new(0),
            dtr: AtomicU32::new(0),
            console_rx_bytes: AtomicU32::new(0),
            console_tx_ok: AtomicU32::new(0),
            console_tx_timeout: AtomicU32::new(0),
            touch_reports: AtomicU32::new(0),
            touch_dropped: AtomicU32::new(0),
            touch_errors: AtomicU32::new(0),
        }
    }
}
pub static STATS: Stats = Stats::new();

/// Seconds since boot at the console task's last iteration. The control
/// handler, which the host exercises regularly, uses it to notice a stalled
/// executor and reboot with diagnostics instead of hanging forever.
pub static CONSOLE_TICK: AtomicU32 = AtomicU32::new(0);
const STALL_SECONDS: u32 = 10;

fn check_stall() {
    let now = Instant::now().as_secs() as u32;
    let tick = CONSOLE_TICK.load(Ordering::Relaxed);
    if now > 2 * STALL_SECONDS && now.saturating_sub(tick) > STALL_SECONDS {
        crate::stall_detected(tick, now);
    }
}

pub fn account(counter: &AtomicU32, since: Instant) {
    counter.fetch_add(since.elapsed().as_micros() as u32, Ordering::Relaxed);
}

pub struct GudHandler {
    interface: InterfaceNumber,
    commands: &'static CommandChannel,
    protocol: Protocol,
}

impl GudHandler {
    pub fn new(interface: InterfaceNumber, commands: &'static CommandChannel) -> Self {
        Self {
            interface,
            commands,
            protocol: Protocol::new(DISPLAY),
        }
    }

    fn is_ours(&self, req: &Request) -> bool {
        req.request_type == RequestType::Vendor
            && match req.recipient {
                Recipient::Interface => req.index == u8::from(self.interface) as u16,
                Recipient::Device => true,
                _ => false,
            }
    }

    fn rejected(&self, request: u8) {
        STATS.rejected.fetch_add(1, Ordering::Relaxed);
        STATS.last_rejected_request.store(request, Ordering::Relaxed);
    }
}

impl Handler for GudHandler {
    fn reset(&mut self) {
        self.protocol.reset();
        self.commands.clear();
    }

    fn control_in<'a>(&'a mut self, req: Request, buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        STATS.control_requests.fetch_add(1, Ordering::Relaxed);
        if !self.is_ours(&req) {
            return None;
        }
        check_stall();
        match self.protocol.control_in(req.request, req.value, buf) {
            Ok(len) => Some(InResponse::Accepted(&buf[..len])),
            Err(_) => {
                self.rejected(req.request);
                Some(InResponse::Rejected)
            }
        }
    }

    fn control_out(&mut self, req: Request, data: &[u8]) -> Option<OutResponse> {
        STATS.control_requests.fetch_add(1, Ordering::Relaxed);
        if !self.is_ours(&req) {
            return None;
        }

        let commands = self.commands;
        // One control request emits at most two commands. Reserve room before
        // mutating protocol state so enable/backlight cannot be partially queued.
        let room = commands.free_capacity() >= 2;
        let result = self.protocol.control_out(req.request, data, |command| {
            if !room { return Err(status::PROTOCOL_ERROR); }
            commands.try_send(command).map_err(|_| status::PROTOCOL_ERROR)?;
            if let Command::Update(rect) = command {
                STATS.updates.fetch_add(1, Ordering::Relaxed);
                STATS.bytes.fetch_add(rect.length, Ordering::Relaxed);
                STATS.wire_bytes.fetch_add(rect.payload, Ordering::Relaxed);
            }
            Ok(())
        });
        Some(match result {
            Ok(()) => OutResponse::Accepted,
            Err(_) => {
                self.rejected(req.request);
                OutResponse::Rejected
            }
        })
    }
}

/// Read exactly `dst.len()` bytes of one update's payload off the bulk
/// endpoint. The host commits to sending this many bytes right after
/// SET_BUFFER, and USB flow control holds them until we are ready, so we
/// always read the whole payload rather than guessing when the host has
/// "given up": abandoning a half-read payload used to leave its tail buffered
/// in the OTG, where the next read consumed it and desynced the stream for
/// good. Returns false if the bus went away (a reset re-arms the endpoint);
/// a genuine host stall reboots, the one safe way to realign an unframed
/// stream.
async fn read_payload<E: EndpointOut>(endpoint: &mut E, dst: &mut [u8]) -> bool {
    let mut packet = [0u8; BULK_PACKET_SIZE as usize];
    let mut progress = PayloadProgress::new(dst.len());
    let started = Instant::now();
    while !progress.complete() {
        let filled = progress.filled();
        let space = progress.remaining();
        // Read straight into place, as many whole packets as fit; the tail of
        // the payload goes through a bounce buffer since reads need packet room.
        let direct = space >= BULK_PACKET_SIZE as usize;
        let read = if direct {
            let want = space.min(BULK_READ_SIZE) / BULK_PACKET_SIZE as usize * BULK_PACKET_SIZE as usize;
            endpoint.read(&mut dst[filled..filled + want])
        } else {
            endpoint.read(&mut packet)
        };
        let received = match with_timeout(PAYLOAD_STALL_TIMEOUT, read).await {
            Ok(result) => result,
            Err(_) => {
                // Host stopped mid-payload. Any bytes it did send are stuck in
                // the OTG with no safe way to count them, so reboot to realign.
                STATS.abandoned.fetch_add(1, Ordering::Relaxed);
                crate::payload_stall();
            }
        };
        let n = match received {
            Ok(n) => n,
            Err(EndpointError::Disabled) => {
                STATS.rx_disabled.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            Err(EndpointError::BufferOverflow) => {
                STATS.rx_overflow.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        STATS.rx_packets.fetch_add(1, Ordering::Relaxed);
        if progress.advance(n).is_err() {
            STATS.rx_overflow.fetch_add(1, Ordering::Relaxed);
            crate::payload_stall();
        }
        if !direct {
            dst[filled..filled + n].copy_from_slice(&packet[..n]);
        }
    }
    account(&STATS.rx_us, started);
    true
}

/// Owns the bulk endpoint: turns queued commands into panel jobs, pulling
/// each update's payload into a free band buffer.
pub async fn receive_task<E: EndpointOut, B: Backlight>(
    mut endpoint: E,
    mut backlight: B,
    commands: &'static CommandChannel,
    jobs: &'static JobChannel,
    free: &'static FreeChannel,
) -> ! {
    loop {
        let waiting = Instant::now();
        let command = commands.receive().await;
        account(&STATS.idle_us, waiting);
        match command {
            Command::Update(rect) => {
                let waiting = Instant::now();
                let payload = free.receive().await;
                account(&STATS.starve_us, waiting);
                if read_payload(&mut endpoint, &mut payload[..rect.payload as usize]).await {
                    jobs.send(PanelJob::Band { payload, rect }).await;
                } else {
                    free.send(payload).await;
                }
            }
            Command::Brightness(percent) => {
                STATS.panel_state.fetch_and(0xff, Ordering::Relaxed);
                STATS.panel_state.fetch_or((percent as u32) << 8, Ordering::Relaxed);
                backlight.set_brightness(percent)
            }
            Command::Rotation(rotation) => jobs.send(PanelJob::Rotate(rotation)).await,
            Command::Enable(on) => {
                if on {
                    STATS.panel_state.fetch_or(1, Ordering::Relaxed);
                } else {
                    STATS.panel_state.fetch_and(!1, Ordering::Relaxed);
                }
                jobs.send(PanelJob::Enable(on)).await
            }
        }
    }
}
