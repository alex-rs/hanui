//! Memory soak test binary entry point (TASK-039).
//!
//! Feature-gated under `#[cfg(feature = "soak")]`.
//! Documented as nightly-only: run via `cargo test --features soak --test soak_tests`.
//!
//! See [`memory`] for the scenario description and assertions.

#[cfg(feature = "soak")]
pub mod memory;
