# Web controller and scoreboard

This directory contains the Rust Temporal Workflow Worker, Axum operator
server, trivia deck, fixed 16:9 scoreboard, phone UI/API, and serverless phone
Activity Worker. The controller starts rounds, schedules question Activities
for badges and phones, observes durable game state, and serves the operator UI
at `127.0.0.1:3000`.

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
time. The controller counts active ESP32 Activity pollers and keeps one badge
free as recovery capacity, so two badges run one Activity at a time and ten
badges run nine. A single-badge game still runs one Activity. Badges that begin
polling during an active round can claim later work, but the recovery capacity
is fixed from the roster detected at round start.

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
Signals, retry scheduling, and shared payload validation.

See the root [game specification](../GAME_SPEC.md) for scoring and retry rules
and the [engineering journal](../blog.md) for live recovery validation.

## Phone players

The public phone path has two Rust processes:

- `phone_api` serves the portrait UI and turns browser heartbeats and answers
  into asynchronous Activity heartbeats/completions.
- `phone_worker` polls `temporal-trivia-phones-v1`, signals the durable
  assignment into the Game Workflow, then returns `WillCompleteAsync`.

The API refreshes all phone assignments with one batched Workflow query every
250 ms. Individual browser polls read that disposable cache, while the Game
Workflow remains the source of truth. Restarting the API repopulates the cache
from Temporal; no database is required.

Run both processes from the repository root:

```sh
./run-phone-api.sh
./run-phone-worker.sh
```

Set `PUBLIC_BASE_URL` when starting the controller so the TV QR targets the
public phone service. Production HTTPS uses a Secure cookie by default; the
local launcher sets `PHONE_COOKIE_SECURE=0` for HTTP localhost.

The eventual Worker Versioning demo is deliberately deferred. It will be a
terminal-triggered deployment step, not an operator UI control, after the
basic Cloud Run deployment is validated.

## Cloud Run container

The root `Dockerfile` builds both Rust binaries. Its default command runs the
public API; deploy the same immutable image to a Worker Pool with the command
overridden to `/usr/local/bin/phone_worker`.

```sh
gcloud builds submit --tag REGION-docker.pkg.dev/PROJECT/REPOSITORY/durable-trivia-phone .

gcloud run deploy durable-trivia-phone \
  --image REGION-docker.pkg.dev/PROJECT/REPOSITORY/durable-trivia-phone \
  --region REGION --allow-unauthenticated \
  --set-env-vars TEMPORAL_ADDRESS=ADDRESS,TEMPORAL_NAMESPACE=NAMESPACE \
  --set-secrets TEMPORAL_API_KEY=temporal-api-key:latest

gcloud run worker-pools deploy durable-trivia-phone-worker \
  --image REGION-docker.pkg.dev/PROJECT/REPOSITORY/durable-trivia-phone \
  --region REGION --instances 1 --command /usr/local/bin/phone_worker \
  --set-env-vars TEMPORAL_ADDRESS=ADDRESS,TEMPORAL_NAMESPACE=NAMESPACE \
  --set-secrets TEMPORAL_API_KEY=temporal-api-key:latest
```

Cloud Run Worker Pools use a fixed instance count rather than request-driven
autoscaling. Keep one warm instance for the stage and booth demo. Put the
Temporal API key in Secret Manager rather than a checked-in env file or shell
history.
