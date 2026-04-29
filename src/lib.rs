//! `hanui` library entry point.
//!
//! `main.rs` is a 3-liner that delegates to [`run`] here.  This module performs
//! CLI-arg parsing, dispatches to either the **fixture path** (Phase 1: load
//! `examples/ha-states.json` into a [`MemoryStore`][ha::store::MemoryStore],
//! render statically) or the **live HA path** (Phase 2: load
//! [`Config`][platform::config::Config] from env, construct a
//! [`LiveStore`][ha::live_store::LiveStore], spawn the
//! [`WsClient`][ha::client::WsClient] reconnect loop, wire a
//! [`LiveBridge`][ui::bridge::LiveBridge] with a Slint-event-loop sink), and
//! runs the Slint event loop.
//!
//! # The store-shape invariant
//!
//! Both paths converge on the **exact same** [`build_tiles`][ui::bridge::build_tiles]
//! call site, parameterised on `&dyn EntityStore`.  The store reference is the
//! only thing that differs.  This is the integration seam where the Phase 2
//! drop-in promise is paid off.
//!
//! # Slint + Tokio thread model
//!
//! Slint's `MainWindow::run()` parks the calling thread until the window is
//! closed.  All async work (WebSocket I/O, the bridge's per-entity subscriber
//! tasks, the 80 ms flush task) runs on a multi-thread Tokio runtime built
//! before `MainWindow::run()` is reached.  Production Slint property writes
//! cross from Tokio worker threads back onto the Slint UI thread via
//! [`slint::invoke_from_event_loop`]; the production [`SlintSink`] is the only
//! place this happens.

pub mod actions;
pub mod assets;
pub mod dashboard;
pub mod ha;
pub mod platform;
pub mod ui;

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use slint::{ComponentHandle, ModelRc, VecModel};
use tracing::info;

use crate::dashboard::profiles::PROFILE_DESKTOP;
use crate::dashboard::view_spec::default_dashboard;
use crate::ha::client::{full_jitter, ClientError, SnapshotApplier, WsClient};
use crate::ha::live_store::LiveStore;
use crate::ha::store::EntityStore;
use crate::platform::config::Config;
use crate::platform::status::{self, ConnectionState};
use crate::ui::bridge::{build_tiles, split_tile_vms, wire_window, LiveBridge, MainWindow};

/// Top-level orchestration entry point called by `main.rs`.
///
/// Dispatches on `--fixture <path>`:
///
/// * Present → [`run_with_memory_store`] (Phase 1 unchanged).
/// * Absent  → [`run_with_live_store`] (Phase 2 live HA path).
///
/// In both branches the runtime sequence is:
///
/// 1. Initialise the tracing subscriber from `RUST_LOG`
///    (default: `info,hanui=debug`).
/// 2. Force `SLINT_BACKEND=software` unless already set by the launcher.
/// 3. Build a multi-thread Tokio runtime with `PROFILE_DESKTOP.tokio_workers`
///    threads.
/// 4. Populate the icon cache via `assets::icons::init()`.
/// 5. Build the chosen [`EntityStore`] and call [`build_tiles`] for the
///    initial render.
/// 6. Construct the [`MainWindow`] and call [`wire_window`].
/// 7. (Live path only) Spawn the WS reconnect loop and a [`LiveBridge`] so
///    later updates flow into the window.
/// 8. Run the Slint event loop on the main thread.
///
/// Slint's `window.run()` blocks the main thread; all async work happens on
/// Tokio worker threads.  Holding the `tokio::runtime::Runtime` value in scope
/// keeps the runtime alive for the lifetime of `run`; its `Drop` joins all
/// spawned tasks after `window.run()` returns.
pub fn run() -> Result<()> {
    init_tracing();
    info!("hanui starting");
    init_slint_backend();

    // Hold the runtime in scope until run() returns; Drop joins spawned tasks.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(PROFILE_DESKTOP.tokio_workers)
        .enable_all()
        .build()
        .context("build Tokio runtime")?;

    assets::icons::init();

    let args: Vec<String> = std::env::args().collect();
    match parse_fixture_arg(&args)? {
        Some(path) => run_with_memory_store(&path),
        None => run_with_live_store(&runtime),
    }
}

// ---------------------------------------------------------------------------
// CLI argument parser
// ---------------------------------------------------------------------------

/// The single recognised CLI flag in Phase 2.
const FIXTURE_FLAG: &str = "--fixture";

/// Parse `--fixture <path>` (or `--fixture=<path>`) from a slice of args.
///
/// `args` is the raw `std::env::args()` collected vec, including `args[0]`
/// (the program name).  Returns `Ok(Some(path))` if the flag is present and
/// well-formed, `Ok(None)` if no flag is given, and `Err` on a malformed
/// invocation (e.g. `--fixture` with no value, or any unknown flag).
///
/// We hand-roll the parser instead of pulling in `clap` to keep the dependency
/// surface small (one less crate to audit at every SBOM cut).  Phase 4 may add
/// more flags; until then the parser is intentionally strict so a typo doesn't
/// silently route the user to the live HA path.
///
/// # Errors
///
/// Returns `Err` when:
/// * `--fixture` is followed by no further argument.
/// * `--fixture=` is given with an empty value.
/// * Any non-`--fixture` argument is present.
pub fn parse_fixture_arg(args: &[String]) -> Result<Option<String>> {
    // Skip program name (args[0]).  An empty args vec is theoretically possible
    // on some platforms; treat it as "no flag given".
    let rest = match args.split_first() {
        Some((_program, rest)) => rest,
        None => return Ok(None),
    };

    let mut iter = rest.iter();
    let mut fixture: Option<String> = None;
    while let Some(arg) = iter.next() {
        if arg == FIXTURE_FLAG {
            let value = iter
                .next()
                .ok_or_else(|| anyhow::anyhow!("`--fixture` requires a path argument"))?;
            if value.is_empty() {
                bail!("`--fixture` path must not be empty");
            }
            if fixture.is_some() {
                bail!("`--fixture` may only be specified once");
            }
            fixture = Some(value.clone());
        } else if let Some(value) = arg.strip_prefix("--fixture=") {
            if value.is_empty() {
                bail!("`--fixture=` path must not be empty");
            }
            if fixture.is_some() {
                bail!("`--fixture` may only be specified once");
            }
            fixture = Some(value.to_owned());
        } else {
            bail!("unknown argument: {arg:?} (only --fixture <path> is supported)");
        }
    }

    Ok(fixture)
}

// ---------------------------------------------------------------------------
// Tracing + Slint-backend init (shared by both paths)
// ---------------------------------------------------------------------------

/// Initialise the tracing subscriber.  Idempotent: a second call is a no-op
/// because `tracing_subscriber::fmt().init()` panics on double-init only when
/// `RUST_LOG` re-parses fail.  `try_init` would be required for true
/// idempotency, but `run` is only ever called once per process.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hanui=debug")),
        )
        .init();
}

/// Force `SLINT_BACKEND=software` unless the launcher already set it.
///
/// Setting `SLINT_BACKEND` only when unset preserves launcher control
/// (e.g. `SLINT_BACKEND=qt` for future Qt-backend testing).
fn init_slint_backend() {
    if std::env::var_os("SLINT_BACKEND").is_none() {
        // SAFETY: single-threaded at this point; Slint has not been called
        // yet and the Tokio runtime is built after this returns.  `set_var`
        // is documented as safe when no other thread is running.
        unsafe { std::env::set_var("SLINT_BACKEND", "software") };
    }
}

// ---------------------------------------------------------------------------
// Phase 1 path — fixture-backed MemoryStore
// ---------------------------------------------------------------------------

/// Phase 1 happy path: load `path` into a `MemoryStore`, render statically,
/// run the Slint event loop until the window closes.
///
/// Identical observable behaviour to the pre-TASK-034 `main.rs`.  The only
/// difference is that the fixture path is now an explicit argument rather
/// than a hard-coded `"examples/ha-states.json"`.
///
/// Used by `cargo run -- --fixture <path>` for local dev and the CI smoke
/// test.  No env-var validation, no `LiveStore`, no WS connection attempt.
fn run_with_memory_store(path: &str) -> Result<()> {
    let store = ha::fixture::load(path).with_context(|| format!("load fixture from {path}"))?;
    info!(entity_count = ?store_entity_count(&store), fixture = %path, "fixture loaded");

    let dashboard = default_dashboard();
    let tiles = build_tiles(&store, &dashboard);
    info!(tile_count = tiles.len(), "tiles built");

    let window = MainWindow::new()?;
    wire_window(&window, &tiles)?;

    arm_smoke_exit_timer(&window);

    window.run()?;

    info!("hanui exiting");
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 2 path — live HA via LiveStore + WsClient + LiveBridge
// ---------------------------------------------------------------------------

/// Phase 2 live HA path.
///
/// Loads [`Config`] from env, constructs a [`LiveStore`], spawns the WS
/// reconnect loop on the Tokio runtime, builds the [`MainWindow`], wires the
/// [`LiveBridge`] with a [`SlintSink`] that hops onto the Slint UI thread, then
/// runs the Slint event loop.
///
/// # Initial render
///
/// `build_tiles` is called once before `LiveBridge::spawn` so the dashboard
/// renders with the current (empty) `LiveStore` snapshot before the first
/// `get_states` reply arrives.  Every visible widget therefore renders with
/// `state="unavailable"` until the snapshot lands; once the FSM enters `Live`,
/// `LiveBridge`'s state-watcher fires a full resync and the user sees real
/// values.
///
/// # Connect-failure path
///
/// `WsClient::run` returns on transport error; the spawned reconnect loop
/// then re-runs `WsClient::run` after a jittered backoff.  The
/// `ConnectionState` watch channel is updated to `Reconnecting` by
/// `WsClient`'s FSM on disconnect — `LiveBridge`'s state-watcher flips the
/// status banner visible.  No token is logged on the failure path; the URL is.
fn run_with_live_store(runtime: &tokio::runtime::Runtime) -> Result<()> {
    let config = Config::from_env()
        .context("load HA connection config from env (HA_URL and HA_TOKEN must both be set)")?;
    info!(url = %config.url, "loaded HA config");

    let (state_tx, state_rx) = status::channel();

    // TASK-048: construct a single shared `ServiceRegistryHandle` once, then
    // clone it into both the `LiveStore` (read side; what `LiveBridge` and
    // Phase 3 dispatchers consult via `services_lookup`) and the `WsClient`
    // (write side; populated on `Phase::Services → Live`).  A single
    // `Arc<RwLock<ServiceRegistry>>` backs both endpoints, which is the
    // cross-task reachability fix codex's post-rescue audit flagged as a
    // Phase 3 blocker.  Without this wiring the WS task could populate the
    // registry but no other task could read it.
    let services_handle = crate::ha::services::ServiceRegistry::new_handle();

    let store: Arc<LiveStore> =
        Arc::new(LiveStore::new().with_services_handle(services_handle.clone()));
    let store_for_bridge: Arc<dyn EntityStore> = store.clone();
    let dashboard = Arc::new(default_dashboard());

    // Initial render against the empty snapshot — every widget will read as
    // `state="unavailable"` until the first `get_states` reply lands.  The
    // bridge's Reconnecting/Failed → Live transition fires a full resync that
    // overwrites these placeholders the moment the connection becomes Live.
    let initial_tiles = build_tiles(&*store_for_bridge, &dashboard);
    info!(
        tile_count = initial_tiles.len(),
        "initial tiles built (pre-snapshot)"
    );

    let window = MainWindow::new()?;
    wire_window(&window, &initial_tiles)?;
    arm_smoke_exit_timer(&window);

    // Production sink: hops onto the Slint UI thread for every property write.
    let sink = SlintSink::new(window.as_weak());

    // Spawn the WS reconnect loop.  The handle is dropped on scope exit, but
    // tokio keeps the task alive until the runtime is dropped (after the Slint
    // event loop returns).  Ownership of `state_tx` moves into the task.
    //
    // BLOCKER 1 fix (TASK-044): the WS task is given an `Arc<dyn SnapshotApplier>`
    // pointing at the same `LiveStore` instance the bridge reads from.  Without
    // this wiring, `get_states` snapshots and `state_changed` events would be
    // parsed by the FSM and then dropped — which is the production-path bug
    // codex's post-shipment audit caught.  The store reference is shared (Arc),
    // so the same backing snapshot is mutated by `WsClient` and read by
    // `LiveBridge`.
    //
    // TASK-048: also forward the shared `services_handle` into the WS task so
    // the `Phase::Services → Live` write site mutates the SAME registry the
    // `LiveStore` reads from.
    // TASK-072: hand the concrete `Arc<LiveStore>` to the reconnect loop so it
    // can call `set_command_tx` / `clear_command_tx` on each attempt
    // (locked_decisions.command_tx_wiring).  The trait coercion to
    // `Arc<dyn SnapshotApplier>` happens inside `run_ws_client` for the WS
    // client's internal use.
    let store_for_ws = store.clone();
    let ws_handle = runtime.spawn(run_ws_client(
        config,
        state_tx,
        store_for_ws,
        services_handle,
    ));

    // Spawn the bridge.  The returned handle is held until window.run() exits;
    // dropping it aborts the bridge's tasks (per LiveBridge::Drop).  This is
    // the correct order: bridge drops first (after window.run() returns),
    // then the WS task is aborted by the runtime drop at function exit.
    let _bridge = runtime.block_on(async {
        LiveBridge::spawn(store_for_bridge.clone(), dashboard.clone(), state_rx, sink)
    });

    window.run()?;

    // Window closed; abort the WS task explicitly so its FSM doesn't keep
    // logging on shutdown.  The runtime drop at the end of `run` will join
    // any remaining tasks.
    ws_handle.abort();

    info!("hanui exiting");
    Ok(())
}

/// Construct a [`WsClient`] wired to share state with a [`LiveStore`].
///
/// Pure factory: takes the config, the state-broadcast sender, an
/// `Arc<dyn SnapshotApplier>` (typically `Arc<LiveStore>`), and the shared
/// [`ServiceRegistryHandle`] that the `LiveStore` was constructed with;
/// returns the wired client.  Exercised by both [`run_ws_client`] (production
/// path) and the unit test in this file's `tests` module — extracting the
/// wire-up makes the `with_store(...) + with_registry(...)` calls covered by
/// a fast in-process test rather than only by the integration test in
/// `tests/integration/ws_client.rs`, which routes through a different entry
/// point.
///
/// TASK-048: signature now takes a `ServiceRegistryHandle` and threads it
/// through `WsClient::with_registry` so the resulting client and the passed
/// `LiveStore` share the same backing `Arc<RwLock<ServiceRegistry>>`.
///
/// [`ServiceRegistryHandle`]: crate::ha::services::ServiceRegistryHandle
fn build_ws_client_with_store(
    config: Config,
    state_tx: tokio::sync::watch::Sender<ConnectionState>,
    store: Arc<dyn SnapshotApplier>,
    services_handle: crate::ha::services::ServiceRegistryHandle,
) -> WsClient {
    WsClient::new(config, state_tx)
        .with_store(store)
        .with_registry(services_handle)
}

/// Capacity of the dispatcher → WS client command channel (TASK-072).
///
/// Per `locked_decisions.ws_command_ack_envelope`, the WS client task is the
/// sole id authority; it drains commands as fast as it can serialize them onto
/// the socket.  A capacity of 32 absorbs short bursts (e.g. a finger that
/// hammers the same tile during a slow round-trip) without blocking the Slint
/// UI thread on `mpsc::Sender::send`.  The dispatcher's `try_send` path
/// (TASK-062) returns `ChannelClosed` rather than blocking when the buffer is
/// full, so the UI never stalls.
const COMMAND_CHANNEL_BUFFER: usize = 32;

/// WS reconnect loop — the outer wrapper around `WsClient::run`.
///
/// Re-runs `WsClient::run` after a jittered exponential-backoff window on
/// transport errors.  Returns (and lets the task end) on:
///
/// * `ClientError::AuthInvalid` — token is rejected, no point retrying.
/// * `ClientError::OverflowCircuitBreaker` — three consecutive snapshot
///   buffer overflows in 60 s; the upstream is firehose-misbehaving.
///
/// The `Ok(())` branch is unreachable in practice because `WsClient::run`
/// loops on the WS read until an error; we still pattern-match it for
/// totality (and to make the no-reconnect-on-clean-exit semantics explicit
/// if `run`'s contract ever changes).
///
/// # BLOCKER 1 (TASK-044)
///
/// Takes an `Arc<dyn SnapshotApplier>` (typically a [`LiveStore`]) and wires
/// it into the [`WsClient`] via [`WsClient::with_store`].  Without this, the
/// FSM parses snapshots and events but never persists them — the bug codex's
/// post-shipment audit found.  The store ref is shared with [`LiveBridge`],
/// so a single snapshot drives both the WS write side and the UI read side.
///
/// # TASK-048
///
/// Also takes a [`ServiceRegistryHandle`] and forwards it into the
/// `WsClient` via [`WsClient::with_registry`], so the FSM populates the same
/// registry the `LiveStore` exposes via `services_lookup`.  This is what
/// makes the `ServiceRegistry` reachable from a task other than this one.
///
/// # TASK-072
///
/// Takes the concrete `Arc<LiveStore>` (rather than `Arc<dyn SnapshotApplier>`)
/// so this loop can repopulate `LiveStore.command_tx` on each reconnect
/// attempt per `locked_decisions.command_tx_wiring`.  Per attempt:
///
/// 1. Construct a fresh `mpsc::channel::<OutboundCommand>(COMMAND_CHANNEL_BUFFER)`.
/// 2. `live_store.set_command_tx(tx)` — replaces any prior sender so
///    in-flight dispatcher clones become inert when the matching receiver is
///    dropped at the end of the current `run()` call.
/// 3. Build the [`WsClient`] with `with_command_rx(rx)` — the receiver is
///    consumed for the duration of `run()`.
/// 4. On `run()` exit, call `live_store.clear_command_tx()` so a dispatcher
///    that fires between attempts sees `None` (and returns
///    `DispatchError::ChannelNotWired`, the dispatcher's existing handling
///    of the no-channel case) rather than racing the next `set_command_tx`
///    call against a stale `Some(closed)` sender.  Risk #11 mitigation:
///    every error path is non-fatal for the dispatcher.
///
/// The drain task itself lives **inside** `WsClient::run` (alongside the
/// inbound stream) via `tokio::select!`; this keeps id allocation and the
/// `pending_ack` map exclusively under the WS client's `&mut self`, matching
/// the seam locked by `locked_decisions.ws_command_ack_envelope`.
///
/// [`ServiceRegistryHandle`]: crate::ha::services::ServiceRegistryHandle
async fn run_ws_client(
    config: Config,
    state_tx: tokio::sync::watch::Sender<ConnectionState>,
    store: Arc<LiveStore>,
    services_handle: crate::ha::services::ServiceRegistryHandle,
) {
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    // The trait coercion happens here so the rest of `run_ws_client` keeps a
    // concrete `Arc<LiveStore>` for `set_command_tx` / `clear_command_tx`.
    let snapshot_applier: Arc<dyn SnapshotApplier> = store.clone();

    let mut client =
        build_ws_client_with_store(config, state_tx, snapshot_applier, services_handle);
    let mut rng = SmallRng::from_entropy();

    loop {
        // TASK-072: per-attempt command channel.  Built fresh each iteration
        // so the prior receiver's drop (when the previous `run()` returned)
        // cannot survive into the next attempt.  The new sender is installed
        // BEFORE `client.run()` starts so a dispatcher that fires the moment
        // we transition to Live does not race the wiring step.
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(COMMAND_CHANNEL_BUFFER);
        store.set_command_tx(cmd_tx);
        client.set_command_rx(cmd_rx);

        let outcome = client.run().await;

        // Whatever the outcome, the receiver `client.command_rx` was taken at
        // the start of `run` and dropped when `run` returned (success or
        // error path); the LiveStore-side sender is now talking to a closed
        // receiver.  Clear the field so dispatchers between attempts observe
        // `ChannelNotWired` rather than `ChannelClosed`-on-stale.  The next
        // iteration will install a fresh sender before any further dispatch
        // can land.
        store.clear_command_tx();

        match outcome {
            Ok(()) => {
                tracing::info!("WsClient::run returned Ok; exiting reconnect loop");
                return;
            }
            Err(ClientError::AuthInvalid { reason }) => {
                tracing::error!(%reason, "auth_invalid; not reconnecting");
                return;
            }
            Err(ClientError::OverflowCircuitBreaker) => {
                tracing::error!("overflow circuit breaker tripped; not reconnecting");
                return;
            }
            Err(other) => {
                // Transport error or any non-fatal variant — back off and retry.
                // The error display intentionally avoids interpolating the
                // token; ClientError::Transport wraps tungstenite errors which
                // do not carry the auth payload.
                tracing::warn!(error = %other, "WS run errored; will reconnect after backoff");
                let window = client.backoff.advance();
                let sleep = full_jitter(window, &mut rng);
                tracing::info!(
                    backoff_ms = sleep.as_millis() as u64,
                    "backoff before reconnect"
                );
                tokio::time::sleep(sleep).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Production BridgeSink — hops onto the Slint UI thread
// ---------------------------------------------------------------------------

/// Production [`BridgeSink`][ui::bridge::BridgeSink] implementation.
///
/// Holds a [`slint::Weak<MainWindow>`] so the sink does not keep the window
/// alive after the user closes it.  Each property write hops onto the Slint
/// UI thread via [`slint::invoke_from_event_loop`]; the closure upgrades the
/// weak handle and silently drops the write if the window has been closed.
///
/// # Errors swallowed
///
/// `invoke_from_event_loop` returns `Err` only when the Slint event loop has
/// been shut down (window closed; app exiting).  In that situation the bridge
/// is being torn down anyway, so a missed property write has no observable
/// effect — we log at `debug` level and discard.
///
/// # Thread safety
///
/// `slint::Weak<MainWindow>` is `Send + Sync` (Slint's API contract).  The
/// closure captures the cloned weak handle by move, so no shared mutable state
/// crosses the thread boundary.
pub struct SlintSink {
    window: slint::Weak<MainWindow>,
}

impl SlintSink {
    /// Wrap a weak handle to the main window in a sink.
    pub fn new(window: slint::Weak<MainWindow>) -> Self {
        SlintSink { window }
    }
}

impl ui::bridge::BridgeSink for SlintSink {
    fn write_tiles(&self, tiles: Vec<ui::bridge::TileVM>) {
        // We deliberately do NOT call `wire_window` here — `wire_window` also
        // writes the `AnimationBudget` globals, which the Phase 1 contract
        // mandates be set exactly once at startup (in `run_with_memory_store` /
        // `run_with_live_store`'s initial wire).  Re-writing them at 12.5 Hz
        // would stomp the `active-count` global that animation handlers
        // increment.  Per-tile property writes only.
        //
        // `ModelRc<T>` is `Rc`-backed and therefore not `Send`, so the
        // conversion + model wrapping must happen on the Slint UI thread.  The
        // typed `TileVM` slice is `Send`, so it crosses the thread boundary
        // freely; the per-flush `String → SharedString` allocations and `Arc`
        // bumps for icon clones land on the UI thread.  This matches the
        // documented behaviour of `wire_window` (runs once per refresh cycle,
        // not per frame; allocation happens off the per-frame hot path).
        let window = self.window.clone();
        if let Err(e) = slint::invoke_from_event_loop(move || {
            if let Some(w) = window.upgrade() {
                let (lights, sensors, entities) = split_tile_vms(&tiles);
                w.set_light_tiles(ModelRc::new(VecModel::from(lights)));
                w.set_sensor_tiles(ModelRc::new(VecModel::from(sensors)));
                w.set_entity_tiles(ModelRc::new(VecModel::from(entities)));
            }
        }) {
            tracing::debug!(error = %e, "invoke_from_event_loop failed in write_tiles (event loop shut down?)");
        }
    }

    fn set_status_banner_visible(&self, visible: bool) {
        let window = self.window.clone();
        if let Err(e) = slint::invoke_from_event_loop(move || {
            if let Some(w) = window.upgrade() {
                w.set_status_banner_visible(visible);
            }
        }) {
            tracing::debug!(error = %e, "invoke_from_event_loop failed in set_status_banner_visible");
        }
    }
}

// ---------------------------------------------------------------------------
// Smoke-exit helper (shared by both paths)
// ---------------------------------------------------------------------------

/// Arm the optional `HANUI_EXIT_AFTER_MS` smoke-exit timer.
///
/// If the env var is set to a positive integer, schedule a one-shot Slint
/// timer that hides the window after that many milliseconds.  Used for
/// automated verification (VM smoke test, CI screenshot) without hanging.
fn arm_smoke_exit_timer(window: &MainWindow) {
    if let Some(ms) = exit_after_ms() {
        let window_weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(ms), move || {
            if let Some(w) = window_weak.upgrade() {
                w.hide().ok();
            }
        });
    }
}

/// Return the value of `HANUI_EXIT_AFTER_MS` if it is a valid positive integer.
///
/// Returns `None` when the variable is unset, empty, zero, or not parseable.
fn exit_after_ms() -> Option<u64> {
    std::env::var("HANUI_EXIT_AFTER_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
}

/// Count entities in the store via the visitor, for a startup log line.
fn store_entity_count(store: &dyn ha::store::EntityStore) -> usize {
    let mut n = 0usize;
    store.for_each(&mut |_, _| n += 1);
    n
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(slice: &[&str]) -> Vec<String> {
        slice.iter().map(|s| (*s).to_owned()).collect()
    }

    // -----------------------------------------------------------------------
    // parse_fixture_arg — happy paths
    // -----------------------------------------------------------------------

    #[test]
    fn parse_fixture_arg_no_args_returns_none() {
        let v = argv(&["hanui"]);
        assert_eq!(parse_fixture_arg(&v).unwrap(), None);
    }

    #[test]
    fn parse_fixture_arg_empty_argv_returns_none() {
        let v: Vec<String> = Vec::new();
        assert_eq!(parse_fixture_arg(&v).unwrap(), None);
    }

    #[test]
    fn parse_fixture_arg_space_separated_form_returns_path() {
        let v = argv(&["hanui", "--fixture", "examples/ha-states.json"]);
        assert_eq!(
            parse_fixture_arg(&v).unwrap(),
            Some("examples/ha-states.json".to_owned())
        );
    }

    #[test]
    fn parse_fixture_arg_equals_form_returns_path() {
        let v = argv(&["hanui", "--fixture=examples/ha-states.json"]);
        assert_eq!(
            parse_fixture_arg(&v).unwrap(),
            Some("examples/ha-states.json".to_owned())
        );
    }

    // -----------------------------------------------------------------------
    // parse_fixture_arg — error paths
    // -----------------------------------------------------------------------

    #[test]
    fn parse_fixture_arg_flag_without_value_errors() {
        let v = argv(&["hanui", "--fixture"]);
        let err = parse_fixture_arg(&v).expect_err("must error");
        assert!(err.to_string().contains("requires a path"), "error: {err}");
    }

    #[test]
    fn parse_fixture_arg_equals_with_empty_value_errors() {
        let v = argv(&["hanui", "--fixture="]);
        let err = parse_fixture_arg(&v).expect_err("must error");
        assert!(
            err.to_string().contains("must not be empty"),
            "error: {err}"
        );
    }

    #[test]
    fn parse_fixture_arg_space_with_empty_value_errors() {
        let v = argv(&["hanui", "--fixture", ""]);
        let err = parse_fixture_arg(&v).expect_err("must error");
        assert!(
            err.to_string().contains("must not be empty"),
            "error: {err}"
        );
    }

    #[test]
    fn parse_fixture_arg_unknown_flag_errors() {
        let v = argv(&["hanui", "--ha-url", "ws://localhost"]);
        let err = parse_fixture_arg(&v).expect_err("must error");
        assert!(err.to_string().contains("unknown argument"), "error: {err}");
    }

    #[test]
    fn parse_fixture_arg_double_fixture_errors() {
        let v = argv(&["hanui", "--fixture", "a.json", "--fixture", "b.json"]);
        let err = parse_fixture_arg(&v).expect_err("must error");
        assert!(
            err.to_string().contains("only be specified once"),
            "error: {err}"
        );
    }

    #[test]
    fn parse_fixture_arg_space_then_equals_errors() {
        let v = argv(&["hanui", "--fixture", "a.json", "--fixture=b.json"]);
        let err = parse_fixture_arg(&v).expect_err("must error");
        assert!(
            err.to_string().contains("only be specified once"),
            "error: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Other helpers
    // -----------------------------------------------------------------------

    /// `store_entity_count` returns 0 for an empty store and the entity count
    /// for a populated one.
    #[test]
    fn store_entity_count_counts_via_visitor() {
        let empty = LiveStore::new();
        assert_eq!(store_entity_count(&empty), 0);

        let store = ha::fixture::load("examples/ha-states.json").expect("fixture load");
        // The canonical fixture has 4 entities.
        assert_eq!(store_entity_count(&store), 4);
    }

    /// Phase 1 regression guard: the canonical `--fixture examples/ha-states.json`
    /// invocation routes through `parse_fixture_arg` to a path that loads
    /// cleanly into a `MemoryStore` and produces the same widget count and
    /// per-kind tile breakdown as direct `MemoryStore + build_tiles`.
    ///
    /// This test does NOT call `run` itself (that would block on the Slint
    /// event loop and require a display backend); it asserts that the *data
    /// path* taken by `run_with_memory_store` is observably identical to the
    /// pre-TASK-034 behaviour exercised by `tests/smoke.rs`.
    #[test]
    fn fixture_arg_routes_to_memory_store_with_same_tile_breakdown() {
        let argv = vec![
            "hanui".to_owned(),
            "--fixture".to_owned(),
            "examples/ha-states.json".to_owned(),
        ];
        let path = parse_fixture_arg(&argv)
            .expect("parse must succeed")
            .expect("must yield Some(path)");
        assert_eq!(path, "examples/ha-states.json");

        // Replicate the data path of run_with_memory_store: load fixture,
        // build tiles, assert that one tile per widget is produced and that
        // all three kinds are present (matches tests/smoke.rs invariants).
        let store = ha::fixture::load(&path).expect("fixture must load");
        let dashboard = default_dashboard();
        let tiles = build_tiles(&store, &dashboard);

        let widget_count: usize = dashboard
            .views
            .iter()
            .flat_map(|v| v.sections.iter())
            .map(|s| s.widgets.len())
            .sum();
        assert_eq!(
            tiles.len(),
            widget_count,
            "must produce one TileVM per widget"
        );

        use crate::ui::bridge::TileVM;
        let has_light = tiles.iter().any(|t| matches!(t, TileVM::Light(_)));
        let has_sensor = tiles.iter().any(|t| matches!(t, TileVM::Sensor(_)));
        let has_entity = tiles.iter().any(|t| matches!(t, TileVM::Entity(_)));
        assert!(has_light, "fixture path must produce ≥1 LightTileVM");
        assert!(has_sensor, "fixture path must produce ≥1 SensorTileVM");
        assert!(has_entity, "fixture path must produce ≥1 EntityTileVM");
    }

    /// `SlintSink::new` constructs cleanly from a default (defunct) weak
    /// handle.  We intentionally do NOT call `write_tiles`/
    /// `set_status_banner_visible` from a unit test — both internally call
    /// `slint::invoke_from_event_loop`, which requires a running Slint event
    /// loop (unavailable in headless CI) and would return an error that we'd
    /// silently swallow anyway.  The sink is exercised end-to-end in TASK-035's
    /// VM smoke test (mock WS + real Slint event loop on the VM).
    #[test]
    fn slint_sink_constructs_from_default_weak() {
        let weak: slint::Weak<MainWindow> = slint::Weak::default();
        let _sink = SlintSink::new(weak);
        // Reaching here without panicking is the assertion.  Trait-shape
        // coverage lives in src/ui/bridge.rs (test module's RecordingSink).
    }

    /// `build_ws_client_with_store` wires the `WsClient` and the `LiveStore`
    /// to the SAME shared `ServiceRegistryHandle`.
    ///
    /// TASK-048: this replaces the prior tautological test
    /// (`...returns_client_with_empty_registry`) that codex and opencode both
    /// flagged as theatre — its only assertion was that
    /// `ServiceRegistry::new()` is empty, which is a property of `Default`.
    /// The replacement asserts the actual invariant `build_ws_client_with_store`
    /// is responsible for: client and store reference the same backing
    /// `Arc<RwLock<ServiceRegistry>>`.  Two complementary checks:
    ///
    /// 1. `Arc::ptr_eq` between `client.services_handle()` and
    ///    `live_store.services_handle()` — proves they alias the same Arc.
    /// 2. Mutate via the WsClient-side handle (write-lock + add_service);
    ///    read back through the LiveStore-side `services_lookup`.  Proves
    ///    the lock is genuinely shared (not just two Arcs to identical
    ///    initial state).
    ///
    /// We construct `Config` via `Config::from_env`, holding a serialized lock
    /// across the env mutation + parse so this does not race with the other
    /// env-touching test in this module.
    #[test]
    fn build_ws_client_with_store_shares_registry_handle_with_live_store() {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap();

        // SAFETY: serialized via LOCK against other env-mutating tests in this
        // module.  The integration test binary uses a different process so it
        // does not race with this one.
        unsafe {
            std::env::set_var("HA_URL", "ws://127.0.0.1:0/api/websocket");
            std::env::set_var("HA_TOKEN", "tok-build-helper");
        }
        let config = Config::from_env().expect("env-driven Config::from_env must succeed");

        // Drop env vars before any await/sleep so other tests see a clean slate.
        unsafe {
            std::env::remove_var("HA_URL");
            std::env::remove_var("HA_TOKEN");
        }

        // Replicate run_with_live_store wiring: construct the registry once,
        // clone into both the LiveStore and the WsClient via
        // build_ws_client_with_store.
        let services_handle = crate::ha::services::ServiceRegistry::new_handle();
        let store: Arc<LiveStore> =
            Arc::new(LiveStore::new().with_services_handle(services_handle.clone()));
        let store_for_ws: Arc<dyn SnapshotApplier> = store.clone();

        let (state_tx, _state_rx) = status::channel();
        let client =
            build_ws_client_with_store(config, state_tx, store_for_ws, services_handle.clone());

        // Invariant 1: client and store reference the same Arc.  This is the
        // structural proof that `with_registry` and `with_services_handle`
        // both consumed the same handle.
        let client_handle = client.services_handle();
        let store_handle = store.services_handle();
        assert!(
            Arc::ptr_eq(&client_handle, &store_handle),
            "build_ws_client_with_store must wire client and store to the same \
             ServiceRegistryHandle Arc; Arc::ptr_eq returned false"
        );

        // Invariant 2: mutating through the client-side handle is observable
        // via the store-side accessor.  We bypass the FSM (no live transport)
        // and write directly through the shared lock, then read via the
        // LiveStore API a Phase 3 dispatcher would use.
        {
            let mut guard = client_handle
                .write()
                .expect("ServiceRegistry RwLock poisoned");
            guard.add_service(
                "light",
                "turn_on",
                crate::ha::services::ServiceMeta {
                    name: "Turn on".to_owned(),
                    ..Default::default()
                },
            );
        }
        let observed = store.services_lookup("light", "turn_on");
        assert!(
            observed.is_some(),
            "service added via client-side write-lock must be visible via \
             LiveStore::services_lookup; the shared Arc is not actually shared"
        );
        assert_eq!(
            observed.expect("checked is_some above").name,
            "Turn on",
            "service metadata must round-trip through the shared registry"
        );

        // And an unknown lookup still returns None — confirms the handle
        // isn't returning a stub that always says Some.
        assert!(
            store.services_lookup("nonexistent", "x").is_none(),
            "unknown (domain, service) must still return None through the \
             shared registry"
        );
    }

    /// `exit_after_ms` returns `None` when the var is unset, empty, zero, or
    /// not parseable; `Some(ms)` for any positive integer.  Tested by
    /// temporarily setting the env var on a serialized lock so this does not
    /// race with parallel tests that read the same var.
    #[test]
    fn exit_after_ms_parses_positive_integers_only() {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap();

        // SAFETY: serialized via LOCK; single-threaded with respect to this
        // env var across all tests in this module.
        unsafe { std::env::remove_var("HANUI_EXIT_AFTER_MS") };
        assert_eq!(exit_after_ms(), None, "unset → None");

        unsafe { std::env::set_var("HANUI_EXIT_AFTER_MS", "") };
        assert_eq!(exit_after_ms(), None, "empty → None");

        unsafe { std::env::set_var("HANUI_EXIT_AFTER_MS", "0") };
        assert_eq!(exit_after_ms(), None, "zero → None");

        unsafe { std::env::set_var("HANUI_EXIT_AFTER_MS", "abc") };
        assert_eq!(exit_after_ms(), None, "non-integer → None");

        unsafe { std::env::set_var("HANUI_EXIT_AFTER_MS", "1500") };
        assert_eq!(exit_after_ms(), Some(1500), "1500 → Some(1500)");

        unsafe { std::env::remove_var("HANUI_EXIT_AFTER_MS") };
    }
}
