#!/bin/sh
# Cargo runner. If a gud-rp2040 board is running, reboot it into BOOTSEL over its
# serial port (open at 1200 baud, close); then copy the firmware to RPI-RP2.
set -e
ELF="$1"
VOLUME=/Volumes/RPI-RP2

if [ ! -d "$VOLUME" ]; then
    for port in /dev/cu.usbmodem*"$(printf GUDRP2040)"*; do
        [ -e "$port" ] || continue
        echo "rebooting $port into BOOTSEL"
        stty -f "$port" 1200 || true
    done
    i=0
    while [ ! -d "$VOLUME" ] && [ "$i" -lt 100 ]; do
        sleep 0.1
        i=$((i + 1))
    done
fi

if [ ! -d "$VOLUME" ]; then
    echo "no $VOLUME: hold BOOT and tap RESET, then run again" >&2
    exit 1
fi

exec elf2uf2-rs -d "$ELF"
