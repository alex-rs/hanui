//! Home Assistant integration boundary.
//!
//! Phase 1: entity types and the `EntityStore` trait — see TASK-006 and TASK-007.
//! Phase 2 adds `LiveStore`, the WebSocket client, and live state subscriptions.
//! This module never performs network I/O in Phase 1.

pub mod client;
pub mod entity;
pub mod fixture;
pub mod protocol;
pub mod services;
pub mod store;
