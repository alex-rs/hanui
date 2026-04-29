//! Phase 3 action types and dispatcher (write/command path).
//!
//! This module is the home of the typed action surface. Phase 3 introduces it
//! incrementally per `docs/plans/2026-04-28-phase-3-actions.md`:
//!
//! | Submodule | Ticket | Contents |
//! |---|---|---|
//! | [`schema`] | TASK-058 | `Action`, `ActionSpec`, `Idempotency` |
//! | [`timing`] | TASK-059 | `GestureConfig`, `ActionTiming`, `ActionOverlapStrategy` |
//! | [`map`]    | TASK-062 | `WidgetActionMap`, `WidgetActionEntry`, `WidgetId` |
//! | [`dispatcher`] | TASK-062 | `Dispatcher`, `DispatchOutcome`, `DispatchError`, `Gesture` |
//! | [`url`]    | TASK-063 | `Url` action handler with `UrlActionMode` gate |
//! | `queue`    | TASK-065 | offline FIFO queue |

pub mod dispatcher;
pub mod map;
pub mod schema;
pub mod timing;
pub mod url;

pub use schema::{Action, ActionSpec, Idempotency};
pub use timing::{ActionOverlapStrategy, ActionTiming, GestureConfig};
pub use url::{handle_url_action, UrlError, UrlOutcome};
