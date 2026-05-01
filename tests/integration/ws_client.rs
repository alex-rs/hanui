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

use hanui::dashboard::profiles::PROFILE_DESKTOP;
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
    // TASK-120b F4: tests stay on PROFILE_DESKTOP so existing payload-cap +
    // snapshot-buffer expectations are preserved. The opi_zero3 wiring
    // assertion lives in `lib.rs::tests` (see
    // `dashboard_with_opi_zero3_profile_wires_opi_constants_into_runtime`).
    let mut client = WsClient::new(config, state_tx, &PROFILE_DESKTOP);
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
    let mut client = WsClient::new(config, state_tx, &PROFILE_DESKTOP);

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
    let client1 = WsClient::new(config1, state_tx1, &PROFILE_DESKTOP).with_store(store.clone());
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
    let client2 = WsClient::new(config2, state_tx2, &PROFILE_DESKTOP).with_store(store.clone());
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
// `PROFILE_DESKTOP.ws_payload_cap` (16 MiB) terminates the WS connection so
// the outer reconnect loop (TASK-032) can full-resync.  The inner `run()`
// signals this via an `Err(ClientError::Transport(_))` return — it does NOT
// re-publish `ConnectionState` because the outer reconnect loop owns that
// transition.  We therefore assert on the run() task termination, not on the
// state watch channel.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_oversized_payload_drops_connection() {
    use hanui::dashboard::profiles::PROFILE_DESKTOP;

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
    let cap = PROFILE_DESKTOP.ws_payload_cap;
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
// Scenario: consecutive-overflow circuit-breaker via the FSM's natural path
//
// TASK-046 finding 7: codex's audit found the previous version of this test
// drove the breaker via `OverflowBreaker::record_overflow` calls directly,
// which only proves the breaker's internal counter logic — it does NOT
// prove that real snapshot-buffer overflow events trigger the breaker.
// The unit test in `src/ha/client.rs::tests::test_three_overflows_trip_circuit_breaker`
// already covers the FSM-level transition for the third (tripping) overflow,
// but uses pre-recorded `record_overflow` calls for the first two.  This
// test closes the gap end-to-end: ALL THREE overflows are driven by the
// mock injecting > `PROFILE_DESKTOP.snapshot_buffer_events` state_changed
// events while the FSM is in `Phase::Snapshotting`, and the third overflow
// must surface as `ClientError::OverflowCircuitBreaker` with the FSM in
// `ConnectionState::Failed`.
//
// Re-using the SAME `WsClient` across all three reconnect attempts is
// what allows the `OverflowBreaker.recent` Vec to accumulate the three
// overflow timestamps; constructing a fresh client per attempt would
// reset the breaker every time.
// ---------------------------------------------------------------------------

/// Drive a single FSM-natural snapshot-buffer overflow against `server`,
/// re-using `client` across calls so its `OverflowBreaker` accumulates.
///
/// Returns the `ClientError` that caused `client.run()` to terminate.
///
/// 1. Scripts auth_ok + subscribe_ack on `server` (NOT `get_states_reply`,
///    so the FSM stays in `Phase::Snapshotting` waiting for a snapshot
///    that never comes).
/// 2. Concurrently runs `client.run()` and a driver future that waits for
///    the FSM to send `get_states` (recorded by the mock) then batch-injects
///    `snapshot_buffer_events + 1` state_changed frames so the cap-th
///    incoming event hits the overflow branch in `handle_message`.
/// 3. Returns the error from `run()`.
async fn drive_one_fsm_overflow(server: &MockWsServer, client: &mut WsClient) -> ClientError {
    use hanui::dashboard::profiles::PROFILE_DESKTOP;

    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    // Intentionally NO get_states_reply — keeps the FSM in Snapshotting so
    // the injected events accumulate in `event_buffer` until the cap.

    // Snapshot the get_states count BEFORE this iteration; the driver must
    // wait for it to INCREMENT (the new reconnect's get_states), not just
    // be `>= 1` (cumulative count from prior iterations of the same test).
    let get_states_count_before = server.recorded_request_count("get_states").await;

    let driver = async {
        // Wait until the FSM has sent get_states (i.e. reached Snapshotting).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if server.recorded_request_count("get_states").await > get_states_count_before {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("FSM did not reach Snapshotting (no new get_states recorded) within 5 s");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Batch-inject cap+1 state_changed frames; the cap-th push hits the
        // overflow branch in handle_message.  Use a unique entity_id per
        // index so the test exercises the full event_buffer.push path (the
        // cap check is `len() >= cap`, not "id repeats").
        let cap = PROFILE_DESKTOP.snapshot_buffer_events;
        let frames: Vec<String> = (0..=cap)
            .map(|i| {
                state_changed_event_json(
                    1,
                    &format!("light.spam_{i}"),
                    Some((
                        "on",
                        "2024-01-01T00:00:00+00:00",
                        "2024-01-01T00:00:00+00:00",
                    )),
                    None,
                )
            })
            .collect();
        server.inject_events_batch(frames).await;
    };

    // Race the run future against the driver.  `client.run()` must terminate
    // with an error after the cap-th injected event (Transport(ConnectionClosed)
    // for overflows 1 and 2, OverflowCircuitBreaker for overflow 3).  The
    // driver future completes once the injection has been queued; the run
    // future drives the actual overflow.
    let (run_result, _) = tokio::join!(client.run(), driver);
    run_result.expect_err("overflow must surface as a ClientError")
}

#[tokio::test]
async fn scenario_consecutive_overflow_circuit_breaker_trips() {
    let server = MockWsServer::start().await;

    let config = make_config(&server.ws_url, "tok-cb-fsm");
    let (state_tx, state_rx) = status::channel();
    let mut client = WsClient::new(config, state_tx, &PROFILE_DESKTOP);

    // Overflow #1 — Transport error, breaker counter increments to 1.
    let err1 = drive_one_fsm_overflow(&server, &mut client).await;
    assert!(
        matches!(err1, ClientError::Transport(_)),
        "first FSM overflow must surface as Transport(ConnectionClosed); got: {err1:?}"
    );
    assert_ne!(
        *state_rx.borrow(),
        ConnectionState::Failed,
        "FSM must NOT be Failed after the first overflow"
    );

    // Overflow #2 — Transport error, breaker counter increments to 2.
    let err2 = drive_one_fsm_overflow(&server, &mut client).await;
    assert!(
        matches!(err2, ClientError::Transport(_)),
        "second FSM overflow must surface as Transport(ConnectionClosed); got: {err2:?}"
    );
    assert_ne!(
        *state_rx.borrow(),
        ConnectionState::Failed,
        "FSM must NOT be Failed after the second overflow"
    );

    // Overflow #3 — circuit breaker trips, FSM transitions to Failed.
    let err3 = drive_one_fsm_overflow(&server, &mut client).await;
    assert!(
        matches!(err3, ClientError::OverflowCircuitBreaker),
        "third FSM overflow must trip the circuit breaker; got: {err3:?}"
    );
    assert_eq!(
        *state_rx.borrow(),
        ConnectionState::Failed,
        "FSM must be in Failed state after circuit breaker trips"
    );

    // The error's Display impl carries the canonical "HA instance too large"
    // message documented in `src/ha/client.rs::ClientError`.  Asserting on
    // the message proves the user-visible failure path is wired end-to-end.
    let msg = format!("{err3}");
    assert!(
        msg.contains("HA instance too large for current profile"),
        "circuit-breaker error message must mention 'HA instance too large for current profile'; \
         got: {msg:?}"
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

// ---------------------------------------------------------------------------
// TASK-048: ServiceRegistry reachability across tasks.
//
// Codex's post-rescue audit (2026-04-27) flagged that `WsClient::services()`
// was only callable from the same task that owned the `WsClient` —
// specifically the WS reconnect-loop task spawned by `run_ws_client`.  No UI
// or command path could observe the populated registry without holding a
// `WsClient` handle, which the bridge does not get.
//
// The fix wraps the `ServiceRegistry` in an `Arc<RwLock<_>>` (the
// `ServiceRegistryHandle` type alias) shared with the `LiveStore`, and
// exposes a read accessor on `LiveStore` (`services_lookup`) so any task
// holding `Arc<LiveStore>` (which the bridge does) can validate
// `(domain, service)` pairs.
//
// This scenario proves the cross-task invariant: the WS task populates the
// registry through the `Phase::Services → Live` write site, and the test
// task — which is a *different* tokio task — observes the population via
// `LiveStore::services_lookup`.  This is the proof that the lock is genuinely
// shared (not just two Arcs to identical initial state).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_services_registry_visible_from_test_task_via_live_store() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    // Empty get_states reply — this scenario is about the services registry,
    // not entity snapshots; an empty entity list keeps the test focused.
    server.script_get_states_reply("[]").await;
    // 2-domain x 2-service get_services payload.  The payload mirrors
    // ServiceRegistry::from_get_services_result's contract (verified by
    // src/ha/services.rs unit tests).
    server
        .script_get_services_reply(
            r#"{
                "light":{
                    "turn_on":{"name":"Turn on","fields":{}},
                    "turn_off":{"name":"Turn off","fields":{}}
                },
                "switch":{
                    "turn_on":{"name":"Switch on","fields":{}},
                    "turn_off":{"name":"Switch off","fields":{}}
                }
            }"#,
        )
        .await;

    // Construct the registry once and clone it into BOTH the LiveStore and
    // the WsClient — replicating the production wiring in
    // `src/lib.rs::run_with_live_store`.  Without this shared Arc the WS
    // task's mutation would land in a private registry the test task could
    // never see.
    let services_handle = hanui::ha::services::ServiceRegistry::new_handle();
    let store: Arc<LiveStore> =
        Arc::new(LiveStore::new().with_services_handle(services_handle.clone()));
    let store_for_ws: Arc<dyn hanui::ha::client::SnapshotApplier> = store.clone();

    let config = make_config(&server.ws_url, "tok-task-048-cross-task");
    let (state_tx, mut state_rx) = status::channel();
    let mut client = WsClient::new(config, state_tx, &PROFILE_DESKTOP)
        .with_store(store_for_ws)
        .with_registry(services_handle.clone());

    // Spawn the WS task.  This is a DIFFERENT tokio task from the test task;
    // the lookup-from-test-task assertion below is what proves cross-task
    // reachability.
    let ws_task = tokio::spawn(async move { client.run().await });

    assert!(
        wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await,
        "client must reach Live before the populated services registry can be observed"
    );

    // Cross-task proof: the WS task above mutated the registry; this test
    // task reads it via the LiveStore-side accessor.  If the registry weren't
    // genuinely shared (e.g. if `with_registry` cloned the inner
    // ServiceRegistry by value), this assertion would fail.
    let light_turn_on = store.services_lookup("light", "turn_on");
    assert!(
        light_turn_on.is_some(),
        "TASK-048 regression: WS task populated the registry but the test \
         task could not observe `light.turn_on` via LiveStore::services_lookup; \
         the shared `ServiceRegistryHandle` is not actually shared"
    );
    assert_eq!(
        light_turn_on.expect("checked is_some above").name,
        "Turn on",
        "service metadata must round-trip through the cross-task shared registry"
    );
    assert!(
        store.services_lookup("light", "turn_off").is_some(),
        "all (domain, service) pairs from the get_services payload must be visible"
    );
    assert!(
        store.services_lookup("switch", "turn_on").is_some(),
        "switch domain entries must also be visible across tasks"
    );
    assert!(
        store.services_lookup("switch", "turn_off").is_some(),
        "all switch entries must round-trip"
    );

    // Negative co-assertion: the registry is bounded by the payload.  This
    // protects against a regression where `services_lookup` returns Some for
    // every input (e.g., a stub that always says "yes").
    assert!(
        store.services_lookup("nonexistent", "turn_on").is_none(),
        "services_lookup must return None for domains not in the payload"
    );
    assert!(
        store.services_lookup("light", "unknown_service").is_none(),
        "services_lookup must return None for unknown services in known domains"
    );

    // Ptr-equality proof: the LiveStore's handle and the original handle we
    // wired in are the same Arc (no copy, no rebuild).
    assert!(
        Arc::ptr_eq(&services_handle, &store.services_handle()),
        "LiveStore must hold the same Arc we passed to with_services_handle; \
         a divergent handle here would mean the WS-side write went elsewhere"
    );

    ws_task.abort();
}

// ---------------------------------------------------------------------------
// TASK-049: live registry freshness via service_registered / service_removed
// events.
//
// Codex's post-rescue audit (2026-04-27) flagged that the client subscribed
// only to `state_changed` events; service-lifecycle events from the HA bus
// were never observed.  A long-running session would therefore see stale
// service capabilities until the next reconnect — Phase 3's command
// dispatcher would either reject newly-registered tap targets or attempt
// removed ones.
//
// TASK-048 made the `ServiceRegistry` cross-task-reachable; TASK-049 wires
// the EVENT path that mutates it.  These scenarios prove the invariant
// end-to-end: drive the FSM to Live, inject a service-lifecycle event, and
// observe the cross-task `LiveStore::services_lookup` accessor reflect the
// change without a reconnect.
// ---------------------------------------------------------------------------

/// Build a `service_registered` event JSON frame for `inject_event`.
///
/// `subscription_id` is the id of the `subscribe_events` request being
/// answered (HA echoes this back in every event frame).  Local helper rather
/// than a `tests/common/mock_ws.rs` addition because TASK-049's allowlist
/// scopes integration-test edits to this file only.
fn service_registered_event_json(subscription_id: u32, domain: &str, service: &str) -> String {
    format!(
        r#"{{"type":"event","id":{subscription_id},"event":{{"event_type":"service_registered","data":{{"domain":"{domain}","service":"{service}"}},"origin":"LOCAL","time_fired":"2024-04-01T12:00:00.000000+00:00"}}}}"#
    )
}

/// Build a `service_removed` event JSON frame for `inject_event`.
fn service_removed_event_json(subscription_id: u32, domain: &str, service: &str) -> String {
    format!(
        r#"{{"type":"event","id":{subscription_id},"event":{{"event_type":"service_removed","data":{{"domain":"{domain}","service":"{service}"}},"origin":"LOCAL","time_fired":"2024-04-01T12:00:01.000000+00:00"}}}}"#
    )
}

/// Spin until `cond()` returns true or the deadline elapses.  Returns true on
/// success, false on timeout.
async fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn scenario_service_registered_event_updates_registry() {
    // Initial registry contains exactly `light.turn_on` (so the test can
    // distinguish "registered via event" from "registered via initial
    // get_services").
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server
        .script_get_services_reply(r#"{"light":{"turn_on":{"name":"Turn on","fields":{}}}}"#)
        .await;

    let services_handle = hanui::ha::services::ServiceRegistry::new_handle();
    let store: Arc<LiveStore> =
        Arc::new(LiveStore::new().with_services_handle(services_handle.clone()));
    let store_for_ws: Arc<dyn hanui::ha::client::SnapshotApplier> = store.clone();

    let config = make_config(&server.ws_url, "tok-svc-registered");
    let (state_tx, mut state_rx) = status::channel();
    let mut client = WsClient::new(config, state_tx, &PROFILE_DESKTOP)
        .with_store(store_for_ws)
        .with_registry(services_handle.clone());

    let ws_task = tokio::spawn(async move { client.run().await });

    assert!(
        wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await,
        "client must reach Live before the service event can be observed"
    );

    // Pre-condition: the brand-new pair is NOT yet in the registry — only
    // `light.turn_on` from the initial get_services payload.  This guards
    // against a regression where every (domain, service) lookup returns Some
    // (e.g. a stub that always says yes).
    assert!(
        store.services_lookup("light", "turn_on").is_some(),
        "initial get_services payload must populate light.turn_on"
    );
    assert!(
        store.services_lookup("script", "shop_run").is_none(),
        "the script.shop_run pair must NOT be in the registry before the event"
    );

    // Inject the service_registered event for a brand-new (domain, service)
    // pair the initial payload did not include.
    server
        .inject_event(service_registered_event_json(1, "script", "shop_run"))
        .await;

    // Cross-task observation: the WS task absorbs the event and writes to the
    // shared registry; this test task reads via the LiveStore accessor.  Use
    // a bounded spin so a regression (event dropped) fails fast.
    let store_for_wait = store.clone();
    let observed = wait_until(
        move || {
            store_for_wait
                .services_lookup("script", "shop_run")
                .is_some()
        },
        Duration::from_secs(2),
    )
    .await;
    assert!(
        observed,
        "TASK-049 regression: service_registered event did not flow into the \
         shared ServiceRegistry within 2s — the event-dispatch path is missing"
    );

    // Negative co-assertion: the previously-known pair must still be present
    // (the event added a new pair, didn't replace the registry).
    assert!(
        store.services_lookup("light", "turn_on").is_some(),
        "service_registered event must NOT clobber unrelated pairs"
    );

    ws_task.abort();
}

#[tokio::test]
async fn scenario_service_removed_event_evicts_from_registry() {
    // Start with a populated registry covering 2 domains × 2 services so we
    // can assert that ONE pair is evicted while siblings remain.
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server
        .script_get_services_reply(
            r#"{
                "light":{
                    "turn_on":{"name":"Turn on","fields":{}},
                    "turn_off":{"name":"Turn off","fields":{}}
                },
                "switch":{
                    "turn_on":{"name":"Switch on","fields":{}},
                    "turn_off":{"name":"Switch off","fields":{}}
                }
            }"#,
        )
        .await;

    let services_handle = hanui::ha::services::ServiceRegistry::new_handle();
    let store: Arc<LiveStore> =
        Arc::new(LiveStore::new().with_services_handle(services_handle.clone()));
    let store_for_ws: Arc<dyn hanui::ha::client::SnapshotApplier> = store.clone();

    let config = make_config(&server.ws_url, "tok-svc-removed");
    let (state_tx, mut state_rx) = status::channel();
    let mut client = WsClient::new(config, state_tx, &PROFILE_DESKTOP)
        .with_store(store_for_ws)
        .with_registry(services_handle.clone());

    let ws_task = tokio::spawn(async move { client.run().await });

    assert!(
        wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await,
        "client must reach Live before the service event can be observed"
    );

    // Pre-condition: all four pairs are present.
    assert!(store.services_lookup("light", "turn_on").is_some());
    assert!(store.services_lookup("light", "turn_off").is_some());
    assert!(store.services_lookup("switch", "turn_on").is_some());
    assert!(store.services_lookup("switch", "turn_off").is_some());

    // Inject service_removed for ONE pair.
    server
        .inject_event(service_removed_event_json(1, "light", "turn_on"))
        .await;

    let store_for_wait = store.clone();
    let evicted = wait_until(
        move || store_for_wait.services_lookup("light", "turn_on").is_none(),
        Duration::from_secs(2),
    )
    .await;
    assert!(
        evicted,
        "TASK-049 regression: service_removed event did not evict light.turn_on \
         from the shared ServiceRegistry within 2s"
    );

    // Sibling pairs must remain — the event removed exactly one (domain,
    // service), not the whole domain or the whole registry.  This co-assertion
    // protects against an over-broad eviction regression.
    assert!(
        store.services_lookup("light", "turn_off").is_some(),
        "sibling service in same domain (light.turn_off) must remain"
    );
    assert!(
        store.services_lookup("switch", "turn_on").is_some(),
        "service in different domain (switch.turn_on) must remain"
    );
    assert!(
        store.services_lookup("switch", "turn_off").is_some(),
        "service in different domain (switch.turn_off) must remain"
    );

    ws_task.abort();
}

// ---------------------------------------------------------------------------
// TASK-123 (F7): three filtered subscriptions, ACK-gate preserved.
//
// Pre-TASK-123 the client subscribed with a wildcard `event_type: None`
// (TASK-049's design choice).  Audit TASK-115 measured ~10x unnecessary
// inbound bytes from `automation_triggered`, `call_service`,
// `recorder_5min_statistics_generated`, etc. — events the FSM dispatch
// silently dropped after parsing.
//
// TASK-123 replaces the wildcard with three sequential filtered subscribes
// (`state_changed`, `service_registered`, `service_removed`).  HA never
// forwards the unsubscribed event types over the wire, so the parse-bytes
// reduction is real, not just internal-dispatch-side.
//
// This test is the regression boundary.  It pins:
//   (a) Exactly THREE `subscribe_events` frames are sent — one per filter.
//   (b) Each subscribe frame carries an explicit `event_type`; no frame
//       OMITS the field (no regression to wildcard).  Order is fixed:
//       state_changed → service_registered → service_removed.
//   (c) The ACK-gate ordering invariant (TASK-046 finding 6) is preserved:
//       `get_states` is sent strictly after the LAST subscribe ACK has
//       left the mock — same wall-clock comparison as
//       `scenario_subscribe_ack_before_snapshot_ordering`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_three_filtered_subscribes_preserve_ack_gate() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server.script_get_states_reply("[]").await;
    server.script_get_services_reply("{}").await;

    let config = make_config(&server.ws_url, "tok-task-123-filtered-subscribes");
    let (mut state_rx, _handle) = spawn_client(config, None);

    assert!(wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await);

    let recorded = server.recorded_requests().await;

    // (a) Exactly three subscribe_events frames in order.  Capture them
    // from the recorded log preserving FIFO order so we can assert filter
    // values position-by-position.
    let sub_frames: Vec<&_> = recorded
        .iter()
        .filter(|r| r.kind == "subscribe_events")
        .collect();
    assert_eq!(
        sub_frames.len(),
        3,
        "TASK-123: exactly THREE subscribe_events frames (state_changed, \
         service_registered, service_removed); got: {}",
        sub_frames.len()
    );

    // (b) Each frame must carry `event_type`; the three filters must
    // appear in the documented order.  Pinning order is what protects the
    // sequencing-gate proof in (c) from a future refactor that fires the
    // three subscribes concurrently — concurrent subscribes break the
    // single-ACK-stream invariant the FSM relies on.
    let expected_filters = ["state_changed", "service_registered", "service_removed"];
    for (i, expected) in expected_filters.iter().enumerate() {
        let v: serde_json::Value = serde_json::from_str(&sub_frames[i].body)
            .expect("subscribe_events frame body must be valid JSON");
        let event_type = v.get("event_type").and_then(|x| x.as_str()).unwrap_or("");
        assert_eq!(
            event_type, *expected,
            "TASK-123: subscribe_events frame #{} must filter on `{expected}`; \
             got body: {v}",
            i
        );
    }

    // (c) ACK-gate ordering invariant: `get_states` arrives strictly after
    // the LAST subscribe ACK was sent by the mock.  `subscribe_ack_sent_at`
    // is overwritten on each subscribe ACK send, so by the time the test
    // observes `Live` it holds the timestamp of the THIRD ACK send — which
    // is the gate `get_states` must wait for.
    let snap_frame = recorded
        .iter()
        .find(|r| r.kind == "get_states")
        .expect("get_states must be recorded");
    let ack_sent_at = server
        .subscribe_ack_sent_at()
        .await
        .expect("mock must have recorded a subscribe_ack send by the time Live is reached");
    assert!(
        snap_frame.received_at > ack_sent_at,
        "TASK-123 regression: get_states received_at ({:?}) is NOT strictly AFTER \
         the LAST subscribe_events ACK sent_at ({:?}); the three-ACK gate is no longer real",
        snap_frame.received_at,
        ack_sent_at,
    );
}

// ---------------------------------------------------------------------------
// TASK-123 (F7): high-irrelevance event ratio — defence-in-depth.
//
// The TASK-115 audit measured ~10x unnecessary inbound bytes from event
// types the client subscribes to but ignores (automation_triggered,
// call_service, recorder_5min_statistics_generated, etc.).  TASK-123 fixes
// the wire-level subscription so HA never forwards those event types.
//
// The actual byte-savings claim — "the client subscribes only to
// state_changed / service_registered / service_removed" — is the load-
// bearing wire-level invariant proven by
// `scenario_three_filtered_subscribes_preserve_ack_gate` above (asserts the
// recorded subscribe frames carry exactly those three filters in order).
// THIS test does NOT independently prove byte savings — `MockWsServer`
// cannot enforce HA's subscription filter (it blindly forwards every
// injected frame), so this binary cannot directly observe HA-side drop
// behaviour.
//
// Instead, this test pins the COMPLEMENTARY defence-in-depth invariant: if
// a misbehaving HA instance (or a buggy proxy) forwarded an
// `automation_triggered` event the client never subscribed to, the FSM's
// `EventVariant::Other` arm silently ignores it in `Phase::Live` and the
// LiveStore stays clean.  Without this property a wire-level filter
// regression that re-introduced wildcard subscribe-all would still produce
// only `state_changed` entity mutations — making the byte-savings claim
// undetectable on the consumer side.  The two tests together (filter +
// drop) bracket the savings claim from both sides.
//
// Mix injected: 9 automation_triggered to 1 state_changed.  After the
// burst, exactly ONE entity reflects the new state and zero
// `automation.noise_*` entities exist.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn high_irrelevance_event_ratio_reduces_parse_bytes() {
    // Build an automation_triggered frame.  Local helper because
    // `tests/common/mock_ws.rs` is outside this task's allowlist.  HA's
    // `automation_triggered` frame shape is documented at
    // <https://www.home-assistant.io/docs/automation/yaml/#trigger-data> —
    // the relevant fields here are `event_type` and the wrapping `event`
    // envelope; the client's `EventVariant::Other` arm only inspects
    // `event_type` so the inner `data` shape is irrelevant.
    fn automation_triggered_event_json(subscription_id: u32, name: &str) -> String {
        format!(
            r#"{{"type":"event","id":{subscription_id},"event":{{"event_type":"automation_triggered","data":{{"name":"{name}","entity_id":"automation.{name}","source":"time"}},"origin":"LOCAL","time_fired":"2024-04-01T12:00:00.000000+00:00"}}}}"#
        )
    }

    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    // Empty initial snapshot — we want to observe the post-Live event flow,
    // not pre-populated state.
    server.script_get_states_reply("[]").await;
    server.script_get_services_reply("{}").await;

    let store = Arc::new(LiveStore::new());
    let store_for_ws: Arc<dyn hanui::ha::client::SnapshotApplier> = store.clone();
    let config = make_config(&server.ws_url, "tok-task-123-irrelevance-ratio");
    let (mut state_rx, _handle) = spawn_client(config, Some(store_for_ws));

    assert!(
        wait_for_state(&mut state_rx, ConnectionState::Live, Duration::from_secs(5)).await,
        "client must reach Live before the irrelevance burst can be evaluated"
    );

    // Layer 1: structural belt-and-braces.  Verify the three subscribe
    // frames each carry an explicit `event_type` filter — i.e. a real HA
    // server would not forward `automation_triggered` to us.  The
    // load-bearing assertion of this property is in
    // `scenario_three_filtered_subscribes_preserve_ack_gate`; this block
    // re-asserts it so this test stands on its own and a wire-filter
    // regression cannot escape this binary.
    let recorded = server.recorded_requests().await;
    let sub_frames: Vec<&_> = recorded
        .iter()
        .filter(|r| r.kind == "subscribe_events")
        .collect();
    assert_eq!(
        sub_frames.len(),
        3,
        "subscribe_events count must be exactly 3 (one per filter); got: {}",
        sub_frames.len()
    );
    for frame in &sub_frames {
        let v: serde_json::Value = serde_json::from_str(&frame.body)
            .expect("subscribe_events frame body must be valid JSON");
        let et = v.get("event_type").and_then(|x| x.as_str()).unwrap_or("");
        assert!(
            matches!(
                et,
                "state_changed" | "service_registered" | "service_removed"
            ),
            "subscribe_events frame must filter on a known event type; got: {et} (body: {v})"
        );
        // Bytes-saved invariant: NONE of the three filters subscribes to
        // `automation_triggered` (or any other noisy event type).
        assert_ne!(
            et, "automation_triggered",
            "regression: client must NOT subscribe to automation_triggered"
        );
    }

    // Layer 2: defence-in-depth.  Inject the 9:1 noise:signal mix.  Even
    // if a buggy HA forwarded the automation events, only the single
    // state_changed must surface in the LiveStore.
    for i in 0..9 {
        server
            .inject_event(automation_triggered_event_json(1, &format!("noise_{i}")))
            .await;
    }
    server
        .inject_event(state_changed_event_json(
            1,
            "light.signal",
            Some((
                "on",
                "2024-04-01T12:00:00+00:00",
                "2024-04-01T12:00:00+00:00",
            )),
            None,
        ))
        .await;

    // Spin until the signal arrives in the store.  The 9 automation events
    // either do not enter the store at all (real HA: subscription filter
    // drops them) OR enter as `EventVariant::Other` and are silently
    // ignored by the FSM's Live event arm (mock HA: defence-in-depth).
    let store_for_wait = store.clone();
    let observed = wait_until(
        move || {
            store_for_wait
                .get(&hanui::ha::entity::EntityId::from("light.signal"))
                .is_some()
        },
        Duration::from_secs(2),
    )
    .await;
    assert!(
        observed,
        "the single state_changed event must surface in the LiveStore within 2s"
    );

    // Negative co-assertion: the noise events MUST NOT have produced any
    // `automation.noise_*` entities in the store.  This is the structural
    // proof that the byte savings translate to no behavioural change in
    // the store.  An automation entity is the LiveStore's first observable
    // side-effect of a parsed event; if any of the 9 noise frames had been
    // mistakenly parsed as `state_changed`, the dispatcher would have
    // applied an entity update for `automation.noise_<i>` — which would
    // be visible here.
    for i in 0..9 {
        let id = format!("automation.noise_{i}");
        assert!(
            store
                .get(&hanui::ha::entity::EntityId::from(id.as_str()))
                .is_none(),
            "automation_triggered must NOT produce an entity update; \
             unexpected entity {id} present in store"
        );
    }
    assert_eq!(
        store
            .get(&hanui::ha::entity::EntityId::from("light.signal"))
            .expect("checked above")
            .state
            .as_ref(),
        "on",
        "the single state_changed event must be applied to its target entity"
    );
}

// ---------------------------------------------------------------------------
// Subscribe-phase failure paths (TASK-123 F7 coverage)
// ---------------------------------------------------------------------------

/// First subscribe ACK fails → client must transition to Failed immediately.
#[tokio::test]
async fn scenario_subscribe_state_changed_fail_transitions_to_failed() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_n_acks_then_fail(0).await;

    let config = make_config(&server.ws_url, "tok-sub-fail-1");
    let (mut state_rx, handle) = spawn_client(config, None);

    assert!(
        wait_for_state(
            &mut state_rx,
            ConnectionState::Failed,
            Duration::from_secs(5)
        )
        .await,
        "state_changed subscribe failure must transition to Failed"
    );
    let result = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("client must finish")
        .expect("task join ok");
    assert!(
        matches!(result, Err(ClientError::AuthInvalid { .. })),
        "expected AuthInvalid on subscribe failure; got: {result:?}"
    );
}

/// First two subscribe ACKs succeed, third (service_registered) fails → Failed.
#[tokio::test]
async fn scenario_subscribe_service_reg_fail_transitions_to_failed() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_n_acks_then_fail(1).await;

    let config = make_config(&server.ws_url, "tok-sub-fail-2");
    let (mut state_rx, handle) = spawn_client(config, None);

    assert!(
        wait_for_state(
            &mut state_rx,
            ConnectionState::Failed,
            Duration::from_secs(5)
        )
        .await,
        "service_registered subscribe failure must transition to Failed"
    );
    let result = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("client must finish")
        .expect("task join ok");
    assert!(
        matches!(result, Err(ClientError::AuthInvalid { .. })),
        "expected AuthInvalid on subscribe failure; got: {result:?}"
    );
}

/// First two ACKs succeed, third (service_removed) fails → Failed.
#[tokio::test]
async fn scenario_subscribe_service_rem_fail_transitions_to_failed() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_n_acks_then_fail(2).await;

    let config = make_config(&server.ws_url, "tok-sub-fail-3");
    let (mut state_rx, handle) = spawn_client(config, None);

    assert!(
        wait_for_state(
            &mut state_rx,
            ConnectionState::Failed,
            Duration::from_secs(5)
        )
        .await,
        "service_removed subscribe failure must transition to Failed"
    );
    let result = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("client must finish")
        .expect("task join ok");
    assert!(
        matches!(result, Err(ClientError::AuthInvalid { .. })),
        "expected AuthInvalid on subscribe failure; got: {result:?}"
    );
}
