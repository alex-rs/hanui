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

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::actions::map::{WidgetActionEntry, WidgetActionMap, WidgetId};
use crate::actions::Action;
use crate::ha::client::{AckResult, OutboundCommand, OutboundFrame};
use crate::ha::entity::EntityId;
use crate::ha::live_store::LiveStore;
use crate::ha::services::ServiceRegistryHandle;
use crate::ha::store::EntityStore;

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
        }
    }
}

impl std::error::Error for DispatchError {}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// The action-routing core for Phase 3.
///
/// Stateless beyond its three injected dependencies (`command_tx`,
/// `services`, optional logger). Tests construct one with a fake
/// `mpsc::Sender<OutboundCommand>` recorder via [`Dispatcher::new`].
///
/// # Cloning
///
/// `Dispatcher` is `Clone` so a single instance can be cloned into Slint
/// gesture callbacks. The underlying `mpsc::Sender` is cheap to clone (it
/// shares the same channel) and `ServiceRegistryHandle` is an `Arc`.
#[derive(Debug, Clone)]
pub struct Dispatcher {
    /// Outbound channel to the WS client task. `None` until TASK-072
    /// wires it; in that interim every WS-bound dispatch returns
    /// [`DispatchError::ChannelNotWired`].
    command_tx: Option<mpsc::Sender<OutboundCommand>>,

    /// Shared handle to the `ServiceRegistry` populated by the WS client
    /// (TASK-048 cross-task accessor). Used for Toggle's capability
    /// fallback.
    services: ServiceRegistryHandle,
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
        }
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
            Action::Navigate { view_id } => Ok(DispatchOutcome::Navigate {
                view_id: view_id.clone(),
            }),
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
            } => self.dispatch_call_service(domain, service, target.as_deref(), data.clone()),
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
            return self.dispatch_call_service(
                &domain,
                "toggle",
                Some(entry.entity_id.as_str()),
                None,
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
            let service = match entity.state.as_ref() {
                "on" => "turn_off",
                "off" => "turn_on",
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
            return self.dispatch_call_service(
                &domain,
                service,
                Some(entry.entity_id.as_str()),
                None,
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
        // channel surfaces as ChannelClosed (the most informative
        // available error variant in this PR; TASK-064 may add a
        // dedicated `ChannelFull` once backpressure semantics are
        // wired).
        tx.try_send(cmd).map_err(|e| match e {
            mpsc::error::TrySendError::Closed(_) => DispatchError::ChannelClosed,
            mpsc::error::TrySendError::Full(_) => DispatchError::ChannelClosed,
        })?;

        Ok(DispatchOutcome::Sent { ack_rx })
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
}
