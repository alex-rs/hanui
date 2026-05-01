//! SBC-class CPU smoke test for the WS + LiveStore hot path.
//!
//! # Gate scope
//!
//! **This test gates the QEMU emulation budget. Real SBC numbers on physical
//! hardware are a Phase 5 acceptance gate, not Phase 2.**
//!
//! The test runs the WS + LiveStore hot path inside `qemu-aarch64` user-mode
//! emulation (cross-compiled aarch64 binary executing on the x86_64 CI runner).
//! QEMU user-mode faithfully reproduces the instruction stream; it does NOT
//! model hardware-specific behaviour (GPIOs, real-time scheduling, graphics).
//! CPU% measured here reflects "how much host CPU the emulated hot path
//! consumes" — a proxy for whether the aarch64 code has a gross regression
//! (e.g. spinning mutex, tight retry loop) that would manifest on real hardware.
//!
//! Budget: `PROFILE_OPI_ZERO3.cpu_smoke_budget_pct` = 50 %.  This is
//! deliberately generous: QEMU user-mode adds ~3–5× emulation overhead vs
//! native; a real OPI Zero 3 running the same workload at 50 ev/s should land
//! well below 10 % CPU.  The budget catches regressions, not performance
//! micro-optimisation.
//!
//! # Running locally (without CI)
//!
//! Requirements:
//! - `qemu-user-static` (or `qemu-aarch64`) installed on the host.
//! - `aarch64-unknown-linux-gnu` cross-toolchain (`gcc-aarch64-linux-gnu`,
//!   `binutils-aarch64-linux-gnu`, and `libc6-dev-arm64-cross` on Debian/Ubuntu).
//! - The `aarch64-unknown-linux-gnu` Rust target: `rustup target add aarch64-unknown-linux-gnu`.
//!
//! Build the aarch64 test binary:
//! ```text
//! cargo build --target aarch64-unknown-linux-gnu --tests
//! ```
//!
//! Run the SBC smoke test under QEMU:
//! ```text
//! qemu-aarch64-static \
//!   -L /usr/aarch64-linux-gnu \
//!   target/aarch64-unknown-linux-gnu/debug/sbc_smoke-<hash> \
//!   --test-threads 1 \
//!   sbc_cpu_smoke_50evs_60s_below_budget
//! ```
//!
//! (Substitute the exact binary name from `target/aarch64-unknown-linux-gnu/debug/`.)
//!
//! Expected outcome on a modern x86_64 host: test passes in ~62 s with CPU%
//! well below the 50 % budget. QEMU adds emulation overhead; the process may
//! pin one host CPU core at 100 % during the run, but that is normal — the CPU%
//! metric in this test is measured against the 60 s wall-clock window, not the
//! instantaneous host CPU utilisation.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hanui::dashboard::profiles::{DeviceProfile, PROFILE_OPI_ZERO3};
use hanui::ha::client::{event_to_entity_update, WsClient};
use hanui::ha::live_store::LiveStore;
use hanui::ha::protocol::{
    EventPayload, EventVariant, RawEntityState, StateChangedData, StateChangedEvent,
};
use hanui::platform::config::Config;
use hanui::platform::status;

use super::mock_ws::{state_changed_event_json, MockWsServer};

// ---------------------------------------------------------------------------
// Helper: build an EntityUpdate via the public event_to_entity_update path
// ---------------------------------------------------------------------------

/// Construct an `EntityUpdate` from an entity-id and a new state string.
///
/// `EntityUpdate` is `#[non_exhaustive]`; external crates must use the
/// `event_to_entity_update` conversion path rather than struct-literal
/// construction.  This helper wraps that path for smoke-test convenience.
fn make_entity_update(entity_id: &str, new_state: &str) -> hanui::ha::store::EntityUpdate {
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
    event_to_entity_update(&payload).expect("state_changed payload must produce Some(EntityUpdate)")
}

// ---------------------------------------------------------------------------
// CPU time reader
// ---------------------------------------------------------------------------

/// Read the process CPU time (user + system) in seconds from `/proc/self/stat`.
///
/// Returns `None` when `/proc/self/stat` is unavailable (non-Linux platforms or
/// permission errors). Tests that call this function skip the assertion
/// gracefully on `None` returns.
///
/// The fields in `/proc/self/stat` are documented in `proc(5)`. Fields are
/// 1-indexed in the man page; here we use 0-indexed array access after
/// splitting on whitespace. Field 14 (index 13) = `utime` (user ticks),
/// field 15 (index 14) = `stime` (kernel ticks). Both are in clock ticks;
/// divide by `sysconf(_SC_CLK_TCK)` to get seconds.
fn read_cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // /proc/self/stat may contain spaces in the comm field (field 2, wrapped
    // in parentheses). Skip past the closing ')' before splitting the rest.
    let after_comm = stat.find(')')?.checked_add(2)?;
    let rest = &stat[after_comm..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After stripping the comm field, field 14 (utime) is at index 11 and
    // field 15 (stime) is at index 12 in the remainder (0-based from the
    // character after ')').
    // stat fields (1-indexed): pid=1 comm=2 state=3 ppid=4 pgrp=5 session=6
    //   tty_nr=7 tpgid=8 flags=9 minflt=10 cminflt=11 majflt=12 cmajflt=13
    //   utime=14 stime=15 ...
    // After stripping "pid (comm) ", fields remaining: state=0 ppid=1 pgrp=2
    //   session=3 tty_nr=4 tpgid=5 flags=6 minflt=7 cminflt=8 majflt=9
    //   cmajflt=10 utime=11 stime=12 ...
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    // sysconf(_SC_CLK_TCK) is almost universally 100 on Linux (including under
    // QEMU user-mode). Using the syscall is more correct but adds an unsafe
    // block; since this is a test-only helper and the hard-coded 100 is the
    // correct value for all supported platforms, we keep it simple.
    let clk_tck: f64 = 100.0;
    Some((utime + stime) as f64 / clk_tck)
}

// ---------------------------------------------------------------------------
// Env serialization for HA_URL / HA_TOKEN mutation
// ---------------------------------------------------------------------------

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_config(url: &str, token: &str) -> Config {
    let _g = ENV_LOCK.lock().unwrap();
    // SAFETY: serialized via ENV_LOCK; only this test mutates HA_URL/HA_TOKEN.
    unsafe {
        std::env::set_var("HA_URL", url);
        std::env::set_var("HA_TOKEN", token);
    }
    Config::from_env().expect("sbc_cpu test: Config::from_env")
}

// ---------------------------------------------------------------------------
// Helpers re-used from ws_client.rs (inlined to avoid cross-binary imports)
// ---------------------------------------------------------------------------

/// Wait until `ConnectionState` equals `target`, or until `timeout` elapses.
async fn wait_for_state(
    rx: &mut tokio::sync::watch::Receiver<hanui::platform::status::ConnectionState>,
    target: hanui::platform::status::ConnectionState,
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
// The CPU smoke scenario
// ---------------------------------------------------------------------------

/// 60 s churn at 50 ev/s — average CPU% must stay ≤ `profile.cpu_smoke_budget_pct`.
///
/// Scenario:
/// 1. Start mock WS server and script a full happy-path handshake.
/// 2. Connect a `WsClient` backed by a `LiveStore`.
/// 3. Inject `state_changed` events at 50 ev/s for 60 s (3 000 events total).
/// 4. After the churn window, measure wall-clock time and process CPU time.
/// 5. Assert average CPU% = (cpu_seconds / wall_seconds) * 100.0 ≤ budget.
///
/// CPU measurement uses `/proc/self/stat`. When the file is unavailable (non-Linux
/// platform or build environment), the assertion is skipped and the test passes
/// vacuously — this avoids false negatives on developer macOS machines.
///
/// The 60 s run is intentionally short: at 50 ev/s the mock generates
/// 3 000 events, which is enough to surface hot-path regressions without
/// making the CI nightly budget painful.
///
/// # TASK-122 F10 — entity injection at scale + SBC profile
///
/// Entity injection now cycles through `WIDGET_COUNT` unique entities (not a
/// hardcoded 10), ensuring the smoke test stresses the full entity set.  The
/// profile binding is `PROFILE_OPI_ZERO3` (50 % CPU budget) rather than
/// `PROFILE_DESKTOP` — this test validates against a real SBC budget, not the
/// desktop development profile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sbc_cpu_smoke_50evs_60s_below_budget() {
    const CHURN_SECS: u64 = 60;
    const EV_PER_SEC: u64 = 50;
    const TOTAL_EVENTS: u64 = CHURN_SECS * EV_PER_SEC;
    // Number of unique entities injected: matches PROFILE_OPI_ZERO3.max_widgets_per_view
    // so the smoke test stresses all widget slots (TASK-122 F10 fix).
    const WIDGET_COUNT: u64 = 20;

    // TASK-122 F10: Use PROFILE_OPI_ZERO3 — the tighter SBC budget (50% CPU)
    // instead of PROFILE_DESKTOP. Both the WsClient construction and the
    // budget assertion draw from the same binding.
    let profile: &'static DeviceProfile = &PROFILE_OPI_ZERO3;

    let server = MockWsServer::start().await;

    // Script a full happy-path handshake.
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    // Start with an empty state snapshot; events will populate it.
    server.script_get_states_reply("[]").await;
    server.script_get_services_reply("{}").await;

    let store = Arc::new(LiveStore::new());
    let config = make_config(&server.ws_url, "tok-sbc-smoke");
    let (state_tx, mut state_rx) = status::channel();
    let client = WsClient::new(config, state_tx, profile).with_store(store.clone());
    let client_handle = tokio::spawn(async move {
        let mut c = client;
        c.run().await
    });

    // Wait for the client to reach Live state before starting the churn.
    assert!(
        wait_for_state(
            &mut state_rx,
            hanui::platform::status::ConnectionState::Live,
            Duration::from_secs(10),
        )
        .await,
        "sbc_cpu_smoke: client must reach Live state before churn begins"
    );

    // Record CPU baseline immediately before the churn window.
    let cpu_start = read_cpu_seconds();
    let wall_start = Instant::now();

    // Inject events at 50 ev/s for CHURN_SECS seconds.
    // The interval is 20 ms; we pre-calculate the total so the injector
    // stops after exactly TOTAL_EVENTS injections.
    // TASK-122 F10: cycle through WIDGET_COUNT unique entities, not 10.
    let server_arc = Arc::new(server);
    let inject_handle = {
        let server_for_inject = Arc::clone(&server_arc);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(1000 / EV_PER_SEC));
            for i in 0..TOTAL_EVENTS {
                interval.tick().await;
                let entity_id = format!("light.smoke_{}", i % WIDGET_COUNT);
                let state = if i % 2 == 0 { "on" } else { "off" };
                let ts = "2024-01-01T00:00:00+00:00";
                let frame = state_changed_event_json(1, &entity_id, Some((state, ts, ts)), None);
                server_for_inject.inject_event(frame).await;
            }
        })
    };

    // Wait for the full injection window to complete.
    inject_handle.await.expect("inject task must not panic");

    // Measure wall time and CPU time after the churn window.
    let wall_secs = wall_start.elapsed().as_secs_f64();
    let cpu_end = read_cpu_seconds();

    // Abort the WsClient — we have the measurements we need.
    client_handle.abort();

    // -----------------------------------------------------------------------
    // Assert CPU budget
    // -----------------------------------------------------------------------

    let budget_pct = f64::from(profile.cpu_smoke_budget_pct);

    match (cpu_start, cpu_end) {
        (Some(start), Some(end)) => {
            let cpu_secs = end - start;
            // Guard against clock warp (should not happen but avoids a panic on
            // unusual environments where the measurement is unreliable).
            assert!(
                wall_secs > 0.0,
                "sbc_cpu_smoke: wall_secs must be positive; got {wall_secs}"
            );
            let avg_cpu_pct = (cpu_secs / wall_secs) * 100.0;
            assert!(
                avg_cpu_pct <= budget_pct,
                "sbc_cpu_smoke: average CPU% {avg_cpu_pct:.1}% exceeds \
                 profile.cpu_smoke_budget_pct={budget_pct}% \
                 (cpu_secs={cpu_secs:.2} wall_secs={wall_secs:.2}). \
                 Check for hot-path regressions in the WS+LiveStore loop."
            );
            // Log the measured values for CI visibility (eprintln goes to
            // test stderr, visible with `cargo test -- --nocapture`).
            eprintln!(
                "sbc_cpu_smoke: avg_cpu_pct={avg_cpu_pct:.1}% \
                 cpu_secs={cpu_secs:.2} wall_secs={wall_secs:.2} \
                 budget={budget_pct}% events={TOTAL_EVENTS}"
            );
        }
        _ => {
            // /proc/self/stat unavailable — skip assertion but do not fail.
            // This branch is taken on non-Linux platforms (developer macOS)
            // and is NOT the execution path in CI (CI runs on ubuntu-latest
            // or under qemu-aarch64, both of which expose /proc/self/stat).
            eprintln!(
                "sbc_cpu_smoke: /proc/self/stat unavailable; \
                 skipping CPU assertion (non-Linux or restricted environment)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Headless LiveBridge: total-vs-visible entity scale test (TASK-122 F10)
// ---------------------------------------------------------------------------

/// Inline recording sink for the headless bridge test.
///
/// Records each `apply_row_updates` call as a `Vec<usize>` of row-update
/// counts so the test can assert on flush cardinality without depending on
/// the bridge-internal `RowUpdate` type.
struct CountingSink {
    /// Each entry is the number of `RowUpdate` items in a single flush call.
    flush_counts: Mutex<Vec<usize>>,
}

impl CountingSink {
    fn new() -> Self {
        CountingSink {
            flush_counts: Mutex::new(Vec::new()),
        }
    }

    fn snapshot(&self) -> Vec<usize> {
        self.flush_counts
            .lock()
            .expect("CountingSink mutex poisoned")
            .clone()
    }
}

impl hanui::ui::bridge::BridgeSink for CountingSink {
    fn write_tiles(&self, _tiles: Vec<hanui::ui::bridge::TileVM>) {
        // Full-model writes (resync path) are not the focus of this test;
        // they are not counted in flush_counts.
    }

    fn apply_row_updates(
        &self,
        updates: Vec<hanui::ui::bridge::RowUpdate>,
        _rebuild_full_tiles: Box<dyn FnOnce() -> Vec<hanui::ui::bridge::TileVM> + Send>,
    ) {
        // Record the number of rows changed in this flush, not the total
        // entity count.  The assertion below verifies that a single-entity
        // state change produces exactly 1 row update regardless of how many
        // total entities exist in the store.
        self.flush_counts
            .lock()
            .expect("CountingSink mutex poisoned")
            .push(updates.len());
    }

    fn set_status_banner_visible(&self, _visible: bool) {}
}

/// Proxy so the test thread retains an `Arc` to `CountingSink` while
/// `LiveBridge` owns the forwarding value.
struct ArcCountingSink(Arc<CountingSink>);

impl hanui::ui::bridge::BridgeSink for ArcCountingSink {
    fn write_tiles(&self, tiles: Vec<hanui::ui::bridge::TileVM>) {
        self.0.write_tiles(tiles);
    }

    fn apply_row_updates(
        &self,
        updates: Vec<hanui::ui::bridge::RowUpdate>,
        rebuild: Box<dyn FnOnce() -> Vec<hanui::ui::bridge::TileVM> + Send>,
    ) {
        self.0.apply_row_updates(updates, rebuild);
    }

    fn set_status_banner_visible(&self, visible: bool) {
        self.0.set_status_banner_visible(visible);
    }
}

/// Build a minimal `Dashboard` with `widget_count` `EntityTile` widgets,
/// each pointing at `{entity_prefix}{i}` for i in 0..widget_count.
fn make_dashboard_with_n_widgets(
    widget_count: usize,
    entity_prefix: &str,
) -> hanui::dashboard::schema::Dashboard {
    use hanui::dashboard::schema::{
        Dashboard, Layout, ProfileKey, Section, SectionGrid, View, Widget, WidgetKind, WidgetLayout,
    };
    let widgets: Vec<Widget> = (0..widget_count)
        .map(|i| Widget {
            id: format!("w{i}"),
            widget_type: WidgetKind::EntityTile,
            entity: Some(format!("{entity_prefix}{i}")),
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
    Dashboard {
        call_service_allowlist: Arc::new(std::collections::BTreeSet::new()),
        version: 1,
        device_profile: ProfileKey::OpiZero3,
        home_assistant: None,
        theme: None,
        default_view: "home".to_string(),
        views: vec![View {
            id: "home".to_string(),
            title: "Home".to_string(),
            layout: Layout::Sections,
            sections: vec![Section {
                grid: SectionGrid::default(),
                id: "s1".to_string(),
                title: "Smoke".to_string(),
                widgets,
            }],
        }],
    }
}

/// 2048 total HA entities, only 20 visible widgets.
///
/// After TASK-119 the flush path calls `apply_row_updates` with O(changed_rows)
/// updates — not O(total_entities).  This test verifies that a single-entity
/// state-changed event produces exactly 1 row update even when the store holds
/// 2048 entities.
///
/// Setup:
/// 1. Populate a `LiveStore` with 2048 entities (`sensor.bg_NNN`).
/// 2. Build a `Dashboard` with 20 `EntityTile` widgets referencing only
///    `light.visible_0..19`.
/// 3. Spawn `LiveBridge` against both, using the OPI Zero 3 profile.
/// 4. Apply a state-changed event for `light.visible_0`.
/// 5. Assert that `CountingSink` received a flush with exactly 1 row update,
///    not 2048 — flush cost must scale with visible widgets, not total entities.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sbc_bridge_total_exceeds_visible_entities() {
    const TOTAL_ENTITIES: usize = 2048;
    const VISIBLE_WIDGETS: usize = 20;

    // Initialise icons so build_tiles → resolve does not panic on a missing
    // icon registry.  Idempotent; safe to call multiple times.
    hanui::assets::icons::init();

    let store = Arc::new(LiveStore::new());

    // Populate TOTAL_ENTITIES background entities (not referenced by any widget).
    let bg_entities: Vec<hanui::ha::entity::Entity> = (0..TOTAL_ENTITIES)
        .map(|i| hanui::ha::entity::Entity {
            id: hanui::ha::entity::EntityId::from(format!("sensor.bg_{i:04}").as_str()),
            state: Arc::from("ok"),
            attributes: Arc::default(),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        })
        .collect();
    store.apply_snapshot(bg_entities);

    // Pre-populate the VISIBLE_WIDGETS light entities so the bridge can build
    // an initial tile snapshot.
    for i in 0..VISIBLE_WIDGETS {
        let id_str = format!("light.visible_{i}");
        store.apply_event(make_entity_update(&id_str, "off"));
    }

    // Dashboard references only the 20 visible lights — not the 2048 bg sensors.
    let dashboard = Arc::new(make_dashboard_with_n_widgets(
        VISIBLE_WIDGETS,
        "light.visible_",
    ));

    let (state_tx, state_rx) = hanui::platform::status::channel();
    // Drive straight to Live so the bridge does not gate writes.
    state_tx
        .send(hanui::platform::status::ConnectionState::Live)
        .expect("status channel must accept Live");

    let sink = Arc::new(CountingSink::new());
    let _bridge = hanui::ui::bridge::LiveBridge::spawn(
        store.clone() as Arc<dyn hanui::ha::store::EntityStore>,
        dashboard.clone(),
        state_rx,
        ArcCountingSink(Arc::clone(&sink)),
    );

    // Allow subscriber tasks to register and the initial resync flush to land.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Clear any flush counts from the initial resync (write_tiles path does
    // not record in flush_counts; apply_row_updates may fire if subscriber
    // tasks race the initial Live transition).
    {
        let mut guard = sink
            .flush_counts
            .lock()
            .expect("CountingSink mutex poisoned");
        guard.clear();
    }

    // Inject a state-changed event for exactly one visible entity.
    store.apply_event(make_entity_update("light.visible_0", "on"));

    // Wait up to 2 flush cadences (2 × 80 ms = 160 ms) plus a small margin.
    let flush_cadence_ms: u64 = hanui::ui::bridge::FLUSH_INTERVAL_MS;
    tokio::time::sleep(Duration::from_millis(flush_cadence_ms * 2 + 20)).await;

    let counts = sink.snapshot();

    // At least one flush must have fired after the entity update.
    assert!(
        !counts.is_empty(),
        "sbc_bridge: expected at least one apply_row_updates call after a state-changed event; \
         got none. Check that the subscriber task is running and the flush loop fires."
    );

    // The first flush after the event must carry exactly 1 row update —
    // O(changed_widgets), not O(total_entities).
    let first_flush_count = counts[0];
    assert_eq!(
        first_flush_count, 1,
        "sbc_bridge: flush must update exactly 1 row for a single-entity state change \
         (visible_widgets={VISIBLE_WIDGETS}, total_entities={TOTAL_ENTITIES}); \
         got {first_flush_count} row updates. \
         Flush cost must scale with visible widgets, not total entities."
    );

    eprintln!(
        "sbc_bridge: flush_counts={counts:?} visible_widgets={VISIBLE_WIDGETS} \
         total_entities={TOTAL_ENTITIES}"
    );
}
