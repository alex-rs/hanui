//! Top-level integration test binary that pulls in `tests/integration/`.
//!
//! Cargo treats every `.rs` file directly under `tests/` as its own integration
//! test binary.  We use a single binary (`integration_tests`) that includes the
//! `integration` module; sub-modules live under `tests/integration/` so the
//! mock WS harness can be reused by future test files (TASK-038, TASK-039,
//! TASK-040).
//!
//! See `tests/integration/mod.rs` for the module index.

#[path = "integration/mod.rs"]
mod integration;
