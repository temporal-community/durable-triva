//! The contract every part of the trivia badge demo agrees on.
//!
//! The firmware, the Mac Worker and the TV page are three separate binaries
//! on two architectures; this crate is the only thing all of them compile.
//! It deliberately depends on nothing but `serde`, because the badge builds
//! for `xtensa-esp32s3-espidf` and anything heavier would not follow it there.
//!
//! - [`contract`] is the wire format carried through Temporal History.
//! - [`env`] is the dotenv reader `firmware/build.rs` uses at build time.
//! - [`identity`] is how a badge names itself, and how the controller
//!   recognises one.
//!
//! Both are re-exported at the crate root, so `temporal_trivia_shared::Question`
//! and `temporal_trivia_shared::contract::Question` are the same type.

pub mod contract;
pub mod env;
pub mod identity;

pub use contract::*;
pub use env::{EnvParseError, parse_env};
pub use identity::{BadgeIdentity, identity_from_mac, is_badge_worker_identity};
