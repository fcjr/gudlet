# gudlet on RP2040

[GUD (Generic USB Display)](https://github.com/notro/gud/wiki) firmware for the
[Waveshare RP2040-Touch-LCD-1.69](https://www.waveshare.com/rp2040-touch-lcd-1.69.htm).
The protocol handling, packet byte-swapping and panel driver come from the
shared crates (see the [top-level README](../../README.md)); this crate is
the usb-device adapter and the board bring-up.

## How it works

The board enumerates as `1d50:614d` with one vendor-class interface and a
single bulk OUT endpoint, which is what the GUD host drivers match on. Control
requests describe the panel (RGB565, one fixed 240×280 mode, a backlight
brightness property) and announce each framebuffer update; the pixels follow
on the bulk endpoint.

There is no framebuffer on the device. `SET_BUFFER` opens an ST7789 RAM write
window matching the update rectangle, and each 64-byte bulk packet is
byte-swapped and pushed out over SPI as it arrives. The panel's auto-increment
inside the window lines up exactly with the tightly packed rectangle the host
sends, so the device can never fall behind the host or run out of memory.

The RP2040 is a full-speed USB device (12 Mbit/s), so a full-screen update
(134 KB) takes roughly 130 ms. Expect around 7 fps for full-screen motion and
much better for small damage rectangles.

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

This crate has not been run on hardware yet; the ESP32-S3 crate has.

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
| `GET_DESCRIPTOR` | Magic, version 1, no flags, no compression, unlimited buffer, fixed 240×280 |
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
| `SET_BUFFER` | validates the rectangle and opens the panel window |
| `SET_CONNECTOR_FORCE_DETECT` | accepted |

Anything else stalls with `REQUEST_NOT_SUPPORTED`. Compressed transfers are
rejected with `INVALID_PARAMETER` (compression is not advertised).

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
