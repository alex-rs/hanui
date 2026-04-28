//! Shared test utilities used by every test binary and bench in the workspace.
//!
//! # Contents
//!
//! - [`mock_ws`] — the **single canonical** mock Home Assistant WebSocket server.
//!   This is the only mock WS implementation in the repo (TASK-042 unification);
//!   integration, soak, smoke and bench targets all import from this module.
//! - [`slint_harness`] — headless Slint rendering harness (TASK-074). Wraps
//!   `MinimalSoftwareWindow` + a custom platform so integration tests can
//!   capture a `Rgba8` pixel buffer from any `slint::ComponentHandle` without
//!   spawning a real window. Consumed by TASK-073 golden-frame tests.

pub mod mock_ws;
pub mod slint_harness;
