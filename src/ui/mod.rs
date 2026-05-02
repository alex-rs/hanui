//! UI bridge between the typed data layer and the Slint component tree.
//!
//! Phase 1: `bridge.rs` maps `EntityStore` + `Dashboard` to typed Slint view
//! models — see TASK-011a and TASK-011b. Raw JSON value types must not appear
//! anywhere in this module; TASK-013a adds the CI grep gate that enforces this.
//!
//! Phase 3: [`view_router`] adds the [`view_router::ViewRouter`] trait and the
//! production [`view_router::SlintViewRouter`] impl that drives the
//! `ViewRouterGlobal::current-view` Slint property from the dispatcher's
//! `Action::Navigate { view_id }` payload (TASK-068, single-view stub).
//!
//! Phase 3: [`toast`] adds the toast banner state + driver (TASK-067) and the
//! per-tile spinner update path that binds to
//! [`crate::ha::live_store::LiveStore::pending_for_widget`].

pub mod alarm;
pub mod bridge;
pub mod camera;
pub mod cover;
pub mod fan;
pub mod history_graph;
pub mod lock;
pub mod more_info;
pub mod toast;
pub mod view_router;
