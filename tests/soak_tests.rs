//! Top-level soak test binary that pulls in `tests/soak/`.
//!
//! Feature-gated: this binary only compiles useful tests when
//! `--features soak` is passed.  Without the feature the binary
//! compiles but contains no test functions.
//!
//! Run the soak suite:
//! ```sh
//! cargo test --features soak --test soak_tests -- --nocapture
//! ```
//!
//! See `tests/soak/memory.rs` for the 10-minute memory soak scenario.

#[path = "common/mod.rs"]
mod common;

#[path = "soak/mod.rs"]
mod soak;
