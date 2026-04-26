//! Integration test crate for Phase 2 mock WS server tests.
//!
//! This module is the entry point for the `ws_integration_tests` integration
//! test binary (see `tests/integration_tests.rs`).  Sub-modules contain the
//! scenario tests for TASK-035 and TASK-036.
//!
//! The mock WS harness lives in `tests/common/mock_ws.rs` (the single canonical
//! location post TASK-042).  It is included here via `#[path]` so the existing
//! sub-modules can keep referring to `super::mock_ws::*` without each one
//! carrying its own `#[path]` directive.

// Re-expose the canonical mock as `mock_ws` inside this module tree so both
// `ws_client.rs` and `lagged_resync.rs` can use `super::mock_ws::*`.  The
// integration binary uses most but not every public method on `MockWsServer`
// — `force_disconnect` (and its backing `disconnect_flag` field) is only
// exercised by the soak binary.  `#[expect(dead_code)]` here suppresses the
// unused-warnings inside the integration binary while staying self-cleaning:
// if the integration tree ever ends up exercising every API, the compiler
// will warn that the expectation is unfulfilled and the annotation can be
// removed.
#[path = "../common/mock_ws.rs"]
#[expect(dead_code)]
pub mod mock_ws;

pub mod lagged_resync;
pub mod ws_client;
