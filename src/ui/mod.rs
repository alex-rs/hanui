//! UI bridge between the typed data layer and the Slint component tree.
//!
//! Phase 1: `bridge.rs` maps `EntityStore` + `Dashboard` to typed Slint view
//! models — see TASK-011a and TASK-011b. Raw JSON value types must not appear
//! anywhere in this module; TASK-013a adds the CI grep gate that enforces this.
