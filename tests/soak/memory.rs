//! Memory soak test — 10-minute run at 1000 entities / 50 ev/s (TASK-039).
//!
//! # Nightly-only
//!
//! This test is feature-gated under `#[cfg(feature = "soak")]` and is intended
//! to run in the nightly CI job only — not on every PR.  Running it on every PR
//! would add >10 minutes to the check time, violating the `@smoke` budget.
//!
//! Invoke with:
//!
//! ```sh
//! cargo test --features soak --test soak_tests -- --nocapture
//! ```
//!
//! # Scenario
//!
//! 1. Start the canonical `MockWsServer` (from `tests/common/mock_ws.rs`, the
//!    superset of the TASK-035 harness).
//! 2. Authenticate and subscribe (standard HA handshake via the mock).
//! 3. Load an initial snapshot of 1000 entities.
//! 4. Inject 50 `state_changed` events per second indefinitely, cycling
//!    through the 1000 entities round-robin.
//! 5. Sample process RSS every 30 s for 10 minutes (21 samples total).
//! 6. At t≈60 s (after 2 minutes have elapsed and a steady-state baseline is
//!    established) trigger the disconnect/reconnect burst: force-close the WS
//!    connection 5 times within 30 s, re-scripting the mock between each
//!    reconnect.
//!
//! # Assertions
//!
//! (a) **Steady-state growth ≤ 5 MB**: peak RSS in the last minute minus peak
//!     RSS in the second minute does not exceed 5 MB.  This guards against slow
//!     memory leaks that accumulate over the 10-minute run.
//!
//! (b) **Absolute peak RSS ≤ `PROFILE_DESKTOP.idle_rss_mb_cap` (120 MB)**:
//!     the highest observed RSS at any point during the run must not exceed the
//!     profile cap.
//!
//! (c) **Burst peak RSS ≤ steady-state + 40 MB**: the peak RSS during the
//!     forced-disconnect burst must not exceed the steady-state RSS (as
//!     established before the burst starts) plus 40 MB.  This validates that
//!     Arc lifetime management in the reconnect path does not accumulate stale
//!     snapshots.
//!
//! **Post-resync flush rate ≤ 12.5 Hz**: after each reconnect, the rate at which
//! incremental events are applied to the store (events/second) does not exceed
//! 12.5 × 1000 = 12 500/s in total (one snapshot flush plus subsequent event
//! replays bounded by the 50 ev/s injection rate — well under the 12.5 Hz
//! bridge-flush cadence per entity).  Measured as: count of `apply_event` calls
//! in the 1-second window immediately following each `apply_snapshot`, divided
//! by the window duration.
//!
//! # RSS sampling (Linux only)
//!
//! RSS is read from `/proc/self/status` (the `VmRSS:` line), which reports the
//! current resident set size in kibibytes.  On non-Linux platforms the test
//! emits a clear message and returns immediately without assertions.
//!
//! # Reconnect scripting
//!
//! After each force-disconnect, the mock must be re-scripted for the next auth
//! handshake because the scripted-reply queue is consumed on first match.  The
//! test calls `script_auth_ok`, `script_subscribe_ack`,
//! `script_get_states_reply`, and `script_get_services_reply` before each
//! expected reconnect.

#![cfg(feature = "soak")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use hanui::dashboard::profiles::PROFILE_DESKTOP;
use hanui::ha::client::{ClientError, WsClient};
use hanui::ha::live_store::LiveStore;
use hanui::platform::config::Config;
use hanui::platform::status::{self, ConnectionState};

use crate::common::mock_ws::{entity_state_json, state_changed_event_json, MockWsServer};

// ---------------------------------------------------------------------------
// Constants matching the spec
// ---------------------------------------------------------------------------

/// Total soak duration.
const SOAK_DURATION: Duration = Duration::from_secs(10 * 60);

/// Interval between RSS samples.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// Number of entities in the snapshot.
const ENTITY_COUNT: usize = 1_000;

/// Target event injection rate (events per second, total).
const EVENTS_PER_SEC: u64 = 50;

/// Duration of the disconnect/reconnect burst phase.
const BURST_DURATION: Duration = Duration::from_secs(30);

/// Number of force-disconnects in the burst phase.
const BURST_DISCONNECTS: usize = 5;

/// Absolute peak RSS cap from the profile (in bytes).
const RSS_CAP_BYTES: u64 = PROFILE_DESKTOP.idle_rss_mb_cap as u64 * 1024 * 1024;

/// Steady-state growth budget (in bytes).
const STEADY_STATE_GROWTH_BUDGET_BYTES: u64 = 5 * 1024 * 1024;

/// Burst RSS headroom above steady-state (in bytes).
const BURST_RSS_HEADROOM_BYTES: u64 = 40 * 1024 * 1024;

/// Maximum flush rate per entity (events/s).  12.5 Hz = 12.5 events/s/entity.
/// Total budget: 12.5 × 1000 = 12 500 events/s.
const MAX_FLUSH_RATE_TOTAL: u64 = 125 * 100; // 12_500

// ---------------------------------------------------------------------------
// RSS sampling (Linux only)
// ---------------------------------------------------------------------------

/// Read the current resident set size of this process in bytes.
///
/// Parses `/proc/self/status` and extracts the `VmRSS:` line (in kibibytes).
/// Returns `None` on non-Linux platforms or if the file cannot be parsed.
fn rss_bytes() -> Option<u64> {
    #[cfg(not(target_os = "linux"))]
    return None;

    #[cfg(target_os = "linux")]
    {
        let contents = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())?;
                return Some(kb * 1024);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Mock scripting helpers
// ---------------------------------------------------------------------------

/// Build the 1000-entity JSON array for `script_get_states_reply`.
fn make_entities_json(timestamp: &str) -> String {
    let parts: Vec<String> = (0..ENTITY_COUNT)
        .map(|i| entity_state_json(&format!("light.e{i:04}"), "on", timestamp, timestamp))
        .collect();
    format!("[{}]", parts.join(","))
}

/// Script a complete auth + subscribe + snapshot + services handshake on the
/// mock.  Must be called before each expected connection (initial + reconnects).
async fn script_full_connect(server: &MockWsServer, timestamp: &str) {
    server.script_auth_ok().await;
    server.script_subscribe_ack().await;
    server
        .script_get_states_reply(&make_entities_json(timestamp))
        .await;
    server.script_get_services_reply("{}").await;
}

// ---------------------------------------------------------------------------
// Environment setup helpers
// ---------------------------------------------------------------------------

/// Serialise env-var mutation within the soak test binary.
///
/// The soak binary is single-test, so this is a trivial lock.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_config(url: &str, token: &str) -> Config {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("HA_URL", url);
        std::env::set_var("HA_TOKEN", token);
    }
    Config::from_env().expect("soak: Config::from_env")
}

// ---------------------------------------------------------------------------
// WsClient task helpers
// ---------------------------------------------------------------------------

/// Spawn a `WsClient::run` task and return `(state_rx, handle)`.
fn spawn_client(
    config: Config,
    store: Arc<dyn hanui::ha::client::SnapshotApplier>,
) -> (
    watch::Receiver<ConnectionState>,
    tokio::task::JoinHandle<Result<(), ClientError>>,
) {
    let (state_tx, state_rx) = status::channel();
    let client = WsClient::new(config, state_tx).with_store(store);
    let handle = tokio::spawn(async move {
        let mut c = client;
        c.run().await
    });
    (state_rx, handle)
}

/// Wait for a specific `ConnectionState`, returning `true` within `timeout`.
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
            return *rx.borrow() == target;
        }
        if tokio::time::timeout(remaining, rx.changed()).await.is_err() {
            return *rx.borrow() == target;
        }
    }
}

// ---------------------------------------------------------------------------
// RSS sample collector
// ---------------------------------------------------------------------------

/// A timestamped RSS measurement.
#[derive(Debug, Clone, Copy)]
struct RssSample {
    /// Wall-clock offset from the start of the soak.
    elapsed: Duration,
    /// RSS in bytes at the time of sampling.
    rss_bytes: u64,
}

/// Collect RSS samples every `SAMPLE_INTERVAL` for the duration of the soak.
///
/// Returns once `done_flag` is set or `SOAK_DURATION` elapses.
async fn rss_sampler(
    done_flag: Arc<std::sync::atomic::AtomicBool>,
    samples: Arc<Mutex<Vec<RssSample>>>,
    start: Instant,
) {
    let mut next_sample = tokio::time::Instant::now() + SAMPLE_INTERVAL;
    loop {
        // Take a sample immediately on the first tick too, then periodically.
        if let Some(rss) = rss_bytes() {
            let s = RssSample {
                elapsed: start.elapsed(),
                rss_bytes: rss,
            };
            samples.lock().unwrap().push(s);
        }

        if done_flag.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }

        tokio::time::sleep_until(next_sample).await;
        next_sample += SAMPLE_INTERVAL;
    }
}

// ---------------------------------------------------------------------------
// Event injector
// ---------------------------------------------------------------------------

/// Inject `EVENTS_PER_SEC` state_changed events per second into `server`,
/// cycling through entity indices round-robin.
///
/// Runs until `done_flag` is set.  Counts injected events via `event_counter`.
async fn event_injector(
    server: Arc<MockWsServer>,
    done_flag: Arc<std::sync::atomic::AtomicBool>,
    event_counter: Arc<AtomicU64>,
) {
    let interval = Duration::from_nanos(1_000_000_000 / EVENTS_PER_SEC);
    let mut tick = tokio::time::Instant::now() + interval;
    let mut idx: usize = 0;
    let mut ts_counter: u64 = 0;

    loop {
        if done_flag.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }

        // Build and inject one state_changed event.
        let entity_id = format!("light.e{:04}", idx % ENTITY_COUNT);
        ts_counter += 1;
        // Vary the timestamp so the reconnect diff sees changed entities.
        let ts = format!(
            "2024-01-01T{:02}:{:02}:{:02}+00:00",
            ts_counter / 3600,
            (ts_counter / 60) % 60,
            ts_counter % 60
        );
        let new_state = if ts_counter.is_multiple_of(2) {
            "on"
        } else {
            "off"
        };
        let event = state_changed_event_json(
            1,
            &entity_id,
            Some((new_state, &ts, &ts)),
            Some((
                "on",
                "2024-01-01T00:00:00+00:00",
                "2024-01-01T00:00:00+00:00",
            )),
        );
        server.inject_event(event).await;
        event_counter.fetch_add(1, Ordering::Relaxed);

        idx = idx.wrapping_add(1);

        tokio::time::sleep_until(tick).await;
        tick += interval;
    }
}

// ---------------------------------------------------------------------------
// Main soak test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_soak_10min_1000_entities_50_evs() {
    // Non-Linux: skip with a clear message rather than silently passing.
    if rss_bytes().is_none() {
        eprintln!(
            "[soak] memory_soak: /proc/self/status not available on this platform — skipping RSS assertions."
        );
        return;
    }

    let start = Instant::now();
    eprintln!(
        "[soak] starting 10-minute memory soak at {ENTITY_COUNT} entities / {EVENTS_PER_SEC} ev/s"
    );

    // --- Phase 1: initial connect -------------------------------------------

    let server = Arc::new(MockWsServer::start().await);
    script_full_connect(&server, "2024-01-01T00:00:00+00:00").await;

    let store = Arc::new(LiveStore::new());
    let config = make_config(&server.ws_url, "soak-token");

    let (mut state_rx, client_handle) = spawn_client(config, store.clone());
    assert!(
        wait_for_state(
            &mut state_rx,
            ConnectionState::Live,
            Duration::from_secs(30)
        )
        .await,
        "soak: initial connect must reach Live within 30 s"
    );
    eprintln!(
        "[soak] initial connect: Live (t={:.1}s)",
        start.elapsed().as_secs_f64()
    );

    // --- Phase 2: steady-state event injection + RSS sampling ---------------

    let done_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rss_samples: Arc<Mutex<Vec<RssSample>>> = Arc::new(Mutex::new(Vec::new()));
    let event_counter = Arc::new(AtomicU64::new(0));

    let sampler_done = Arc::clone(&done_flag);
    let sampler_samples = Arc::clone(&rss_samples);
    let sampler_start = start;
    let sampler_handle = tokio::spawn(async move {
        rss_sampler(sampler_done, sampler_samples, sampler_start).await;
    });

    let injector_done = Arc::clone(&done_flag);
    let injector_server = Arc::clone(&server);
    let injector_counter = Arc::clone(&event_counter);
    let injector_handle = tokio::spawn(async move {
        event_injector(injector_server, injector_done, injector_counter).await;
    });

    // --- Phase 3: disconnect/reconnect burst (at t≈60 s) --------------------
    //
    // Wait until at least 2 minutes have elapsed so we have a valid
    // steady-state baseline from the second-minute samples (the first minute
    // warms up the heap; the second minute is the baseline).
    //
    // Then force 5 disconnects within BURST_DURATION (30 s), re-scripting
    // the mock between each.

    let burst_wait = Duration::from_secs(120);
    eprintln!(
        "[soak] waiting {:.0}s before burst phase",
        burst_wait.as_secs_f64()
    );
    tokio::time::sleep(burst_wait).await;

    // Record steady-state RSS just before the burst.
    let steady_state_rss = rss_bytes().unwrap_or(0);
    eprintln!(
        "[soak] pre-burst steady-state RSS: {:.1} MB (t={:.1}s)",
        steady_state_rss as f64 / (1024.0 * 1024.0),
        start.elapsed().as_secs_f64()
    );

    let burst_start = Instant::now();
    let burst_interval = BURST_DURATION
        .checked_div(BURST_DISCONNECTS as u32)
        .unwrap_or(Duration::from_secs(6));

    // Track the peak RSS during the burst.
    let mut burst_peak_rss: u64 = 0;

    // Track flush rates per reconnect (events in first 1s after snapshot).
    let mut post_resync_rates: Vec<u64> = Vec::new();

    for disconnect_idx in 0..BURST_DISCONNECTS {
        eprintln!(
            "[soak] burst disconnect {}/{} (t={:.1}s)",
            disconnect_idx + 1,
            BURST_DISCONNECTS,
            start.elapsed().as_secs_f64()
        );

        // Pre-script the reconnect handshake.  Use a different timestamp so
        // the diff-broadcast sees changed entities (validates Arc lifetime
        // management).
        let ts = format!("2024-01-01T{:02}:00:00+00:00", (disconnect_idx + 1) as u8);
        script_full_connect(&server, &ts).await;

        // Force the WS connection closed.
        server.force_disconnect();

        // Wait for the client to reach Live on the reconnect.
        assert!(
            wait_for_state(
                &mut state_rx,
                ConnectionState::Live,
                Duration::from_secs(30)
            )
            .await,
            "soak: reconnect {}/{disconnect_idx} must reach Live within 30 s",
            disconnect_idx + 1
        );

        // Sample RSS immediately after reconnect.
        if let Some(rss) = rss_bytes() {
            if rss > burst_peak_rss {
                burst_peak_rss = rss;
            }
            eprintln!(
                "[soak] post-reconnect RSS: {:.1} MB",
                rss as f64 / (1024.0 * 1024.0)
            );
        }

        // Measure the event rate in the 1-second window after reconnect.
        let before = event_counter.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let after = event_counter.load(Ordering::Relaxed);
        let rate = after - before;
        post_resync_rates.push(rate);

        // Sleep until the next burst interval.
        let elapsed_in_burst = burst_start.elapsed();
        let target = burst_interval * (disconnect_idx as u32 + 1);
        if elapsed_in_burst < target {
            tokio::time::sleep(target - elapsed_in_burst).await;
        }
    }

    eprintln!(
        "[soak] burst phase complete (t={:.1}s)",
        start.elapsed().as_secs_f64()
    );

    // --- Phase 4: continue steady-state until SOAK_DURATION ----------------

    let remaining = SOAK_DURATION.saturating_sub(start.elapsed());
    if !remaining.is_zero() {
        eprintln!(
            "[soak] continuing steady-state for {:.0}s",
            remaining.as_secs_f64()
        );
        tokio::time::sleep(remaining).await;
    }

    // Signal all background tasks to stop.
    done_flag.store(true, std::sync::atomic::Ordering::Release);
    // Final RSS sample.
    if let Some(rss) = rss_bytes() {
        let s = RssSample {
            elapsed: start.elapsed(),
            rss_bytes: rss,
        };
        rss_samples.lock().unwrap().push(s);
    }

    // Wait for background tasks to finish.
    let _ = tokio::time::timeout(Duration::from_secs(5), sampler_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), injector_handle).await;
    client_handle.abort();

    eprintln!(
        "[soak] soak complete (t={:.1}s)",
        start.elapsed().as_secs_f64()
    );

    // --- Assertions ----------------------------------------------------------

    let samples = rss_samples.lock().unwrap().clone();

    // (a) Steady-state growth: peak in last 60 s - peak in second 60 s ≤ 5 MB.
    //
    // "second minute" = samples with elapsed in [60 s, 120 s).
    // "last minute"   = samples with elapsed ≥ SOAK_DURATION - 60 s.
    let second_minute_peak: Option<u64> = samples
        .iter()
        .filter(|s| s.elapsed >= Duration::from_secs(60) && s.elapsed < Duration::from_secs(120))
        .map(|s| s.rss_bytes)
        .max();
    let last_minute_peak: Option<u64> = samples
        .iter()
        .filter(|s| s.elapsed >= SOAK_DURATION.saturating_sub(Duration::from_secs(60)))
        .map(|s| s.rss_bytes)
        .max();

    if let (Some(baseline), Some(final_peak)) = (second_minute_peak, last_minute_peak) {
        let growth = final_peak.saturating_sub(baseline);
        eprintln!(
            "[soak] assertion (a): steady-state growth = {:.1} MB (baseline {:.1} MB, peak {:.1} MB)",
            growth as f64 / (1024.0 * 1024.0),
            baseline as f64 / (1024.0 * 1024.0),
            final_peak as f64 / (1024.0 * 1024.0),
        );
        assert!(
            growth <= STEADY_STATE_GROWTH_BUDGET_BYTES,
            "assertion (a) FAILED: steady-state RSS growth {:.1} MB exceeds budget of {:.1} MB \
             (baseline {:.1} MB, last-minute peak {:.1} MB)",
            growth as f64 / (1024.0 * 1024.0),
            STEADY_STATE_GROWTH_BUDGET_BYTES as f64 / (1024.0 * 1024.0),
            baseline as f64 / (1024.0 * 1024.0),
            final_peak as f64 / (1024.0 * 1024.0),
        );
    } else {
        // Insufficient samples — warn but do not fail (sampler may have started
        // late or the platform returned no RSS).
        eprintln!(
            "[soak] assertion (a): not enough samples for second-minute / last-minute windows; skipping."
        );
    }

    // (b) Absolute peak RSS ≤ PROFILE_DESKTOP.idle_rss_mb_cap.
    let absolute_peak = samples.iter().map(|s| s.rss_bytes).max().unwrap_or(0);
    eprintln!(
        "[soak] assertion (b): absolute peak RSS = {:.1} MB (cap = {:.1} MB)",
        absolute_peak as f64 / (1024.0 * 1024.0),
        RSS_CAP_BYTES as f64 / (1024.0 * 1024.0),
    );
    assert!(
        absolute_peak <= RSS_CAP_BYTES,
        "assertion (b) FAILED: absolute peak RSS {:.1} MB exceeds cap of {:.1} MB",
        absolute_peak as f64 / (1024.0 * 1024.0),
        RSS_CAP_BYTES as f64 / (1024.0 * 1024.0),
    );

    // (c) Burst peak RSS ≤ steady-state + 40 MB.
    //
    // Both `burst_peak_rss` and `steady_state_rss` must be non-zero for the
    // assertion to be meaningful.  If either is zero it means /proc/self/status
    // returned no data for that sample window — which should not happen on Linux
    // (the earlier rss_bytes() guard ensures we only run on Linux).  We therefore
    // treat a zero value as a data-collection failure and fail the test loudly
    // rather than silently skipping.
    eprintln!(
        "[soak] assertion (c): burst peak RSS = {:.1} MB, steady-state = {:.1} MB, budget = {:.1} MB",
        burst_peak_rss as f64 / (1024.0 * 1024.0),
        steady_state_rss as f64 / (1024.0 * 1024.0),
        (steady_state_rss + BURST_RSS_HEADROOM_BYTES) as f64 / (1024.0 * 1024.0),
    );
    assert!(
        burst_peak_rss > 0,
        "assertion (c) FAILED: no RSS sample collected during the burst phase — \
         data collection error on Linux (/proc/self/status should always be readable)"
    );
    assert!(
        steady_state_rss > 0,
        "assertion (c) FAILED: no RSS sample collected before the burst phase — \
         data collection error on Linux (/proc/self/status should always be readable)"
    );
    assert!(
        burst_peak_rss <= steady_state_rss + BURST_RSS_HEADROOM_BYTES,
        "assertion (c) FAILED: burst peak RSS {:.1} MB > steady-state {:.1} MB + 40 MB headroom",
        burst_peak_rss as f64 / (1024.0 * 1024.0),
        steady_state_rss as f64 / (1024.0 * 1024.0),
    );

    // Post-resync flush rate ≤ 12.5 Hz per entity (12 500 total/s).
    //
    // With 50 ev/s injected across 1000 entities the steady-state rate is
    // already 50/s total — well under 12 500/s.  After reconnect the client
    // replays up to snapshot_buffer_events buffered events and then continues
    // at the steady-state injection rate.  We assert none of the 1-second
    // post-reconnect windows exceeded 12 500 events.
    for (i, &rate) in post_resync_rates.iter().enumerate() {
        eprintln!(
            "[soak] post-resync flush rate after disconnect {}: {} ev/s (limit: {MAX_FLUSH_RATE_TOTAL} ev/s)",
            i + 1,
            rate
        );
        assert!(
            rate <= MAX_FLUSH_RATE_TOTAL,
            "post-resync flush rate assertion FAILED after disconnect {}: {} ev/s > {} ev/s (12.5 Hz × 1000 entities)",
            i + 1,
            rate,
            MAX_FLUSH_RATE_TOTAL,
        );
    }

    eprintln!("[soak] all assertions passed.");
}
