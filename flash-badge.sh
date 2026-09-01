#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
firmware_elf="$project_dir/target/xtensa-esp32s3-espidf/release/temporal-trivia-badge-firmware"
partition_table="$project_dir/firmware/partitions.csv"

if [ -n "${ESPFLASH:-}" ]; then
    espflash=$ESPFLASH
elif [ -x "$project_dir/.tools/bin/espflash" ]; then
    espflash="$project_dir/.tools/bin/espflash"
else
    espflash=$(command -v espflash || true)
fi
if [ -z "$espflash" ] || [ ! -x "$espflash" ]; then
    echo "missing espflash; install it with: cargo install espflash" >&2
    exit 1
fi

if [ ! -f "$firmware_elf" ]; then
    echo "missing firmware build; run ./build-firmware.sh first" >&2
    exit 1
fi

port_args=""
if [ "$#" -gt 1 ]; then
    echo "usage: $0 [serial-port]" >&2
    exit 1
fi
if [ "$#" -eq 1 ]; then
    port_args="--port $1"
fi

# The explicit partition table is required. Without it, espflash assumes a
# 4 MiB default app partition even though the ELF was built for this 16 MiB
# badge layout.
#
# --no-skip writes every segment. Incremental writes twice left a changed
# trailing application segment erased and produced `invalid segment length
# 0xffffffff` at boot, which needs a full write to recover from -- so this
# pays two minutes per flash to never hand back a badge that will not boot.
# The serial monitor is the default because a human flashing one badge wants
# the boot log. Set FLASH_MONITOR=0 to flash several badges from a script.
monitor_arg="--monitor"
if [ "${FLASH_MONITOR:-1}" = "0" ]; then
    monitor_arg=""
fi

# shellcheck disable=SC2086
exec "$espflash" flash \
    --chip esp32s3 \
    --flash-size 16mb \
    --partition-table "$partition_table" \
    --target-app-partition factory \
    --no-skip \
    $monitor_arg \
    $port_args \
    "$firmware_elf"
