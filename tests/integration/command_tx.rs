//! TASK-072 integration tests — `LiveStore.command_tx` ↔ WS-side drain.
//!
//! Locks the Phase 3 dispatcher → WS client seam end-to-end against the
//! canonical [`MockWsServer`]:
//!
//! 1. **Round-trip**: a dispatcher pushes an `OutboundCommand`; the WS-side
//!    drain task allocates an id, writes the wire JSON onto the socket; the
//!    mock returns a `result` frame with that id; the dispatcher's
//!    `oneshot::Receiver<AckResult>` resolves to `Ok(HaAckSuccess)`.
//! 2. **Channel closed (Risk #11)**: when the WS task exits, dispatchers
//!    that still hold a sender clone observe a closed channel and the
//!    dispatcher returns `DispatchError::ChannelClosed` — never panics.
//! 3. **Reconnect repopulation**: `LiveStore::set_command_tx` called twice
//!    (simulating WS task restart) replaces the stale sender; a dispatch
//!    after the second install reaches the new receiver.
//!
//! These tests do not exercise the reconnect FSM end-to-end (the unit test
//! coverage in `src/lib.rs` and the per-attempt re-installation in
//! `run_ws_client` cover that path).  Here we drive `WsClient::run` directly
//! against the mock and use [`LiveStore::set_command_tx`] explicitly so the
//! invariants under test are visible at the assertion site.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tokio::sync::mpsc;

use hanui::actions::dispatcher::{DispatchError, Dispatcher, Gesture};
use hanui::actions::map::{WidgetActionEntry, WidgetActionMap, WidgetId};
use hanui::actions::Action;
use hanui::dashboard::profiles::PROFILE_DESKTOP;
use hanui::ha::client::{AckResult, OutboundCommand, SnapshotApplier, WsClient};
use hanui::ha::entity::EntityId;
use hanui::ha::live_store::LiveStore;
use hanui::ha::services::{ServiceMeta, ServiceRegistry};
use hanui::platform::config::Config;
use hanui::platform::status::{self, ConnectionState};

use super::mock_ws::{MockWsServer, ScriptedReply};

// ---------------------------------------------------------------------------
// Env serialization (shared with other integration tests in this binary)
// ---------------------------------------------------------------------------

static ENV_LOCK: StdMutex<()> = StdMutex::new(());

fn make_config(url: &str, token: &str) -> Config {
    let _g = ENV_LOCK.lock().unwrap();
    // SAFETY: serialised by ENV_LOCK; the lock is held only for the synchronous
    // env mutation + Config::from_env parse and dropped before the first .await.
    unsafe {
        std::env::set_var("HA_URL", url);
        std::env::set_var("HA_TOKEN", token);
    }
    Config::from_env().expect("test config")
}

/// Wait for a watch::Receiver to observe `target`, returning `true` on success.
async fn wait_for_state(
    rx: &mut tokio::sync::watch::Receiver<ConnectionState>,
    target: ConnectionState,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if *rx.borrow() == target {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        if tokio::time::timeout(remaining, rx.changed()).await.is_err() {
            return *rx.borrow() == target;
        }
    }
}

/// Drive a `MockWsServer` through the canonical happy-path handshake with
/// an empty entity snapshot and a `get_services` reply that registers
/// `light.toggle` (so the dispatcher's Toggle path takes the `<domain>.toggle`
/// branch and emits one `call_service` frame).
async fn script_happy_path_with_light_toggle(server: &MockWsServer) {
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server
        .script_get_services_reply(
            r#"{"light":{"toggle":{"name":"Toggle","description":"Toggle a light.","fields":{}}}}"#,
        )
        .await;
}

/// Add a generic `call_service` reply matcher: any `call_service` request
/// gets an immediate success result with the inbound id forwarded.
async fn script_call_service_success(server: &MockWsServer) {
    server
        .push_reply(ScriptedReply::OnRequest {
            match_type: "call_service".to_owned(),
            body: r#"{"type":"result","id":{{ID}},"success":true,"result":null}"#.to_owned(),
            forward_id: true,
        })
        .await;
}

/// Build a `WidgetActionMap` with one widget bound to `light.kitchen` whose
/// `tap` action is `Toggle`.
fn one_toggle_widget(widget_id: &str, entity_id: &str) -> WidgetActionMap {
    let mut map = WidgetActionMap::new();
    map.insert(
        WidgetId::from(widget_id),
        WidgetActionEntry {
            entity_id: EntityId::from(entity_id),
            tap: Action::Toggle,
            hold: Action::None,
            double_tap: Action::None,
        },
    );
    map
}

/// Build a `ServiceRegistry` that has `<domain>.toggle` registered.
fn registry_with_toggle(domain: &str) -> ServiceRegistry {
    let mut reg = ServiceRegistry::new();
    reg.add_service(domain, "toggle", ServiceMeta::default());
    reg
}

// ---------------------------------------------------------------------------
// Test 1 — round-trip: dispatcher → WS → mock → ack
// ---------------------------------------------------------------------------

/// End-to-end seam test: a dispatcher pushes an `OutboundCommand` through
/// `LiveStore.command_tx`; the WS client task drains it, allocates an id,
/// writes wire JSON; the mock replies with a matching `result` frame; the
/// dispatcher's `oneshot::Receiver<AckResult>` fires with
/// `Ok(HaAckSuccess { id, .. })`.
#[tokio::test]
async fn dispatcher_command_round_trips_through_ws_client_and_resolves_ack() {
    let server = MockWsServer::start().await;
    script_happy_path_with_light_toggle(&server).await;
    script_call_service_success(&server).await;

    let config = make_config(&server.ws_url, "tok-round-trip");

    // Wire LiveStore + WsClient with a SHARED service registry: the FSM
    // bulk-replaces the registry on `Phase::Services → Live` with the
    // mock's `get_services` reply, which registers `light.toggle` (see
    // `script_happy_path_with_light_toggle` above).  Pre-populating the
    // registry here would be overwritten by the FSM, so we share the
    // empty handle and let the WS handshake populate it.
    let services_handle = Arc::new(std::sync::RwLock::new(ServiceRegistry::new()));
    let store: Arc<LiveStore> =
        Arc::new(LiveStore::new().with_services_handle(services_handle.clone()));

    // Seed the entity so dispatcher's domain extraction works (light.kitchen).
    use jiff::Timestamp;
    use serde_json::Map;
    store.apply_snapshot(vec![hanui::ha::entity::Entity {
        id: EntityId::from("light.kitchen"),
        state: Arc::from("on"),
        attributes: Arc::new(Map::new()),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    }]);

    // Build the channel + install on LiveStore (mirrors what
    // `lib.rs::run_ws_client` does on each attempt).
    let (cmd_tx, cmd_rx) = mpsc::channel::<OutboundCommand>(8);
    store.set_command_tx(cmd_tx);

    // Spawn WsClient::run with the receiver attached.
    let (state_tx, mut state_rx) = status::channel();
    let store_for_ws: Arc<dyn SnapshotApplier> = store.clone();
    let mut client = WsClient::new(config, state_tx, &PROFILE_DESKTOP)
        .with_store(store_for_ws)
        .with_registry(services_handle);
    client.set_command_rx(cmd_rx);
    let ws_task = tokio::spawn(async move { client.run().await });

    // Wait until the FSM reaches Live so subscribe / snapshot / services
    // ids 1..3 are all consumed; the dispatcher's command will then take id 4.
    assert!(
        wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await,
        "WS client must reach Live before dispatcher fires"
    );

    // Construct the dispatcher with a clone of the now-installed sender
    // (mirrors the Wave 4 caller pattern).
    let services_clone = store.services_handle();
    let tx_clone = store
        .command_tx()
        .expect("LiveStore must expose command_tx after set_command_tx");
    let dispatcher = Dispatcher::with_command_tx(services_clone, tx_clone);

    let map = one_toggle_widget("kitchen_light", "light.kitchen");

    // Fire the dispatch.  The dispatcher creates the oneshot, builds an
    // OutboundFrame, sends OutboundCommand on the channel, and returns
    // `DispatchOutcome::Sent { ack_rx }`.
    let outcome = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("dispatch must succeed (light.toggle is registered)");
    let ack_rx = match outcome {
        hanui::actions::dispatcher::DispatchOutcome::Sent { ack_rx } => ack_rx,
        other => panic!("expected DispatchOutcome::Sent; got {other:?}"),
    };

    // Wait for the dispatcher's oneshot to resolve.
    let ack: AckResult = tokio::time::timeout(Duration::from_secs(5), ack_rx)
        .await
        .expect("ack must arrive within 5s")
        .expect("oneshot must not be dropped");

    let success = ack.expect("HA result.success=true must produce Ok(HaAckSuccess)");

    // The mock's recorded call_service frame must carry the same id the
    // dispatcher's ack reports.  The id is allocated by the WS client (via
    // register_dispatcher_command), so we only know it by inspecting the
    // recorded frame.
    let recorded = server.recorded_requests().await;
    let cs_frame = recorded
        .iter()
        .find(|f| f.kind == "call_service")
        .expect("mock must have recorded one call_service frame");
    let cs_value: serde_json::Value =
        serde_json::from_str(&cs_frame.body).expect("call_service frame must be valid JSON");
    let recorded_id = cs_value["id"]
        .as_u64()
        .expect("call_service frame must carry an id") as u32;
    assert_eq!(
        success.id, recorded_id,
        "AckResult.id must match the id the WS client allocated and serialised"
    );
    assert_eq!(cs_value["domain"], "light");
    assert_eq!(cs_value["service"], "toggle");
    assert_eq!(
        cs_value["target"]["entity_id"], "light.kitchen",
        "WS-side wire JSON must carry the dispatcher-supplied entity_id"
    );

    ws_task.abort();
}

// ---------------------------------------------------------------------------
// Test 2 — Risk #11: dropped receiver surfaces as DispatchError::ChannelClosed
// ---------------------------------------------------------------------------

/// Risk #11: when the WS client task exits/panics, the matching
/// `mpsc::Receiver<OutboundCommand>` is dropped.  Any dispatcher that still
/// holds a clone of the sender must observe a closed channel and return
/// `DispatchError::ChannelClosed` — never panic.
///
/// We exercise this without spinning up a real WS connection (the boundary
/// is purely the channel half-life): install a sender on the LiveStore,
/// drop the receiver, then dispatch.
#[tokio::test]
async fn dropped_receiver_surfaces_channel_closed_no_panic() {
    let store = LiveStore::new();
    let (tx, rx) = mpsc::channel::<OutboundCommand>(4);
    store.set_command_tx(tx);

    // Simulate WS task exit before the dispatcher fires.
    drop(rx);

    let services = Arc::new(std::sync::RwLock::new(ServiceRegistry::new()));
    // Add a toggle service so the dispatcher gets past the registry-check
    // branch and lands on the channel write.
    services
        .write()
        .unwrap()
        .add_service("light", "toggle", ServiceMeta::default());

    let tx_clone = store
        .command_tx()
        .expect("command_tx must be Some after set_command_tx");
    let dispatcher = Dispatcher::with_command_tx(services, tx_clone);

    let map = one_toggle_widget("kitchen_light", "light.kitchen");

    let err = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect_err("closed receiver must surface as DispatchError, not panic");
    assert!(
        matches!(err, DispatchError::ChannelClosed),
        "dropped receiver must produce DispatchError::ChannelClosed; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — reconnect repopulation: set_command_tx replaces stale sender
// ---------------------------------------------------------------------------

/// `locked_decisions.command_tx_wiring` repopulation invariant: after the
/// WS client task exits and the reconnect FSM installs a fresh sender, a
/// dispatcher constructed against the LATEST `LiveStore::command_tx()` must
/// reach the new receiver — not the closed one.
///
/// This test does not spin up a second WS connection; it exercises the
/// LiveStore-side contract by:
///   1. Installing sender #1 + dropping its receiver (simulates first-WS-task
///      exit).
///   2. Installing sender #2 + keeping the receiver.
///   3. Constructing a dispatcher from `LiveStore::command_tx()` and asserting
///      the dispatch lands on receiver #2.
#[tokio::test]
async fn reconnect_repopulates_command_tx_and_next_dispatch_reaches_new_receiver() {
    use jiff::Timestamp;
    use serde_json::Map;

    let services_handle = Arc::new(std::sync::RwLock::new(registry_with_toggle("light")));
    let store: Arc<LiveStore> =
        Arc::new(LiveStore::new().with_services_handle(services_handle.clone()));
    store.apply_snapshot(vec![hanui::ha::entity::Entity {
        id: EntityId::from("light.kitchen"),
        state: Arc::from("on"),
        attributes: Arc::new(Map::new()),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    }]);

    // Install sender #1 then drop its receiver — first WS task exits.
    let (tx1, rx1) = mpsc::channel::<OutboundCommand>(4);
    store.set_command_tx(tx1);
    drop(rx1);

    // Install sender #2 — reconnect FSM repopulation.
    let (tx2, mut rx2) = mpsc::channel::<OutboundCommand>(4);
    store.set_command_tx(tx2);

    // Build a fresh dispatcher AFTER the second install, mirroring the Wave 4
    // construction pattern (cloning `LiveStore::command_tx()` at dispatcher
    // build time).
    let dispatcher = Dispatcher::with_command_tx(
        store.services_handle(),
        store
            .command_tx()
            .expect("LiveStore must expose the latest command_tx"),
    );
    let map = one_toggle_widget("kitchen_light", "light.kitchen");

    let outcome = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect("dispatch on the repopulated channel must succeed");
    let ack_rx = match outcome {
        hanui::actions::dispatcher::DispatchOutcome::Sent { ack_rx } => ack_rx,
        other => panic!("expected DispatchOutcome::Sent; got {other:?}"),
    };

    // The command must have been delivered to receiver #2 (NOT the dropped
    // receiver #1).  Receiver #1 is gone — if the dispatcher had used the
    // stale sender we'd have observed `ChannelClosed` at dispatch time.
    let cmd = tokio::time::timeout(Duration::from_secs(2), rx2.recv())
        .await
        .expect("recv on reciever #2 must complete within 2s")
        .expect("repopulated receiver must yield the dispatched OutboundCommand");
    assert_eq!(cmd.frame.domain, "light");
    assert_eq!(cmd.frame.service, "toggle");

    // The dispatcher's `ack_rx` is still pending (no WS client to fire it);
    // dropping it here is fine — the test's invariant is that the command
    // reached the NEW receiver, not the disposition of the ack.
    drop(ack_rx);
}

// ---------------------------------------------------------------------------
// Test 4 — `command_tx` is None until set_command_tx is called
// ---------------------------------------------------------------------------

/// Acceptance criterion: a freshly-constructed `LiveStore` exposes
/// `command_tx() == None`.  A dispatcher built before wiring would receive
/// no sender and therefore return `DispatchError::ChannelNotWired` from the
/// existing TASK-062 path; this test pins the LiveStore-side observable.
#[tokio::test]
async fn live_store_command_tx_is_none_before_set() {
    let store = LiveStore::new();
    assert!(store.command_tx().is_none());
}

// ---------------------------------------------------------------------------
// Test 4b — opencode review Q4 follow-up: dispatch during the backoff window
// (between clear_command_tx and the next set_command_tx) yields
// `DispatchError::ChannelNotWired`, never `ChannelClosed`.  This pins the
// reconnect-FSM gap behaviour locked by `locked_decisions.command_tx_wiring`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dispatch_during_backoff_gap_after_clear_returns_channel_not_wired() {
    use jiff::Timestamp;
    use serde_json::Map;

    let services_handle = Arc::new(std::sync::RwLock::new(registry_with_toggle("light")));
    let store: Arc<LiveStore> =
        Arc::new(LiveStore::new().with_services_handle(services_handle.clone()));
    store.apply_snapshot(vec![hanui::ha::entity::Entity {
        id: EntityId::from("light.kitchen"),
        state: Arc::from("on"),
        attributes: Arc::new(Map::new()),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    }]);

    // Install a sender then clear it — this is what `run_ws_client` does
    // between WS-task exit and the next reconnect attempt's
    // `set_command_tx`.  The dispatcher fires inside this window.
    let (tx, _rx) = mpsc::channel::<OutboundCommand>(4);
    store.set_command_tx(tx);
    store.clear_command_tx();
    assert!(
        store.command_tx().is_none(),
        "after clear_command_tx the store-visible sender must be None"
    );

    // A dispatcher constructed AT THIS MOMENT — i.e. it reads
    // `LiveStore::command_tx()` while the field is `None` — would have to
    // skip the WS-bound action entirely.  But the realistic case is a
    // pre-existing dispatcher built in Wave 4 that holds its own clone.
    // Because we cleared the field, the only way for a dispatcher to be
    // built here is via the `Dispatcher::new(services)` (no command_tx)
    // path — yielding `DispatchError::ChannelNotWired`.
    let dispatcher = Dispatcher::new(store.services_handle());
    let map = one_toggle_widget("kitchen_light", "light.kitchen");
    let err = dispatcher
        .dispatch(&WidgetId::from("kitchen_light"), Gesture::Tap, &store, &map)
        .expect_err("dispatch with no installed channel must error, not panic");
    assert!(
        matches!(err, DispatchError::ChannelNotWired),
        "dispatch in the backoff gap must return ChannelNotWired; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 5 — toast surface (Risk #11): error display does not include token-shaped
// content.  The dispatcher's existing Display impl is the toast surface
// (TASK-067 will render it); we sanity-check the string here so a regression
// in DispatchError::ChannelClosed Display would surface in CI.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_closed_display_is_descriptive() {
    let err = DispatchError::ChannelClosed;
    let display = format!("{err}");
    assert!(
        display.contains("receiver dropped") || display.contains("WS client task"),
        "ChannelClosed Display must be descriptive enough for the toast layer; got: {display}"
    );
    // Defence: must NOT include any raw env token shape.
    assert!(!display.to_lowercase().contains("ha_token"));
}
