//! Shared internals for the operator-side binaries.
//!
//! `src/bin/simulate_badges.rs` cannot reach modules declared in `main.rs`, so
//! anything both binaries need lives here instead of being copied into each.

pub mod cloud;
