//! UI / golden-frame integration tests for Phase 3 actions (TASK-073).
//!
//! These tests exercise the Phase 3 *visible* outcomes through the headless
//! Slint renderer (`tests/common/slint_harness.rs`, TASK-074). They are the
//! pixel-level companion to `tests/integration/actions_protocol.rs`
//! (TASK-069), which asserts the dispatcher / state-machine transitions
//! without rendering.
//!
//! # Acceptance scope (per TASK-073 ticket)
//!
//! 1. **Optimistic-revert without flicker — golden-frame compare.** The
//!    LOAD-BEARING addition of this ticket. The dispatcher's revert path on
//!    ack-error must surface as a clean two-state sequence (optimistic
//!    state, then prior state); no captured frame in the revert window may
//!    show a third state. A sibling test
//!    (`partial_revert_render_is_distinct_from_both_canonicals`)
//!    deliberately constructs a partial-update frame to prove the no-
//!    flicker test would actually catch a real production flicker
//!    (vacuity defense, opencode review 2026-04-29 BLOCKER #1). See the
//!    `optimistic_revert_*` tests below.
//! 2. **More-info modal lazy-render assertion.** Already covered by
//!    `tests/integration/more_info_modal.rs` (TASK-066) — specifically
//!    `row_builder_is_not_invoked_on_entity_update_while_open` (50 entity
//!    updates with the modal open call `render_rows` exactly once),
//!    `reopen_recomputes_against_current_entity_attributes` (close +
//!    reopen reflects current entity state), and
//!    `modal_overlay_renders_and_visible_state_changes_pixel_buffer`
//!    (open vs. closed produces different rendered pixels). Not
//!    re-tested here.
//! 3. **Toast auto-dismiss + tap-to-dismiss + newer-replaces-older.**
//!    Most lifecycle bullets are covered by
//!    `tests/integration/toast_spinner.rs` (TASK-067):
//!    `pushing_toast_event_changes_rendered_frame` (toast appears →
//!    pixels differ), `auto_dismiss_clears_toast_after_dismiss_ms` (wait
//!    past dismiss → matches baseline),
//!    `tap_to_dismiss_clears_toast_immediately`,
//!    `newer_replaces_older_in_visible_pixels` (two toasts with
//!    intervening renders, the second wins). This file adds the
//!    rapid-succession render-stability extension
//!    (`newer_toast_replaces_older_at_pixel_level_without_intermediate_render`):
//!    pushing two toasts with NO intervening render must produce a frame
//!    pixel-equal to a fresh-state push of just the newer event — proves
//!    the older event leaves no rendered residue. Also adds an
//!    auto-dismiss convergence check
//!    (`toast_auto_dismiss_returns_to_no_toast_baseline_at_pixel_level`)
//!    that re-renders three times after dismiss to guard against a
//!    residual composite leaking back in.
//! 4. **Per-tile pending spinner visibility.** Covered by
//!    `tests/integration/toast_spinner.rs::spinner_visibility_binds_to_pending_for_widget`
//!    (insert / drop OptimisticEntry flips pixels) and
//!    `spinner_does_not_leak_across_unrelated_tiles` (per-tile
//!    discipline of `pending_for_widget`). Not re-tested here.
//!
//! # Golden frame strategy (`tests/fixtures/golden/`)
//!
//! The TASK-073 acceptance criterion mentions
//! `tests/fixtures/golden/`. We deliberately keep the fixtures
//! **in-process and runtime-derived** rather than checked-in PNGs:
//!
//! * Slint's software renderer output depends on the host's font stack and
//!   sub-pixel layout. A disk-stored PNG would either drift across
//!   developer machines (false positives on every CI host) or require a
//!   tolerance threshold that defeats the purpose of a golden compare.
//! * The "no-flicker" assertion is **set-membership**, not byte-for-byte
//!   identity against an external file. Capturing the canonical "off"
//!   frame and the canonical "on" frame at the start of the test gives
//!   us the two valid set members; every captured frame from the revert
//!   window must equal one of those two — never a third buffer.
//! * Determinism is preserved: `TZ=UTC` is the suite-wide convention
//!   (`CLAUDE.md` testing expectations), the harness installs a fresh
//!   `MinimalSoftwareWindow` per worker thread, and the Slint scene we
//!   render holds no time-dependent state (no animations are running
//!   during the captures because we drive the model directly via
//!   `set_row_data` rather than trigger a press).
//!
//! See `docs/plans/2026-04-28-phase-3-actions.md` §
//! `locked_decisions.optimistic_reconciliation_key` for the source of
//! truth on revert semantics, and the plan's Acceptance line
//! "Optimistic-revert golden frame".
//!
//! # Renderer warm-up (opencode review 2026-04-29 BLOCKER #2)
//!
//! The Slint software renderer defers some work — glyph cache
//! population, layout pass — until the first render. The first
//! `take_snapshot()` after `MainWindow::new()` therefore can produce
//! pixels that differ from a second snapshot of the same logical state
//! (cold-cache vs. warm-cache). Every test below that compares captured
//! frames against canonicals first calls [`warm_up_renderer`], which
//! captures and discards a frame to pre-populate the glyph cache so
//! subsequent captures of the same logical state are byte-identical.
//! Without the warm-up, the set-membership assertion would fail
//! spuriously even with zero bugs in the production code.
//!
//! # Test isolation (opencode review 2026-04-29 CONCERN #4)
//!
//! Each `#[test]` constructs its own [`LiveStore`], its own seeded
//! [`MainWindow`], and re-installs the tile model via
//! [`install_test_tiles`] before any captures. The harness's
//! `MinimalSoftwareWindow` is reused across calls on a thread (per-thread
//! `OnceCell` in `tests/common/slint_harness.rs`), but the explicit
//! per-test re-seed of every model property combined with the warm-up
//! render resets the visible scene to the test's intended baseline
//! regardless of what the previous test on the same worker thread did.

use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use tokio::sync::mpsc;

use hanui::actions::dispatcher::{BackpressureScope, ToastEvent};
use hanui::actions::map::{WidgetActionEntry, WidgetActionMap, WidgetId};
use hanui::actions::schema::Action;
use hanui::ha::entity::EntityId;
use hanui::ha::live_store::{LiveStore, OptimisticEntry};
use hanui::ui::bridge::{slint_ui, MainWindow};
use hanui::ui::toast::{
    apply_pending_for_widgets, apply_to_window, install_test_tiles, ToastDriver, ToastState,
};

use super::slint_harness::{CapturedFrame, HeadlessRenderer};

// ---------------------------------------------------------------------------
// Fixture helpers — kept local rather than shared with toast_spinner.rs to
// avoid cross-module test coupling. The shapes are intentionally identical
// to the toast_spinner versions; if the cost of duplication grows, hoist
// to `tests/common/`.
// ---------------------------------------------------------------------------

/// Build a `LightTileVM` with a known state and `pending = false`.
///
/// Production code populates the `icon` field via the asset resolver in
/// `src/assets/`; tests want a tile we can construct without spinning up
/// the SVG decoder, so `icon` is left at its `Image::default()` (an empty
/// image renders nothing — layout stays stable per the `LightTile` Slint
/// component's design).
fn empty_light_vm(name: &str, state: &str) -> slint_ui::LightTileVM {
    slint_ui::LightTileVM {
        name: slint::SharedString::from(name),
        state: slint::SharedString::from(state),
        r#icon_id: slint::SharedString::from(""),
        icon: slint::Image::default(),
        preferred_columns: 2,
        preferred_rows: 2,
        placement: slint_ui::TilePlacement {
            col: 0,
            row: 0,
            span_cols: 2,
            span_rows: 2,
        },
        pending: false,
    }
}

/// Construct a widget action map with a single entry — kept minimal so
/// the dispatcher's downstream lookups have a target without dragging in
/// the dashboard view-spec loader.
fn make_widget_map(widget_id: &str, entity_id: &str) -> WidgetActionMap {
    let mut map = WidgetActionMap::new();
    map.insert(
        WidgetId::from(widget_id),
        WidgetActionEntry {
            entity_id: EntityId::from(entity_id),
            tap: Action::None,
            hold: Action::None,
            double_tap: Action::None,
        },
    );
    map
}

/// Build an `OptimisticEntry` for the `Toggle off → on` case used by the
/// no-flicker test. `prior_state = "off"` and `tentative_state = "on"`
/// match the locked-decision revert contract: ack-error returns the tile
/// to `prior_state`.
fn make_toggle_off_to_on_entry(entity_id: &str) -> OptimisticEntry {
    OptimisticEntry {
        entity_id: EntityId::from(entity_id),
        request_id: 1,
        dispatched_at: Timestamp::UNIX_EPOCH,
        tentative_state: Arc::from("on"),
        prior_state: Arc::from("off"),
    }
}

/// Replace the state of the single light row with `new_state` and apply
/// the pending-spinner refresh, returning a freshly-captured frame.
///
/// Production code applies optimistic state and pending spinner via the
/// dispatcher's reconciliation task feeding the bridge. Tests do the
/// equivalent two-step write directly — `set_row_data` for the state,
/// then `apply_pending_for_widgets` for the spinner overlay — so the
/// rendered frame reflects exactly what a real dispatch would draw.
fn set_light_state(window: &MainWindow, new_state: &str) {
    use slint::Model as _;
    let lights = window.get_light_tiles();
    let mut row = lights
        .row_data(0)
        .expect("seeded light row must exist before mutation");
    row.state = slint::SharedString::from(new_state);
    lights.set_row_data(0, row);
}

/// Capture a frame at the documented test viewport. 480×600 matches
/// `MainWindow`'s `preferred-width`/`preferred-height` and the
/// `slint_harness` smoke test, keeping the rendering footprint
/// consistent across the suite.
fn capture(harness: &mut HeadlessRenderer, window: &MainWindow) -> CapturedFrame {
    harness
        .render_component(window, 480, 600)
        .expect("render frame")
}

/// Pre-populate the Slint software renderer's glyph cache and layout
/// pass by capturing one frame and discarding it. Subsequent captures of
/// the same logical state are then byte-identical (warm-cache pixels
/// only). See module-level "Renderer warm-up" doc-comment for the
/// rationale (opencode review 2026-04-29 BLOCKER #2).
fn warm_up_renderer(harness: &mut HeadlessRenderer, window: &MainWindow) {
    let _ = capture(harness, window);
}

// ---------------------------------------------------------------------------
// 1. Optimistic-revert without flicker — golden-frame compare
//
//    Plan: docs/plans/2026-04-28-phase-3-actions.md
//      § locked_decisions.optimistic_reconciliation_key
//      § Acceptance: "Optimistic-revert golden frame"
//
//    The test simulates the full optimistic-then-revert flow without a
//    live WS roundtrip:
//
//      1. Light tile renders in state "off" (with pending=false).
//         Capture the canonical "off" baseline.
//      2. Optimistic update fires: insert OptimisticEntry, flip the
//         tile's state to "on", apply the pending-spinner refresh.
//         Capture the canonical "on with spinner" frame.
//      3. Ack-ERROR arrives: drop the OptimisticEntry, revert the tile's
//         state back to "off", apply spinner refresh again. Capture a
//         sequence of frames covering the revert window.
//      4. Assert: every captured frame in the revert sequence equals
//         either the canonical "off" baseline or the canonical "on with
//         spinner" frame — NEVER a third pixel buffer.
//
//    The "every captured frame" loop is what makes this load-bearing
//    versus the simpler before/after pair already in toast_spinner.rs:
//    a flicker would be a single transient frame that does not equal
//    either canonical state, and the loop is the only structure that
//    can detect it.
// ---------------------------------------------------------------------------

#[test]
fn optimistic_revert_emits_only_two_distinct_frames_no_flicker() {
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    // Seed: one light tile, state "off", pending=false. Use a known
    // widget_id/entity_id pair so the WidgetActionMap lookup resolves.
    let widget_id = WidgetId::from("kitchen_light");
    let entity_id = EntityId::from("light.kitchen");
    install_test_tiles(
        &window,
        vec![empty_light_vm("Kitchen", "off")],
        vec![],
        vec![],
    );

    let store = LiveStore::new();
    store.set_widget_action_map(Arc::new(make_widget_map(
        widget_id.as_str(),
        entity_id.as_str(),
    )));
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);

    // Warm-up: pre-populate the renderer's glyph cache so subsequent
    // captures of the same logical state are byte-identical (BLOCKER
    // #2 from opencode review).
    warm_up_renderer(&mut harness, &window);

    // ── Step 1: canonical "off" baseline (no pending). ─────────────────
    let canonical_off = capture(&mut harness, &window);

    // ── Step 2: optimistic update — insert entry, flip state, refresh. ─
    // After this the tile renders state "on" AND has the pending spinner
    // visible. This is the canonical "on-with-spinner" frame the
    // dispatcher would draw between dispatch and ack.
    store
        .insert_optimistic_entry(make_toggle_off_to_on_entry(entity_id.as_str()))
        .expect("insert optimistic entry");
    set_light_state(&window, "on");
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    let canonical_on_pending = capture(&mut harness, &window);

    // The two canonical frames MUST differ — if they were equal, the
    // assertion below would be vacuously satisfied by any frame at all.
    assert_ne!(
        canonical_off.pixels, canonical_on_pending.pixels,
        "canonical off and on-with-spinner frames must differ — \
         else the no-flicker check would be vacuous"
    );

    // ── Step 3: revert window — capture frames around the revert.
    //
    // Production atomicity model (verified against `src/lib.rs::SlintSink`
    // and `src/ui/bridge.rs::run_flush_loop`):
    //
    // The dispatcher's reconciliation task (in
    // `src/actions/dispatcher.rs::spawn_reconciliation`, a spawned
    // tokio task) drops the optimistic entry on the `LiveStore`
    // asynchronously. The next `run_flush_loop` tick (~80ms cadence)
    // then rebuilds the entire `Vec<TileVM>` from current store state
    // and calls `SlintSink::write_tiles`, which hops onto the Slint
    // UI thread via `slint::invoke_from_event_loop` and replaces ALL
    // three tile array properties (`set_light_tiles`,
    // `set_sensor_tiles`, `set_entity_tiles`) inside one closure. The
    // entire model swap happens in a single event-loop callback — the
    // Slint renderer therefore never observes a partial mid-rebuild
    // scene; it only ever sees the pre-flush state or the post-flush
    // state.
    //
    // The status-banner property is written via a sibling
    // `invoke_from_event_loop` call (see `src/lib.rs::SlintSink`). The
    // pending-spinner refresh in `src/ui/toast.rs::apply_pending_for_widgets`
    // mutates the existing tile model in place via `set_row_data` —
    // production callers of that function are expected to be on the
    // Slint event-loop thread already (the function is called from
    // bridge code that the Slint runtime invokes synchronously). Each
    // `set_row_data` is itself an atomic property write from the
    // renderer's perspective.
    //
    // The test exercises the same renderer-observable invariant. The
    // three writes (drop_optimistic_entry, set_light_state,
    // apply_pending_for_widgets) execute back-to-back on the test
    // thread WITHOUT an intervening `capture()` call. `capture` is
    // the only thing in this file that triggers a render
    // (`take_snapshot`); without it, the renderer sees only the
    // post-write scene. Captured frames therefore correspond exactly
    // to what production's renderer can ever observe — pre-revert
    // (still optimistic) and post-revert (settled to prior_state).
    //
    // The post-revert dwell loop below captures multiple frames after
    // the writes complete to surface any residual composite that
    // might leak across re-snapshots.
    let mut revert_window: Vec<(&'static str, CapturedFrame)> = Vec::new();

    // (a) Pre-revert capture — still optimistic.
    revert_window.push(("pre-revert", capture(&mut harness, &window)));

    // (b) Atomic revert: ack-error path applies all three writes back-
    // to-back, then captures. This is the production path: bridge
    // polling executes all three in one iteration, then Slint renders.
    let dropped = store.drop_optimistic_entry(&entity_id, 1);
    assert!(dropped.is_some(), "the entry we just inserted must drop");
    set_light_state(&window, "off");
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    revert_window.push(("post-revert", capture(&mut harness, &window)));

    // (c) Post-revert dwell: capture more frames to give any deferred
    // rendering work a chance to surface a residual composite. Each
    // sample is independent — the harness re-snapshots the same window
    // state. Six samples is empirical: the full smoke + integration
    // suite has no scenario where a frame settles in fewer than 1-2
    // re-snapshots, and 6 gives ample margin without inflating runtime.
    for _ in 0..6 {
        revert_window.push(("post-revert-dwell", capture(&mut harness, &window)));
    }

    // ── Step 4: assert no third frame exists in the captured window. ──
    //
    // For each captured frame, it must be byte-equal to either
    // canonical_off OR canonical_on_pending. A third pixel buffer would
    // be a flicker — the visible artefact the locked decision forbids.
    for (label, frame) in revert_window.iter() {
        let matches_off = frame.pixels == canonical_off.pixels;
        let matches_on_pending = frame.pixels == canonical_on_pending.pixels;
        assert!(
            matches_off || matches_on_pending,
            "revert-window frame at '{label}' matches NEITHER canonical \
             state — this is a wrong-state intermediate (flicker) per \
             locked_decisions.optimistic_reconciliation_key"
        );
    }

    // Additionally: the LAST frame must be the off baseline. The revert
    // path settles the tile, and the post-revert sequence must converge.
    let (last_label, last_frame) = revert_window
        .last()
        .expect("revert window has at least one frame");
    assert_eq!(
        last_frame.pixels, canonical_off.pixels,
        "after the revert path completes, the final captured frame \
         (label='{last_label}') must equal the canonical off baseline \
         (state=off, pending=false)"
    );

    // The pre-revert frame must be the on-with-spinner state (we
    // captured it before dropping the optimistic entry).
    assert_eq!(
        revert_window[0].1.pixels, canonical_on_pending.pixels,
        "the first frame of the revert window (label='pre-revert') \
         must equal the canonical on-with-spinner state"
    );
}

/// Negative companion test to `optimistic_revert_emits_only_two_distinct_frames_no_flicker`:
/// proves that the load-bearing test would actually catch a flicker if
/// production introduced one. We deliberately render between the
/// `drop_optimistic_entry` and `set_row_data` writes — the worst-case
/// "WS state has moved but UI hasn't yet" condition that BLOCKER #1
/// asked about — and capture the resulting frame. That frame must
/// differ from BOTH canonical buffers; if the captured frame matched
/// either canonical, our no-flicker test would be unable to detect a
/// real production flicker.
///
/// This is NOT testing production behavior (production batches the
/// three writes within a single polling-loop iteration so the renderer
/// never sees this state). It is testing that the no-flicker test is
/// not vacuous — i.e. that an actual partial-update WOULD be detected
/// as a non-canonical frame. Removes the opencode-review concern that
/// the no-flicker test could pass even if production had a real
/// intermediate render.
#[test]
fn partial_revert_render_is_distinct_from_both_canonicals() {
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    let widget_id = WidgetId::from("kitchen_light");
    let entity_id = EntityId::from("light.kitchen");
    install_test_tiles(
        &window,
        vec![empty_light_vm("Kitchen", "off")],
        vec![],
        vec![],
    );

    let store = LiveStore::new();
    store.set_widget_action_map(Arc::new(make_widget_map(
        widget_id.as_str(),
        entity_id.as_str(),
    )));
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    warm_up_renderer(&mut harness, &window);

    // Canonical off baseline.
    let canonical_off = capture(&mut harness, &window);

    // Canonical on-with-spinner.
    store
        .insert_optimistic_entry(make_toggle_off_to_on_entry(entity_id.as_str()))
        .expect("insert optimistic entry");
    set_light_state(&window, "on");
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    let canonical_on_pending = capture(&mut harness, &window);

    // Force a partial-update frame: drop the entry + refresh the
    // spinner, but leave state="on" on the tile. This simulates a
    // hypothetical production bug where the bridge polling loop split
    // the three writes across two render-eligible iterations. Such a
    // frame shows state="on" but pending=false — visually "on without
    // the spinner indicator", which is neither canonical state.
    let dropped = store.drop_optimistic_entry(&entity_id, 1);
    assert!(dropped.is_some());
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    let partial_frame = capture(&mut harness, &window);

    assert_ne!(
        partial_frame.pixels, canonical_off.pixels,
        "the partial-update frame must NOT equal the canonical off frame \
         (state is still on) — else the no-flicker test could not catch \
         a real production flicker that landed in this state"
    );
    assert_ne!(
        partial_frame.pixels, canonical_on_pending.pixels,
        "the partial-update frame must NOT equal the canonical \
         on-with-spinner frame (spinner is gone) — else the no-flicker \
         test could not catch a real production flicker"
    );
}

#[test]
fn optimistic_revert_window_distinct_pixel_buffers_count_is_at_most_two() {
    // Strengthening of the previous test from "every frame matches one
    // of two known states" to "the SET of distinct pixel buffers in the
    // revert window has cardinality ≤ 2". The two assertions are
    // logically equivalent — if any frame did not match either canonical
    // state, the set would contain at least three members. Stating the
    // invariant both ways is intentional: an evolution of the test that
    // weakens the per-frame check (e.g. adds a tolerance) would break
    // the cardinality check, and vice versa.
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    let widget_id = WidgetId::from("kitchen_light");
    let entity_id = EntityId::from("light.kitchen");
    install_test_tiles(
        &window,
        vec![empty_light_vm("Kitchen", "off")],
        vec![],
        vec![],
    );

    let store = LiveStore::new();
    store.set_widget_action_map(Arc::new(make_widget_map(
        widget_id.as_str(),
        entity_id.as_str(),
    )));

    let mut frames: Vec<CapturedFrame> = Vec::new();

    // Warm-up: pre-populate the renderer's glyph cache so subsequent
    // captures of the same logical state are byte-identical (BLOCKER
    // #2 from opencode review).
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    warm_up_renderer(&mut harness, &window);

    // Off baseline (warm-cache).
    frames.push(capture(&mut harness, &window));

    // Optimistic on-with-spinner.
    store
        .insert_optimistic_entry(make_toggle_off_to_on_entry(entity_id.as_str()))
        .expect("insert optimistic entry");
    set_light_state(&window, "on");
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    frames.push(capture(&mut harness, &window));
    // Re-render to surface any non-determinism in repeat captures of
    // the same logical state.
    frames.push(capture(&mut harness, &window));

    // Ack-error revert.
    store.drop_optimistic_entry(&entity_id, 1);
    set_light_state(&window, "off");
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    frames.push(capture(&mut harness, &window));
    frames.push(capture(&mut harness, &window));

    // Set of distinct pixel buffers across all captures. Hashing
    // 480×600×4 = ~1.15 MB per frame is fast enough at this scale and
    // gives an exact-equality count.
    use std::collections::HashSet;
    let distinct: HashSet<&[u8]> = frames.iter().map(|f| f.pixels.as_slice()).collect();
    assert!(
        distinct.len() <= 2,
        "captured {} distinct pixel buffers across the optimistic-revert \
         flow; locked_decisions.optimistic_reconciliation_key requires at \
         most TWO (off baseline + on-with-spinner). A third buffer is a \
         flicker.",
        distinct.len()
    );
    assert_eq!(
        distinct.len(),
        2,
        "expected exactly two distinct buffers (off + on-with-spinner); \
         a count of 1 means the optimistic update was not observable, \
         which would mean the bridge or the harness is not propagating \
         state writes (regression)"
    );
}

// ---------------------------------------------------------------------------
// 2. Toast renders — the new addition vs. TASK-067's existing tests.
//
// TASK-067's `tests/integration/toast_spinner.rs::newer_replaces_older_in_visible_pixels`
// covers the rendered-pixel-difference between two toasts. The case we add
// here is the "captured between rapid pushes" assertion called out in the
// TASK-073 ticket: push two toasts in immediate succession with NO
// intervening render that could show the older one, then assert the
// captured frame's `toast-text` Slint property reflects exclusively the
// newer event. The rendered frame must also equal the frame produced
// by pushing JUST the second event from a fresh state — proving the older
// event left no residue.
// ---------------------------------------------------------------------------

#[test]
fn newer_toast_replaces_older_at_pixel_level_without_intermediate_render() {
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    // Warm-up: pre-populate the glyph cache so the reference frame and
    // the rapid-succession frame compare cleanly (BLOCKER #2).
    warm_up_renderer(&mut harness, &window);

    // Reference frame: push ONLY the newer event from a fresh driver and
    // capture. This is what the rendered frame must equal in the rapid-
    // succession case if "newer replaces older" actually holds at the
    // pixel level.
    let (tx_ref, rx_ref) = mpsc::channel::<ToastEvent>(4);
    let mut driver_ref = ToastDriver::new(ToastState::new(), rx_ref);
    tx_ref
        .try_send(ToastEvent::BackpressureRejected {
            entity_id: EntityId::from("light.bbbbbbb"),
            scope: BackpressureScope::Global,
        })
        .expect("send reference toast");
    driver_ref.poll_with_now(Instant::now());
    apply_to_window(driver_ref.state(), &window);
    let reference_only_newer = capture(&mut harness, &window);

    // Reset the window's toast properties so the next phase starts
    // from no-toast. `clear_for_navigate` and `dismiss` share the same
    // implementation (per `src/ui/toast.rs::ToastState::clear_for_navigate`),
    // so the reset path is architecturally identical to a tap-dismiss
    // — no risk of residual state diverging between the reset and the
    // production path (opencode review CONCERN #6).
    driver_ref.state_mut().clear_for_navigate();
    apply_to_window(driver_ref.state(), &window);

    // Rapid-succession case: push TWO events back-to-back without any
    // intervening render. The driver drains the backlog in `poll_with_now`,
    // so by construction only the newer is visible in `state()` after the
    // poll. The captured frame must equal the reference frame above —
    // pixel-equal, not "approximately" — proving the older event's text
    // never made it into the rendered output.
    let (tx, rx) = mpsc::channel::<ToastEvent>(4);
    let mut driver = ToastDriver::new(ToastState::new(), rx);
    tx.try_send(ToastEvent::BackpressureRejected {
        entity_id: EntityId::from("light.aaaaaaa"),
        scope: BackpressureScope::PerEntity,
    })
    .expect("send first toast event");
    tx.try_send(ToastEvent::BackpressureRejected {
        entity_id: EntityId::from("light.bbbbbbb"),
        scope: BackpressureScope::Global,
    })
    .expect("send second toast event");
    driver.poll_with_now(Instant::now());
    apply_to_window(driver.state(), &window);
    let rapid_succession = capture(&mut harness, &window);

    assert!(
        window.get_toast_visible(),
        "after rapid-succession push the newer toast must be visible"
    );
    assert!(
        window
            .get_toast_text()
            .to_string()
            .contains("light.bbbbbbb"),
        "rendered toast-text must reflect the NEWER event verbatim"
    );
    assert_eq!(
        rapid_succession.pixels, reference_only_newer.pixels,
        "rendered frame after two-in-a-row push must equal the frame \
         produced by pushing JUST the newer event — proves \
         'newer replaces older' leaves no rendered residue from the \
         older event (locked_decisions.toast_behavior)"
    );
}

#[test]
fn toast_auto_dismiss_returns_to_no_toast_baseline_at_pixel_level() {
    // Convergence sibling to TASK-067's
    // `auto_dismiss_clears_toast_after_dismiss_ms`. That test asserts
    // pixel-equality between the post-dismiss frame and the pre-toast
    // baseline. We add a longer dwell after dismiss to verify the
    // post-dismiss state is STABLE — re-rendering after another small
    // tick advance must keep producing the no-toast baseline. This is
    // the "no residual composite leaks back in" guard.
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    // Warm-up so the baseline and post-dismiss frames are warm-cache
    // pixel-identical (BLOCKER #2).
    warm_up_renderer(&mut harness, &window);

    // Pre-toast baseline.
    let baseline = capture(&mut harness, &window);

    let (tx, rx) = mpsc::channel::<ToastEvent>(4);
    let mut driver = ToastDriver::new(ToastState::with_dismiss_ms(80), rx);
    tx.try_send(ToastEvent::BackpressureRejected {
        entity_id: EntityId::from("light.kitchen"),
        scope: BackpressureScope::PerEntity,
    })
    .expect("send toast event");

    let now = Instant::now();
    driver.poll_with_now(now);
    apply_to_window(driver.state(), &window);
    assert!(window.get_toast_visible());
    let with_toast = capture(&mut harness, &window);
    assert_ne!(
        with_toast.pixels, baseline.pixels,
        "with-toast frame must differ from baseline (sanity check)"
    );

    // Tick well past the 80 ms dismiss threshold.
    driver.poll_with_now(now + Duration::from_millis(120));
    apply_to_window(driver.state(), &window);
    assert!(!window.get_toast_visible());

    // Three samples after dismiss — each must match the baseline. A
    // residual composite (e.g. an overlay stuck visible) would surface
    // as a non-baseline frame anywhere in the loop.
    for sample in 0..3 {
        let post = capture(&mut harness, &window);
        assert_eq!(
            post.pixels, baseline.pixels,
            "post-auto-dismiss sample #{sample} must equal the pre-toast \
             baseline — residual toast composite would be a regression \
             of locked_decisions.toast_behavior"
        );
    }
}

// ---------------------------------------------------------------------------
// Cross-references for scenarios NOT re-tested here
//
// Per the TASK-073 ticket: "Don't duplicate — extend OR cite the existing
// test." The file-level doc-comment above § "Acceptance scope" already
// enumerates the cited tests in `tests/integration/toast_spinner.rs`
// (TASK-067) and `tests/integration/more_info_modal.rs` (TASK-066) by
// name. A reader of this file looking for the lazy-render / tap-dismiss
// / single-spinner assertions follows that doc-comment to the source.
//
// Empty cite-only `#[test]` functions were considered and rejected
// (opencode review 2026-04-29 BLOCKER #3): they would carry no
// behavioural assertions, score zero on mutation testing, and provide
// false confidence in coverage. The module doc-comment is the single
// source of truth for cross-references.
// ---------------------------------------------------------------------------
