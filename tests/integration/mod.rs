//! Integration test crate for Phase 2 mock WS server tests.
//!
//! This module is the entry point for the `ws_integration_tests` integration
//! test binary (see `tests/integration_tests.rs`).  Sub-modules contain the
//! canonical mock WS harness and the scenario tests for TASK-035 and TASK-036.

pub mod lagged_resync;
pub mod mock_ws;
pub mod ws_client;
