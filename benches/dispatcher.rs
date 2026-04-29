//! Per-dispatch CPU micro-bench (TASK-064).
//!
//! # Purpose
//!
//! `docs/backlog/TASK-064.md` mandates a CI-hard CPU gate: each
//! `Dispatcher::dispatch` call must complete in < 5 ms median. The Phase 2
//! churn harness (`benches/churn.rs`) does NOT exercise the dispatcher /
//! optimistic / timer / toast paths, so per-dispatch latency is the
//! mechanical proxy for the ≤25% sustained-CPU constraint at 10 Hz tap
//! cadence (codex review 2026-04-28).
//!
//! # How to run
//!
//! Feature-gated under `bench` so `cargo test` never includes it. Run
//! explicitly:
//!
//! ```text
//! cargo test --features bench --test dispatcher_bench -- --nocapture
//! ```
//!
//! # CI schedule
//!
//! Nightly only; not on every PR. The default `cargo test` invocation
//! omits the `bench` feature, so the bench does not block per-PR latency.
//!
//! # Methodology
//!
//! The bench fixture wires a real `LiveStore` + `Dispatcher`
//! (`with_optimistic_reconciliation`) against a fake
//! `mpsc::Sender<OutboundCommand>` recorder. The recorder is drained in a
//! background tokio task that immediately resolves each `ack_tx` with
//! `Ok(HaAckSuccess)` and then drops the entry via the LiveStore — this
//! keeps the optimistic bucket bounded across the bench's 10 000 iterations
//! so per-entity / global cap pressure is not the dominant cost. Each
//! iteration measures wall-clock latency from `dispatch(...)` entry to
//! return; the assertion at the end is `median < 5 ms`.
//!
//! Iteration count (`ITERATIONS`) is chosen so the median has at least
//! 1000 samples after the warm-up window, giving a stable percentile.
//! `WARMUP` discards the first 1 000 samples to avoid first-allocation
//! and JIT-style cold-cache effects on percentile calculations.
//!
//! The reported PR-body number is the median (50th percentile) and the
//! 99th percentile in microseconds.

#![cfg(feature = "bench")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use tokio::sync::mpsc;

use hanui::actions::dispatcher::{Dispatcher, Gesture, ToastEvent};
use hanui::actions::map::{WidgetActionEntry, WidgetActionMap, WidgetId};
use hanui::actions::timing::ActionTiming;
use hanui::actions::Action;
use hanui::ha::client::{HaAckSuccess, OutboundCommand};
use hanui::ha::entity::{Entity, EntityId};
use hanui::ha::live_store::LiveStore;
use hanui::ha::services::{ServiceMeta, ServiceRegistry};

/// Total dispatches measured. Each dispatch builds an `OutboundCommand`,
/// inserts an `OptimisticEntry`, sends on the recorder channel, and spawns
/// a reconciliation task — that's the full per-dispatch cost we care about.
const ITERATIONS: usize = 10_000;

/// Discarded warm-up samples — the first dispatches pay one-time
/// allocation costs (broadcast sender creation, hashmap rehash) that are
/// not representative of steady-state per-dispatch CPU.
const WARMUP: usize = 1_000;

/// Median latency target per dispatch from `docs/backlog/TASK-064.md`. Acts
/// as the CI-hard gate for the ≤25% sustained-CPU constraint at 10 Hz.
const MAX_MEDIAN_NS: u128 = 5_000_000; // 5 ms

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_per_dispatch_latency_under_5ms_median() {
    // Build the fixture: a real LiveStore + reconciliation-enabled
    // Dispatcher pointed at a fake recorder. The recorder drain loop
    // resolves every ack and drops the optimistic entry so the bucket
    // stays bounded across ITERATIONS.
    let mut reg = ServiceRegistry::new();
    reg.add_service("light", "toggle", ServiceMeta::default());
    let services = Arc::new(std::sync::RwLock::new(reg));

    let store: Arc<LiveStore> = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![Entity {
        id: EntityId::from("light.bench"),
        state: Arc::from("off"),
        attributes: Arc::new(serde_json::Map::new()),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    }]);

    // Channel sized generously so try_send does not back up under burst
    // dispatch — the bench measures dispatcher CPU, not channel pressure.
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<OutboundCommand>(256);
    let (toast_tx, mut toast_rx) = mpsc::channel::<ToastEvent>(64);

    let dispatcher = Dispatcher::with_command_tx(services, cmd_tx).with_optimistic_reconciliation(
        store.clone(),
        ActionTiming::default(),
        toast_tx,
    );

    let mut map = WidgetActionMap::new();
    map.insert(
        WidgetId::from("bench_widget"),
        WidgetActionEntry {
            entity_id: EntityId::from("light.bench"),
            tap: Action::Toggle,
            hold: Action::None,
            double_tap: Action::None,
        },
    );

    // Recorder drain task: resolves each ack as success and drops the
    // entry so the per-entity bucket never saturates. Without this, after
    // ~4 dispatches every subsequent call would short-circuit on
    // BackpressureRejected and the bench would measure the wrong path.
    let store_for_drain = store.clone();
    let drain = tokio::spawn(async move {
        let mut counter: u32 = 0;
        while let Some(cmd) = cmd_rx.recv().await {
            // Resolve ack — the dispatcher's reconciliation task will see
            // success and (combined with state_changed below) drop the
            // optimistic entry.
            let _ = cmd.ack_tx.send(Ok(HaAckSuccess {
                id: counter,
                payload: None,
            }));
            counter = counter.wrapping_add(1);
            // Drop ALL optimistic entries on the bench entity directly so
            // we do not depend on the reconciliation task's timing window
            // for cap relief. This isolates the bench from the rule-2
            // hold-and-revert tail.
            let _ = store_for_drain.drop_all_optimistic_entries(&EntityId::from("light.bench"));
        }
    });

    // Toast drain task — the bench should not produce any toasts (cap
    // never trips), but a missing drain would stall on a full channel.
    let toast_drain = tokio::spawn(async move { while toast_rx.recv().await.is_some() {} });

    let widget_id = WidgetId::from("bench_widget");

    // Warm-up: untimed dispatches to amortize lazy allocations.
    for _ in 0..WARMUP {
        let _ = dispatcher.dispatch(&widget_id, Gesture::Tap, &store, &map);
        // Allow the drain task to clear the pending bucket between calls
        // so the next dispatch sees a clean slate (matches real-world tap
        // cadence at 10 Hz which is 100 ms apart — well past the drain).
        tokio::task::yield_now().await;
    }

    // Measured loop. We measure ONLY the dispatch call (the reconciliation
    // task is spawned synchronously inside dispatch via `tokio::spawn`,
    // which itself is the cost we want included).
    let mut samples_ns: Vec<u128> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let t0 = Instant::now();
        let _ = dispatcher.dispatch(&widget_id, Gesture::Tap, &store, &map);
        samples_ns.push(t0.elapsed().as_nanos());
        // Yield so the drain task can run between iterations and the
        // optimistic bucket stays small. Without the yield the bench
        // measures cap-pressure rather than per-dispatch CPU.
        tokio::task::yield_now().await;
    }

    samples_ns.sort_unstable();
    let median = samples_ns[ITERATIONS / 2];
    let p99 = samples_ns[(ITERATIONS * 99) / 100];
    let p999 = samples_ns[(ITERATIONS * 999) / 1000];
    let max = *samples_ns.last().expect("non-empty samples");
    let mean = samples_ns.iter().sum::<u128>() / (ITERATIONS as u128);

    println!(
        "dispatcher per-dispatch latency over {ITERATIONS} samples: \
         median={:.2}us p99={:.2}us p99.9={:.2}us max={:.2}us mean={:.2}us",
        median as f64 / 1000.0,
        p99 as f64 / 1000.0,
        p999 as f64 / 1000.0,
        max as f64 / 1000.0,
        mean as f64 / 1000.0,
    );

    // CI-hard gate: median < 5 ms (the per-dispatch CPU constraint
    // referenced by `docs/backlog/TASK-064.md` acceptance criteria).
    assert!(
        median < MAX_MEDIAN_NS,
        "TASK-064 CPU gate: per-dispatch median {} ns must be below {} ns (5 ms)",
        median,
        MAX_MEDIAN_NS
    );

    // Shut down the drains by dropping the dispatcher (which drops the
    // last cmd_tx clone). Bench is single-shot so a hard timeout here is
    // sufficient to surface a stuck drain task.
    drop(dispatcher);
    let _ = tokio::time::timeout(Duration::from_secs(5), drain).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), toast_drain).await;
}
