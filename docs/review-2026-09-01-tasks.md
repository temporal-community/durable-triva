# Review remediation — 2026-09-01

Source: full-project review at `b0979d6`. Tracks the decision to remove the
phone player path and to fix every remaining finding.

## Decision: the phone path is removed

The public phone service, its Cloud Run Worker Pool, the phone UI and the
phone half of the game contract are deleted rather than repaired. Four of the
review's findings (P0 answer spoofing, the broken 6 s crash blackout, the
permanent join lockout, and the leak-or-fail load-test flag) were all in that
surface, and it was the only publicly reachable one. Badges — physical and
simulated — remain the whole game.

Replay note: badge-only histories are unaffected. Phone Activities were only
ever scheduled when `registered_phone_count > 0` or `detected_badge_count == 0`,
so a badge-only history records the same command sequence before and after this
change. Histories from phone rounds are deliberately abandoned.

## Tasks

| # | Pri | Task | Status |
|---|---|---|---|
| R0 | — | Remove the phone path end to end | done |
| T2 | P1 | Rust-only chaos must not starve the field; refill on eligibility, behind a patch marker | done |
| T6 | P1 | Tolerate transient Activity heartbeat RPC failures inside the 15 s budget | done |
| T7 | P2 | Refuse an abandoned question before drawing it or touching Cloud/NVS | done |
| T8 | P2 | Give the badge display a single owner so a live question cannot be erased | done |
| T9 | P2 | Gate the USB HIL protocol behind a non-default cargo feature | done |
| T12 | P2 | `flash-badge.sh` must use `--no-skip` | done |
| T13 | P2 | Document that a flashed badge carries an extractable namespace key | done |
| T14 | P2 | HIL runner must not depend on a boot-only log line | done |
| T15 | P2 | Bound the OLED I2C timeout so a stuck bus cannot wedge the Worker | done |
| T16 | P3 | Delete dead `GameWorkflow::questions` state | done |
| T17 | P3 | Fix the broken `#shared-temporal-configuration` doc anchors | done |
| T18 | P3 | Make three misnamed tests prove what they claim | done |
| T20 | P3 | `no-store` on `/api/state` and `/api/history` | done |
| T21 | P3 | Drop the unused `temporalio-workflow` dependency | withdrawn, see below |
| T22 | P3 | Fix `config_path`'s duplicated argument | done |
| T23 | P3 | Consolidate duplicated `unix_ms` / Workflow stubs | done |
| T24 | P3 | Add the missing `AGENTS.md` | done |

Two further fixes came out of hardware acceptance rather than the review, and
are recorded in `blog.md`: the runner and the TV attract roster both needed the
controller's new `GET /api/badges` (the firmware's own `polling=` flag only
means the Worker task started, while the round is sized from Temporal's poller
list), and the result watcher needed a bounded retry on its restore after the
new display-ownership check let it skip and never try again.

T5 (phone launch scripts missing `--target`) and T10/T11/T19 are resolved by R0:
the files they lived in no longer exist.

**T21 was a bad finding and is withdrawn.** `temporalio-workflow` looks unused
to grep because nothing names it — the `#[workflow]` and `#[workflow_methods]`
macros expand to paths rooted at `::temporalio_workflow`. Removing it broke both
crates immediately; it is restored in `web/Cargo.toml` and `firmware/Cargo.toml`
with a comment so nobody deletes it again. `qrcode` and `reqwest` were genuinely
unused once the phone binaries were gone and stay removed.

## Demo readiness — 2026-09-02

Both badges run the default image and were measured over 20 rounds with no
serial port held open. Six rounds saw a badge drop and rejoin unattended; one
produced a genuine heartbeat timeout and reassignment. No badge failed
permanently or needed a power cycle.

The residual fault is a bare `BREAK` with a corrupted backtrace, no panic
message, no allocation failure, and no `abort()` line. It is not a Rust panic
(the allocation-free panic hook never fires), not a Rust allocation failure
(the allocation reporter never fires), not stack overflow in our threads
(measured headroom: UI 11.7K of 16K, main 40K of 64K), not a blocking-thread
leak, not USB Serial/JTAG, and not one bad board. The single fully decoded
instance was newlib `lock_init_generic` failing to create a mutex and calling
`abort()` — a C-side allocation Rust's hooks cannot observe, consistent with
internal-DRAM pressure.

It is bounded rather than fixed: cost is under one round of absence, it is
visible on the OLED while it recovers, and a mid-question drop is what the
Workflow reassignment path exists for. Starting a round while a badge is still
booting is the only operator-facing consequence, and `README.md` documents
waiting for the attract roster.
