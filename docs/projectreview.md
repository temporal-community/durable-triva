# Project Review Log

Rolling log of code reviews for temporal-trivia-badge. Newest findings first.

---

## 2026-08-30 — Workflow history size review

**Reviewer:** conversational, prompted by "do we have anything in the workflow for continue-as-new?" and a follow-up on history size in bytes rather than event count.
**Scope:** `web/src/workflow.rs` history growth, payload sizes measured against the real Open Trivia DB deck and the `MAX_PROMPT_CHARS` / `MAX_ANSWER_CHARS` caps in `web/src/questions.rs:11-12`.

### Findings

| # | Severity | Location | Fix | Status |
|---|---|---|---|---|
| W1 | Medium | `web/src/workflow.rs:279-287` | The `upsert_search_attributes` call sits outside the `if !players.contains_key(...)` guard above it, so every `badge_started` Signal fires it — ~245 per round — while `TriviaBadgeCount` changes on ~10 and `TriviaReassignments` only when the `is_reassignment` branch at `:225` ran. ~225 upserts per round write values identical to what is already there, each costing a history event *and* a visibility-store write. Hoist `!players.contains_key(...)` into a `joined` binding, set a `reassigned` flag in the `:225` branch, and gate the upsert on `joined \|\| reassigned` | UNVERIFIED |
| W2 | Low | `web/src/main.rs:310` | `build_deck(rand::random(), 500)` passes all 500 questions as `GameInput`, ~104 KB in `WorkflowExecutionStarted`. A 90-second round with ten badges can consume roughly 200 — a badge needs a few seconds to read and press — so over half is dealt to a Workflow that never reads it and sits in history for the life of the execution. It is also the only payload within one order of magnitude of a limit (512 KB blob warning). A 150-question deck takes the input to ~32 KB and changes nothing observable | UNVERIFIED |

### Measured payload sizes

| Payload | Size | Written |
|---|---|---|
| `GameInput` — 500 questions | ~104 KB | once, `WorkflowExecutionStarted` |
| `QuestionTask` | 351 B | per Activity scheduled |
| `BadgeEvent` / `BadgeAnswer` | ~98 B | per Signal / per completion |
| `GameSnapshot` result | 5.1 KB | once, at completion |
| `RoundMemo` | 195 B | once |

Heavy round — ten badges, the full 90 seconds, ~245 Activities — lands around **0.5-0.6 MB** of history once protobuf event overhead is added (the `ActivityTaskScheduled` attributes carry the retry policy, four timeouts, Task Queue and Activity id, which outweigh the 351-byte payload). Against standard limits of 10 MB warn / 50 MB error on history size, that is ~5% of the warning.

### Continue-As-New — checked and found unnecessary

No Continue-As-New anywhere in the Workflow; the only occurrence in the project is a trivia question about it at `web/src/questions.rs:572`. It is not needed, and the reasons are structural rather than incidental:

- The round is bounded at 90 seconds — `GAME_SECONDS` 60 plus a single-use +30s extension, made one-shot by `chaos.extension_used`. The loop breaks on `now_unix_ms >= deadline_unix_ms` at `web/src/workflow.rs:104`.
- Deck exhaustion is an explicit exit at `:163`; sudden death breaks on the first correct answer.
- Multi-round is handled by starting a fresh execution under the fixed ID `temporal-trivia-active` with `AllowDuplicate` reuse and `Fail` on conflict (`web/src/main.rs:339-341`). Each round keeps its own history, its own `TriviaRoundSummary` memo, and its own row in the UI — better for the demo than collapsing rounds into a continuation chain.

Revisit only if a round ever becomes open-ended (a session cycling decks all conference-day), which is also the scenario where W1 and W2 stop being cosmetic.

---

## 2026-08-26 — Rust review (whole workspace)

**Reviewer:** `rust-code-reviewer` agent, against The Rust Book, the Rust API Guidelines, and clippy.
**Scope:** all first-party `.rs` in `web/`, `shared/`, `badge-screen/`, `firmware/`, plus every `Cargo.toml`, `build.rs`, `rust-toolchain.toml`, `.cargo/config.toml`. Excluded `firmware/vendor/` (vendored Temporal Rust SDK 1.0.0, Core 0.9.0), `web/static/index.html`, and the Open Trivia DB snapshot.

### Baseline before any change

| Check | web / shared / badge-screen | firmware |
|---|---|---|
| `cargo fmt --all -- --check` | clean | clean |
| `cargo check --all-targets` | clean | clean |
| `cargo clippy -- -D warnings` | 0 warnings | 2 (`collapsible_if`) |
| `cargo test` | 31 passing, 0 failing | no tests |

Host crates checked with `RUSTUP_TOOLCHAIN=stable cargo ... --target aarch64-apple-darwin`.
Firmware checked on the `esp` toolchain (Xtensa Rust 1.97.0.0) against ESP-IDF v5.4 — firmware findings are compiled, not inferred.

**Totals:** 2 High, 6 Medium, 11 Low. No Critical. No unsoundness, no UB, no data race, no `MutexGuard` held across an `.await`. Both `unsafe` blocks carry accurate `// SAFETY:` comments.

### Fixes — verified

Each was applied, compiled, clippy'd, tested, then reverted; the baseline was reconfirmed after every one. Diffs live in the session scratchpad (`fix-*.diff`).

| # | Severity | Location | Fix | Status |
|---|---|---|---|---|
| F1 | High | `web/src/main.rs:233` | Replace the literal `temporal_query_succeeded: true` with an `Arc<AtomicBool>` on `AppState`, set on observed query success and cleared on the resume failure path | VERIFIED |
| F2 | High | `web/src/main.rs:662` | Drop the `../../TrafficLight/.env` fallback; make an explicit `TEMPORAL_ENV_FILE` binding — `bail!` when it names a missing file | VERIFIED |
| F5 | Medium | `web/src/workflow.rs:216` | Replace `.expect("round summary memo update")` with an `if let Err` that pushes an `EventKind::Fault` instead of failing the Workflow Task | VERIFIED |
| F7 | Medium | `badge-screen/src/lib.rs:84` + 3 files | Introduce `pub enum Status` owning headline and instruction together; `Canvas::status` and `BadgeDisplay::show_status` take the variant; preview iterates `Status::ALL` | VERIFIED (host + firmware) |
| FW1 | Medium | `firmware/src/main.rs:653` | Convert the discarded `if let Ok(...)` to a `match` with a `consecutive_errors` counter, logging the 1st failure and every 120th | VERIFIED (firmware) |
| F4 | Medium | `web/src/main.rs:505` | Use `format!("WorkflowId = '{ACTIVE_WORKFLOW_ID}' ...")` instead of re-typing the ID literal | VERIFIED |
| F8 | Medium | `badge-screen/src/lib.rs:42` | Add `PartialEq, Eq` and a hand-written `Debug` reporting shape + lit-pixel count (derive would dump 1 KiB into failures) | VERIFIED |

### Fixes — not compiled

| # | Severity | Location | Fix | Status |
|---|---|---|---|---|
| F2b | High | `web/src/bin/simulate_badges.rs:198`, `firmware/build.rs:49` | Same `TEMPORAL_ENV_FILE` correction; remove the `TrafficLight` branch. Mechanical, but not built — land it with F2 so the three copies stop drifting | UNVERIFIED |
| F3 | Medium | `.cargo/config.toml:2` | Workspace-wide `build.target` + `esp` override means bare `cargo test`/`check`/`clippy` fails for everyone. Least invasive: document the host command in the root README and add `check-host.sh`. Moving `[build] target` into `firmware/.cargo/config.toml` is cleaner but changes `build-firmware.sh:41`'s `cd`, untested against hardware | UNVERIFIED (config/process, not code) |
| L7 | Low | `firmware/src/main.rs:711-756` | Collapse six near-identical display wrappers into one `with_display<T>(...)` helper. Lateral readability trade | UNVERIFIED |

### Low severity — batched

1. 2 clippy `collapsible_if` warnings, `firmware/src/main.rs:427` and `:450` — `cargo clippy --fix` handles both. Only visible under the `esp` toolchain's clippy 1.97, not host 1.95.
2. `active_modifier` (`web/src/workflow.rs:505`) calls an expired modifier active; `active_points:489` and `rust_only:109` both compare against now. Three definitions of "active" in one file; ≤1 s window.
3. `GameSnapshot::target_backlog` default (`shared/src/lib.rs:358`) is `10.max(players.len() * 2)`; GAME_SPEC says `max(1, active_badges - 1)`. Reachable only on replay of older history.
4. `shared/src/lib.rs` mixes the game contract with a dotenv parser (lines 21-138) — wants `contract` and `env` modules.
5. No `//!` on `shared`.
6. Sparse `///` on `badge-screen`'s public API (`WIDTH`, `HEIGHT`, `BUFFER_LEN`, most `Canvas` methods).
7. See L7 above.
8. `Question`'s `correct_index` invariant is enforced only at the serde boundary (`shared/src/lib.rs:385`); `web/src/workflow.rs:470` then indexes unguarded. Currently unreachable.
9. `session.begin_game`'s `bool` return (`firmware/src/session.rs:34`) is discarded by its only caller.
10. Magic numbers: `0..45` (`firmware/src/main.rs:443`), `selected_index > 3` (`web/src/workflow.rs:439`).
11. `let _ = write!(...)` (`badge-screen/src/bin/preview.rs:157`) — infallible; prefer `.expect()` with the reason.

### Suggested order

1. **F1 + F2 together** — independent small edits to `web/src/main.rs`; include F2b in the same commit.
2. **F5 alone** — changes Workflow behaviour on a failure path, deserves its own commit.
3. **F7 as one commit** — touches 4 files across 3 crates, will not compile in halves.
4. **FW1, F4, F8, and `clippy --fix`** — cheap and independent.
5. **F3 deferred, not dropped** — it decides whether the 31 tests still run in six months. Documenting the host command is a five-minute win.

### Checked and found correct

Stated so the absence of findings reads as a result: both `unsafe` blocks name real invariants and `power.rs` routes every ESP-IDF code through `check()`; no `MutexGuard` crosses an `.await` in firmware; the `u64` subtraction at `workflow.rs:173` is guarded by the `break` at :106 with no intervening yield; `wrong[0..2]` at `questions.rs:153` is guarded by the `len() != 3` filter at :135; `start_game` reserves the single-game slot before the Cloud call and releases on error. `unwrap` in tests: correct, not reported.

### Environment changes made outside the repo

- ESP-IDF venv forced to build from `/usr/bin/python3` (3.9.6) — asdf Python 3.13.13 lacks `_lzma`.
- Inside the gitignored `.embuild/.../idf5.4_py3.9_env` venv only: pip downgraded to 23.3.2, `ruamel.yaml==0.17.32` / `ruamel.yaml.clib==0.2.8` pinned (pip 26's PEP 503 dist-info names break IDF 5.4's checker).
- New gitignored dirs: `target/` and `.embuild/` (ESP-IDF + toolchains), left in a working state.

**Working tree unmodified.** All 8 edited files restored from snapshots, byte-identical by SHA-1; `git status --porcelain` empty; full baseline re-run matches.
