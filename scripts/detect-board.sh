#!/bin/sh
# Print which supported board is on the USB bus: `esp32s3` or `rp2040`.
# Looks for a running GUD firmware first (by the serial number in its CDC
# port name), then for the chip's own bootloader.
set -eu

have_port() {
    ls /dev/cu.usbmodem*"$1"* >/dev/null 2>&1
}

if have_port GUDESP32S3; then
    echo esp32s3
elif have_port GUDRP2040; then
    echo rp2040
elif [ -d /Volumes/RPI-RP2 ]; then
    # RP2040 in BOOTSEL mounts a drive.
    echo rp2040
elif ioreg -p IOUSB -l -w0 2>/dev/null | grep -q '"USB Product Name" = "USB JTAG_serial debug unit"'; then
    # ESP32-S3 ROM download mode (Espressif 303a:1001).
    echo esp32s3
else
    echo "no GUD board found: plug one in, or put it in its bootloader" >&2
    exit 1
fi
