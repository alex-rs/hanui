//! Integration tests for the Phase 3 view router (TASK-068).
//!
//! These tests exercise the production [`SlintViewRouter`] against a real
//! `MainWindow` constructed under the headless Slint harness from TASK-074
//! (`tests/common/slint_harness.rs`). The harness installs a
//! `MinimalSoftwareWindow`-based platform once per worker thread; this file
//! reuses that platform via `HeadlessRenderer::new()`.
//!
//! # Acceptance criteria covered
//!
//! 1. `ViewRouterGlobal::current-view` is initialised to `"default"` from
//!    the Slint property declaration alone — no Rust write needed.
//! 2. `Navigate { view_id: "default" }` is observably a no-op — the global
//!    holds `"default"` both before and after the navigate call. No panic.
//! 3. `Navigate { view_id: "unknown" }` does not panic; the global is
//!    updated verbatim. Phase 3 has no Slint readers for `current-view` so
//!    the write is the only observable effect (Phase 4 will populate
//!    readers).
//! 4. The dispatcher invokes the router on `Action::Navigate` and the
//!    Slint-side global picks up the new value end-to-end (with no
//!    `RecordingViewRouter` substitution).
//!
//! # Why an integration test (in addition to the bridge unit tests)
//!
//! `src/ui/view_router.rs::tests` exercises `SlintViewRouter` directly with
//! its own headless platform install. This file re-tests via the canonical
//! `HeadlessRenderer` from `tests/common/slint_harness.rs` — the same harness
//! TASK-073 will use for golden-frame compares — to confirm the production
//! path also works under that platform install ordering. It also adds the
//! end-to-end `Dispatcher → SlintViewRouter → ViewRouterGlobal` test, which
//! the `src/` unit-test module cannot do without pulling integration deps
//! into the library crate.

use hanui::actions::dispatcher::{DispatchOutcome, Dispatcher, Gesture};
use hanui::actions::map::{WidgetActionEntry, WidgetActionMap, WidgetId};
use hanui::actions::Action;
use hanui::ha::entity::EntityId;
use hanui::ha::live_store::LiveStore;
use hanui::ha::services::ServiceRegistry;
use hanui::ui::bridge::{MainWindow, ViewRouterGlobal};
use hanui::ui::view_router::{SlintViewRouter, ViewRouter, DEFAULT_VIEW_ID};
use slint::ComponentHandle;

use super::slint_harness::HeadlessRenderer;

/// Read the current view id from the Slint global.
fn read_current_view(window: &MainWindow) -> String {
    window
        .global::<ViewRouterGlobal>()
        .get_current_view()
        .to_string()
}

/// Construct a `MainWindow` under the harness platform. Returns the harness
/// alongside the window so the caller can keep both alive for the test
/// duration.
fn fresh_window() -> (HeadlessRenderer, MainWindow) {
    let harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");
    (harness, window)
}

#[test]
fn current_view_is_initialised_to_default_without_any_rust_write() {
    // Acceptance #1: the Slint property declaration alone seeds the global
    // with `"default"`. This is the floor for the Phase 3 single-view
    // contract — `Navigate { view_id: "default" }` cannot produce a visible
    // change because the value is already there.
    let (_harness, window) = fresh_window();
    assert_eq!(
        read_current_view(&window),
        DEFAULT_VIEW_ID,
        "ViewRouterGlobal::current-view must be initialised to \"default\" by the Slint \
         property declaration so no Rust seed write is required"
    );
}

#[test]
fn navigate_to_default_does_not_change_visible_state_no_panic() {
    // Acceptance #2: navigate-to-default is observably a no-op. We assert
    // the property value is `"default"` both before AND after the navigate
    // call; the router's write produces no diff. No panic.
    let (_harness, window) = fresh_window();
    assert_eq!(read_current_view(&window), DEFAULT_VIEW_ID);

    let router = SlintViewRouter::new(window.as_weak());
    router.navigate(DEFAULT_VIEW_ID);

    assert_eq!(
        read_current_view(&window),
        DEFAULT_VIEW_ID,
        "navigate-to-default must leave current-view at \"default\" (no visible change)"
    );
}

#[test]
fn navigate_to_unknown_does_not_panic_and_writes_verbatim() {
    // Acceptance #3: navigate-to-unknown does not panic; the Slint global
    // is updated to the new value. Phase 3 has no readers — Phase 4 will
    // bind a `if root.current-view == ...` block. No flicker because no
    // animation depends on `current-view` in Phase 3.
    let (_harness, window) = fresh_window();
    let router = SlintViewRouter::new(window.as_weak());

    router.navigate("kitchen");

    assert_eq!(
        read_current_view(&window),
        "kitchen",
        "the router must write the requested view_id verbatim onto the global"
    );
}

#[test]
fn dispatcher_navigate_action_propagates_to_slint_global() {
    // Acceptance #4: end-to-end Dispatcher -> SlintViewRouter ->
    // ViewRouterGlobal flow. The dispatcher consumes Action::Navigate and
    // the Slint global picks up the new view_id without any test-side
    // shimming. This is the production wiring.
    let (_harness, window) = fresh_window();
    let router = SlintViewRouter::new(window.as_weak());

    let services = std::sync::Arc::new(std::sync::RwLock::new(ServiceRegistry::new()));
    let dispatcher = Dispatcher::new(services).with_view_router(router);

    let entry = WidgetActionEntry {
        entity_id: EntityId::from("light.kitchen"),
        tap: Action::Navigate {
            view_id: DEFAULT_VIEW_ID.to_owned(),
        },
        hold: Action::None,
        double_tap: Action::Navigate {
            view_id: "kitchen".to_owned(),
        },
    };
    let mut map = WidgetActionMap::new();
    map.insert(WidgetId::from("nav-tile"), entry);
    let store = LiveStore::new();

    // Tap → Navigate { view_id: "default" } — observable no-op.
    let outcome = dispatcher
        .dispatch(&WidgetId::from("nav-tile"), Gesture::Tap, &store, &map)
        .expect("Navigate-to-default must dispatch");
    assert!(matches!(outcome, DispatchOutcome::Navigate { .. }));
    assert_eq!(
        read_current_view(&window),
        DEFAULT_VIEW_ID,
        "Navigate-to-default through the dispatcher must leave the global at \"default\""
    );

    // DoubleTap → Navigate { view_id: "kitchen" } — global updates verbatim.
    let outcome = dispatcher
        .dispatch(
            &WidgetId::from("nav-tile"),
            Gesture::DoubleTap,
            &store,
            &map,
        )
        .expect("Navigate-to-kitchen must dispatch");
    assert!(matches!(outcome, DispatchOutcome::Navigate { .. }));
    assert_eq!(
        read_current_view(&window),
        "kitchen",
        "Navigate-to-unknown through the dispatcher must propagate verbatim to the global"
    );

    // And back to default — confirms the dispatcher does not retain state
    // across calls.
    let entry_back = WidgetActionEntry {
        entity_id: EntityId::from("light.kitchen"),
        tap: Action::Navigate {
            view_id: DEFAULT_VIEW_ID.to_owned(),
        },
        hold: Action::None,
        double_tap: Action::None,
    };
    let mut map_back = WidgetActionMap::new();
    map_back.insert(WidgetId::from("nav-tile"), entry_back);
    let outcome = dispatcher
        .dispatch(&WidgetId::from("nav-tile"), Gesture::Tap, &store, &map_back)
        .expect("Navigate-back-to-default must dispatch");
    assert!(matches!(outcome, DispatchOutcome::Navigate { .. }));
    assert_eq!(
        read_current_view(&window),
        DEFAULT_VIEW_ID,
        "Navigate-back-to-default must round-trip through the dispatcher"
    );
}
