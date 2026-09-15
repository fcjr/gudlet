# gudlet on ESP32-S3

GUD firmware for the Waveshare ESP32-S3-Touch-LCD-1.69, built on esp-hal and
embassy-usb. The protocol handling and the panel driver come from the shared
crates (see the [top-level README](../../README.md)); this crate is the USB
adapter, the two-core pipeline and the board bring-up.

## How it performs

The ESP32-S3 is a full-speed USB device, so everything is built around
squeezing a 12 Mbit/s link:

- **LZ4.** The device advertises LZ4 block compression and a whole frame as
  `max_buffer_size`, so each frame is one control request plus one bulk
  transfer of compressed pixels. A window being dragged over a downscaled
  desktop compresses about 3.5:1; a static desktop 10:1 or better.
- **Multi-packet USB transfers.** Upstream embassy-usb-synopsys-otg wakes the
  reading task once per 64-byte packet, which costs about 110 µs each and caps
  the link near 550 KB/s. The vendored copy takes up to 128 packets per
  transfer (see [vendor/README.md](../../vendor/README.md)); the link now runs at 850 to 900 KB/s,
  close to what full-speed bulk allows.
- **Two cores.** Core 0 runs the USB stack and receiver; core 1 decompresses,
  byte-swaps and pushes pixels to the panel by SPI DMA at 40 MHz, overlapping
  the next frame's decode with the current frame's transfer. The DMA handle
  carries panel commands too, which needs its small scratch buffers: without
  them a one-byte command write fails and the panel never initialises.
- **PSRAM.** Compressed payload buffers live in the 8 MB PSRAM; only the
  decoded frame and the DMA chunk buffers use internal RAM.

Measured with GUD Display on macOS: a full-screen scroll of random hex
(worst case, 3:1) runs at 17 to 18 fps; dragging a window at 22 to 23 fps.
Those measurements predate damage tracking in GUD Display. A later
95-second animated-screen test sustained about 30 full-panel updates per
second with no receive or decode errors. These are different workloads,
not a controlled before/after benchmark. The host completes each payload
before sending the next SET_BUFFER, matching the Linux GUD driver's order.

## Building

Requires the Xtensa Rust toolchain from [espup](https://github.com/esp-rs/espup),
[espflash](https://github.com/esp-rs/espflash), and [uv](https://github.com/astral-sh/uv)
for the post-flash reset:

```sh
espup install
cargo install espflash
just build esp32s3        # or `cargo build --release` in this directory
```

`rust-toolchain.toml` selects the `esp` toolchain automatically. To present the
panel rotated 90° as 280×240 instead:

```sh
just features=landscape build esp32s3
```

## Flashing

`just flash esp32s3` (or `cargo run --release` here) runs `scripts/flash.sh`,
which handles the normal update cycle:

- **Firmware already running:** it writes `gud-reflash` to the board's serial
  console, which makes the firmware reboot into the ROM download mode. No
  buttons needed.
- **Command did not work:** while the firmware is running, hold **BOOT by
  itself for one second**. Release it, then run the flash command again.
- **First flash, or an unresponsive board:** unplug it, hold **BOOT**, plug
  it back in, then release BOOT. If a battery is connected, disconnect that
  too for a full power cycle. The download port identifies as Espressif's
  `303a:1001` USB-Serial-JTAG controller.

Do not use BOOT+RESET to recover firmware that already switched the USB PHY
to OTG. That selection survives RESET, so the ROM may have no USB connection.
The BOOT-hold handler restores the PHY before rebooting.

The reflash handler first disconnects USB OTG for 250 ms, then restores the
RTC USB configuration and enters download mode. Without that disconnect,
macOS can retain the GUD device's address and descriptors even though the
ROM is running; the console and display appear frozen. The flash script
selects the ROM port by Espressif's VID/PID and refuses ambiguous matches.
Connect only the ESP32 board you intend to flash.

Either way the script then flashes over that port, clears the
`FORCE_DOWNLOAD_BOOT` flag the firmware used to get there (the ROM never clears
it, so without this every reset lands back in the bootloader), and resets into
the app with the RTC watchdog. A DTR/RTS reset won't do: the USB-serial-JTAG
peripheral samples BOOT low whenever the host holds DTR, which macOS does on
every open. That last step uses esptool through
[uv](https://github.com/astral-sh/uv), so `uv` needs to be on the path;
nothing else is installed.

## Serial console

The CDC port is named `/dev/cu.usbmodemGUDESP32S3*` on macOS (`just console`
tails it). While it is open
the firmware prints one line a second with counters (updates, uncompressed
and wire bytes, rejected requests, abandoned transfers, decode errors) and
per-stage timings in microseconds (`rx_us`, `decode_us`, `swap_us`, `spi_us`,
`idle_us` waiting for the host, `starve_us` waiting for a free buffer). Every
five seconds it adds a boot line:

```
boot #3 previous_stage=5 last_stall=0/0 last_panic=none
```

That comes from a record in RTC RAM that survives resets: the boot count, the
last boot stage reached, the last panic message, and whether the USB control
handler ever found the console task stalled (it reboots when it does). Boot
stage prints also go to the ROM's USB-serial-JTAG port, which is live until
the USB stack claims the PHY.

Do not write to the console other than the reflash string: macOS replays
queued tty output into a re-enumerated device with the same name, which is
how a 1200-baud reboot trigger ended up rebooting the board on every open.

## On-panel diagnostics

At boot the panel shows the gudlet logo, a mint monitor above a lowercase
wordmark on a dark background. It uses the same DMA path as incoming frames.
The backlight turns on after the logo is drawn, and USB starts half a second
later. If the previous run panicked, the panel shows the panic message for
four seconds instead.

Building with `just features=debug-strip build esp32s3` draws a hex status strip across the
top 36 rows once a second (uptime, console tick, control requests, DTR,
console bytes in, console writes ok/timed out, updates, bands written). It
depends on nothing but the panel, so it keeps reporting when USB is dead.

## Pinout

From Waveshare's `pin_config.h`. Only the LCD is used; the touch controller,
IMU, RTC and buzzer are untouched.

| Signal    | GPIO |
|-----------|------|
| LCD DC    | 4    |
| LCD CS    | 5    |
| LCD SCK   | 6    |
| LCD MOSI  | 7    |
| LCD RST   | 8    |
| Backlight | 15 (LEDC) |
| USB D-    | 19   |
| USB D+    | 20   |

The USB pins are shared with the ROM's USB-serial-JTAG. The firmware switches
the PHY to the OTG controller, so that port disappears while the app runs.

## Protocol coverage

| Request | Behaviour |
|---|---|
| `GET_STATUS` | Status of the last request |
| `GET_DESCRIPTOR` | Magic, version 1, no flags, LZ4 accepted, one frame per update, fixed 240×280 |
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
| `SET_BUFFER` | validates the rectangle and queues the panel window |
| `SET_CONNECTOR_FORCE_DETECT` | accepted |

Anything else stalls with `REQUEST_NOT_SUPPORTED`. Updates larger than one
frame, or LZ4 payloads that are empty or larger than the raw size, are
rejected with `INVALID_PARAMETER`.

## Credits

Modelled on scd31's
[rp2040-rcade-crt-driver](https://gitlab.scd31.com/sophie/rp2040-rcade-crt-driver)
and
[stm32-usb-vga-rcade-adapter](https://gitlab.scd31.com/sophie/stm32-usb-vga-rcade-adapter).
