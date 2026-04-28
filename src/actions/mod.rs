//! Phase 3 action types and dispatcher (write/command path).
//!
//! This module is the home of the typed action surface. Phase 3 introduces it
//! incrementally per `docs/plans/2026-04-28-phase-3-actions.md`:
//!
//! | Submodule | Ticket | Contents |
//! |---|---|---|
//! | [`schema`] | TASK-058 (this) | `Action`, `ActionSpec`, `Idempotency` |
//! | `timing`   | TASK-059 | `GestureConfig`, `ActionTiming` |
//! | `map`      | TASK-062 | `WidgetActionMap` |
//! | `dispatcher` | TASK-062 | `dispatch()` |
//! | `url`      | TASK-063 | `Url` action handler |
//! | `queue`    | TASK-065 | offline FIFO queue |
//!
//! Only `schema` exists today; the rest are added by their respective tickets.

pub mod schema;

pub use schema::{Action, ActionSpec, Idempotency};
