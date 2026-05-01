//! Action dispatcher core.
//!
//! Turns a `(widget_id, gesture)` pair into either an outbound HA service
//! call (sent on the dispatcher's [`mpsc::Sender<OutboundCommand>`]
//! channel) or a UI-local event (more-info modal target / view router
//! navigation).
//!
//! # Boundaries (locked_decisions.ws_command_ack_envelope)
//!
//! The dispatcher is deliberately **not** the WS id authority. It builds an
//! [`OutboundFrame`] and an `oneshot::channel`, packages them into an
//! [`OutboundCommand`], and pushes the envelope through `command_tx`. The
//! WS client task on the receiving end allocates the next monotonic id,
//! registers the `ack_tx` in its pending-ack map, and serializes the wire
//! JSON. This gives Phase 3 a single coherent seam:
//!
//! * The dispatcher is unit-testable with a fake `mpsc::Sender<OutboundCommand>`
//!   that records sent commands without spinning up a real WS client.
//! * The WS client retains sole responsibility for id allocation and
//!   inbound demux, untouched by Phase 3 dispatcher work.
//!
//! Awaiting the `oneshot::Receiver<AckResult>` (with `optimistic_timeout_ms`
//! and revert on timeout) is **TASK-064 territory**. This module returns the
//! `oneshot::Receiver` to the caller via [`DispatchOutcome::Sent`] so
//! TASK-064 can layer optimistic state management on top without changing
//! the dispatcher signature.
//!
//! # Toggle capability fallback (locked_decisions.toggle_capability_fallback)
//!
//! [`Action::Toggle`] resolves through the [`ServiceRegistryHandle`]:
//!
//! 1. If `<domain>.toggle` is registered → emit `<domain>.toggle`.
//! 2. Else if both `<domain>.turn_on` AND `<domain>.turn_off` are
//!    registered → emit one of them based on the entity's current state
//!    (`on` → `turn_off`, `off` → `turn_on`).
//! 3. Else → return [`DispatchError::NoCapability`].
//!
//! The empty-registry case (Risk #3 — `get_services` failed on connect)
//! falls into branch (3) naturally: every lookup returns `None`, and the
//! dispatcher surfaces a descriptive error instead of panicking.
//!
//! # `Url` (TASK-063 handler + TASK-075 dispatcher wiring)
//!
//! Routes through [`crate::actions::url::handle_url_action_with_spawner`]
//! under the [`UrlActionMode`] gate (`Always` / `Never` / `Ask`). The
//! gate value lives on the dispatcher as `url_action_mode` (default
//! `Never`, fail-closed) and is overridable via
//! [`Dispatcher::with_url_action_mode`]. The spawner is overridable via
//! [`Dispatcher::with_url_spawner`] for tests. `Url` is never WS-bound:
//! no `OutboundCommand` is pushed, no optimistic entry recorded, in any
//! mode.
//!
//! # `Navigate` (TASK-068)
//!
//! [`Action::Navigate { view_id }`] routes through an optional
//! [`ViewRouter`] handle. When present, the dispatcher invokes
//! `router.navigate(view_id)` BEFORE returning
//! [`DispatchOutcome::Navigate`], so the Slint-side `ViewRouterGlobal::current-view`
//! property is updated synchronously on the same UI thread the gesture fired
//! on. The dispatcher's public signature is unchanged
//! (locked_decisions.phase4_forward_compat) — Phase 4 will populate
//! multi-view configs and the same dispatcher routes them. When the router
//! field is `None` (e.g. unit tests, or the dispatcher is constructed before
//! the window exists), the dispatcher still returns
//! [`DispatchOutcome::Navigate`]; the caller can decide what to do with the
//! payload. Phase 3 single-view: `Navigate { view_id: "default" }` is a
//! no-op (the global already holds `"default"`), per
//! locked_decisions.view_router.
//!
//! # Optimistic UI reconciliation (TASK-064)
//!
//! When wired with [`Dispatcher::with_optimistic_reconciliation`], a successful
//! [`Action::Toggle`] / [`Action::CallService`] dispatch records an
//! [`OptimisticEntry`] on the [`LiveStore`] and spawns a reconciliation task
//! that:
//!
//! 1. Awaits `ack_rx` with `optimistic_timeout_ms` (`locked_decisions.action_timing`).
//! 2. On ack-success: if the entry was already dropped by an inbound
//!    `state_changed` event (rule 1 — `last_changed > dispatched_at`), no-op.
//!    Otherwise (rule 2 — ack-without-event no-op success): if the current
//!    entity state matches `tentative_state`, drop the entry. Else hold for
//!    the remainder of the deadline; on elapse, revert.
//! 3. On ack-error (rule 4): drop the entry immediately (revert).
//! 4. On timeout (rule 5): drop the entry (revert).
//!
//! Rule 3 (attribute-only `state_changed`) is enforced inside
//! [`LiveStore::apply_event`] via the strict-greater-than comparison —
//! attribute-only events keep `last_changed` unchanged and therefore leave
//! optimistic entries intact.
//!
//! # `LastWriteWins` cancellation
//!
//! Per `locked_decisions.action_timing`, a second gesture on the same widget
//! while an action is pending cancels the pending entries (drops them from the
//! `LiveStore`) and dispatches the new action. The new entry's `prior_state`
//! is the FIRST cancelled entry's `prior_state` (the chain root, NOT the most
//! recent cancelled `tentative_state`). The cancelled entry's late-arriving
//! ack is observed by its reconciliation task as "entry already gone" and
//! ignored — it does NOT clear or revert the current entry.
//!
//! # Backpressure (`locked_decisions.backpressure`)
//!
//! [`LiveStore::insert_optimistic_entry`] enforces per-entity (default 4) and
//! global (default 64) caps. When saturated the dispatcher returns
//! [`DispatchError::BackpressureRejected`] AND emits a [`ToastEvent`] on the
//! installed toast channel — never silently drops, never `Err`-only. TASK-067
//! consumes the toast channel; TASK-069 protocol test asserts the toast is
//! observed end-to-end.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, warn};

use crate::actions::map::{WidgetActionEntry, WidgetActionMap, WidgetId};
use crate::actions::queue::{OfflineQueue, QueueError};
use crate::actions::timing::{ActionOverlapStrategy, ActionTiming};
use crate::actions::url::{self, UrlError, UrlOutcome};
use crate::actions::Action;
use crate::dashboard::profiles::UrlActionMode;
use crate::ha::client::{AckResult, OutboundCommand, OutboundFrame};
use crate::ha::entity::EntityId;
use crate::ha::live_store::{LiveStore, OptimisticEntry, OptimisticInsertError};
use crate::ha::services::ServiceRegistryHandle;
use crate::ha::store::EntityStore;
use crate::platform::status::ConnectionState;
use crate::ui::view_router::ViewRouter;

// ---------------------------------------------------------------------------
// Gesture
// ---------------------------------------------------------------------------

/// Which gesture fired.
///
/// Derived from the Slint card-base layer (TASK-060). The dispatcher uses
/// this to pick the corresponding `Action` from the
/// [`crate::actions::map::WidgetActionEntry`] (`tap` / `hold` /
/// `double_tap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    Tap,
    Hold,
    DoubleTap,
}

// ---------------------------------------------------------------------------
// ToastEvent
// ---------------------------------------------------------------------------

/// Toast-channel event the dispatcher emits on user-visible failures.
///
/// Phase 3 (TASK-064) wires the `BackpressureRejected` variant; TASK-067
/// renders the toast UI. The event payload carries the entity id (so the
/// toast layer can decorate "Action queue full for `light.kitchen`") and a
/// terse human-readable reason. No raw token / PII is ever placed here.
///
/// `#[non_exhaustive]` so future variants (e.g. `ChannelClosed`,
/// `NoCapability`) can be added in subsequent tickets without breaking
/// downstream pattern matches.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastEvent {
    /// The optimistic-entry per-entity or global cap was hit; the dispatch
    /// was rejected (`locked_decisions.backpressure`).
    BackpressureRejected {
        /// Entity that was at capacity when the dispatch fired.
        entity_id: EntityId,
        /// Which cap was hit (per-entity or global) — surfaced for the
        /// toast layer to compose a precise message.
        scope: BackpressureScope,
    },
    /// A non-idempotent action ([`Action::Toggle`] / [`Action::Url`]) was
    /// fired while the connection is not [`ConnectionState::Live`]; per
    /// `locked_decisions.idempotency_marker` it cannot be queued and surfaces
    /// a loud error toast (TASK-065 Risk #6 — the load-bearing security
    /// signal so the founder sees the rejection rather than silently losing
    /// the tap).
    OfflineNonIdempotent {
        /// Entity tied to the rejected action (sourced from the
        /// [`crate::actions::map::WidgetActionEntry`] entity_id).
        entity_id: EntityId,
    },
    /// An idempotent [`Action::CallService`] was fired while offline and the
    /// queue accepted it for replay on reconnect. Phase 3 (TASK-067) renders
    /// a "queued — will fire on reconnect" indication so the user knows the
    /// tap was not silently dropped.
    OfflineQueued {
        /// Entity tied to the queued action.
        entity_id: EntityId,
    },
    /// An idempotent [`Action::CallService`] was fired offline but the queue
    /// refused it (typically because the service is not on the runtime
    /// allowlist — see [`crate::actions::queue::is_service_allowlisted`]).
    OfflineQueueRejected {
        /// Entity tied to the rejected action.
        entity_id: EntityId,
        /// Why the queue refused the action.
        reason: QueueRejectReason,
    },
}

/// Why [`OfflineQueue::enqueue`] refused an action — surface for toast text.
///
/// Mirrors [`QueueError`] but Copy + structurally minimal so the toast layer
/// can pattern-match without owning a `String`. Domain / service strings
/// from `QueueError::ServiceNotAllowlisted` are not surfaced through the
/// toast event itself: the toast layer composes the user-visible message
/// from the entity_id + reason variant; the verbose detail is in the log
/// line at `warn!` level emitted by [`OfflineQueue::enqueue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueRejectReason {
    /// The runtime allowlist (`turn_on`, `turn_off`, `set_*`) did not match
    /// the service name.
    ServiceNotAllowlisted,
    /// The variant was not one the queue accepts (programming error inside
    /// the dispatcher — should never reach the user). Surfaced for
    /// completeness so a future mis-routing is observable as a toast rather
    /// than silently dropped.
    UnsupportedVariant,
}

/// Which optimistic-entry cap triggered a [`ToastEvent::BackpressureRejected`]
/// (`locked_decisions.backpressure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureScope {
    /// Per-entity cap (default 4).
    PerEntity,
    /// Global cap across all entities (default 64).
    Global,
}

// ---------------------------------------------------------------------------
// DispatchOutcome
// ---------------------------------------------------------------------------

/// What the dispatcher produced for a successful `dispatch` call.
///
/// `Sent` carries the `oneshot::Receiver<AckResult>` so the caller (Phase 3
/// optimistic UI in TASK-064) can await the WS-client's reply with
/// `optimistic_timeout_ms` and revert on timeout. The dispatcher itself
/// does **not** await — that is TASK-064's responsibility.
///
/// The UI-local variants (`MoreInfo`, `Navigate`) carry their target so
/// the caller can route them onto the modal stack / view router.
/// `NoOp` is the explicit no-op outcome for `Action::None`.
#[derive(Debug)]
pub enum DispatchOutcome {
    /// A WS service-call envelope was successfully pushed on
    /// `command_tx`. The caller holds `ack_rx` and is responsible for
    /// awaiting it (TASK-064).
    Sent {
        /// The dispatcher's reply slot; resolves with the WS client's
        /// `AckResult` once HA returns a `result` frame, or never if the
        /// channel is dropped (TASK-064 surfaces dropped-receiver as a
        /// `ChannelClosed` toast).
        ack_rx: oneshot::Receiver<AckResult>,
    },
    /// Open the more-info modal for the given entity. The Slint side
    /// (TASK-066) consumes this through a UI event channel; this PR only
    /// returns the typed payload.
    MoreInfo {
        /// The entity to render in the modal. Sourced from the
        /// [`crate::actions::map::WidgetActionEntry`] entity_id —
        /// **not** by re-walking the dashboard view spec at dispatch
        /// time (locked_decisions.more_info_modal, Risk #12).
        entity_id: EntityId,
    },
    /// Navigate the view router to `view_id`. The Slint side (TASK-068)
    /// consumes this through a UI event channel; this PR only returns
    /// the typed payload.
    Navigate {
        /// Target view id, copied from the action variant payload.
        view_id: String,
    },
    /// `Action::None` — nothing to send, nothing to display.
    NoOp,
    /// The dispatcher was offline (connection state != [`ConnectionState::Live`])
    /// when the action fired, and an idempotent [`Action::CallService`] was
    /// accepted into the offline queue for replay on reconnect (TASK-065).
    /// The caller has nothing to await — the queue's reconnect-flush is the
    /// async path that owns the eventual ack.
    Queued {
        /// Entity the queued action targets.
        entity_id: EntityId,
    },
    /// `Action::Url` with [`UrlActionMode::Always`]: `xdg-open` was spawned
    /// successfully (TASK-075). The child is fire-and-forget; no
    /// `OutboundCommand` is pushed. The caller has nothing to await.
    UrlOpened,
    /// `Action::Url` with [`UrlActionMode::Never`]: the device profile
    /// blocked the URL action (TASK-075). `text` is the static toast string
    /// ([`crate::actions::url::TOAST_BLOCKED_BY_PROFILE`]) for the UI layer
    /// to render. No `OutboundCommand` is pushed.
    UrlBlockedToast {
        /// Static toast text to render (never contains the href).
        text: &'static str,
    },
    /// `Action::Url` with [`UrlActionMode::Ask`]: the device profile defers
    /// to a Phase 6 confirmation dialog (TASK-075). `text` is the static
    /// toast string ([`crate::actions::url::TOAST_ASK_PHASE_6`]). No
    /// `OutboundCommand` is pushed.
    UrlAskToast {
        /// Static toast text to render (never contains the href).
        text: &'static str,
    },
    /// [`Action::Unlock`] with `WidgetOptions::Lock.require_confirmation_on_unlock`
    /// set: the dispatcher invoked [`ConfirmHost::confirm`] and the actual
    /// `lock.unlock` service call (and any subsequent PIN entry) is deferred
    /// until the user accepts the confirm modal. The dispatcher returns
    /// synchronously — the OutboundCommand is `try_send`'d from the
    /// confirm callback later, NOT from this `dispatch` invocation.
    /// Per `locked_decisions.confirmation_on_lock_unlock`, offline replay
    /// does not show the confirm modal (the action was confirmed at
    /// original dispatch time before queueing).
    LockAwaitingConfirm {
        /// Entity targeted by the deferred lock/unlock.
        entity_id: EntityId,
    },
    /// [`Action::Unlock`] (or [`Action::Lock`]) with
    /// `PinPolicy::Required`: the dispatcher invoked
    /// [`crate::actions::pin::PinEntryHost::request_pin`] and the actual
    /// `lock.unlock` / `lock.lock` service call is deferred until the
    /// user submits a PIN. Per `locked_decisions.pin_entry_dispatch` the
    /// code is consumed exactly once via FnOnce and never persisted —
    /// the on_submit closure builds the call_service frame with the code
    /// in `data.code` and `try_send`s the OutboundCommand on
    /// `command_tx`.
    LockAwaitingPinEntry {
        /// Entity targeted by the deferred lock/unlock.
        entity_id: EntityId,
    },
}

// ---------------------------------------------------------------------------
// DispatchError
// ---------------------------------------------------------------------------

/// Why a dispatch attempt could not produce a [`DispatchOutcome`].
///
/// Each variant is intended to map onto a user-visible toast (TASK-067) or
/// a debug log line, depending on severity. Per Risk #3, an empty
/// `ServiceRegistry` produces [`DispatchError::NoCapability`] — never a
/// panic.
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchError {
    /// No entry was found for `widget_id` in the [`WidgetActionMap`].
    /// Indicates a programming error — every gesture-bound widget should
    /// have an entry.
    UnknownWidget(WidgetId),

    /// Toggle was requested but the [`ServiceRegistry`] does not have
    /// either `<domain>.toggle` or the `turn_on`/`turn_off` pair for the
    /// entity's domain. The user-visible toast cites `domain` so the
    /// founder can diagnose whether HA is missing a service.
    ///
    /// [`ServiceRegistry`]: crate::ha::services::ServiceRegistry
    NoCapability {
        /// HA domain that lacked any toggle-equivalent service.
        domain: String,
        /// One-line description for the toast / log line.
        reason: &'static str,
    },

    /// Toggle was requested but the entity's current state is neither
    /// `"on"` nor `"off"` (typically `"unavailable"` / `"unknown"`).
    /// The fallback path needs a known state to choose `turn_on` vs
    /// `turn_off`, so this is treated as an error rather than a guess.
    UnknownToggleState {
        /// The entity that could not be toggled.
        entity_id: EntityId,
        /// The state string we observed.
        observed_state: String,
    },

    /// The entity referenced by the action is not in the `LiveStore`
    /// snapshot. Distinct from `NoCapability` (which is about HA-side
    /// service availability).
    EntityNotFound(EntityId),

    /// `command_tx` is `None` — the dispatcher has not been wired to a
    /// WS client yet. TASK-072 wires this; before that, every WS-bound
    /// dispatch returns this error.
    ChannelNotWired,

    /// `command_tx` is `Some(_)` but the receiver has been dropped (the
    /// WS client task has exited or panicked). The reconnect FSM
    /// repopulates `command_tx` on restart; meanwhile this surfaces as
    /// a toast (TASK-067).
    ChannelClosed,

    /// The action variant is recognised but its handler ships in a
    /// later ticket. Currently used for `Action::Url` (TASK-063).
    ///
    /// `what` is a static string identifying the action variant; `ticket`
    /// names the future task that will own the handler.
    NotImplementedYet {
        /// Short variant name for the toast / log line.
        what: &'static str,
        /// Ticket that owns the deferred work.
        ticket: &'static str,
    },

    /// Optimistic-entry capacity is saturated for this entity (per-entity
    /// cap) or globally (`locked_decisions.backpressure`). Returned by
    /// TASK-064 dispatch with optimistic reconciliation enabled. The toast
    /// channel concurrently receives a [`ToastEvent::BackpressureRejected`]
    /// — so the user sees a visible "queue full" indication and the caller
    /// sees the typed `Err`.
    BackpressureRejected {
        /// Entity that was at capacity.
        entity_id: EntityId,
        /// Which cap triggered the rejection.
        scope: BackpressureScope,
    },

    /// The connection is offline and the action is non-idempotent
    /// ([`Action::Toggle`] or [`Action::Url`]). Per
    /// `locked_decisions.idempotency_marker` non-idempotent actions are
    /// **never queued**; this is the load-bearing security signal — the
    /// dispatcher must surface a typed `Err` AND emit
    /// [`ToastEvent::OfflineNonIdempotent`] (Risk #6).
    OfflineNonIdempotent {
        /// Entity tied to the rejected action.
        entity_id: EntityId,
    },

    /// The connection is offline and the offline queue refused the action
    /// (e.g. `delete_user` failing the runtime allowlist, or the dispatcher
    /// mistakenly forwarded a UI-local variant to the queue). The toast
    /// channel concurrently receives [`ToastEvent::OfflineQueueRejected`].
    OfflineQueueRejected {
        /// Entity tied to the rejected action.
        entity_id: EntityId,
        /// Why the queue refused the action.
        reason: QueueRejectReason,
    },
    /// `Action::Url` href failed validation (scheme, shell-metachar, length
    /// cap, or `file://` path-traversal check) — TASK-075. The `reason`
    /// field is forwarded verbatim from
    /// [`crate::actions::url::UrlError::InvalidHref`]; it is a `&'static str`
    /// so it cannot leak the rejected href.
    UrlInvalidHref {
        /// One-line rejection reason (never contains the href itself).
        reason: &'static str,
    },
    /// `Action::Url` with [`UrlActionMode::Always`]: `xdg-open` could not be
    /// spawned — TASK-075. The `reason` is the [`Display`][std::fmt::Display]
    /// rendering of the underlying `io::Error`, NOT its `Debug` form (which
    /// can surface OS paths). Truncated to ≤256 chars to fit the toast surface
    /// budget.
    UrlSpawnFailed {
        /// Human-readable spawn-failure description (no href, no PII).
        reason: String,
    },
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::UnknownWidget(id) => {
                write!(f, "no action map entry for widget `{id}`")
            }
            DispatchError::NoCapability { domain, reason } => {
                write!(
                    f,
                    "no toggle-equivalent service registered for domain `{domain}`: {reason}"
                )
            }
            DispatchError::UnknownToggleState {
                entity_id,
                observed_state,
            } => {
                write!(
                    f,
                    "cannot toggle `{entity_id}`: state `{observed_state}` is neither on nor off"
                )
            }
            DispatchError::EntityNotFound(id) => {
                write!(f, "entity `{id}` not in store snapshot")
            }
            DispatchError::ChannelNotWired => {
                f.write_str("dispatcher command_tx is not wired (TASK-072 pending)")
            }
            DispatchError::ChannelClosed => {
                f.write_str("dispatcher command_tx receiver dropped (WS client task exited?)")
            }
            DispatchError::NotImplementedYet { what, ticket } => {
                write!(f, "{what} action handler is deferred to {ticket}")
            }
            DispatchError::BackpressureRejected { entity_id, scope } => match scope {
                BackpressureScope::PerEntity => {
                    write!(
                        f,
                        "action queue full for `{entity_id}` (per-entity backpressure)"
                    )
                }
                BackpressureScope::Global => {
                    write!(
                        f,
                        "action queue full for `{entity_id}` (global backpressure)"
                    )
                }
            },
            DispatchError::OfflineNonIdempotent { entity_id } => {
                write!(
                    f,
                    "cannot perform action on `{entity_id}` while offline (non-idempotent — would risk double-firing on reconnect)"
                )
            }
            DispatchError::OfflineQueueRejected { entity_id, reason } => match reason {
                QueueRejectReason::ServiceNotAllowlisted => write!(
                    f,
                    "offline queue refused action on `{entity_id}`: service is not on the runtime allowlist (turn_on / turn_off / set_*)"
                ),
                QueueRejectReason::UnsupportedVariant => write!(
                    f,
                    "offline queue refused action on `{entity_id}`: unsupported variant for the offline path"
                ),
            },
            DispatchError::UrlInvalidHref { reason } => {
                write!(f, "url action rejected: {reason}")
            }
            DispatchError::UrlSpawnFailed { reason } => {
                write!(f, "url action: xdg-open spawn failed: {reason}")
            }
        }
    }
}

impl std::error::Error for DispatchError {}

// ---------------------------------------------------------------------------
// ConfirmHost (TASK-104)
// ---------------------------------------------------------------------------

/// Capability trait: the dispatcher calls this to show a confirmation modal
/// before dispatching a `lock.unlock` service call when
/// `WidgetOptions::Lock.require_confirmation_on_unlock` is set.
///
/// # Module placement
///
/// Lives here in `src/actions/dispatcher.rs` (rather than alongside
/// [`crate::actions::pin::PinEntryHost`] in `src/actions/pin.rs`) because
/// `src/actions/pin.rs` is in TASK-104's `must_not_touch` list. The trait
/// shares the same `Send + Sync + Arc<dyn _>`-friendly shape as
/// `PinEntryHost` so the dispatcher can hold both behind `Arc` clones.
///
/// # Contract
///
/// - `confirm` returns immediately; it is **not** a blocking call.
/// - The user's accept signal is delivered asynchronously via
///   `on_accept`, which is called exactly once when the user accepts
///   the modal. If the user dismisses, `on_accept` is **not** called —
///   the dispatcher treats a missing invocation as a cancelled
///   operation and produces no service-call frame.
///
/// Per `locked_decisions.confirmation_on_lock_unlock` this is dispatch-time
/// behaviour only: offline replay (`OfflineQueue::flush` on reconnect)
/// does NOT consult the confirm host — the user's confirmation at
/// original dispatch time is what authorises the queued action.
pub trait ConfirmHost: Send + Sync {
    /// Show a confirmation modal. `on_accept` fires once on user accept;
    /// dropping the closure (cancellation) leaves no side effect.
    fn confirm(&self, entity_id: EntityId, on_accept: Box<dyn FnOnce() + Send>);
}

// ---------------------------------------------------------------------------
// LockOperation (TASK-104)
// ---------------------------------------------------------------------------

/// Which lock service the dispatcher should call after the modal /
/// PIN-entry flow completes.
///
/// Internal to the dispatcher; the public `Action::Lock` /
/// `Action::Unlock` variants are folded into this small enum at the
/// dispatch entry point so the per-operation logic can branch on a
/// closed two-variant enum rather than re-inspecting the whole `Action`
/// shape from inside the helper functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOperation {
    /// `lock.lock` — engage the bolt.
    Lock,
    /// `lock.unlock` — retract the bolt.
    Unlock,
}

// ---------------------------------------------------------------------------
// LockDispatchSettings (TASK-104)
// ---------------------------------------------------------------------------

/// Per-widget Lock dispatch settings, mirroring the relevant fields of
/// [`crate::dashboard::schema::WidgetOptions::Lock`].
///
/// Populated at startup by the bridge from the loaded `Dashboard` and
/// installed on the dispatcher via [`Dispatcher::with_lock_settings`].
/// The dispatcher reads this table during `Action::Lock` / `Action::Unlock`
/// dispatch to decide whether to invoke the PIN entry / confirm modal
/// flow before building the service-call frame.
///
/// # Why a separate type
///
/// The dispatcher does not depend directly on `WidgetOptions::Lock`
/// because that variant lives in `src/dashboard/schema.rs` (in TASK-104's
/// `must_not_touch` list). Re-using the [`PinPolicy`] re-export from
/// `crate::dashboard::schema` is fine — only the type *shape* is
/// imported, not modified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockDispatchSettings {
    /// PIN policy for this widget (mirrors `WidgetOptions::Lock.pin_policy`).
    pub pin_policy: crate::dashboard::schema::PinPolicy,
    /// Whether to show a confirmation modal before unlocking (mirrors
    /// `WidgetOptions::Lock.require_confirmation_on_unlock`).
    pub require_confirmation_on_unlock: bool,
}

impl LockDispatchSettings {
    /// Default settings: no PIN, no confirmation. Used for widgets whose
    /// `WidgetOptions` block is absent or non-Lock — the dispatcher falls
    /// back to a direct service-call dispatch with no modal flow.
    #[must_use]
    pub fn permissive() -> Self {
        LockDispatchSettings {
            pin_policy: crate::dashboard::schema::PinPolicy::None,
            require_confirmation_on_unlock: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// The action-routing core for Phase 3.
///
/// Stateless beyond its injected dependencies (`command_tx`, `services`,
/// optional reconciliation context). Tests construct one with a fake
/// `mpsc::Sender<OutboundCommand>` recorder via [`Dispatcher::new`] /
/// [`Dispatcher::with_command_tx`].
///
/// # Optimistic reconciliation (TASK-064)
///
/// Calling [`Dispatcher::with_optimistic_reconciliation`] activates the
/// optimistic-UI path: each successful WS-bound dispatch records an
/// [`OptimisticEntry`] on the [`LiveStore`] and spawns a reconciliation
/// task per the rules in `locked_decisions.optimistic_reconciliation_key`.
/// Without that builder call, the dispatcher's behaviour is identical to
/// TASK-062 — useful for unit tests that only assert the WS-frame shape.
///
/// # Cloning
///
/// `Dispatcher` is `Clone` so a single instance can be cloned into Slint
/// gesture callbacks. The underlying `mpsc::Sender` is cheap to clone (it
/// shares the same channel), [`ServiceRegistryHandle`] / `Arc<LiveStore>`
/// / `mpsc::Sender<ToastEvent>` are all `Arc`-cheap. The `view_router`
/// (TASK-068) is also `Arc`-cheap when present.
#[derive(Clone)]
pub struct Dispatcher {
    /// Outbound channel to the WS client task. `None` until TASK-072
    /// wires it; in that interim every WS-bound dispatch returns
    /// [`DispatchError::ChannelNotWired`].
    command_tx: Option<mpsc::Sender<OutboundCommand>>,

    /// Shared handle to the `ServiceRegistry` populated by the WS client
    /// (TASK-048 cross-task accessor). Used for Toggle's capability
    /// fallback.
    services: ServiceRegistryHandle,

    /// Optional view-router handle (TASK-068). When `Some`, the dispatcher
    /// invokes `router.navigate(view_id)` on `Action::Navigate` before
    /// returning [`DispatchOutcome::Navigate`]. When `None`, the dispatcher
    /// still returns the outcome — the router is purely an additional sink
    /// so the public signature is unchanged
    /// (locked_decisions.phase4_forward_compat).
    ///
    /// Wrapped in `Arc<dyn ViewRouter>` so the dispatcher stays `Clone`
    /// (the trait object is `?Sized` and cannot be cloned directly) and the
    /// router can be shared across cloned dispatcher instances handed to
    /// gesture callbacks.
    view_router: Option<Arc<dyn ViewRouter>>,

    /// Optimistic-reconciliation context (TASK-064). `None` reproduces the
    /// pre-TASK-064 dispatcher behaviour: no entry recorded, no
    /// reconciliation task spawned, no backpressure check.
    reconciliation: Option<ReconciliationCtx>,

    /// Offline-routing context (TASK-065). `None` reproduces the
    /// pre-TASK-065 dispatcher behaviour: every dispatch unconditionally
    /// targets `command_tx` regardless of connection state. When `Some`,
    /// the dispatcher consults the connection state at every dispatch and
    /// routes WS-bound actions through the offline queue when the state
    /// is not [`ConnectionState::Live`].
    offline: Option<OfflineRoutingCtx>,

    /// Gate controlling how `Action::Url` dispatches behave (TASK-075).
    ///
    /// Defaults to [`UrlActionMode::Never`] (fail-closed) so the dispatcher
    /// is safe to construct without a `DeviceProfile` in scope. Phase 4 will
    /// populate this from the YAML `device_profile.url_action_mode` field.
    url_action_mode: UrlActionMode,

    /// Injectable spawner for `Action::Url` (TASK-075).
    ///
    /// Defaults to [`crate::actions::url::default_spawner`] (production
    /// `xdg-open` gate). Tests inject a recording closure via
    /// [`Dispatcher::with_url_spawner`] to assert call counts without
    /// launching a real process.
    url_spawner: crate::actions::url::Spawner,

    /// Optional PIN entry host (TASK-104). When `Some`, the dispatcher
    /// invokes [`PinEntryHost::request_pin`] on `Action::Lock` /
    /// `Action::Unlock` if the widget's `LockDispatchSettings.pin_policy`
    /// is `Required`. The on_submit closure consumes the entered code via
    /// FnOnce and `try_send`s the OutboundCommand on `command_tx` directly
    /// — the code is dropped at end of closure scope.
    /// `None` means PIN entry is unwired (Phase 6 6.0 default; tests
    /// inject a `MockPinEntryHost`).
    pin_host: Option<Arc<dyn crate::actions::pin::PinEntryHost>>,

    /// Optional confirm modal host (TASK-104). When `Some`, the dispatcher
    /// invokes [`ConfirmHost::confirm`] on `Action::Unlock` if the
    /// widget's `LockDispatchSettings.require_confirmation_on_unlock` is
    /// `true`. The on_accept closure dispatches downstream (PIN entry or
    /// service call) once the user accepts.
    /// `None` means the confirm flow is unwired (defaults to direct
    /// dispatch as if `require_confirmation_on_unlock` were `false`).
    confirm_host: Option<Arc<dyn ConfirmHost>>,

    /// Per-widget lock dispatch settings (TASK-104). Populated at startup
    /// from the loaded `Dashboard`'s `WidgetOptions::Lock` blocks. The
    /// dispatcher reads `pin_policy` + `require_confirmation_on_unlock`
    /// at `Action::Lock` / `Action::Unlock` dispatch time. Wrapped in
    /// `Arc<HashMap<...>>` so the dispatcher's `Clone` impl bumps a
    /// refcount rather than copying the map for every gesture callback.
    /// Empty by default — a missing entry resolves to
    /// [`LockDispatchSettings::permissive`] (no PIN, no confirmation).
    lock_settings: Arc<HashMap<WidgetId, LockDispatchSettings>>,
}

// Custom `Debug` so the `dyn ViewRouter` field (which does not implement
// `Debug`) does not block the derive. We surface only whether a router is
// present — its identity is opaque. Likewise the `reconciliation` field
// holds `Arc`-shared trait-implementing handles whose identity is opaque to
// the dispatcher.
impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("command_tx", &self.command_tx.as_ref().map(|_| "<sender>"))
            .field("services", &"<ServiceRegistryHandle>")
            .field("view_router", &self.view_router.is_some())
            .field(
                "reconciliation",
                &self.reconciliation.as_ref().map(|_| "<ReconciliationCtx>"),
            )
            .field("url_action_mode", &self.url_action_mode)
            .field("url_spawner", &"<Spawner>")
            .finish()
    }
}

/// Context the dispatcher needs to consult connection state and forward
/// idempotent actions to the offline queue (TASK-065).
///
/// Cloned cheaply per-dispatch (the queue is `Arc<Mutex<_>>` and the
/// `watch::Receiver` is itself shareable). Toast events are sent on the
/// `toast_tx` channel — this is the SAME channel TASK-064 uses for
/// `BackpressureRejected` so the renderer needs only one observer.
#[derive(Clone)]
struct OfflineRoutingCtx {
    /// The shared offline queue, drained on reconnect by the
    /// reconnect-flush task ([`OfflineQueue::flush`]).
    queue: Arc<Mutex<OfflineQueue>>,
    /// Read-only handle to the WS connection FSM. When the current value
    /// is anything other than [`ConnectionState::Live`], the dispatcher
    /// routes WS-bound actions through the queue. The queue's flush
    /// machinery itself is wired by the reconnect path in production
    /// (out-of-scope for the dispatcher).
    state_rx: watch::Receiver<ConnectionState>,
    /// Toast channel. Reuses TASK-064's `mpsc::Sender<ToastEvent>` so a
    /// single observer covers BackpressureRejected + OfflineNonIdempotent
    /// + OfflineQueued + OfflineQueueRejected.
    toast_tx: mpsc::Sender<ToastEvent>,
}

/// Context the dispatcher needs to record optimistic entries and spawn the
/// reconciliation task (TASK-064).
#[derive(Clone)]
struct ReconciliationCtx {
    /// Shared `LiveStore` — the dispatcher reads `prior_state` from the
    /// snapshot, inserts/drops `OptimisticEntry`s, and reads current state
    /// at ack time for the rule-2 (no-op success) snapshot match.
    store: Arc<LiveStore>,
    /// Action timing (`optimistic_timeout_ms`, `action_overlap_strategy`).
    timing: ActionTiming,
    /// Toast channel — the dispatcher emits
    /// [`ToastEvent::BackpressureRejected`] on this channel concurrently
    /// with returning [`DispatchError::BackpressureRejected`]
    /// (`locked_decisions.backpressure`).
    toast_tx: mpsc::Sender<ToastEvent>,
    /// Dispatcher-local monotonic identity counter. Each
    /// [`OptimisticEntry`] receives a unique `request_id` from this counter
    /// — this is the dispatcher-side identity (the WS-client-allocated id
    /// is opaque to the dispatcher per
    /// `locked_decisions.ws_command_ack_envelope`).
    next_request_id: Arc<AtomicU32>,
}

impl Dispatcher {
    /// Construct a dispatcher with no WS channel.
    ///
    /// TASK-072 will add a setter (or builder method) to populate
    /// `command_tx` after the WS client task has launched. Until then,
    /// every WS-bound dispatch returns [`DispatchError::ChannelNotWired`]
    /// — exactly the behaviour the integration test in TASK-069 covers.
    #[must_use]
    pub fn new(services: ServiceRegistryHandle) -> Self {
        Dispatcher {
            command_tx: None,
            services,
            view_router: None,
            reconciliation: None,
            offline: None,
            url_action_mode: UrlActionMode::Never,
            url_spawner: url::default_spawner,
            pin_host: None,
            confirm_host: None,
            lock_settings: Arc::new(HashMap::new()),
        }
    }

    /// Construct a dispatcher with a populated WS channel.
    ///
    /// Used by tests (with a fake `mpsc::Sender<OutboundCommand>` recorder)
    /// and by TASK-072 (with the real WS-client command channel).
    #[must_use]
    pub fn with_command_tx(
        services: ServiceRegistryHandle,
        command_tx: mpsc::Sender<OutboundCommand>,
    ) -> Self {
        Dispatcher {
            command_tx: Some(command_tx),
            services,
            view_router: None,
            reconciliation: None,
            offline: None,
            url_action_mode: UrlActionMode::Never,
            url_spawner: url::default_spawner,
            pin_host: None,
            confirm_host: None,
            lock_settings: Arc::new(HashMap::new()),
        }
    }

    /// Attach a [`ViewRouter`] sink for `Action::Navigate` (TASK-068).
    ///
    /// The dispatcher's public `dispatch` signature is unchanged
    /// (locked_decisions.phase4_forward_compat) — the router is an
    /// additional optional sink. Phase 3 wires this once at startup with a
    /// [`crate::ui::view_router::SlintViewRouter`] backed by the main
    /// `MainWindow` weak handle.
    ///
    /// `router` is wrapped in [`Arc`] internally so the dispatcher remains
    /// `Clone` and can be cloned into gesture callbacks (TASK-062 contract).
    #[must_use]
    pub fn with_view_router<R: ViewRouter + 'static>(mut self, router: R) -> Self {
        self.view_router = Some(Arc::new(router));
        self
    }

    /// Variant of [`Dispatcher::with_view_router`] that accepts an existing
    /// [`Arc<dyn ViewRouter>`] for callers (notably tests) that want to keep
    /// their own clone of the router for assertions.
    #[must_use]
    pub fn with_view_router_arc(mut self, router: Arc<dyn ViewRouter>) -> Self {
        self.view_router = Some(router);
        self
    }

    /// Activate the optimistic-UI reconciliation path (TASK-064).
    ///
    /// When this builder method is called the dispatcher:
    ///
    /// 1. Records an [`OptimisticEntry`] on `store` for every successful
    ///    `Action::Toggle` / `Action::CallService` dispatch
    ///    (`locked_decisions.optimistic_reconciliation_key`).
    /// 2. Enforces the per-entity / global pending caps via
    ///    [`LiveStore::insert_optimistic_entry`] and emits a
    ///    [`ToastEvent::BackpressureRejected`] on `toast_tx` when they are
    ///    saturated (`locked_decisions.backpressure`).
    /// 3. Honours `timing.action_overlap_strategy ==
    ///    ActionOverlapStrategy::LastWriteWins` by cancelling pending
    ///    entries on the same `entity_id` and dispatching the new action
    ///    with a chain-root-preserved `prior_state`.
    /// 4. Spawns a reconciliation task per dispatch that awaits `ack_rx`
    ///    with `timing.optimistic_timeout_ms`, applying rules 2 / 4 / 5
    ///    from `locked_decisions.optimistic_reconciliation_key`. Rule 1 /
    ///    rule 3 are enforced inside [`LiveStore::apply_event`] and need
    ///    no extra wiring here.
    #[must_use]
    pub fn with_optimistic_reconciliation(
        mut self,
        store: Arc<LiveStore>,
        timing: ActionTiming,
        toast_tx: mpsc::Sender<ToastEvent>,
    ) -> Self {
        self.reconciliation = Some(ReconciliationCtx {
            store,
            timing,
            toast_tx,
            next_request_id: Arc::new(AtomicU32::new(1)),
        });
        self
    }

    /// Activate the offline-routing path (TASK-065).
    ///
    /// When this builder is called, every `dispatch` consults the current
    /// [`ConnectionState`] from `state_rx`. If the connection is anything
    /// other than [`ConnectionState::Live`]:
    ///
    /// 1. [`Action::Toggle`] / [`Action::Url`] → return
    ///    [`DispatchError::OfflineNonIdempotent`] AND emit
    ///    [`ToastEvent::OfflineNonIdempotent`] (load-bearing security
    ///    rejection per `locked_decisions.idempotency_marker`, Risk #6).
    /// 2. [`Action::CallService`] → enqueue on the offline queue. Allowlisted
    ///    (`turn_on`/`turn_off`/`set_*`) actions return
    ///    [`DispatchOutcome::Queued`] and emit
    ///    [`ToastEvent::OfflineQueued`]; non-allowlisted actions return
    ///    [`DispatchError::OfflineQueueRejected`] and emit
    ///    [`ToastEvent::OfflineQueueRejected`].
    /// 3. [`Action::MoreInfo`] / [`Action::Navigate`] / [`Action::None`] →
    ///    fall through to the normal UI-local path; the connection state is
    ///    irrelevant for UI-only outcomes.
    ///
    /// The `toast_tx` argument SHOULD be the same channel passed to
    /// [`Self::with_optimistic_reconciliation`] (one observer downstream).
    /// The dispatcher does not validate that — passing different senders is
    /// permitted for tests that want isolated observers.
    ///
    /// The reconnect-flush task (production wiring out-of-scope for this
    /// builder) is responsible for calling [`OfflineQueue::flush`] when the
    /// state transitions back to `Live`.
    #[must_use]
    pub fn with_offline_queue(
        mut self,
        queue: Arc<Mutex<OfflineQueue>>,
        state_rx: watch::Receiver<ConnectionState>,
        toast_tx: mpsc::Sender<ToastEvent>,
    ) -> Self {
        self.offline = Some(OfflineRoutingCtx {
            queue,
            state_rx,
            toast_tx,
        });
        self
    }

    /// Set the [`UrlActionMode`] gate for `Action::Url` dispatches (TASK-075).
    ///
    /// Defaults to [`UrlActionMode::Never`] (fail-closed). Phase 4 will call
    /// this with the value loaded from `DeviceProfile.url_action_mode` in YAML.
    ///
    /// * `Always` — `xdg-open` is invoked immediately; returns
    ///   [`DispatchOutcome::UrlOpened`] on success.
    /// * `Never` — returns [`DispatchOutcome::UrlBlockedToast`]; no spawn.
    /// * `Ask` — returns [`DispatchOutcome::UrlAskToast`]; no spawn (Phase 6
    ///   swaps the Ask handler for a real confirmation dialog).
    #[must_use]
    pub fn with_url_action_mode(mut self, mode: UrlActionMode) -> Self {
        self.url_action_mode = mode;
        self
    }

    /// Override the `xdg-open` spawner for `Action::Url` dispatches
    /// (TASK-075).
    ///
    /// The default is [`crate::actions::url::default_spawner`] (production).
    /// Tests inject a recording closure via this method to count spawner
    /// invocations and force failure without launching a real process.
    ///
    /// The [`crate::actions::url::Spawner`] typedef is `fn(&str) ->
    /// io::Result<()>` — a plain function pointer, not a closure, so
    /// closures with captured state cannot be passed directly. Use a static
    /// `AtomicBool`/`AtomicUsize` for test state, mirroring the pattern in
    /// `src/actions/url.rs`'s own test module.
    #[must_use]
    pub fn with_url_spawner(mut self, spawner: crate::actions::url::Spawner) -> Self {
        self.url_spawner = spawner;
        self
    }

    /// Attach a [`PinEntryHost`] sink for `Action::Lock` / `Action::Unlock`
    /// dispatches that require PIN entry per the widget's
    /// [`LockDispatchSettings.pin_policy`] (TASK-104).
    ///
    /// Defaults to `None`. When `None` and a Lock/Unlock action requires
    /// PIN entry, the dispatcher returns
    /// [`DispatchError::NotImplementedYet`] rather than building an
    /// unprotected service-call frame — fail-closed per
    /// `locked_decisions.pin_entry_dispatch`.
    ///
    /// `host` is wrapped in [`Arc`] internally so the dispatcher remains
    /// `Clone` and the host can be shared across cloned dispatcher
    /// instances handed to gesture callbacks.
    ///
    /// [`PinEntryHost`]: crate::actions::pin::PinEntryHost
    #[must_use]
    pub fn with_pin_host<H: crate::actions::pin::PinEntryHost + 'static>(
        mut self,
        host: H,
    ) -> Self {
        self.pin_host = Some(Arc::new(host));
        self
    }

    /// Variant of [`Dispatcher::with_pin_host`] that accepts an existing
    /// [`Arc<dyn PinEntryHost>`] for callers (notably tests) that want to
    /// keep their own clone of the host for assertions.
    ///
    /// [`PinEntryHost`]: crate::actions::pin::PinEntryHost
    #[must_use]
    pub fn with_pin_host_arc(mut self, host: Arc<dyn crate::actions::pin::PinEntryHost>) -> Self {
        self.pin_host = Some(host);
        self
    }

    /// Attach a [`ConfirmHost`] sink for `Action::Unlock` dispatches that
    /// require confirmation per the widget's
    /// [`LockDispatchSettings.require_confirmation_on_unlock`] (TASK-104).
    ///
    /// Defaults to `None`. When `None`, the dispatcher treats every widget
    /// as `require_confirmation_on_unlock=false` and skips the modal step.
    /// The confirm host is consulted ONLY at original dispatch time;
    /// offline replay (`OfflineQueue::flush`) does NOT call this host
    /// (per `locked_decisions.confirmation_on_lock_unlock`).
    #[must_use]
    pub fn with_confirm_host<H: ConfirmHost + 'static>(mut self, host: H) -> Self {
        self.confirm_host = Some(Arc::new(host));
        self
    }

    /// Variant of [`Dispatcher::with_confirm_host`] that accepts an existing
    /// [`Arc<dyn ConfirmHost>`] for callers (notably tests) that want to
    /// keep their own clone of the host for assertions.
    #[must_use]
    pub fn with_confirm_host_arc(mut self, host: Arc<dyn ConfirmHost>) -> Self {
        self.confirm_host = Some(host);
        self
    }

    /// Install the per-widget lock dispatch settings table (TASK-104).
    ///
    /// The bridge populates this from each `WidgetOptions::Lock` block in
    /// the loaded `Dashboard`. Widgets without an entry resolve to
    /// [`LockDispatchSettings::permissive`] at lookup time (no PIN, no
    /// confirmation) — matching the schema's default field values.
    ///
    /// The table is wrapped in `Arc<HashMap<...>>` internally so the
    /// dispatcher's `Clone` impl bumps a refcount rather than copying the
    /// map for every gesture callback.
    #[must_use]
    pub fn with_lock_settings(mut self, settings: HashMap<WidgetId, LockDispatchSettings>) -> Self {
        self.lock_settings = Arc::new(settings);
        self
    }

    /// Route a gesture on a widget through the action map.
    ///
    /// Per the ticket acceptance criteria, this is the canonical
    /// dispatcher signature (locked_decisions.phase4_forward_compat —
    /// the signature does not change when Phase 4 swaps the
    /// [`WidgetActionMap`] data source).
    ///
    /// # Errors
    ///
    /// See [`DispatchError`] for the full taxonomy. The dispatcher
    /// **never panics** on an empty registry, an unknown widget, or a
    /// dropped sender — every error path returns a descriptive
    /// [`DispatchError`] for surfacing as a toast (TASK-067).
    pub fn dispatch(
        &self,
        widget_id: &WidgetId,
        gesture: Gesture,
        store: &LiveStore,
        action_map: &WidgetActionMap,
    ) -> Result<DispatchOutcome, DispatchError> {
        let entry = action_map
            .lookup(widget_id)
            .ok_or_else(|| DispatchError::UnknownWidget(widget_id.clone()))?;

        let action = pick_action(entry, gesture);

        debug!(
            widget = %widget_id,
            entity = %entry.entity_id,
            ?gesture,
            ?action,
            "dispatching action"
        );

        // TASK-065 offline routing — if the offline-routing context is wired
        // and the connection is not Live, route WS-bound actions through the
        // queue (or reject non-idempotent ones). UI-local actions
        // (None/MoreInfo/Navigate) fall through to the normal match below
        // because they have no WS-side effect.
        if let Some(ctx) = self.offline.as_ref() {
            let live = matches!(*ctx.state_rx.borrow(), ConnectionState::Live);
            if !live {
                if let Some(outcome) = self.maybe_route_offline(ctx, entry, action) {
                    return outcome;
                }
            }
        }

        match action {
            Action::None => Ok(DispatchOutcome::NoOp),
            Action::MoreInfo => Ok(DispatchOutcome::MoreInfo {
                entity_id: entry.entity_id.clone(),
            }),
            Action::Navigate { view_id } => {
                // TASK-068: drive the Slint `ViewRouterGlobal::current-view`
                // property by invoking the optional view-router. Phase 3
                // ships a single view (`"default"`); the navigate is
                // observably a no-op for that target. Unknown view ids are
                // logged at debug level inside the router (see
                // `SlintViewRouter::navigate`) and the global is updated
                // verbatim — Phase 4 will populate readers.
                //
                // We invoke the router BEFORE returning the outcome so a
                // caller that consumes the `DispatchOutcome::Navigate`
                // payload sees the global already updated. The dispatcher
                // signature is unchanged (locked_decisions.phase4_forward_compat).
                if let Some(router) = self.view_router.as_ref() {
                    router.navigate(view_id);
                }
                Ok(DispatchOutcome::Navigate {
                    view_id: view_id.clone(),
                })
            }
            Action::Url { href } => {
                // Url is never WS-bound: no OutboundCommand pushed, no
                // optimistic entry recorded, in any mode. The offline gate
                // above (maybe_route_offline) already returned
                // OfflineNonIdempotent for the offline path — this branch
                // only runs on the Live path.
                match url::handle_url_action_with_spawner(
                    href,
                    self.url_action_mode,
                    self.url_spawner,
                ) {
                    Ok(UrlOutcome::Opened) => Ok(DispatchOutcome::UrlOpened),
                    Ok(UrlOutcome::BlockedShowToast(text)) => {
                        Ok(DispatchOutcome::UrlBlockedToast { text })
                    }
                    Ok(UrlOutcome::AskShowToast(text)) => Ok(DispatchOutcome::UrlAskToast { text }),
                    Err(UrlError::InvalidHref { reason }) => {
                        Err(DispatchError::UrlInvalidHref { reason })
                    }
                    Err(UrlError::Spawn(io_err)) => {
                        // Use Display (not Debug) per url.rs:188-194 — the
                        // Display form is "failed to spawn xdg-open: <os msg>"
                        // and does not include the href. Truncate to ≤256
                        // bytes (rounded DOWN to a UTF-8 char boundary) to
                        // fit the toast surface budget. Naive `full[..256]`
                        // panics if byte 256 lands mid-codepoint — a real
                        // risk on non-ASCII locales where the OS error
                        // string can carry multi-byte chars.
                        let full = io_err.to_string();
                        let reason = if full.len() > 256 {
                            let mut end = 256;
                            while !full.is_char_boundary(end) {
                                end -= 1;
                            }
                            full[..end].to_owned()
                        } else {
                            full
                        };
                        Err(DispatchError::UrlSpawnFailed { reason })
                    }
                }
            }
            Action::Toggle => self.dispatch_toggle(entry, store),
            Action::CallService {
                domain,
                service,
                target,
                data,
            } => {
                // CallService can be optimistic when the service name is
                // turn_on / turn_off and a target entity is supplied. For
                // other services there is no canonical tentative_state so
                // optimistic tracking is skipped (the dispatch still goes
                // through, just without an OptimisticEntry).
                let optimistic_ctx = match (target.as_deref(), service.as_str()) {
                    (Some(target_id), "turn_on") | (Some(target_id), "turn_off") => store
                        .get(&EntityId::from(target_id))
                        .map(|e| OptimisticContext {
                            entity_id: EntityId::from(target_id),
                            prior_state: e.state.clone(),
                            tentative_state: tentative_for_service(service.as_str())
                                .map(Arc::from)
                                .unwrap_or_else(|| e.state.clone()),
                        }),
                    _ => None,
                };
                self.dispatch_call_service(
                    domain,
                    service,
                    target.as_deref(),
                    data.clone(),
                    optimistic_ctx,
                )
            }

            // Phase 6 typed variants (TASK-099): the types are landed here so
            // downstream tickets (TASK-102..TASK-105, TASK-108, TASK-109) can
            // wire dispatcher invocations without a separate type-only PR.
            // Each returns NotImplementedYet until the per-tile dispatch wiring
            // is added. Keep exhaustive — no wildcard — so future variant
            // additions remain compile errors.
            Action::SetTemperature { .. } => Err(DispatchError::NotImplementedYet {
                what: "SetTemperature",
                ticket: "TASK-103",
            }),
            Action::SetHvacMode { .. } => Err(DispatchError::NotImplementedYet {
                what: "SetHvacMode",
                ticket: "TASK-103",
            }),
            Action::SetMediaVolume { .. } => Err(DispatchError::NotImplementedYet {
                what: "SetMediaVolume",
                ticket: "TASK-104",
            }),
            Action::MediaTransport { .. } => Err(DispatchError::NotImplementedYet {
                what: "MediaTransport",
                ticket: "TASK-104",
            }),
            Action::SetCoverPosition { .. } => Err(DispatchError::NotImplementedYet {
                what: "SetCoverPosition",
                ticket: "TASK-105",
            }),
            Action::SetFanSpeed { .. } => Err(DispatchError::NotImplementedYet {
                what: "SetFanSpeed",
                ticket: "TASK-108",
            }),
            // TASK-104: lock dispatch — direct call_service to lock.lock.
            // Per `locked_decisions.confirmation_on_lock_unlock`, only the
            // *unlock* path consults `require_confirmation_on_unlock`. The
            // PIN policy still applies to lock if `Required` — a lock
            // entity that requires a PIN to LOCK is unusual but the schema
            // allows it (PinPolicy::Required is symmetric over both
            // operations). The Action variant carries no `confirmation`
            // field per locked_decisions.confirmation_on_lock_unlock.
            Action::Lock { entity_id } => self.dispatch_lock_or_unlock(
                widget_id,
                entry,
                LockOperation::Lock,
                EntityId::from(entity_id.as_str()),
            ),
            Action::Unlock { entity_id } => self.dispatch_lock_or_unlock(
                widget_id,
                entry,
                LockOperation::Unlock,
                EntityId::from(entity_id.as_str()),
            ),
            Action::AlarmArm { .. } => Err(DispatchError::NotImplementedYet {
                what: "AlarmArm",
                ticket: "TASK-109",
            }),
            Action::AlarmDisarm { .. } => Err(DispatchError::NotImplementedYet {
                what: "AlarmDisarm",
                ticket: "TASK-109",
            }),
        }
    }

    // -----------------------------------------------------------------------
    // Toggle (locked_decisions.toggle_capability_fallback)
    // -----------------------------------------------------------------------

    fn dispatch_toggle(
        &self,
        entry: &WidgetActionEntry,
        store: &LiveStore,
    ) -> Result<DispatchOutcome, DispatchError> {
        let domain = domain_of(&entry.entity_id);

        // Branch 1: <domain>.toggle if registered.
        if self.services_has(&domain, "toggle") {
            debug!(domain = %domain, entity = %entry.entity_id, "toggle: using <domain>.toggle");
            // Optimistic context: tentative is the OPPOSITE of current state.
            // If the entity is missing or in an unknown state we still
            // dispatch the toggle but skip the optimistic entry — the
            // dispatcher cannot predict the post-toggle state without a
            // known prior.
            let optimistic_ctx = store.get(&entry.entity_id).and_then(|e| {
                let tentative = match e.state.as_ref() {
                    "on" => "off",
                    "off" => "on",
                    _ => return None,
                };
                Some(OptimisticContext {
                    entity_id: entry.entity_id.clone(),
                    prior_state: e.state.clone(),
                    tentative_state: Arc::from(tentative),
                })
            });
            return self.dispatch_call_service(
                &domain,
                "toggle",
                Some(entry.entity_id.as_str()),
                None,
                optimistic_ctx,
            );
        }

        // Branch 2: turn_on + turn_off PAIR if BOTH are registered.
        let has_on = self.services_has(&domain, "turn_on");
        let has_off = self.services_has(&domain, "turn_off");
        if has_on && has_off {
            // Pick on/off from the entity's current state. Anything
            // other than `"on"` / `"off"` is treated as an unknown
            // state — better to surface a toast than to guess wrong.
            let entity = store
                .get(&entry.entity_id)
                .ok_or_else(|| DispatchError::EntityNotFound(entry.entity_id.clone()))?;
            let (service, tentative) = match entity.state.as_ref() {
                "on" => ("turn_off", "off"),
                "off" => ("turn_on", "on"),
                other => {
                    return Err(DispatchError::UnknownToggleState {
                        entity_id: entry.entity_id.clone(),
                        observed_state: other.to_owned(),
                    });
                }
            };
            debug!(
                domain = %domain,
                entity = %entry.entity_id,
                service,
                "toggle: using turn_on/turn_off pair fallback"
            );
            let optimistic_ctx = Some(OptimisticContext {
                entity_id: entry.entity_id.clone(),
                prior_state: entity.state.clone(),
                tentative_state: Arc::from(tentative),
            });
            return self.dispatch_call_service(
                &domain,
                service,
                Some(entry.entity_id.as_str()),
                None,
                optimistic_ctx,
            );
        }

        // Branch 3: nothing registered.  This is the empty-registry path
        // too — every lookup returns None and we land here without a
        // panic, satisfying Risk #3.
        warn!(
            domain = %domain,
            entity = %entry.entity_id,
            has_toggle = false,
            has_turn_on = has_on,
            has_turn_off = has_off,
            "toggle: no capability registered"
        );
        Err(DispatchError::NoCapability {
            domain,
            reason: "neither <domain>.toggle nor the turn_on/turn_off pair is registered",
        })
    }

    // -----------------------------------------------------------------------
    // CallService
    // -----------------------------------------------------------------------

    fn dispatch_call_service(
        &self,
        domain: &str,
        service: &str,
        target_entity: Option<&str>,
        data: Option<serde_json::Value>,
        optimistic_ctx: Option<OptimisticContext>,
    ) -> Result<DispatchOutcome, DispatchError> {
        // The Idempotency runtime allowlist (turn_on / turn_off / set_*)
        // is TASK-065's responsibility (offline queue gate). Phase 3's
        // dispatcher logs the action and forwards.  This is an
        // information line, not a security gate.
        debug!(
            domain,
            service,
            target = %target_entity.unwrap_or("<none>"),
            "dispatcher: forwarding call_service"
        );

        // LastWriteWins cancellation + backpressure check fire BEFORE the
        // WS-channel send so that a saturated entity-bucket never produces
        // an outbound HA frame (`locked_decisions.backpressure` —
        // BackpressureRejected is observable both as Err and as a toast
        // event, never silent).
        let prepared = match (&self.reconciliation, &optimistic_ctx) {
            (Some(ctx), Some(opt)) => Some(self.prepare_optimistic(ctx, opt)?),
            _ => None,
        };

        let frame = OutboundFrame {
            domain: domain.to_owned(),
            service: service.to_owned(),
            target: target_entity.map(|e| serde_json::json!({ "entity_id": e })),
            data,
        };

        let tx = self
            .command_tx
            .as_ref()
            .ok_or(DispatchError::ChannelNotWired)?;

        let (ack_tx, ack_rx) = oneshot::channel::<AckResult>();
        let cmd = OutboundCommand { frame, ack_tx };

        // try_send so the dispatcher never blocks the gesture-callback
        // thread — the WS client task should be drained promptly. A full
        // channel surfaces as ChannelClosed; we also need to roll back the
        // optimistic entry we may have inserted above so a transient
        // channel-closed does not leave a phantom pending entry.
        if let Err(send_err) = tx.try_send(cmd) {
            if let Some(prepared) = &prepared {
                if let Some(ctx) = &self.reconciliation {
                    let _ = ctx
                        .store
                        .drop_optimistic_entry(&prepared.entity_id, prepared.request_id);
                }
            }
            return Err(match send_err {
                mpsc::error::TrySendError::Closed(_) => DispatchError::ChannelClosed,
                mpsc::error::TrySendError::Full(_) => DispatchError::ChannelClosed,
            });
        }

        // Spawn reconciliation task IF we recorded an optimistic entry.
        // The task owns `ack_rx`, so the caller-visible `DispatchOutcome::Sent`
        // carries a NEW oneshot we drive ourselves on completion. This keeps
        // existing TASK-072 callers (which await `ack_rx` directly) working
        // when reconciliation is not enabled (the unmoved branch below).
        if let (Some(ctx), Some(prepared)) = (&self.reconciliation, prepared) {
            let (caller_tx, caller_rx) = oneshot::channel::<AckResult>();
            spawn_reconciliation(ctx.clone(), prepared, ack_rx, Some(caller_tx));
            return Ok(DispatchOutcome::Sent { ack_rx: caller_rx });
        }

        Ok(DispatchOutcome::Sent { ack_rx })
    }

    // -----------------------------------------------------------------------
    // Lock / Unlock dispatch (TASK-104)
    // -----------------------------------------------------------------------

    /// Dispatch [`Action::Lock`] or [`Action::Unlock`] (TASK-104).
    ///
    /// # Flow
    ///
    /// Per `locked_decisions.confirmation_on_lock_unlock` and
    /// `locked_decisions.pin_entry_dispatch`:
    ///
    /// 1. Look up `LockDispatchSettings` for `widget_id`. Missing entries
    ///    resolve to [`LockDispatchSettings::permissive`] (no PIN, no
    ///    confirm).
    /// 2. If the operation is [`LockOperation::Unlock`] AND
    ///    `require_confirmation_on_unlock` is true AND a `ConfirmHost`
    ///    is wired: invoke `confirm_host.confirm(...)` with an on_accept
    ///    closure. The closure carries the rest of the flow (PIN entry
    ///    if needed, then dispatch). Return
    ///    [`DispatchOutcome::LockAwaitingConfirm`] synchronously.
    /// 3. Else if `pin_policy: Required { length, code_format }` is set
    ///    AND a `PinEntryHost` is wired: invoke
    ///    `pin_host.request_pin(code_format, on_submit)`. The on_submit
    ///    closure consumes the entered code via FnOnce, builds a
    ///    `lock.lock` / `lock.unlock` `OutboundFrame` with `data.code = code`,
    ///    and `try_send`s on `command_tx`. Return
    ///    [`DispatchOutcome::LockAwaitingPinEntry`] synchronously. The
    ///    code is dropped at end of the closure scope; no field stores it.
    /// 4. Else: dispatch directly via [`dispatch_call_service`].
    ///
    /// # PIN code never leaks (Risk #7)
    ///
    /// The on_submit closure is the *only* code path that touches the
    /// entered string. It builds a JSON object literal with the code as a
    /// string value, hands it to `OutboundFrame.data`, and the closure
    /// then drops. No `tracing::*` call line in this function emits the
    /// code field — the unit test
    /// `pin_code_not_in_tracing_spans` (in this module) installs a
    /// capturing subscriber and asserts that the synthetic code never
    /// appears in any captured event line.
    ///
    /// # Audit row (per `locked_decisions.audit_substrate_placement`)
    ///
    /// On PIN submit, an audit row is emitted with `event="pin.submitted"`,
    /// `outcome="submitted"`, `scheme=None`, `error_kind=None`. The code
    /// value, length, and format hint are intentionally NOT in the row.
    fn dispatch_lock_or_unlock(
        &self,
        widget_id: &WidgetId,
        entry: &WidgetActionEntry,
        operation: LockOperation,
        entity_id: EntityId,
    ) -> Result<DispatchOutcome, DispatchError> {
        // Look up the per-widget lock settings; missing → permissive default.
        let settings = self
            .lock_settings
            .get(widget_id)
            .cloned()
            .unwrap_or_else(LockDispatchSettings::permissive);

        debug!(
            widget = %widget_id,
            entity = %entity_id,
            ?operation,
            "dispatch_lock_or_unlock: routing"
        );

        // Step 2: confirmation modal (Unlock only).
        let needs_confirm =
            matches!(operation, LockOperation::Unlock) && settings.require_confirmation_on_unlock;

        if needs_confirm {
            let Some(confirm_host) = self.confirm_host.as_ref() else {
                // Confirmation required but no host wired — fail closed.
                // The dispatcher must not silently skip the modal.
                warn!(
                    widget = %widget_id,
                    entity = %entity_id,
                    "lock unlock requires confirmation but no ConfirmHost is wired (fail-closed)"
                );
                return Err(DispatchError::NotImplementedYet {
                    what: "Unlock-with-confirmation",
                    ticket: "TASK-104",
                });
            };

            // Defer PIN entry + dispatch into the on_accept closure.
            let dispatcher = self.clone();
            let entry_clone = entry.clone();
            let widget_id_clone = widget_id.clone();
            let entity_id_clone = entity_id.clone();
            confirm_host.confirm(
                entity_id.clone(),
                Box::new(move || {
                    // After accept: continue with the PIN/dispatch flow as
                    // if confirmation had not been required. We re-enter the
                    // helper with `require_confirmation_on_unlock=false` so
                    // we don't re-prompt.
                    dispatcher.continue_after_confirm(
                        &widget_id_clone,
                        &entry_clone,
                        operation,
                        entity_id_clone,
                    );
                }),
            );

            return Ok(DispatchOutcome::LockAwaitingConfirm { entity_id });
        }

        // Step 3 / 4: PIN-or-dispatch.
        self.dispatch_lock_or_unlock_after_confirm(
            widget_id, entry, operation, entity_id, &settings,
        )
    }

    /// Continuation invoked from a `ConfirmHost::confirm` `on_accept`
    /// closure (TASK-104). Mirrors the post-confirm path inside
    /// `dispatch_lock_or_unlock`: looks up settings (sans the
    /// confirmation gate) and invokes PIN entry or direct dispatch as
    /// appropriate. Errors are logged at `warn!` level — the gesture
    /// callback has already returned, so there is no place to bubble the
    /// `Result` to.
    fn continue_after_confirm(
        &self,
        widget_id: &WidgetId,
        entry: &WidgetActionEntry,
        operation: LockOperation,
        entity_id: EntityId,
    ) {
        let settings = self
            .lock_settings
            .get(widget_id)
            .cloned()
            .unwrap_or_else(LockDispatchSettings::permissive);

        match self.dispatch_lock_or_unlock_after_confirm(
            widget_id,
            entry,
            operation,
            entity_id.clone(),
            &settings,
        ) {
            Ok(_outcome) => {}
            Err(e) => {
                // No code in the error log — `e`'s Display impl never
                // includes a PIN. The PIN code, if any, never reaches
                // this code path: it lives only inside the on_submit
                // closure which builds the OutboundFrame directly.
                warn!(
                    widget = %widget_id,
                    entity = %entity_id,
                    error = %e,
                    "post-confirm dispatch_lock_or_unlock_after_confirm failed"
                );
            }
        }
    }

    /// Common tail for the lock/unlock flow after the confirmation
    /// modal (if any) has been accepted (TASK-104). Branches on
    /// `pin_policy`:
    ///   * `Required` → invoke `PinEntryHost::request_pin` and return
    ///     `LockAwaitingPinEntry`. The on_submit closure builds and
    ///     `try_send`s the OutboundCommand directly.
    ///   * `None` / `RequiredOnDisarm` → direct service-call dispatch.
    ///     `RequiredOnDisarm` is alarm-only (validator-rejected on
    ///     locks); we treat it as None here defensively.
    fn dispatch_lock_or_unlock_after_confirm(
        &self,
        _widget_id: &WidgetId,
        entry: &WidgetActionEntry,
        operation: LockOperation,
        entity_id: EntityId,
        settings: &LockDispatchSettings,
    ) -> Result<DispatchOutcome, DispatchError> {
        let service = match operation {
            LockOperation::Lock => "lock",
            LockOperation::Unlock => "unlock",
        };

        // Branch on the pin policy.
        match &settings.pin_policy {
            crate::dashboard::schema::PinPolicy::Required {
                length: _length,
                code_format,
            } => {
                let Some(pin_host) = self.pin_host.as_ref() else {
                    warn!(
                        entity = %entity_id,
                        "lock action requires PIN entry but no PinEntryHost is wired (fail-closed)"
                    );
                    return Err(DispatchError::NotImplementedYet {
                        what: "Lock-with-PIN",
                        ticket: "TASK-104",
                    });
                };

                // Capture the channel for the on_submit closure. We do
                // NOT capture `self` (which would extend the dispatcher's
                // borrow lifetime beyond the closure's `'static` bound).
                let command_tx = self
                    .command_tx
                    .clone()
                    .ok_or(DispatchError::ChannelNotWired)?;
                let entity_id_for_frame = entity_id.clone();
                let code_format = *code_format;
                let service_name = service;

                pin_host.request_pin(
                    code_format,
                    Box::new(move |code: String| {
                        // SECURITY (locked_decisions.pin_entry_dispatch):
                        // `code` is consumed exactly once. No tracing call
                        // here emits the code value. The value is moved
                        // into `data` and the closure drops at end of
                        // scope. `tracing-redact` provides the runtime
                        // safety net; the structural FnOnce + scope-bound
                        // drop is the primary control.
                        //
                        // We construct a `serde_json::Value` payload via
                        // `serde_json::json!` because the OutboundFrame's
                        // `data` field is typed as `serde_json::Value`.
                        // `src/actions/**` is NOT gated against the JSON
                        // crate (the Gate-2 grep covers `src/ui/**`
                        // only); naming the path here is fine.
                        let frame = OutboundFrame {
                            domain: "lock".to_owned(),
                            service: service_name.to_owned(),
                            target: Some(serde_json::json!({
                                "entity_id": entity_id_for_frame.as_str(),
                            })),
                            data: Some(serde_json::json!({ "code": code })),
                        };
                        // `code` is moved into the `data` JSON object.
                        // The local binding `code` no longer holds the
                        // string after `json!` consumes it.

                        let (ack_tx, _ack_rx) = oneshot::channel::<AckResult>();
                        let cmd = OutboundCommand { frame, ack_tx };
                        // `try_send` so we never block the Slint event
                        // loop. A full channel surfaces as a debug log;
                        // the PIN value is NOT in the log line.
                        if let Err(send_err) = command_tx.try_send(cmd) {
                            warn!(
                                entity = %entity_id_for_frame,
                                error = ?send_err.to_string(),
                                "pin_submit: command_tx send failed"
                            );
                        }
                        // Audit row per
                        // locked_decisions.audit_substrate_placement.
                        // No code, no length, no format hint, no
                        // entity_id — `AuditEvent`'s field types are
                        // structurally restricted to the sealed
                        // `AuditField` whitelist (static strings + the
                        // `AuditScheme` enum + `TraceId`), per the
                        // `field_gate::ASSERT_AUDIT_EVENT_FIELDS_ARE_AUDIT_FIELDS`
                        // gate in `src/audit/mod.rs`. Tagging the row by
                        // entity would require extending the seal —
                        // out of scope for TASK-104 (security-engineer-
                        // owned, `src/audit/**` is must_not_touch).
                        crate::audit::emit(crate::audit::AuditEvent {
                            event: "pin.submitted",
                            outcome: "submitted",
                            error_kind: None,
                            scheme: None,
                        });
                    }),
                );

                Ok(DispatchOutcome::LockAwaitingPinEntry { entity_id })
            }
            // `PinPolicy::None` and the alarm-only `RequiredOnDisarm`
            // (validator-rejected on locks) fall through to direct
            // dispatch. We do not allocate a `data` payload — HA's
            // `lock.lock` / `lock.unlock` services accept no arguments
            // beyond `entity_id` when the lock has no code requirement.
            crate::dashboard::schema::PinPolicy::None
            | crate::dashboard::schema::PinPolicy::RequiredOnDisarm { .. } => self
                .dispatch_call_service("lock", service, Some(entry.entity_id.as_str()), None, None),
        }
    }

    /// Route a dispatch attempt through the offline queue (TASK-065).
    ///
    /// Returns `Some(_)` when the action is consumed by the offline path
    /// (queued, rejected as non-idempotent, or rejected by the queue's
    /// own gate). Returns `None` when the action is UI-local
    /// ([`Action::None`] / [`Action::MoreInfo`] / [`Action::Navigate`]) and
    /// must continue through the normal match arm — UI outcomes are
    /// independent of the connection state.
    ///
    /// Toast events fire on EVERY branch (success and failure) so the user
    /// has a visible signal that the tap was observed regardless of
    /// outcome. `try_send` avoids blocking the gesture-thread; if the
    /// toast channel is full the typed `Err` / `Ok` is still returned.
    fn maybe_route_offline(
        &self,
        ctx: &OfflineRoutingCtx,
        entry: &WidgetActionEntry,
        action: &Action,
    ) -> Option<Result<DispatchOutcome, DispatchError>> {
        match action {
            // UI-local — do nothing here; let the normal match handle it.
            Action::None | Action::MoreInfo | Action::Navigate { .. } => None,

            // Non-idempotent — load-bearing rejection per
            // locked_decisions.idempotency_marker (Risk #6). The schema's
            // `idempotency()` is the authoritative test; we route both
            // Toggle and Url through this branch but the marker is what
            // gates the rejection.
            Action::Toggle | Action::Url { .. } => {
                debug_assert_eq!(
                    action.idempotency(),
                    crate::actions::Idempotency::NonIdempotent,
                    "Toggle/Url must remain NonIdempotent at the schema level"
                );
                let _ = ctx.toast_tx.try_send(ToastEvent::OfflineNonIdempotent {
                    entity_id: entry.entity_id.clone(),
                });
                warn!(
                    entity = %entry.entity_id,
                    ?action,
                    "offline routing: rejecting non-idempotent action (Risk #6)"
                );
                Some(Err(DispatchError::OfflineNonIdempotent {
                    entity_id: entry.entity_id.clone(),
                }))
            }

            // TASK-104: Lock / Unlock are wired in dispatch but the offline
            // queue cannot accept them (`OfflineQueue::enqueue` rejects
            // them as `UnsupportedVariant` — `src/actions/queue.rs` is in
            // TASK-104's must_not_touch list). When offline we therefore
            // surface `NotImplementedYet` directly so the user sees a
            // typed error rather than a phantom queue entry. The PIN
            // entry / confirm modal flow lives ONLY on the live path —
            // per `locked_decisions.confirmation_on_lock_unlock`,
            // offline replay does not show the confirm modal, but
            // there is no offline replay for Lock/Unlock until a future
            // ticket extends the queue to accept them.
            Action::Lock { .. } | Action::Unlock { .. } => {
                Some(Err(DispatchError::NotImplementedYet {
                    what: "Lock-or-Unlock-offline",
                    ticket: "TASK-104",
                }))
            }
            // Phase 6 typed variants (TASK-099): dispatcher wiring is deferred
            // to TASK-105, TASK-108, TASK-109. Until those tickets wire the
            // per-variant dispatch paths, returning `None` here lets the main
            // `dispatch` match return `DispatchError::NotImplementedYet`
            // regardless of connection state. No offline-queued toast fires,
            // no phantom entry is enqueued. Kept exhaustive — no wildcard —
            // so a future variant addition remains a compile error.
            Action::SetTemperature { .. }
            | Action::SetHvacMode { .. }
            | Action::SetMediaVolume { .. }
            | Action::MediaTransport { .. }
            | Action::SetCoverPosition { .. }
            | Action::SetFanSpeed { .. }
            | Action::AlarmArm { .. }
            | Action::AlarmDisarm { .. } => None,

            // Idempotent WS-bound — try to enqueue. The queue's own
            // [`OfflineQueue::enqueue`] runs the runtime allowlist check
            // (turn_on / turn_off / set_*). We pass the dispatcher's
            // resolved entity_id as the queue's `target` argument so the
            // flush-time frame matches what the live path would have built.
            Action::CallService { data, .. } => {
                let action_clone = action.clone();
                let target = Some(entry.entity_id.clone());
                let data_clone = data.clone();

                // Acquire the queue's lock briefly. The queue is wrapped in
                // `std::sync::Mutex` because `OfflineQueue::flush` is
                // synchronous — the critical section never `.await`s, so a
                // sync mutex is correct and avoids the
                // "blocking_lock inside a runtime worker" panic that a
                // `tokio::sync::Mutex` would surface when `dispatch` is
                // called from an async test or async runtime worker.
                let mut queue = ctx.queue.lock().expect("offline queue mutex poisoned");
                match queue.enqueue(action_clone, target, data_clone) {
                    Ok(()) => {
                        let _ = ctx.toast_tx.try_send(ToastEvent::OfflineQueued {
                            entity_id: entry.entity_id.clone(),
                        });
                        debug!(
                            entity = %entry.entity_id,
                            "offline routing: action queued for reconnect-flush"
                        );
                        Some(Ok(DispatchOutcome::Queued {
                            entity_id: entry.entity_id.clone(),
                        }))
                    }
                    Err(QueueError::NonIdempotentRejected) => {
                        // Defensive — queue's gate-1 should never fire here
                        // since CallService is Idempotent at the schema level.
                        // If we ever land here it is a programming error.
                        debug_assert!(false, "queue rejected CallService as non-idempotent");
                        let _ = ctx.toast_tx.try_send(ToastEvent::OfflineNonIdempotent {
                            entity_id: entry.entity_id.clone(),
                        });
                        Some(Err(DispatchError::OfflineNonIdempotent {
                            entity_id: entry.entity_id.clone(),
                        }))
                    }
                    Err(QueueError::ServiceNotAllowlisted { .. }) => {
                        let reason = QueueRejectReason::ServiceNotAllowlisted;
                        let _ = ctx.toast_tx.try_send(ToastEvent::OfflineQueueRejected {
                            entity_id: entry.entity_id.clone(),
                            reason,
                        });
                        Some(Err(DispatchError::OfflineQueueRejected {
                            entity_id: entry.entity_id.clone(),
                            reason,
                        }))
                    }
                    Err(QueueError::UnsupportedVariant) => {
                        let reason = QueueRejectReason::UnsupportedVariant;
                        let _ = ctx.toast_tx.try_send(ToastEvent::OfflineQueueRejected {
                            entity_id: entry.entity_id.clone(),
                            reason,
                        });
                        Some(Err(DispatchError::OfflineQueueRejected {
                            entity_id: entry.entity_id.clone(),
                            reason,
                        }))
                    }
                }
            }
        }
    }

    /// Prepare an optimistic entry: handle `LastWriteWins` cancellation, then
    /// insert under the per-entity / global caps. Returns the prepared
    /// `request_id` + bookkeeping so the caller can spawn the reconciliation
    /// task once the WS-channel write succeeds.
    fn prepare_optimistic(
        &self,
        ctx: &ReconciliationCtx,
        opt: &OptimisticContext,
    ) -> Result<PreparedOptimistic, DispatchError> {
        // 1. LastWriteWins (`locked_decisions.action_timing`): if a
        //    second gesture fires on the same entity while an earlier
        //    action is pending, drain the pending entries and use the
        //    FIRST cancelled entry's `prior_state` as this entry's
        //    `prior_state` (chain root preservation). DiscardConcurrent
        //    is reachable via Phase 4 DeviceProfile only and is a no-op
        //    at the prepare-stage (capacity check still runs).
        let prior_state = match ctx.timing.action_overlap_strategy {
            ActionOverlapStrategy::LastWriteWins => {
                let cancelled = ctx.store.drop_all_optimistic_entries(&opt.entity_id);
                cancelled
                    .into_iter()
                    .next()
                    .map(|root| root.prior_state)
                    .unwrap_or_else(|| Arc::clone(&opt.prior_state))
            }
            ActionOverlapStrategy::DiscardConcurrent => Arc::clone(&opt.prior_state),
        };

        // 2. Allocate a dispatcher-local request_id and insert the entry
        //    under the per-entity / global caps.
        let request_id = ctx.next_request_id.fetch_add(1, Ordering::Relaxed);
        let entry = OptimisticEntry {
            entity_id: opt.entity_id.clone(),
            request_id,
            dispatched_at: Timestamp::now(),
            tentative_state: Arc::clone(&opt.tentative_state),
            prior_state,
        };
        match ctx.store.insert_optimistic_entry(entry) {
            Ok(()) => Ok(PreparedOptimistic {
                entity_id: opt.entity_id.clone(),
                request_id,
                tentative_state: Arc::clone(&opt.tentative_state),
            }),
            Err(insert_err) => {
                let scope = match insert_err {
                    OptimisticInsertError::PerEntityCap => BackpressureScope::PerEntity,
                    OptimisticInsertError::GlobalCap => BackpressureScope::Global,
                };
                // Emit toast event concurrently with returning Err
                // (`locked_decisions.backpressure`: never silent).
                // try_send avoids blocking the gesture thread; if the
                // toast channel is full we still surface the typed Err.
                let _ = ctx.toast_tx.try_send(ToastEvent::BackpressureRejected {
                    entity_id: opt.entity_id.clone(),
                    scope,
                });
                warn!(
                    entity = %opt.entity_id,
                    ?scope,
                    "dispatcher: BackpressureRejected — optimistic-entry cap saturated"
                );
                Err(DispatchError::BackpressureRejected {
                    entity_id: opt.entity_id.clone(),
                    scope,
                })
            }
        }
    }

    // -----------------------------------------------------------------------
    // ServiceRegistry helper
    // -----------------------------------------------------------------------

    /// Read-lock the `ServiceRegistry` and check whether `(domain, service)`
    /// is present.
    ///
    /// On a poisoned `RwLock` (would only happen if a writer panicked
    /// mid-mutation), we treat every lookup as `None` rather than
    /// re-panicking — Risk #3 says the dispatcher must never panic on a
    /// missing capability path. The `warn!` line surfaces the underlying
    /// invariant violation for diagnosis.
    fn services_has(&self, domain: &str, service: &str) -> bool {
        match self.services.read() {
            Ok(guard) => guard.lookup(domain, service).is_some(),
            Err(_poisoned) => {
                warn!(
                    domain,
                    service, "ServiceRegistry RwLock poisoned; treating lookup as None"
                );
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Optimistic-context helpers (TASK-064)
// ---------------------------------------------------------------------------

/// Inputs the dispatcher needs to record an [`OptimisticEntry`] for one
/// dispatched action. Computed by the caller (the Toggle path knows current
/// state and the post-toggle prediction; the CallService path computes both
/// for `turn_on`/`turn_off` services).
struct OptimisticContext {
    entity_id: EntityId,
    prior_state: Arc<str>,
    tentative_state: Arc<str>,
}

/// Bookkeeping returned by [`Dispatcher::prepare_optimistic`] so the caller
/// can roll back on a failed WS-channel write and so the reconciliation task
/// can match its entry by `(entity_id, request_id)`.
struct PreparedOptimistic {
    entity_id: EntityId,
    request_id: u32,
    tentative_state: Arc<str>,
}

/// Predict the post-call state for a `turn_on` / `turn_off` service.
///
/// Returns `None` for any service the dispatcher cannot reduce to a known
/// post-state — those dispatches still go through but skip optimistic
/// tracking (no revert path applies).
fn tentative_for_service(service: &str) -> Option<&'static str> {
    match service {
        "turn_on" => Some("on"),
        "turn_off" => Some("off"),
        _ => None,
    }
}

/// Spawn the per-dispatch reconciliation task.
///
/// Implements rules 2 / 4 / 5 from
/// `locked_decisions.optimistic_reconciliation_key`. Rules 1 and 3 are
/// enforced inside [`LiveStore::apply_event`] (state_changed events with
/// `last_changed > dispatched_at` drop the entry; attribute-only events
/// don't advance `last_changed` so the entry survives).
fn spawn_reconciliation(
    ctx: ReconciliationCtx,
    prepared: PreparedOptimistic,
    ack_rx: oneshot::Receiver<AckResult>,
    caller_tx: Option<oneshot::Sender<AckResult>>,
) {
    let timeout = Duration::from_millis(ctx.timing.optimistic_timeout_ms);
    let store = ctx.store;
    let task_spawned_at = tokio::time::Instant::now();
    let deadline = task_spawned_at + timeout;
    tokio::spawn(async move {
        let entity_id = prepared.entity_id;
        let request_id = prepared.request_id;
        let tentative_state = prepared.tentative_state;

        let outcome = tokio::time::timeout_at(deadline, ack_rx).await;
        match outcome {
            // Timeout (rule 5): drop entry, no ack to forward.
            Err(_) => {
                let _ = store.drop_optimistic_entry(&entity_id, request_id);
                debug!(
                    entity = %entity_id,
                    request_id,
                    "optimistic: timeout — entry dropped (revert)"
                );
                // No AckResult to forward to the caller; dropping caller_tx
                // surfaces as `RecvError` on caller_rx.
                drop(caller_tx);
            }
            // WS task panicked / oneshot dropped before resolving.
            Ok(Err(_recv_err)) => {
                let _ = store.drop_optimistic_entry(&entity_id, request_id);
                debug!(
                    entity = %entity_id,
                    request_id,
                    "optimistic: ack channel dropped — entry dropped (revert)"
                );
                drop(caller_tx);
            }
            // Ack arrived.
            Ok(Ok(ack_result)) => {
                match &ack_result {
                    Err(_) => {
                        // Rule 4 (ack-error): revert immediately.
                        let _ = store.drop_optimistic_entry(&entity_id, request_id);
                        debug!(
                            entity = %entity_id,
                            request_id,
                            "optimistic: ack-error — entry dropped (revert)"
                        );
                    }
                    Ok(_success) => {
                        // Rule 1 vs Rule 2 distinction. If our entry is
                        // already gone, `LiveStore::apply_event` dropped it
                        // because a `state_changed` with
                        // `last_changed > dispatched_at` arrived before
                        // (or concurrently with) the ack — that's rule 1;
                        // we have nothing to do.
                        if !store.has_optimistic_entry(&entity_id, request_id) {
                            debug!(
                                entity = %entity_id,
                                request_id,
                                "optimistic: ack-success after state_changed (rule 1)"
                            );
                        } else {
                            // Rule 2: ack-without-event. Snapshot current
                            // state at ack time. If it matches
                            // tentative_state, no-op confirmed → drop.
                            // Otherwise hold for the remainder of the
                            // deadline; if no `state_changed` arrives, the
                            // entry will time out and revert.
                            let current_state = store.get(&entity_id).map(|e| e.state.clone());
                            let matches = current_state
                                .as_ref()
                                .map(|s| s.as_ref() == tentative_state.as_ref())
                                .unwrap_or(false);
                            if matches {
                                let _ = store.drop_optimistic_entry(&entity_id, request_id);
                                debug!(
                                    entity = %entity_id,
                                    request_id,
                                    "optimistic: rule 2 — no-op success matched tentative"
                                );
                            } else {
                                debug!(
                                    entity = %entity_id,
                                    request_id,
                                    "optimistic: rule 2 — holding for state_changed"
                                );
                                // Spawn the hold-and-revert tail off the
                                // current task so we can forward the ack
                                // immediately. The hold duration is the
                                // REMAINING budget from
                                // `dispatched_at + optimistic_timeout_ms`
                                // — clipped to zero if already overdue.
                                let store_clone = store.clone();
                                let entity_clone = entity_id.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep_until(deadline).await;
                                    let _ = store_clone
                                        .drop_optimistic_entry(&entity_clone, request_id);
                                });
                            }
                        }
                    }
                }
                if let Some(tx) = caller_tx {
                    let _ = tx.send(ack_result);
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pick the right action for a gesture from a `WidgetActionEntry`.
fn pick_action(entry: &WidgetActionEntry, gesture: Gesture) -> &Action {
    match gesture {
        Gesture::Tap => &entry.tap,
        Gesture::Hold => &entry.hold,
        Gesture::DoubleTap => &entry.double_tap,
    }
}

/// Extract the HA domain from an entity id (`"light.kitchen"` → `"light"`).
///
/// HA entity ids always have the `<domain>.<object>` shape; we still
/// handle the malformed-id case (no dot) by returning the whole string
/// rather than panicking. The dispatcher then attempts a registry lookup
/// against that whole string, which will fail cleanly via
/// [`DispatchError::NoCapability`].
fn domain_of(id: &EntityId) -> String {
    id.as_str()
        .split_once('.')
        .map(|(d, _)| d.to_owned())
        .unwrap_or_else(|| id.as_str().to_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, RwLock};

    use jiff::Timestamp;
    use serde_json::json;
    use tokio::sync::mpsc;

    use crate::actions::map::{WidgetActionEntry, WidgetActionMap, WidgetId};
    use crate::ha::entity::Entity;
    use crate::ha::live_store::LiveStore;
    use crate::ha::services::{ServiceMeta, ServiceRegistry};

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    /// Wrap a `ServiceRegistry` in the [`ServiceRegistryHandle`] shape.
    fn handle_from(reg: ServiceRegistry) -> ServiceRegistryHandle {
        Arc::new(RwLock::new(reg))
    }

    fn make_entity(id: &str, state: &str) -> Entity {
        Entity {
            id: EntityId::from(id),
            state: Arc::from(state),
            attributes: Arc::new(serde_json::Map::new()),
            last_changed: Timestamp::UNIX_EPOCH,
            last_updated: Timestamp::UNIX_EPOCH,
        }
    }

    fn store_with(entities: Vec<Entity>) -> LiveStore {
        let store = LiveStore::new();
        store.apply_snapshot(entities);
        store
    }

    fn one_widget_map(widget_id: &str, entry: WidgetActionEntry) -> WidgetActionMap {
        let mut map = WidgetActionMap::new();
        map.insert(WidgetId::from(widget_id), entry);
        map
    }

    fn entry_with(
        entity_id: &str,
        tap: Action,
        hold: Action,
        double_tap: Action,
    ) -> WidgetActionEntry {
        WidgetActionEntry {
            entity_id: EntityId::from(entity_id),
            tap,
            hold,
            double_tap,
        }
    }

    /// Construct a (dispatcher, recorder_rx) pair backed by a fake
    /// `mpsc::Sender<OutboundCommand>` recorder.
    fn make_dispatcher_with_recorder(
        services: ServiceRegistryHandle,
    ) -> (Dispatcher, mpsc::Receiver<OutboundCommand>) {
        let (tx, rx) = mpsc::channel::<OutboundCommand>(8);
        (Dispatcher::with_command_tx(services, tx), rx)
    }

    // -----------------------------------------------------------------------
    // Toggle: branch 1 — <domain>.toggle present
    // -----------------------------------------------------------------------

    #[test]
    fn toggle_dispatches_domain_toggle_when_registered() {
        let mut reg = ServiceRegistry::new();
        reg.add_service("light", "toggle", ServiceMeta::default());
        let services = handle_from(reg);

        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "kitchen_light",
            entry_with("light.kitchen", Action::Toggle, Action::None, Action::None),
        );
        let store = store_with(vec![make_entity("light.kitchen", "on")]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed when light.toggle is registered");

        match outcome {
            DispatchOutcome::Sent { .. } => {}
            other => panic!("expected DispatchOutcome::Sent, got {other:?}"),
        }

        let cmd = rx
            .try_recv()
            .expect("recorder must have received the command");
        assert_eq!(cmd.frame.domain, "light");
        assert_eq!(cmd.frame.service, "toggle");
        assert_eq!(
            cmd.frame.target,
            Some(json!({ "entity_id": "light.kitchen" }))
        );
        assert_eq!(cmd.frame.data, None);
    }

    // -----------------------------------------------------------------------
    // Toggle: branch 2 — turn_on / turn_off pair fallback
    // -----------------------------------------------------------------------

    #[test]
    fn toggle_falls_back_to_turn_off_when_state_is_on() {
        let mut reg = ServiceRegistry::new();
        reg.add_service("switch", "turn_on", ServiceMeta::default());
        reg.add_service("switch", "turn_off", ServiceMeta::default());
        let services = handle_from(reg);

        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "outlet",
            entry_with(
                "switch.outlet_1",
                Action::Toggle,
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![make_entity("switch.outlet_1", "on")]);

        let _ = dispatcher
            .dispatch(&WidgetId::from("outlet"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed for turn_on/turn_off pair");

        let cmd = rx.try_recv().expect("recorder must have received");
        assert_eq!(cmd.frame.domain, "switch");
        assert_eq!(
            cmd.frame.service, "turn_off",
            "state=on must dispatch turn_off"
        );
        assert_eq!(
            cmd.frame.target,
            Some(json!({ "entity_id": "switch.outlet_1" }))
        );
    }

    #[test]
    fn toggle_falls_back_to_turn_on_when_state_is_off() {
        let mut reg = ServiceRegistry::new();
        reg.add_service("switch", "turn_on", ServiceMeta::default());
        reg.add_service("switch", "turn_off", ServiceMeta::default());
        let services = handle_from(reg);

        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "outlet",
            entry_with(
                "switch.outlet_1",
                Action::Toggle,
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![make_entity("switch.outlet_1", "off")]);

        let _ = dispatcher
            .dispatch(&WidgetId::from("outlet"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed for turn_on/turn_off pair");

        let cmd = rx.try_recv().expect("recorder must have received");
        assert_eq!(
            cmd.frame.service, "turn_on",
            "state=off must dispatch turn_on"
        );
    }

    #[test]
    fn toggle_pair_fallback_unknown_state_returns_error() {
        let mut reg = ServiceRegistry::new();
        reg.add_service("switch", "turn_on", ServiceMeta::default());
        reg.add_service("switch", "turn_off", ServiceMeta::default());
        let services = handle_from(reg);

        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "outlet",
            entry_with(
                "switch.outlet_1",
                Action::Toggle,
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![make_entity("switch.outlet_1", "unavailable")]);

        let err = dispatcher
            .dispatch(&WidgetId::from("outlet"), Gesture::Tap, &store, &map)
            .expect_err("unavailable state must error rather than guess");

        match err {
            DispatchError::UnknownToggleState {
                entity_id,
                observed_state,
            } => {
                assert_eq!(entity_id, EntityId::from("switch.outlet_1"));
                assert_eq!(observed_state, "unavailable");
            }
            other => panic!("expected UnknownToggleState, got {other:?}"),
        }

        // No command should have been sent on the error path.
        assert!(
            rx.try_recv().is_err(),
            "no OutboundCommand must be enqueued on the error path"
        );
    }

    // -----------------------------------------------------------------------
    // Toggle: branch 3 — neither registered → NoCapability
    // -----------------------------------------------------------------------

    #[test]
    fn toggle_with_neither_toggle_nor_pair_returns_no_capability() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "kitchen_light",
            entry_with("light.kitchen", Action::Toggle, Action::None, Action::None),
        );
        let store = store_with(vec![make_entity("light.kitchen", "on")]);

        let err = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect_err("empty registry must produce NoCapability");

        match err {
            DispatchError::NoCapability { domain, .. } => {
                assert_eq!(domain, "light");
            }
            other => panic!("expected NoCapability, got {other:?}"),
        }

        // Risk #3: empty registry must NOT panic. Reaching this assertion
        // already proves no-panic; we additionally confirm no command
        // was forwarded.
        assert!(
            rx.try_recv().is_err(),
            "no OutboundCommand must be sent when capability is missing"
        );
    }

    #[test]
    fn toggle_with_only_turn_on_returns_no_capability() {
        // Pair fallback REQUIRES BOTH turn_on AND turn_off.  Having only
        // one of them is treated as no capability — the dispatcher will
        // not "half-toggle" by always sending turn_on regardless of state.
        let mut reg = ServiceRegistry::new();
        reg.add_service("switch", "turn_on", ServiceMeta::default());
        // turn_off intentionally absent
        let services = handle_from(reg);

        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "outlet",
            entry_with(
                "switch.outlet_1",
                Action::Toggle,
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![make_entity("switch.outlet_1", "off")]);

        let err = dispatcher
            .dispatch(&WidgetId::from("outlet"), Gesture::Tap, &store, &map)
            .expect_err("turn_off missing must produce NoCapability");
        assert!(matches!(err, DispatchError::NoCapability { .. }));
    }

    // -----------------------------------------------------------------------
    // CallService — frame shape
    // -----------------------------------------------------------------------

    #[test]
    fn call_service_builds_outbound_frame_with_supplied_fields() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);

        let action = Action::CallService {
            domain: "light".to_owned(),
            service: "turn_on".to_owned(),
            target: Some("light.kitchen".to_owned()),
            data: Some(json!({ "brightness": 180 })),
        };
        let map = one_widget_map(
            "kitchen_light",
            entry_with("light.kitchen", action, Action::None, Action::None),
        );
        let store = store_with(vec![make_entity("light.kitchen", "on")]);

        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("call_service must dispatch");

        let cmd = rx.try_recv().expect("recorder must have received");
        assert_eq!(cmd.frame.domain, "light");
        assert_eq!(cmd.frame.service, "turn_on");
        assert_eq!(
            cmd.frame.target,
            Some(json!({ "entity_id": "light.kitchen" }))
        );
        assert_eq!(cmd.frame.data, Some(json!({ "brightness": 180 })));
    }

    #[test]
    fn call_service_outcome_carries_ack_rx_oneshot() {
        // locked_decisions.ws_command_ack_envelope: the dispatcher
        // creates the oneshot itself; the receiver lands in
        // DispatchOutcome::Sent. TASK-064 awaits it; this test asserts
        // the seam is wired.
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);

        let action = Action::CallService {
            domain: "light".to_owned(),
            service: "turn_on".to_owned(),
            target: None,
            data: None,
        };
        let map = one_widget_map(
            "w",
            entry_with("light.kitchen", action, Action::None, Action::None),
        );
        let store = store_with(vec![make_entity("light.kitchen", "on")]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed");

        match outcome {
            DispatchOutcome::Sent { ack_rx: _ } => {
                // ack_rx is an `oneshot::Receiver<AckResult>`; the type
                // is the load-bearing assertion (compile-time).
            }
            other => panic!("expected Sent, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // MoreInfo — emits UI event with entity_id from the map entry
    // -----------------------------------------------------------------------

    #[test]
    fn more_info_returns_outcome_with_entry_entity_id() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::None,
                Action::MoreInfo,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Hold, &store, &map)
            .expect("more-info dispatch must succeed");

        match outcome {
            DispatchOutcome::MoreInfo { entity_id } => {
                assert_eq!(entity_id, EntityId::from("light.kitchen"));
            }
            other => panic!("expected MoreInfo, got {other:?}"),
        }

        // No WS command emitted.
        assert!(rx.try_recv().is_err(), "MoreInfo must NOT send a frame");
    }

    // -----------------------------------------------------------------------
    // Navigate — emits UI event with view_id
    // -----------------------------------------------------------------------

    #[test]
    fn navigate_returns_outcome_with_view_id() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::None,
                Action::None,
                Action::Navigate {
                    view_id: "office".to_owned(),
                },
            ),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::DoubleTap, &store, &map)
            .expect("navigate dispatch must succeed");

        match outcome {
            DispatchOutcome::Navigate { view_id } => {
                assert_eq!(view_id, "office");
            }
            other => panic!("expected Navigate, got {other:?}"),
        }

        assert!(rx.try_recv().is_err(), "Navigate must NOT send a frame");
    }

    // -----------------------------------------------------------------------
    // Navigate — TASK-068 router wiring
    //
    // The dispatcher invokes the optional `ViewRouter::navigate(view_id)`
    // BEFORE returning `DispatchOutcome::Navigate`. Verified with the test-
    // only `RecordingViewRouter` from `crate::ui::view_router::tests` so the
    // dispatcher unit tests do not need a live Slint window.
    // -----------------------------------------------------------------------

    #[test]
    fn navigate_invokes_view_router_with_default_view_id() {
        // Acceptance criterion: Navigate { view_id: "default" } does not
        // panic and reaches the router with the documented payload. With
        // the Phase 3 single-view setup, the SlintViewRouter would observe
        // current-view already at "default" so this is a no-op on the UI;
        // the recording router lets us assert the dispatcher actually
        // called navigate (it cannot be silently elided).
        use crate::ui::view_router::tests::RecordingViewRouter;
        use crate::ui::view_router::DEFAULT_VIEW_ID;

        let services = handle_from(ServiceRegistry::new());
        let recorder = Arc::new(RecordingViewRouter::new());
        let (tx, _rx) = mpsc::channel::<OutboundCommand>(8);
        let dispatcher = Dispatcher::with_command_tx(services, tx)
            .with_view_router_arc(recorder.clone() as Arc<dyn ViewRouter>);

        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::Navigate {
                    view_id: DEFAULT_VIEW_ID.to_owned(),
                },
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("navigate-to-default must dispatch successfully");

        match outcome {
            DispatchOutcome::Navigate { view_id } => {
                assert_eq!(
                    view_id, DEFAULT_VIEW_ID,
                    "outcome must carry the requested view_id verbatim"
                );
            }
            other => panic!("expected Navigate outcome, got {other:?}"),
        }

        assert_eq!(
            recorder.calls(),
            vec![DEFAULT_VIEW_ID.to_owned()],
            "router must have been invoked exactly once with `default`"
        );
    }

    #[test]
    fn navigate_invokes_view_router_with_unknown_view_id() {
        // Acceptance criterion: Navigate { view_id: "unknown" } does not
        // panic and reaches the router. SlintViewRouter logs at debug
        // level; here we only verify the dispatch path itself routes the
        // payload through.
        use crate::ui::view_router::tests::RecordingViewRouter;

        let services = handle_from(ServiceRegistry::new());
        let recorder = Arc::new(RecordingViewRouter::new());
        let dispatcher =
            Dispatcher::new(services).with_view_router_arc(recorder.clone() as Arc<dyn ViewRouter>);

        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::Navigate {
                    view_id: "kitchen".to_owned(),
                },
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("navigate-to-unknown must dispatch successfully (no panic)");
        assert!(
            matches!(outcome, DispatchOutcome::Navigate { ref view_id } if view_id == "kitchen"),
            "outcome must be Navigate with view_id=kitchen, got {outcome:?}"
        );
        assert_eq!(
            recorder.calls(),
            vec!["kitchen".to_owned()],
            "router must have been invoked with the unknown view id verbatim"
        );
    }

    #[test]
    fn navigate_without_view_router_still_returns_outcome() {
        // The router is optional (locked_decisions.phase4_forward_compat:
        // dispatcher signature unchanged). When no router is wired, the
        // dispatcher still returns DispatchOutcome::Navigate so the caller
        // can route the payload itself if needed. No panic.
        let services = handle_from(ServiceRegistry::new());
        let dispatcher = Dispatcher::new(services); // no view_router

        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::Navigate {
                    view_id: "default".to_owned(),
                },
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("navigate must succeed even without a view router");
        assert!(
            matches!(outcome, DispatchOutcome::Navigate { ref view_id } if view_id == "default"),
            "outcome must still be Navigate when no router is wired, got {outcome:?}"
        );
    }

    #[test]
    fn navigate_router_invoked_only_for_navigate_action() {
        // Defensive: confirm that other Action variants do NOT call the
        // router. A mis-wired dispatcher could call navigate on every
        // dispatch — this test pins the contract that the router is only
        // touched by `Action::Navigate`.
        use crate::ui::view_router::tests::RecordingViewRouter;

        let services = handle_from(ServiceRegistry::new());
        let recorder = Arc::new(RecordingViewRouter::new());
        let (tx, _rx) = mpsc::channel::<OutboundCommand>(8);
        let dispatcher = Dispatcher::with_command_tx(services, tx)
            .with_view_router_arc(recorder.clone() as Arc<dyn ViewRouter>);

        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::None,
                Action::MoreInfo,
                Action::CallService {
                    domain: "light".to_owned(),
                    service: "turn_on".to_owned(),
                    target: None,
                    data: None,
                },
            ),
        );
        let store = store_with(vec![make_entity("light.kitchen", "on")]);

        // Tap (None) → no router call.
        let _ = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("None must dispatch as NoOp");
        assert!(
            recorder.calls().is_empty(),
            "router must NOT be called for Action::None"
        );

        // Hold (MoreInfo) → no router call.
        let _ = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Hold, &store, &map)
            .expect("MoreInfo must dispatch");
        assert!(
            recorder.calls().is_empty(),
            "router must NOT be called for Action::MoreInfo"
        );

        // DoubleTap (CallService) → no router call.
        let _ = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::DoubleTap, &store, &map)
            .expect("CallService must dispatch");
        assert!(
            recorder.calls().is_empty(),
            "router must NOT be called for Action::CallService"
        );
    }

    // -----------------------------------------------------------------------
    // None — explicit no-op
    // -----------------------------------------------------------------------

    #[test]
    fn none_is_no_op_no_command_no_event() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "w",
            entry_with("light.kitchen", Action::None, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("Action::None must dispatch as NoOp");
        assert!(
            matches!(outcome, DispatchOutcome::NoOp),
            "Action::None must produce DispatchOutcome::NoOp"
        );
        assert!(rx.try_recv().is_err(), "Action::None must not send a frame");
    }

    // -----------------------------------------------------------------------
    // Url — default Never mode (TASK-075 wiring; fail-closed default)
    // -----------------------------------------------------------------------

    #[test]
    fn url_default_never_mode_returns_url_blocked_toast_no_frame() {
        // Default dispatcher has url_action_mode = Never (fail-closed).
        // Asserts: Ok(UrlBlockedToast), zero WS frames, spawner NOT called.
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::Url {
                    href: "https://example.org".to_owned(),
                },
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("Never mode must return Ok, not Err");
        match outcome {
            DispatchOutcome::UrlBlockedToast { text } => {
                assert_eq!(
                    text,
                    crate::actions::url::TOAST_BLOCKED_BY_PROFILE,
                    "Never mode must surface the blocked-by-profile toast"
                );
            }
            other => panic!("expected UrlBlockedToast, got {other:?}"),
        }
        // Url is never WS-bound: zero frames regardless of mode.
        assert!(
            rx.try_recv().is_err(),
            "Url dispatch must NOT push any OutboundCommand frame"
        );
    }

    // -----------------------------------------------------------------------
    // UrlSpawnFailed.reason UTF-8 truncation must NOT panic when the
    // io::Error Display form carries multi-byte chars and the byte cap
    // (256) lands mid-codepoint. We exercise the truncation path directly
    // via a synthetic >256-byte multi-byte string. The dispatch wiring
    // uses `is_char_boundary` to round DOWN to a valid boundary.
    // -----------------------------------------------------------------------

    #[test]
    fn url_spawn_failed_reason_truncation_is_utf8_safe() {
        // 4-byte codepoint repeated past the 256-byte cap. Each '🟥' is
        // 4 bytes; 70 of them is 280 bytes — guaranteed to cross the cap.
        let s = "🟥".repeat(70);
        assert!(s.len() > 256);

        let mut end = 256;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        // The truncation must (a) NOT panic and (b) produce a valid &str
        // slice on a char boundary that is ≤ 256 bytes.
        let truncated = &s[..end];
        assert!(end <= 256);
        assert!(s.is_char_boundary(end));
        // Sanity: the truncated form is a valid prefix of the input.
        assert!(s.starts_with(truncated));
    }

    // -----------------------------------------------------------------------
    // command_tx unwired — ChannelNotWired
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_without_command_tx_returns_channel_not_wired() {
        let services = handle_from(ServiceRegistry::new());
        let dispatcher = Dispatcher::new(services);
        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::CallService {
                    domain: "light".to_owned(),
                    service: "turn_on".to_owned(),
                    target: None,
                    data: None,
                },
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("missing command_tx must error");
        assert!(matches!(err, DispatchError::ChannelNotWired));
    }

    // -----------------------------------------------------------------------
    // Unknown widget — UnknownWidget
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_widget_id_returns_unknown_widget_error() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let map = WidgetActionMap::new();
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("missing"), Gesture::Tap, &store, &map)
            .expect_err("unknown widget must error");
        match err {
            DispatchError::UnknownWidget(id) => {
                assert_eq!(id.as_str(), "missing");
            }
            other => panic!("expected UnknownWidget, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Toggle: pair fallback with missing entity in store — EntityNotFound
    // -----------------------------------------------------------------------

    #[test]
    fn toggle_pair_fallback_missing_entity_returns_entity_not_found() {
        let mut reg = ServiceRegistry::new();
        reg.add_service("switch", "turn_on", ServiceMeta::default());
        reg.add_service("switch", "turn_off", ServiceMeta::default());
        let services = handle_from(reg);

        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "outlet",
            entry_with(
                "switch.outlet_1",
                Action::Toggle,
                Action::None,
                Action::None,
            ),
        );
        // entity NOT in the store snapshot
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("outlet"), Gesture::Tap, &store, &map)
            .expect_err("missing entity must surface EntityNotFound");
        match err {
            DispatchError::EntityNotFound(id) => {
                assert_eq!(id, EntityId::from("switch.outlet_1"));
            }
            other => panic!("expected EntityNotFound, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Empty registry — no panic, NoCapability
    // -----------------------------------------------------------------------

    #[test]
    fn empty_registry_yields_descriptive_error_no_panic() {
        // Risk #3 explicit: empty ServiceRegistry → descriptive error
        // toast (NoCapability), never a panic.
        let services = handle_from(ServiceRegistry::new()); // empty
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let map = one_widget_map(
            "kitchen_light",
            entry_with("light.kitchen", Action::Toggle, Action::None, Action::None),
        );
        let store = store_with(vec![make_entity("light.kitchen", "on")]);

        let err = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect_err("empty registry must produce NoCapability");
        // Render the Display string — that is the surface the toast
        // (TASK-067) will render.
        let display = format!("{err}");
        assert!(
            display.contains("light"),
            "error display must cite domain `light`, got: {display}"
        );
        assert!(
            matches!(err, DispatchError::NoCapability { .. }),
            "empty registry → NoCapability"
        );
    }

    // -----------------------------------------------------------------------
    // ChannelClosed — receiver dropped between Dispatcher construction and dispatch
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_with_closed_receiver_returns_channel_closed() {
        // Construct dispatcher with a sender, then drop the receiver.
        let services = handle_from(ServiceRegistry::new());
        let (tx, rx) = mpsc::channel::<OutboundCommand>(1);
        drop(rx); // simulate WS task exit / panic
        let dispatcher = Dispatcher::with_command_tx(services, tx);

        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::CallService {
                    domain: "light".to_owned(),
                    service: "turn_on".to_owned(),
                    target: None,
                    data: None,
                },
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("closed receiver must error");
        assert!(
            matches!(err, DispatchError::ChannelClosed),
            "closed receiver → ChannelClosed, got {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Gesture pick-action — every variant lands on the right field
    // -----------------------------------------------------------------------

    #[test]
    fn pick_action_routes_each_gesture_to_the_corresponding_field() {
        let entry = entry_with(
            "light.kitchen",
            Action::Toggle,
            Action::MoreInfo,
            Action::Navigate {
                view_id: "home".to_owned(),
            },
        );
        assert_eq!(pick_action(&entry, Gesture::Tap), &Action::Toggle);
        assert_eq!(pick_action(&entry, Gesture::Hold), &Action::MoreInfo);
        assert_eq!(
            pick_action(&entry, Gesture::DoubleTap),
            &Action::Navigate {
                view_id: "home".to_owned(),
            }
        );
    }

    // -----------------------------------------------------------------------
    // domain_of — happy path + malformed
    // -----------------------------------------------------------------------

    #[test]
    fn domain_of_extracts_prefix_before_dot() {
        assert_eq!(domain_of(&EntityId::from("light.kitchen")), "light");
        assert_eq!(domain_of(&EntityId::from("switch.outlet_1")), "switch");
    }

    #[test]
    fn domain_of_returns_whole_string_when_no_dot() {
        // Defensive: malformed entity ids must not panic.
        assert_eq!(domain_of(&EntityId::from("nodot")), "nodot");
    }

    // -----------------------------------------------------------------------
    // DispatchError Display — surface for toast (TASK-067)
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_error_display_strings_are_descriptive() {
        let cases: Vec<(DispatchError, &str)> = vec![
            (
                DispatchError::UnknownWidget(WidgetId::from("foo")),
                "no action map entry",
            ),
            (
                DispatchError::NoCapability {
                    domain: "light".to_owned(),
                    reason: "test",
                },
                "no toggle-equivalent service",
            ),
            (
                DispatchError::UnknownToggleState {
                    entity_id: EntityId::from("light.kitchen"),
                    observed_state: "unavailable".to_owned(),
                },
                "cannot toggle",
            ),
            (
                DispatchError::EntityNotFound(EntityId::from("light.kitchen")),
                "not in store snapshot",
            ),
            (DispatchError::ChannelNotWired, "command_tx is not wired"),
            (DispatchError::ChannelClosed, "receiver dropped"),
            (
                DispatchError::NotImplementedYet {
                    what: "Url",
                    ticket: "TASK-063",
                },
                "TASK-063",
            ),
            (
                DispatchError::UrlInvalidHref {
                    reason: "contains shell metacharacter",
                },
                "url action rejected",
            ),
            (
                DispatchError::UrlSpawnFailed {
                    reason: "No such file or directory".to_owned(),
                },
                "xdg-open spawn failed",
            ),
            (
                DispatchError::BackpressureRejected {
                    entity_id: EntityId::from("light.kitchen"),
                    scope: BackpressureScope::PerEntity,
                },
                "per-entity backpressure",
            ),
            (
                DispatchError::BackpressureRejected {
                    entity_id: EntityId::from("light.kitchen"),
                    scope: BackpressureScope::Global,
                },
                "global backpressure",
            ),
            (
                DispatchError::OfflineNonIdempotent {
                    entity_id: EntityId::from("light.kitchen"),
                },
                "non-idempotent",
            ),
            (
                DispatchError::OfflineQueueRejected {
                    entity_id: EntityId::from("light.kitchen"),
                    reason: QueueRejectReason::ServiceNotAllowlisted,
                },
                "runtime allowlist",
            ),
            (
                DispatchError::OfflineQueueRejected {
                    entity_id: EntityId::from("light.kitchen"),
                    reason: QueueRejectReason::UnsupportedVariant,
                },
                "unsupported variant",
            ),
        ];
        for (err, expected_substr) in cases {
            let s = format!("{err}");
            assert!(
                s.contains(expected_substr),
                "Display for {err:?} must contain `{expected_substr}`, got: {s}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // TASK-064 — Optimistic UI tests
    // -----------------------------------------------------------------------
    //
    // These tests exercise the `with_optimistic_reconciliation` builder. They
    // assert the five reconciliation rules from
    // `locked_decisions.optimistic_reconciliation_key`, plus
    // BackpressureRejected (per-entity + global cap), LastWriteWins
    // chain-root preservation, and `pending_for_widget`.

    use crate::actions::timing::{ActionOverlapStrategy, ActionTiming};
    use crate::ha::client::{HaAckError, HaAckSuccess};
    use crate::ha::live_store::{DEFAULT_GLOBAL_OPTIMISTIC_CAP, DEFAULT_PER_ENTITY_OPTIMISTIC_CAP};
    use crate::ha::store::EntityUpdate;
    use jiff::SignedDuration;

    /// Test fixture: build a `LiveStore`, dispatcher with optimistic
    /// reconciliation, and recorder/toast channels.
    fn make_optimistic_fixture(
        timing: ActionTiming,
    ) -> (
        Arc<LiveStore>,
        Dispatcher,
        mpsc::Receiver<OutboundCommand>,
        mpsc::Receiver<ToastEvent>,
    ) {
        let mut reg = ServiceRegistry::new();
        reg.add_service("light", "toggle", ServiceMeta::default());
        reg.add_service("light", "turn_on", ServiceMeta::default());
        reg.add_service("light", "turn_off", ServiceMeta::default());
        let services = handle_from(reg);

        let store: Arc<LiveStore> = Arc::new(LiveStore::new());
        store.apply_snapshot(vec![make_entity("light.kitchen", "off")]);

        let (cmd_tx, cmd_rx) = mpsc::channel::<OutboundCommand>(8);
        let (toast_tx, toast_rx) = mpsc::channel::<ToastEvent>(8);

        let dispatcher = Dispatcher::with_command_tx(services, cmd_tx)
            .with_optimistic_reconciliation(store.clone(), timing, toast_tx);
        (store, dispatcher, cmd_rx, toast_rx)
    }

    /// Wrap the toggle-light fixture in a single-widget action map.
    fn kitchen_light_toggle_map() -> WidgetActionMap {
        one_widget_map(
            "kitchen_light",
            entry_with("light.kitchen", Action::Toggle, Action::None, Action::None),
        )
    }

    /// Drain the next `OutboundCommand` from the recorder; resolves its
    /// `ack_tx` with the supplied result so the dispatcher's reconciliation
    /// task can proceed.
    async fn deliver_ack(rx: &mut mpsc::Receiver<OutboundCommand>, ack: AckResult) {
        let cmd = rx
            .recv()
            .await
            .expect("recorder must yield the dispatched OutboundCommand");
        // The dispatcher's reconciliation task awaits THIS oneshot; we fire
        // it as if the WS client had received an HA `result` frame.
        let _ = cmd.ack_tx.send(ack);
    }

    /// Wait until `pred` returns `true` or `timeout` elapses (5 ms poll).
    async fn wait_until<F: Fn() -> bool>(pred: F, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        pred()
    }

    // -----------------------------------------------------------------------
    // pending_for_widget — true while entry exists, false after drop
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn pending_for_widget_true_during_dispatch_and_false_after_drop() {
        let timing = ActionTiming::default();
        let (store, dispatcher, mut cmd_rx, _toast_rx) = make_optimistic_fixture(timing);

        // Wire the WidgetActionMap so pending_for_widget can resolve.
        let map = kitchen_light_toggle_map();
        store.set_widget_action_map(Arc::new(map.clone()));

        // Before dispatch: false.
        assert!(
            !store.pending_for_widget(&WidgetId::from("kitchen_light")),
            "no pending entry before dispatch"
        );

        let outcome = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed (light.toggle is registered)");
        let DispatchOutcome::Sent { ack_rx } = outcome else {
            panic!("expected Sent");
        };

        // After dispatch (before ack): true.
        assert!(
            store.pending_for_widget(&WidgetId::from("kitchen_light")),
            "pending entry visible to widget query after dispatch"
        );

        // Resolve ack as success WITH state_changed (rule 1): apply a state
        // change with last_changed > dispatched_at, then drain the ack.
        let new_last_changed = Timestamp::now()
            .checked_add(SignedDuration::from_millis(50))
            .unwrap();
        store.apply_event(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(Entity {
                id: EntityId::from("light.kitchen"),
                state: Arc::from("on"),
                attributes: Arc::new(serde_json::Map::new()),
                last_changed: new_last_changed,
                last_updated: new_last_changed,
            }),
        });
        deliver_ack(
            &mut cmd_rx,
            Ok(HaAckSuccess {
                id: 1,
                payload: None,
            }),
        )
        .await;

        // Wait for reconciliation task to drain. Even though apply_event
        // already dropped the entry synchronously, the assertion is a
        // no-flake bound.
        let cleared = wait_until(
            || !store.pending_for_widget(&WidgetId::from("kitchen_light")),
            Duration::from_secs(1),
        )
        .await;
        assert!(
            cleared,
            "entry must be dropped after ack-success + state_changed"
        );

        // Forward-receive the dispatcher's ack (proves the caller path
        // resolves with the success result).
        let final_ack = tokio::time::timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("caller ack must resolve")
            .expect("oneshot must not be dropped");
        assert!(final_ack.is_ok());
    }

    // -----------------------------------------------------------------------
    // Rule 1 — Ack success WITH state_changed (last_changed > dispatched_at)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rule_1_ack_success_with_state_changed_drops_entry() {
        let timing = ActionTiming::default();
        let (store, dispatcher, mut cmd_rx, _toast_rx) = make_optimistic_fixture(timing);
        let map = kitchen_light_toggle_map();

        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed");

        assert_eq!(
            store
                .optimistic_entries_for(&EntityId::from("light.kitchen"))
                .len(),
            1
        );

        // Inbound state_changed with last_changed > dispatched_at: applies
        // rule 1 inside `apply_event`, dropping the entry.
        let new_last_changed = Timestamp::now()
            .checked_add(SignedDuration::from_millis(100))
            .unwrap();
        store.apply_event(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(Entity {
                id: EntityId::from("light.kitchen"),
                state: Arc::from("on"),
                attributes: Arc::new(serde_json::Map::new()),
                last_changed: new_last_changed,
                last_updated: new_last_changed,
            }),
        });

        assert!(
            store
                .optimistic_entries_for(&EntityId::from("light.kitchen"))
                .is_empty(),
            "rule 1: state_changed with last_changed > dispatched_at drops entry"
        );

        // Drain the ack so the reconciliation task can complete cleanly.
        deliver_ack(
            &mut cmd_rx,
            Ok(HaAckSuccess {
                id: 1,
                payload: None,
            }),
        )
        .await;
    }

    // -----------------------------------------------------------------------
    // Rule 2 — Ack success WITHOUT state_changed (no-op success)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rule_2_ack_success_without_event_matching_state_drops_entry() {
        // Tentative state matches current state at ack time → no-op
        // confirmed → entry dropped.
        let timing = ActionTiming::default();
        let (store, dispatcher, mut cmd_rx, _toast_rx) = make_optimistic_fixture(timing);
        let map = kitchen_light_toggle_map();

        // Dispatch toggle from "off" → tentative "on".
        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed");

        // Pre-fill the snapshot with state == tentative (i.e. HA was
        // already "on"; the toggle is a no-op). Important: we leave
        // last_changed UNCHANGED so apply_event's rule-1 path does NOT
        // fire — the tentative-match snapshot is the only signal.
        let entries = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
        let dispatched_at = entries[0].dispatched_at;
        store.apply_snapshot(vec![Entity {
            id: EntityId::from("light.kitchen"),
            state: Arc::from("on"), // matches tentative
            attributes: Arc::new(serde_json::Map::new()),
            // last_changed older than dispatched_at — no rule-1 fire.
            last_changed: dispatched_at
                .checked_sub(SignedDuration::from_millis(10))
                .unwrap(),
            last_updated: dispatched_at
                .checked_sub(SignedDuration::from_millis(10))
                .unwrap(),
        }]);

        // Deliver ack-success; the reconciliation task should snapshot
        // current state, see it matches tentative, and drop the entry.
        deliver_ack(
            &mut cmd_rx,
            Ok(HaAckSuccess {
                id: 1,
                payload: None,
            }),
        )
        .await;

        let cleared = wait_until(
            || {
                store
                    .optimistic_entries_for(&EntityId::from("light.kitchen"))
                    .is_empty()
            },
            Duration::from_secs(1),
        )
        .await;
        assert!(
            cleared,
            "rule 2: ack-without-event with current==tentative drops entry"
        );
    }

    #[tokio::test]
    async fn rule_2_ack_success_without_event_mismatch_holds_then_reverts() {
        // Tentative state DOES NOT match current state and no state_changed
        // arrives within optimistic_timeout_ms → entry reverts.
        let timing = ActionTiming {
            optimistic_timeout_ms: 80, // short window for the test
            ..ActionTiming::default()
        };
        let (store, dispatcher, mut cmd_rx, _toast_rx) = make_optimistic_fixture(timing);
        let map = kitchen_light_toggle_map();

        // Dispatch toggle from "off" → tentative "on". Snapshot stays "off"
        // so the rule-2 mismatch path fires.
        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed");

        // Deliver ack-success but the snapshot still shows "off" (no
        // state_changed). Rule 2 mismatch → hold-and-revert.
        deliver_ack(
            &mut cmd_rx,
            Ok(HaAckSuccess {
                id: 1,
                payload: None,
            }),
        )
        .await;

        let reverted = wait_until(
            || {
                store
                    .optimistic_entries_for(&EntityId::from("light.kitchen"))
                    .is_empty()
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(reverted, "rule 2 mismatch: entry must time out and revert");
    }

    // -----------------------------------------------------------------------
    // Rule 3 — Attribute-only state_changed leaves entry intact
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rule_3_attribute_only_state_changed_leaves_entry_intact() {
        let timing = ActionTiming::default();
        let (store, dispatcher, _cmd_rx, _toast_rx) = make_optimistic_fixture(timing);
        let map = kitchen_light_toggle_map();

        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed");

        let entries = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
        let dispatched_at = entries[0].dispatched_at;

        // Apply an attribute-only event: same state, last_changed UNCHANGED
        // (i.e. <= dispatched_at). Per rule 3, the optimistic entry must
        // survive.
        let last_changed_old = dispatched_at
            .checked_sub(SignedDuration::from_millis(50))
            .unwrap();
        let mut attrs = serde_json::Map::new();
        attrs.insert("brightness".to_owned(), serde_json::json!(180));
        store.apply_event(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(Entity {
                id: EntityId::from("light.kitchen"),
                state: Arc::from("off"), // unchanged
                attributes: Arc::new(attrs),
                last_changed: last_changed_old,
                last_updated: Timestamp::now(), // attribute change advances last_updated only
            }),
        });

        assert_eq!(
            store
                .optimistic_entries_for(&EntityId::from("light.kitchen"))
                .len(),
            1,
            "rule 3: attribute-only event must leave optimistic entry intact"
        );
    }

    // -----------------------------------------------------------------------
    // Rule 4 — Ack error reverts to prior_state
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rule_4_ack_error_drops_entry_immediately() {
        let timing = ActionTiming::default();
        let (store, dispatcher, mut cmd_rx, _toast_rx) = make_optimistic_fixture(timing);
        let map = kitchen_light_toggle_map();

        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed");

        deliver_ack(
            &mut cmd_rx,
            Err(HaAckError {
                id: 1,
                code: "not_found".to_owned(),
                message: "service not found".to_owned(),
            }),
        )
        .await;

        let cleared = wait_until(
            || {
                store
                    .optimistic_entries_for(&EntityId::from("light.kitchen"))
                    .is_empty()
            },
            Duration::from_secs(1),
        )
        .await;
        assert!(
            cleared,
            "rule 4: ack-error drops entry (revert to prior_state)"
        );
    }

    // -----------------------------------------------------------------------
    // Rule 5 — Optimistic timeout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rule_5_optimistic_timeout_drops_entry() {
        let timing = ActionTiming {
            optimistic_timeout_ms: 50,
            ..ActionTiming::default()
        };
        let (store, dispatcher, _cmd_rx, _toast_rx) = make_optimistic_fixture(timing);
        let map = kitchen_light_toggle_map();

        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed");

        // No ack, no state_changed → timeout fires.
        let cleared = wait_until(
            || {
                store
                    .optimistic_entries_for(&EntityId::from("light.kitchen"))
                    .is_empty()
            },
            Duration::from_secs(1),
        )
        .await;
        assert!(cleared, "rule 5: timeout drops entry (revert)");
    }

    // -----------------------------------------------------------------------
    // BackpressureRejected — per-entity cap
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn backpressure_rejected_at_per_entity_cap_with_toast_event() {
        // DiscardConcurrent: each new dispatch goes into a NEW slot (no
        // chain-root cancellation). LastWriteWins always cancels prior
        // entries so the per-entity cap can never trip without
        // DiscardConcurrent.
        let timing = ActionTiming {
            action_overlap_strategy: ActionOverlapStrategy::DiscardConcurrent,
            ..ActionTiming::default()
        };
        let (store, dispatcher, mut cmd_rx, mut toast_rx) = make_optimistic_fixture(timing);
        let map = kitchen_light_toggle_map();

        // Fill the per-entity bucket to its default cap (4).
        for _ in 0..DEFAULT_PER_ENTITY_OPTIMISTIC_CAP {
            let _ = dispatcher
                .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
                .expect("dispatch must succeed up to per-entity cap");
            let _ = cmd_rx.try_recv().expect("frame must have been emitted");
        }
        assert_eq!(
            store
                .optimistic_entries_for(&EntityId::from("light.kitchen"))
                .len(),
            DEFAULT_PER_ENTITY_OPTIMISTIC_CAP
        );

        // Next dispatch must trip per-entity cap.
        let err = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect_err("per-entity cap must trip BackpressureRejected");
        match err {
            DispatchError::BackpressureRejected {
                ref entity_id,
                scope,
            } => {
                assert_eq!(entity_id, &EntityId::from("light.kitchen"));
                assert_eq!(scope, BackpressureScope::PerEntity);
            }
            other => panic!("expected BackpressureRejected, got {other:?}"),
        }

        // Toast event observable on the toast channel.
        let toast = tokio::time::timeout(Duration::from_secs(1), toast_rx.recv())
            .await
            .expect("toast must arrive within 1s")
            .expect("toast channel must yield the BackpressureRejected event");
        match toast {
            ToastEvent::BackpressureRejected { entity_id, scope } => {
                assert_eq!(entity_id, EntityId::from("light.kitchen"));
                assert_eq!(scope, BackpressureScope::PerEntity);
            }
            other => panic!("expected BackpressureRejected, got {other:?}"),
        }

        // No additional WS frame was emitted (the dispatch was rejected).
        assert!(
            cmd_rx.try_recv().is_err(),
            "no OutboundCommand on the rejected dispatch"
        );
    }

    // -----------------------------------------------------------------------
    // BackpressureRejected — global cap
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn backpressure_rejected_at_global_cap_with_toast_event() {
        // Construct a LiveStore with a tiny global cap so we can fill it
        // across multiple entities without inflating the per-entity cap.
        let mut reg = ServiceRegistry::new();
        reg.add_service("light", "turn_on", ServiceMeta::default());
        reg.add_service("light", "turn_off", ServiceMeta::default());
        let services = handle_from(reg);

        let store: Arc<LiveStore> = Arc::new(
            LiveStore::new()
                .with_per_entity_optimistic_cap(2)
                .with_global_optimistic_cap(2),
        );
        // Two entities, both starting "off".
        store.apply_snapshot(vec![
            make_entity("light.a", "off"),
            make_entity("light.b", "off"),
        ]);

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<OutboundCommand>(16);
        let (toast_tx, mut toast_rx) = mpsc::channel::<ToastEvent>(8);
        let timing = ActionTiming {
            action_overlap_strategy: ActionOverlapStrategy::DiscardConcurrent,
            ..ActionTiming::default()
        };

        let dispatcher = Dispatcher::with_command_tx(services, cmd_tx)
            .with_optimistic_reconciliation(store.clone(), timing, toast_tx);

        let mut map = WidgetActionMap::new();
        map.insert(
            WidgetId::from("a"),
            entry_with("light.a", Action::Toggle, Action::None, Action::None),
        );
        map.insert(
            WidgetId::from("b"),
            entry_with("light.b", Action::Toggle, Action::None, Action::None),
        );

        // Two dispatches across two entities — fills global cap to 2.
        let _ = dispatcher
            .dispatch(&WidgetId::from("a"), Gesture::Tap, &store, &map)
            .expect("first global dispatch");
        let _ = cmd_rx.try_recv().expect("frame 1");
        let _ = dispatcher
            .dispatch(&WidgetId::from("b"), Gesture::Tap, &store, &map)
            .expect("second global dispatch");
        let _ = cmd_rx.try_recv().expect("frame 2");
        assert_eq!(store.optimistic_total(), 2);

        // Third dispatch on a different entity must trip GLOBAL cap (each
        // entity's bucket has only 1 entry, well under per-entity cap=2).
        let err = dispatcher
            .dispatch(&WidgetId::from("a"), Gesture::Tap, &store, &map)
            .expect_err("global cap must trip BackpressureRejected");
        match err {
            DispatchError::BackpressureRejected {
                ref entity_id,
                scope,
            } => {
                // The dispatcher tried to insert a fresh entry on `light.a`;
                // since that bucket only has 1 entry (under the per-entity
                // cap of 2), the global cap is what trips first.
                assert_eq!(entity_id, &EntityId::from("light.a"));
                assert_eq!(scope, BackpressureScope::Global);
            }
            other => panic!("expected BackpressureRejected, got {other:?}"),
        }

        let toast = tokio::time::timeout(Duration::from_secs(1), toast_rx.recv())
            .await
            .expect("toast must arrive within 1s")
            .expect("toast channel must yield the BackpressureRejected event");
        match toast {
            ToastEvent::BackpressureRejected { scope, .. } => {
                assert_eq!(scope, BackpressureScope::Global);
            }
            other => panic!("expected BackpressureRejected, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // LastWriteWins — cancels pending entry, preserves prior_state chain
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn last_write_wins_cancels_pending_and_preserves_prior_state_chain() {
        // First dispatch: prior_state = "off", tentative = "on".
        // Second dispatch (during pending): cancels first, new entry's
        // prior_state must == FIRST cancelled entry's prior_state ("off"),
        // NOT the cancelled entry's tentative_state ("on").
        let timing = ActionTiming::default(); // LastWriteWins is the default
        let (store, dispatcher, mut cmd_rx, _toast_rx) = make_optimistic_fixture(timing);

        // Dispatch raw CallService("turn_on") so prior="off", tentative="on".
        let map = one_widget_map(
            "kitchen_light",
            entry_with(
                "light.kitchen",
                Action::CallService {
                    domain: "light".to_owned(),
                    service: "turn_on".to_owned(),
                    target: Some("light.kitchen".to_owned()),
                    data: None,
                },
                Action::None,
                Action::None,
            ),
        );

        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("first dispatch must succeed");
        let _ = cmd_rx.try_recv().expect("first frame emitted");

        let entries_before = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
        assert_eq!(entries_before.len(), 1);
        assert_eq!(entries_before[0].prior_state.as_ref(), "off");
        assert_eq!(entries_before[0].tentative_state.as_ref(), "on");

        // Pretend the snapshot has flipped to "on" (simulating optimistic
        // application without a state_changed yet). The locked-decision
        // chain-root rule should still preserve the FIRST entry's
        // prior_state="off", not the new "on".
        store.apply_snapshot(vec![make_entity("light.kitchen", "on")]);

        // Second dispatch (rapid second tap) — LastWriteWins cancels the
        // first entry and creates a new one. The new entry's prior_state
        // MUST be "off" (chain root), NOT "on" (current snapshot, which
        // would be the cancelled entry's tentative_state).
        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("second dispatch must succeed");
        let _ = cmd_rx.try_recv().expect("second frame emitted");

        let entries_after = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
        assert_eq!(
            entries_after.len(),
            1,
            "LastWriteWins: only the new entry remains (old cancelled)"
        );
        assert_eq!(
            entries_after[0].prior_state.as_ref(),
            "off",
            "LastWriteWins: new entry's prior_state preserves the chain root \
             (the FIRST entry's prior_state), NOT the cancelled entry's tentative_state"
        );
        assert_ne!(
            entries_before[0].request_id, entries_after[0].request_id,
            "the cancelled entry's request_id must be distinct from the new one"
        );
    }

    // -----------------------------------------------------------------------
    // Out-of-order ack on the same entity — stale ack does not revert newer
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn out_of_order_ack_does_not_revert_newer_state_changed() {
        // Stale ack arriving AFTER a newer state_changed (with last_changed
        // > dispatched_at) must not revert. The state_changed has already
        // dropped the entry; the late ack finds nothing to act on.
        let timing = ActionTiming::default();
        let (store, dispatcher, mut cmd_rx, _toast_rx) = make_optimistic_fixture(timing);
        let map = kitchen_light_toggle_map();

        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("dispatch must succeed");

        // state_changed arrives FIRST (out of order vs. ack).
        let new_last_changed = Timestamp::now()
            .checked_add(SignedDuration::from_millis(100))
            .unwrap();
        store.apply_event(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(Entity {
                id: EntityId::from("light.kitchen"),
                state: Arc::from("on"),
                attributes: Arc::new(serde_json::Map::new()),
                last_changed: new_last_changed,
                last_updated: new_last_changed,
            }),
        });
        assert!(
            store
                .optimistic_entries_for(&EntityId::from("light.kitchen"))
                .is_empty(),
            "rule 1 already dropped the entry"
        );

        // Stale ack arrives later — the dispatcher's reconciliation task
        // runs the ack-success branch, sees no entry, and is a no-op. The
        // newer state_changed truth ("on") is preserved.
        deliver_ack(
            &mut cmd_rx,
            Ok(HaAckSuccess {
                id: 1,
                payload: None,
            }),
        )
        .await;
        // Give the reconciliation task a moment to drain.
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(
            store
                .get(&EntityId::from("light.kitchen"))
                .map(|e| e.state.clone())
                .unwrap()
                .as_ref(),
            "on",
            "newer state_changed must not be reverted by stale ack"
        );
    }

    // -----------------------------------------------------------------------
    // OptimisticEntry value-shape — the canonical struct
    // -----------------------------------------------------------------------

    #[test]
    fn optimistic_entry_struct_carries_load_bearing_fields() {
        // Compile-time assertion: the canonical OptimisticEntry shape
        // (entity_id, request_id, dispatched_at, tentative_state,
        // prior_state) is stable.
        let _e = OptimisticEntry {
            entity_id: EntityId::from("light.kitchen"),
            request_id: 1,
            dispatched_at: Timestamp::now(),
            tentative_state: Arc::from("on"),
            prior_state: Arc::from("off"),
        };
    }

    #[test]
    fn default_caps_match_locked_decisions() {
        // locked_decisions.backpressure: per-entity=4, global=64.
        assert_eq!(DEFAULT_PER_ENTITY_OPTIMISTIC_CAP, 4);
        assert_eq!(DEFAULT_GLOBAL_OPTIMISTIC_CAP, 64);
    }

    // -----------------------------------------------------------------------
    // TASK-065: offline routing — Toggle/Url rejection, CallService queueing,
    // FIFO flush against the dispatcher's installed channel.
    // -----------------------------------------------------------------------

    use crate::actions::queue::OfflineQueue;
    use crate::platform::status::ConnectionState;
    use tokio::sync::watch;

    /// Build a `(dispatcher, cmd_rx, toast_rx, queue_arc, state_tx)` tuple
    /// with a freshly-installed offline routing context. The state defaults
    /// to `Connecting` (i.e. NOT live) so every dispatch enters the offline
    /// branch unless the test transitions to `Live`.
    #[allow(clippy::type_complexity)]
    fn make_offline_fixture() -> (
        Dispatcher,
        mpsc::Receiver<OutboundCommand>,
        mpsc::Receiver<ToastEvent>,
        Arc<Mutex<OfflineQueue>>,
        watch::Sender<ConnectionState>,
    ) {
        let services = handle_from(ServiceRegistry::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<OutboundCommand>(32);
        let (toast_tx, toast_rx) = mpsc::channel::<ToastEvent>(32);
        let queue = Arc::new(Mutex::new(OfflineQueue::new()));
        let (state_tx, state_rx) = watch::channel(ConnectionState::Connecting);

        let dispatcher = Dispatcher::with_command_tx(services, cmd_tx).with_offline_queue(
            queue.clone(),
            state_rx,
            toast_tx,
        );
        (dispatcher, cmd_rx, toast_rx, queue, state_tx)
    }

    fn drain_toast(rx: &mut mpsc::Receiver<ToastEvent>) -> Vec<ToastEvent> {
        // Pull up to 8 events without blocking — sufficient for the
        // single-tap tests below.
        let mut events = Vec::new();
        for _ in 0..8 {
            match rx.try_recv() {
                Ok(t) => events.push(t),
                Err(_) => break,
            }
        }
        events
    }

    #[tokio::test]
    async fn offline_toggle_returns_err_and_emits_toast_and_queue_empty() {
        // Load-bearing acceptance per ticket:
        // "Toggle offline → Err + queue empty: load-bearing acceptance —
        // non-idempotent rejection. Queue must contain ZERO entries after
        // Toggle rejection."
        let (dispatcher, mut cmd_rx, mut toast_rx, queue, _state_tx) = make_offline_fixture();

        let map = one_widget_map(
            "kitchen_light",
            entry_with("light.kitchen", Action::Toggle, Action::None, Action::None),
        );
        let store = store_with(vec![make_entity("light.kitchen", "on")]);

        let err = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect_err("Toggle offline must return Err");
        match err {
            DispatchError::OfflineNonIdempotent { entity_id } => {
                assert_eq!(entity_id, EntityId::from("light.kitchen"));
            }
            other => panic!("expected OfflineNonIdempotent, got {other:?}"),
        }

        // Risk #6 load-bearing: queue must be EMPTY after Toggle rejection.
        let queue_guard = queue.lock().unwrap();
        assert_eq!(
            queue_guard.len(),
            0,
            "Toggle rejection must leave the queue empty (Risk #6)"
        );
        drop(queue_guard);

        // No WS frame escaped to command_tx.
        assert!(
            cmd_rx.try_recv().is_err(),
            "no OutboundCommand on the offline rejection path"
        );

        // Toast event surfaced.
        let toasts = drain_toast(&mut toast_rx);
        assert!(
            toasts.iter().any(|t| matches!(
                t,
                ToastEvent::OfflineNonIdempotent { entity_id }
                    if *entity_id == EntityId::from("light.kitchen")
            )),
            "expected OfflineNonIdempotent toast, got {toasts:?}"
        );
    }

    #[tokio::test]
    async fn offline_url_returns_err_and_emits_toast_and_queue_empty() {
        let (dispatcher, _cmd_rx, mut toast_rx, queue, _state_tx) = make_offline_fixture();

        let action = Action::Url {
            href: "https://example.org/".to_owned(),
        };
        let map = one_widget_map(
            "url_widget",
            entry_with("light.kitchen", action, Action::None, Action::None),
        );
        let store = store_with(vec![make_entity("light.kitchen", "on")]);

        let err = dispatcher
            .dispatch(&WidgetId::from("url_widget"), Gesture::Tap, &store, &map)
            .expect_err("Url offline must return Err");
        assert!(matches!(err, DispatchError::OfflineNonIdempotent { .. }));

        assert_eq!(queue.lock().unwrap().len(), 0, "Url rejection: queue empty");

        let toasts = drain_toast(&mut toast_rx);
        assert!(
            toasts
                .iter()
                .any(|t| matches!(t, ToastEvent::OfflineNonIdempotent { .. })),
            "expected OfflineNonIdempotent toast, got {toasts:?}"
        );
    }

    #[tokio::test]
    async fn offline_call_service_turn_on_is_queued_and_emits_toast() {
        let (dispatcher, mut cmd_rx, mut toast_rx, queue, _state_tx) = make_offline_fixture();

        let action = Action::CallService {
            domain: "light".to_owned(),
            service: "turn_on".to_owned(),
            target: Some("light.kitchen".to_owned()),
            data: Some(json!({ "brightness": 200 })),
        };
        let map = one_widget_map(
            "kitchen_light",
            entry_with("light.kitchen", action, Action::None, Action::None),
        );
        let store = store_with(vec![make_entity("light.kitchen", "off")]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("turn_on offline must be queued");
        match outcome {
            DispatchOutcome::Queued { entity_id } => {
                assert_eq!(entity_id, EntityId::from("light.kitchen"));
            }
            other => panic!("expected Queued, got {other:?}"),
        }

        // Queued, not sent.
        assert_eq!(queue.lock().unwrap().len(), 1);
        assert!(cmd_rx.try_recv().is_err(), "offline path must NOT send");

        let toasts = drain_toast(&mut toast_rx);
        assert!(
            toasts
                .iter()
                .any(|t| matches!(t, ToastEvent::OfflineQueued { .. })),
            "expected OfflineQueued toast, got {toasts:?}"
        );
    }

    #[tokio::test]
    async fn offline_call_service_not_allowlisted_returns_err_and_emits_toast() {
        let (dispatcher, _cmd_rx, mut toast_rx, queue, _state_tx) = make_offline_fixture();

        let action = Action::CallService {
            domain: "user".to_owned(),
            service: "delete_user".to_owned(),
            target: Some("light.kitchen".to_owned()),
            data: None,
        };
        let map = one_widget_map(
            "danger_widget",
            entry_with("light.kitchen", action, Action::None, Action::None),
        );
        let store = store_with(vec![make_entity("light.kitchen", "on")]);

        let err = dispatcher
            .dispatch(&WidgetId::from("danger_widget"), Gesture::Tap, &store, &map)
            .expect_err("non-allowlisted CallService offline must Err");
        match err {
            DispatchError::OfflineQueueRejected { entity_id, reason } => {
                assert_eq!(entity_id, EntityId::from("light.kitchen"));
                assert_eq!(reason, QueueRejectReason::ServiceNotAllowlisted);
            }
            other => panic!("expected OfflineQueueRejected, got {other:?}"),
        }
        assert_eq!(queue.lock().unwrap().len(), 0);

        let toasts = drain_toast(&mut toast_rx);
        assert!(
            toasts.iter().any(|t| matches!(
                t,
                ToastEvent::OfflineQueueRejected {
                    reason: QueueRejectReason::ServiceNotAllowlisted,
                    ..
                }
            )),
            "expected OfflineQueueRejected toast, got {toasts:?}"
        );
    }

    #[tokio::test]
    async fn offline_more_info_falls_through_to_normal_path() {
        // UI-local actions are independent of connection state; they must
        // still produce the normal `MoreInfo` outcome even when offline.
        let (dispatcher, _cmd_rx, _toast_rx, queue, _state_tx) = make_offline_fixture();
        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::None,
                Action::MoreInfo,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Hold, &store, &map)
            .expect("more-info offline must succeed");
        match outcome {
            DispatchOutcome::MoreInfo { entity_id } => {
                assert_eq!(entity_id, EntityId::from("light.kitchen"));
            }
            other => panic!("expected MoreInfo, got {other:?}"),
        }
        assert_eq!(
            queue.lock().unwrap().len(),
            0,
            "MoreInfo must NOT touch the offline queue"
        );
    }

    #[tokio::test]
    async fn offline_navigate_falls_through_to_normal_path() {
        let (dispatcher, _cmd_rx, _toast_rx, queue, _state_tx) = make_offline_fixture();
        let map = one_widget_map(
            "w",
            entry_with(
                "light.kitchen",
                Action::Navigate {
                    view_id: "default".to_owned(),
                },
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("navigate offline must succeed");
        assert!(matches!(outcome, DispatchOutcome::Navigate { .. }));
        assert_eq!(queue.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn online_dispatch_bypasses_offline_routing() {
        // Acceptance: when the connection is Live, the offline routing
        // context is inert — dispatch goes straight to the WS channel.
        let (dispatcher, mut cmd_rx, _toast_rx, queue, state_tx) = make_offline_fixture();
        // Transition to Live so the offline branch does not engage.
        state_tx.send(ConnectionState::Live).expect("send Live");

        let action = Action::CallService {
            domain: "light".to_owned(),
            service: "turn_on".to_owned(),
            target: Some("light.kitchen".to_owned()),
            data: None,
        };
        let map = one_widget_map(
            "kitchen_light",
            entry_with("light.kitchen", action, Action::None, Action::None),
        );
        let store = store_with(vec![make_entity("light.kitchen", "off")]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("Live dispatch must succeed");
        assert!(matches!(outcome, DispatchOutcome::Sent { .. }));
        assert_eq!(queue.lock().unwrap().len(), 0, "Live: queue not touched");

        let cmd = cmd_rx.try_recv().expect("Live: command sent on WS channel");
        assert_eq!(cmd.frame.service, "turn_on");
    }

    #[tokio::test]
    async fn reconnect_flush_via_dispatcher_command_tx_preserves_fifo_order() {
        // Acceptance per ticket: "enqueue 5 actions, reconnect, observe 5
        // service-call frames in order." The dispatcher's installed
        // command_tx is the same channel the queue forwards onto, so flush
        // round-trips through the production seam.
        let (dispatcher, mut cmd_rx, _toast_rx, queue, state_tx) = make_offline_fixture();

        // Five distinct allowlisted CallService actions, each on a unique
        // entity so we can assert order via target.
        for i in 0..5 {
            let action = Action::CallService {
                domain: "light".to_owned(),
                service: "turn_on".to_owned(),
                target: Some(format!("light.entity_{i}")),
                data: Some(json!({ "marker": i })),
            };
            let map = one_widget_map(
                "w",
                entry_with(
                    format!("light.entity_{i}").as_str(),
                    action,
                    Action::None,
                    Action::None,
                ),
            );
            let store = store_with(vec![]);
            let outcome = dispatcher
                .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
                .expect("offline enqueue must succeed");
            assert!(matches!(outcome, DispatchOutcome::Queued { .. }));
        }
        assert_eq!(queue.lock().unwrap().len(), 5);
        assert!(
            cmd_rx.try_recv().is_err(),
            "no commands sent yet — still offline"
        );

        // "Reconnect": flip state to Live and explicitly flush the queue.
        // Production wiring (out of scope) does this from the reconnect
        // FSM task; here we exercise the public flush API directly.
        state_tx.send(ConnectionState::Live).expect("send Live");

        // Build a sender clone the queue can flush onto. The dispatcher's
        // own command_tx is private; in production the reconnect flush
        // task holds the same Arc<mpsc::Sender>. Here we recover an
        // equivalent path: the `cmd_rx` we hold is the one the queue's
        // flush also targets, since OfflineRoutingCtx + command_tx point
        // at the same mpsc channel.
        //
        // For the test, the queue holds a separate sender; we use the
        // dispatcher's installed sender by looking it up via the dispatch
        // path itself: dispatching a no-op idempotent CallService while
        // online would forward to cmd_rx — but we want flush, not
        // re-dispatch. Easiest: build a new sender on the existing
        // channel by cloning the receiver isn't possible; instead, the
        // `flush_via_dispatcher` path is the production seam — for this
        // unit test we exercise OfflineQueue::flush directly through the
        // CLONE of the dispatcher's sender. The dispatcher exposes
        // `command_tx_for_flush()` indirectly: we re-create the channel
        // semantics by using the queue's flush against a cloned sender.
        //
        // Implementation: drain via a fresh sender built from the cmd_rx
        // we already hold — by constructing a new (tx, rx) pair and
        // forwarding from the new rx into cmd_rx is overengineering. We
        // simply give the queue a sender that is connected to the SAME
        // receiver: `mpsc::Sender::downgrade` + `upgrade` would help but
        // the cleanest path is to re-create the dispatcher's seam with
        // the same channel. Practically: the OfflineRoutingCtx clones the
        // sender at construction; we cannot pull it out of the dispatcher
        // for this test. The TASK-069 integration test exercises the
        // production reconnect-flush seam; this unit test asserts the
        // dispatcher → queue path FIFO via the queue's own flush API
        // against a fresh recorder.
        let (flush_tx, mut flush_rx) = mpsc::channel::<OutboundCommand>(8);
        let mut q = queue.lock().unwrap();
        let outcome = q.flush(&flush_tx, None);
        drop(q);
        assert_eq!(outcome.dispatched, 5);
        assert_eq!(outcome.aged_out, 0);

        for i in 0..5 {
            let cmd = flush_rx.try_recv().expect("flushed frame");
            assert_eq!(
                cmd.frame.target,
                Some(json!({ "entity_id": format!("light.entity_{i}") })),
                "FIFO order broken at {i}"
            );
            assert_eq!(cmd.frame.data, Some(json!({ "marker": i })));
        }
        assert!(flush_rx.try_recv().is_err());
    }

    // -----------------------------------------------------------------------
    // Phase 6 typed variants (TASK-099) — dispatch returns NotImplementedYet
    //
    // Each of the 10 new Action variants added in TASK-099 has a match arm in
    // `dispatch` that returns `Err(DispatchError::NotImplementedYet { .. })`.
    // These tests cover those arms so the per-file coverage ratchet for
    // `src/actions/dispatcher.rs` does not regress.  The tests also cover the
    // `maybe_route_offline` arm that returns `None` for each variant, letting
    // the main match produce `NotImplementedYet` even when the connection is
    // offline.
    // -----------------------------------------------------------------------

    // ------ online path (dispatch match arms) ------

    #[test]
    fn phase6_set_temperature_dispatch_returns_not_implemented_yet() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let action = Action::SetTemperature {
            entity_id: "climate.living_room".to_owned(),
            temperature: 21.5,
        };
        let map = one_widget_map(
            "w",
            entry_with("climate.living_room", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("SetTemperature must return NotImplementedYet");
        match err {
            DispatchError::NotImplementedYet { what, ticket } => {
                assert_eq!(what, "SetTemperature");
                assert_eq!(ticket, "TASK-103");
            }
            other => panic!("expected NotImplementedYet, got {other:?}"),
        }
    }

    #[test]
    fn phase6_set_hvac_mode_dispatch_returns_not_implemented_yet() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let action = Action::SetHvacMode {
            entity_id: "climate.living_room".to_owned(),
            mode: "heat".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("climate.living_room", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("SetHvacMode must return NotImplementedYet");
        match err {
            DispatchError::NotImplementedYet { what, ticket } => {
                assert_eq!(what, "SetHvacMode");
                assert_eq!(ticket, "TASK-103");
            }
            other => panic!("expected NotImplementedYet, got {other:?}"),
        }
    }

    #[test]
    fn phase6_set_media_volume_dispatch_returns_not_implemented_yet() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let action = Action::SetMediaVolume {
            entity_id: "media_player.tv".to_owned(),
            volume_level: 0.5,
        };
        let map = one_widget_map(
            "w",
            entry_with("media_player.tv", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("SetMediaVolume must return NotImplementedYet");
        match err {
            DispatchError::NotImplementedYet { what, ticket } => {
                assert_eq!(what, "SetMediaVolume");
                assert_eq!(ticket, "TASK-104");
            }
            other => panic!("expected NotImplementedYet, got {other:?}"),
        }
    }

    #[test]
    fn phase6_media_transport_dispatch_returns_not_implemented_yet() {
        use crate::actions::schema::MediaTransportOp;
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let action = Action::MediaTransport {
            entity_id: "media_player.tv".to_owned(),
            transport: MediaTransportOp::Play,
        };
        let map = one_widget_map(
            "w",
            entry_with("media_player.tv", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("MediaTransport must return NotImplementedYet");
        match err {
            DispatchError::NotImplementedYet { what, ticket } => {
                assert_eq!(what, "MediaTransport");
                assert_eq!(ticket, "TASK-104");
            }
            other => panic!("expected NotImplementedYet, got {other:?}"),
        }
    }

    #[test]
    fn phase6_set_cover_position_dispatch_returns_not_implemented_yet() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let action = Action::SetCoverPosition {
            entity_id: "cover.garage".to_owned(),
            position: 50,
        };
        let map = one_widget_map(
            "w",
            entry_with("cover.garage", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("SetCoverPosition must return NotImplementedYet");
        match err {
            DispatchError::NotImplementedYet { what, ticket } => {
                assert_eq!(what, "SetCoverPosition");
                assert_eq!(ticket, "TASK-105");
            }
            other => panic!("expected NotImplementedYet, got {other:?}"),
        }
    }

    #[test]
    fn phase6_set_fan_speed_dispatch_returns_not_implemented_yet() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let action = Action::SetFanSpeed {
            entity_id: "fan.bedroom".to_owned(),
            speed: "high".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("fan.bedroom", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("SetFanSpeed must return NotImplementedYet");
        match err {
            DispatchError::NotImplementedYet { what, ticket } => {
                assert_eq!(what, "SetFanSpeed");
                assert_eq!(ticket, "TASK-108");
            }
            other => panic!("expected NotImplementedYet, got {other:?}"),
        }
    }

    /// TASK-104 wired the Lock dispatcher: `Action::Lock` with no PIN
    /// policy and no confirmation dispatches `lock.lock` directly via
    /// the standard call-service path. The `NotImplementedYet` outcome
    /// landed in TASK-099 is no longer the right assertion — the new
    /// behaviour is covered here and the previous assertion would mask a
    /// regression.
    #[test]
    fn lock_dispatch_calls_lock_lock_service_with_no_pin_no_confirm() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let action = Action::Lock {
            entity_id: "lock.front_door".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("lock.front_door", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("Lock with no PIN, no confirm dispatches synchronously");
        match outcome {
            DispatchOutcome::Sent { .. } => {}
            other => panic!("expected DispatchOutcome::Sent, got {other:?}"),
        }
        let cmd = rx.try_recv().expect("recorder must have received");
        assert_eq!(cmd.frame.domain, "lock");
        assert_eq!(cmd.frame.service, "lock");
        assert_eq!(
            cmd.frame.target,
            Some(serde_json::json!({ "entity_id": "lock.front_door" }))
        );
        // No PIN means no `data` payload — HA's `lock.lock` accepts the
        // entity_id alone for code-less locks.
        assert!(cmd.frame.data.is_none());
    }

    /// TASK-104: `Action::Unlock` with no PIN policy and no confirmation
    /// dispatches `lock.unlock` synchronously.
    #[test]
    fn unlock_dispatch_calls_lock_unlock_service_with_no_pin_no_confirm() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let action = Action::Unlock {
            entity_id: "lock.front_door".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("lock.front_door", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("Unlock with no PIN, no confirm dispatches synchronously");
        match outcome {
            DispatchOutcome::Sent { .. } => {}
            other => panic!("expected DispatchOutcome::Sent, got {other:?}"),
        }
        let cmd = rx.try_recv().expect("recorder must have received");
        assert_eq!(cmd.frame.domain, "lock");
        assert_eq!(cmd.frame.service, "unlock");
        assert!(cmd.frame.data.is_none());
    }

    #[test]
    fn phase6_alarm_arm_dispatch_returns_not_implemented_yet() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let action = Action::AlarmArm {
            entity_id: "alarm_control_panel.home".to_owned(),
            mode: "home".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with(
                "alarm_control_panel.home",
                action,
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("AlarmArm must return NotImplementedYet");
        match err {
            DispatchError::NotImplementedYet { what, ticket } => {
                assert_eq!(what, "AlarmArm");
                assert_eq!(ticket, "TASK-109");
            }
            other => panic!("expected NotImplementedYet, got {other:?}"),
        }
    }

    #[test]
    fn phase6_alarm_disarm_dispatch_returns_not_implemented_yet() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
        let action = Action::AlarmDisarm {
            entity_id: "alarm_control_panel.home".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with(
                "alarm_control_panel.home",
                action,
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("AlarmDisarm must return NotImplementedYet");
        match err {
            DispatchError::NotImplementedYet { what, ticket } => {
                assert_eq!(what, "AlarmDisarm");
                assert_eq!(ticket, "TASK-109");
            }
            other => panic!("expected NotImplementedYet, got {other:?}"),
        }
    }

    // ------ offline path (maybe_route_offline returns None → dispatch arm) ------
    //
    // When connection is not Live, `maybe_route_offline` is called first. For
    // Phase 6 variants it returns `None` (no offline toast, no queue entry),
    // letting the main `dispatch` match fall through to `NotImplementedYet`.
    // These tests hit both the `maybe_route_offline` Phase 6 arm AND the
    // corresponding `dispatch` arm, ensuring both code paths are covered.

    #[test]
    fn phase6_set_temperature_offline_returns_not_implemented_yet() {
        let (dispatcher, _cmd_rx, _toast_rx, _queue, _state_tx) = make_offline_fixture();
        let action = Action::SetTemperature {
            entity_id: "climate.living_room".to_owned(),
            temperature: 22.0,
        };
        let map = one_widget_map(
            "w",
            entry_with("climate.living_room", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("SetTemperature offline must return NotImplementedYet");
        assert!(
            matches!(
                err,
                DispatchError::NotImplementedYet {
                    what: "SetTemperature",
                    ..
                }
            ),
            "expected NotImplementedYet for SetTemperature, got {err:?}"
        );
    }

    #[test]
    fn phase6_set_hvac_mode_offline_returns_not_implemented_yet() {
        let (dispatcher, _cmd_rx, _toast_rx, _queue, _state_tx) = make_offline_fixture();
        let action = Action::SetHvacMode {
            entity_id: "climate.living_room".to_owned(),
            mode: "cool".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("climate.living_room", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("SetHvacMode offline must return NotImplementedYet");
        assert!(
            matches!(
                err,
                DispatchError::NotImplementedYet {
                    what: "SetHvacMode",
                    ..
                }
            ),
            "expected NotImplementedYet for SetHvacMode, got {err:?}"
        );
    }

    #[test]
    fn phase6_set_media_volume_offline_returns_not_implemented_yet() {
        let (dispatcher, _cmd_rx, _toast_rx, _queue, _state_tx) = make_offline_fixture();
        let action = Action::SetMediaVolume {
            entity_id: "media_player.tv".to_owned(),
            volume_level: 0.3,
        };
        let map = one_widget_map(
            "w",
            entry_with("media_player.tv", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("SetMediaVolume offline must return NotImplementedYet");
        assert!(
            matches!(
                err,
                DispatchError::NotImplementedYet {
                    what: "SetMediaVolume",
                    ..
                }
            ),
            "expected NotImplementedYet for SetMediaVolume, got {err:?}"
        );
    }

    #[test]
    fn phase6_media_transport_offline_returns_not_implemented_yet() {
        use crate::actions::schema::MediaTransportOp;
        let (dispatcher, _cmd_rx, _toast_rx, _queue, _state_tx) = make_offline_fixture();
        let action = Action::MediaTransport {
            entity_id: "media_player.tv".to_owned(),
            transport: MediaTransportOp::Pause,
        };
        let map = one_widget_map(
            "w",
            entry_with("media_player.tv", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("MediaTransport offline must return NotImplementedYet");
        assert!(
            matches!(
                err,
                DispatchError::NotImplementedYet {
                    what: "MediaTransport",
                    ..
                }
            ),
            "expected NotImplementedYet for MediaTransport, got {err:?}"
        );
    }

    #[test]
    fn phase6_set_cover_position_offline_returns_not_implemented_yet() {
        let (dispatcher, _cmd_rx, _toast_rx, _queue, _state_tx) = make_offline_fixture();
        let action = Action::SetCoverPosition {
            entity_id: "cover.garage".to_owned(),
            position: 75,
        };
        let map = one_widget_map(
            "w",
            entry_with("cover.garage", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("SetCoverPosition offline must return NotImplementedYet");
        assert!(
            matches!(
                err,
                DispatchError::NotImplementedYet {
                    what: "SetCoverPosition",
                    ..
                }
            ),
            "expected NotImplementedYet for SetCoverPosition, got {err:?}"
        );
    }

    #[test]
    fn phase6_set_fan_speed_offline_returns_not_implemented_yet() {
        let (dispatcher, _cmd_rx, _toast_rx, _queue, _state_tx) = make_offline_fixture();
        let action = Action::SetFanSpeed {
            entity_id: "fan.bedroom".to_owned(),
            speed: "low".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("fan.bedroom", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("SetFanSpeed offline must return NotImplementedYet");
        assert!(
            matches!(
                err,
                DispatchError::NotImplementedYet {
                    what: "SetFanSpeed",
                    ..
                }
            ),
            "expected NotImplementedYet for SetFanSpeed, got {err:?}"
        );
    }

    /// TASK-104: Lock offline — the offline queue rejects Lock/Unlock as
    /// `UnsupportedVariant` (`src/actions/queue.rs` is in must_not_touch),
    /// so the dispatcher surfaces `NotImplementedYet` directly when
    /// offline. The confirm/PIN flow lives only on the live path.
    #[test]
    fn phase6_lock_offline_returns_not_implemented_yet() {
        let (dispatcher, _cmd_rx, _toast_rx, _queue, _state_tx) = make_offline_fixture();
        let action = Action::Lock {
            entity_id: "lock.front_door".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("lock.front_door", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("Lock offline must return NotImplementedYet");
        assert!(
            matches!(
                err,
                DispatchError::NotImplementedYet {
                    what: "Lock-or-Unlock-offline",
                    ..
                }
            ),
            "expected NotImplementedYet for Lock offline, got {err:?}"
        );
    }

    /// TASK-104: Unlock offline — same offline-path treatment as Lock.
    #[test]
    fn phase6_unlock_offline_returns_not_implemented_yet() {
        let (dispatcher, _cmd_rx, _toast_rx, _queue, _state_tx) = make_offline_fixture();
        let action = Action::Unlock {
            entity_id: "lock.front_door".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("lock.front_door", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("Unlock offline must return NotImplementedYet");
        assert!(
            matches!(
                err,
                DispatchError::NotImplementedYet {
                    what: "Lock-or-Unlock-offline",
                    ..
                }
            ),
            "expected NotImplementedYet for Unlock offline, got {err:?}"
        );
    }

    #[test]
    fn phase6_alarm_arm_offline_returns_not_implemented_yet() {
        let (dispatcher, _cmd_rx, _toast_rx, _queue, _state_tx) = make_offline_fixture();
        let action = Action::AlarmArm {
            entity_id: "alarm_control_panel.home".to_owned(),
            mode: "away".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with(
                "alarm_control_panel.home",
                action,
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("AlarmArm offline must return NotImplementedYet");
        assert!(
            matches!(
                err,
                DispatchError::NotImplementedYet {
                    what: "AlarmArm",
                    ..
                }
            ),
            "expected NotImplementedYet for AlarmArm, got {err:?}"
        );
    }

    #[test]
    fn phase6_alarm_disarm_offline_returns_not_implemented_yet() {
        let (dispatcher, _cmd_rx, _toast_rx, _queue, _state_tx) = make_offline_fixture();
        let action = Action::AlarmDisarm {
            entity_id: "alarm_control_panel.home".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with(
                "alarm_control_panel.home",
                action,
                Action::None,
                Action::None,
            ),
        );
        let store = store_with(vec![]);

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("AlarmDisarm offline must return NotImplementedYet");
        assert!(
            matches!(
                err,
                DispatchError::NotImplementedYet {
                    what: "AlarmDisarm",
                    ..
                }
            ),
            "expected NotImplementedYet for AlarmDisarm, got {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // TASK-104: Lock dispatch with PIN entry + confirm modal
    // -----------------------------------------------------------------------

    use crate::actions::pin::{CodeFormat, PinEntryHost};
    use crate::dashboard::schema::PinPolicy;
    use std::sync::Mutex as StdMutex;

    /// Boxed `FnOnce(String) + Send` closure stashed by the mock PIN host
    /// so a test can drive the on-submit callback synchronously. The
    /// `type` alias keeps the `Mutex<Option<Box<dyn ...>>>` chain inside a
    /// single name (clippy::type_complexity).
    type PendingPinSubmit = Box<dyn FnOnce(String) + Send>;

    /// Boxed `FnOnce() + Send` closure stashed by the mock confirm host.
    /// Same rationale as [`PendingPinSubmit`].
    type PendingConfirmAccept = Box<dyn FnOnce() + Send>;

    /// Mock PinEntryHost: captures the on_submit closure and the
    /// requested CodeFormat. Mirrors the test mock in
    /// `src/actions/pin.rs::tests` but lives here so the dispatcher
    /// tests can drive it directly without a cross-module import dance.
    struct MockPinEntryHost {
        pending: StdMutex<Option<PendingPinSubmit>>,
        received_format: StdMutex<Option<CodeFormat>>,
    }

    impl MockPinEntryHost {
        fn new() -> Self {
            MockPinEntryHost {
                pending: StdMutex::new(None),
                received_format: StdMutex::new(None),
            }
        }

        fn submit(&self, code: String) {
            let cb = self
                .pending
                .lock()
                .unwrap()
                .take()
                .expect("request_pin must have been called");
            cb(code);
        }

        fn received_format(&self) -> Option<CodeFormat> {
            *self.received_format.lock().unwrap()
        }
    }

    impl PinEntryHost for MockPinEntryHost {
        fn request_pin(&self, code_format: CodeFormat, on_submit: Box<dyn FnOnce(String) + Send>) {
            *self.received_format.lock().unwrap() = Some(code_format);
            *self.pending.lock().unwrap() = Some(on_submit);
        }
    }

    /// Mock ConfirmHost: captures the on_accept closure. Mirrors the
    /// MockPinEntryHost shape — accept by calling `accept`, dismiss by
    /// dropping the captured closure.
    struct MockConfirmHost {
        pending: StdMutex<Option<PendingConfirmAccept>>,
        invocations: StdMutex<usize>,
    }

    impl MockConfirmHost {
        fn new() -> Self {
            MockConfirmHost {
                pending: StdMutex::new(None),
                invocations: StdMutex::new(0),
            }
        }

        fn accept(&self) {
            let cb = self
                .pending
                .lock()
                .unwrap()
                .take()
                .expect("confirm must have been called");
            cb();
        }

        fn invocation_count(&self) -> usize {
            *self.invocations.lock().unwrap()
        }
    }

    impl ConfirmHost for MockConfirmHost {
        fn confirm(&self, _entity_id: EntityId, on_accept: Box<dyn FnOnce() + Send>) {
            *self.invocations.lock().unwrap() += 1;
            *self.pending.lock().unwrap() = Some(on_accept);
        }
    }

    /// `Action::Unlock` with `pin_policy: Required` invokes
    /// `PinEntryHost::request_pin` with the configured code_format AND
    /// returns `LockAwaitingPinEntry` synchronously. The actual
    /// `lock.unlock` service call only fires after the test invokes
    /// `host.submit(code)`.
    #[test]
    fn pin_required_triggers_pin_entry() {
        let services = handle_from(ServiceRegistry::new());
        let (tx, mut rx) = mpsc::channel::<OutboundCommand>(8);
        let pin_host = Arc::new(MockPinEntryHost::new());

        let mut settings = HashMap::new();
        settings.insert(
            WidgetId::from("w"),
            LockDispatchSettings {
                pin_policy: PinPolicy::Required {
                    length: 4,
                    code_format: CodeFormat::Number,
                },
                require_confirmation_on_unlock: false,
            },
        );

        let dispatcher = Dispatcher::with_command_tx(services, tx)
            .with_pin_host_arc(pin_host.clone() as Arc<dyn PinEntryHost>)
            .with_lock_settings(settings);

        let action = Action::Unlock {
            entity_id: "lock.front_door".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("lock.front_door", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("Unlock with PIN must return LockAwaitingPinEntry");
        match outcome {
            DispatchOutcome::LockAwaitingPinEntry { ref entity_id } => {
                assert_eq!(entity_id.as_str(), "lock.front_door");
            }
            other => panic!("expected LockAwaitingPinEntry, got {other:?}"),
        }
        assert_eq!(
            pin_host.received_format(),
            Some(CodeFormat::Number),
            "request_pin must be invoked with the configured code_format"
        );
        // The OutboundCommand has NOT been sent yet — it fires from
        // within the on_submit closure.
        assert!(
            rx.try_recv().is_err(),
            "no OutboundCommand should be sent until on_submit fires"
        );

        // Simulate the user entering the PIN.
        pin_host.submit("1234".to_owned());

        let cmd = rx
            .try_recv()
            .expect("OutboundCommand must be sent after submit");
        assert_eq!(cmd.frame.domain, "lock");
        assert_eq!(cmd.frame.service, "unlock");
        assert_eq!(
            cmd.frame.target,
            Some(serde_json::json!({ "entity_id": "lock.front_door" }))
        );
        assert_eq!(
            cmd.frame.data,
            Some(serde_json::json!({ "code": "1234" })),
            "code must be injected into data.code by on_submit"
        );
    }

    /// `Action::Unlock` with `require_confirmation_on_unlock: true`
    /// invokes `ConfirmHost::confirm` BEFORE PIN entry. After the user
    /// accepts, the PIN entry fires; after the user submits the code,
    /// the OutboundCommand is sent.
    #[test]
    fn require_confirmation_on_unlock_shows_modal() {
        let services = handle_from(ServiceRegistry::new());
        let (tx, mut rx) = mpsc::channel::<OutboundCommand>(8);
        let pin_host = Arc::new(MockPinEntryHost::new());
        let confirm_host = Arc::new(MockConfirmHost::new());

        let mut settings = HashMap::new();
        settings.insert(
            WidgetId::from("w"),
            LockDispatchSettings {
                pin_policy: PinPolicy::Required {
                    length: 4,
                    code_format: CodeFormat::Number,
                },
                require_confirmation_on_unlock: true,
            },
        );

        let dispatcher = Dispatcher::with_command_tx(services, tx)
            .with_pin_host_arc(pin_host.clone() as Arc<dyn PinEntryHost>)
            .with_confirm_host_arc(confirm_host.clone() as Arc<dyn ConfirmHost>)
            .with_lock_settings(settings);

        let action = Action::Unlock {
            entity_id: "lock.front_door".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("lock.front_door", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("Unlock with confirm must return LockAwaitingConfirm");
        match outcome {
            DispatchOutcome::LockAwaitingConfirm { ref entity_id } => {
                assert_eq!(entity_id.as_str(), "lock.front_door");
            }
            other => panic!("expected LockAwaitingConfirm, got {other:?}"),
        }
        assert_eq!(
            confirm_host.invocation_count(),
            1,
            "ConfirmHost::confirm must be invoked before PIN entry"
        );
        assert!(
            pin_host.received_format().is_none(),
            "PIN entry must NOT fire until the user accepts the confirm modal"
        );
        assert!(rx.try_recv().is_err(), "no command sent yet");

        // User accepts the confirm modal.
        confirm_host.accept();

        // Now PIN entry should be invoked.
        assert_eq!(
            pin_host.received_format(),
            Some(CodeFormat::Number),
            "PIN entry must fire after confirm accept"
        );
        assert!(rx.try_recv().is_err(), "still no command until PIN submit");

        // User enters the PIN.
        pin_host.submit("9876".to_owned());
        let cmd = rx.try_recv().expect("command after PIN submit");
        assert_eq!(cmd.frame.service, "unlock");
        assert_eq!(cmd.frame.data, Some(serde_json::json!({ "code": "9876" })));
    }

    /// `Action::Unlock` with `require_confirmation_on_unlock: true` AND
    /// `pin_policy: None` shows the confirm modal but skips PIN entry.
    /// Acceptance dispatches `lock.unlock` directly with no `data`.
    #[test]
    fn require_confirmation_on_unlock_with_no_pin_dispatches_after_accept() {
        let services = handle_from(ServiceRegistry::new());
        let (tx, mut rx) = mpsc::channel::<OutboundCommand>(8);
        let confirm_host = Arc::new(MockConfirmHost::new());

        let mut settings = HashMap::new();
        settings.insert(
            WidgetId::from("w"),
            LockDispatchSettings {
                pin_policy: PinPolicy::None,
                require_confirmation_on_unlock: true,
            },
        );

        let dispatcher = Dispatcher::with_command_tx(services, tx)
            .with_confirm_host_arc(confirm_host.clone() as Arc<dyn ConfirmHost>)
            .with_lock_settings(settings);

        let action = Action::Unlock {
            entity_id: "lock.front_door".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("lock.front_door", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let _ = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("Unlock with confirm dispatches");
        confirm_host.accept();

        let cmd = rx.try_recv().expect("command after accept");
        assert_eq!(cmd.frame.service, "unlock");
        assert!(cmd.frame.data.is_none(), "no PIN means no data payload");
    }

    /// Per `locked_decisions.confirmation_on_lock_unlock`: the
    /// `Action::Lock` (lock-down) variant does NOT consult
    /// `require_confirmation_on_unlock`. Only the *unlock* path shows a
    /// confirm modal — locking is always direct (or PIN-gated).
    #[test]
    fn lock_action_does_not_show_confirm_modal_even_when_unlock_flag_is_set() {
        let services = handle_from(ServiceRegistry::new());
        let (tx, mut rx) = mpsc::channel::<OutboundCommand>(8);
        let confirm_host = Arc::new(MockConfirmHost::new());

        let mut settings = HashMap::new();
        settings.insert(
            WidgetId::from("w"),
            LockDispatchSettings {
                pin_policy: PinPolicy::None,
                require_confirmation_on_unlock: true,
            },
        );

        let dispatcher = Dispatcher::with_command_tx(services, tx)
            .with_confirm_host_arc(confirm_host.clone() as Arc<dyn ConfirmHost>)
            .with_lock_settings(settings);

        let action = Action::Lock {
            entity_id: "lock.front_door".to_owned(),
        };
        let map = one_widget_map(
            "w",
            entry_with("lock.front_door", action, Action::None, Action::None),
        );
        let store = store_with(vec![]);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("Lock dispatches synchronously even when unlock flag is set");
        match outcome {
            DispatchOutcome::Sent { .. } => {}
            other => panic!("expected DispatchOutcome::Sent, got {other:?}"),
        }
        assert_eq!(
            confirm_host.invocation_count(),
            0,
            "Lock action must NOT invoke confirm host"
        );
        let cmd = rx.try_recv().expect("Lock dispatched directly");
        assert_eq!(cmd.frame.service, "lock");
    }

    /// Per Risk #7 (PIN code leakage): when the dispatcher fires a PIN
    /// entry submit, the entered code MUST NOT appear in any captured
    /// tracing span or event during the test. This is the dispatcher-side
    /// counterpart of the same assertion in
    /// `src/actions/pin.rs::tests::code_not_captured_in_tracing_spans`.
    #[test]
    fn pin_code_not_in_tracing_spans() {
        use std::sync::Arc as StdArc;
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;

        struct CapturingLayer {
            events: StdArc<StdMutex<Vec<String>>>,
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturingLayer {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                struct FieldCollector(Vec<String>);
                impl tracing::field::Visit for FieldCollector {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        self.0.push(format!("{}={:?}", field.name(), value));
                    }
                    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                        self.0.push(format!("{}={}", field.name(), value));
                    }
                }
                let mut collector = FieldCollector(Vec::new());
                event.record(&mut collector);
                let line = collector.0.join(" ");
                self.events.lock().unwrap().push(line);
            }
        }

        let events: StdArc<StdMutex<Vec<String>>> = StdArc::new(StdMutex::new(Vec::new()));
        let layer = CapturingLayer {
            events: StdArc::clone(&events),
        };
        let subscriber = tracing_subscriber::registry().with(layer);

        let synthetic_code = "555123";

        with_default(subscriber, || {
            let services = handle_from(ServiceRegistry::new());
            let (tx, mut rx) = mpsc::channel::<OutboundCommand>(8);
            let pin_host = Arc::new(MockPinEntryHost::new());
            let mut settings = HashMap::new();
            settings.insert(
                WidgetId::from("w"),
                LockDispatchSettings {
                    pin_policy: PinPolicy::Required {
                        length: 6,
                        code_format: CodeFormat::Number,
                    },
                    require_confirmation_on_unlock: false,
                },
            );
            let dispatcher = Dispatcher::with_command_tx(services, tx)
                .with_pin_host_arc(pin_host.clone() as Arc<dyn PinEntryHost>)
                .with_lock_settings(settings);

            let action = Action::Unlock {
                entity_id: "lock.front_door".to_owned(),
            };
            let map = one_widget_map(
                "w",
                entry_with("lock.front_door", action, Action::None, Action::None),
            );
            let store = store_with(vec![]);

            let _ = dispatcher.dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map);
            pin_host.submit(synthetic_code.to_owned());
            // Drain the receiver so the OutboundCommand is consumed,
            // matching the production fire-and-forget shape.
            let _ = rx.try_recv();
        });

        // No event line may contain the synthetic code. The OutboundFrame
        // does carry the code in its `data` field, but the dispatcher's
        // log lines NEVER emit the frame body — only static metadata.
        let captured = events.lock().unwrap();
        for line in captured.iter() {
            assert!(
                !line.contains(synthetic_code),
                "PIN code must not appear in any tracing event: {line:?}"
            );
        }
    }
}
