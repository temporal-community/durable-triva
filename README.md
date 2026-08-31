# Durable Trivia

Durable Trivia is a 60-second competitive game where Temporal Replay 2026
badges and phone players share real Rust Temporal Activities. A Rust/Axum
controller runs the Workflow Worker and a 16:9 scoreboard on a laptop; Temporal
Cloud coordinates questions, retries unfinished work, and preserves the round
through crashes.
Both the controller and badge firmware currently pin Temporal Rust SDK `0.7.0`.

## Start here

- [Firmware setup, building, flashing, and badge controls](firmware/README.md)
- [Web controller, Temporal Cloud, and scoreboard setup](web/README.md)
- [Game rules and behavior](GAME_SPEC.md)
- [Engineering journal](blog.md)

The firmware and web controller can be developed independently. You only need
both running for a physical game.

## Architecture

- `firmware/` contains the Rust/ESP-IDF Activity Worker for the
  [Temporal Replay 2026 Badge](https://badge.temporal.io/), including its OLED
  UI, button input, deterministic identity, and NVS session state.
- `web/` contains the Rust Workflow Worker, Axum operator server, 16:9
  scoreboard, phone UI/API, serverless phone Activity Worker, load simulator,
  and bundled trivia deck.
- `shared/` contains the serialized game contract used by both Workers so their
  Temporal payloads cannot drift independently.

The Mac Workflow Worker and badge Activity Workers use the same logical Task
Queue, `temporal-trivia-badges-v1`, while polling only their respective task
types. This makes every process visible together in a Workflow's **Workers**
tab without allowing the Mac to answer trivia or a badge to execute Workflows.

Temporal Cloud is the durable system of record. The laptop may restart and
badges may disconnect without resetting the active Workflow. Activities
abandoned by a failed Worker return to the Task Queue for another badge. Wrong
answers are successful Activity results and do not retry.

Operator power-ups are durable Workflow Updates. Every awake badge queries the
Workflow state, displays a short power-up overlay, and then restores its active
question or waiting screen; answer input is suppressed while the overlay is up.

## Shared requirements

- Git.
- Rust installed with `rustup`.
- A Temporal Cloud namespace and API key.
- macOS or Linux for the host-side tools.

Clone the repository and run all documented commands from its root unless a
guide says otherwise:

```sh
git clone https://github.com/Shy/temporal-trivia-badge.git
cd temporal-trivia-badge
```

## Shared Temporal configuration

Both components use the same ignored Temporal Cloud configuration:

```sh
cp .env.temporal.example .env
```

Fill in `.env`:

```dotenv
TEMPORAL_ADDRESS=your-namespace.tmprl.cloud:7233
TEMPORAL_NAMESPACE=your-namespace.your-account
TEMPORAL_API_KEY=your-api-key
```

Process environment variables override these values. Set `TEMPORAL_ENV_FILE`
to use a file in another location; an explicit path must exist. Temporal Cloud
uses server-authenticated TLS, and the API key replaces a client certificate,
not TLS itself.

Configuration files containing Temporal or Wi-Fi credentials are ignored by
Git. Never commit populated dotenv files, private keys, or generated firmware
configuration.

## Common checks

Run formatting and whitespace checks from the repository root:

```sh
cargo fmt --all -- --check
git diff --check
```

Component-specific build, test, and hardware verification commands live in the
[firmware guide](firmware/README.md) and [web guide](web/README.md).

## Simulate badges on a Mac

Start ten software badges against the configured Temporal Cloud namespace:

```sh
./simulate-badges.sh 10
```

The launcher creates one process per badge. Each process registers one real
Rust SDK Activity Worker named `badge/SIM-01` through `badge/SIM-10`, with one
Activity slot, on the same Task Queue used by the firmware. Stop all simulated
badges together with `Ctrl-C`. Simulated badges answer correctly 80% of the
time and incorrectly 20% of the time on a deterministic cadence, so the board
exercises both score directions. Run `./run-web.sh` in another terminal to use
the normal controller and web UI.

The most recent physical validation covered build, flash, boot, Wi-Fi,
Temporal Cloud polling, answers, sleep/wake, supervised Mac Worker recovery,
and a real heartbeat timeout moving the same Activity from
`KEEN-SEAL-70` attempt 1 to `KEEN-RAVEN-C8` attempt 2.

## Run phone players locally

Use three terminals after starting the Mac controller:

```sh
# Terminal 1: TV/controller, with a QR that points at the phone API
PUBLIC_BASE_URL=http://127.0.0.1:8080 ./run-web.sh

# Terminal 2: phone web UI and API
./run-phone-api.sh

# Terminal 3: Rust Activity Worker for phone assignments
./run-phone-worker.sh
```

Open <http://127.0.0.1:8080> on a phone or another browser. The API gives the
browser a stable anonymous callsign cookie. It does not send the correct answer
to the browser. Each session owns at most one real Temporal Activity.

For a 100-phone load test, restart the phone API with its test-only answer key
enabled, start a round, and run the simulator:

```sh
PHONE_SIMULATION=1 ./run-phone-api.sh
./simulate-phones.sh 100
```

The test-only flag adds the correct answer index to the simulator payload. Do
not set it on the public Cloud Run service.
