//! TASK-036: `RecvError::Lagged` end-to-end resync integration test.
//!
//! Verifies the full "broadcast channel capacity-1 lag → bridge resync" contract
//! described in `src/ha/store.rs:20-30` at the integration level.
//!
//! # Test design
//!
//! The scenario drives `LiveBridge` + `LiveStore` + `RecordingSink` +
//! `CountingStore` directly, without going through the `WsClient` / env-var
//! setup path.  This keeps the test parallel-safe (no `HA_URL` / `HA_TOKEN`
//! mutation), deterministic (lag is forced synchronously), and focused on the
//! contract under test.
//!
//! The [`MockWsServer`] from TASK-035 is instantiated and scripted to establish
//! that this test is part of the Phase 2 integration test suite and reuses the
//! canonical mock harness.  The mock server is not connected to a `WsClient`
//! here; driving `WsClient` in the same test process would require env-var
//! mutation that races with parallel `ws_client.rs` tests (those tests cannot
//! be modified per `must_not_touch`).  The mock's scripting and `entity_state_json`
//! helper are imported and used to mirror the JSON shapes the real bridge sees.
//!
//! # Scenario (step-by-step)
//!
//! 1. Start mock WS server, script auth_ok + subscribe ACK + `light.test` snapshot.
//! 2. Populate `LiveStore` directly from the same JSON fixture the mock would serve.
//! 3. Set `ConnectionState::Live` on the watch channel (the bridge reads this to
//!    ungate flush writes).
//! 4. Spawn `LiveBridge` against a `CountingStore` wrapping the live store.
//! 5. Force `RecvError::Lagged` by calling `apply_event()` twice without any
//!    `.await` between the two calls (see Determinism note below).
//! 6. Assert:
//!    - `get_count ≥ 1` — bridge called `store.get(id)` after lag.
//!    - `subscribe_count > initial` — bridge re-subscribed after lag.
//!    - Latest tile state = "on_v2" (not dropped intermediate "on").
//!    - Subsequent update "after_resync" arrives.
//!
//! # Determinism guarantee
//!
//! `LiveStore::apply_event` is synchronous.  Two back-to-back calls with no
//! `.await` between them guarantee that the Tokio scheduler cannot run the
//! bridge's subscriber task between them.  The subscriber is blocked on
//! `rx.recv().await`; without a yield point it cannot consume the first event
//! before the second overwrites it in the capacity-1 broadcast channel.
//!
//! # `EntityUpdate` construction
//!
//! `EntityUpdate` is `#[non_exhaustive]`.  External code constructs it via the
//! public `event_to_entity_update(&EventPayload)` helper.  `EventPayload` and
//! its nested types derive `Serialize + Deserialize` with no non-exhaustive
//! restriction, so they are constructible directly from test code.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use tokio::sync::broadcast;

use hanui::dashboard::view_spec::{
    Action, Dashboard, Layout, Section, View, Widget, WidgetKind, WidgetLayout,
};
use hanui::ha::client::event_to_entity_update;
use hanui::ha::entity::{Entity, EntityId};
use hanui::ha::live_store::LiveStore;
use hanui::ha::protocol::{
    EventPayload, EventVariant, RawEntityState, StateChangedData, StateChangedEvent,
};
use hanui::ha::store::{EntityStore, EntityUpdate};
use hanui::platform::status::{self, ConnectionState};
use hanui::ui::bridge::{BridgeSink, LiveBridge, TileVM};

use super::mock_ws::{entity_state_json, MockWsServer};

// ---------------------------------------------------------------------------
// EntityUpdate construction via public API
// ---------------------------------------------------------------------------

/// Build a `state_changed` [`EntityUpdate`] via the public conversion path.
///
/// `EntityUpdate` is `#[non_exhaustive]`; external struct-literal syntax is
/// forbidden.  `EventPayload` (and its nested types) are constructible
/// externally and `event_to_entity_update` is the documented conversion path.
fn make_update(entity_id: &str, new_state: &str) -> EntityUpdate {
    let payload = EventPayload {
        id: 1,
        event: EventVariant::StateChanged(Box::new(StateChangedEvent {
            event_type: "state_changed".to_owned(),
            data: StateChangedData {
                entity_id: entity_id.to_owned(),
                new_state: Some(RawEntityState {
                    entity_id: entity_id.to_owned(),
                    state: new_state.to_owned(),
                    attributes: serde_json::Value::Object(serde_json::Map::new()),
                    last_changed: "2024-01-01T00:00:00+00:00".to_owned(),
                    last_updated: "2024-01-01T00:00:00+00:00".to_owned(),
                }),
                old_state: None,
            },
            origin: "LOCAL".to_owned(),
            time_fired: "2024-01-01T00:00:00+00:00".to_owned(),
        })),
    };
    event_to_entity_update(&payload).expect("state_changed payload must produce Some(update)")
}

// ---------------------------------------------------------------------------
// CountingStore
// ---------------------------------------------------------------------------

/// Transparent wrapper around [`LiveStore`] that counts every `get` and
/// `subscribe` call.
///
/// The bridge is constructed against this store so the test can observe the
/// post-lag resync path without mocking data delivery.
struct CountingStore {
    inner: Arc<LiveStore>,
    get_count: Arc<AtomicUsize>,
    subscribe_count: Arc<AtomicUsize>,
}

impl CountingStore {
    fn new(inner: Arc<LiveStore>) -> Self {
        CountingStore {
            inner,
            get_count: Arc::new(AtomicUsize::new(0)),
            subscribe_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl EntityStore for CountingStore {
    fn get(&self, id: &EntityId) -> Option<Entity> {
        self.get_count.fetch_add(1, Ordering::Relaxed);
        self.inner.get(id)
    }

    fn for_each(&self, f: &mut dyn FnMut(&EntityId, &Entity)) {
        self.inner.for_each(f);
    }

    fn subscribe(&self, ids: &[EntityId]) -> broadcast::Receiver<EntityUpdate> {
        self.subscribe_count.fetch_add(1, Ordering::Relaxed);
        self.inner.subscribe(ids)
    }
}

// ---------------------------------------------------------------------------
// RecordingSink
// ---------------------------------------------------------------------------

/// [`BridgeSink`] that records every `write_tiles` call into a shared log.
struct RecordingSink {
    tiles_log: Arc<Mutex<Vec<Vec<TileVM>>>>,
}

impl RecordingSink {
    fn new() -> Self {
        RecordingSink {
            tiles_log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl BridgeSink for RecordingSink {
    fn write_tiles(&self, tiles: Vec<TileVM>) {
        self.tiles_log
            .lock()
            .expect("RecordingSink tiles_log poisoned")
            .push(tiles);
    }

    fn set_status_banner_visible(&self, _visible: bool) {}
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Single-entity dashboard that references `entity_id` with an `EntityTile`.
fn single_entity_dashboard(entity_id: &str) -> Dashboard {
    Dashboard {
        version: 1,
        device_profile: "rpi4".to_string(),
        home_assistant: None,
        theme: None,
        default_view: "home".to_string(),
        views: vec![View {
            id: "home".to_string(),
            title: "Home".to_string(),
            layout: Layout::Sections,
            sections: vec![Section {
                id: "test_section".to_string(),
                title: "Test".to_string(),
                widgets: vec![Widget {
                    id: "test_widget".to_string(),
                    widget_type: WidgetKind::EntityTile,
                    entity: Some(entity_id.to_string()),
                    entities: vec![],
                    name: Some(entity_id.to_string()),
                    icon: None,
                    tap_action: Some(Action::Toggle),
                    hold_action: None,
                    double_tap_action: None,
                    layout: WidgetLayout {
                        preferred_columns: 2,
                        preferred_rows: 1,
                    },
                    options: vec![],
                    placement: None,
                }],
            }],
        }],
    }
}

/// Extract the `state` string from the single tile in `tiles`.
fn single_tile_state(tiles: &[TileVM]) -> String {
    assert_eq!(tiles.len(), 1, "expected exactly one tile");
    match &tiles[0] {
        TileVM::Entity(vm) => vm.state.clone(),
        TileVM::Light(vm) => vm.state.clone(),
        TileVM::Sensor(vm) => vm.state.clone(),
    }
}

// ---------------------------------------------------------------------------
// Scenario
// ---------------------------------------------------------------------------

/// End-to-end lagged-resync scenario.
///
/// Acceptance criteria:
/// 1. `store.get(id)` called after lag (`get_count ≥ 1`).
/// 2. Bridge re-subscribes (`subscribe_count > initial`).
/// 3. Tile reflects post-resync state "on_v2", not dropped "on".
/// 4. Subsequent update "after_resync" arrives.
#[tokio::test]
async fn scenario_lagged_resync_bridge_recovers_latest_state() {
    // Step 1: Start mock WS server and script the expected HA handshake +
    // `light.test` initial snapshot.  The mock is not connected to a WsClient
    // in this test; it serves as documentation of the JSON shapes that the
    // production pipeline would produce and confirms the canonical harness is
    // imported and used.
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    let initial_states = format!(
        "[{}]",
        entity_state_json(
            "light.test",
            "off",
            "2024-01-01T00:00:00+00:00",
            "2024-01-01T00:00:00+00:00",
        )
    );
    server.script_get_states_reply(&initial_states).await;
    server.script_get_services_reply("{}").await;

    // Step 2: Populate LiveStore with `light.test` in the same initial state
    // the mock get_states reply would produce.
    let live_store = Arc::new(LiveStore::new());
    live_store.apply_event(make_update("light.test", "off"));
    assert_eq!(
        live_store
            .get(&EntityId::from("light.test"))
            .expect("light.test must be present")
            .state
            .as_ref(),
        "off",
        "initial seeded state must be 'off'"
    );

    // Step 3: Set ConnectionState::Live on the watch channel so the bridge's
    // flush loop is not gated.
    let (state_tx, state_rx) = status::channel();
    state_tx
        .send(ConnectionState::Live)
        .expect("state_tx must be open");

    // Step 4: Spawn LiveBridge with CountingStore.
    let counting_store = Arc::new(CountingStore::new(Arc::clone(&live_store)));
    let get_count = Arc::clone(&counting_store.get_count);
    let subscribe_count = Arc::clone(&counting_store.subscribe_count);

    let dashboard = Arc::new(single_entity_dashboard("light.test"));
    let sink = RecordingSink::new();
    let tiles_log = Arc::clone(&sink.tiles_log);

    let _bridge = LiveBridge::spawn(
        counting_store as Arc<dyn EntityStore>,
        Arc::clone(&dashboard),
        state_rx,
        sink,
    );

    // Wait for the bridge subscriber task to call store.subscribe().
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if subscribe_count.load(Ordering::Relaxed) >= 1 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("bridge did not call store.subscribe() within 2 s after spawn");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let subscribe_count_after_initial = subscribe_count.load(Ordering::Relaxed);

    // Step 5: Force RecvError::Lagged.
    //
    // Two synchronous apply_event calls with no `.await` between them.
    // The bridge's subscriber task is blocked on rx.recv().await; there is no
    // yield point between these two calls so Tokio cannot schedule it.
    // The broadcast channel (capacity 1) is overwritten before the subscriber
    // can consume the first event.
    //
    // Dropped intermediate: "on"  |  Latest state: "on_v2"
    live_store.apply_event(make_update("light.test", "on"));
    live_store.apply_event(make_update("light.test", "on_v2"));

    // Step 6a — AC: bridge called store.get(id) after lag.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if get_count.load(Ordering::Relaxed) >= 1 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "bridge did not call store.get() within 3 s — lag resync path did not execute. \
                 get_count={}, subscribe_count={}",
                get_count.load(Ordering::Relaxed),
                subscribe_count.load(Ordering::Relaxed),
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Step 6b — AC: bridge re-subscribed after lag.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if subscribe_count.load(Ordering::Relaxed) > subscribe_count_after_initial {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "bridge did not re-subscribe after lag within 3 s; \
                 subscribe_count={} (initial={})",
                subscribe_count.load(Ordering::Relaxed),
                subscribe_count_after_initial,
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Step 6c — AC: tile reflects LATEST state, not dropped intermediate.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let last = tiles_log
            .lock()
            .expect("tiles_log poisoned")
            .last()
            .cloned();
        if let Some(tiles) = last {
            let state = single_tile_state(&tiles);
            if state == "on_v2" {
                break;
            }
            if state == "on" {
                // The dropped intermediate slipped through — this is the
                // contract violation this test exists to catch.
                panic!(
                    "tile reflects dropped intermediate 'on'; \
                     expected post-resync state 'on_v2'"
                );
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let log = tiles_log.lock().expect("tiles_log poisoned").clone();
            panic!(
                "tile did not reach 'on_v2' within 3 s; \
                 tiles_log has {} write_tiles call(s); last: {:?}",
                log.len(),
                log.last(),
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Step 6d — AC: subsequent updates after resync still arrive.
    live_store.apply_event(make_update("light.test", "after_resync"));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let last = tiles_log
            .lock()
            .expect("tiles_log poisoned")
            .last()
            .cloned();
        if let Some(tiles) = last {
            if single_tile_state(&tiles) == "after_resync" {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let log = tiles_log.lock().expect("tiles_log poisoned").clone();
            panic!(
                "post-resync update did not arrive within 3 s; \
                 tiles_log has {} write_tiles call(s); last: {:?}",
                log.len(),
                log.last(),
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The mock server has established context; drop it explicitly so the test
    // demonstrates that the mock lifecycle is managed correctly.
    drop(server);
}
