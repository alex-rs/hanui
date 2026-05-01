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

use hanui::dashboard::profiles::PROFILE_DESKTOP;
use hanui::dashboard::schema::{
    Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
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
            options: None,
            placement: None,
            visibility: "always".to_string(),
        })
        .collect();

    Arc::new(Dashboard {
        call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
        version: 1,
        device_profile: ProfileKey::Desktop,
        home_assistant: None,
        theme: None,
        default_view: "bench".to_owned(),
        views: vec![View {
            id: "bench".to_owned(),
            title: "Bench".to_owned(),
            layout: Layout::Sections,
            sections: vec![Section {
                grid: hanui::dashboard::schema::SectionGrid::default(),
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
    let client = WsClient::new(config, state_tx, &PROFILE_DESKTOP)
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

// ===========================================================================
// TASK-116 audit-recommended scenarios (PERFORMANCE_AUDIT.md §9)
//
// Each scenario declares **explicit warm-up iterations and sample count** so
// the timings recorded in `benches/baseline.json` are reproducible and Risk #9
// of `docs/plans/2026-04-30-phase-7-performance.md` (Criterion noise on shared
// CI runners) is mitigated structurally rather than by Criterion-default magic.
//
// We do **not** depend on the `criterion` crate: Cargo.toml dependency
// additions beyond F13 are explicitly out of scope per the active plan's
// Non-goals. Instead, each scenario implements a hand-rolled timing loop with
// the warm-up and sample-count semantics that Criterion would have provided —
// expressed as named constants so a reader can verify the configuration
// without running the bench.
//
// Scope: each scenario measures `build_tiles` (the UI flush hot path) and,
// where relevant, `LiveStore::apply_event` (the ingest hot path). These are
// the exact functions targeted by Phase 7 Wave 2 (TASK-117/118/119), so the
// baseline captured here is what the regression checks will measure deltas
// against.
// ===========================================================================

use std::sync::Mutex;
use std::time::Instant;

use hanui::ha::client::event_to_entity_update;
use hanui::ha::protocol::{
    EventPayload, EventVariant, RawEntityState, StateChangedData, StateChangedEvent,
};
use hanui::ha::store::EntityUpdate;
use hanui::ui::bridge::build_tiles;

/// Warm-up iterations performed before any timing samples are recorded.
/// Discarded outputs flush instruction cache and JIT-equivalent state.
const WARM_UP_ITERS: usize = 8;

/// Number of timed samples per scenario. Higher than Criterion's default of
/// 100 only where the per-iteration cost is sub-millisecond and we need more
/// samples to resolve p95 reliably; otherwise 64 is plenty for a regression
/// guard.
const SAMPLE_COUNT_FAST: usize = 256;
const SAMPLE_COUNT_SLOW: usize = 64;

/// Build a `state_changed` [`EntityUpdate`] via the public conversion path.
///
/// `EntityUpdate` is `#[non_exhaustive]`, so external crates (the bench is one)
/// cannot use struct-literal syntax. The canonical external constructor is
/// [`event_to_entity_update`], which takes an [`EventPayload`] whose every
/// nested type is fully public. Mirrors the helper at
/// `tests/integration/lagged_resync.rs:85`.
fn make_state_changed_update(entity_id: &str, new_state: &str) -> EntityUpdate {
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
    event_to_entity_update(&payload)
        .expect("state_changed payload must produce Some(update) — bench helper invariant")
}

/// Build a [`Dashboard`] with `widget_count` LightTile widgets where the i-th
/// widget references entity `light.e{i}`.
fn build_dashboard_with(widget_count: usize, profile: ProfileKey) -> Arc<Dashboard> {
    let widgets: Vec<Widget> = (0..widget_count)
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
            options: None,
            placement: None,
            visibility: "always".to_string(),
        })
        .collect();
    Arc::new(Dashboard {
        call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
        version: 1,
        device_profile: profile,
        home_assistant: None,
        theme: None,
        default_view: "bench".to_owned(),
        views: vec![View {
            id: "bench".to_owned(),
            title: "Bench".to_owned(),
            layout: Layout::Sections,
            sections: vec![Section {
                grid: hanui::dashboard::schema::SectionGrid::default(),
                id: "main".to_owned(),
                title: "Main".to_owned(),
                widgets,
            }],
        }],
    })
}

/// Pre-populate a [`LiveStore`] with `entity_count` entities, all named
/// `light.e{i}` and initialised in the `"off"` state.
///
/// Goes through the canonical `event_to_entity_update` → `apply_event` path
/// rather than touching internal store structures, so the populated store
/// matches the steady-state shape after a full WS handshake + snapshot apply.
fn populate_store(store: &Arc<LiveStore>, entity_count: usize) {
    for i in 0..entity_count {
        let id_str = format!("light.e{i}");
        store.apply_event(make_state_changed_update(&id_str, "off"));
    }
}

/// Reduce a sorted vector of nanosecond timings to (p50, p95, mean).
/// Caller is responsible for sorting `samples` first.
fn percentiles(samples: &[u128]) -> (u128, u128, u128) {
    let n = samples.len();
    if n == 0 {
        return (0, 0, 0);
    }
    let p50_idx = n / 2;
    let p95_idx = ((n as f64) * 0.95) as usize;
    let p95_idx = p95_idx.min(n - 1);
    let mean = samples.iter().sum::<u128>() / (n as u128);
    (samples[p50_idx], samples[p95_idx], mean)
}

/// Print a one-line bench result. Format is parser-friendly so the seeded
/// `benches/baseline.json` capture step can scrape it.
fn print_result(label: &str, p50_ns: u128, p95_ns: u128, mean_ns: u128, samples: usize) {
    println!(
        "BENCH-RESULT scenario={label} p50_ns={p50_ns} p95_ns={p95_ns} mean_ns={mean_ns} samples={samples}"
    );
}

// ---------------------------------------------------------------------------
// Scenario (a): 20 widgets / 2,048 entities / OPI Zero 3 profile
// ---------------------------------------------------------------------------
//
// This is the OPI-profile baseline scenario from PERFORMANCE_AUDIT.md §9.
// 2048 entities live in the store but only 20 are visible (referenced by the
// dashboard). Tests the "total HA entities >> visible widgets" property —
// the store-walk cost dominates `build_tiles` until F2/F3 lands.
//
// Configuration (Risk #9 mitigation: explicit warm-up + sample count):
//   warm_up_iters = WARM_UP_ITERS
//   sample_count  = SAMPLE_COUNT_FAST
//
// Operation under measurement: a single `build_tiles` call against the live
// store and dashboard. This is the hot path that `LiveBridge` invokes on
// every flush tick.
//
// **Bench-broken-blocker exemption:** TASK-116 is exempt from the
// `performance-engineer` bench-broken-blocker clause — F9 *is* the restoration
// PR, not a hot-path PR being gated by it.

#[test]
fn opi_profile_20w_2048e() {
    let store = Arc::new(LiveStore::new());
    populate_store(&store, 2_048);
    let dashboard = build_dashboard_with(20, ProfileKey::OpiZero3);

    // Warm-up phase: discarded.
    for _ in 0..WARM_UP_ITERS {
        let _ = build_tiles(&*store, &dashboard);
    }

    // Timed phase.
    let mut samples_ns: Vec<u128> = Vec::with_capacity(SAMPLE_COUNT_FAST);
    for _ in 0..SAMPLE_COUNT_FAST {
        let t0 = Instant::now();
        let tiles = build_tiles(&*store, &dashboard);
        let elapsed = t0.elapsed().as_nanos();
        // Black-box-ish: keep `tiles` until after timing so the optimizer
        // does not eliminate the call.
        std::hint::black_box(tiles);
        samples_ns.push(elapsed);
    }
    samples_ns.sort_unstable();
    let (p50, p95, mean) = percentiles(&samples_ns);
    print_result("opi_profile_20w_2048e", p50, p95, mean, SAMPLE_COUNT_FAST);

    // Sanity floor: build_tiles must complete in non-zero time and not
    // overflow. The actual regression delta is enforced by the
    // `performance-engineer` agent reading `benches/baseline.json`.
    assert!(p50 > 0, "p50 must be > 0 ns");
    assert!(p95 >= p50, "p95 must be >= p50");
}

// ---------------------------------------------------------------------------
// Scenario (b): 32 widgets / 4,096 entities / Raspberry Pi 4 profile
// ---------------------------------------------------------------------------
//
// RPI-profile baseline. 4096 is `PROFILE_RPI4.max_entities`; 32 is
// `PROFILE_RPI4.max_widgets_per_view`. Together these constants represent
// the "fully loaded RPI" steady state.
//
// Configuration: explicit warm-up + sample count (Risk #9).

#[test]
fn rpi_profile_32w_4096e() {
    let store = Arc::new(LiveStore::new());
    populate_store(&store, 4_096);
    let dashboard = build_dashboard_with(32, ProfileKey::Rpi4);

    for _ in 0..WARM_UP_ITERS {
        let _ = build_tiles(&*store, &dashboard);
    }

    let mut samples_ns: Vec<u128> = Vec::with_capacity(SAMPLE_COUNT_FAST);
    for _ in 0..SAMPLE_COUNT_FAST {
        let t0 = Instant::now();
        let tiles = build_tiles(&*store, &dashboard);
        let elapsed = t0.elapsed().as_nanos();
        std::hint::black_box(tiles);
        samples_ns.push(elapsed);
    }
    samples_ns.sort_unstable();
    let (p50, p95, mean) = percentiles(&samples_ns);
    print_result("rpi_profile_32w_4096e", p50, p95, mean, SAMPLE_COUNT_FAST);
    assert!(p50 > 0);
    assert!(p95 >= p50);
}

// ---------------------------------------------------------------------------
// Scenario (c): bursty updates with 1 changed visible entity
// ---------------------------------------------------------------------------
//
// Models the common case from PERFORMANCE_AUDIT.md §1: a single visible
// entity flips state while thousands of irrelevant entities sit in the store.
// Both the `apply_event` cost (currently a full HashMap clone, which F1 will
// fix) and the subsequent `build_tiles` cost are measured.
//
// Configuration: explicit warm-up + sample count (Risk #9).

#[test]
fn bursty_one_visible_change() {
    let store = Arc::new(LiveStore::new());
    populate_store(&store, 4_096);
    let dashboard = build_dashboard_with(32, ProfileKey::Rpi4);

    for i in 0..WARM_UP_ITERS {
        let state = if i % 2 == 0 { "on" } else { "off" };
        store.apply_event(make_state_changed_update("light.e0", state));
        let _ = build_tiles(&*store, &dashboard);
    }

    let mut apply_samples_ns: Vec<u128> = Vec::with_capacity(SAMPLE_COUNT_FAST);
    let mut tile_samples_ns: Vec<u128> = Vec::with_capacity(SAMPLE_COUNT_FAST);
    for i in 0..SAMPLE_COUNT_FAST {
        let state = if i % 2 == 0 { "on" } else { "off" };

        let t0 = Instant::now();
        store.apply_event(make_state_changed_update("light.e0", state));
        apply_samples_ns.push(t0.elapsed().as_nanos());

        let t1 = Instant::now();
        let tiles = build_tiles(&*store, &dashboard);
        tile_samples_ns.push(t1.elapsed().as_nanos());
        std::hint::black_box(tiles);
    }
    apply_samples_ns.sort_unstable();
    tile_samples_ns.sort_unstable();
    let (a_p50, a_p95, a_mean) = percentiles(&apply_samples_ns);
    let (t_p50, t_p95, t_mean) = percentiles(&tile_samples_ns);
    print_result(
        "bursty_one_visible_change__apply_event",
        a_p50,
        a_p95,
        a_mean,
        SAMPLE_COUNT_FAST,
    );
    print_result(
        "bursty_one_visible_change__build_tiles",
        t_p50,
        t_p95,
        t_mean,
        SAMPLE_COUNT_FAST,
    );
    assert!(a_p50 > 0);
    assert!(t_p50 > 0);
}

// ---------------------------------------------------------------------------
// Scenario (d): reconnect diff with N=profile-cap changed entities
// ---------------------------------------------------------------------------
//
// Models WS reconnect: the entire store is updated in a tight loop, then a
// single `build_tiles` runs. Tests both ingest throughput and the re-walk
// cost. Bursty pattern uses a serialised lock (Mutex around sample collection)
// because the apply loop is a sequential timeline, not a concurrent one.
//
// Configuration: explicit warm-up + sample count (Risk #9). Sample count is
// the slow variant because each sample re-applies 4096 events.

#[test]
fn reconnect_diff_full_cap() {
    let dashboard = build_dashboard_with(32, ProfileKey::Rpi4);
    let entity_cap: usize = 4_096; // PROFILE_RPI4.max_entities

    // Pre-build the EntityUpdate corpus once outside the timed loop so the
    // measurement reflects apply + tiles cost, not allocator churn from
    // building synthetic events. Wrapped in a Mutex purely for ergonomic
    // ownership across the warm-up vs timed phases (no contention).
    let updates: Mutex<Vec<EntityUpdate>> = Mutex::new(
        (0..entity_cap)
            .map(|i| {
                let id_str = format!("light.e{i}");
                make_state_changed_update(&id_str, "on")
            })
            .collect(),
    );

    // Warm-up: a single full reconnect cycle into a throwaway store.
    for _ in 0..WARM_UP_ITERS.min(2) {
        let store = Arc::new(LiveStore::new());
        let updates_guard = updates.lock().unwrap();
        for upd in updates_guard.iter().cloned() {
            store.apply_event(upd);
        }
        drop(updates_guard);
        let _ = build_tiles(&*store, &dashboard);
    }

    // Timed phase. Each sample = (1) populate fresh store with N events,
    // (2) one build_tiles call, recorded as separate measurements.
    let mut populate_samples_ns: Vec<u128> = Vec::with_capacity(SAMPLE_COUNT_SLOW);
    let mut tile_samples_ns: Vec<u128> = Vec::with_capacity(SAMPLE_COUNT_SLOW);
    for _ in 0..SAMPLE_COUNT_SLOW {
        let store = Arc::new(LiveStore::new());
        let updates_guard = updates.lock().unwrap();
        let snapshot: Vec<EntityUpdate> = updates_guard.iter().cloned().collect();
        drop(updates_guard);

        let t0 = Instant::now();
        for upd in snapshot {
            store.apply_event(upd);
        }
        populate_samples_ns.push(t0.elapsed().as_nanos());

        let t1 = Instant::now();
        let tiles = build_tiles(&*store, &dashboard);
        tile_samples_ns.push(t1.elapsed().as_nanos());
        std::hint::black_box(tiles);
    }
    populate_samples_ns.sort_unstable();
    tile_samples_ns.sort_unstable();
    let (p_p50, p_p95, p_mean) = percentiles(&populate_samples_ns);
    let (t_p50, t_p95, t_mean) = percentiles(&tile_samples_ns);
    print_result(
        "reconnect_diff_full_cap__populate_4096",
        p_p50,
        p_p95,
        p_mean,
        SAMPLE_COUNT_SLOW,
    );
    print_result(
        "reconnect_diff_full_cap__build_tiles",
        t_p50,
        t_p95,
        t_mean,
        SAMPLE_COUNT_SLOW,
    );
    assert!(p_p50 > 0);
    assert!(t_p50 > 0);
}

// ---------------------------------------------------------------------------
// Scenario (e): contention at OPI-profile caps (TASK-117 / F1, Risk #1)
// ---------------------------------------------------------------------------
//
// Writer/reader contention at OPI Zero 3 caps with multiple concurrent
// writers and readers hammering the `RwLock`-backed `LiveStore`.  Validates
// the F1 in-place mutation does not regress p95 lock-wait time on the
// most-loaded SBC profile.
//
// Configuration (Risk #9 mitigation: explicit warm-up + sample count):
//   warm_up_iters             = WARM_UP_ITERS
//   sample_count_per_writer   = CONTENTION_OPS_PER_WRITER
//   writer_tasks              = CONTENTION_WRITERS
//   reader_tasks              = CONTENTION_READERS
//
// Operation under measurement: each `apply_event` call's wall-clock
// duration as observed by the writer task (i.e. lock-wait + in-place
// `HashMap::insert` + per-entity broadcast).  Reader tasks call
// `for_each` continuously to maximize read-side contention against the
// `RwLock` write half — they are NOT measured directly; their purpose is
// to load the lock so writer-side p95 is meaningful.
//
// Target (per TASK-117 acceptance criteria): p95 < 1 ms at OPI-profile
// caps and event-rate cap (50 ev/s).  Bench DOES NOT assert the 1 ms
// target — `performance-engineer` parses the BENCH-RESULT line and
// compares against `benches/baseline.json`.  If the captured number is
// > 1 ms, escalate to backend-engineer for sharding (separate plan) per
// TASK-117 spec.
//
// Label convention (per advisory from performance-engineer on TASK-116):
// flat snake_case labels matching the BENCH-RESULT scrape format used by
// the regression parser, e.g. `contention_opi_profile__write_p95_ms`.

/// Concurrent writer tasks for the contention scenario.
const CONTENTION_WRITERS: usize = 4;

/// Concurrent reader tasks for the contention scenario.
const CONTENTION_READERS: usize = 4;

/// `apply_event` operations per writer task in the timed phase.
/// 4 writers × 256 ops = 1024 timed write samples — comfortably above the
/// 100-sample Criterion default for stable p95 estimation.
const CONTENTION_OPS_PER_WRITER: usize = 256;

/// OPI Zero 3 profile entity cap. Mirrors `PROFILE_OPI_ZERO3.max_entities`
/// (held as a literal here to avoid pulling the constant through a
/// non-pub re-export).
const CONTENTION_ENTITY_COUNT: usize = 2_048;

/// `for_each` invocations per reader task in the timed phase. Unbounded
/// readers would dominate the run wall-clock; this bound keeps the run
/// duration predictable while still saturating the read-lock side.
const CONTENTION_READS_PER_READER: usize = 4_096;

#[test]
fn contention_opi_profile() {
    use std::thread;

    let store = Arc::new(LiveStore::new());
    populate_store(&store, CONTENTION_ENTITY_COUNT);

    // Warm-up phase: discarded.  A small number of single-threaded writes
    // primes the snapshot map so the timed phase is not measuring first-
    // touch allocator behaviour.
    for i in 0..WARM_UP_ITERS {
        let id = format!("light.e{}", i % CONTENTION_ENTITY_COUNT);
        store.apply_event(make_state_changed_update(&id, "off"));
    }

    // Timed phase: spawn writers + readers, collect per-write durations.
    //
    // Each writer collects its own Vec<u128> of per-call durations to
    // avoid contending on a shared aggregator (which would dirty the
    // measurement). The aggregator merges after all writers have joined.
    //
    // Readers do NOT record samples; their purpose is to load the read
    // half of the RwLock so writer-side p95 reflects realistic contention.
    let writer_handles: Vec<thread::JoinHandle<Vec<u128>>> = (0..CONTENTION_WRITERS)
        .map(|writer_idx| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let mut samples = Vec::with_capacity(CONTENTION_OPS_PER_WRITER);
                for op_idx in 0..CONTENTION_OPS_PER_WRITER {
                    // Spread writes across all entities so writers do not
                    // serialise on a single key's hash bucket.
                    let entity_idx =
                        (writer_idx * CONTENTION_OPS_PER_WRITER + op_idx) % CONTENTION_ENTITY_COUNT;
                    let id_str = format!("light.e{entity_idx}");
                    let new_state = if op_idx % 2 == 0 { "on" } else { "off" };
                    let update = make_state_changed_update(&id_str, new_state);

                    let t0 = Instant::now();
                    let _changed = store.apply_event(update);
                    samples.push(t0.elapsed().as_nanos());
                }
                samples
            })
        })
        .collect();

    let reader_handles: Vec<thread::JoinHandle<()>> = (0..CONTENTION_READERS)
        .map(|_| {
            let store: Arc<LiveStore> = Arc::clone(&store);
            thread::spawn(move || {
                // Cast to &dyn EntityStore so for_each is dispatched via the
                // visitor seam — same path the bridge takes during a flush.
                for _ in 0..CONTENTION_READS_PER_READER {
                    let entity_store: &dyn EntityStore = &*store;
                    let mut count = 0usize;
                    entity_store.for_each(&mut |_id, _entity| {
                        count += 1;
                    });
                    std::hint::black_box(count);
                }
            })
        })
        .collect();

    // Join writers; merge per-thread sample vectors.
    let mut all_samples_ns: Vec<u128> =
        Vec::with_capacity(CONTENTION_WRITERS * CONTENTION_OPS_PER_WRITER);
    for h in writer_handles {
        let samples = h.join().expect("writer thread panicked");
        all_samples_ns.extend(samples);
    }
    // Drain readers — they have no samples to collect.
    for h in reader_handles {
        h.join().expect("reader thread panicked");
    }

    all_samples_ns.sort_unstable();
    let (p50, p95, mean) = percentiles(&all_samples_ns);

    // Convert ns → ms for the readability of the human label, while still
    // emitting the canonical ns fields so the regression parser sees the
    // same shape as the other scenarios.  Per the TASK-117 advisory from
    // performance-engineer, also emit a single explicit "p95_ms" line so a
    // glance at the bench output reveals whether the < 1 ms target holds.
    print_result(
        "contention_opi_profile__write",
        p50,
        p95,
        mean,
        all_samples_ns.len(),
    );
    let p95_ms = p95 as f64 / 1_000_000.0;
    println!(
        "BENCH-RESULT scenario=contention_opi_profile__write_p95_ms value_ms={p95_ms:.4} \
        target_ms=1.0 writers={CONTENTION_WRITERS} readers={CONTENTION_READERS} \
        ops_per_writer={CONTENTION_OPS_PER_WRITER} entity_count={CONTENTION_ENTITY_COUNT}"
    );

    assert!(p50 > 0, "p50 must be > 0 ns");
    assert!(p95 >= p50, "p95 must be >= p50");
    // NOTE: we deliberately do NOT assert p95 < 1 ms here. The 1 ms target
    // is enforced by `performance-engineer` reading the BENCH-RESULT line
    // against `benches/baseline.json`. Asserting in-process would either
    // (a) regress to a bench-broken-blocker on noisy runners, or
    // (b) silently mask a real regression when the assertion is too loose.
    // The regression check is the single source of truth.
}
