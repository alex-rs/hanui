//! SBC smoke tests for Phase 2.
//!
//! This module is the top-level entry for the `sbc_smoke` integration test
//! binary (see `Cargo.toml` `[[test]]` entry).  Sub-modules contain scenario
//! tests gated on aarch64 QEMU emulation performance.
//!
//! The mock WS harness lives in `tests/common/mock_ws.rs` (the single canonical
//! location post TASK-042).  Tests in this module MUST import `mock_ws` from
//! the `tests/common/` tree — a second mock WS implementation is forbidden.

// mock_ws is included from the canonical TASK-042 common harness.
// The sbc_smoke binary uses only a subset of MockWsServer's API; the remaining
// methods (script_auth_invalid, inject_auth_required, recorded_requests,
// recorded_request_count, force_disconnect) are exercised by the
// integration_tests and soak_tests binaries.
// #[expect(dead_code)] is used here so the compiler will warn if mock_ws.rs
// is ever refactored to remove these methods and this annotation can be dropped.
// Unlike the forbidden #[allow(…)] attribute, #[expect(…)] produces a warning
// when the suppressed lint no longer fires — making it self-cleaning.
#[path = "../common/mock_ws.rs"]
#[expect(dead_code)]
mod mock_ws;

mod sbc_cpu;
