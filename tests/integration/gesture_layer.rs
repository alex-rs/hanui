//! Integration tests for the Slint gesture layer in `card_base.slint` (TASK-060).
//!
//! These tests drive the gesture state machine end-to-end: the
//! [`HeadlessRenderer`] from `tests/common/slint_harness.rs` (TASK-074) installs
//! the platform; we instantiate `GestureTestWindow`
//! (`ui/slint/gesture_test_window.slint`, compiled by `build.rs` as a second
//! Slint entry point), wire its `tap-count` / `hold-count` / `double-tap-count`
//! `out` properties, dispatch synthetic `WindowEvent::PointerPressed` and
//! `PointerReleased` events at known positions, sleep just long enough to
//! elapse Slint's animation tick past the gesture timer thresholds, and assert
//! the counters reflect exactly the expected number of events.
//!
//! # Why a separate test wrapper component
//!
//! `CardBase` (the unit under test) inherits `Rectangle`, not `Window`.
//! Slint's `slint::include_modules!()` only emits a top-level Rust
//! `ComponentHandle` for components rooted in a `Window`; to drive `CardBase`
//! from a Rust test we therefore need a Window-rooted wrapper.
//! `gesture_test_window.slint` hosts a single `CardBase` filling its window
//! and forwards each gesture callback to an `out` counter so the test can
//! assert counts via the generated `get_<name>` accessors.
//!
//! # Why a second Slint compile
//!
//! `slint::include_modules!()` consumes the `SLINT_INCLUDE_GENERATED` env var
//! that `slint_build::compile()` emits, and only one such environment value
//! can exist per package. `build.rs` invokes `slint_build::compile_with_output_path`
//! for the test wrapper instead, which writes a separate generated `.rs` file
//! and exposes its absolute path via `cargo:rustc-env=HANUI_GESTURE_TEST_INCLUDE=...`.
//! This file then `include!`s that path inside a private module so the test
//! window's generated bindings live alongside (but disjoint from) the
//! production `MainWindow` bindings.
//!
//! # Acceptance criteria covered (TASK-060)
//!
//! 1. **A single tap fires exactly one `tap` callback and zero `hold`** —
//!    `tap_fires_exactly_once_when_arm_double_tap_disabled` (synchronous path)
//!    and `tap_fires_exactly_once_after_promotion_window_when_armed`
//!    (post-promotion-timer path).
//! 2. **A held press fires exactly one `hold` and zero `tap`** (Risk #4) —
//!    `hold_fires_once_and_release_does_not_fire_tap`.
//! 3. **`arm_double_tap_timer == false` ⇒ tap fires synchronously on
//!    touch-up; two quick taps fire two `tap` callbacks (no double-tap
//!    promotion)** — `two_quick_taps_with_double_tap_disabled_fire_two_taps`.
//! 4. **`arm_double_tap_timer == true` ⇒ two rapid touch-downs within the
//!    gap fire `double_tap` exactly once and the pending single tap is
//!    cancelled** — `two_rapid_taps_with_double_tap_enabled_fire_double_tap`.
//!
//! # Determinism
//!
//! All thresholds are mutated through `GestureConfigGlobal` to short values
//! (50 ms tap window, 80 ms hold threshold, 60 ms double-tap gap) so each
//! test sleeps a small multiple of the threshold and finishes well under
//! the 5-minute `@smoke` budget. The assertions are exact-equality on the
//! counters (no `>= 1`), enforcing the "exactly one event per gesture"
//! invariant Risk #4 calls out explicitly.

#![allow(clippy::cast_possible_truncation)]

use std::time::Duration;

use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition};

// `super::slint_harness` is re-exposed by `tests/integration/mod.rs` via a
// `#[path = "../common/slint_harness.rs"]` module so this file sees the same
// `HeadlessRenderer` API the smoke test uses.
use super::slint_harness::HeadlessRenderer;

// Test-only Slint bindings. `build.rs` writes a generated `.rs` file for
// `ui/slint/gesture_test_window.slint` to `$OUT_DIR/gesture_test_window.rs`
// and exposes its absolute path via `HANUI_GESTURE_TEST_INCLUDE`. `include!`
// pulls those bindings into a private module so they don't leak into the
// integration binary's top-level namespace.
mod gesture_test_slint {
    #![allow(clippy::all)]
    #![allow(unused_imports)]
    #![allow(unused_must_use)]
    #![allow(non_snake_case)]
    #![allow(non_camel_case_types)]
    #![allow(dead_code)]
    include!(env!("HANUI_GESTURE_TEST_INCLUDE"));
}

use gesture_test_slint::{GestureConfigGlobal, GestureTestWindow};

// ---------------------------------------------------------------------------
// Fixture knobs
// ---------------------------------------------------------------------------

/// Tight test window (50 px square) keeps every event well inside the
/// `gesture-touch` TouchArea regardless of layout drift.
const WIN_W: u32 = 50;
const WIN_H: u32 = 50;

/// Click position — center of the window. Slint's `LogicalPosition` is in
/// logical pixels; with no scale factor override these match physical px.
fn center() -> LogicalPosition {
    LogicalPosition::new((WIN_W as f32) / 2.0, (WIN_H as f32) / 2.0)
}

/// Build a fresh `GestureTestWindow` rendered through the headless harness.
///
/// The harness `MUST` be constructed before the first `ComponentHandle::new()`
/// so its custom platform wins the race against any auto-installed backend.
/// We render the component once at construction so the `MinimalSoftwareWindow`
/// inside the harness picks up the component's size and Slint sets up its
/// per-window state (so subsequent `dispatch_event` calls land on the right
/// item tree). Every test calls this helper at the top.
fn fresh_window() -> (HeadlessRenderer, GestureTestWindow) {
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = GestureTestWindow::new().expect("instantiate GestureTestWindow");

    // Render once to register the component with the harness window. The
    // captured frame is discarded — we only care about the side effect of
    // the show()/snapshot()/hide() cycle inside `render_component`.
    harness
        .render_component(&window, WIN_W, WIN_H)
        .expect("initial render seeds harness platform");

    (harness, window)
}

/// Configure `GestureConfigGlobal` for a deterministic, fast test.
///
/// `arm_double_tap_timer` is set explicitly per the locked decision: the
/// Slint side reads this boolean and only this boolean — no zero-gap
/// inference.
fn configure_timing(
    window: &GestureTestWindow,
    tap_max_ms: i32,
    hold_min_ms: i32,
    double_tap_max_gap_ms: i32,
    arm_double_tap_timer: bool,
) {
    let global = window.global::<GestureConfigGlobal>();
    global.set_tap_max_ms(tap_max_ms);
    global.set_hold_min_ms(hold_min_ms);
    global.set_double_tap_max_gap_ms(double_tap_max_gap_ms);
    global.set_double_tap_enabled(arm_double_tap_timer);
    global.set_arm_double_tap_timer(arm_double_tap_timer);
}

/// Dispatch a synthetic `PointerPressed` (left button) at `pos` to the test
/// window's underlying `slint::Window`.
fn press(window: &GestureTestWindow, pos: LogicalPosition) {
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position: pos,
        button: PointerEventButton::Left,
    });
}

/// Dispatch a synthetic `PointerReleased` (left button) at `pos`.
fn release(window: &GestureTestWindow, pos: LogicalPosition) {
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position: pos,
            button: PointerEventButton::Left,
        });
}

/// Sleep on the OS clock for `ms` milliseconds, then drive Slint's timer
/// queue and event loop one cycle so the `Timer` items inside `card_base.slint`
/// have a chance to fire.
///
/// `slint::platform::update_timers_and_animations()` advances the animation
/// tick and triggers any due timers. We pair it with `run_event_loop_until`
/// in single-shot mode (no actual blocking; the closure returns immediately)
/// to flush queued callbacks.
fn advance(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
    // Process any due timers and pending property updates without blocking.
    // `update_timers_and_animations` is the documented path for headless
    // tests — see slint::platform module docs.
    slint::platform::update_timers_and_animations();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// AC #1a: with `arm_double_tap_timer == false`, a single press/release inside
/// `tap_max_ms` fires `tap` exactly once and `hold` zero times. The tap is
/// emitted synchronously on touch-up — there is no `double_tap_max_gap_ms`
/// delay because the boolean explicitly disables the promotion timer.
#[test]
fn tap_fires_exactly_once_when_arm_double_tap_disabled() {
    let (_harness, window) = fresh_window();
    configure_timing(
        &window, /* tap_max_ms */ 50, /* hold_min_ms */ 80,
        /* double_tap_max_gap_ms */ 60, /* arm_double_tap_timer */ false,
    );

    press(&window, center());
    advance(5); // well under tap_max_ms (50 ms) and hold_min_ms (80 ms)
    release(&window, center());
    // Synchronous fire — no need to wait for any timer. Drive one tick so
    // the property update from the callback is visible.
    advance(1);

    assert_eq!(
        window.get_tap_count(),
        1,
        "tap must fire exactly once on a synchronous (no-armed-promotion) tap"
    );
    assert_eq!(
        window.get_hold_count(),
        0,
        "hold must NOT fire when the press releases inside tap_max_ms"
    );
    assert_eq!(
        window.get_double_tap_count(),
        0,
        "double-tap must NOT fire when arm_double_tap_timer is false"
    );
}

/// AC #1b: with `arm_double_tap_timer == true`, a single press/release inside
/// `tap_max_ms` fires `tap` exactly once AFTER the `double_tap_max_gap_ms`
/// promotion window elapses with no second touch-down. Crucially, the
/// counter is still 0 immediately after release — the tap is deferred.
#[test]
fn tap_fires_exactly_once_after_promotion_window_when_armed() {
    let (_harness, window) = fresh_window();
    configure_timing(
        &window, /* tap_max_ms */ 50, /* hold_min_ms */ 200,
        /* double_tap_max_gap_ms */ 40, /* arm_double_tap_timer */ true,
    );

    press(&window, center());
    advance(5);
    release(&window, center());

    // Immediately after release the tap should NOT have fired yet — the
    // promotion timer is armed and waiting for a possible second-down.
    advance(1);
    assert_eq!(
        window.get_tap_count(),
        0,
        "tap must NOT fire synchronously when arm_double_tap_timer is true"
    );

    // Wait out the promotion window (40 ms) plus a small slack for timer
    // dispatch and assert the deferred tap fired exactly once.
    advance(80);
    assert_eq!(
        window.get_tap_count(),
        1,
        "tap must fire exactly once after double_tap_max_gap_ms when no second \
         touch-down arrives"
    );
    assert_eq!(window.get_hold_count(), 0);
    assert_eq!(window.get_double_tap_count(), 0);
}

/// AC #2 / Risk #4: a held press fires `hold` exactly once AND the
/// subsequent release does NOT fire `tap`. This is the explicit no-spurious-
/// tap-on-hold-release invariant.
#[test]
fn hold_fires_once_and_release_does_not_fire_tap() {
    let (_harness, window) = fresh_window();
    configure_timing(
        &window, /* tap_max_ms */ 30, /* hold_min_ms */ 60,
        /* double_tap_max_gap_ms */ 40, /* arm_double_tap_timer */ true,
    );

    press(&window, center());
    // Wait past hold_min_ms so the hold timer fires while still pressed.
    advance(120);

    // Hold must have fired exactly once by now.
    assert_eq!(
        window.get_hold_count(),
        1,
        "hold must fire exactly once after hold_min_ms while still pressed"
    );
    assert_eq!(
        window.get_tap_count(),
        0,
        "tap must NOT fire while hold is in flight"
    );

    // Release. The press duration far exceeds tap_max_ms, AND hold_fired is
    // true — both code paths must independently suppress the tap.
    release(&window, center());
    advance(80); // well past double_tap_max_gap_ms

    assert_eq!(
        window.get_hold_count(),
        1,
        "hold must NOT re-fire on release"
    );
    assert_eq!(
        window.get_tap_count(),
        0,
        "tap must NOT fire on hold-release (Risk #4 explicit acceptance)"
    );
    assert_eq!(window.get_double_tap_count(), 0);
}

/// AC #3: with `arm_double_tap_timer == false`, two quick taps fire `tap`
/// twice (one per release) — no double-tap promotion. This is the explicit
/// "Slint never infers double-tap intent from a zero gap" guarantee, tested
/// in its observable form: with the boolean off, two taps are two taps.
#[test]
fn two_quick_taps_with_double_tap_disabled_fire_two_taps() {
    let (_harness, window) = fresh_window();
    configure_timing(
        &window, /* tap_max_ms */ 50, /* hold_min_ms */ 200,
        /* double_tap_max_gap_ms */ 40, /* arm_double_tap_timer */ false,
    );

    press(&window, center());
    advance(5);
    release(&window, center());
    advance(5);

    // First tap must have fired synchronously on the previous release.
    assert_eq!(
        window.get_tap_count(),
        1,
        "first tap must fire synchronously when arm_double_tap_timer is false"
    );

    press(&window, center());
    advance(5);
    release(&window, center());
    advance(5);

    assert_eq!(
        window.get_tap_count(),
        2,
        "second tap must fire synchronously — no double-tap promotion when disabled"
    );
    assert_eq!(window.get_hold_count(), 0);
    assert_eq!(
        window.get_double_tap_count(),
        0,
        "double-tap must NEVER fire when arm_double_tap_timer is false, even \
         if the two taps happen to fall inside double_tap_max_gap_ms"
    );
}

/// AC #4: with `arm_double_tap_timer == true`, two rapid press/release pairs
/// where the second touch-down arrives inside `double_tap_max_gap_ms` of the
/// first release fire `double_tap` exactly once and the still-pending first
/// tap is cancelled rather than emitted.
#[test]
fn two_rapid_taps_with_double_tap_enabled_fire_double_tap() {
    let (_harness, window) = fresh_window();
    configure_timing(
        &window, /* tap_max_ms */ 50, /* hold_min_ms */ 200,
        /* double_tap_max_gap_ms */ 60, /* arm_double_tap_timer */ true,
    );

    // First press/release — pending-tap is armed but not fired.
    press(&window, center());
    advance(5);
    release(&window, center());
    advance(5);
    assert_eq!(
        window.get_tap_count(),
        0,
        "first tap must be deferred on the promotion timer"
    );

    // Second press inside the promotion window cancels the pending tap and
    // fires double-tap.
    press(&window, center());
    advance(2);
    assert_eq!(
        window.get_double_tap_count(),
        1,
        "double-tap must fire on second touch-down inside double_tap_max_gap_ms"
    );
    assert_eq!(
        window.get_tap_count(),
        0,
        "the still-pending first tap must be CANCELLED, not emitted"
    );

    // Release the second press; it lands in the dead zone (we don't care
    // whether tap fires here for the second press, the AC is about the
    // double-tap promotion of the FIRST pair). Wait past the promotion
    // window so any deferred tap from the second release would have fired.
    release(&window, center());
    advance(120);

    assert_eq!(
        window.get_double_tap_count(),
        1,
        "double-tap must fire exactly once across the whole sequence"
    );
    // The second release is itself a tap-up that arms a NEW promotion
    // timer (per the state machine: every up that classifies as a tap
    // arms the promotion timer when armed). After the gap elapses we
    // expect that second tap to fire.
    assert_eq!(
        window.get_tap_count(),
        1,
        "second release's promotion timer fires its own tap after the gap; \
         the cancelled first-pending tap is NOT counted"
    );
    assert_eq!(window.get_hold_count(), 0);
}
