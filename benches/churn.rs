//! Synthetic churn benchmark: 1000 entities at 50 state-changes/sec for 60 s.
//!
//! # Purpose
//!
//! Validates the Phase 2 flush-cadence budget from TASK-033: the `LiveBridge`
//! 80 ms (12.5 Hz) flush interval must hold under realistic load. With 1000
//! entities receiving 50 state events per second, every flush tick will find
//! a non-empty pending map, but no event burst should force the bridge to
//! flush faster than its configured cadence.
//!
//! # How to run
//!
//! This bench is **not** part of the standard `cargo test` run. It is
//! feature-gated so the 60-second wall-clock runtime does not block PR CI.
//! Run it explicitly:
//!
//! ```text
//! cargo test --features bench --test churn -- --nocapture
//! ```
//!
//! # CI schedule
//!
//! **Nightly only; not on every PR.** The run is gated by the `bench` Cargo
//! feature so a plain `cargo test` never includes it. The nightly CI job must
//! pass `--features bench` explicitly. See `docs/backlog/TASK-038.md` for the
//! scheduling contract.
//!
//! # Assertion
//!
//! Average flush rate over the full run must be ≤ 12.5 Hz — that is, the
//! actual tick count for a `T`-second run must satisfy:
//!
//! ```text
//! flushes / T ≤ 12.5
//! ```
//!
//! The 12.5 Hz cap is the reciprocal of the 80 ms [`FLUSH_INTERVAL_MS`]
//! constant exported from `src/ui/bridge.rs`.
//!
//! # Mock harness
//!
//! Reuses `tests/common/mock_ws.rs` via a `#[path]` directive. This is the
//! single canonical mock harness post TASK-042; no duplicate mock
//! implementation exists. The bench uses only a subset of `MockWsServer`'s
//! API (the rest is exercised by the integration / soak / smoke binaries) so
//! the mock module is annotated with `#[expect(dead_code)]` — this produces a
//! warning if the mock is ever refactored such that every method becomes used
//! here, prompting the annotation to be removed (self-cleaning vs. the
//! forbidden `#[allow(…)]` form).
//!
//! [`FLUSH_INTERVAL_MS`]: hanui::ui::bridge::FLUSH_INTERVAL_MS

// Feature-gate: nothing in this file compiles or runs unless --features bench
// is passed. This prevents the 60-second scenario from executing during a
// normal `cargo test` run on every PR.
#![cfg(feature = "bench")]

#[path = "../tests/common/mock_ws.rs"]
#[expect(dead_code)]
mod mock_ws;

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::sync::watch;

use hanui::dashboard::view_spec::{
    Dashboard, Layout, Section, View, Widget, WidgetKind, WidgetLayout,
};
use hanui::ha::client::WsClient;
use hanui::ha::live_store::LiveStore;
use hanui::ha::store::EntityStore;
use hanui::platform::config::Config;
use hanui::platform::status::{self, ConnectionState};
use hanui::ui::bridge::{BridgeSink, LiveBridge, TileVM, FLUSH_INTERVAL_MS};

use mock_ws::{entity_state_json, state_changed_event_json, MockWsServer};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of entities in the benchmark scenario.
const ENTITY_COUNT: usize = 1_000;

/// State-change events injected per second across all entities.
const EVENTS_PER_SEC: u64 = 50;

/// Benchmark run duration in seconds.
const RUN_DURATION_SECS: u64 = 60;

/// Maximum allowed average flush rate (Hz). Matches the 80 ms cadence from
/// TASK-033. Defined as `1000 / FLUSH_INTERVAL_MS` to stay in sync with the
/// actual bridge constant rather than duplicating a magic number.
const MAX_FLUSH_HZ: f64 = 1_000.0 / FLUSH_INTERVAL_MS as f64;

// ---------------------------------------------------------------------------
// Counting BridgeSink
// ---------------------------------------------------------------------------

/// A [`BridgeSink`] that counts how many times `write_tiles` is called.
///
/// Each call to `write_tiles` represents one flush cycle where the bridge
/// found a non-empty pending map and wrote updated tiles. This count divided
/// by elapsed wall-clock seconds gives the average flush rate — the metric the
/// assertion is based on.
///
/// `set_status_banner_visible` calls are not counted because the benchmark
/// runs in `Live` state the entire time, so no banner transitions occur.
struct CountingSink {
    flush_count: Arc<AtomicU64>,
}

impl CountingSink {
    fn new(flush_count: Arc<AtomicU64>) -> Self {
        CountingSink { flush_count }
    }
}

impl BridgeSink for CountingSink {
    fn write_tiles(&self, _tiles: Vec<TileVM>) {
        self.flush_count.fetch_add(1, Ordering::Relaxed);
    }

    fn set_status_banner_visible(&self, _visible: bool) {
        // No-op: the banner never shows during this scenario.
    }
}

// ---------------------------------------------------------------------------
// Env-var serialization lock
// ---------------------------------------------------------------------------

/// Serializes `HA_URL` / `HA_TOKEN` env-var mutations. Mirrors the pattern
/// used in `tests/integration/ws_client.rs` to avoid race conditions when
/// multiple tests in the same binary touch the same env vars.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_config(url: &str, token: &str) -> Config {
    let _g = ENV_LOCK.lock().unwrap();
    // SAFETY: serialized via ENV_LOCK; no other thread reads HA_URL/HA_TOKEN
    // concurrently. Single-threaded with respect to these vars for the
    // duration of Config::from_env().
    unsafe {
        std::env::set_var("HA_URL", url);
        std::env::set_var("HA_TOKEN", token);
    }
    Config::from_env().expect("bench config: env-driven Config::from_env")
}

/// Wait for the given [`ConnectionState`] with a deadline. Returns `true` if
/// the target state is observed before the timeout elapses.
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
// Fixture builders
// ---------------------------------------------------------------------------

/// Build the initial `get_states` reply JSON for `ENTITY_COUNT` entities.
///
/// Each entity has `entity_id = "light.e{i}"` and initial state `"off"`.
/// All entities share the same timestamp so that the diff-and-broadcast path
/// in `WsClient` treats them as new (they were not present before this
/// snapshot).
fn build_initial_states_json() -> String {
    let ts = "2024-01-01T00:00:00+00:00";
    let entries: Vec<String> = (0..ENTITY_COUNT)
        .map(|i| entity_state_json(&format!("light.e{i}"), "off", ts, ts))
        .collect();
    format!("[{}]", entries.join(","))
}

/// Build a minimal [`Dashboard`] that references all `ENTITY_COUNT` entities.
///
/// Uses a flat single-view, single-section layout. Every entity maps to a
/// `LightTile` widget. The tile kind does not affect flush throughput, but
/// the widget count must match `ENTITY_COUNT` so that `build_tiles` walks all
/// entity slots on every flush tick, reproducing realistic bridge CPU load.
fn build_bench_dashboard() -> Arc<Dashboard> {
    let widgets: Vec<Widget> = (0..ENTITY_COUNT)
        .map(|i| Widget {
            id: format!("e{i}"),
            widget_type: WidgetKind::LightTile,
            entity: Some(format!("light.e{i}")),
            entities: vec![],
            name: None,
            icon: None,
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 1,
                preferred_rows: 1,
            },
            options: vec![],
            placement: None,
        })
        .collect();

    Arc::new(Dashboard {
        version: 1,
        device_profile: "desktop".to_owned(),
        home_assistant: None,
        theme: None,
        default_view: "bench".to_owned(),
        views: vec![View {
            id: "bench".to_owned(),
            title: "Bench".to_owned(),
            layout: Layout::Sections,
            sections: vec![Section {
                id: "main".to_owned(),
                title: "Main".to_owned(),
                widgets,
            }],
        }],
    })
}

// ---------------------------------------------------------------------------
// Benchmark scenario
// ---------------------------------------------------------------------------

/// Synthetic churn: 1000 entities at 50 ev/s for 60 s.
///
/// Steps:
///
/// 1. Start the mock WS server; script the full HA handshake.
/// 2. Spawn a `WsClient` backed by a `LiveStore` and wait for
///    `ConnectionState::Live`.
/// 3. Spawn a `LiveBridge` with a `CountingSink` over all 1000 entity widgets.
/// 4. Inject `EVENTS_PER_SEC` `state_changed` events per second for
///    `RUN_DURATION_SECS` seconds, cycling through entities round-robin so
///    that every flush tick finds at least one dirty entity.
/// 5. Assert that the observed average flush rate is ≤ `MAX_FLUSH_HZ` × 1.05.
///
/// The 5 % headroom accommodates timer jitter on a loaded CI box and the one
/// extra flush that may fire at the very start before the 80 ms interval
/// stabilises.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn churn_1000_entities_50_evs_holds_flush_cadence() {
    let server = MockWsServer::start().await;
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server
        .script_get_states_reply(&build_initial_states_json())
        .await;
    server.script_get_services_reply("{}").await;

    let store = Arc::new(LiveStore::new());
    let config = make_config(&server.ws_url, "tok-bench-churn");

    // Spawn WsClient; it owns the state sender.
    let (state_tx, mut state_rx) = status::channel();
    let client = WsClient::new(config, state_tx)
        .with_store(store.clone() as Arc<dyn hanui::ha::client::SnapshotApplier>);
    let _client_handle = tokio::spawn(async move {
        let mut c = client;
        let _ = c.run().await;
    });

    // Wait for the client to complete the full HA handshake and enter Live.
    assert!(
        wait_for_state(
            &mut state_rx,
            ConnectionState::Live,
            Duration::from_secs(10)
        )
        .await,
        "bench: WsClient did not reach Live within 10 s"
    );

    // Wire bridge with the counting sink over all 1000 entity widgets.
    let flush_count = Arc::new(AtomicU64::new(0));
    let sink = CountingSink::new(Arc::clone(&flush_count));
    let dashboard = build_bench_dashboard();

    let _bridge = LiveBridge::spawn(
        store.clone() as Arc<dyn EntityStore>,
        Arc::clone(&dashboard),
        state_rx.clone(),
        sink,
    );

    // ---------------------------------------------------------------------------
    // Inject events at EVENTS_PER_SEC for RUN_DURATION_SECS.
    //
    // The injection loop paces itself with a Tokio interval. Each tick injects
    // one `state_changed` frame for the next entity in round-robin order.
    // Subscription id 1 matches the `subscribe_events` request id that
    // WsClient sends for the first subscription (id sequence starts at 1).
    // ---------------------------------------------------------------------------
    let interval_ms = 1_000 / EVENTS_PER_SEC;
    let total_events = EVENTS_PER_SEC * RUN_DURATION_SECS;
    let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));

    let start = tokio::time::Instant::now();

    for i in 0u64..total_events {
        ticker.tick().await;

        // Round-robin across entities: entity 0, 1, …, 999, 0, 1, …
        let entity_idx = (i as usize) % ENTITY_COUNT;
        // Alternate on/off so last_updated changes each event and the client
        // does not silently skip the broadcast on identical state.
        let new_state = if i % 2 == 0 { "on" } else { "off" };
        // Advance the timestamp monotonically so the diff sees a real change.
        let hour = (i / 3_600) % 24;
        let minute = (i / 60) % 60;
        let second = i % 60;
        let ts = format!("2024-01-01T{hour:02}:{minute:02}:{second:02}+00:00");

        let event = state_changed_event_json(
            1,
            &format!("light.e{entity_idx}"),
            Some((new_state, &ts, &ts)),
            None,
        );
        server.inject_event(event).await;
    }

    let elapsed = start.elapsed();
    let actual_secs = elapsed.as_secs_f64();

    // Allow two full flush intervals after injection ends so the bridge can
    // drain any events buffered in the last tick window.
    tokio::time::sleep(Duration::from_millis(FLUSH_INTERVAL_MS * 2)).await;

    let flushes = flush_count.load(Ordering::Relaxed);
    let avg_flush_hz = flushes as f64 / actual_secs;

    // 5 % headroom on top of the hard 12.5 Hz cap.
    let allowed_hz = MAX_FLUSH_HZ * 1.05;

    println!(
        "churn bench: {flushes} flushes in {actual_secs:.2} s \
        → avg {avg_flush_hz:.3} Hz  (limit {allowed_hz:.3} Hz = {MAX_FLUSH_HZ:.2} Hz × 1.05)"
    );

    assert!(
        avg_flush_hz <= allowed_hz,
        "average flush rate {avg_flush_hz:.3} Hz exceeds {allowed_hz:.3} Hz \
        ({MAX_FLUSH_HZ:.2} Hz × 1.05 headroom); \
        the LiveBridge is flushing faster than its 80 ms cadence"
    );
}
