# Working in this repository

Durable Trivia is a Temporal demo with three runtimes in one workspace. Most
mistakes here come from treating them as one codebase, so read this before
changing anything.

## The four boundaries

| Boundary | Crates | What is different about it |
|---|---|---|
| Deterministic Workflow | `web/src/workflow.rs` | Replayed from history. No clocks, no randomness, no I/O, no iteration over a `HashMap`. Any change to the command sequence needs a patch marker. |
| Controller | rest of `web/` | Ordinary async Rust on a multi-thread Tokio runtime. Free to do I/O. |
| Firmware | `firmware/` | Builds only for `xtensa-esp32s3-espidf`, on one current-thread Tokio runtime with about 8 MiB of PSRAM. A blocking call anywhere stops every task, heartbeats included. |
| Shared contract | `shared/`, `badge-screen/`, `badge-input/` | Compiled by both sides. `shared` depends on nothing but `serde` so it can follow the badge; keep it that way. |

Code that looks alike across these is often not duplication. Do not merge a
firmware helper with a controller helper because the bodies match — their
runtime, determinism and resource constraints do not.

`firmware/vendor/` is a vendored copy of Temporal Rust SDK 0.7.0. Do not edit
it; fix our integration instead.

## Commands

```sh
./check-host.sh                       # fmt, strict clippy and every host test
./build-firmware.sh                   # the image people carry
./build-firmware.sh --features hil    # the acceptance image, with USB HIL
./flash-badge.sh /dev/cu.usbmodem101  # full --no-skip write plus a monitor
uv run --script tools/test_physical_badges.py   # two-badge hardware acceptance
```

`.cargo/config.toml` pins `build.target` to the badge, so a bare `cargo test`
or `cargo clippy` from the root tries to build the host crates for the ESP32
and fails. Always go through `check-host.sh`, and pass `--target "$host_target"`
in any new script that runs a host binary.

## Rules that have already cost us a round

- **Patch every Workflow change that moves a command.** `ctx.patched(...)` at
  the top of `run`, threaded down to the branch. `full-participant-backlog-v1`
  and `eligible-deck-refill-v1` are the worked examples.
- **New wire fields need `#[serde(default)]`.** Everything in
  `shared/src/contract.rs` is already in somebody's history.
- **Timings that must agree live together** in `shared/src/contract.rs` behind
  compile-time assertions. The heartbeat interval, the failure budget, the
  server timeout and the crash blackout are one ordered set, not four numbers.
- **Only the Activity may draw over a live question.** The sleep monitor and
  the result watcher go through `with_display_if_idle`, which re-tests
  ownership under the display lock.
- **Nothing in the firmware may block without a timeout.** One runtime, one
  thread; `BLOCK` on an I2C write once meant a stuck bus could take the Worker
  down with the screen.
- **Do not ship a debug backdoor.** The USB HIL reader is behind the `hil`
  feature and `build-firmware.sh` asserts which image it built.

## Documentation that has to stay true

`GAME_SPEC.md` is the contract, not a description — if behaviour changes, it
changes in the same commit. `blog.md` is the engineering journal: append what
was tried, what failed and what was actually validated, including on hardware.
Say plainly what was *not* verified.
