//! Panel worker: runs synchronously on the second core. Takes bands from the
//! receiver, decompresses and byte-swaps them, and pushes them to the panel
//! by DMA in chunks through two DMA buffers, so decoding the next band
//! overlaps the SPI transfer of the current one.

use core::sync::atomic::Ordering;

use embassy_time::Instant;
use embedded_hal::digital::OutputPin;
use esp_hal::dma::DmaTxBuf;
use esp_hal::spi::master::{SpiDma, SpiDmaTransfer};
use esp_hal::Blocking;

use gud_panel::{strip, St7789, WIDTH};
use gud_protocol::stream::swap_pairs;

use crate::gud::{account, Band, FreeChannel, JobChannel, PanelJob, CONSOLE_TICK, STATS};

type Spi = SpiDma<'static, Blocking>;
type Transfer = SpiDmaTransfer<'static, Blocking, DmaTxBuf>;

/// Largest single SPI DMA transfer the peripheral accepts.
pub const DMA_CHUNK: usize = 32736;

pub struct PanelWorker<DC, CS, RST> {
    lcd: St7789<DC, CS, RST>,
    spi: Option<Spi>,
    in_flight: Option<Transfer>,
    idle_buffers: [Option<DmaTxBuf>; 2],
    rect_open: bool,
}

impl<DC, CS, RST> PanelWorker<DC, CS, RST>
where
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
{
    pub fn new(lcd: St7789<DC, CS, RST>, spi: Spi, buffers: [DmaTxBuf; 2]) -> Self {
        let [a, b] = buffers;
        Self {
            lcd,
            spi: Some(spi),
            in_flight: None,
            idle_buffers: [Some(a), Some(b)],
            rect_open: false,
        }
    }

    fn take_idle(&mut self) -> Option<DmaTxBuf> {
        self.idle_buffers.iter_mut().find_map(|slot| slot.take())
    }

    fn put_idle(&mut self, buf: DmaTxBuf) {
        let slot = self.idle_buffers.iter_mut().find(|slot| slot.is_none()).unwrap();
        *slot = Some(buf);
    }

    /// Block until the transfer in flight (if any) has finished.
    fn wait_in_flight(&mut self) {
        if let Some(transfer) = self.in_flight.take() {
            let (spi, buf) = transfer.wait();
            self.spi = Some(spi);
            self.put_idle(buf);
        }
    }

    /// Raise CS once nothing is clocking out any more.
    fn close_rect(&mut self) {
        if self.rect_open {
            self.wait_in_flight();
            self.lcd.end_rect();
            self.rect_open = false;
        }
    }

    /// Called while idle: finish bookkeeping for a completed transfer.
    fn tidy(&mut self) {
        if self.in_flight.as_ref().is_some_and(|t| t.is_done()) {
            self.close_rect();
        }
    }

    fn start_chunk(&mut self, chunk: &[u8]) {
        // Fill the spare buffer while the previous chunk is still clocking
        // out, then wait for it: the transfer owns the SPI, so only one can
        // be in flight.
        let mut buf = match self.take_idle() {
            Some(buf) => buf,
            None => {
                self.wait_in_flight();
                self.take_idle().unwrap()
            }
        };
        buf.set_length(chunk.len());
        buf.as_mut_slice()[..chunk.len()].copy_from_slice(chunk);
        self.wait_in_flight();
        let spi = self.spi.take().unwrap();
        match spi.write_buffer(chunk.len(), buf) {
            Ok(transfer) => self.in_flight = Some(transfer),
            Err((_, spi, buf)) => {
                self.spi = Some(spi);
                self.put_idle(buf);
            }
        }
    }

    /// Draw the shared boot logo through the same DMA path as display frames.
    pub fn show_boot_logo(&mut self, scratch: &mut [u8]) {
        let width = WIDTH as usize;
        let height = gud_panel::HEIGHT as usize;
        let rows = scratch.len() / (width * 2);
        assert!(rows > 0, "boot logo needs at least one row of scratch space");
        let mut y = 0;
        while y < height {
            let band_rows = rows.min(height - y);
            let band = &mut scratch[..band_rows * width * 2];
            gud_panel::splash::render_rows(y as u16, band);
            self.write_band(0, y as u16, width as u16, band_rows as u16, band);
            y += band_rows;
        }
        self.close_rect();
    }

    /// Big-endian RGB565 pixels for a rectangle. Returns with the last chunk
    /// still in flight so the caller can go and prepare the next band.
    fn write_band(&mut self, x: u16, y: u16, width: u16, height: u16, pixels: &[u8]) {
        // The previous band must be fully out before CS/DC change.
        self.close_rect();
        let spi = self.spi.as_mut().unwrap();
        self.lcd.begin_rect(spi, x, y, width, height);
        self.rect_open = true;
        for chunk in pixels.chunks(DMA_CHUNK) {
            self.start_chunk(chunk);
        }
    }

    /// Show a message (a panic, typically) across the top of the panel.
    pub fn show_message(&mut self, message: &[u8], scratch: &mut [u8]) {
        let mut text = [b' '; 128];
        let n = strip::wrap(message, &mut text);
        let len = WIDTH as usize * strip::STRIP_H * 2;
        strip::render(&text[..n], &mut scratch[..len]);
        self.write_band(0, 0, WIDTH, strip::STRIP_H as u16, &scratch[..len]);
        self.close_rect();
    }

    /// Draw the bring-up status strip across the top of the panel.
    fn draw_status(&mut self, scratch: &mut [u8]) {
        // Three fields of "L:xxxxxx " per row: 27 of the 30 columns.
        let mut text = [b' '; 128];
        let now = Instant::now().as_secs() as u32;
        let fields: [(&[u8], u32); 9] = [
            (b"T:", now),
            (b"K:", CONSOLE_TICK.load(Ordering::Relaxed)),
            (b"R:", STATS.control_requests.load(Ordering::Relaxed)),
            (b"D:", STATS.dtr.load(Ordering::Relaxed)),
            (b"M:", STATS.console_rx_bytes.load(Ordering::Relaxed)),
            (b"W:", STATS.console_tx_ok.load(Ordering::Relaxed)),
            (b"E:", STATS.console_tx_timeout.load(Ordering::Relaxed)),
            (b"U:", STATS.updates.load(Ordering::Relaxed)),
            (b"B:", STATS.bands_written.load(Ordering::Relaxed)),
        ];
        let mut pos = 0;
        for (i, (label, value)) in fields.iter().enumerate() {
            if i == 3 || i == 6 {
                text[pos] = b'\n';
                pos += 1;
            }
            text[pos..pos + 2].copy_from_slice(label);
            strip::hex(*value, &mut text[pos + 2..pos + 8]);
            text[pos + 8] = b' ';
            pos += 9;
        }
        let len = WIDTH as usize * strip::STRIP_H * 2;
        strip::render(&text[..pos], &mut scratch[..len]);
        self.write_band(0, 0, WIDTH, strip::STRIP_H as u16, &scratch[..len]);
        self.close_rect();
    }

    pub fn run(
        mut self,
        jobs: &'static JobChannel,
        free: &'static FreeChannel,
        decoded: &'static mut Band,
        status_scratch: &'static mut [u8],
        mut idle: impl FnMut(),
    ) -> ! {
        let mut last_status = Instant::now();
        loop {
            let job = loop {
                match jobs.try_receive() {
                    Ok(job) => break job,
                    Err(_) => {
                        self.tidy();
                        if cfg!(feature = "debug-strip") && last_status.elapsed().as_millis() >= 1000 {
                            last_status = Instant::now();
                            self.draw_status(status_scratch);
                        }
                        idle();
                    }
                }
            };
            match job {
                PanelJob::Band { payload, rect } => {
                    let length = rect.length as usize;
                    let pixels: &mut [u8] = if rect.compressed {
                        let started = Instant::now();
                        let result = lz4_flex::block::decompress_into(
                            &payload[..rect.payload as usize],
                            &mut decoded[..length],
                        );
                        account(&STATS.decode_us, started);
                        match result {
                            Ok(n) if n == length => &mut decoded[..length],
                            _ => {
                                STATS.decode_errors.fetch_add(1, Ordering::Relaxed);
                                let _ = free.try_send(payload);
                                continue;
                            }
                        }
                    } else {
                        &mut payload[..length]
                    };

                    // Host sends little-endian RGB565; the panel wants the
                    // high byte first.
                    let started = Instant::now();
                    swap_pairs(pixels);
                    account(&STATS.swap_us, started);

                    let started = Instant::now();
                    self.write_band(rect.x, rect.y, rect.width, rect.height, pixels);
                    account(&STATS.spi_us, started);
                    STATS.bands_written.fetch_add(1, Ordering::Relaxed);
                    STATS.pixels_written.fetch_add(pixels.len() as u32, Ordering::Relaxed);

                    // Capacity equals the number of buffers, so this never fails.
                    let _ = free.try_send(payload);
                }
                PanelJob::Enable(on) => {
                    self.close_rect();
                    let spi = self.spi.as_mut().unwrap();
                    self.lcd.set_display_on(spi, on);
                }
            }
        }
    }
}
