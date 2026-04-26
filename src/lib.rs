pub mod assets;
pub mod dashboard;
pub mod ha;
pub mod ui;

use anyhow::Result;
use tracing::info;

/// Orchestration entry point called by `main.rs`.
///
/// Execution order:
/// 1. Tracing subscriber initialised from `RUST_LOG` (default: `info,hanui=debug`).
/// 2. `SLINT_BACKEND=software` set so the window always uses the CPU renderer.
///    If the environment variable is already set (e.g. by the process launcher)
///    the existing value wins; `std::env::set_var` is a no-op on non-empty values.
/// 3. Tokio multi-thread runtime built with `DEFAULT_PROFILE.tokio_workers`
///    threads.  Phase 1 spawns no async tasks; the runtime is held in scope so
///    Phase 2 can call `runtime.handle().spawn(...)` without re-building.
/// 4. Icon cache populated via `icons::init()` (OnceLock — idempotent).
/// 5. Fixture loaded from `examples/ha-states.json` into a `MemoryStore`.
/// 6. Default dashboard view-spec built via `default_dashboard()`.
/// 7. Tile view-models built via `build_tiles(&store, &dashboard)`.
/// 8. Slint `MainWindow` instantiated and wired via `wire_window`.
///    `wire_window` also writes the `AnimationBudget` globals from
///    `DEFAULT_PROFILE`; no duplicate writes here.
/// 9. Optional smoke exit: if `HANUI_EXIT_AFTER_MS` is set to a positive
///    integer, a one-shot timer fires after that many milliseconds and closes
///    the window so automated verification does not block indefinitely.
/// 10. Slint event loop runs on the main thread until the window is closed.
///
/// # Tokio + Slint event-loop interaction
///
/// Slint runs its own event loop via `window.run()`, which blocks the calling
/// thread until the window is closed.  Calling `window.run()` inside a Tokio
/// `block_on` future would block the runtime's current thread while the Slint
/// event loop also tries to park the same thread — a deadlock.
///
/// The pattern here is: build the Tokio runtime, then call `window.run()` on
/// the main thread *outside* any `block_on` block.  The runtime is kept alive
/// by holding the `tokio::runtime::Runtime` value in scope.  Phase 2's
/// WebSocket task will be spawned via `runtime.handle().spawn(...)` before
/// `window.run()` is reached, and the runtime destructor will wait for all
/// spawned tasks to complete after the window closes.
pub fn run() -> Result<()> {
    use dashboard::profiles::DEFAULT_PROFILE;
    use dashboard::view_spec::default_dashboard;
    use ha::fixture;
    use slint::ComponentHandle;
    use ui::bridge::{build_tiles, wire_window, MainWindow};

    // ── 1. Tracing ──────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hanui=debug")),
        )
        .init();

    info!("hanui starting");

    // ── 2. Slint backend ────────────────────────────────────────────────────
    // Set only when not already overridden so the process launcher retains
    // control (e.g. `SLINT_BACKEND=qt` for future Qt-backend testing).
    if std::env::var_os("SLINT_BACKEND").is_none() {
        // SAFETY: single-threaded at this point; Slint has not been called yet.
        // set_var is documented as safe when no other thread is running, which
        // is true here — the Tokio runtime is built after this line.
        unsafe { std::env::set_var("SLINT_BACKEND", "software") };
    }

    // ── 3. Tokio runtime ────────────────────────────────────────────────────
    // Hold the Runtime in scope for the entire duration of run(); the
    // destructor joins all spawned tasks after window.run() returns.
    let _runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(DEFAULT_PROFILE.tokio_workers)
        .enable_all()
        .build()?;

    // ── 4. Icon cache ────────────────────────────────────────────────────────
    assets::icons::init();

    // ── 5. Fixture ───────────────────────────────────────────────────────────
    let store = fixture::load("examples/ha-states.json")?;
    info!(entity_count = ?store_entity_count(&store), "fixture loaded");

    // ── 6. Dashboard ─────────────────────────────────────────────────────────
    let dashboard = default_dashboard();

    // ── 7. Tile view-models ──────────────────────────────────────────────────
    let tiles = build_tiles(&store, &dashboard);
    info!(tile_count = tiles.len(), "tiles built");

    // ── 8. Slint window + property wiring ───────────────────────────────────
    // MainWindow::new() contacts the Slint backend; it must be called after
    // SLINT_BACKEND is set (step 2) and on the thread that will run the event
    // loop (the main thread, here).
    let window = MainWindow::new()?;
    wire_window(&window, &tiles)?;

    // ── 9. Optional smoke exit ───────────────────────────────────────────────
    // If HANUI_EXIT_AFTER_MS is set to a positive integer, close the window
    // after that many milliseconds. This is used for automated verification
    // (VM smoke test, CI screenshot) without hanging indefinitely.
    if let Some(ms) = exit_after_ms() {
        let window_weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(ms), move || {
            if let Some(w) = window_weak.upgrade() {
                w.hide().ok();
            }
        });
    }

    // ── 10. Event loop ────────────────────────────────────────────────────────
    window.run()?;

    info!("hanui exiting");
    Ok(())
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
