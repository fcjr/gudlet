# GUD firmware for the Waveshare 1.69" LCD boards. `just flash` finds the
# board on USB; `just flash esp32s3` / `just flash rp2040` pick one.

set shell := ["sh", "-eu", "-c"]

# Extra cargo features, e.g. `just features=debug-strip flash esp32s3`.
features := ""
_features := if features == "" { "" } else { "--features " + features }

default:
    @just --list --unsorted

# Build a board's firmware (esp32s3 | rp2040)
build board:
    cd firmware/{{board}} && cargo build --release {{_features}}

# Build and flash a board; with no argument, detect it from what is on USB
flash board="auto":
    #!/bin/sh
    set -eu
    board={{board}}
    if [ "$board" = auto ]; then
        board=$(scripts/detect-board.sh)
        echo "detected $board"
    fi
    cd "firmware/$board" && cargo run --release {{_features}}

# Which board is plugged in
detect:
    @scripts/detect-board.sh

# Unit-test the shared crates on the host
test:
    cargo test

# Everything CI would: host tests plus both firmware builds
check: test (build "esp32s3") (build "rp2040")

# Watch a board's serial console (read-only; never write to it, see the READMEs)
console board="auto":
    #!/bin/sh
    set -eu
    board={{board}}
    if [ "$board" = auto ]; then board=$(scripts/detect-board.sh); fi
    case "$board" in
        esp32s3) tag=GUDESP32S3 ;;
        rp2040) tag=GUDRP2040 ;;
        *) echo "unknown board $board" >&2; exit 1 ;;
    esac
    port=$(ls /dev/cu.usbmodem*"$tag"* 2>/dev/null | head -1)
    [ -n "$port" ] || { echo "no $tag console port; is the firmware running?" >&2; exit 1; }
    echo "reading $port (ctrl-c to stop)"
    stty -f "$port" 115200 raw
    cat "$port"

# Remove build output
clean:
    cargo clean
