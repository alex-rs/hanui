//! Shared test utilities used by every test binary and bench in the workspace.
//!
//! # Contents
//!
//! - [`mock_ws`] — the **single canonical** mock Home Assistant WebSocket server.
//!   This is the only mock WS implementation in the repo (TASK-042 unification);
//!   integration, soak, smoke and bench targets all import from this module.

pub mod mock_ws;
