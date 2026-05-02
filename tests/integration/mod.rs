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

// Re-expose the headless Slint harness (TASK-074) so the gesture-layer tests
// (TASK-060) can construct a `HeadlessRenderer` without spinning up a window
// system. The harness's own smoke test runs as a separate `[[test]]` target
// (`slint_harness_smoke`); here we only consume its API. Smoke-test items
// inside the file are gated on `#[cfg(test)]`, which is true for this
// integration binary as well.
#[path = "../common/slint_harness.rs"]
#[expect(dead_code)]
pub mod slint_harness;

pub mod actions_protocol;
pub mod actions_ui;
pub mod camera_pool;
pub mod command_tx;
pub mod gesture_layer;
pub mod lagged_resync;
pub mod layout;
pub mod loader;
pub mod more_info_modal;
pub mod offline_queue;
pub mod power_flow;
pub mod schema_lock;
pub mod toast_spinner;
pub mod url_action;
pub mod validation;
pub mod view_router;
pub mod ws_client;
