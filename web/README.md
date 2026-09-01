# Web controller and scoreboard

This directory contains the Rust Temporal Workflow Worker, Axum operator
server, trivia deck, fixed 16:9 scoreboard, and the badge simulator. The
controller starts rounds, schedules question Activities for badges, observes
durable game state, and serves the operator UI at `127.0.0.1:3000`.

The controller polls Workflow Tasks on the same
`temporal-trivia-badges-v1` Task Queue used by badge Activity Workers. Each
Worker enables only its own task type, and Temporal UI can show the Mac and
physical badges together on the Workflow's **Workers** tab.

Run the commands below from the repository root.

## Requirements

- Rust installed with `rustup`.
- A Temporal Cloud namespace and API key.
- A macOS or Linux laptop that can reach Temporal Cloud.
- A browser suitable for mirroring to the event display.

The web controller can run without a connected badge, but a physical round
needs at least one flashed Replay 2026 badge polling the shared Task Queue. See
the [firmware guide](../firmware/README.md) to prepare one.

## Configure Temporal Cloud

Complete the root [shared Temporal configuration](../README.md#shared-temporal-configuration).
The controller reads that configuration when `run-web.sh` starts it.

## Run the controller

Start the included restart supervisor:

```sh
./run-web.sh
```

Open <http://127.0.0.1:3000> and mirror that browser window to the TV. Use
`Ctrl+C` in the terminal to stop the controller.

Always use `run-web.sh` for the demo. It marks the Worker as supervised and
restarts it after the operator deliberately crashes the Mac process. The frozen
board stays on screen and readable throughout — scores and score bars hold
exactly where they were — while the rail advances only after observing process
loss, a new process ID, a successful Temporal query, and a restored-state
digest matching the frozen board.

## Run a game

Click **START ROUND** after badges are polling. Only one round can run at a
time. The controller counts active ESP32 Activity pollers and keeps one
question Activity outstanding per detected badge, so both badges in a
two-badge game receive work. Badges that begin polling during an active round
can claim later work, but the badge target is fixed from the roster detected at
round start. A heartbeat retry may wait briefly for a Worker instead of keeping
a healthy badge idle throughout normal play.

Rust-only scheduling deals no other category, so when the deck runs out of Rust
cards the Workflow recycles it into a fresh cycle rather than dealing nothing.
The change is behind the `eligible-deck-refill-v1` patch marker, so histories
written before it replay their recorded command sequence.

Open the operator tray with the small **TP7** test pad in the bottom-right
corner or the `O` keyboard shortcut. It rises from the bottom edge and the
lanes compress upward, so every score stays visible while an Update lands. Its
validated Workflow Updates provide:

- Double points for 10 seconds.
- Rust-only scheduling for 10 seconds.
- Sudden death, where the next correct answer ends the round.
- One 30-second extension.
- Ending the round immediately, for when the crowd moves on.
- A supervised Mac Worker crash and automatic restart demonstration.

Double points, Rust only, and sudden death are mutually exclusive. The timer
extension is independent. Ending a round brings the durable deadline forward, so
it closes through the normal finish path within about a second and still writes
its round summary.

Every control has a keyboard shortcut so a beat can be hit without opening the
tray over the board: `1` double points, `2` Rust only, `3` sudden death, `4`
+30 seconds, `E` end the round, and `C` crash the Mac Worker. `C` asks for
confirmation because it really does kill the process. The History tab includes the exact Workflow ID and
Run ID and opens the current execution in Temporal Cloud.

Completed rounds are stored in Workflow Memo and listed through Temporal
Visibility. No database or namespace changes are required.

## Optional Search Attributes

Typed Search Attributes are optional and require an API key with
namespace-operator permission:

```sh
./configure-visibility.sh
TRIVIA_SEARCH_ATTRIBUTES=1 ./run-web.sh
```

The game and round history work without them.

## Test the web and shared crates

```sh
host_target=$(rustc -vV | awk '/^host:/ { print $2 }')
cargo test --offline -p temporal-trivia-shared -p temporal-trivia-web --target "$host_target"
cargo clippy --offline -p temporal-trivia-shared -p temporal-trivia-web \
  --target "$host_target" --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
```

The question-pool tests verify the category mix, badge-size constraints, unique
questions, and answer indexes. Workflow tests cover durable timing, chaos
Signals, retry scheduling, deck recycling under Rust-only, and shared payload
validation.

Prefer `./check-host.sh` from the repository root, which runs all of the above
with the host target and the stable toolchain already supplied.

See the root [game specification](../GAME_SPEC.md) for scoring and retry rules
and the [engineering journal](../blog.md) for live recovery validation.
