//! View router for the Phase 3 single-view dashboard (TASK-068).
//!
//! # Locked decision (`docs/plans/2026-04-28-phase-3-actions.md`,
//! `locked_decisions.view_router`)
//!
//! > `current_view: SharedString` global driven by `Navigate`. Single view in
//! > Phase 3 (default), but the API is wired so Phase 4 does not retrofit
//! > the dispatcher.
//!
//! This module is the Rust-side bridge between the dispatcher's
//! [`crate::actions::dispatcher::DispatchOutcome::Navigate`] payload and the
//! Slint-side [`ViewRouterGlobal::current-view`] property declared in
//! `ui/slint/view_router.slint`. The dispatcher hands a router handle the
//! `view_id` from a successful `Navigate` action; the router writes it onto
//! the global. Phase 3 ships exactly one view (`"default"`); both
//! `Navigate { view_id: "default" }` (the only "real" target) and
//! `Navigate { view_id: <anything-else> }` are no-ops on the rendered UI
//! because no Slint code branches on `current-view` yet. Phase 4 wires
//! multi-view layouts; the router signature does not change.
//!
//! # Why a trait
//!
//! The dispatcher (`src/actions/dispatcher.rs`) is unit-tested without a real
//! Slint window — TASK-062's tests use `RecordingRouter`-style fakes. By
//! exposing a [`ViewRouter`] trait we can:
//!
//! 1. Record navigations in tests against a deterministic recorder.
//! 2. Skip the `slint::Weak` upgrade hop in unit tests (which would require
//!    a live event loop).
//! 3. Plug in a Phase 4 multi-view router without changing the dispatcher
//!    signature (locked_decisions.phase4_forward_compat).
//!
//! # Single-view semantics
//!
//! [`SlintViewRouter::navigate`] writes `view_id` onto `ViewRouterGlobal::current-view`
//! verbatim. Phase 3 always seeds the global with `"default"` (the Slint
//! property declaration has `: "default"`); the dispatcher only ever fires
//! `Navigate { view_id: "default" }` from the in-code action map. A
//! navigate-to-`"default"` write is therefore observably a no-op (the
//! property already holds that value, and no Slint code branches on it).
//!
//! Navigate-to-unknown is treated identically at the wire level: the global
//! is updated, but no Slint code reads the property in Phase 3 so nothing
//! visible changes. A `tracing::debug!` line records the unknown-view
//! attempt so Phase 4 development can spot mis-wired action specs.
//!
//! # Phase 4 forward-compat
//!
//! Phase 4 will add multi-view layouts — likely `if root.current-view ==
//! "kitchen" { ... }` or a `for` loop over a view model. The Rust side does
//! NOT need to change: the `SlintViewRouter::navigate` method already writes
//! whatever `view_id` it receives, and Phase 4 will simply add Slint-side
//! readers and a YAML-driven validator that constrains the `view_id` to a
//! known set before dispatch. The `tracing::debug!` line below is the only
//! Phase 3 visibility into "Navigate to a non-default view"; Phase 4 will
//! tighten that to an explicit `KnownView` validation if needed.
//!
//! ## Phase 4 friction note: double-navigate on outcome forwarding
//!
//! [`crate::actions::dispatcher::Dispatcher`] invokes the router BEFORE
//! returning [`crate::actions::dispatcher::DispatchOutcome::Navigate`]. The
//! global is therefore already updated when the caller sees the outcome.
//! Phase 4 must NOT have a second consumer that interprets the outcome as
//! a "please navigate" instruction — doing so would re-invoke the router
//! (or worse, race a Phase 4 multi-view rerender). The outcome is purely
//! informational ("a navigation just fired"); the authoritative write is
//! the in-dispatcher `router.navigate(view_id)` call. Phase 4's YAML
//! validator that constrains `view_id` to a known set must run BEFORE
//! [`crate::actions::dispatcher::Dispatcher::dispatch`], not after, so the
//! router never sees an invalid id.
//!
//! # Naming
//!
//! Two distinct types share the `View` stem:
//!
//! * [`ViewRouter`] (Rust trait, this module) — the dispatcher-facing
//!   navigate sink.
//! * `ViewRouterGlobal` (Slint global, `ui/slint/view_router.slint`) — the
//!   Slint-side property holder that [`SlintViewRouter`] writes to.
//!
//! The Slint global has a `Global` suffix to make the two unambiguous in
//! grep output and in the bridge re-exports
//! (`pub use slint_ui::{..., ViewRouterGlobal}` in `src/ui/bridge.rs`).

use slint::{ComponentHandle, SharedString};
use tracing::debug;

use crate::ui::bridge::{MainWindow, ViewRouterGlobal};

/// The single Phase 3 view id. Both the Slint global default
/// (`view_router.slint`) and the dispatcher's in-code `Navigate` actions use
/// this string. Phase 4 will populate multi-view configs from YAML; this
/// constant remains the seed value for the global until then.
pub const DEFAULT_VIEW_ID: &str = "default";

// ---------------------------------------------------------------------------
// ViewRouter trait
// ---------------------------------------------------------------------------

/// Routes a navigation request to the active view sink.
///
/// Implementors:
///
/// * [`SlintViewRouter`] — the production wiring. Wraps a
///   `slint::Weak<MainWindow>` and writes `view_id` to the
///   `ViewRouterGlobal::current-view` property.
/// * `RecordingViewRouter` (test-only, in `view_router::tests`) — captures
///   every navigate call into a `Mutex<Vec<String>>` for assertions in
///   dispatcher unit tests.
///
/// The trait is `Send + Sync` so a single router can be cloned into the
/// dispatcher (which is itself `Clone` and shared across tile gesture
/// callbacks per TASK-062).
pub trait ViewRouter: Send + Sync {
    /// Route a navigation request to `view_id`.
    ///
    /// Phase 3 contract:
    ///
    /// * `view_id == "default"` (`DEFAULT_VIEW_ID`) → write the global,
    ///   which is observably a no-op because the global already holds
    ///   `"default"` and no Slint code branches on `current-view` yet. No
    ///   panic, no crash, no visible state change.
    /// * `view_id == anything-else` → write the global verbatim and emit a
    ///   `tracing::debug!` line so Phase 4 developers can see mis-wired
    ///   action specs. Still no panic. Still no visible state change in
    ///   Phase 3 (no Slint reader).
    ///
    /// Implementations MUST NOT panic. The dispatcher invokes this from a
    /// gesture callback on the Slint UI thread; a panic there would crash
    /// the UI.
    fn navigate(&self, view_id: &str);
}

// ---------------------------------------------------------------------------
// SlintViewRouter — production impl
// ---------------------------------------------------------------------------

/// Production [`ViewRouter`] backed by a [`slint::Weak<MainWindow>`].
///
/// `slint::Weak<MainWindow>` is documented as `Send + Sync`. The actual
/// property write happens via `weak.upgrade()` — which is only sound on the
/// Slint UI thread. The Phase 3 dispatcher is invoked from gesture callbacks
/// that already run on the Slint UI thread (per TASK-060), so a direct
/// upgrade-and-write is correct.
///
/// If a future caller needs to invoke `navigate` from a non-UI thread (e.g.
/// the WS reconnect loop reacting to a server-pushed view-change), wrap this
/// router behind a `slint::invoke_from_event_loop` hop in that caller — the
/// router itself does NOT do the hop because doing so unconditionally would
/// add UI-thread-bouncing latency on the common case where the dispatcher
/// already holds the UI thread.
#[derive(Clone)]
pub struct SlintViewRouter {
    /// Weak handle so the router does not keep the window alive after the
    /// user closes it. Mirrors the [`crate::SlintSink`] pattern in
    /// `src/lib.rs`.
    window: slint::Weak<MainWindow>,
}

impl SlintViewRouter {
    /// Wrap a weak handle to the main window in a router.
    ///
    /// The caller obtains the weak handle via `window.as_weak()` after
    /// constructing `MainWindow::new()` (production path) or after the
    /// headless test platform install (test path).
    #[must_use]
    pub fn new(window: slint::Weak<MainWindow>) -> Self {
        SlintViewRouter { window }
    }
}

impl ViewRouter for SlintViewRouter {
    fn navigate(&self, view_id: &str) {
        // Single-view debug-log gate. Phase 3 ships only `"default"`;
        // anything else is a wiring bug or a Phase 4 leak — surface it at
        // debug level so the founder can grep traces. Per the locked
        // decision the navigate STILL writes the property; Phase 4 readers
        // will react. Phase 3 has no readers, so the write is observably a
        // no-op either way.
        if view_id != DEFAULT_VIEW_ID {
            debug!(
                requested_view = view_id,
                default_view = DEFAULT_VIEW_ID,
                "view-router: navigate to non-default view (Phase 3 single-view; \
                 Phase 4 will populate multi-view configs)"
            );
        }

        // Upgrade the weak handle and write the property. If the window has
        // been closed (or this method is called before the window is
        // constructed in tests), the upgrade returns `None` and we silently
        // skip — same pattern as `SlintSink`. No panic on a dropped window.
        let Some(window) = self.window.upgrade() else {
            debug!(
                view_id,
                "view-router: window already closed; skipping navigate"
            );
            return;
        };
        window
            .global::<ViewRouterGlobal>()
            .set_current_view(SharedString::from(view_id));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    // -----------------------------------------------------------------------
    // RecordingViewRouter — test-only fake used by dispatcher unit tests
    // -----------------------------------------------------------------------

    /// Captures every `navigate` call into an in-memory log.
    ///
    /// Used by the dispatcher's unit tests to assert that the dispatcher
    /// invokes the router with the expected `view_id` payload, without
    /// requiring a live Slint window. The record is `Mutex<Vec<String>>` so
    /// the type stays `Send + Sync` (the [`ViewRouter`] trait bound).
    pub(crate) struct RecordingViewRouter {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingViewRouter {
        pub(crate) fn new() -> Self {
            RecordingViewRouter {
                calls: Mutex::new(Vec::new()),
            }
        }

        /// Snapshot the recorded `view_id` values in call order.
        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .expect("RecordingViewRouter mutex poisoned")
                .clone()
        }
    }

    impl ViewRouter for RecordingViewRouter {
        fn navigate(&self, view_id: &str) {
            self.calls
                .lock()
                .expect("RecordingViewRouter mutex poisoned")
                .push(view_id.to_owned());
        }
    }

    // -----------------------------------------------------------------------
    // Recording-router behaviour
    // -----------------------------------------------------------------------

    #[test]
    fn recording_router_captures_default_navigation() {
        let router = RecordingViewRouter::new();
        router.navigate(DEFAULT_VIEW_ID);
        assert_eq!(router.calls(), vec!["default".to_owned()]);
    }

    #[test]
    fn recording_router_captures_unknown_view_id() {
        let router = RecordingViewRouter::new();
        router.navigate("kitchen");
        assert_eq!(router.calls(), vec!["kitchen".to_owned()]);
    }

    #[test]
    fn recording_router_captures_call_order() {
        let router = RecordingViewRouter::new();
        router.navigate("default");
        router.navigate("kitchen");
        router.navigate("default");
        assert_eq!(
            router.calls(),
            vec![
                "default".to_owned(),
                "kitchen".to_owned(),
                "default".to_owned(),
            ]
        );
    }

    // -----------------------------------------------------------------------
    // SlintViewRouter — defunct weak handle (no panic)
    // -----------------------------------------------------------------------

    #[test]
    fn slint_router_navigate_with_defunct_weak_does_not_panic() {
        // Default `slint::Weak<MainWindow>` upgrades to None — same shape as a
        // window that has been closed. The router must skip the property
        // write silently rather than panic.
        let weak: slint::Weak<MainWindow> = slint::Weak::default();
        let router = SlintViewRouter::new(weak);
        // Both arms (default + unknown) must be no-panic on a defunct weak.
        router.navigate(DEFAULT_VIEW_ID);
        router.navigate("kitchen");
    }

    // -----------------------------------------------------------------------
    // SlintViewRouter — live window write (headless platform)
    // -----------------------------------------------------------------------
    //
    // These tests install the `MinimalSoftwareWindow`-based Slint platform
    // already used by the bridge's headless tests (see the helpers in
    // `src/ui/bridge.rs::tests::install_test_platform_once_per_thread`).
    // Each test thread gets its own platform via a `thread_local!` OnceCell;
    // a dirty thread (some other test installed a different platform first)
    // surfaces as a clear panic rather than a silent wrong-backend pick.

    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter};
    use slint::PlatformError;

    /// Minimal Slint platform for headless tests. Hands every component the
    /// same `MinimalSoftwareWindow` so `MainWindow::new()` succeeds without
    /// any real graphics backend.
    struct HeadlessTestPlatform {
        window: std::rc::Rc<MinimalSoftwareWindow>,
    }

    impl Platform for HeadlessTestPlatform {
        fn create_window_adapter(&self) -> Result<std::rc::Rc<dyn WindowAdapter>, PlatformError> {
            Ok(self.window.clone())
        }
    }

    thread_local! {
        static TEST_PLATFORM_INSTALLED: std::cell::OnceCell<()> =
            const { std::cell::OnceCell::new() };
    }

    /// Install the headless test platform on the current libtest worker
    /// thread. Idempotent: subsequent calls on the same thread are no-ops.
    fn install_test_platform_once_per_thread() {
        TEST_PLATFORM_INSTALLED.with(|cell| {
            if cell.get().is_some() {
                return;
            }
            let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
            let platform = HeadlessTestPlatform { window };
            // If something else already installed a platform on this thread,
            // surface it as a single panic so the failure points at the
            // colliding caller.
            slint::platform::set_platform(Box::new(platform))
                .expect("test platform install: another platform was already set on this thread");
            cell.set(())
                .expect("OnceCell set must succeed on first call");
        });
    }

    /// Read the current view from the Slint global.
    fn read_current_view(window: &MainWindow) -> String {
        window
            .global::<ViewRouterGlobal>()
            .get_current_view()
            .to_string()
    }

    #[test]
    fn slint_router_default_seed_holds_default_view_id() {
        // Acceptance criterion: the global is initialised to `"default"` by
        // the Slint property declaration alone. No Rust write needed; this
        // is the property the harness-driven `Navigate` test cases bottom
        // out on.
        install_test_platform_once_per_thread();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");
        assert_eq!(
            read_current_view(&window),
            DEFAULT_VIEW_ID,
            "ViewRouterGlobal::current-view must be initialised to \"default\" by the Slint \
             property declaration so a Navigate-to-default is observably a no-op without any \
             prior Rust write"
        );
    }

    #[test]
    fn slint_router_navigate_to_default_is_observably_a_no_op() {
        // Acceptance criterion: `Navigate { view_id: "default" }` does not
        // panic and does not change the rendered state. We assert the
        // property value is `"default"` both before AND after the navigate
        // call — the global was seeded with the same value, so the write
        // produces no observable diff.
        install_test_platform_once_per_thread();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");
        let before = read_current_view(&window);
        assert_eq!(before, DEFAULT_VIEW_ID);

        let router = SlintViewRouter::new(window.as_weak());
        router.navigate(DEFAULT_VIEW_ID);

        assert_eq!(
            read_current_view(&window),
            DEFAULT_VIEW_ID,
            "navigate-to-default must leave current-view at \"default\" (no visible change)"
        );
    }

    #[test]
    fn slint_router_navigate_to_unknown_view_writes_value_no_panic() {
        // Acceptance criterion: `Navigate { view_id: "unknown" }` does not
        // panic and is logged at debug level. Phase 3 has no Slint readers
        // for `current-view`, so the write itself is the observable effect
        // — we assert it propagates verbatim. A future Phase 4 validator
        // will tighten this to a known-views allowlist.
        install_test_platform_once_per_thread();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");
        let router = SlintViewRouter::new(window.as_weak());

        router.navigate("kitchen");

        assert_eq!(
            read_current_view(&window),
            "kitchen",
            "the global must hold the requested view_id verbatim; Phase 3 has no Slint readers \
             so this is the only observable effect"
        );
    }

    #[test]
    fn slint_router_writes_through_view_id_verbatim() {
        // Stronger invariant: the router does NOT mutate, sanitize, or
        // intern `view_id`. The global ends up holding exactly the bytes the
        // caller passed (including unicode and non-trivial whitespace) so a
        // Phase 4 reader sees the spec-defined ids without surprises.
        install_test_platform_once_per_thread();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");
        let router = SlintViewRouter::new(window.as_weak());

        for &view_id in &["default", "kitchen", "öffice", "view with spaces"] {
            router.navigate(view_id);
            assert_eq!(
                read_current_view(&window),
                view_id,
                "router must propagate `{view_id}` verbatim"
            );
        }
    }

    #[test]
    fn slint_router_can_round_trip_back_to_default() {
        // After a navigate-to-non-default, navigating back to `"default"`
        // restores the original value. This is the Phase 3 stand-in for
        // Phase 4's "back gesture / parent view" plumbing — it asserts the
        // router itself does not retain state that prevents the fallback.
        install_test_platform_once_per_thread();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");
        let router = SlintViewRouter::new(window.as_weak());

        router.navigate("kitchen");
        assert_eq!(read_current_view(&window), "kitchen");

        router.navigate(DEFAULT_VIEW_ID);
        assert_eq!(
            read_current_view(&window),
            DEFAULT_VIEW_ID,
            "router must allow round-tripping back to the default view"
        );
    }
}
