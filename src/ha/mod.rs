//! Home Assistant integration boundary.
//!
//! Phase 1: entity types and the `EntityStore` trait — see TASK-006 and TASK-007.
//! Phase 2 adds `LiveStore`, the WebSocket client, and live state subscriptions.
//! Phase 6.0 adds `http` — the shared HTTP layer for REST API access (TASK-097).
//! Phase 6 Wave 2 adds `history` — REST history fetch + LTTB downsampling (TASK-106).
//! Phase 6 Wave 2 adds `camera` — bounded decoder pool for snapshot fetches (TASK-107).
//! This module never performs network I/O in Phase 1.

pub mod camera;
pub mod client;
pub mod entity;
pub mod fixture;
pub mod history;
pub mod http;
pub mod live_store;
pub mod protocol;
pub mod services;
pub mod store;
