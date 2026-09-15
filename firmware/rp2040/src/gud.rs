//! Polling USB adapter. Core 0 only receives bytes and queues work; it never
//! waits for SPI. The endpoint NAKs while all input buffers belong to core 1.
use embedded_hal::pwm::SetDutyCycle;
use gud_panel::{HEIGHT, WIDTH};
use gud_pipeline::{CommandChannel, PayloadProgress};
use gud_protocol::{status, Command, Display, Protocol, Rect, BULK_PACKET_SIZE, MAX_IN_LEN};
use usb_device::class_prelude::*;
use usb_device::control::{Recipient, Request, RequestType};

pub const BAND_BYTES: usize = 48 * 1024;
pub const NUM_BANDS: usize = 3;
pub const DISPLAY: Display = Display::new(WIDTH, HEIGHT).with_lz4(BAND_BYTES as u32).with_rotation();
pub type Band = [u8; BAND_BYTES];
pub type PanelJob = gud_pipeline::PanelJob<BAND_BYTES>;
pub type JobChannel = gud_pipeline::JobChannel<BAND_BYTES>;
pub type FreeChannel = gud_pipeline::FreeChannel<BAND_BYTES, NUM_BANDS>;

#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub updates: u32,
    pub bytes: u32,
    pub wire_bytes: u32,
    pub rejected: u32,
    pub last_rejected_request: u8,
    pub brightness: u8,
}

struct Receiving {
    payload: &'static mut Band,
    rect: Rect,
    progress: PayloadProgress,
    last_progress: u64,
}

pub struct GudDevice<'a, B: UsbBus, BL> {
    interface: InterfaceNumber,
    bulk_out: EndpointOut<'a, B>,
    backlight: BL,
    protocol: Protocol,
    commands: CommandChannel,
    jobs: &'static JobChannel,
    free: &'static FreeChannel,
    pending_command: Option<Command>,
    pending_job: Option<PanelJob>,
    receiving: Option<Receiving>,
    now: u64,
    stats: Stats,
}

impl<'a, B: UsbBus, BL: SetDutyCycle> GudDevice<'a, B, BL> {
    pub fn new(
        alloc: &'a UsbBusAllocator<B>,
        backlight: BL,
        jobs: &'static JobChannel,
        free: &'static FreeChannel,
    ) -> Self {
        Self {
            interface: alloc.interface(),
            bulk_out: alloc.bulk(BULK_PACKET_SIZE),
            backlight,
            protocol: Protocol::new(DISPLAY),
            commands: CommandChannel::new(),
            jobs,
            free,
            pending_command: None,
            pending_job: None,
            receiving: None,
            now: 0,
            stats: Stats {
                brightness: 100,
                ..Stats::default()
            },
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn service(&mut self, now_us: u64) {
        self.now = now_us;
        self.drain_bulk();
        if self
            .receiving
            .as_ref()
            .is_some_and(|r| now_us - r.last_progress >= 2_000_000)
        {
            // The stream has no framing to recover a partially sent update.
            // Re-enumeration is also how the ESP32-S3 recovers a host stall.
            crate::payload_stall();
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

    fn rejected(&mut self, request: u8) {
        self.stats.rejected = self.stats.rejected.wrapping_add(1);
        self.stats.last_rejected_request = request;
    }

    fn drain_bulk(&mut self) {
        // Bound each batch so CDC, EP0 and the stall timer remain responsive.
        for _ in 0..64 {
            if let Some(job) = self.pending_job.take() {
                if let Err(gud_pipeline::TrySendError::Full(job)) = self.jobs.try_send(job) {
                    self.pending_job = Some(job);
                    return;
                }
                crate::wake_panel();
            }
            if let Some(rx) = self.receiving.as_mut() {
                let filled = rx.progress.filled();
                let space = rx.progress.remaining();
                let mut tail = [0u8; BULK_PACKET_SIZE as usize];
                let direct = space >= tail.len();
                let result = if direct {
                    self.bulk_out
                        .read(&mut rx.payload[filled..filled + tail.len()])
                } else {
                    self.bulk_out.read(&mut tail)
                };
                let n = match result {
                    Ok(n) => n,
                    Err(UsbError::WouldBlock) => return,
                    // An overlong packet cannot safely be attributed to another band.
                    Err(_) => crate::payload_stall(),
                };
                if rx.progress.advance(n).is_err() {
                    crate::payload_stall();
                }
                if !direct {
                    rx.payload[filled..filled + n].copy_from_slice(&tail[..n]);
                }
                if n > 0 {
                    rx.last_progress = self.now;
                }
                if rx.progress.complete() {
                    let rx = self.receiving.take().unwrap();
                    self.stats.updates = self.stats.updates.wrapping_add(1);
                    self.stats.bytes = self.stats.bytes.wrapping_add(rx.rect.length);
                    self.stats.wire_bytes = self.stats.wire_bytes.wrapping_add(rx.rect.payload);
                    self.pending_job = Some(PanelJob::Band {
                        payload: rx.payload,
                        rect: rx.rect,
                    });
                }
                continue;
            }
            let Some(command) = self
                .pending_command
                .take()
                .or_else(|| self.commands.try_receive().ok())
            else {
                return;
            };
            match command {
                Command::Update(rect) => match self.free.try_receive() {
                    Ok(payload) => {
                        self.receiving = Some(Receiving {
                            payload,
                            rect,
                            progress: PayloadProgress::new(rect.payload as usize),
                            last_progress: self.now,
                        })
                    }
                    Err(_) => {
                        self.pending_command = Some(command);
                        return;
                    }
                },
                Command::Enable(on) => self.pending_job = Some(PanelJob::Enable(on)),
                Command::Rotation(rotation) => self.pending_job = Some(PanelJob::Rotate(rotation)),
                Command::Brightness(percent) => {
                    if self.backlight.set_duty_cycle_percent(percent).is_ok() {
                        self.stats.brightness = percent;
                    }
                }
            }
        }
    }
}

impl<'a, B: UsbBus, BL: SetDutyCycle> UsbClass<B> for GudDevice<'a, B, BL> {
    fn get_configuration_descriptors(
        &self,
        writer: &mut DescriptorWriter,
    ) -> usb_device::Result<()> {
        writer.iad(self.interface, 1, 0xff, 0xff, 0, None)?;
        writer.interface(self.interface, 0xff, 0xff, 0)?;
        writer.endpoint(&self.bulk_out)?;
        Ok(())
    }

    fn reset(&mut self) {
        if let Some(rx) = self.receiving.take() {
            assert!(self.free.try_send(rx.payload).is_ok());
        }
        if let Some(PanelJob::Band { payload, .. }) = self.pending_job.take() {
            assert!(self.free.try_send(payload).is_ok());
        }
        // Already queued complete bands may finish before the next session's
        // work. Their buffers still belong exclusively to the panel worker.
        self.pending_command = None;
        self.commands.clear();
        self.protocol.reset();
    }

    fn poll(&mut self) {
        self.drain_bulk();
    }

    fn control_in(&mut self, xfer: ControlIn<B>) {
        let request = *xfer.request();
        if !self.is_ours(&request) {
            return;
        }
        let mut buf = [0u8; MAX_IN_LEN];
        match self
            .protocol
            .control_in(request.request, request.value, &mut buf)
        {
            Ok(len) => {
                let _ = xfer.accept_with(&buf[..len]);
            }
            Err(_) => {
                self.rejected(request.request);
                let _ = xfer.reject();
            }
        }
    }

    fn control_out(&mut self, xfer: ControlOut<B>) {
        let request = *xfer.request();
        if !self.is_ours(&request) {
            return;
        }
        // EP0 can be serviced before the previous update's final ACKed bulk
        // packet. Keep its receiver intact and queue the new command in order.
        self.drain_bulk();
        // A request emits at most two commands; reserve room for both before
        // protocol state changes. Core 0 is the only command producer.
        let room = self.commands.free_capacity() >= 2;
        let commands = &self.commands;
        let result = self
            .protocol
            .control_out(request.request, xfer.data(), |command| {
                if !room {
                    return Err(status::PROTOCOL_ERROR);
                }
                commands
                    .try_send(command)
                    .map_err(|_| status::PROTOCOL_ERROR)
            });
        match result {
            Ok(()) => {
                let _ = xfer.accept();
            }
            Err(_) => {
                self.rejected(request.request);
                let _ = xfer.reject();
            }
        }
    }
}

#[cfg(test)]
#[path = "gud_tests.rs"]
mod tests;
