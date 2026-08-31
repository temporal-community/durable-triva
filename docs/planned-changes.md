# Planned changes — temporal-trivia-badge

**Status:** the 2026-08-26/30 review plan is **complete** — all 9 stages and all 11 low-severity items written, tested and committed to `updates` (base `7f35c4f`). One new finding is open and specced below: **badge input latency**, not yet implemented.
**Supersedes:** `projectreview.md` (reviews of 2026-08-26 and 2026-08-30) and `web-envconfig-migration.md` (2026-08-27). Both remain as provenance — findings, measurements and verification transcripts live there; this file is the decided plan.
**Patch on disk:** `web-envconfig-migration.diff` — 5 files, +164 / −141. Re-checked with `git apply --check` on 2026-08-30: **applies cleanly**.

**Tests:** 70 passing, 0 failing (badge-input 11, badge-screen 13, shared 17, web lib 2, web bin 27). Was 31. Host and `esp` toolchains both clean at `clippy -D warnings`; firmware clippy is clean for the first time (baseline had 2).

| Commit | What |
| --- | --- |
| `157aefd` | Stage 1 — 14 tests |
| `2761511`, `6a5c431` | Stage 2 — envconfig migration (M1, F2, F2b-web) |
| `5729cf7` | L2 — `active_modifier` takes the clock |
| `3af1efd` | Stage 3 — F1 |
| `c58ef55` | Stage 4 — F2b-fw |
| `193dbfa` | Stage 5 — F5 |
| `fe903f8` | Stage 6 — W1 |
| `f3b4efb` | Stage 7 — F7 |
| `911c2ee` | Stage 8 — FW1, F4, F8, L1, W2 |
| `e5e13a8` | Stage 9 — F3, `check-host.sh` |
| `1892ec8` | L8, L9, L10, L11 |
| `61bc1bf` | L4, L5 — `shared` split |
| `8d78a7a` | L6 — `badge-screen` docs |
| `58ab43e` | L3 — backlog fallback |
| `1f27609` | L7 — `with_display` |
| `4a89fdb` | Scoreless rounds tie |
| `5324426` | F1 tested via a `SnapshotQuery` trait |
| `e9713d2` | `identity_from_mac` into `shared` |
| `e43abf0` | `ButtonState` enum in a new `badge-input` crate |
| `8948bd7` | Both crossover directions covered |

---

## Open — badge input latency

**Reported:** 2026-08-31, from handling a physical badge. Presses "seemed to need to be held for a long time" before registering.

### What is actually happening

Not a debounce or a hold threshold — there is neither. `BadgeInput::sample()` reads `is_low()` on four `Pull::Up` GPIOs, and the only timers in the input path are the 20 ms poll interval and `PANIC_HOLD` (500 ms, and only for LEFT+RIGHT together). A single press should register in about 20 ms.

Three things stop it, in order of cost:

| # | Cause | Cost |
| --- | --- | --- |
| 1 | Nothing samples the buttons until the `badge_started` Signal has been awaited | up to `GAME_SIGNAL_TIMEOUT` = **750 ms**, plus two NVS flash operations |
| 2 | `wait_for_choice` opens by draining whatever is already held, and discards it | consumes the press entirely |
| 3 | LEFT and RIGHT answer on release, not on press | by design |

In `answer_question` the order is: draw → `begin_game` (NVS write) → `start_result_watcher` → **await the Signal** → `is_abandoned` (NVS read) → `wait_for_choice`. The question is on screen for that whole window with nothing reading the buttons.

Then `wait_for_choice` starts with `while self.sample_buttons()?.any() { sleep(20 ms) }`. If the player is still holding when sampling finally begins, that loop waits for release and enters the main loop with nothing armed. The press is discarded, not queued.

Net effect: **the first press after a question appears is normally eaten**, and the press that works is a later one. Subjectively that is indistinguishable from "I had to hold it."

Cause 3 is correct and stays — the panic combo has to get a chance to be recognised before either side release counts as an answer. It is listed because it explains why the symptom is worse on LEFT/RIGHT than on UP/DOWN, which is a useful thing to confirm on hardware.

### Constraint the fix must not break

The Signal currently precedes the `is_abandoned` check **deliberately**, and the comment at that call site says why: every real Temporal attempt must be reported before this Worker refuses a question it already abandoned, so the public attempt count stays aligned with `ActivityContext` even when the same badge polls a retry. Any reordering has to keep the Signal *sent* before the abandon path returns.

### Proposed fix — overlap the Signal with the wait, do not drop it

Start the Signal, then run it concurrently with `wait_for_choice` rather than before it:

1. `show_question`
2. `begin_game` (NVS)
3. `start_result_watcher`
4. Build the Signal future — do **not** await it yet
5. If `is_abandoned`: await the Signal future (still bounded by `GAME_SIGNAL_TIMEOUT`), then the existing 250 ms sleep and early `Err` — constraint preserved
6. Otherwise `join!` the Signal future with `wait_for_choice`

Sampling then begins within milliseconds of the question appearing, and the 750 ms overlaps the player's thinking time instead of preceding it. Total Activity time is unchanged: `join!` finishes at `max(signal, choice)`, and the choice is always the longer of the two.

**Why `join!` and not `tokio::spawn`.** Spawning is simpler but changes when the Signal can land relative to the Activity completing. An orphaned task could deliver `badge_started` after a retry has already started on another badge, which the Workflow would read as a spurious handoff — `is_reassignment` compares badge id and attempt against `assignments`, and it has no way to know the Signal is stale. `join!` keeps the Signal inside the Activity's own lifetime, exactly as today; only its position moves.

**Left alone:** the drain loop. It exists so a press meant for the previous screen does not leak into this question, which is right. Its problem is only that it currently runs a second late; once sampling starts promptly, it drains a press that is genuinely stale rather than the player's first real attempt.

### Verify before and after

The wall-clock cost is unmeasured — the 750 ms is a ceiling, not an observation, and the two NVS operations are unquantified. Land instrumentation first:

- `log::info!` with `Instant::now()` deltas either side of the Signal await and either side of the drain loop, on a real badge against the real Cloud namespace.
- Confirm the asymmetry the analysis predicts: UP/DOWN should already feel better than LEFT/RIGHT, since only the latter wait for release.

Then re-measure the same two deltas after the change. Expected: the pre-sampling gap drops from "hundreds of ms" to the NVS write alone.

No host test can cover this — it is ordering inside an Activity against a live Temporal connection. `badge-input`'s 11 tests already cover what a press *means* once sampled; this is about when sampling starts.

### Not doing

- **Debouncing.** No evidence it is needed: bounce would produce spurious extra presses, and the reported symptom is missed ones. Revisit only if instrumentation shows double-fires.
- **Shortening the 20 ms poll.** It is not the bottleneck, and it is what bounds the crossover window that used to wedge the side buttons.
- **Moving `begin_game` off the critical path.** It must precede input: it is what records the round that `is_abandoned` and the panic path read.

---

## What "VERIFIED" means in the source docs

The 2026-08-26 review applied each fix, compiled it, ran clippy and the tests, then **reverted it** and reconfirmed the baseline. VERIFIED means *proven to compile and pass*, not *in the tree*. Every item below is still to be written.

## Supersessions resolved

The two documents overlap, and one of the overlaps changes the shape of the work.

| Original | Resolution |
| --- | --- |
| **F2** (High) — drop the `TrafficLight/.env` fallback in `web/src/main.rs:662`, `bail!` on a missing `TEMPORAL_ENV_FILE` | **Superseded by M1.** The migration deletes `read_cloud_settings` outright rather than patching it. Do not do both — F2's point-fix edits a function that M1 removes. |
| **F2b** (High) — same correction in `web/src/bin/simulate_badges.rs:198` *and* `firmware/build.rs:49` | **Split.** The `simulate_badges.rs` half is fully absorbed by M1 (that copy is deleted). The `firmware/build.rs` half survives as **F2b-fw** below — M1 explicitly excludes firmware. |
| **Low #4** — split `shared/src/lib.rs` into `contract` + `env` modules | **Still valid, and narrower than it looks.** The migration confirmed the dotenv parser cannot be deleted: `firmware/build.rs:18` calls `parse_env` at build time and the badge has no environment to read instead. This is a module split, not a removal. |

**F2b-fw is smaller than the review stated.** `config_path` at `firmware/build.rs:25-36` already asserts the file exists when `TEMPORAL_ENV_FILE` is set (`:28-32`). The only real defect is the `legacy_temporal` binding at `:49-53` still resolving to `TrafficLight/.env`. Once M1 lands, that is the last reference to `TrafficLight` in the repo.

## Ordering

**Tests go first.** Every stage below changes behaviour that nothing currently pins — the 31-test baseline had no coverage of `finish()`, the event window, `RoundMemo` aggregation, or deck payload size, and firmware had none at all. Refactors land more safely behind tests than in front of them.

**M1 goes second**, which reverses the review's "F1 + F2 together" opener. The reason is mechanical rather than a judgement about severity: M1 is the only item that exists as a *patch file*, and its applicability degrades as `web/src/main.rs` accumulates unrelated edits. F1 (`:233`) and F4 (`:505`) are far from M1's `:662` and would probably still apply with fuzz, but "probably" is not worth spending when the alternative is free. Everything after M1 is a hand-edit whose location can be re-derived.

### Stage 1 — tests (written; 1 failing) — **[DONE — 157aefd]**

14 tests added across 4 files, chosen to cover the behaviour the later stages disturb. Host toolchain, `cargo fmt` and `clippy -D warnings` clean.

| Test | File | Covers |
| --- | --- | --- |
| `a_scoreless_round_names_every_badge_that_joined` | `shared/src/lib.rs` | **Pins current behaviour, does not endorse it.** `badge_started` inserts a player at zero before that badge answers, so a round whose timer beats the first answer leaves the field tied at the maximum and every panel reads WINNER. Decision pending — see Open questions |
| `a_round_nobody_joined_has_no_winner` | `shared/src/lib.rs` | The empty-field path is the *only* one that reports no winner, and its message says "no answers" while it actually means no players |
| `a_negative_field_still_names_the_least_bad_badge` | `shared/src/lib.rs` | An all-negative round names a winner rather than nobody |
| `the_event_window_drops_answers_before_faults` | `shared/src/lib.rs` | `push_kind`'s eviction: one fault survives 48 routine answers. This is the mechanism that keeps the durable story on screen at ten badges |
| `the_event_window_evicts_the_oldest_when_nothing_is_an_answer` | `shared/src/lib.rs` | The `.unwrap_or(0)` fallback when there is no answer to sacrifice |
| `an_in_flight_badge_survives_the_thirty_second_extension` | `shared/src/lib.rs` | `latest_possible_deadline_unix_ms`, including the legacy-zero case that must not pull the ceiling below the real deadline |
| `an_unquoted_hash_without_leading_space_stays_in_the_value` | `shared/src/lib.rs` | `parse_env` comment stripping — `build#42` is a value, `value # comment` is not |
| `round_memo_totals_every_player` | `web/src/model.rs` | The `RoundMemo` aggregation had a serde-compat test but nothing checking the sums |
| `round_memo_of_an_untouched_snapshot_is_all_zero` | `web/src/model.rs` | Default path |
| `an_expired_modifier_is_not_reported_as_active` | `web/src/workflow.rs` | **FAILING** — review finding L2. See below |
| `rust_only_scheduling_falls_back_when_the_rust_pool_is_empty` | `web/src/workflow.rs` | Rust-only must refuse rather than silently deal a non-rust question, and a refused deal leaves the deck intact |
| `expire_chaos_leaves_a_live_modifier_alone` | `web/src/workflow.rs` | Expiry is inclusive of the boundary and does not clear a future modifier |
| `a_retry_on_the_same_badge_is_not_a_reassignment` | `web/src/workflow.rs` | The negative case of `is_reassignment` — same badge, higher attempt |
| `the_shipped_deck_stays_inside_the_payload_budget` | `web/src/questions.rs` | Guards W2 — serializes the real 500-question `GameInput` and asserts it stays under the 512 KB blob warning |

#### Coverage that could not be built, and why

Three gaps need a refactor before a test can exist. Each is scheduled as work rather than left implicit.

1. **Firmware has zero tests and cannot get them as structured.** `firmware/Cargo.toml` declares `[[bin]] harness = false` and firmware has no lib target, so `cargo test` has nowhere to put unit tests — and `esp-idf-svc` will not build for the host regardless. The fix is the pattern `badge-screen` already demonstrates: extract pure logic into a host-testable crate. The immediate candidate is `firmware/src/identity.rs:9`, where `factory_identity` mixes an `unsafe` MAC read with a pure `[u8; 6] -> BadgeIdentity` derivation. Split out `identity_from_mac(mac: [u8; 6])` and it is testable in isolation.

   This matters more than it looks. The callsign format is *produced* in `identity.rs:29` and *validated* in `web/src/main.rs` by `badge_worker_identity_accepts_named_and_legacy_badges_only`. Two encodings of one format, in two crates, with no test that they agree.

2. **W1 is not unit-testable as written.** `upsert_search_attributes` needs a real `WorkflowContext`; every existing `workflow.rs` test covers free functions only. Extracting the decision into a pure predicate is both the way to test it and the cleanest way to write the fix — do them together in Stage 6.

3. **F1 needs an `AppState` harness.** The `Arc<AtomicBool>` lives on state assembled in `main`. Worth building the harness as part of Stage 3 rather than leaving the High finding unpinned.

### Stage 2 — `web/` credential loading onto the SDK's `envconfig` (M1) — **[DONE — 2761511, 6a5c431]**

Hands Temporal credential resolution to `temporalio-common`'s `envconfig` feature and deletes both hand-rolled copies. Resolves F2 and the `simulate_badges.rs` half of F2b.

Two commits, per the migration draft:

1. `web/src/lib.rs` + `web/src/cloud.rs` + the `Cargo.toml` feature — the new path, unused.
2. Switch `main.rs` and `simulate_badges.rs` over, delete both old copies.

Splitting it this way keeps commit 2 a pure deletion, which is easy to review.

| File | Change |
| --- | --- |
| `web/Cargo.toml` | `temporalio-common` gains `features = ["envconfig"]` (pulls in `toml` + `dirs`) |
| `web/src/lib.rs` | **new** — 5 lines, exposes `pub mod cloud` so both binaries share it |
| `web/src/cloud.rs` | **new** — `load_profile()`, `connect()`, dotenv reader, 2 unit tests |
| `web/src/main.rs` | `connect_cloud` becomes 3 lines; `read_cloud_settings`, `parse_env_file`, `required` deleted (−66) |
| `web/src/bin/simulate_badges.rs` | same deletion (−58), same 3-line delegation |

Resolution order afterwards: `temporal.toml` profile (missing file is not an error) → repo dotenv (`.env`, else `.env.temporal`, else an explicit `TEMPORAL_ENV_FILE` **which must exist**) → real process environment, highest precedence. Steps 2 and 3 merge into one map before `load_client_config_profile`, because passing an explicit map makes the SDK read *only* that map — folding `std::env::vars()` in on top is what preserves "environment beats file".

**A `lib.rs` is required, not stylistic.** `src/bin/simulate_badges.rs` cannot reach modules declared in `main.rs` — that limitation is *why* the credential code was duplicated. `main.rs` keeps its own `mod model/questions/workflow`; only `cloud` moves.

Three traps already hit while building this, recorded so they are not re-discovered:

- `ConfigError` is not `Send + Sync` (holds `LoadError(Box<dyn Error>)`), so `?` into `anyhow::Result` will not compile. Flatten with `.map_err(|error| anyhow!("load Temporal client configuration: {error}"))`.
- `ConnectionOptions` is a `bon` typestate builder — conditionally chaining `.tls_options(...)` will not compile, since each setter returns a different type. Use the generated `maybe_tls_options(Option<TlsOptions>)` in one chain.
- Do **not** enable `envconfig` on `shared` or `firmware`. It drags in `dirs`, which exists to locate `~/.config` — a concept ESP-IDF does not have. Under resolver 2, firmware built alone never sees web's features, so the current scripts are safe; building the whole workspace for the xtensa target would unify them and pull `dirs` into the firmware graph.

### Stage 3 — F1, the remaining High (`web/src/main.rs:233`) — **[DONE — 3af1efd]**

Replace the literal `temporal_query_succeeded: true` with an `Arc<AtomicBool>` on `AppState`, set on observed query success and cleared on the resume failure path. Currently the field reports success unconditionally, so the UI cannot distinguish a working query from a broken one.

Own commit. Re-derive the line number after Stage 2, and build the `AppState` harness noted in Stage 1.

### Stage 4 — F2b-fw (`firmware/build.rs:49-53`) — **[DONE — c58ef55]**

Delete the `legacy_temporal` binding and its `TrafficLight/.env` resolution; collapse the two `config_path` calls now that both arms pass the same fallback. Leave `config_path`'s missing-file assertion alone — it is already correct.

Land close behind Stage 2 so the three copies stop drifting for good.

### Stage 5 — F5 (`web/src/workflow.rs:216`) — **[DONE]**

Replace `.expect("round summary memo update")` with an `if let Err` that pushes an `EventKind::Fault`. A failed memo update currently fails the Workflow Task and retries it forever.

Own commit — this changes Workflow behaviour on a failure path.

### Stage 6 — W1, redundant search-attribute upserts (`web/src/workflow.rs:279-287`) — **[DONE]**

The `upsert_search_attributes` call sits outside the `if !players.contains_key(...)` guard immediately above it, so every `badge_started` Signal fires it — roughly 245 per round — while `TriviaBadgeCount` changes on about 10 of those and `TriviaReassignments` only when the `is_reassignment` branch at `:225` ran. Around 225 upserts per round write values identical to what is already stored, each costing a history event **and** a visibility-store write. The visibility write is the more expensive half.

Hoist `!players.contains_key(...)` into a `joined` binding, set a `reassigned` flag in the `:225` branch, and gate the upsert on `joined || reassigned`.

Own commit, and a good one to land near Stage 5 while `workflow.rs` is already open. Extract the decision into a pure predicate so it can carry a test — see Stage 1, gap 2.

### Stage 7 — F7, the `Status` enum (`badge-screen/src/lib.rs:84` + 3 files) — **[DONE — f3b4efb]**

Introduce `pub enum Status` owning headline and instruction together; `Canvas::status` and `BadgeDisplay::show_status` take the variant; `preview` iterates `Status::ALL`.

One commit — it touches 4 files across 3 crates and **will not compile in halves**. Verified on host *and* firmware.

### Stage 8 — cheap and independent — **[DONE — 911c2ee]**

Batch freely; none of these interact.

| # | Location | Change |
| --- | --- | --- |
| FW1 | `firmware/src/main.rs:653` | Discarded `if let Ok(...)` becomes a `match` with a `consecutive_errors` counter, logging the 1st failure and every 120th |
| F4 | `web/src/main.rs:505` | `format!("WorkflowId = '{ACTIVE_WORKFLOW_ID}' ...")` instead of re-typing the ID literal |
| F8 | `badge-screen/src/lib.rs:42` | Add `PartialEq, Eq` and a hand-written `Debug` reporting shape + lit-pixel count — derive would dump 1 KiB into every failure |
| L1 | `firmware/src/main.rs:427`, `:450` | 2 clippy `collapsible_if`; `cargo clippy --fix` handles both. Only visible under the `esp` toolchain's clippy 1.97, not host 1.95 |
| W2 | `web/src/main.rs:310` | `build_deck(rand::random(), 500)` ships ~104 KB of `GameInput`; a 90-second round with ten badges consumes roughly 200. Drop to 150 — takes the input to ~32 KB and changes nothing observable |

### Stage 9 — F3, deferred but not dropped (`.cargo/config.toml:2`) — **[DONE]**

Workspace-wide `build.target` plus the `esp` override means a bare `cargo test` / `check` / `clippy` fails for everyone. Least invasive: document the host command in the root README and add `check-host.sh`. Moving `[build] target` into `firmware/.cargo/config.toml` is cleaner but changes `build-firmware.sh:41`'s `cd` and is untested against hardware.

Deferred because it is process rather than code — but it decides whether the 31 tests still run in six months, and documenting the host command is a five-minute win.

### Remaining low severity — unscheduled — **[DONE — all 11]**

Do these opportunistically, in whatever file you are already editing.

| # | Location | Note |
| --- | --- | --- |
| L2 | `web/src/workflow.rs:505` | `active_modifier` calls an expired modifier active; `active_points:489` and `rust_only:109` both compare against now. Three definitions of "active" in one file; ≤1 s window |
| L3 | `shared/src/lib.rs:358` | `target_backlog` is `10.max(players.len() * 2)`; GAME_SPEC says `max(1, active_badges - 1)`. Reachable only on replay of older history |
| L4 | `shared/src/lib.rs:21-138` | Split the game contract from the dotenv parser into `contract` + `env` modules. The parser stays — see Supersessions |
| L5 | `shared/src/lib.rs` | No `//!` |
| L6 | `badge-screen` | Sparse `///` on `WIDTH`, `HEIGHT`, `BUFFER_LEN`, most `Canvas` methods |
| L7 | `firmware/src/main.rs:711-756` | Collapse six near-identical display wrappers into one `with_display<T>(...)`. Lateral readability trade — judgement call |
| L8 | `shared/src/lib.rs:385`, `web/src/workflow.rs:470` | `correct_index` is enforced only at the serde boundary; `workflow.rs` then indexes unguarded. Currently unreachable |
| L9 | `firmware/src/session.rs:34` | `begin_game`'s `bool` return is discarded by its only caller |
| L10 | `firmware/src/main.rs:443`, `web/src/workflow.rs:439` | Magic numbers `0..45` and `selected_index > 3` |
| L11 | `badge-screen/src/bin/preview.rs:157` | `let _ = write!(...)` is infallible; prefer `.expect()` with the reason |

## Not doing

- **Continue-As-New.** Checked on 2026-08-30 and found unnecessary. A round is bounded at 90 seconds (`GAME_SECONDS` 60 plus a single-use +30s made one-shot by `chaos.extension_used`); deck exhaustion and sudden death are explicit exits; multi-round is a fresh execution under `temporal-trivia-active` with `AllowDuplicate` reuse and `Fail` on conflict (`web/src/main.rs:339-341`), which keeps each round's history, memo and UI row separate — better for the demo than a continuation chain. Revisit only if a round becomes open-ended, which is also the point where W1 and W2 stop being cosmetic.
- **Firmware onto `envconfig`.** Compile-time constants from `build.rs` remain correct: no environment, no filesystem, and `build.rs` validates all five keys at build time — a stronger guarantee than a boot-time parse.
- **`BADGE_WIFI_*` through the SDK.** Not Temporal's concern; `build.rs` keeps loading those.
- **`grpc_meta` and `codec` profile fields.** Parsed by the SDK, ignored by `connect()`. Wire up if a remote data converter or custom headers are ever needed.
- **Migrating `.env.temporal` to `temporal.toml`.** Optional follow-up — the dotenv path still works. Doing it would make the demo reproducible with plain `temporal` CLI config.

## Failed tests — **[RESOLVED — 5729cf7; suite green at 47]**

One test fails. It asserts correct behaviour against code that is wrong, so the fix belongs in the code — but that is your call, and nothing has been changed.

### `an_expired_modifier_is_not_reported_as_active`

**File:** `web/src/workflow.rs:665`
**Corresponds to:** review finding L2 (Low, 2026-08-26)

```
thread 'workflow::tests::an_expired_modifier_is_not_reported_as_active' panicked at
web/src/workflow.rs:680:9:
assertion `left == right` failed: double points expired at 1000, it is now 5000
  left: Some("double points")
 right: None
```

**What the test does.** Sets `chaos.double_points_until_unix_ms = Some(1_000)` and asks both halves of the pair about a notional now of `5_000`. `active_points` (`:489`) correctly reports 1 — it takes `now_unix_ms` and compares. `active_modifier` (`:505`) still reports `Some("double points")` — it takes no clock at all and only tests `.is_some()`.

**Why it matters beyond tidiness.** `active_modifier` is not cosmetic: `validate_apply_chaos` (`:338`) uses it to reject an operator's powerup with *"double points is already active; gameplay modifiers cannot overlap"*. `expire_chaos` clears the field once per loop tick, and the tick is `min(remaining, 1000ms)`. So for up to a second after a modifier ends, the next powerup is refused by an Update validator citing a modifier that has already expired. On stage that reads as an unresponsive button.

**Two ways to fix it, pick one:**

1. **Give `active_modifier` the clock** — `fn active_modifier(snapshot: &GameSnapshot, now_unix_ms: u64)`, comparing with `is_some_and(|until| until > now_unix_ms)` like `active_points` does. The validator has a `&WorkflowContextView` and can supply the time. Correct at any instant, and collapses two of the three definitions of "active" that L2 counted.
2. **Call `expire_chaos` at the top of the validator.** Smaller, but it treats the symptom: `active_modifier` stays wrong for any future caller, and the validator takes `&self` so it cannot mutate — this would need restructuring.

Option 1 is the real fix. It makes the test pass as written.

**If you would rather defer:** the test is accurate and should stay. Mark it `#[ignore]` with a note pointing at L2 rather than weakening the assertion, so the gap stays visible in the test output.

## Open questions

Two behaviours are now pinned by tests but were never actually decided. Neither blocks anything.

1. **Should a scoreless round have winners?** `a_scoreless_round_names_every_badge_that_joined` pins the current answer: yes, all of them. If the round timer beats the first answer, every badge that joined sits at zero, ties at the maximum, and lights up WINNER. Adding `.filter(|score| *score > 0)` to the `max()` in `GameSnapshot::finish` (`shared/src/lib.rs:343`) picks the other answer — but it would also mean a genuinely-played round ending all-negative reports NO WINNER. The test changes with the decision either way.
2. **Is `firmware/src/main.rs`'s button state machine worth extracting?** Gap 1 above wants pure firmware logic in a host-testable crate. `wait_for_choice` (`:315-405`) carries four correlated locals — `left_armed`, `right_armed`, `combo_started`, `suppress_until_release` — with illegal combinations representable; the power-up branch has to clear three of them by hand. Replacing them with a `ButtonState` enum is the same edit that would make the input logic testable off-hardware. Related to L7, which touches the same file.

## Verification

Host crates, stable toolchain (the `esp` override makes bare cargo commands fail — see Stage 9). `--no-fail-fast` matters while a test is red, or the run stops before `shared` and `badge-screen` are reached:

```
RUSTUP_TOOLCHAIN=stable cargo check  -p temporal-trivia-web --all-targets --target aarch64-apple-darwin
RUSTUP_TOOLCHAIN=stable cargo clippy -p temporal-trivia-web --all-targets --target aarch64-apple-darwin -- -D warnings
RUSTUP_TOOLCHAIN=stable cargo fmt --all -- --check
RUSTUP_TOOLCHAIN=stable cargo test --no-fail-fast -p temporal-trivia-web -p temporal-trivia-shared -p badge-screen --target aarch64-apple-darwin
```

Per package, now: `shared` 12, `badge-screen` 11, `web` 22 (21 passing).

Firmware on the `esp` toolchain (Xtensa Rust 1.97.0.0, ESP-IDF v5.4).

**Baseline before Stage 1:** 31 passing, 0 failing. **Now:** 45 tests, 44 passing, 1 failing (see [Failed tests](#failed-tests)); fmt clean, clippy 0 warnings on host. Firmware unchanged — 2 `collapsible_if`, no tests. Stage 2 adds the 2 tests in `cloud.rs` for **47**. Stages 6 and 7 should each add coverage; the rest are behaviour-preserving or already covered.

## Reference — measured history sizes (2026-08-30)

Against the real Open Trivia DB deck and the `MAX_PROMPT_CHARS` / `MAX_ANSWER_CHARS` caps at `web/src/questions.rs:11-12`.

| Payload | Size | Written |
| --- | --- | --- |
| `GameInput` — 500 questions | ~104 KB | once, `WorkflowExecutionStarted` |
| `QuestionTask` | 351 B | per Activity scheduled |
| `BadgeEvent` / `BadgeAnswer` | ~98 B | per Signal / per completion |
| `GameSnapshot` result | 5.1 KB | once, at completion |
| `RoundMemo` | 195 B | once |

A heavy round — ten badges, the full 90 seconds, ~245 Activities — lands around **0.5-0.6 MB** once protobuf event overhead is added (`ActivityTaskScheduled` carries the retry policy, four timeouts, Task Queue and Activity id, which outweigh the 351-byte payload). Standard limits are 10 MB warn / 50 MB error, so that is roughly 5% of the warning. `GameInput` is the only payload within an order of magnitude of any limit — 104 KB against a 512 KB blob warning — which is what W2 addresses.

## Reference — checked and found correct

Stated so the absence of findings reads as a result. Both `unsafe` blocks name real invariants and `power.rs` routes every ESP-IDF code through `check()`; no `MutexGuard` crosses an `.await` in firmware; the `u64` subtraction at `workflow.rs:173` is guarded by the `break` at `:106` with no intervening yield; `wrong[0..2]` at `questions.rs:153` is guarded by the `len() != 3` filter at `:135`; `start_game` reserves the single-game slot before the Cloud call and releases on error. `unwrap` in tests: correct, not reported. No unsoundness, no UB, no data race.

`firmware/vendor/` is a live `[patch.crates-io]` source, not a reference copy — root `Cargo.toml:18-22`. The `envconfig` module read while drafting M1 is exactly the code that compiles.

## Reference — environment state (outside the repo)

- ESP-IDF venv forced to build from `/usr/bin/python3` (3.9.6) — asdf Python 3.13.13 lacks `_lzma`.
- Inside the gitignored `.embuild/.../idf5.4_py3.9_env` venv only: pip downgraded to 23.3.2, `ruamel.yaml==0.17.32` / `ruamel.yaml.clib==0.2.8` pinned (pip 26's PEP 503 dist-info names break IDF 5.4's checker).
- Gitignored `target/` and `.embuild/` (ESP-IDF + toolchains), left in a working state.

## Still open

Nothing outstanding from this plan.

`firmware/` has no test harness of its own and still cannot get one — `[[bin]] harness = false`, no lib target, and `esp-idf-svc` will not build for a host. That is now a property of the crate rather than a coverage gap: what remains inside it is genuinely device-bound (efuse, I2C, LEDC, NVS, the ESP-IDF event loop, the Temporal Worker wiring). Every piece of decision logic that used to be trapped there has moved somewhere with a harness:

| Was | Now | Tests |
| --- | --- | --- |
| screens | `badge-screen` | 13 |
| callsign derivation | `shared::identity` | 4 of 17 |
| button gestures | `badge-input` | 11 |

## Resolved since

- **Scoreless-round winners** — decided: a scoreless round is a tie between every badge that joined, not a void. `4a89fdb` states the rule in `finish()` and adds a non-zero-tie test; the empty-roster message now says "no badges", which is the condition it was actually reporting.
- **F1 had no test** — `5324426` introduces the `SnapshotQuery` trait as a seam below `AppState`, so the health flag's transitions run against a fake instead of a live `Client`. It also names the distinction the two call sites only had implicitly, as `OnQueryFailure::{Retract, Keep}`.
- **`identity_from_mac` was unreachable from a host test** — `e9713d2`.
- **The button state machine** — `e43abf0`. Modelling it surfaced a real defect, described below.

## Bug found while refactoring

**A fast roll between the two side buttons wedged the badge's side answers.** In `wait_for_choice`'s four-boolean form, one sample showing exactly one side down followed by the very next showing exactly the other left both `left_armed` and `right_armed` set. From then on neither release could answer, because each check required the other side to be clear — the badge stopped accepting LEFT and RIGHT for the rest of that question.

Established by transcribing the original algorithm from git history and running it, not inferred from the replacement. An exhaustive search over side-button sequences up to length five gives the precise shape:

- **Symmetric.** Both `LR` and `RL` wedge. Every wedging sequence ends with the same single-sample crossover; nothing slower reaches it.
- **Narrow trigger.** If any tick sees neither button, the release answers normally and nothing stays armed. If any tick sees both, it is the combo branch. So the crossover has to complete inside one 20 ms window — a fast thumb-roll, or contact bounce, since `BadgeInput::sample()` reads the pins raw with no debouncing.
- **Recoverable.** A later both-button press hits the combo branch, whose `combo.take()` clears both flags. That is also the panic gesture, so the escape hatch was "simulate a crash."

`Armed(Side)` cannot represent the state: the side still held supersedes the one armed before, which at a 20 ms sample is what a roll means. Three tests pin it — both directions, and the caught-gap case that bounds reachability.

Real, but not routine: it needed the roll to land inside a single sample, so it is unlikely to have been mistaken for a quiet player on stage.
