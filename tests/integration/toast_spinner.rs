//! Integration tests for the toast banner + per-tile pending spinner
//! (TASK-067).
//!
//! These tests exercise the production code paths end-to-end through the
//! [`HeadlessRenderer`] from `tests/common/slint_harness.rs` (TASK-074):
//!
//! * Toast event arrives → driver writes the formatted text to the
//!   [`MainWindow`] → captured pixels include the toast's distinctive
//!   chrome (the pre-toast frame and post-toast frame must differ).
//! * Auto-dismiss path: render after the dismiss interval → the toast is
//!   no longer in the rendered frame.
//! * `BackpressureRejected` end-to-end: the dispatcher's
//!   `mpsc::Sender<ToastEvent>` is the production seam; we drive it with
//!   a real `BackpressureRejected` event and assert the toast becomes
//!   visible (proves the cross-task wiring).
//! * Spinner visibility binds to `LiveStore::pending_for_widget`
//!   (`locked_decisions.pending_state_read_api`): inserting an
//!   `OptimisticEntry` flips `pending_for_widget` to `true`, the spinner
//!   refresh updates the tile model row, and the rendered frame differs;
//!   dropping the entry flips it back, the row updates, and the frame
//!   reverts.
//!
//! # Determinism notes
//!
//! * `ToastDriver::poll_with_now` is used in lieu of wall-clock `sleep`
//!   so tests do not block for 4 seconds. The `Instant` argument lets us
//!   advance the dismiss clock arbitrarily.
//! * The harness's `OnceLock` per-thread platform install is shared with
//!   other integration tests; each test fetches a fresh `HeadlessRenderer`
//!   handle, which is idempotent on the current thread.

#![allow(clippy::cast_possible_truncation)]

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
    apply_pending_for_widgets, apply_to_window, format_toast_message, install_test_tiles,
    ToastDriver, ToastState,
};

use super::slint_harness::HeadlessRenderer;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Build a Slint-typed `LightTileVM` with `pending = false` for tests.
/// Production code uses [`hanui::ui::bridge::wire_window`] which goes
/// through the icon-resolver; we want a tile we can inspect without any
/// SVG decode side effects.
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

fn make_widget_map(pairs: &[(&str, &str)]) -> WidgetActionMap {
    let mut map = WidgetActionMap::new();
    for (widget_id, entity_id) in pairs {
        map.insert(
            WidgetId::from(*widget_id),
            WidgetActionEntry {
                entity_id: EntityId::from(*entity_id),
                tap: Action::None,
                hold: Action::None,
                double_tap: Action::None,
            },
        );
    }
    map
}

fn make_optimistic_entry(entity_id: &str) -> OptimisticEntry {
    OptimisticEntry {
        entity_id: EntityId::from(entity_id),
        request_id: 1,
        dispatched_at: Timestamp::UNIX_EPOCH,
        tentative_state: Arc::from("on"),
        prior_state: Arc::from("off"),
    }
}

// ---------------------------------------------------------------------------
// Toast pixel-render tests
// ---------------------------------------------------------------------------

#[test]
fn pushing_toast_event_changes_rendered_frame() {
    // Acceptance: a `ToastEvent` flowing through the driver must produce
    // a visibly-different frame because the toast pill is composited on
    // top of the window. We do not assert pixel-perfect contents — we
    // assert the frame differs from the pre-toast baseline (proves the
    // toast actually composited) and that `toast-visible` reads true.
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    // Baseline render with no toast.
    let frame_before = harness
        .render_component(&window, 480, 600)
        .expect("render baseline frame");

    // Drive a toast event through the driver and apply to the window.
    let (tx, rx) = mpsc::channel::<ToastEvent>(4);
    let mut driver = ToastDriver::new(ToastState::new(), rx);
    tx.try_send(ToastEvent::BackpressureRejected {
        entity_id: EntityId::from("light.kitchen"),
        scope: BackpressureScope::PerEntity,
    })
    .expect("send toast event");
    let changed = driver.poll_with_now(Instant::now());
    assert!(changed, "driver must report state change after event");
    apply_to_window(driver.state(), &window);
    assert!(window.get_toast_visible(), "toast-visible must be true");

    let frame_with_toast = harness
        .render_component(&window, 480, 600)
        .expect("render frame with toast");

    assert_eq!(
        frame_before.pixels.len(),
        frame_with_toast.pixels.len(),
        "frames must be the same dimensions"
    );
    assert!(
        frame_before.pixels != frame_with_toast.pixels,
        "rendering with a visible toast must produce different pixels (proves the overlay composites)"
    );
}

#[test]
fn auto_dismiss_clears_toast_after_dismiss_ms() {
    // Acceptance: locked_decisions.toast_behavior — auto-dismiss after
    // toast_dismiss_ms = 4000ms (we use a short value here for test
    // determinism). After ticking past the deadline, the toast must
    // disappear; the post-dismiss frame must equal the pre-toast frame
    // (the toast composite is no longer present).
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    let frame_before = harness
        .render_component(&window, 480, 600)
        .expect("render baseline frame");

    // Use a 100 ms dismiss interval so the test does not wait 4 seconds.
    let (tx, rx) = mpsc::channel::<ToastEvent>(4);
    let mut driver = ToastDriver::new(ToastState::with_dismiss_ms(100), rx);
    tx.try_send(ToastEvent::BackpressureRejected {
        entity_id: EntityId::from("light.kitchen"),
        scope: BackpressureScope::PerEntity,
    })
    .expect("send toast event");

    let now = Instant::now();
    driver.poll_with_now(now);
    apply_to_window(driver.state(), &window);
    assert!(window.get_toast_visible());

    // Advance past the dismiss threshold.
    let later = now + Duration::from_millis(150);
    let cleared = driver.poll_with_now(later);
    assert!(cleared);
    apply_to_window(driver.state(), &window);
    assert!(
        !window.get_toast_visible(),
        "auto-dismiss must flip toast-visible back to false"
    );

    let frame_after = harness
        .render_component(&window, 480, 600)
        .expect("render after auto-dismiss");

    assert_eq!(
        frame_before.pixels, frame_after.pixels,
        "frame after auto-dismiss must equal the pre-toast baseline (no residual composite)"
    );
}

#[test]
fn tap_to_dismiss_clears_toast_immediately() {
    // Acceptance: tap-to-dismiss (locked_decisions.toast_behavior).
    // Simulating a Slint tap on the toast surface fires the
    // `toast-dismissed` callback; the wiring path is "the bridge calls
    // state.dismiss() on that callback". We exercise that path
    // explicitly here — the integration target is the Rust side; the
    // Slint-side TouchArea click is covered by the gesture-layer tests.
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    let (tx, rx) = mpsc::channel::<ToastEvent>(4);
    let mut driver = ToastDriver::new(ToastState::new(), rx);
    tx.try_send(ToastEvent::BackpressureRejected {
        entity_id: EntityId::from("switch.outlet_1"),
        scope: BackpressureScope::PerEntity,
    })
    .expect("send toast event");
    driver.poll_with_now(Instant::now());
    apply_to_window(driver.state(), &window);
    assert!(window.get_toast_visible());

    // User taps → driver clears the state via the public API.
    let cleared = driver.state_mut().dismiss();
    assert!(cleared);
    apply_to_window(driver.state(), &window);
    assert!(!window.get_toast_visible(), "tap-dismiss must hide toast");

    let _ = harness
        .render_component(&window, 480, 600)
        .expect("render after tap-dismiss");
}

#[test]
fn newer_replaces_older_in_visible_pixels() {
    // Acceptance: locked_decisions.toast_behavior — newer replaces older.
    // We write a known LONG message first, render, then write a known
    // SHORT message and render. The pill layout must reflect the newer
    // text — assert the two frames differ AND `get_toast_text` returns
    // the second message verbatim.
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    let (tx, rx) = mpsc::channel::<ToastEvent>(4);
    let mut driver = ToastDriver::new(ToastState::new(), rx);

    // First event: per-entity backpressure on a light.
    tx.try_send(ToastEvent::BackpressureRejected {
        entity_id: EntityId::from("light.aaaaaaa"),
        scope: BackpressureScope::PerEntity,
    })
    .expect("send first toast event");
    driver.poll_with_now(Instant::now());
    apply_to_window(driver.state(), &window);
    let frame_first = harness
        .render_component(&window, 480, 600)
        .expect("render first toast");
    let text_first = window.get_toast_text().to_string();
    assert!(text_first.contains("light.aaaaaaa"));

    // Second event: global backpressure (different message). The global-
    // scope wording adds "system-wide" so the rendered text differs.
    tx.try_send(ToastEvent::BackpressureRejected {
        entity_id: EntityId::from("light.bbbbbbb"),
        scope: BackpressureScope::Global,
    })
    .expect("send second toast event");
    driver.poll_with_now(Instant::now());
    apply_to_window(driver.state(), &window);
    let frame_second = harness
        .render_component(&window, 480, 600)
        .expect("render second toast");
    let text_second = window.get_toast_text().to_string();
    assert_eq!(
        text_second,
        format_toast_message(&ToastEvent::BackpressureRejected {
            entity_id: EntityId::from("light.bbbbbbb"),
            scope: BackpressureScope::Global,
        }),
        "toast-text must reflect the latest event verbatim"
    );
    assert!(
        text_second.contains("system-wide"),
        "global-scope wording must override the previous per-entity wording"
    );

    // Frames must differ — both are showing toasts but with different
    // text; the rendered pixels follow the text.
    assert!(
        frame_first.pixels != frame_second.pixels,
        "rendered frames must differ between two toast events with different text"
    );
}

#[test]
fn navigate_clears_visible_toast() {
    // Acceptance: locked_decisions.toast_behavior — no queue across view
    // changes. After a Navigate outcome (which is the dispatcher's
    // signal that the view changed), the visible toast is cleared
    // regardless of the auto-dismiss clock.
    //
    // Harness must be installed BEFORE `MainWindow::new()` so its
    // custom platform wins the race against any auto-installed backend
    // (the order matters even though we never render in this test).
    let _harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    let (tx, rx) = mpsc::channel::<ToastEvent>(4);
    let mut driver = ToastDriver::new(ToastState::new(), rx);
    tx.try_send(ToastEvent::BackpressureRejected {
        entity_id: EntityId::from("light.kitchen"),
        scope: BackpressureScope::PerEntity,
    })
    .expect("send toast event");
    driver.poll_with_now(Instant::now());
    apply_to_window(driver.state(), &window);
    assert!(window.get_toast_visible());

    // Simulate the view-router path: Navigate → driver clears.
    let cleared = driver.state_mut().clear_for_navigate();
    assert!(cleared);
    apply_to_window(driver.state(), &window);
    assert!(
        !window.get_toast_visible(),
        "Navigate must clear the visible toast even before auto-dismiss fires"
    );
}

// ---------------------------------------------------------------------------
// Spinner-binding pixel tests
// ---------------------------------------------------------------------------

#[test]
fn spinner_visibility_binds_to_pending_for_widget() {
    // Acceptance: per-tile spinner visibility is bound to
    // `LiveStore::pending_for_widget` — the cross-owner read API from
    // TASK-064 (locked_decisions.pending_state_read_api). We populate
    // one light tile, install a `WidgetActionMap` mapping the tile's
    // `widget_id` to its `entity_id`, and assert:
    //
    //   1. With no `OptimisticEntry`, `pending_for_widget` returns false
    //      → spinner is invisible → frame matches a known baseline.
    //   2. After inserting an `OptimisticEntry` for that entity,
    //      `pending_for_widget` returns true → applying the refresh
    //      flips the tile's pending field → frame differs.
    //   3. After dropping the entry, the refresh flips back → frame
    //      returns to the baseline.
    //
    // This exercises the full Risk #14 mitigation chain: TASK-067 only
    // reads `pending_for_widget`; there is no parallel pending-state
    // path.
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    // Build one light tile bound to widget_id "kitchen_light" /
    // entity_id "light.kitchen".
    let widget_id = WidgetId::from("kitchen_light");
    let entity_id = EntityId::from("light.kitchen");

    install_test_tiles(
        &window,
        vec![empty_light_vm("Kitchen", "on")],
        vec![],
        vec![],
    );

    let store = Arc::new(LiveStore::new());
    store.set_widget_action_map(Arc::new(make_widget_map(&[(
        "kitchen_light",
        "light.kitchen",
    )])));

    // Phase 1: no optimistic entry → pending_for_widget is false.
    assert!(
        !store.pending_for_widget(&widget_id),
        "without an OptimisticEntry, pending_for_widget must return false"
    );
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    let frame_idle = harness
        .render_component(&window, 480, 600)
        .expect("render idle frame");

    // Phase 2: insert an OptimisticEntry → pending_for_widget flips
    // true; refresh writes the tile row.
    store
        .insert_optimistic_entry(make_optimistic_entry(entity_id.as_str()))
        .expect("insert optimistic entry");
    assert!(
        store.pending_for_widget(&widget_id),
        "after insert_optimistic_entry, pending_for_widget must return true"
    );
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    let frame_pending = harness
        .render_component(&window, 480, 600)
        .expect("render pending frame");

    assert!(
        frame_idle.pixels != frame_pending.pixels,
        "spinner overlay must produce a different pixel buffer when pending=true \
         (proves the binding fires through pending_for_widget end-to-end)"
    );

    // Phase 3: drop the entry → pending_for_widget flips back; refresh
    // hides the spinner; frame returns to the idle baseline.
    let dropped = store.drop_optimistic_entry(&entity_id, 1);
    assert!(dropped.is_some(), "the entry we just inserted must drop");
    assert!(
        !store.pending_for_widget(&widget_id),
        "after drop_optimistic_entry, pending_for_widget must return false again"
    );
    apply_pending_for_widgets(&window, &store, std::slice::from_ref(&widget_id), &[], &[]);
    let frame_idle_after = harness
        .render_component(&window, 480, 600)
        .expect("render idle-after frame");

    assert_eq!(
        frame_idle.pixels, frame_idle_after.pixels,
        "after dropping the entry the frame must match the pre-pending baseline \
         (the spinner is gone with no residual)"
    );
}

#[test]
fn spinner_does_not_leak_across_unrelated_tiles() {
    // Acceptance: a pending entry on entity_id A must not flip the
    // spinner on a tile bound to entity_id B. This is the per-tile
    // discipline locked into the cross-owner API: each `pending_for_widget`
    // call only consults the entity bound to that specific widget id.
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    let kitchen = WidgetId::from("kitchen_light");
    let bedroom = WidgetId::from("bedroom_light");

    install_test_tiles(
        &window,
        vec![
            empty_light_vm("Kitchen", "on"),
            empty_light_vm("Bedroom", "off"),
        ],
        vec![],
        vec![],
    );

    let store = Arc::new(LiveStore::new());
    store.set_widget_action_map(Arc::new(make_widget_map(&[
        ("kitchen_light", "light.kitchen"),
        ("bedroom_light", "light.bedroom"),
    ])));

    // Insert an OptimisticEntry for kitchen ONLY.
    store
        .insert_optimistic_entry(make_optimistic_entry("light.kitchen"))
        .expect("insert kitchen entry");

    apply_pending_for_widgets(
        &window,
        &store,
        &[kitchen.clone(), bedroom.clone()],
        &[],
        &[],
    );

    // Read back via the Slint model to assert exactly the kitchen row's
    // pending flipped — the bedroom row stays false.
    use slint::Model as _;
    let lights = window.get_light_tiles();
    let kitchen_row = lights.row_data(0).expect("kitchen row exists");
    let bedroom_row = lights.row_data(1).expect("bedroom row exists");
    assert!(
        kitchen_row.pending,
        "the kitchen tile must have pending=true (its entity has an entry)"
    );
    assert!(
        !bedroom_row.pending,
        "the bedroom tile must NOT have pending=true (no entry on its entity) \
         — proves per-tile discipline of pending_for_widget"
    );

    // Render once so the property write is exercised through the
    // platform. The frame is discarded — we already asserted the row
    // values, which is the more precise check.
    let _ = harness
        .render_component(&window, 480, 600)
        .expect("render mixed-pending frame");
}

#[test]
fn backpressure_rejected_event_end_to_end_makes_toast_visible() {
    // Acceptance: a real `ToastEvent::BackpressureRejected` flowing
    // through the dispatcher's `mpsc::Sender<ToastEvent>` and the
    // production driver must produce a visible toast on the window.
    // This is the cross-task wiring proof — TASK-064 emits the event,
    // TASK-067's driver consumes it. Harness installed first so the
    // platform claim wins on this test thread.
    let _harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    // Production seam: tx is what the dispatcher's
    // `with_optimistic_reconciliation(..., toast_tx)` retains.
    let (toast_tx, toast_rx) = mpsc::channel::<ToastEvent>(8);
    let mut driver = ToastDriver::new(ToastState::new(), toast_rx);

    // Simulate the dispatcher firing on a saturated cap.
    toast_tx
        .try_send(ToastEvent::BackpressureRejected {
            entity_id: EntityId::from("light.kitchen"),
            scope: BackpressureScope::PerEntity,
        })
        .expect("dispatcher → driver send must succeed");

    // Driver pumps the event and writes through to the window.
    driver.poll_with_now(Instant::now());
    apply_to_window(driver.state(), &window);

    assert!(
        window.get_toast_visible(),
        "BackpressureRejected from the dispatcher must surface as a visible toast"
    );
    assert_eq!(
        window.get_toast_text().to_string(),
        "Action queue full for `light.kitchen`",
        "the visible toast text must match the formatted event"
    );
}
