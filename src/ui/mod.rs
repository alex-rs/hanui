//! UI bridge between the typed data layer and the Slint component tree.
//!
//! Phase 1: `bridge.rs` maps `EntityStore` + `Dashboard` to typed Slint view
//! models — see TASK-011a and TASK-011b. No `serde_json::Value` access is
//! permitted anywhere in this module (TASK-013a adds the CI grep gate).
