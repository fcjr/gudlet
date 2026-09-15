//! Core 1 owns the LCD and SPI. Two bounce buffers let the next copy/decode
//! overlap DMA without retaining the USB input buffer until SPI goes idle.
use embedded_hal::{digital::OutputPin, spi::SpiBus};
use gud_panel::St7789;
use gud_protocol::stream::swap_pairs;
use portable_atomic::{AtomicU32, Ordering};
use rp2040_hal::dma::{single_buffer, Channel, ReadTarget, WriteTarget, CH0};

use crate::gud::{Band, FreeChannel, JobChannel, PanelJob};

pub const DMA_CHUNK: usize = 8192;
pub static ENABLED: AtomicU32 = AtomicU32::new(1);
pub static COMPLETED: AtomicU32 = AtomicU32::new(0);
pub static PIXEL_BYTES: AtomicU32 = AtomicU32::new(0);
pub static DECODE_ERRORS: AtomicU32 = AtomicU32::new(0);

struct Buffer {
    bytes: &'static mut [u8; DMA_CHUNK],
    len: usize,
}

// SAFETY: the exclusive static allocation has a stable address even when this
// wrapper moves. Only start_chunk sets len, bounded by bytes.len(); Transfer
// owns the wrapper until DMA finishes, so nobody can mutate/reuse its bytes.
unsafe impl ReadTarget for Buffer {
    type ReceivedWord = u8;
    fn rx_treq() -> Option<u8> {
        None
    }
    fn rx_address_count(&self) -> (u32, u32) {
        (self.bytes.as_ptr() as u32, self.len as u32)
    }
    fn rx_increment(&self) -> bool {
        true
    }
}

type Transfer<SPI> = single_buffer::Transfer<Channel<CH0>, Buffer, SPI>;

pub struct PanelWorker<SPI: WriteTarget, DC, CS, RST> {
    lcd: St7789<DC, CS, RST>,
    spi: Option<SPI>,
    channel: Option<Channel<CH0>>,
    in_flight: Option<Transfer<SPI>>,
    idle: [Option<Buffer>; 2],
    pending_bytes: Option<u32>,
}

impl<SPI, DC, CS, RST> PanelWorker<SPI, DC, CS, RST>
where
    SPI: SpiBus<u8> + WriteTarget<TransmittedWord = u8>,
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
{
    pub fn new(
        lcd: St7789<DC, CS, RST>,
        spi: SPI,
        channel: Channel<CH0>,
        buffers: [&'static mut [u8; DMA_CHUNK]; 2],
    ) -> Self {
        let [a, b] = buffers;
        Self {
            lcd,
            spi: Some(spi),
            channel: Some(channel),
            in_flight: None,
            idle: [
                Some(Buffer { bytes: a, len: 0 }),
                Some(Buffer { bytes: b, len: 0 }),
            ],
            pending_bytes: None,
        }
    }

    fn wait(&mut self) {
        if let Some(transfer) = self.in_flight.take() {
            let (channel, buffer, spi) = transfer.wait();
            self.spi = Some(spi);
            self.channel = Some(channel);
            *self.idle.iter_mut().find(|s| s.is_none()).unwrap() = Some(buffer);
        }
    }

    fn close_rect(&mut self) {
        if let Some(bytes) = self.pending_bytes.take() {
            self.wait();
            // DMA completion only means the last byte reached the TX FIFO.
            let spi = self.spi.as_mut().unwrap();
            spi.flush().unwrap();
            self.lcd.end_rect();
            PIXEL_BYTES.fetch_add(bytes, Ordering::Relaxed);
            COMPLETED.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn start_chunk(&mut self, chunk: &[u8]) {
        let mut buffer = self.idle.iter_mut().find_map(Option::take).unwrap();
        buffer.bytes[..chunk.len()].copy_from_slice(chunk);
        buffer.len = chunk.len();
        self.wait();
        self.in_flight = Some(
            single_buffer::Config::new(
                self.channel.take().unwrap(),
                buffer,
                self.spi.take().unwrap(),
            )
            .start(),
        );
    }

    pub fn run(
        mut self,
        jobs: &'static JobChannel,
        free: &'static FreeChannel,
        decoded: &'static mut Band,
    ) -> ! {
        loop {
            match jobs.try_receive() {
                Ok(PanelJob::Band { payload, rect }) => {
                    let pixels = match gud_pipeline::decode_pixels(&rect, payload, decoded) {
                        Ok(pixels) => pixels,
                        Err(_) => {
                            DECODE_ERRORS.fetch_add(1, Ordering::Relaxed);
                            assert!(free.try_send(payload).is_ok());
                            continue;
                        }
                    };
                    swap_pairs(pixels);
                    self.close_rect();
                    self.lcd.begin_rect(
                        self.spi.as_mut().unwrap(),
                        rect.x,
                        rect.y,
                        rect.width,
                        rect.height,
                    );
                    self.pending_bytes = Some(rect.length);
                    for chunk in pixels.chunks(DMA_CHUNK) {
                        self.start_chunk(chunk);
                    }
                    // All input pixels have been copied; the last DMA transfer
                    // owns a separate bounce buffer and may still be running.
                    assert!(free.try_send(payload).is_ok());
                }
                Ok(PanelJob::Enable(on)) => {
                    self.close_rect();
                    self.lcd.set_display_on(self.spi.as_mut().unwrap(), on);
                    ENABLED.store(on as u32, Ordering::Relaxed);
                }
                Ok(PanelJob::Rotate(rotation)) => {
                    self.close_rect();
                    self.lcd.set_rotation(self.spi.as_mut().unwrap(), rotation);
                }
                Err(_) => {
                    if self.in_flight.as_ref().is_some_and(|t| t.is_done()) {
                        self.close_rect();
                    }
                    if self.in_flight.is_none() {
                        // SEV after each enqueue makes this race-safe: an event
                        // arriving before WFE remains latched. Avoid hammering
                        // the shared hardware spinlock while USB is receiving.
                        cortex_m::asm::wfe();
                    } else {
                        core::hint::spin_loop();
                    }
                }
            }
        }
    }
}
