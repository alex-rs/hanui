//! Toast banner state + driver, and per-tile pending spinner refresh
//! (TASK-067).
//!
//! # Two responsibilities, one module
//!
//! TASK-067 ships two transient-feedback surfaces co-delivered in one PR:
//!
//! * **Toast** — single transient banner driven by the dispatcher's toast
//!   channel. Auto-dismiss after [`DEFAULT_TOAST_DISMISS_MS`] (4 s),
//!   tap-to-dismiss earlier, single visible at a time, newer replaces older,
//!   no queue across view changes. Every behavioural rule comes from
//!   `docs/plans/2026-04-28-phase-3-actions.md` § `locked_decisions.toast_behavior`.
//! * **Per-tile spinner** — overlay on `card_base.slint`. Visibility binds to
//!   [`crate::ha::live_store::LiveStore::pending_for_widget`] — the
//!   cross-owner read API locked in
//!   `locked_decisions.pending_state_read_api` and shipped by TASK-064. The
//!   binding is one-way (reader-only): this module never writes optimistic
//!   state, only reads it through the API.
//!
//! Both surfaces share this module because:
//!
//! 1. Both are driven from the dispatcher's `ToastEvent` channel + the
//!    `LiveStore` pending read API — co-locating keeps the reader logic in
//!    one place rather than splitting it across `toast.rs` and a separate
//!    `spinner.rs` that would only export one function.
//! 2. The TASK-067 ticket enumerates them as a single deliverable.
//!
//! # No `src/ha/**` writes
//!
//! This module reads `LiveStore::pending_for_widget` and consumes
//! `mpsc::Receiver<ToastEvent>`; it never modifies optimistic state. The
//! ticket's `must_not_touch` rule covers `src/actions/**` (toast events
//! originate there) and `src/ha/**` (pending state lives there) — both
//! are consumed read-only here.
//!
//! # Lazy / poll-driven application
//!
//! [`ToastState`] is a pure state machine. Production callers (`src/lib.rs`)
//! drive [`ToastDriver::poll`] from the same Tokio runtime that owns the
//! Slint event loop; the driver consumes pending events on the toast
//! channel, advances the auto-dismiss clock, and writes the resulting state
//! to the [`MainWindow`] via [`apply_to_window`]. Tests exercise the state
//! machine directly without instantiating a Slint window.

use std::time::{Duration, Instant};

use slint::{ModelRc, SharedString, VecModel};
use tokio::sync::mpsc;

use crate::actions::dispatcher::{BackpressureScope, ToastEvent};
use crate::actions::map::WidgetId;
use crate::ha::live_store::LiveStore;
use crate::ui::bridge::{slint_ui, MainWindow};

// ---------------------------------------------------------------------------
// Tunables (locked_decisions.toast_behavior)
// ---------------------------------------------------------------------------

/// Default auto-dismiss interval for a toast in milliseconds
/// (`locked_decisions.toast_behavior` — `toast_dismiss_ms = 4000`).
///
/// Tests may pass a smaller value to [`ToastState::with_dismiss_ms`] to
/// bound the wall-clock cost; production wiring uses this default.
pub const DEFAULT_TOAST_DISMISS_MS: u64 = 4000;

// ---------------------------------------------------------------------------
// ToastState
// ---------------------------------------------------------------------------

/// Pure state machine for the single visible toast.
///
/// The machine encodes the four behaviours locked in
/// `locked_decisions.toast_behavior`:
///
/// 1. **Single visible at a time / newer replaces older.** The state holds
///    at most one `Some(visible)`; [`ToastState::push`] overwrites the
///    field unconditionally, re-arming the auto-dismiss clock.
/// 2. **Auto-dismiss after `dismiss_ms`.** [`ToastState::tick`] takes a
///    monotonic `Instant` and clears the visible toast when the
///    `dispatched_at + dismiss_ms` deadline has passed.
/// 3. **Tap-to-dismiss earlier.** [`ToastState::dismiss`] clears the
///    visible toast immediately; the Rust driver wires Slint's
///    `toast-dismissed` callback to this method.
/// 4. **No queue across view changes.** [`ToastState::clear_for_navigate`]
///    clears the visible toast on every navigation outcome — the
///    dispatcher fires this from `view_router` integration.
#[derive(Debug, Clone)]
pub struct ToastState {
    visible: Option<VisibleToast>,
    dismiss_ms: u64,
}

#[derive(Debug, Clone)]
struct VisibleToast {
    text: String,
    dispatched_at: Instant,
}

impl ToastState {
    /// Construct a fresh toast state with the default 4-second auto-dismiss.
    #[must_use]
    pub fn new() -> Self {
        ToastState {
            visible: None,
            dismiss_ms: DEFAULT_TOAST_DISMISS_MS,
        }
    }

    /// Construct with a custom dismiss interval (tests use this to short-
    /// circuit the wall-clock without sleeping for 4 s).
    #[must_use]
    pub fn with_dismiss_ms(dismiss_ms: u64) -> Self {
        ToastState {
            visible: None,
            dismiss_ms,
        }
    }

    /// Currently-visible toast text, or `None` when nothing is shown.
    #[must_use]
    pub fn visible_text(&self) -> Option<&str> {
        self.visible.as_ref().map(|v| v.text.as_str())
    }

    /// `true` while a toast is currently being shown.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible.is_some()
    }

    /// Configured auto-dismiss interval in milliseconds.
    #[must_use]
    pub fn dismiss_ms(&self) -> u64 {
        self.dismiss_ms
    }

    /// Push a new toast. Replaces any currently-visible toast (newer wins),
    /// rearming the auto-dismiss clock. `now` is the monotonic instant at
    /// dispatch time — production wiring passes [`Instant::now`], tests
    /// pass a known instant for determinism.
    pub fn push(&mut self, text: impl Into<String>, now: Instant) {
        self.visible = Some(VisibleToast {
            text: text.into(),
            dispatched_at: now,
        });
    }

    /// Push a [`ToastEvent`] after formatting it via [`format_toast_message`].
    /// The two-argument overload exists so the production driver can hand
    /// an event directly without re-wrapping the formatter at every call
    /// site; unit tests prefer [`ToastState::push`] with a fixed string.
    pub fn push_event(&mut self, event: &ToastEvent, now: Instant) {
        self.push(format_toast_message(event), now);
    }

    /// Advance the dismiss clock to `now`. If the visible toast's
    /// `dispatched_at + dismiss_ms` deadline has passed, the toast is
    /// cleared.
    ///
    /// Returns `true` if the call cleared a previously-visible toast,
    /// `false` otherwise (no toast / not yet expired). The boolean lets
    /// the driver skip a redundant Slint property write on the steady-state
    /// path.
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(visible) = &self.visible else {
            return false;
        };
        let elapsed = now.saturating_duration_since(visible.dispatched_at);
        if elapsed >= Duration::from_millis(self.dismiss_ms) {
            self.visible = None;
            true
        } else {
            false
        }
    }

    /// Dismiss the visible toast immediately (tap-to-dismiss path). Returns
    /// `true` if a toast was cleared, `false` if no toast was visible.
    pub fn dismiss(&mut self) -> bool {
        if self.visible.is_some() {
            self.visible = None;
            true
        } else {
            false
        }
    }

    /// Clear the visible toast on a navigation event
    /// (`locked_decisions.toast_behavior` — "no queue persists across view
    /// changes"). Distinct from [`Self::dismiss`] only at the call site;
    /// both clear unconditionally. Returns `true` if a toast was cleared.
    pub fn clear_for_navigate(&mut self) -> bool {
        self.dismiss()
    }
}

impl Default for ToastState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Toast event → message formatting
// ---------------------------------------------------------------------------

/// Format a [`ToastEvent`] into the user-facing message string.
///
/// Centralised here so the wording is stable across:
///
/// * The interactive toast banner (this module).
/// * Future log lines (e.g. `tracing::info!` mirroring the toast text).
/// * Test fixtures that assert the toast text shown on a specific event.
///
/// Keeps the format deterministic — no localisation / formatting variance
/// — so unit and integration tests can compare exact strings.
#[must_use]
pub fn format_toast_message(event: &ToastEvent) -> String {
    match event {
        ToastEvent::BackpressureRejected { entity_id, scope } => match scope {
            BackpressureScope::PerEntity => {
                format!("Action queue full for `{entity_id}`")
            }
            BackpressureScope::Global => {
                format!("Action queue full (system-wide), dropping `{entity_id}`")
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Slint property wiring
// ---------------------------------------------------------------------------

/// Apply a [`ToastState`] to the [`MainWindow`]'s `toast-*` properties.
///
/// Idempotent: calling it twice with the same state produces the same
/// window state. The function only writes the two properties — it never
/// mutates the state — so the driver can call it after every state
/// transition without risk of double-writes interacting badly.
///
/// Production callers ([`ToastDriver::poll`]) call this once per poll cycle
/// after [`ToastState::tick`] / [`ToastState::push_event`] returned a
/// state-changed signal; tests call it directly to verify the property
/// writes.
pub fn apply_to_window(state: &ToastState, window: &MainWindow) {
    let text = state.visible_text().unwrap_or("");
    window.set_toast_text(SharedString::from(text));
    window.set_toast_visible(state.is_visible());
}

// ---------------------------------------------------------------------------
// ToastDriver
// ---------------------------------------------------------------------------

/// Drives a [`ToastState`] from a [`mpsc::Receiver<ToastEvent>`] and applies
/// the result to the [`MainWindow`].
///
/// Production wiring: the dispatcher creates the toast channel, hands the
/// receiver to a `ToastDriver`, and the driver task is spawned on the same
/// runtime as the Slint event loop. The driver polls in a loop:
///
/// 1. Try to receive a new [`ToastEvent`] on the channel — if one arrives,
///    push it onto the state with `Instant::now`.
/// 2. Advance the dismiss clock to `Instant::now`.
/// 3. If either step changed the state, write through to the window via
///    [`apply_to_window`].
///
/// The poll cadence is set by the caller (typically 100 ms — short enough
/// to make the auto-dismiss feel snappy, long enough to avoid runtime
/// overhead). Tests substitute a fake clock and call [`Self::poll_with_now`]
/// directly to advance time deterministically.
pub struct ToastDriver {
    state: ToastState,
    rx: mpsc::Receiver<ToastEvent>,
}

impl ToastDriver {
    /// Construct a driver around an existing [`ToastState`] and a toast
    /// channel receiver. The receiver is exclusively owned by the driver —
    /// production code creates the channel pair and hands the sender to the
    /// dispatcher, the receiver to this driver.
    #[must_use]
    pub fn new(state: ToastState, rx: mpsc::Receiver<ToastEvent>) -> Self {
        Self { state, rx }
    }

    /// Read-only view into the current state. Tests use this to assert
    /// transitions without consuming the driver.
    pub fn state(&self) -> &ToastState {
        &self.state
    }

    /// Mutable view into the state. Production callers use this to invoke
    /// [`ToastState::clear_for_navigate`] from the view-router integration
    /// path; tests use it to drive the state directly.
    pub fn state_mut(&mut self) -> &mut ToastState {
        &mut self.state
    }

    /// Poll once at the current `Instant::now()`. Returns `true` if the
    /// state transitioned (a new event was consumed or the visible toast
    /// auto-dismissed); production wiring uses the boolean to elide a
    /// redundant property write on quiet ticks.
    pub fn poll(&mut self) -> bool {
        self.poll_with_now(Instant::now())
    }

    /// Poll variant taking an explicit `now`. Tests use this to advance
    /// the dismiss clock without `tokio::time::sleep` or wall-clock
    /// dependence.
    pub fn poll_with_now(&mut self, now: Instant) -> bool {
        let mut changed = false;
        // Drain every queued event in one go so a backlog does not show
        // a stale toast for an extra poll cycle. `try_recv` returns
        // `Empty` once the channel is drained; `Disconnected` is treated
        // identically — the driver outlives the channel and continues to
        // run the auto-dismiss clock.
        while let Ok(event) = self.rx.try_recv() {
            self.state.push_event(&event, now);
            changed = true;
        }
        if self.state.tick(now) {
            changed = true;
        }
        changed
    }
}

// ---------------------------------------------------------------------------
// Per-tile pending spinner refresh
// ---------------------------------------------------------------------------

/// Refresh the per-tile pending spinner state on the [`MainWindow`].
///
/// **Cross-owner binding (locked_decisions.pending_state_read_api).**
/// Walks the three tile models on the window, maps each tile to its
/// widget id via the parallel `widget_ids_*` slices supplied by the
/// caller, looks up each widget's pending state via
/// [`LiveStore::pending_for_widget`] (the cross-owner read API from
/// TASK-064), and rewrites each tile's `pending` field with the result.
///
/// Why parallel slices instead of a `Vec<(WidgetId, TileVM)>`:
///
/// * The bridge already produces three per-kind models from `split_tile_vms`;
///   asking the caller to also produce three parallel `Vec<WidgetId>`
///   slices keeps the per-kind discipline that
///   [`crate::ui::bridge::wire_window`] already enforces.
/// * No Rust struct churn — the existing `LightTileVM` / `SensorTileVM` /
///   `EntityTileVM` Rust types already carry `pending: bool`, and the
///   Slint-typed mirror types written into the model also have that
///   field. The driver just rewrites the field on each row.
///
/// The function rewrites `set_row_data` for every row so a previously-
/// `true` pending value flips back to `false` once the dispatcher has
/// drained the matching optimistic entry — the spinner disappears
/// promptly without an extra `wire_window` rebuild.
///
/// # Panics
///
/// Does not panic. If a `widget_ids_*` slice is shorter than its
/// corresponding model, the missing rows are left with their previous
/// `pending` value (defensive: mismatched lengths indicate a wiring bug
/// in the caller; we do not silently truncate the model).
pub fn apply_pending_for_widgets(
    window: &MainWindow,
    store: &LiveStore,
    widget_ids_lights: &[WidgetId],
    widget_ids_sensors: &[WidgetId],
    widget_ids_entities: &[WidgetId],
) {
    use slint::Model;

    let lights = window.get_light_tiles();
    for (idx, widget_id) in widget_ids_lights.iter().enumerate() {
        if let Some(mut row) = lights.row_data(idx) {
            let pending = store.pending_for_widget(widget_id);
            if row.pending != pending {
                row.pending = pending;
                lights.set_row_data(idx, row);
            }
        }
    }

    let sensors = window.get_sensor_tiles();
    for (idx, widget_id) in widget_ids_sensors.iter().enumerate() {
        if let Some(mut row) = sensors.row_data(idx) {
            let pending = store.pending_for_widget(widget_id);
            if row.pending != pending {
                row.pending = pending;
                sensors.set_row_data(idx, row);
            }
        }
    }

    let entities = window.get_entity_tiles();
    for (idx, widget_id) in widget_ids_entities.iter().enumerate() {
        if let Some(mut row) = entities.row_data(idx) {
            let pending = store.pending_for_widget(widget_id);
            if row.pending != pending {
                row.pending = pending;
                entities.set_row_data(idx, row);
            }
        }
    }
}

/// Convenience: install fresh `[*TileVM]` models on `window` populated
/// from the supplied per-kind tile vecs.
///
/// Used by integration tests that need to seed a known tile layout
/// before exercising the spinner-refresh path. Production code uses
/// [`crate::ui::bridge::wire_window`] for the same purpose; this helper
/// avoids pulling in icon resolution at test time.
pub fn install_test_tiles(
    window: &MainWindow,
    lights: Vec<slint_ui::LightTileVM>,
    sensors: Vec<slint_ui::SensorTileVM>,
    entities: Vec<slint_ui::EntityTileVM>,
) {
    let light_model: ModelRc<slint_ui::LightTileVM> = ModelRc::new(VecModel::from(lights));
    let sensor_model: ModelRc<slint_ui::SensorTileVM> = ModelRc::new(VecModel::from(sensors));
    let entity_model: ModelRc<slint_ui::EntityTileVM> = ModelRc::new(VecModel::from(entities));
    window.set_light_tiles(light_model);
    window.set_sensor_tiles(sensor_model);
    window.set_entity_tiles(entity_model);
}

// ---------------------------------------------------------------------------
// Tests — pure state machine
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::entity::EntityId;

    fn t0() -> Instant {
        Instant::now()
    }

    // ── ToastState ─────────────────────────────────────────────────────

    #[test]
    fn new_state_has_no_visible_toast() {
        let s = ToastState::new();
        assert!(!s.is_visible());
        assert_eq!(s.visible_text(), None);
        assert_eq!(s.dismiss_ms(), DEFAULT_TOAST_DISMISS_MS);
    }

    #[test]
    fn push_makes_toast_visible() {
        let mut s = ToastState::new();
        let now = t0();
        s.push("hello", now);
        assert!(s.is_visible());
        assert_eq!(s.visible_text(), Some("hello"));
    }

    #[test]
    fn push_replaces_older_toast_immediately() {
        // Locked decision: newer replaces older immediately. No queue.
        let mut s = ToastState::new();
        let t = t0();
        s.push("older", t);
        s.push("newer", t + Duration::from_millis(100));
        assert_eq!(
            s.visible_text(),
            Some("newer"),
            "newer push must overwrite older — no queue"
        );
    }

    #[test]
    fn tick_below_dismiss_threshold_keeps_toast_visible() {
        let mut s = ToastState::with_dismiss_ms(4000);
        let t = t0();
        s.push("hi", t);
        // Advance 3 s — still visible.
        let cleared = s.tick(t + Duration::from_millis(3000));
        assert!(!cleared, "tick below threshold must not clear");
        assert!(s.is_visible());
    }

    #[test]
    fn tick_at_dismiss_threshold_clears_toast() {
        let mut s = ToastState::with_dismiss_ms(4000);
        let t = t0();
        s.push("bye", t);
        // Advance exactly 4 s — clears.
        let cleared = s.tick(t + Duration::from_millis(4000));
        assert!(cleared, "tick at threshold must clear (>=)");
        assert!(!s.is_visible());
    }

    #[test]
    fn tick_above_dismiss_threshold_clears_toast() {
        let mut s = ToastState::with_dismiss_ms(4000);
        let t = t0();
        s.push("bye", t);
        let cleared = s.tick(t + Duration::from_millis(5000));
        assert!(cleared);
        assert!(!s.is_visible());
    }

    #[test]
    fn push_rearms_dismiss_clock() {
        // After a push, the dismiss clock starts fresh — a tick that
        // would have cleared the OLD toast must NOT clear the new one.
        let mut s = ToastState::with_dismiss_ms(4000);
        let t = t0();
        s.push("first", t);
        // Advance 3 s, then push a new toast — old clock would expire at
        // t+4s, but new toast clock expires at t+3s+4s = t+7s.
        s.push("second", t + Duration::from_millis(3000));
        // Tick at t+5s would have killed the first; second is at t+5s-t+3s=2s old.
        let cleared = s.tick(t + Duration::from_millis(5000));
        assert!(!cleared, "newer toast must use its own dispatch time");
        assert_eq!(s.visible_text(), Some("second"));
    }

    #[test]
    fn dismiss_clears_visible_toast_immediately() {
        let mut s = ToastState::new();
        let t = t0();
        s.push("tap-me", t);
        let cleared = s.dismiss();
        assert!(cleared);
        assert!(!s.is_visible());
    }

    #[test]
    fn dismiss_on_empty_state_returns_false() {
        let mut s = ToastState::new();
        assert!(!s.dismiss(), "dismiss on empty state is a no-op");
    }

    #[test]
    fn clear_for_navigate_clears_visible_toast() {
        // Locked decision: no queue across view changes. Navigate clears
        // the current toast even if its auto-dismiss has not fired.
        let mut s = ToastState::with_dismiss_ms(4000);
        let t = t0();
        s.push("survives-tick-not-navigate", t);
        let cleared = s.clear_for_navigate();
        assert!(cleared);
        assert!(!s.is_visible());
    }

    #[test]
    fn clear_for_navigate_on_empty_state_returns_false() {
        let mut s = ToastState::new();
        assert!(!s.clear_for_navigate());
    }

    // ── format_toast_message ──────────────────────────────────────────

    #[test]
    fn format_per_entity_backpressure_names_entity() {
        let event = ToastEvent::BackpressureRejected {
            entity_id: EntityId::from("light.kitchen"),
            scope: BackpressureScope::PerEntity,
        };
        assert_eq!(
            format_toast_message(&event),
            "Action queue full for `light.kitchen`"
        );
    }

    #[test]
    fn format_global_backpressure_indicates_system_wide() {
        let event = ToastEvent::BackpressureRejected {
            entity_id: EntityId::from("switch.outlet_1"),
            scope: BackpressureScope::Global,
        };
        let msg = format_toast_message(&event);
        assert!(
            msg.contains("system-wide"),
            "global scope must indicate system-wide: {msg}"
        );
        assert!(
            msg.contains("switch.outlet_1"),
            "entity id must still be present: {msg}"
        );
    }

    // ── push_event delegates to push + format ─────────────────────────

    #[test]
    fn push_event_uses_formatted_message() {
        let mut s = ToastState::new();
        let event = ToastEvent::BackpressureRejected {
            entity_id: EntityId::from("light.bedroom"),
            scope: BackpressureScope::PerEntity,
        };
        s.push_event(&event, t0());
        assert_eq!(
            s.visible_text(),
            Some("Action queue full for `light.bedroom`")
        );
    }

    // ── ToastDriver ───────────────────────────────────────────────────

    #[tokio::test]
    async fn driver_consumes_event_and_makes_toast_visible() {
        let (tx, rx) = mpsc::channel::<ToastEvent>(4);
        let mut driver = ToastDriver::new(ToastState::new(), rx);
        let event = ToastEvent::BackpressureRejected {
            entity_id: EntityId::from("light.kitchen"),
            scope: BackpressureScope::PerEntity,
        };
        tx.send(event).await.unwrap();

        let now = Instant::now();
        let changed = driver.poll_with_now(now);
        assert!(changed);
        assert_eq!(
            driver.state().visible_text(),
            Some("Action queue full for `light.kitchen`")
        );
    }

    #[tokio::test]
    async fn driver_advances_dismiss_clock() {
        let (tx, rx) = mpsc::channel::<ToastEvent>(4);
        let mut driver = ToastDriver::new(ToastState::with_dismiss_ms(100), rx);
        let event = ToastEvent::BackpressureRejected {
            entity_id: EntityId::from("light.kitchen"),
            scope: BackpressureScope::PerEntity,
        };
        tx.send(event).await.unwrap();

        let now = Instant::now();
        driver.poll_with_now(now);
        assert!(driver.state().is_visible());

        // Advance past the dismiss threshold.
        let later = now + Duration::from_millis(150);
        let changed = driver.poll_with_now(later);
        assert!(changed, "auto-dismiss must report state change");
        assert!(!driver.state().is_visible());
    }

    #[tokio::test]
    async fn driver_drains_multiple_queued_events_per_poll() {
        // Backlog test: two events arrive between polls. The driver
        // applies them in order, so the LATER event is the visible one
        // (newer-replaces-older).
        let (tx, rx) = mpsc::channel::<ToastEvent>(4);
        let mut driver = ToastDriver::new(ToastState::new(), rx);

        for entity in &["light.a", "light.b", "light.c"] {
            tx.send(ToastEvent::BackpressureRejected {
                entity_id: EntityId::from(*entity),
                scope: BackpressureScope::PerEntity,
            })
            .await
            .unwrap();
        }

        driver.poll_with_now(Instant::now());
        assert_eq!(
            driver.state().visible_text(),
            Some("Action queue full for `light.c`"),
            "the latest of a backlog must be the visible toast"
        );
    }

    #[tokio::test]
    async fn driver_tick_with_no_events_returns_false_when_no_visible_toast() {
        let (_tx, rx) = mpsc::channel::<ToastEvent>(4);
        let mut driver = ToastDriver::new(ToastState::new(), rx);
        let changed = driver.poll_with_now(Instant::now());
        assert!(!changed, "quiet poll must report no change");
    }

    #[tokio::test]
    async fn driver_poll_uses_real_instant_now_under_the_hood() {
        // Coverage for the production wrapper `poll()` (which delegates
        // to `poll_with_now(Instant::now())`). We push an event and
        // immediately call `poll()` — the visible toast must come up.
        // This proves the `poll()` path is wired correctly without
        // relying on `poll_with_now` having different behaviour.
        let (tx, rx) = mpsc::channel::<ToastEvent>(4);
        let mut driver = ToastDriver::new(ToastState::with_dismiss_ms(60_000), rx);
        tx.send(ToastEvent::BackpressureRejected {
            entity_id: EntityId::from("light.kitchen"),
            scope: BackpressureScope::PerEntity,
        })
        .await
        .unwrap();

        let changed = driver.poll();
        assert!(changed, "poll() must consume the event and report change");
        assert!(driver.state().is_visible());
    }

    #[tokio::test]
    async fn driver_state_mut_supports_navigate_clear() {
        // The view-router integration calls `state_mut().clear_for_navigate()`
        // when a Navigate outcome fires. Verify the path works through the
        // public mutable accessor.
        let (tx, rx) = mpsc::channel::<ToastEvent>(4);
        let mut driver = ToastDriver::new(ToastState::new(), rx);
        tx.send(ToastEvent::BackpressureRejected {
            entity_id: EntityId::from("light.kitchen"),
            scope: BackpressureScope::PerEntity,
        })
        .await
        .unwrap();
        driver.poll_with_now(Instant::now());
        assert!(driver.state().is_visible());

        let cleared = driver.state_mut().clear_for_navigate();
        assert!(cleared);
        assert!(!driver.state().is_visible());
    }

    // ── apply_pending_for_widgets defensive cases (no Slint window) ───
    //
    // The `apply_pending_for_widgets` function requires a Slint window —
    // exercised by the integration tests under
    // `tests/integration/toast_spinner.rs`. The type alias below keeps
    // `LiveStore` referenced in the unit-test module so the import is
    // not flagged as unused, without requiring a window construction.

    #[test]
    fn live_store_pending_for_widget_default_is_false() {
        // The cross-owner read API is the binding source for the spinner.
        // Verify the default-empty-store / default-no-map case returns
        // false (steady-state path).
        use crate::actions::map::WidgetActionMap;
        use std::sync::Arc;
        let store = LiveStore::new();
        let widget_id = WidgetId::from("kitchen_light");
        // No WidgetActionMap installed → must return false.
        assert!(!store.pending_for_widget(&widget_id));

        // Installing an empty WidgetActionMap also returns false.
        let map = WidgetActionMap::new();
        store.set_widget_action_map(Arc::new(map));
        assert!(!store.pending_for_widget(&widget_id));
    }
}
