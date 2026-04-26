//! Phase 2 integration tests for `WsClient` against the canonical mock WS server.
//!
//! See `tests/common/mock_ws.rs` for the harness (the single canonical
//! location post TASK-042; re-exposed inside this binary as `super::mock_ws`
//! via the `#[path]` directive in `tests/integration/mod.rs`).  This file
//! contains the TASK-035 scenario tests.  All tests run inside `cargo test`;
//! no external HA instance is required.
//!
//! # Test isolation
//!
//! Each test serializes against [`ENV_LOCK`] before mutating `HA_URL` /
//! `HA_TOKEN`.  We avoid `tokio::test(flavor = "multi_thread")` — single-thread
//! is enough and keeps env-var races out.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tokio::sync::watch;

use hanui::ha::client::{ClientError, WsClient};
use hanui::ha::live_store::LiveStore;
use hanui::ha::store::EntityStore;
use hanui::platform::config::Config;
use hanui::platform::status::{self, ConnectionState};

use super::mock_ws::{entity_state_json, state_changed_event_json, MockWsServer, ScriptedReply};

// ---------------------------------------------------------------------------
// Env serialization
// ---------------------------------------------------------------------------

/// All tests in this binary that mutate `HA_URL` / `HA_TOKEN` MUST take this
/// lock before mutating.  Held for the duration of `Config::from_env()` only;
/// dropped before the test's first `.await` to satisfy `clippy::await_holding_lock`.
static ENV_LOCK: StdMutex<()> = StdMutex::new(());

fn make_config(url: &str, token: &str) -> Config {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("HA_URL", url);
        std::env::set_var("HA_TOKEN", token);
    }
    Config::from_env().expect("test config: env-driven Config::from_env")
}

/// Spawn a `WsClient::run()` task with the given store and return:
/// - the `state_rx` watch receiver,
/// - the `JoinHandle` for the run task (so the test can `.abort()` or await
///   the completion error).
fn spawn_client(
    config: Config,
    store: Option<Arc<dyn hanui::ha::client::SnapshotApplier>>,
) -> (
    watch::Receiver<ConnectionState>,
    tokio::task::JoinHandle<Result<(), ClientError>>,
) {
    let (state_tx, state_rx) = status::channel();
    let mut client = WsClient::new(config, state_tx);
    if let Some(s) = store {
        client = client.with_store(s);
    }
    let handle = tokio::spawn(async move { client.run().await });
    (state_rx, handle)
}

/// Wait until the given `ConnectionState` is observed on the receiver, or the
/// timeout elapses.  Returns true on success.
async fn wait_for_state(
    rx: &mut watch::Receiver<ConnectionState>,
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

// ---------------------------------------------------------------------------
// Scenario: auth_ok happy path → reaches Live
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_auth_ok_reaches_live() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server.script_get_services_reply("{}").await;

    let config = make_config(&server.ws_url, "tok-happy");
    let (mut state_rx, _handle) = spawn_client(config, None);

    assert!(
        wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await,
        "client must reach Live state through full happy-path handshake"
    );

    assert!(server.recorded_request_count("auth").await >= 1);
    assert!(server.recorded_request_count("subscribe_events").await >= 1);
    assert!(server.recorded_request_count("get_states").await >= 1);
    assert!(server.recorded_request_count("get_services").await >= 1);

    // The recorded auth frame must carry the configured token in the
    // access_token field (mock-side echo verification — confirms the secret
    // exposure path actually serialised into the WS frame).
    let auth_frame = server
        .recorded_requests()
        .await
        .into_iter()
        .find(|r| r.kind == "auth")
        .expect("auth frame must be recorded");
    let auth_v: serde_json::Value =
        serde_json::from_str(&auth_frame.body).expect("auth frame must be valid JSON");
    assert_eq!(auth_v["access_token"], "tok-happy");
}

// ---------------------------------------------------------------------------
// Scenario: auth_invalid → Failed, no reconnect attempts within 60 s
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_auth_invalid_transitions_to_failed_no_reconnect() {
    let server = MockWsServer::start().await;
    server.script_auth_invalid("Invalid access token").await;

    let config = make_config(&server.ws_url, "tok-invalid");
    let (mut state_rx, handle) = spawn_client(config, None);

    assert!(
        wait_for_state(
            &mut state_rx,
            ConnectionState::Failed,
            Duration::from_secs(5)
        )
        .await,
        "auth_invalid must transition to Failed"
    );

    let result = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("client task must finish quickly on auth_invalid")
        .expect("task join must succeed");
    assert!(
        matches!(result, Err(ClientError::AuthInvalid { .. })),
        "expected ClientError::AuthInvalid; got: {result:?}"
    );

    // No reconnect: only one auth request was recorded.  Wait briefly to
    // give a hypothetical retry time to fire.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        server.recorded_request_count("auth").await,
        1,
        "auth_invalid must NOT trigger reconnect"
    );
}

// ---------------------------------------------------------------------------
// Scenario: subscribe-ACK before snapshot ordering — TASK-029 sequencing gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_subscribe_ack_before_snapshot_ordering() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server.script_get_services_reply("{}").await;

    let config = make_config(&server.ws_url, "tok-order");
    let (mut state_rx, _handle) = spawn_client(config, None);

    assert!(wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await);

    let recorded = server.recorded_requests().await;

    let sub_frame = recorded
        .iter()
        .find(|r| r.kind == "subscribe_events")
        .expect("subscribe_events must be recorded");
    let snap_frame = recorded
        .iter()
        .find(|r| r.kind == "get_states")
        .expect("get_states must be recorded");

    // (Coarser, original assertion — kept as a sanity check.)  get_states
    // arrives strictly AFTER subscribe_events at the mock — and because the
    // mock sends the ACK only on receipt of subscribe_events, the client
    // must have processed the ACK before sending get_states.  This is the
    // TASK-029 sequencing gate AC.
    let sub_seq = sub_frame.seq;
    let snap_seq = snap_frame.seq;
    assert!(
        snap_seq > sub_seq,
        "get_states (seq {snap_seq}) must arrive after subscribe_events (seq {sub_seq})"
    );

    // (Tighter, TASK-046 finding 6 assertion.)  Codex's audit observed that
    // the seq-based check above would still pass if the client sent
    // `get_states` optimistically before the ACK had physically left the
    // mock, as long as the mock happened to record the two inbound frames
    // in the canonical order.  The real invariant is: the FSM gates
    // `get_states` on actual ACK arrival.
    //
    // The mock now records the wall-clock instant at which it finished
    // sending the `subscribe_events` ACK reply (see
    // `tests/common/mock_ws.rs::SharedState::subscribe_ack_sent_at`).
    // Asserting `get_states_received_at > subscribe_ack_sent_at` proves
    // the client could not have sent `get_states` before the ACK had
    // left the mock — i.e. the ACK gate is real, not optimistic.
    let ack_sent_at = server
        .subscribe_ack_sent_at()
        .await
        .expect("mock must have recorded a subscribe_ack send by the time Live is reached");
    assert!(
        snap_frame.received_at > ack_sent_at,
        "get_states received_at ({:?}) must be strictly AFTER subscribe_events ACK \
         sent_at ({:?}); FSM ACK gate is not real",
        snap_frame.received_at,
        ack_sent_at,
    );
}

// ---------------------------------------------------------------------------
// Scenario: get_services round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_get_services_round_trip() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server
        .script_get_services_reply(r#"{"light":{"turn_on":{"name":"Turn on"}}}"#)
        .await;

    let config = make_config(&server.ws_url, "tok-services");
    let (mut state_rx, _handle) = spawn_client(config, None);

    assert!(wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await);

    assert!(
        server.recorded_request_count("get_services").await >= 1,
        "get_services must be issued during Phase 2 connect"
    );
}

// ---------------------------------------------------------------------------
// Scenario: malformed JSON — no panic; FSM stays alive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_malformed_json_does_not_panic() {
    let server = MockWsServer::start().await;
    // Inject a malformed-JSON frame BEFORE the auth_required exchange.
    server
        .push_reply(ScriptedReply::Immediate("not valid json {{".to_owned()))
        .await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server.script_get_services_reply("{}").await;

    let config = make_config(&server.ws_url, "tok-malformed");
    let (mut state_rx, _handle) = spawn_client(config, None);

    // Despite the malformed leading frame, the client must still reach Live.
    assert!(
        wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await,
        "malformed JSON must be skipped without panic"
    );
}

// ---------------------------------------------------------------------------
// Scenario: live state_changed event routes into LiveStore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_live_event_routes_into_live_store() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    let states = format!(
        "[{}]",
        entity_state_json(
            "light.x",
            "off",
            "2024-01-01T00:00:00+00:00",
            "2024-01-01T00:00:00+00:00",
        )
    );
    server.script_get_states_reply(&states).await;
    server.script_get_services_reply("{}").await;

    let store = Arc::new(LiveStore::new());
    let config = make_config(&server.ws_url, "tok-live-event");
    let (mut state_rx, _handle) = spawn_client(config, Some(store.clone()));

    assert!(wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await);

    let entity = store
        .get(&hanui::ha::entity::EntityId::from("light.x"))
        .expect("light.x must be present after snapshot apply");
    assert_eq!(&*entity.state, "off");

    server
        .inject_event(state_changed_event_json(
            1,
            "light.x",
            Some((
                "on",
                "2024-01-01T01:00:00+00:00",
                "2024-01-01T01:00:00+00:00",
            )),
            Some((
                "off",
                "2024-01-01T00:00:00+00:00",
                "2024-01-01T00:00:00+00:00",
            )),
        ))
        .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let e = store
            .get(&hanui::ha::entity::EntityId::from("light.x"))
            .expect("entity must still be present");
        if &*e.state == "on" {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "expected light.x state to flip to 'on' within 2s; still: {}",
                &*e.state
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// Scenario: entity-removal — `entity: None` in EntityUpdate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_entity_removal_carries_none_and_drops_from_store() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    let states = format!(
        "[{}]",
        entity_state_json(
            "light.gone",
            "on",
            "2024-01-01T00:00:00+00:00",
            "2024-01-01T00:00:00+00:00",
        )
    );
    server.script_get_states_reply(&states).await;
    server.script_get_services_reply("{}").await;

    let store = Arc::new(LiveStore::new());
    let config = make_config(&server.ws_url, "tok-removal");
    let (mut state_rx, _handle) = spawn_client(config, Some(store.clone()));

    assert!(wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await);

    assert!(store
        .get(&hanui::ha::entity::EntityId::from("light.gone"))
        .is_some());

    let mut rx = store.subscribe(&[hanui::ha::entity::EntityId::from("light.gone")]);

    server
        .inject_event(state_changed_event_json(
            1,
            "light.gone",
            None,
            Some((
                "on",
                "2024-01-01T00:00:00+00:00",
                "2024-01-01T00:00:00+00:00",
            )),
        ))
        .await;

    let update = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("removal broadcast must arrive within 2s")
        .expect("removal broadcast must be Ok");
    assert_eq!(update.id.as_str(), "light.gone");
    assert!(
        update.entity.is_none(),
        "removal must carry entity: None; got: {:?}",
        update.entity
    );

    assert!(
        store
            .get(&hanui::ha::entity::EntityId::from("light.gone"))
            .is_none(),
        "light.gone must be absent from store after removal event"
    );
}

// ---------------------------------------------------------------------------
// Scenario: id-correlation — out-of-order replies resolve correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_id_correlation_out_of_order_replies() {
    let config = make_config("ws://127.0.0.1:1/api/websocket", "tok-corr");
    let (state_tx, _state_rx) = status::channel();
    let mut client = WsClient::new(config, state_tx);

    let rx10 = client.register_pending(10);
    let rx20 = client.register_pending(20);
    let rx30 = client.register_pending(30);

    client
        .resolve_pending(30, Ok(serde_json::json!("r30")))
        .unwrap();
    client
        .resolve_pending(10, Ok(serde_json::json!("r10")))
        .unwrap();
    client
        .resolve_pending(20, Ok(serde_json::json!("r20")))
        .unwrap();

    assert_eq!(rx10.await.unwrap().unwrap(), serde_json::json!("r10"));
    assert_eq!(rx20.await.unwrap().unwrap(), serde_json::json!("r20"));
    assert_eq!(rx30.await.unwrap().unwrap(), serde_json::json!("r30"));

    let r = client.resolve_pending(999, Ok(serde_json::Value::Null));
    assert!(
        matches!(r, Err(ClientError::IdMismatch { received: 999 })),
        "expected IdMismatch; got: {r:?}"
    );

    // No-reply within timeout: drop the client; the orphan oneshot's sender
    // is dropped, so the receiver yields a recv error.
    let rx_orphan = client.register_pending(500);
    drop(client);
    let r = tokio::time::timeout(Duration::from_millis(200), rx_orphan).await;
    let inner = r.expect("recv must not stall after client drop");
    assert!(
        inner.is_err(),
        "orphan oneshot must surface as a recv error after client drop"
    );
}

// ---------------------------------------------------------------------------
// Scenario: status banner via ConnectionState — flips on disconnect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_status_banner_visibility_via_connection_state() {
    use hanui::ui::bridge::is_writes_gated;

    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server.script_get_services_reply("{}").await;

    let config = make_config(&server.ws_url, "tok-banner");
    let (mut state_rx, handle) = spawn_client(config, None);

    assert!(wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await);
    assert!(
        !is_writes_gated(*state_rx.borrow()),
        "banner must be hidden in Live state"
    );

    // Trigger mid-session auth_required → Reconnecting transition.
    server.inject_auth_required().await;

    assert!(
        wait_for_state(
            &mut state_rx,
            ConnectionState::Reconnecting,
            Duration::from_secs(5),
        )
        .await,
        "client must transition to Reconnecting after mid-session auth_required"
    );

    assert!(
        is_writes_gated(*state_rx.borrow()),
        "banner must be visible in Reconnecting state"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// Scenario: mid-session auth_required → Reconnecting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_mid_session_auth_required_triggers_reconnecting() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server.script_get_services_reply("{}").await;

    let config = make_config(&server.ws_url, "tok-mid-auth");
    let (mut state_rx, handle) = spawn_client(config, None);

    assert!(wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await);

    server.inject_auth_required().await;

    assert!(
        wait_for_state(
            &mut state_rx,
            ConnectionState::Reconnecting,
            Duration::from_secs(5),
        )
        .await,
        "mid-session auth_required must transition to Reconnecting"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// Scenario: reconnect resync — diff-broadcast only changed entities
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_reconnect_resync_only_broadcasts_changed_entities() {
    // First connect populates the store; second connect with a different
    // snapshot must broadcast only the changed entity.
    let server1 = MockWsServer::start().await;
    server1.script_auth_ok().await;
    server1.script_subscribe_ack().await;
    let states1 = format!(
        "[{},{}]",
        entity_state_json(
            "light.a",
            "on",
            "2024-01-01T00:00:00+00:00",
            "2024-01-01T00:00:00+00:00",
        ),
        entity_state_json(
            "light.b",
            "off",
            "2024-01-01T00:00:00+00:00",
            "2024-01-01T00:00:00+00:00",
        ),
    );
    server1.script_get_states_reply(&states1).await;
    server1.script_get_services_reply("{}").await;

    let store = Arc::new(LiveStore::new());

    let config1 = make_config(&server1.ws_url, "tok-resync-1");
    let (state_tx1, mut state_rx1) = status::channel();
    let client1 = WsClient::new(config1, state_tx1).with_store(store.clone());
    let handle1 = tokio::spawn(async move {
        let mut c = client1;
        c.run().await
    });

    assert!(
        wait_for_state(
            &mut state_rx1,
            ConnectionState::Live,
            Duration::from_secs(5)
        )
        .await
    );
    handle1.abort();

    assert!(store
        .get(&hanui::ha::entity::EntityId::from("light.a"))
        .is_some());
    assert!(store
        .get(&hanui::ha::entity::EntityId::from("light.b"))
        .is_some());

    // Subscribe before reconnect.
    let mut rx_a = store.subscribe(&[hanui::ha::entity::EntityId::from("light.a")]);
    let mut rx_b = store.subscribe(&[hanui::ha::entity::EntityId::from("light.b")]);

    let server2 = MockWsServer::start().await;
    server2.script_auth_ok().await;
    server2.script_subscribe_ack().await;
    let states2 = format!(
        "[{},{}]",
        // Same last_updated → not diffed.
        entity_state_json(
            "light.a",
            "on",
            "2024-01-01T00:00:00+00:00",
            "2024-01-01T00:00:00+00:00",
        ),
        // Changed last_updated → must broadcast.
        entity_state_json(
            "light.b",
            "on",
            "2024-01-02T00:00:00+00:00",
            "2024-01-02T00:00:00+00:00",
        ),
    );
    server2.script_get_states_reply(&states2).await;
    server2.script_get_services_reply("{}").await;

    let config2 = make_config(&server2.ws_url, "tok-resync-2");
    let (state_tx2, mut state_rx2) = status::channel();
    let client2 = WsClient::new(config2, state_tx2).with_store(store.clone());
    let handle2 = tokio::spawn(async move {
        let mut c = client2;
        c.run().await
    });

    assert!(
        wait_for_state(
            &mut state_rx2,
            ConnectionState::Live,
            Duration::from_secs(5)
        )
        .await,
        "second connect must reach Live"
    );

    let b_update = tokio::time::timeout(Duration::from_secs(2), rx_b.recv())
        .await
        .expect("light.b broadcast must arrive within 2s")
        .expect("light.b broadcast must be Ok");
    assert_eq!(b_update.id.as_str(), "light.b");
    assert!(b_update.entity.is_some());

    // light.a must not have any broadcast within a short window.
    let a_result = tokio::time::timeout(Duration::from_millis(300), rx_a.recv()).await;
    assert!(
        a_result.is_err(),
        "light.a must NOT be broadcast on resync (last_updated unchanged); got: {a_result:?}"
    );

    handle2.abort();
}

// ---------------------------------------------------------------------------
// Scenario: oversized payload — connection drops (run() exits with transport err)
//
// The acceptance criterion requires that a frame above
// `DEFAULT_PROFILE.ws_payload_cap` (16 MiB) terminates the WS connection so
// the outer reconnect loop (TASK-032) can full-resync.  The inner `run()`
// signals this via an `Err(ClientError::Transport(_))` return — it does NOT
// re-publish `ConnectionState` because the outer reconnect loop owns that
// transition.  We therefore assert on the run() task termination, not on the
// state watch channel.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_oversized_payload_drops_connection() {
    use hanui::dashboard::profiles::DEFAULT_PROFILE;

    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server.script_get_services_reply("{}").await;

    let config = make_config(&server.ws_url, "tok-oversized");
    let (mut state_rx, handle) = spawn_client(config, None);

    assert!(wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await);

    // Inject a frame just above the cap; the WS layer (mock or client) rejects
    // it, terminating the connection.
    let cap = DEFAULT_PROFILE.ws_payload_cap;
    let oversized = format!(
        r#"{{"type":"event","id":1,"_pad":"{}"}}"#,
        "x".repeat(cap + 1)
    );
    server.inject_event(oversized).await;

    // Wait for the run() task to terminate.
    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("run() must terminate after oversized frame")
        .expect("task join must succeed");

    // The terminated result must be a transport error (oversized frame
    // surfaces as a Tungstenite/IO error wrapped in ClientError::Transport).
    assert!(
        matches!(result, Err(ClientError::Transport(_))),
        "oversized frame must terminate run() with a transport error; got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario: consecutive-overflow circuit-breaker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_consecutive_overflow_circuit_breaker_trips() {
    // Drives the circuit-breaker via the public OverflowBreaker API.  The
    // FSM-level Failed transition for this exact case is covered by the
    // unit test in `src/ha/client.rs::tests::test_three_overflows_trip_circuit_breaker`.
    let config = make_config("ws://127.0.0.1:1/api/websocket", "tok-cb");
    let (state_tx, _state_rx) = status::channel();
    let mut client = WsClient::new(config, state_tx);

    assert!(!client.overflow_breaker.record_overflow());
    assert!(!client.overflow_breaker.record_overflow());
    let tripped = client.overflow_breaker.record_overflow();
    assert!(
        tripped,
        "third consecutive overflow within 60s must trip the circuit-breaker"
    );
}

// ---------------------------------------------------------------------------
// BLOCKER 1 (TASK-044) verification: production wiring routes events to store.
//
// Codex's post-shipment audit found that `src/lib.rs::run_with_live_store`
// constructed a `LiveStore` and a `WsClient` independently and never wired
// them together via `WsClient::with_store(...)`.  The Phase 2 live HA path
// would parse `get_states` / `state_changed` then drop everything on the
// floor, leaving the UI rendering "unavailable" forever.
//
// This test reproduces the same wiring `run_with_live_store` uses (build a
// shared `Arc<LiveStore>`, hand the same Arc to the WS client and to the test
// observer) and asserts that a mid-Live `state_changed` event mutates the
// store the test holds.  Pre-fix, this assertion would fail because the WS
// task held no store reference.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_run_with_live_store_routes_events_into_store() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    let states = format!(
        "[{}]",
        entity_state_json(
            "light.kitchen",
            "off",
            "2024-01-01T00:00:00+00:00",
            "2024-01-01T00:00:00+00:00",
        )
    );
    server.script_get_states_reply(&states).await;
    server.script_get_services_reply("{}").await;

    // Replicate run_with_live_store wiring: a single Arc<LiveStore>, used both
    // as the SnapshotApplier sink for the WS task AND as the read handle the
    // (test stand-in for the) bridge consults.  The Arc clone is what makes
    // BLOCKER 1's fix observable — without with_store(), the writes never land.
    let store: Arc<LiveStore> = Arc::new(LiveStore::new());
    let store_for_ws: Arc<dyn hanui::ha::client::SnapshotApplier> = store.clone();

    let config = make_config(&server.ws_url, "tok-blocker1-wiring");
    let (mut state_rx, _handle) = spawn_client(config, Some(store_for_ws));

    assert!(
        wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await,
        "client must reach Live before the snapshot can be observed"
    );

    // After Live the snapshot must be visible to the read-side `store` handle
    // — proving the same Arc backs both endpoints (BLOCKER 1's invariant).
    let initial = store
        .get(&hanui::ha::entity::EntityId::from("light.kitchen"))
        .expect("snapshot apply via WS must populate the read-side store");
    assert_eq!(
        &*initial.state, "off",
        "snapshot value must surface through the shared Arc"
    );

    // Fire a mid-Live state_changed event; it must mutate the same store.
    server
        .inject_event(state_changed_event_json(
            1,
            "light.kitchen",
            Some((
                "on",
                "2024-01-01T01:00:00+00:00",
                "2024-01-01T01:00:00+00:00",
            )),
            Some((
                "off",
                "2024-01-01T00:00:00+00:00",
                "2024-01-01T00:00:00+00:00",
            )),
        ))
        .await;

    // Spin briefly until the read-side store reflects the new value.  Bound
    // the wait so a regression of BLOCKER 1 (events dropped) fails fast.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let e = store
            .get(&hanui::ha::entity::EntityId::from("light.kitchen"))
            .expect("entity must remain present through the event");
        if &*e.state == "on" {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "BLOCKER 1 regression: state_changed did not flow into the shared LiveStore \
                 within 2s — WsClient::with_store wiring is missing.  Last seen state: {}",
                &*e.state
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
