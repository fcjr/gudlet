//! usb-device side of the GUD device: one vendor-class interface with a
//! bulk OUT endpoint, driven from the main loop's `poll`.
//!
//! Updates are streamed straight into the panel: `SET_BUFFER` opens a RAM
//! write window matching the damage rectangle, and every bulk packet is
//! byte-swapped and pushed out over SPI as it arrives. Nothing is buffered,
//! so the device never falls behind the host and needs no framebuffer RAM.

use gud_panel::{HEIGHT, WIDTH};
use gud_protocol::stream::PixelStream;
use gud_protocol::{req, Command, Display, Protocol, BULK_PACKET_SIZE, MAX_IN_LEN};
use usb_device::class_prelude::*;
use usb_device::control::RequestType;

/// What the host is told: fixed RGB565 panel, raw updates of any size.
pub const DISPLAY: Display = Display::new(WIDTH, HEIGHT);

/// Where the pixels go.
pub trait Panel {
    fn begin_rect(&mut self, x: u16, y: u16, width: u16, height: u16);
    /// Big-endian RGB565, high byte first.
    fn write_pixels(&mut self, data: &[u8]);
    fn end_rect(&mut self);
    fn set_enabled(&mut self, on: bool);
    /// 0..=100
    fn set_brightness(&mut self, percent: u8);
}

/// Counters reported over the debug serial port.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub updates: u32,
    pub bytes: u32,
    pub rejected: u32,
    pub last_rejected_request: u8,
}

pub struct GudDevice<'a, B: UsbBus, P: Panel> {
    interface: InterfaceNumber,
    bulk_out: EndpointOut<'a, B>,
    panel: P,
    protocol: Protocol,
    stream: PixelStream,
    stats: Stats,
}

impl<'a, B: UsbBus, P: Panel> GudDevice<'a, B, P> {
    pub fn new(alloc: &'a UsbBusAllocator<B>, mut panel: P) -> Self {
        panel.set_brightness(100);
        Self {
            interface: alloc.interface(),
            bulk_out: alloc.bulk(BULK_PACKET_SIZE),
            panel,
            protocol: Protocol::new(DISPLAY),
            stream: PixelStream::new(),
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    fn rejected(&mut self, request: u8) {
        self.stats.rejected = self.stats.rejected.wrapping_add(1);
        self.stats.last_rejected_request = request;
    }

    /// Feed every packet waiting on the bulk endpoint into the open rectangle.
    fn drain_bulk(&mut self) {
        let mut buf = [0u8; BULK_PACKET_SIZE as usize];
        let mut out = [0u8; BULK_PACKET_SIZE as usize + 1];
        while let Ok(len) = self.bulk_out.read(&mut buf) {
            let (n, done) = self.stream.feed(&buf[..len], &mut out);
            if n > 0 {
                self.panel.write_pixels(&out[..n]);
            }
            if done {
                self.panel.end_rect();
            }
        }
    }

    fn abort_transfer(&mut self) {
        if self.stream.abort() {
            self.panel.end_rect();
        }
    }
}

impl<'a, B: UsbBus, P: Panel> UsbClass<B> for GudDevice<'a, B, P> {
    fn get_configuration_descriptors(&self, writer: &mut DescriptorWriter) -> usb_device::Result<()> {
        // Vendor-specific class/subclass is how the Linux driver matches us.
        writer.iad(self.interface, 1, 0xff, 0xff, 0, None)?;
        writer.interface(self.interface, 0xff, 0xff, 0)?;
        writer.endpoint(&self.bulk_out)?;
        Ok(())
    }

    fn reset(&mut self) {
        self.abort_transfer();
        self.protocol.reset();
    }

    fn poll(&mut self) {
        self.drain_bulk();
    }

    fn control_in(&mut self, xfer: ControlIn<B>) {
        let request = *xfer.request();
        if request.request_type != RequestType::Vendor {
            return;
        }
        let mut buf = [0u8; MAX_IN_LEN];
        match self.protocol.control_in(request.request, request.value, &mut buf) {
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
        if request.request_type != RequestType::Vendor {
            return;
        }
        // The controller ACKs a bulk packet as soon as it lands in the
        // endpoint buffer, and EP0 is serviced before class polling, so the
        // last packet of the previous update can still be unread when the
        // next SET_BUFFER arrives. Attribute it to the right rectangle.
        self.drain_bulk();
        // A new update while one is still in flight means the host gave up
        // on the previous bulk transfer; drop what was pending and resync.
        if request.request == req::SET_BUFFER {
            self.abort_transfer();
        }

        let Self { protocol, panel, stream, stats, .. } = self;
        let result = protocol.control_out(request.request, xfer.data(), |command| {
            match command {
                Command::Update(rect) => {
                    panel.begin_rect(rect.x, rect.y, rect.width, rect.height);
                    stream.begin(rect.length);
                    stats.updates = stats.updates.wrapping_add(1);
                    stats.bytes = stats.bytes.wrapping_add(rect.length);
                }
                Command::Brightness(percent) => panel.set_brightness(percent),
                Command::Enable(on) => panel.set_enabled(on),
            }
            Ok(())
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
