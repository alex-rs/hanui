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
//! # `Url` (TASK-063)
//!
//! Returns [`DispatchError::NotImplementedYet`] with a static reference to
//! TASK-063, the ticket that owns the `xdg-open` shell-out boundary and
//! `UrlActionMode` gating. The dispatcher does not shell out itself.
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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::actions::map::{WidgetActionEntry, WidgetActionMap, WidgetId};
use crate::actions::timing::{ActionOverlapStrategy, ActionTiming};
use crate::actions::Action;
use crate::ha::client::{AckResult, OutboundCommand, OutboundFrame};
use crate::ha::entity::EntityId;
use crate::ha::live_store::{LiveStore, OptimisticEntry, OptimisticInsertError};
use crate::ha::services::ServiceRegistryHandle;
use crate::ha::store::EntityStore;
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
        }
    }
}

impl std::error::Error for DispatchError {}

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
            .finish()
    }
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
            Action::Url { .. } => Err(DispatchError::NotImplementedYet {
                what: "Url",
                ticket: "TASK-063",
            }),
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
    // Url — deferred to TASK-063
    // -----------------------------------------------------------------------

    #[test]
    fn url_returns_not_implemented_yet_referencing_task_063() {
        let services = handle_from(ServiceRegistry::new());
        let (dispatcher, _rx) = make_dispatcher_with_recorder(services);
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

        let err = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect_err("Url is deferred to TASK-063");
        match err {
            DispatchError::NotImplementedYet { what, ticket } => {
                assert_eq!(what, "Url");
                assert_eq!(ticket, "TASK-063");
            }
            other => panic!("expected NotImplementedYet, got {other:?}"),
        }
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
}
