# Migration draft — `web/` credential loading onto the SDK's `envconfig`

**Date:** 2026-08-27
**Status:** drafted and compiled in a scratch copy of the workspace. Nothing in the repo was modified.
**Patch:** `web-envconfig-migration.diff` (5 files, +164 / −141) — apply from the repo root with `git apply`.

## Why

`web/src/main.rs:662` and `web/src/bin/simulate_badges.rs:193` each carried their own copy of Temporal credential resolution, layered on the hand-rolled dotenv parser in `shared/src/lib.rs:21-138`. That duplication is what let the two copies drift, and it produced High finding **F2** in the 2026-08-26 review: a fallback to `~/Projects/TrafficLight/.env`, and a `TEMPORAL_ENV_FILE` pointing at a missing file being silently ignored.

`temporalio-common` already ships the resolution logic, behind a non-default `envconfig` feature. It reads the same `TEMPORAL_*` names this project already chose, plus the full TLS/codec/gRPC-metadata surface, and it supports `temporal.toml` profiles selected by `TEMPORAL_PROFILE` — the same convention as the `temporal` CLI.

This migration hands resolution to the SDK and deletes both copies of the hand-rolled version.

## What changed

| File | Change |
|---|---|
| `web/Cargo.toml` | `temporalio-common` gains `features = ["envconfig"]` (pulls in `toml` + `dirs`) |
| `web/src/lib.rs` | **new** — 5 lines, exposes `pub mod cloud` so both binaries can share it |
| `web/src/cloud.rs` | **new** — `load_profile()`, `connect()`, dotenv reader, 2 unit tests |
| `web/src/main.rs` | `connect_cloud` becomes 3 lines delegating to `cloud`; `read_cloud_settings`, `parse_env_file`, `required` deleted (−66 lines) |
| `web/src/bin/simulate_badges.rs` | same deletion (−58 lines), same 3-line delegation |

### Why a `lib.rs` was needed

`src/bin/simulate_badges.rs` cannot reach modules declared in `main.rs` — that limitation is *why* the credential code was duplicated in the first place. A minimal lib target is the smallest fix that lets one implementation serve both binaries. `main.rs` keeps its own `mod model/questions/workflow` as before; only `cloud` moved into the lib.

### Resolution order after the change

1. `temporal.toml` profile — located via `config_source`, then `TEMPORAL_CONFIG_FILE`, then the platform default path; profile chosen by `TEMPORAL_PROFILE`, else `default`. A missing file is not an error.
2. The repo's dotenv file (`.env`, else `.env.temporal`, or an explicit `TEMPORAL_ENV_FILE` **which must exist**).
3. Real process environment variables — highest precedence, matching previous behaviour.

Steps 2 and 3 are merged into one map and handed to `load_client_config_profile` as its `env_vars` argument. The merge has to happen on our side: passing an explicit map makes the SDK read *only* that map, so folding `std::env::vars()` in on top is what preserves "environment beats file".

### F2 is resolved rather than patched

The `TrafficLight` fallback is gone, and an explicit `TEMPORAL_ENV_FILE` naming a missing file now fails with `TEMPORAL_ENV_FILE points to missing file <path>` — which is what `firmware/build.rs:28` has always asserted. A unit test covers it.

## Verification

Run in a scratch copy at `<scratchpad>/migration-web`, host target, stable toolchain:

```
RUSTUP_TOOLCHAIN=stable cargo check   -p temporal-trivia-web --all-targets --target aarch64-apple-darwin   → clean
RUSTUP_TOOLCHAIN=stable cargo clippy  -p temporal-trivia-web --all-targets --target aarch64-apple-darwin -- -D warnings   → exit 0
RUSTUP_TOOLCHAIN=stable cargo fmt --all -- --check   → clean
RUSTUP_TOOLCHAIN=stable cargo test -p temporal-trivia-web -p temporal-trivia-shared -p badge-screen --target aarch64-apple-darwin   → 33 passed, 0 failed
```

33 = the 31 pre-existing tests plus the 2 new ones in `cloud.rs`.

## Gotchas found while building it

1. **`ConfigError` is not `Send + Sync`.** It holds `LoadError(Box<dyn std::error::Error>)`, so `?` into an `anyhow::Result` does not compile. The draft flattens it with `.map_err(|error| anyhow!("load Temporal client configuration: {error}"))`.
2. **`ConnectionOptions` is a `bon` typestate builder.** Conditionally chaining `.tls_options(...)` will not compile because each setter returns a different type. Use the generated `maybe_tls_options(Option<TlsOptions>)` in a single chain.
3. **`shared`'s dotenv parser must stay.** `firmware/build.rs:18` calls `temporal_trivia_shared::parse_env` at build time, and the badge has no environment to read instead. This migration therefore does **not** delete that parser — an earlier assumption that it would was wrong. Review Low finding #4 (splitting `shared` into `contract` + `env` modules) is still worth doing, separately.
4. **Do not enable `envconfig` on `shared` or `firmware`.** The feature drags in `dirs`, which exists to locate `~/.config` — a concept ESP-IDF does not have. Under resolver 2, firmware built alone (`-p temporal-trivia-badge-firmware`, xtensa target) never sees web's features, so the current build scripts are safe. Building the entire workspace for the xtensa target would unify them and pull `dirs` into the firmware graph.
5. **`firmware/vendor/` is a live `[patch.crates-io]` source**, not a reference copy — root `Cargo.toml:18-22`. The `envconfig` module being read here is exactly the code that compiles.

## Not included

- **Firmware.** Compile-time constants from `build.rs` remain the right answer: no environment, no filesystem, and `build.rs` validates all five keys at build time — a stronger guarantee than a boot-time parse.
- **`BADGE_WIFI_*`.** Not Temporal's concern; `build.rs` keeps loading those itself.
- **`grpc_meta` and `codec` profile fields.** Parsed by the SDK, ignored by `connect()`. Wire them up if a remote data converter or custom headers are ever needed.
- **Migrating `.env.temporal` to `temporal.toml`.** Optional follow-up. The dotenv path still works, so this can wait; doing it would make the demo reproducible with plain `temporal` CLI config.

## Suggested commits

1. `web/src/lib.rs` + `web/src/cloud.rs` + `Cargo.toml` feature — the new path, unused.
2. Switch `main.rs` and `simulate_badges.rs` over, delete both old copies.

Splitting it this way keeps step 2 a pure deletion, which is easy to review.
