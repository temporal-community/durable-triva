# Replay 2026 badge firmware

This directory contains the Rust/ESP-IDF Temporal Activity Worker specifically
for the [Temporal Replay 2026 Badge](https://badge.temporal.io/). It is not a
generic ESP32-S3 firmware project. It depends on that badge's 16 MiB flash,
8 MiB PSRAM, OLED, directional buttons, and GPIO mapping.

The Worker renders questions, reads answers, heartbeats while a player decides,
stores its stable badge/session state in NVS, and runs Temporal Rust SDK `1.0.0`.

Each badge also sends Temporal Worker heartbeats every 10 seconds and registers
with a readable `badge/CALLSIGN` Worker identity. In Temporal Cloud, filter the
Workers page by Task Queue `temporal-trivia-badges-v1` to see the physical Rust
Activity Workers and their live slot and poller telemetry.

Run the commands below from the repository root.

## Hardware and host requirements

- A Temporal Replay 2026 Badge connected with a USB data cable.
- A 2.4 GHz Wi-Fi network that can reach Temporal Cloud.
- macOS or Linux.
- A Temporal Cloud namespace and API key.

The official badge developer guide covers the original MicroPython firmware,
WebSerial, and the complete hardware APIs. This repository replaces the badge
application firmware with a Rust/ESP-IDF image for the trivia Worker.

## Install the ESP Rust toolchain

```sh
cargo install espup --locked
espup install
. "$HOME/export-esp.sh"
cargo install espflash ldproxy
```

Source the `espup` export file in every terminal used to build the firmware.
The workspace pins ESP-IDF v5.4 and selects `xtensa-esp32s3-espidf` through the
root `.cargo/config.toml`.

## Configure the badge

Complete the root [shared Temporal configuration](../README.md#shared-temporal-configuration),
then create the ignored Wi-Fi file:

```sh
cp firmware/.env.wifi.example firmware/.env.wifi
```

Set the badge network in `firmware/.env.wifi`:

```dotenv
BADGE_WIFI_SSID=your-2.4-ghz-network
BADGE_WIFI_PASS=your-password
```

Firmware configuration is compiled into the flash image. Rebuild and reflash
after changing Temporal credentials or Wi-Fi. Generated configuration stays
under the ignored root `target/` directory, and build output does not print
credential values.

**A flashed badge is a credential.** The Wi-Fi password and the Temporal Cloud
namespace, address and API key are compiled into the application partition as
plain strings, and anyone holding the badge can recover them:

```sh
esptool.py --chip esp32s3 read_flash 0x10000 0xE00000 dump.bin
strings dump.bin | grep tmprl
```

That key has full access to the namespace — it can start, signal, query and
terminate every Workflow in it. Treat badges you hand out accordingly: use a
namespace dedicated to this demo with nothing else in it, and rotate the API
key once the event is over.

Set `BADGE_WIFI_ENV_FILE` to use a Wi-Fi file in another location. An explicit
path must exist.

## Build

```sh
./build-firmware.sh
```

The first build downloads and compiles ESP-IDF dependencies and can take
several minutes. The release ELF is written to:

```text
target/xtensa-esp32s3-espidf/release/temporal-trivia-badge-firmware
```

If the Xtensa compiler is outside a normal `espup` or ESP-IDF installation,
set `ESP_GCC_DIR` to the directory containing `xtensa-esp32s3-elf-gcc`.

## Flash

Connect one Replay 2026 badge and identify its serial device:

```sh
ls /dev/cu.usbmodem* /dev/ttyACM* 2>/dev/null
```

Flash the discovered device, for example:

```sh
./flash-badge.sh /dev/cu.usbmodem101
```

On Linux, use the discovered `/dev/ttyACM...` path. The script selects the
ESP32-S3, 16 MiB flash, `firmware/partitions.csv`, and the factory application
partition. These settings are required because the Rust application is larger
than the default 4 MiB layout.

Flashing this image replaces the badge's installed application firmware. Keep
the official Replay 2026 badge recovery instructions available if you want to
restore the original image later.

The flash script opens a serial monitor. A successful boot reports the stable
badge callsign, Wi-Fi connection, and polling of
`temporal-trivia-badges-v1`. Exit the monitor with `Ctrl+C`; the badge keeps
running. Set `ESPFLASH` to an executable path to override the flashing tool.

## Badge controls

- Press the directional button matching the on-screen answer position.
- Hold **LEFT+RIGHT** for 500 ms to simulate a Worker failure. The badge stops
  heartbeating for 16 seconds; Temporal's 15-second heartbeat timeout makes the
  unfinished question available to another Worker.
- A wrong answer applies the score penalty and completes the Activity normally.
  Only a simulated Worker failure returns the question to the Task Queue.
- A badge that has abandoned a question refuses it on sight if Temporal offers
  it back, without drawing anything, so the retry reaches another Worker.
- Dropped heartbeats are tolerated for ten seconds before the badge releases
  its question. Temporal's fifteen-second timeout remains what reassigns it.
- A web power-up causes each awake badge to vibrate and show a 1.5-second
  overlay from the durable Workflow state. The badge restores its question or
  waiting screen afterward and ignores answer input while the overlay is up.
- While waiting for work, hold **DOWN** for three seconds to sleep. Release it
  after `SLEEPING`, then press any face button to wake. Sleep is disabled while
  an Activity owns the controls.
- Haptics are always on and reserved for meaningful state changes. The sleep
  countdown pulses on `3`, `2`, `1`, and `0`; correct and wrong answers,
  simulated crash/recovery, and round results each have distinct short
  patterns. Routine input, polling, connection changes, boot, and wake are
  silent.

## Firmware verification

The release build is the primary automated firmware gate:

```sh
./build-firmware.sh
```

Physical verification requires checking the serial log and badge display after
flashing. Confirm boot, PSRAM, Wi-Fi, Temporal polling, question rendering,
button input, simulated crash, and sleep/wake behavior.

Confirm the haptic strength and patterns by hand on the physical badge; serial
logs can verify the event path but cannot verify how the motor feels.

### Automated two-badge acceptance

The HIL command protocol is **not** in the default image. `HIL ANSWER CORRECT`
reads the correct index out of the question the badge is holding, so a badge
carrying that reader is a badge anyone with a USB cable can win a round on.
`build-firmware.sh` asserts the gate either way and prints which image it built.

Build and flash the acceptance image explicitly:

```sh
./build-firmware.sh --features hil
./flash-badge.sh /dev/cu.usbmodem101
```

Then, with the controller running and exactly two badges connected, run:

```sh
uv run --script tools/test_physical_badges.py
```

Reflash the badges with a plain `./build-firmware.sh` image before handing them
to anyone.

The runner identifies both physical callsigns, starts a Workflow using the
normal scheduler policy, and requires both real badge Workers to own questions
simultaneously before answering either one. The round does not start until both
Workers have logged that they are polling Temporal. The runner then asks each
badge firmware to inject the question's correct directional gesture and
verifies that Temporal records one correct answer per badge. It also verifies
that both badges leave the final result screen and return to waiting for the
next round. `--backlog-override N` remains available for a deliberately
nonstandard diagnostic run.

Readiness and simultaneous ownership are both asked for over `HIL STATUS`,
which reports `polling=` and `active=` on demand. The runner opens each port
without toggling reset, so it cannot depend on a boot log line that a
still-running badge printed minutes ago.

This exercises the physical ESP32-S3 boards, firmware, Wi-Fi, Temporal Workers,
and the same input state machine used by the face buttons. It does not optically
inspect OLED pixels or confirm the physical feel of haptics and buttons. Those,
sleep/wake, and the deliberate crash gesture still require hands-on acceptance.

## Screens

Every OLED screen is composed by the [`badge-screen`](../badge-screen) crate,
which has no ESP-IDF dependency. That keeps the layout unit tested and lets you
review all of it without flashing hardware:

```sh
host_target=$(rustc -vV | awk '/^host:/ { print $2 }')
cargo test --offline -p badge-screen --target "$host_target"
cargo run --offline -p badge-screen --bin preview --target "$host_target" > screens.html
```

Open `screens.html` to see every screen rendered at 128x64. `firmware/display.rs`
owns the I2C transport and nothing else.

Small text uses Tom Thumb (also published as Fixed4x6), a 3x5 monospace bitmap
face by Brian Swetland with readability revisions by Robey Pointer, released for
any use under CC0 / CC-BY 3.0. It replaced a mechanical downsample of the 5x7
face that rendered `N` as `H` and `0` as `8`.

See the root [engineering journal](../blog.md) for the current Rust SDK
portability patches and physical validation results.
