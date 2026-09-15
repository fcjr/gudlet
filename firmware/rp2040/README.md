# gudlet on RP2040

[GUD (Generic USB Display)](https://github.com/notro/gud/wiki) firmware for the
[Waveshare RP2040-Touch-LCD-1.69](https://www.waveshare.com/rp2040-touch-lcd-1.69.htm).
The protocol, band decoding, cross-core job types and panel driver come from the
shared crates (see the [top-level README](../../README.md)); this crate is
the usb-device adapter and the board bring-up.

## How it works

The board enumerates as `1d50:614d` with one vendor-class interface and a
single bulk OUT endpoint, which is what the GUD host drivers match on. Control
requests describe the panel (RGB565, one fixed 240×280 mode, a backlight
brightness property) and announce each framebuffer update; the pixels follow
on the bulk endpoint.

There is no full framebuffer. Core 0 receives raw RGB565 or LZ4 blocks into
three 48 KiB input buffers and queues complete bands. Core 1 decompresses,
byte-swaps and sends pixels over 31.25 MHz SPI using DMA. Two 8 KiB DMA buffers
let copying and the next band's decoding overlap the current transfer.
The CPU and peripheral clocks stay at the default 125 MHz. The panel uses
the original vendor-demo SPI rate; 62.5 MHz needs visual hardware validation.

`max_buffer_size` is 49,152 decoded bytes. Hosts must split larger updates into
bands, including compressed updates whose decoded size exceeds that limit.
A portrait full-screen update needs three bands of 102, 102 and 76 rows.
GUD Display on macOS already uses the advertised limit to split updates.

The shared `gud-pipeline` crate provides the job queues, exact payload tracking,
and bounded LZ4 decode path used by both boards. USB and DMA adapters stay
board-specific. When all input buffers are busy, USB NAKs provide backpressure.
A new `SET_BUFFER` is queued behind the previous payload, including its final
ACKed packet. A payload that stops arriving for two seconds triggers a reset
and USB re-enumeration, as on the ESP32-S3.

The pipeline uses 208 KiB for pixel buffers plus an 8 KiB core 1 stack. The
linker reserves at least 16 KiB for core 0's stack. There is no heap or PSRAM.
The ESP32-S3 has enough memory for whole-frame input buffers and a USB driver
that batches packets. The RP2040 USB adapter still services 64-byte packets,
so equal architecture does not imply equal frame rates. A two-minute animation and window-movement test streamed frames but reproduced
USB control timeouts. Stable FPS parity with the ESP32-S3 has not been established.

## Diagnostics and validation

The CDC console prints cumulative counters once per second at 115200 baud:
`rx`, `bytes`, and `wire` count complete payloads and their decoded/wire sizes.
`done` and `written` count completed panel bands and pixel bytes, only after
DMA and the SPI shift register have drained. `decode_errors` counts malformed
compressed bands. `rejected` and `last` describe failed control requests.
`bl` reports the applied backlight duty, `on` the last panel-enable command
completed by core 1, `spi` the SPI clock in Hz, and `up` seconds since boot.
For an FPS comparison, divide the change in `written` by the frame size and
elapsed time, using the same animation and orientation on both boards.
Do not compare `rx` directly to the ESP32-S3's full-frame update count.

`cargo test` at the repository root exercises the shared decoder and protocol,
and compiles this board's actual USB adapter against a fake bus. It checks
packet boundaries, queued updates, exhausted buffers, reset ownership, payload
overruns and stalled transfers. Both portrait and landscape builds should pass
before flashing. Raw color bars have been confirmed visible on the panel.
Sustained motion testing still reproduces intermittent USB control timeouts;
the pipeline is not yet validated for reliable continuous use.

## Building

Requires a stable Rust toolchain with the `thumbv6m-none-eabi` target and
[`elf2uf2-rs`](https://github.com/JoNil/elf2uf2-rs):

```sh
cargo install elf2uf2-rs
just build rp2040         # or `cargo build --release` in this directory
elf2uf2-rs ../../target/thumbv6m-none-eabi/release/gud-rp2040 gud-rp2040.uf2
```

`rust-toolchain.toml` pulls in the target. To present the panel rotated 90° as
280×240 instead:

```sh
just features=landscape build rp2040
```

## Flashing

The optimized pipeline has been flashed and stress-tested, but USB stability
remains unresolved.

1. Hold the **BOOT** button and tap **RESET** (or hold BOOT while plugging the
   board in). A drive named `RPI-RP2` mounts.
2. Either copy `gud-rp2040.uf2` onto that drive, or run `just flash rp2040`
   (`cargo run --release` here), which does the same via `elf2uf2-rs -d`.

The board reboots into the firmware, shows the gudlet boot logo, turns on
the backlight, and waits for a host.

## Pinout

From Waveshare's `DEV_Config.h`. Only the LCD is used; the touch controller,
IMU, RTC and buzzer are left untouched.

| Signal      | GPIO |
|-------------|------|
| LCD DC      | 8    |
| LCD CS      | 9    |
| LCD SCK     | 10   |
| LCD MOSI    | 11   |
| LCD MISO    | 12   |
| LCD RST     | 13   |
| Backlight   | 25 (PWM slice 4 B) |

The ST7789V2 has 240×320 of RAM; the visible 280 rows start at row 20, which
the driver offsets internally.

## Protocol coverage

| Request | Behaviour |
|---|---|
| `GET_STATUS` | Status of the last request |
| `GET_DESCRIPTOR` | Magic, version 1, no flags, LZ4, 49,152-byte decoded buffer limit, fixed 240×280 |
| `GET_FORMATS` | `RGB565` only |
| `GET_PROPERTIES` | none |
| `GET_CONNECTORS` | one `PANEL` connector, no status polling |
| `GET_CONNECTOR_PROPERTIES` | `BACKLIGHT_BRIGHTNESS` (0–100) |
| `GET_CONNECTOR_STATUS` | always connected |
| `GET_CONNECTOR_MODES` | one preferred 240×280 mode at ~60 Hz |
| `GET_CONNECTOR_EDID` | empty |
| `SET_STATE_CHECK` | validates mode/format/connector, stages brightness |
| `SET_STATE_COMMIT` | applies brightness |
| `SET_CONTROLLER_ENABLE` | accepted |
| `SET_DISPLAY_ENABLE` | panel DISPON/DISPOFF and backlight |
| `SET_BUFFER` | validates a bounded rectangle and queues its raw or LZ4 payload |
| `SET_CONNECTOR_FORCE_DETECT` | accepted |

Anything else stalls with `REQUEST_NOT_SUPPORTED`. Invalid rectangles, oversized
decoded bands and unsupported compression types return `INVALID_PARAMETER`.

## Credits

Modelled on scd31's
[rp2040-rcade-crt-driver](https://gitlab.scd31.com/sophie/rp2040-rcade-crt-driver)
and
[stm32-usb-vga-rcade-adapter](https://gitlab.scd31.com/sophie/stm32-usb-vga-rcade-adapter).
Panel init sequence from Waveshare's demo code.

See the [project acknowledgments](../../README.md#thanks-sophie) for
Sophie's RCade write-up and the work that made gudlet possible.

## License

MIT
