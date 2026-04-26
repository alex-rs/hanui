//! Shared test utilities used by multiple test binaries.
//!
//! # Contents
//!
//! - [`mock_ws`] — the canonical mock Home Assistant WebSocket server.  This is
//!   the **superset** copy of `tests/integration/mock_ws.rs` with the
//!   `force_disconnect` extension needed by TASK-039's memory-soak burst
//!   scenario.  `tests/integration/` continues to use its own copy so the
//!   integration test binary has no dependency on this module.

pub mod mock_ws;
