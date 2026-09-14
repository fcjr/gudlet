#!/bin/sh
# Cargo runner. If the firmware is running, reboot it into the ROM download
# mode by writing the magic string to its serial console; then flash with
# espflash over the ROM's USB-Serial-JTAG port and boot the app with a
# watchdog reset.
set -e
ELF="$1"

find_rom_port() {
    # Match Espressif's USB-Serial-JTAG VID/PID, not an arbitrary serial port.
    # espflash prints PID:VID. macOS exposes both tty and cu names; use cu.
    ports=$(espflash list-ports --skip-update-check | awk '$1 ~ /^\/dev\/cu\./ && toupper($2) == "1001:303A" { print $1 }')
    case "$ports" in
        *'
'*) echo "multiple Espressif download ports found; connect only the board to flash" >&2; exit 1 ;;
    esac
    printf '%s\n' "$ports"
}

find_app_port() {
    ports=$(ls /dev/cu.usbmodem*GUDESP32S3* 2>/dev/null || true)
    case "$ports" in
        *'
'*) echo "multiple gudlet ESP32 boards found; connect only the board to flash" >&2; exit 1 ;;
    esac
    printf '%s\n' "$ports"
}

# Keep the port open while macOS submits the write. The firmware disconnects
# OTG before switching to USB-Serial-JTAG, so the ROM gets a fresh USB address.
request_download_mode() {
    port="$1"
    echo "rebooting $port into download mode"
    stty -f "$port" 115200 raw 2>/dev/null || true
    (
        printf 'gud-reflash\n'
        sleep 0.5
    ) > "$port" 2>/dev/null || echo "could not write to $port" >&2
}

# Wait up to $1 tenths of a second for the ROM port.
wait_rom_port() {
    i=0
    while [ "$i" -lt "$1" ]; do
        PORT=$(find_rom_port)
        [ -z "$PORT" ] || return 0
        sleep 0.1
        i=$((i + 1))
    done
    return 1
}

PORT=$(find_rom_port)
if [ -z "$PORT" ]; then
    app=$(find_app_port)
    if [ -n "$app" ]; then
        request_download_mode "$app"
        # If the app is still there after a while, ask again.
        attempt=0
        while ! wait_rom_port 50 && [ "$attempt" -lt 2 ]; do
            attempt=$((attempt + 1))
            app=$(find_app_port)
            [ -n "$app" ] || break
            request_download_mode "$app"
        done
    fi
fi

if [ -z "$PORT" ]; then
    if [ -n "$(find_app_port)" ]; then
        echo "download port did not appear; hold BOOT alone for one second, then run again" >&2
    else
        echo "no download-mode port found." >&2
        echo "  - If the board just vanished from USB, its USB PHY is stuck on the OTG side:" >&2
        echo "    unplug it, hold BOOT, plug it back in, then run again." >&2
        echo "  - Otherwise hold BOOT and tap RESET, then run again." >&2
    fi
    exit 1
fi

# The port node can appear a moment before the ROM is ready to talk.
sleep 0.5
echo "flashing via $PORT"
espflash flash --port "$PORT" --chip esp32s3 --after no-reset "$ELF"

# The firmware enters download mode by setting RTC_CNTL_OPTION1's
# FORCE_DOWNLOAD_BOOT bit, and the ROM never clears it, so every reset would
# land back in the bootloader (arduino-esp32 issue 6762). Clear it, then
# reset into the app with the RTC watchdog: a DTR/RTS "hard reset" through
# the USB-serial-JTAG peripheral samples BOOT low whenever the host holds DTR,
# which macOS does on every open. esptool runs through uv, nothing is installed.
echo "clearing FORCE_DOWNLOAD_BOOT and resetting into the app"
exec uvx --from esptool esptool --chip esp32s3 --port "$PORT" \
    --before no-reset --after watchdog-reset --no-stub write-mem 0x6000812C 0
