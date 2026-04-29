//! TASK-069 integration tests — protocol/dispatcher subset.
//!
//! Mock-WS observers + dispatcher state assertions + toast-channel observers,
//! NO headless rendering (TASK-073 owns rendering). Each scenario in
//! `docs/backlog/TASK-069.md` and `docs/plans/2026-04-28-phase-3-actions.md`
//! `locked_decisions.optimistic_reconciliation_key` /
//! `locked_decisions.toggle_capability_fallback` /
//! `locked_decisions.idempotency_marker` /
//! `locked_decisions.backpressure` is one `#[test]` or `#[tokio::test]` fn
//! below.
//!
//! The seam under test is the public dispatcher API (`Dispatcher::dispatch`)
//! coupled with:
//!
//! * a fake [`tokio::sync::mpsc::Sender<OutboundCommand>`] recorder (the
//!   "mock WS observer" — every dispatched frame lands here in receipt order),
//! * the `LiveStore` optimistic-entry state,
//! * the toast channel ([`tokio::sync::mpsc::Sender<ToastEvent>`]),
//! * the offline routing context (queue + connection-state watch + toast).
//!
//! This is the same seam TASK-072's full WS round-trip uses; the integration
//! test in `tests/integration/command_tx.rs` covers the WS-side wire path
//! against [`MockWsServer`]. Tests below stay inside the dispatcher boundary
//! so the assertions are about state-machine transitions and observable
//! events, not about wire framing — keeping the suite fast (no port binding)
//! and deterministic (no socket scheduling).
//!
//! # Url + dispatcher integration (TASK-075)
//!
//! [`Action::Url`] is wired through the dispatcher under the
//! [`UrlActionMode`] gate. The handler-level integration tests live in
//! `tests/integration/url_action.rs` (TASK-063 — handler API); the
//! dispatcher-level wiring is captured here by
//! [`url_through_dispatcher_modes`], which exercises every mode (`Always`,
//! `Never`, `Ask`), the invalid-href rejection path, and the spawn-failure
//! path. `Url` is never WS-bound in any mode — every assertion checks for
//! zero `OutboundCommand` frames.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use serde_json::{json, Map};
use tokio::sync::{mpsc, watch};

use hanui::actions::dispatcher::{
    BackpressureScope, DispatchError, DispatchOutcome, Dispatcher, Gesture, QueueRejectReason,
    ToastEvent,
};
use hanui::actions::map::{WidgetActionEntry, WidgetActionMap, WidgetId};
use hanui::actions::queue::OfflineQueue;
use hanui::actions::timing::{ActionOverlapStrategy, ActionTiming};
use hanui::actions::url::{TOAST_ASK_PHASE_6, TOAST_BLOCKED_BY_PROFILE};
use hanui::actions::Action;
use hanui::dashboard::profiles::UrlActionMode;
use hanui::ha::client::{event_to_entity_update, HaAckSuccess, OutboundCommand};
use hanui::ha::entity::{Entity, EntityId};
use hanui::ha::live_store::LiveStore;
use hanui::ha::protocol::{
    EventPayload, EventVariant, RawEntityState, StateChangedData, StateChangedEvent,
};
use hanui::ha::services::{ServiceMeta, ServiceRegistry, ServiceRegistryHandle};
use hanui::ha::store::EntityUpdate;
use hanui::platform::status::ConnectionState;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Wrap a fresh `ServiceRegistry` in the [`ServiceRegistryHandle`] shape.
fn handle_with(reg: ServiceRegistry) -> ServiceRegistryHandle {
    Arc::new(std::sync::RwLock::new(reg))
}

/// Wrap an empty `ServiceRegistry` (the empty-registry / Risk #3 case).
fn empty_handle() -> ServiceRegistryHandle {
    handle_with(ServiceRegistry::new())
}

fn make_entity(id: &str, state: &str) -> Entity {
    Entity {
        id: EntityId::from(id),
        state: Arc::from(state),
        attributes: Arc::new(Map::new()),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    }
}

/// Build a `state_changed` [`EntityUpdate`] from this crate (external to
/// `hanui`).
///
/// `EntityUpdate` is `#[non_exhaustive]`, so a struct-literal in test code is
/// forbidden. The public conversion path is
/// [`event_to_entity_update`]; this helper composes the surrounding
/// `EventPayload` shape with the test-supplied timestamps + state value +
/// optional attributes.
fn make_state_changed_update(
    entity_id: &str,
    new_state: &str,
    last_changed: Timestamp,
    last_updated: Timestamp,
    attrs: serde_json::Value,
) -> EntityUpdate {
    let payload = EventPayload {
        id: 1,
        event: EventVariant::StateChanged(Box::new(StateChangedEvent {
            event_type: "state_changed".to_owned(),
            data: StateChangedData {
                entity_id: entity_id.to_owned(),
                new_state: Some(RawEntityState {
                    entity_id: entity_id.to_owned(),
                    state: new_state.to_owned(),
                    attributes: attrs,
                    last_changed: last_changed.to_string(),
                    last_updated: last_updated.to_string(),
                }),
                old_state: None,
            },
            origin: "LOCAL".to_owned(),
            time_fired: "2024-01-01T00:00:00+00:00".to_owned(),
        })),
    };
    event_to_entity_update(&payload).expect("state_changed payload must produce Some(update)")
}

fn entry_with(entity_id: &str, tap: Action, hold: Action, double_tap: Action) -> WidgetActionEntry {
    WidgetActionEntry {
        entity_id: EntityId::from(entity_id),
        tap,
        hold,
        double_tap,
    }
}

fn one_widget_map(widget_id: &str, entry: WidgetActionEntry) -> WidgetActionMap {
    let mut map = WidgetActionMap::new();
    map.insert(WidgetId::from(widget_id), entry);
    map
}

/// Drive the recorder mpsc receiver into a Vec for an order-asserting
/// observation. `try_recv` is non-blocking; pulling up to `n` frames covers
/// all the in-test scenarios.
fn drain_commands(rx: &mut mpsc::Receiver<OutboundCommand>, n: usize) -> Vec<OutboundCommand> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        match rx.try_recv() {
            Ok(c) => out.push(c),
            Err(_) => break,
        }
    }
    out
}

/// Pull all currently-buffered toast events without blocking. Tests then
/// assert variant identity / count.
fn drain_toasts(rx: &mut mpsc::Receiver<ToastEvent>) -> Vec<ToastEvent> {
    let mut out = Vec::new();
    for _ in 0..16 {
        match rx.try_recv() {
            Ok(t) => out.push(t),
            Err(_) => break,
        }
    }
    out
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

/// Construct a dispatcher (no reconciliation, no offline) backed by a
/// recorder. Convenience wrapper for the action-routing tests that just
/// need to assert frame shape.
fn make_dispatcher_with_recorder(
    services: ServiceRegistryHandle,
) -> (Dispatcher, mpsc::Receiver<OutboundCommand>) {
    let (tx, rx) = mpsc::channel::<OutboundCommand>(16);
    (Dispatcher::with_command_tx(services, tx), rx)
}

/// Construct a dispatcher with optimistic reconciliation enabled. Returns
/// (store, dispatcher, cmd_rx, toast_rx, action_map) — the action map is
/// pre-bound to a single `kitchen_light` widget toggling `light.kitchen`.
#[allow(clippy::type_complexity)]
fn make_optimistic_fixture_for_toggle(
    timing: ActionTiming,
    initial_state: &str,
) -> (
    Arc<LiveStore>,
    Dispatcher,
    mpsc::Receiver<OutboundCommand>,
    mpsc::Receiver<ToastEvent>,
    WidgetActionMap,
) {
    let mut reg = ServiceRegistry::new();
    reg.add_service("light", "toggle", ServiceMeta::default());
    reg.add_service("light", "turn_on", ServiceMeta::default());
    reg.add_service("light", "turn_off", ServiceMeta::default());
    let services = handle_with(reg);

    let store: Arc<LiveStore> = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![make_entity("light.kitchen", initial_state)]);

    let (cmd_tx, cmd_rx) = mpsc::channel::<OutboundCommand>(32);
    let (toast_tx, toast_rx) = mpsc::channel::<ToastEvent>(32);

    let dispatcher = Dispatcher::with_command_tx(services, cmd_tx).with_optimistic_reconciliation(
        store.clone(),
        timing,
        toast_tx,
    );

    let map = one_widget_map(
        "kitchen_light",
        entry_with("light.kitchen", Action::Toggle, Action::None, Action::None),
    );
    (store, dispatcher, cmd_rx, toast_rx, map)
}

/// Construct a dispatcher with the offline-routing context wired. Returns
/// (dispatcher, cmd_rx, toast_rx, queue, state_tx). Connection state
/// defaults to `Connecting`, so the offline branch fires immediately. Tests
/// flip state via `state_tx.send(ConnectionState::Live)` for reconnect
/// scenarios.
#[allow(clippy::type_complexity)]
fn make_offline_fixture(
    services: ServiceRegistryHandle,
) -> (
    Dispatcher,
    mpsc::Receiver<OutboundCommand>,
    mpsc::Receiver<ToastEvent>,
    Arc<StdMutex<OfflineQueue>>,
    watch::Sender<ConnectionState>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<OutboundCommand>(32);
    let (toast_tx, toast_rx) = mpsc::channel::<ToastEvent>(32);
    let queue = Arc::new(StdMutex::new(OfflineQueue::new()));
    let (state_tx, state_rx) = watch::channel(ConnectionState::Connecting);
    let dispatcher = Dispatcher::with_command_tx(services, cmd_tx).with_offline_queue(
        queue.clone(),
        state_rx,
        toast_tx,
    );
    (dispatcher, cmd_rx, toast_rx, queue, state_tx)
}

/// Resolve the next `OutboundCommand` on the recorder with the supplied ack
/// (so the dispatcher's reconciliation task can advance).
async fn deliver_ack_success(rx: &mut mpsc::Receiver<OutboundCommand>, id: u32) {
    let cmd = rx
        .recv()
        .await
        .expect("recorder must yield the dispatched OutboundCommand");
    let _ = cmd.ack_tx.send(Ok(HaAckSuccess { id, payload: None }));
}

// ===========================================================================
// SECTION 1 — Action variant routing (6 variants)
// ===========================================================================
//
// One scenario per `Action` variant, asserting the dispatch outcome shape and
// the recorder's WS-frame observation (or absence of frame for UI-local
// variants). These pin the surface that Phase 4 YAML loaders will populate
// without touching the dispatcher.

// 1. Toggle dispatched (with `<domain>.toggle` registered)
#[tokio::test]
async fn toggle_with_domain_toggle_emits_one_call_service_frame() {
    let mut reg = ServiceRegistry::new();
    reg.add_service("light", "toggle", ServiceMeta::default());
    let services = handle_with(reg);

    let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
    let map = one_widget_map(
        "kitchen_light",
        entry_with("light.kitchen", Action::Toggle, Action::None, Action::None),
    );
    let store = LiveStore::new();
    store.apply_snapshot(vec![make_entity("light.kitchen", "on")]);

    let outcome = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("Toggle dispatch must succeed when light.toggle is registered");
    assert!(matches!(outcome, DispatchOutcome::Sent { .. }));

    let frames = drain_commands(&mut rx, 4);
    assert_eq!(frames.len(), 1, "Toggle must emit exactly one frame");
    assert_eq!(frames[0].frame.domain, "light");
    assert_eq!(frames[0].frame.service, "toggle");
    assert_eq!(
        frames[0].frame.target,
        Some(json!({ "entity_id": "light.kitchen" }))
    );
    assert_eq!(frames[0].frame.data, None);
}

// 2. CallService dispatched
#[tokio::test]
async fn call_service_emits_one_frame_with_supplied_fields() {
    let services = empty_handle();
    let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
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
    let store = LiveStore::new();
    store.apply_snapshot(vec![make_entity("light.kitchen", "off")]);

    let outcome = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("CallService must dispatch");
    assert!(matches!(outcome, DispatchOutcome::Sent { .. }));

    let frames = drain_commands(&mut rx, 4);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].frame.domain, "light");
    assert_eq!(frames[0].frame.service, "turn_on");
    assert_eq!(
        frames[0].frame.target,
        Some(json!({ "entity_id": "light.kitchen" }))
    );
    assert_eq!(frames[0].frame.data, Some(json!({ "brightness": 200 })));
}

// 3. MoreInfo emits UI event (no WS frame)
#[tokio::test]
async fn more_info_emits_ui_outcome_and_no_frame() {
    let services = empty_handle();
    let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
    let map = one_widget_map(
        "kitchen_light",
        entry_with(
            "light.kitchen",
            Action::None,
            Action::MoreInfo,
            Action::None,
        ),
    );
    let store = LiveStore::new();

    let outcome = dispatcher
        .dispatch(
            &WidgetId::from("kitchen_light"),
            Gesture::Hold,
            &store,
            &map,
        )
        .expect("MoreInfo dispatch must succeed");
    match outcome {
        DispatchOutcome::MoreInfo { entity_id } => {
            assert_eq!(entity_id, EntityId::from("light.kitchen"));
        }
        other => panic!("expected MoreInfo, got {other:?}"),
    }
    assert_eq!(
        drain_commands(&mut rx, 1).len(),
        0,
        "MoreInfo must NOT send a WS frame"
    );
}

// 4. Navigate emits UI event (no WS frame)
#[tokio::test]
async fn navigate_emits_ui_outcome_and_no_frame() {
    let services = empty_handle();
    let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
    let map = one_widget_map(
        "kitchen_light",
        entry_with(
            "light.kitchen",
            Action::Navigate {
                view_id: "default".to_owned(),
            },
            Action::None,
            Action::None,
        ),
    );
    let store = LiveStore::new();

    let outcome = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("Navigate dispatch must succeed");
    match outcome {
        DispatchOutcome::Navigate { view_id } => assert_eq!(view_id, "default"),
        other => panic!("expected Navigate, got {other:?}"),
    }
    assert_eq!(drain_commands(&mut rx, 1).len(), 0);
}

// 5. Url through dispatcher — TASK-075 wires the handler with the
//    UrlActionMode gate. This test exercises every mode's observable plus
//    the two error paths (invalid href, spawn failure). The previous
//    `url_through_dispatcher_returns_not_implemented_yet` assertion was
//    deleted in TASK-075 because its inverse — dispatch returning a routed
//    outcome — is exactly what this ticket flips.
//
// Spawner state is shared across `#[tokio::test]` instances via statics
// because `Spawner = fn(&str) -> io::Result<()>` cannot capture closures.
// The test serialises with a Mutex so concurrent libtest threads do not
// race on SPAWN_COUNT / SPAWN_FAILS.
mod url_dispatcher_modes_recorder {
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(super) static SPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);
    pub(super) static SPAWN_FAILS: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    pub(super) static SPAWN_FAILS_LONG: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    pub(super) static SPAWN_LAST_HREF: std::sync::Mutex<Option<String>> =
        std::sync::Mutex::new(None);
    pub(super) static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(super) fn reset(force_fail: bool) {
        SPAWN_COUNT.store(0, Ordering::SeqCst);
        SPAWN_FAILS.store(force_fail, Ordering::SeqCst);
        SPAWN_FAILS_LONG.store(false, Ordering::SeqCst);
        *SPAWN_LAST_HREF.lock().unwrap() = None;
    }

    pub(super) fn reset_with_long_failure() {
        SPAWN_COUNT.store(0, Ordering::SeqCst);
        SPAWN_FAILS.store(true, Ordering::SeqCst);
        SPAWN_FAILS_LONG.store(true, Ordering::SeqCst);
        *SPAWN_LAST_HREF.lock().unwrap() = None;
    }

    /// Recording spawner with `fn`-pointer signature (no closure capture).
    /// Records the href so the Always-mode test can assert it was forwarded
    /// verbatim, and toggles between Ok and `NotFound` based on SPAWN_FAILS.
    /// When SPAWN_FAILS_LONG is set, the failure carries a >256-byte
    /// message so the dispatcher's UTF-8-aware truncation branch executes.
    pub(super) fn recording_spawner(href: &str) -> io::Result<()> {
        SPAWN_COUNT.fetch_add(1, Ordering::SeqCst);
        *SPAWN_LAST_HREF.lock().unwrap() = Some(href.to_owned());
        if SPAWN_FAILS.load(Ordering::SeqCst) {
            let message = if SPAWN_FAILS_LONG.load(Ordering::SeqCst) {
                // 4-byte codepoint × 80 = 320 bytes — past the 256 cap.
                // Forces the truncation branch in dispatcher.rs to walk
                // backwards from byte 256 to the nearest char boundary.
                "🟥".repeat(80)
            } else {
                "test forced spawn failure".to_owned()
            };
            Err(io::Error::new(io::ErrorKind::NotFound, message))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn url_through_dispatcher_modes() {
    use std::sync::atomic::Ordering;
    use url_dispatcher_modes_recorder::{
        recording_spawner, reset, SPAWN_COUNT, SPAWN_LAST_HREF, TEST_SERIAL,
    };

    let _serial = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());

    let map = one_widget_map(
        "url_widget",
        entry_with(
            "light.kitchen",
            Action::Url {
                href: "https://example.org/path".to_owned(),
            },
            Action::None,
            Action::None,
        ),
    );
    let store = LiveStore::new();

    // -------- Always: spawner called once, Ok(UrlOpened), zero frames.
    {
        reset(false);
        let services = empty_handle();
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let dispatcher = dispatcher
            .with_url_action_mode(UrlActionMode::Always)
            .with_url_spawner(recording_spawner);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("url_widget"), Gesture::Tap, &store, &map)
            .expect("Always mode must succeed for a valid href");
        assert!(
            matches!(outcome, DispatchOutcome::UrlOpened),
            "Always mode must return UrlOpened, got {outcome:?}"
        );
        assert_eq!(
            SPAWN_COUNT.load(Ordering::SeqCst),
            1,
            "Always mode must invoke the spawner exactly once"
        );
        assert_eq!(
            SPAWN_LAST_HREF.lock().unwrap().as_deref(),
            Some("https://example.org/path"),
            "spawner must receive the href verbatim"
        );
        assert_eq!(
            drain_commands(&mut rx, 1).len(),
            0,
            "Url is never WS-bound: zero OutboundCommand frames in Always mode"
        );
    }

    // -------- Never: no spawn, Ok(UrlBlockedToast(TOAST_BLOCKED_BY_PROFILE)).
    {
        reset(false);
        let services = empty_handle();
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let dispatcher = dispatcher
            .with_url_action_mode(UrlActionMode::Never)
            .with_url_spawner(recording_spawner);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("url_widget"), Gesture::Tap, &store, &map)
            .expect("Never mode must return Ok with a blocked-toast outcome");
        match outcome {
            DispatchOutcome::UrlBlockedToast { text } => {
                assert_eq!(text, TOAST_BLOCKED_BY_PROFILE);
            }
            other => panic!("expected UrlBlockedToast, got {other:?}"),
        }
        assert_eq!(
            SPAWN_COUNT.load(Ordering::SeqCst),
            0,
            "Never mode must NOT invoke the spawner"
        );
        assert_eq!(drain_commands(&mut rx, 1).len(), 0);
    }

    // -------- Ask: no spawn, Ok(UrlAskToast(TOAST_ASK_PHASE_6)).
    {
        reset(false);
        let services = empty_handle();
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let dispatcher = dispatcher
            .with_url_action_mode(UrlActionMode::Ask)
            .with_url_spawner(recording_spawner);

        let outcome = dispatcher
            .dispatch(&WidgetId::from("url_widget"), Gesture::Tap, &store, &map)
            .expect("Ask mode must return Ok with the Phase-6 deferred toast");
        match outcome {
            DispatchOutcome::UrlAskToast { text } => {
                assert_eq!(text, TOAST_ASK_PHASE_6);
            }
            other => panic!("expected UrlAskToast, got {other:?}"),
        }
        assert_eq!(SPAWN_COUNT.load(Ordering::SeqCst), 0);
        assert_eq!(drain_commands(&mut rx, 1).len(), 0);
    }

    // -------- Invalid href in any mode: UrlInvalidHref, no spawn.
    //          The classic shell-meta payload `;rm -rf /` must be rejected
    //          BEFORE the Always branch reaches the spawner.
    {
        let bad_map = one_widget_map(
            "url_widget",
            entry_with(
                "light.kitchen",
                Action::Url {
                    href: "https://example.org/;rm -rf /".to_owned(),
                },
                Action::None,
                Action::None,
            ),
        );
        for mode in [
            UrlActionMode::Always,
            UrlActionMode::Never,
            UrlActionMode::Ask,
        ] {
            reset(false);
            let services = empty_handle();
            let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
            let dispatcher = dispatcher
                .with_url_action_mode(mode)
                .with_url_spawner(recording_spawner);

            let err = dispatcher
                .dispatch(
                    &WidgetId::from("url_widget"),
                    Gesture::Tap,
                    &store,
                    &bad_map,
                )
                .expect_err("shell-meta href must be rejected pre-spawn");
            match err {
                DispatchError::UrlInvalidHref { reason } => {
                    assert!(
                        reason.contains("metacharacter") || reason.contains("shell"),
                        "rejection reason must cite shell metacharacter, got: {reason}"
                    );
                }
                other => panic!("expected UrlInvalidHref, got {other:?} (mode {mode:?})"),
            }
            assert_eq!(
                SPAWN_COUNT.load(Ordering::SeqCst),
                0,
                "spawner must NOT be invoked for an invalid href (mode {mode:?})"
            );
            assert_eq!(drain_commands(&mut rx, 1).len(), 0);
        }
    }

    // -------- Spawn failure in Always mode: UrlSpawnFailed, reason
    //          carries the io::Error Display form but NOT the href.
    {
        reset(true); // recording spawner returns NotFound on call.
        let services = empty_handle();
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let dispatcher = dispatcher
            .with_url_action_mode(UrlActionMode::Always)
            .with_url_spawner(recording_spawner);

        let err = dispatcher
            .dispatch(&WidgetId::from("url_widget"), Gesture::Tap, &store, &map)
            .expect_err("forced spawn failure must surface as UrlSpawnFailed");
        match err {
            DispatchError::UrlSpawnFailed { reason } => {
                assert!(
                    !reason.contains("example.org"),
                    "UrlSpawnFailed.reason must NOT leak the href, got: {reason}"
                );
                assert!(
                    reason.contains("test forced spawn failure")
                        || reason.contains("not found")
                        || reason.contains("No such"),
                    "reason must surface the underlying io::Error Display form, got: {reason}"
                );
            }
            other => panic!("expected UrlSpawnFailed, got {other:?}"),
        }
        assert_eq!(SPAWN_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(drain_commands(&mut rx, 1).len(), 0);
    }

    // -------- Long spawn-failure message (>256 bytes, multi-byte) — exercises
    //          the dispatcher's UTF-8-aware truncation branch. The reason is
    //          ≤256 bytes AND a valid UTF-8 prefix of the original message
    //          (so no codepoint is split mid-byte).
    {
        url_dispatcher_modes_recorder::reset_with_long_failure();
        let services = empty_handle();
        let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
        let dispatcher = dispatcher
            .with_url_action_mode(UrlActionMode::Always)
            .with_url_spawner(recording_spawner);

        let err = dispatcher
            .dispatch(&WidgetId::from("url_widget"), Gesture::Tap, &store, &map)
            .expect_err("long-message forced spawn failure must surface as UrlSpawnFailed");
        match err {
            DispatchError::UrlSpawnFailed { reason } => {
                assert!(
                    reason.len() <= 256,
                    "truncated reason must be ≤256 bytes, got {}",
                    reason.len()
                );
                // `reason` stores `io_err.to_string()` directly — the
                // dispatcher does NOT prepend "failed to spawn xdg-open:"
                // at storage time (that wrapping happens at
                // DispatchError::Display time). The forced error message
                // is "🟥".repeat(80); truncated to ≤256 bytes the result
                // is some whole number of 🟥 codepoints — never a
                // half-codepoint at the boundary.
                assert!(
                    reason.chars().all(|c| c == '🟥'),
                    "every char in the truncated reason must be the 🟥 codepoint, got: {reason}"
                );
                // String guarantees valid UTF-8 — the assertion above
                // would not even compile-as-iterable if the truncation
                // had broken UTF-8.
                assert!(
                    !reason.contains("example.org"),
                    "UrlSpawnFailed.reason must not leak the href"
                );
            }
            other => panic!("expected UrlSpawnFailed, got {other:?}"),
        }
        assert_eq!(SPAWN_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(drain_commands(&mut rx, 1).len(), 0);
    }
}

/// Coverage probe: the `Dispatcher` `Debug` impl is otherwise never
/// invoked by tests. Format the dispatcher with `{:?}` so the impl body
/// (notably the new `url_action_mode` and `url_spawner` fields) executes.
#[test]
fn dispatcher_debug_impl_executes_for_coverage() {
    let services = empty_handle();
    let (tx, _rx) = mpsc::channel::<OutboundCommand>(1);
    let dispatcher = Dispatcher::with_command_tx(services, tx);
    let s = format!("{dispatcher:?}");
    // Sanity: the Debug output must mention the struct name and the new
    // url-action-mode / url-spawner fields.
    assert!(s.contains("Dispatcher"));
    assert!(s.contains("url_action_mode"));
    assert!(s.contains("url_spawner"));
}

// 6. None → no-op
#[tokio::test]
async fn action_none_is_no_op_no_command_no_event() {
    let services = empty_handle();
    let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
    let map = one_widget_map(
        "kitchen_light",
        entry_with("light.kitchen", Action::None, Action::None, Action::None),
    );
    let store = LiveStore::new();

    let outcome = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("Action::None must dispatch as NoOp");
    assert!(matches!(outcome, DispatchOutcome::NoOp));
    assert_eq!(drain_commands(&mut rx, 1).len(), 0);
}

// ===========================================================================
// SECTION 2 — Toggle capability fallback (3 cases)
// ===========================================================================
//
// `locked_decisions.toggle_capability_fallback`:
//   1. <domain>.toggle present → toggle dispatched
//   2. toggle absent + turn_on/turn_off pair present → pair dispatched
//   3. neither present → Err(NoCapability)

// 7. <domain>.toggle present → toggle dispatched
#[tokio::test]
async fn toggle_capability_branch1_uses_domain_toggle() {
    let mut reg = ServiceRegistry::new();
    reg.add_service("light", "toggle", ServiceMeta::default());
    // Pair also registered: branch 1 takes priority over branch 2.
    reg.add_service("light", "turn_on", ServiceMeta::default());
    reg.add_service("light", "turn_off", ServiceMeta::default());
    let services = handle_with(reg);

    let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
    let map = one_widget_map(
        "kitchen_light",
        entry_with("light.kitchen", Action::Toggle, Action::None, Action::None),
    );
    let store = LiveStore::new();
    store.apply_snapshot(vec![make_entity("light.kitchen", "on")]);

    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("dispatch must succeed");
    let frames = drain_commands(&mut rx, 2);
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].frame.service, "toggle",
        "branch 1 (toggle) must take priority over branch 2 (turn_on/turn_off pair)"
    );
}

// 8. toggle absent + turn_on/turn_off pair present → pair dispatched
//    (verify on/off chosen from current state)
#[tokio::test]
async fn toggle_capability_branch2_pair_uses_state_to_choose_on_or_off() {
    let mut reg = ServiceRegistry::new();
    reg.add_service("switch", "turn_on", ServiceMeta::default());
    reg.add_service("switch", "turn_off", ServiceMeta::default());
    // No <domain>.toggle.
    let services = handle_with(reg);

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

    // state == "on" → must dispatch turn_off.
    let store_on = LiveStore::new();
    store_on.apply_snapshot(vec![make_entity("switch.outlet_1", "on")]);
    let _ = dispatcher
        .dispatch(&WidgetId::from("outlet"), Gesture::Tap, &store_on, &map)
        .expect("dispatch must succeed");
    let frame_off = drain_commands(&mut rx, 2);
    assert_eq!(frame_off.len(), 1);
    assert_eq!(frame_off[0].frame.service, "turn_off");

    // state == "off" → must dispatch turn_on.
    let store_off = LiveStore::new();
    store_off.apply_snapshot(vec![make_entity("switch.outlet_1", "off")]);
    let _ = dispatcher
        .dispatch(&WidgetId::from("outlet"), Gesture::Tap, &store_off, &map)
        .expect("dispatch must succeed");
    let frame_on = drain_commands(&mut rx, 2);
    assert_eq!(frame_on.len(), 1);
    assert_eq!(frame_on[0].frame.service, "turn_on");
}

// 9. Both absent → Err(NoCapability) + descriptive error toast surface
#[tokio::test]
async fn toggle_capability_branch3_neither_returns_no_capability() {
    let services = empty_handle();
    let (dispatcher, mut rx) = make_dispatcher_with_recorder(services);
    let map = one_widget_map(
        "kitchen_light",
        entry_with("light.kitchen", Action::Toggle, Action::None, Action::None),
    );
    let store = LiveStore::new();
    store.apply_snapshot(vec![make_entity("light.kitchen", "on")]);

    let err = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect_err("empty registry must produce NoCapability");
    match err {
        DispatchError::NoCapability { ref domain, .. } => assert_eq!(domain, "light"),
        other => panic!("expected NoCapability, got {other:?}"),
    }

    // Toast surface: the Display string is what TASK-067 will surface.
    let display = format!("{err}");
    assert!(
        display.contains("light"),
        "NoCapability Display must cite the missing domain: {display}"
    );
    // No frame escaped on the error path.
    assert_eq!(drain_commands(&mut rx, 1).len(), 0);
}

// ===========================================================================
// SECTION 3 — Optimistic reconciliation (5 rules from locked_decisions)
// ===========================================================================
//
// Each rule has a dedicated test below. These exercise the full dispatcher →
// LiveStore → reconciliation-task seam (the one wired by
// `Dispatcher::with_optimistic_reconciliation`). Rules 1 and 3 fire inside
// `LiveStore::apply_event`; rules 2, 4, 5 fire inside the reconciliation task.

// 10. Rule 1: ack with state_changed (last_changed > dispatched_at) → drop.
#[tokio::test]
async fn rule_1_ack_with_state_changed_drops_entry_and_renders_new_state() {
    let timing = ActionTiming::default();
    let (store, dispatcher, mut cmd_rx, _toast_rx, map) =
        make_optimistic_fixture_for_toggle(timing, "off");

    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("dispatch must succeed");

    let entries = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
    assert_eq!(entries.len(), 1, "entry recorded after dispatch");

    // state_changed with last_changed > dispatched_at → rule 1 drops the entry
    // synchronously inside apply_event.
    let new_last_changed = Timestamp::now()
        .checked_add(SignedDuration::from_millis(100))
        .unwrap();
    store.apply_event(make_state_changed_update(
        "light.kitchen",
        "on",
        new_last_changed,
        new_last_changed,
        json!({}),
    ));
    assert!(
        store
            .optimistic_entries_for(&EntityId::from("light.kitchen"))
            .is_empty(),
        "rule 1: entry dropped by inbound state_changed"
    );

    // Tile-visible state (via the live snapshot) is now the new HA truth.
    let cur = store
        .snapshot()
        .get(&EntityId::from("light.kitchen"))
        .map(|e| e.state.clone());
    assert_eq!(cur.as_deref(), Some("on"));

    // Drain the ack so the reconciliation task completes cleanly.
    deliver_ack_success(&mut cmd_rx, 1).await;
}

// 11a. Rule 2 — ack-without-event no-op success, snapshot match → drop.
#[tokio::test]
async fn rule_2_no_op_success_with_snapshot_match_drops_entry() {
    // Tentative state matches the current snapshot at ack time → no-op
    // confirmed → entry dropped without revert.
    let timing = ActionTiming::default();
    let (store, dispatcher, mut cmd_rx, _toast_rx, map) =
        make_optimistic_fixture_for_toggle(timing, "off");

    // Dispatch toggle from "off" — tentative becomes "on".
    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("dispatch must succeed");

    // Pre-fill snapshot to "on" (matches tentative). Crucially we leave
    // last_changed older than dispatched_at so apply_event's rule-1 path does
    // NOT fire; only the rule-2 ack-time snapshot match should drive the drop.
    let entries = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
    let dispatched_at = entries[0].dispatched_at;
    store.apply_snapshot(vec![Entity {
        id: EntityId::from("light.kitchen"),
        state: Arc::from("on"),
        attributes: Arc::new(Map::new()),
        last_changed: dispatched_at
            .checked_sub(SignedDuration::from_millis(10))
            .unwrap(),
        last_updated: dispatched_at
            .checked_sub(SignedDuration::from_millis(10))
            .unwrap(),
    }]);

    deliver_ack_success(&mut cmd_rx, 1).await;

    let cleared = wait_until(
        || {
            store
                .optimistic_entries_for(&EntityId::from("light.kitchen"))
                .is_empty()
        },
        Duration::from_secs(1),
    )
    .await;
    assert!(cleared, "rule 2: snapshot match drops entry without revert");
}

// 11b. Rule 2 — ack-without-event mismatch → hold to timeout → revert.
#[tokio::test]
async fn rule_2_no_op_success_with_mismatch_holds_then_reverts() {
    let timing = ActionTiming {
        optimistic_timeout_ms: 80, // short window for the test
        ..ActionTiming::default()
    };
    let (store, dispatcher, mut cmd_rx, _toast_rx, map) =
        make_optimistic_fixture_for_toggle(timing, "off");

    // Tentative is "on"; snapshot stays "off" (mismatch). Ack-success arrives
    // but no state_changed → rule-2 holds, then reverts.
    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("dispatch must succeed");
    deliver_ack_success(&mut cmd_rx, 1).await;

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

// 12. Rule 3 — attribute-only state_changed leaves entry intact.
#[tokio::test]
async fn rule_3_attribute_only_state_changed_leaves_entry_intact() {
    let timing = ActionTiming::default();
    let (store, dispatcher, _cmd_rx, _toast_rx, map) =
        make_optimistic_fixture_for_toggle(timing, "off");

    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("dispatch must succeed");

    let entries = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
    let dispatched_at = entries[0].dispatched_at;

    // Attribute-only event: state value unchanged, last_changed UNCHANGED
    // (older than dispatched_at), last_updated advances. Rule 3 leaves the
    // optimistic entry intact while the attribute snapshot updates.
    let last_changed_old = dispatched_at
        .checked_sub(SignedDuration::from_millis(50))
        .unwrap();
    let attrs = json!({ "brightness": 180 });
    store.apply_event(make_state_changed_update(
        "light.kitchen",
        "off", // unchanged
        last_changed_old,
        Timestamp::now(),
        attrs,
    ));

    assert_eq!(
        store
            .optimistic_entries_for(&EntityId::from("light.kitchen"))
            .len(),
        1,
        "rule 3: attribute-only event must leave optimistic entry intact"
    );

    // Attribute snapshot updated independently (the dispatcher's reconciliation
    // task does not gate this).
    let attrs_now = store
        .snapshot()
        .get(&EntityId::from("light.kitchen"))
        .map(|e| e.attributes.clone())
        .expect("entity must still be present");
    assert_eq!(attrs_now.get("brightness"), Some(&json!(180)));
}

// 13. Rule 4 — ack error reverts (drops entry) immediately.
#[tokio::test]
async fn rule_4_ack_error_drops_entry_immediately() {
    let timing = ActionTiming::default();
    let (store, dispatcher, mut cmd_rx, _toast_rx, map) =
        make_optimistic_fixture_for_toggle(timing, "off");

    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("dispatch must succeed");

    // Resolve the ack with HaAckError (rule 4).
    let cmd = cmd_rx.recv().await.expect("cmd recorded");
    let _ = cmd.ack_tx.send(Err(hanui::ha::client::HaAckError {
        id: 1,
        code: "not_found".to_owned(),
        message: "service not found".to_owned(),
    }));

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
        "rule 4: ack-error must drop the optimistic entry (revert path)"
    );
}

// 14. Rule 5 — optimistic timeout reverts.
#[tokio::test]
async fn rule_5_optimistic_timeout_drops_entry() {
    let timing = ActionTiming {
        optimistic_timeout_ms: 50,
        ..ActionTiming::default()
    };
    let (store, dispatcher, _cmd_rx, _toast_rx, map) =
        make_optimistic_fixture_for_toggle(timing, "off");

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

// ===========================================================================
// SECTION 4 — Out-of-order ack (last_changed tie-break)
// ===========================================================================
//
// The newer state_changed (last_changed > dispatched_at) drops the entry via
// rule 1 BEFORE the older ack arrives; the late ack must therefore be a no-op
// — it must NOT revert the newer state. Pin both: the snapshot's state stays
// at the newer value, AND the optimistic bucket stays empty.

// 15. Out-of-order ack: newer state_changed before older ack — older ack does
//     not revert.
#[tokio::test]
async fn out_of_order_ack_does_not_revert_newer_state() {
    let timing = ActionTiming::default();
    let (store, dispatcher, mut cmd_rx, _toast_rx, map) =
        make_optimistic_fixture_for_toggle(timing, "off");

    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("dispatch must succeed");

    // Newer state_changed arrives FIRST (out of order vs. ack).
    let new_last_changed = Timestamp::now()
        .checked_add(SignedDuration::from_millis(100))
        .unwrap();
    store.apply_event(make_state_changed_update(
        "light.kitchen",
        "on",
        new_last_changed,
        new_last_changed,
        json!({}),
    ));
    assert!(
        store
            .optimistic_entries_for(&EntityId::from("light.kitchen"))
            .is_empty(),
        "rule 1 already dropped the entry"
    );

    // Stale ack arrives later. The reconciliation task observes the missing
    // entry and is a no-op.
    deliver_ack_success(&mut cmd_rx, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Newer state_changed must NOT have been reverted.
    let cur = store
        .snapshot()
        .get(&EntityId::from("light.kitchen"))
        .map(|e| e.state.clone());
    assert_eq!(
        cur.as_deref(),
        Some("on"),
        "newer state_changed must not be reverted by stale ack"
    );
    assert!(
        store
            .optimistic_entries_for(&EntityId::from("light.kitchen"))
            .is_empty(),
        "no phantom entry left after stale ack"
    );
}

// ===========================================================================
// SECTION 5 — action_overlap_strategy: LastWriteWins
// ===========================================================================
//
// (a) N=2: second tap while first pending cancels the old entry, sends two
//     service-call frames, preserves chain-root prior_state.
// (b) N=3: third tap with two acks outstanding — canceled mid-entry's late
//     ack must NOT clear or revert the current entry. (Codex review
//     2026-04-28: load-bearing.)

// 16. N=2: second tap cancels old entry, sends two frames, prior_state chain
//     is preserved (root is "off").
#[tokio::test]
async fn last_write_wins_n2_two_frames_one_entry_chain_root_preserved() {
    let timing = ActionTiming::default(); // LastWriteWins is the default
    let (store, dispatcher, mut cmd_rx, _toast_rx, _map_unused) =
        make_optimistic_fixture_for_toggle(timing, "off");

    // CallService with explicit turn_on so prior="off", tentative="on" deterministically.
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

    // First dispatch.
    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("first dispatch must succeed");

    let entries = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].prior_state.as_ref(), "off");
    assert_eq!(entries[0].tentative_state.as_ref(), "on");

    // Pretend the snapshot has flipped to "on" (simulating the optimistic
    // update applied) without a state_changed yet. Chain-root rule must still
    // preserve prior="off" on the next dispatch.
    store.apply_snapshot(vec![make_entity("light.kitchen", "on")]);

    // Second dispatch: LastWriteWins cancels the first entry, creates a new one.
    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("second dispatch must succeed");

    // Exactly two service-call frames must have been observed in receipt order.
    let frames = drain_commands(&mut cmd_rx, 4);
    assert_eq!(
        frames.len(),
        2,
        "LastWriteWins N=2: exactly two service-call frames"
    );
    assert_eq!(frames[0].frame.service, "turn_on");
    assert_eq!(frames[1].frame.service, "turn_on");

    let entries_after = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
    assert_eq!(
        entries_after.len(),
        1,
        "LastWriteWins: only the new entry remains (old cancelled)"
    );
    assert_eq!(
        entries_after[0].prior_state.as_ref(),
        "off",
        "LastWriteWins: new entry's prior_state preserves the chain root"
    );
}

// 17. N=3: third tap with two acks outstanding — the canceled mid-entry's
//     late-arriving ack must NOT clear/revert the current entry.
#[tokio::test]
async fn last_write_wins_n3_canceled_mid_entry_late_ack_does_not_clear_current() {
    let timing = ActionTiming::default();
    let (store, dispatcher, mut cmd_rx, _toast_rx, _) =
        make_optimistic_fixture_for_toggle(timing, "off");

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

    // Tap 1.
    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("tap 1");
    // Tap 2 — cancels tap 1's entry. (cmd #1 is now orphan; its ack will
    // arrive late and must NOT clear the current entry.)
    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("tap 2");
    // Tap 3 — cancels tap 2's entry. cmd #2 is now also orphan.
    let _ = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("tap 3");

    // Three frames recorded; only one optimistic entry remains (tap 3's).
    let frames = drain_commands(&mut cmd_rx, 8);
    assert_eq!(frames.len(), 3, "three frames observed in FIFO order");
    let entries_after = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
    assert_eq!(
        entries_after.len(),
        1,
        "LastWriteWins N=3: only the latest (tap 3) entry remains"
    );
    let current_request_id = entries_after[0].request_id;

    // Now resolve the FIRST and SECOND commands' acks (the canceled
    // mid-entries' late acks). Their reconciliation tasks should observe
    // missing entries and be no-ops. `oneshot::Sender` is not `Clone`, so we
    // move ownership out of the recorded frames via `into_iter`.
    let mut frames_iter = frames.into_iter();
    let cmd1 = frames_iter.next().expect("frame 1");
    let cmd2 = frames_iter.next().expect("frame 2");
    let cmd3 = frames_iter.next().expect("frame 3");

    let _ = cmd1.ack_tx.send(Ok(HaAckSuccess {
        id: 1,
        payload: None,
    }));
    let _ = cmd2.ack_tx.send(Ok(HaAckSuccess {
        id: 2,
        payload: None,
    }));

    // Give the reconciliation tasks time to run.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The current (tap 3) entry must STILL be present — the canceled
    // mid-entries' late acks must not have cleared / reverted it.
    let entries_now = store.optimistic_entries_for(&EntityId::from("light.kitchen"));
    assert_eq!(
        entries_now.len(),
        1,
        "tap 3 entry must survive the canceled mid-entries' late acks"
    );
    assert_eq!(
        entries_now[0].request_id, current_request_id,
        "the surviving entry must still be tap 3 (same request_id)"
    );

    // Cleanup: deliver tap 3's ack so the reconciliation task can complete.
    // Snapshot still says "off", so rule-2 mismatch will hold-and-revert via
    // timeout — that's not what we're asserting here, but we want to drop the
    // sender cleanly so the test does not leak a pending oneshot.
    let _ = cmd3.ack_tx.send(Ok(HaAckSuccess {
        id: 3,
        payload: None,
    }));
}

// ===========================================================================
// SECTION 6 — Idempotency gating (offline)
// ===========================================================================
//
// `locked_decisions.idempotency_marker`: Toggle is NonIdempotent → never
// queued; Url is NonIdempotent → never queued. CallService is checked at
// runtime against the allowlist (turn_on / turn_off / set_*).

// 18. Toggle disconnected → Err, queue empty, toast emitted.
#[tokio::test]
async fn offline_toggle_returns_err_queue_empty_and_emits_toast() {
    let services = empty_handle();
    let (dispatcher, mut cmd_rx, mut toast_rx, queue, _state_tx) = make_offline_fixture(services);

    let map = one_widget_map(
        "kitchen_light",
        entry_with("light.kitchen", Action::Toggle, Action::None, Action::None),
    );
    let store = LiveStore::new();
    store.apply_snapshot(vec![make_entity("light.kitchen", "on")]);

    let err = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect_err("Toggle offline must return Err (Risk #6)");
    assert!(matches!(err, DispatchError::OfflineNonIdempotent { .. }));

    // Queue contains zero entries.
    assert_eq!(
        queue.lock().unwrap().len(),
        0,
        "Toggle rejection must leave the queue empty"
    );
    // No frame escaped on the offline rejection path.
    assert_eq!(drain_commands(&mut cmd_rx, 1).len(), 0);
    // Toast emitted (load-bearing security signal).
    let toasts = drain_toasts(&mut toast_rx);
    assert!(
        toasts
            .iter()
            .any(|t| matches!(t, ToastEvent::OfflineNonIdempotent { .. })),
        "expected OfflineNonIdempotent toast, got {toasts:?}"
    );
}

// 19. Url disconnected → Err, queue empty, toast emitted.
//     Url action is non-idempotent (idempotency_marker) so the offline
//     branch fires before the live-path UrlActionMode gate (TASK-075) can
//     even consult `url_action_mode`. The offline gate is the test seam
//     here.
#[tokio::test]
async fn offline_url_returns_err_queue_empty_and_emits_toast() {
    let services = empty_handle();
    let (dispatcher, _cmd_rx, mut toast_rx, queue, _state_tx) = make_offline_fixture(services);

    let map = one_widget_map(
        "url_widget",
        entry_with(
            "light.kitchen",
            Action::Url {
                href: "https://example.org".to_owned(),
            },
            Action::None,
            Action::None,
        ),
    );
    let store = LiveStore::new();
    store.apply_snapshot(vec![make_entity("light.kitchen", "on")]);

    let err = dispatcher
        .dispatch(&WidgetId::from("url_widget"), Gesture::Tap, &store, &map)
        .expect_err("Url offline must return Err (idempotency_marker)");
    assert!(matches!(err, DispatchError::OfflineNonIdempotent { .. }));

    assert_eq!(queue.lock().unwrap().len(), 0, "Url offline: queue empty");
    let toasts = drain_toasts(&mut toast_rx);
    assert!(toasts
        .iter()
        .any(|t| matches!(t, ToastEvent::OfflineNonIdempotent { .. })));
}

// 20. CallService disconnected, NOT allowlisted → Err, queue empty, toast.
#[tokio::test]
async fn offline_call_service_not_allowlisted_returns_err_queue_empty() {
    let services = empty_handle();
    let (dispatcher, _cmd_rx, mut toast_rx, queue, _state_tx) = make_offline_fixture(services);

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
    let store = LiveStore::new();
    store.apply_snapshot(vec![make_entity("light.kitchen", "on")]);

    let err = dispatcher
        .dispatch(&WidgetId::from("danger_widget"), Gesture::Tap, &store, &map)
        .expect_err("non-allowlisted CallService offline must Err");
    match err {
        DispatchError::OfflineQueueRejected { reason, .. } => {
            assert_eq!(reason, QueueRejectReason::ServiceNotAllowlisted);
        }
        other => panic!("expected OfflineQueueRejected, got {other:?}"),
    }
    assert_eq!(
        queue.lock().unwrap().len(),
        0,
        "non-allowlisted CallService: queue empty"
    );
    let toasts = drain_toasts(&mut toast_rx);
    assert!(toasts.iter().any(|t| matches!(
        t,
        ToastEvent::OfflineQueueRejected {
            reason: QueueRejectReason::ServiceNotAllowlisted,
            ..
        }
    )));
}

// ===========================================================================
// SECTION 7 — Reconnect-flush (FIFO order)
// ===========================================================================
//
// Queue 5 idempotent CallService actions while offline, reconnect (transition
// state to Live), invoke `OfflineQueue::flush` against a recorder, observe 5
// frames in FIFO order. The flush API is the production seam the reconnect
// FSM uses (see `tests/integration/command_tx.rs` for the WS-side wiring).

// 21. Reconnect-flush 5 actions → 5 frames in FIFO order.
#[tokio::test]
async fn reconnect_flush_preserves_fifo_order_for_five_idempotent_actions() {
    let services = empty_handle();
    let (dispatcher, mut cmd_rx, _toast_rx, queue, state_tx) = make_offline_fixture(services);

    // Enqueue 5 distinct allowlisted CallService actions, each on a unique
    // entity so we can assert order via the target field.
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
        let store = LiveStore::new();
        let outcome = dispatcher
            .dispatch(&WidgetId::from("w"), Gesture::Tap, &store, &map)
            .expect("offline enqueue must succeed");
        assert!(matches!(outcome, DispatchOutcome::Queued { .. }));
    }
    assert_eq!(queue.lock().unwrap().len(), 5);
    // No commands sent yet — still offline.
    assert_eq!(drain_commands(&mut cmd_rx, 1).len(), 0);

    // Reconnect: transition state to Live and explicitly flush.
    state_tx.send(ConnectionState::Live).expect("state Live");

    // Production wiring: the reconnect FSM holds a clone of the same
    // `mpsc::Sender<OutboundCommand>` and calls `OfflineQueue::flush(&tx, _)`.
    // Build a fresh recorder so we can observe FIFO order on the flush path.
    let (flush_tx, mut flush_rx) = mpsc::channel::<OutboundCommand>(8);
    let outcome = {
        let mut q = queue.lock().unwrap();
        q.flush(&flush_tx, None)
    };
    assert_eq!(outcome.dispatched, 5);
    assert_eq!(outcome.aged_out, 0);

    let frames = drain_commands(&mut flush_rx, 8);
    assert_eq!(frames.len(), 5, "5 frames observed on the flush channel");
    for (i, cmd) in frames.iter().enumerate() {
        assert_eq!(
            cmd.frame.target,
            Some(json!({ "entity_id": format!("light.entity_{i}") })),
            "FIFO order broken at {i}"
        );
        assert_eq!(cmd.frame.data, Some(json!({ "marker": i })));
    }
}

// ===========================================================================
// SECTION 8 — BackpressureRejected toast event
// ===========================================================================
//
// `locked_decisions.backpressure`: per-entity cap = 4 (default). A 5th
// dispatch returns `Err(BackpressureRejected)` AND emits
// `ToastEvent::BackpressureRejected` on the toast channel. Founder-smoke
// alone is too weak a gate (opencode review 2026-04-28); a toast-channel
// observer is required.

// 22. Per-entity cap = 4: fifth dispatch returns Err AND emits toast event.
#[tokio::test]
async fn backpressure_rejected_at_per_entity_cap_emits_typed_err_and_toast() {
    // Use DiscardConcurrent so each new dispatch lands in a NEW slot —
    // LastWriteWins always cancels prior entries so the per-entity bucket
    // never exceeds 1 there. The cap behaviour under DiscardConcurrent is
    // the load-bearing test; the toast-event observation is the assertion
    // founder-smoke could not provide.
    let timing = ActionTiming {
        action_overlap_strategy: ActionOverlapStrategy::DiscardConcurrent,
        ..ActionTiming::default()
    };
    let (store, dispatcher, mut cmd_rx, mut toast_rx, map) =
        make_optimistic_fixture_for_toggle(timing, "off");

    // Fill the per-entity bucket to its default cap (4).
    let cap = store.per_entity_optimistic_cap();
    assert_eq!(cap, 4, "default per-entity cap must match locked_decisions");

    for _ in 0..cap {
        let _ = dispatcher
            .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
            .expect("dispatch up to cap must succeed");
    }
    let frames = drain_commands(&mut cmd_rx, 8);
    assert_eq!(frames.len(), cap, "exactly `cap` frames observed");

    // Fifth dispatch must trip the per-entity cap.
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

    // Toast event observable on the toast channel — the load-bearing
    // assertion that founder-smoke alone could not provide.
    let toast = tokio::time::timeout(Duration::from_secs(1), toast_rx.recv())
        .await
        .expect("toast must arrive within 1s")
        .expect("toast channel must yield BackpressureRejected");
    match toast {
        ToastEvent::BackpressureRejected { entity_id, scope } => {
            assert_eq!(entity_id, EntityId::from("light.kitchen"));
            assert_eq!(scope, BackpressureScope::PerEntity);
        }
        other => panic!("expected BackpressureRejected, got {other:?}"),
    }

    // No additional WS frame on the rejected dispatch.
    assert_eq!(
        drain_commands(&mut cmd_rx, 1).len(),
        0,
        "no OutboundCommand on rejected dispatch"
    );
}
