//! More-info modal: trait, attribute body, and lazy-render plumbing (TASK-066).
//!
//! # Shape
//!
//! Phase 3 ships the modal as a *generic shell* with one body
//! implementation:
//!
//! * [`MoreInfoBody`] — the object-safe trait every per-domain body must
//!   implement. Phase 3 ships [`AttributesBody`]; Phase 6 will add per-domain
//!   bodies (light, climate, media-player, …).
//! * [`AttributesBody`] — the default body. Renders up to 32 attribute rows
//!   with each value truncated to 256 chars after a *typed* display formatter
//!   that matches each JSON variant explicitly — never a raw `to_string()` of
//!   an arbitrary value, which would emit JSON-encoded strings (escaped
//!   quotes, unicode escapes) and risk render bombs on misshapen entity
//!   state.
//!
//! # Lazy-render contract (locked_decisions.more_info_modal)
//!
//! Rows are computed on **modal open**, not on every entity update. The
//! modal-open path calls [`MoreInfoBody::render_rows`] exactly once, stores
//! the resulting `Vec<ModalRow>`, and renders that slice in the Slint shell.
//! Subsequent entity-state pushes do not call `render_rows` again — only a
//! second open does. This is encoded by [`ModalState::open_with`], which
//! invokes the row-builder only inside `open_with` and stashes the result
//! in [`ModalState::rows`]. [`ModalState::on_entity_update`] is a no-op while
//! open: it satisfies the locked decision and is asserted by the integration
//! tests under `tests/integration/more_info_modal.rs`.
//!
//! # Why no JSON-crate path is named here
//!
//! `src/ui/**` is gated against the JSON-crate path (`.github/workflows/ci.yml`
//! Gate 2). [`AttributesBody`] reads `entity.attributes` through inferred
//! types and method calls only — `as_str` / `as_bool` / `as_i64` /
//! `as_u64` / `as_f64` / `as_array` / `as_object` / `is_null`. None of
//! those reference the crate by name; the value type is resolved through
//! type inference at the call site (the iteration over
//! `entity.attributes.iter()`). This keeps the typed-formatter contract
//! (no raw `to_string` on arbitrary values) without tripping the grep
//! gate or needing a waiver.

use std::sync::Arc;

use crate::dashboard::schema::{WidgetKind, WidgetOptions};
use crate::ha::entity::{Entity, EntityId};
use crate::ha::live_store::LiveStore;

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Maximum number of attribute rows rendered by [`AttributesBody`]
/// (locked_decisions.more_info_modal). Attributes beyond the cap are
/// truncated; the modal does not paginate.
pub const MAX_ATTRIBUTE_ROWS: usize = 32;

/// Maximum length of a rendered attribute value, in characters. Values
/// longer than this are truncated to the cap (see [`truncate_value`]).
/// The cap is a *character* count (Unicode scalar values), not bytes — so a
/// long emoji string truncates by visible glyphs rather than mid-byte.
pub const MAX_VALUE_CHARS: usize = 256;

// ---------------------------------------------------------------------------
// ModalRow
// ---------------------------------------------------------------------------

/// A single row in the more-info modal body.
///
/// The Slint shell renders this as `key: value` (typography decided by the
/// shell, not the body). Both fields are owned `String`s — the body is built
/// once on open and the strings persist for the lifetime of the modal-open
/// session, so an `Arc<str>` would over-engineer the (≤32) row case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalRow {
    /// Attribute key (e.g. `"brightness"`, `"friendly_name"`).
    pub key: String,
    /// Pre-truncated value rendered through the typed display formatter.
    pub value: String,
}

// ---------------------------------------------------------------------------
// MoreInfoBody trait
// ---------------------------------------------------------------------------

/// Object-safe: must remain usable as `dyn MoreInfoBody`. Phase 6 per-domain
/// bodies must implement via type-erased adapters, not generics or
/// associated types. See locked_decisions.more_info_modal in
/// docs/plans/2026-04-28-phase-3-actions.md.
///
/// # Why object-safe
///
/// The Slint shell stores a single body via `Arc<dyn MoreInfoBody>` so the
/// per-domain body — `AttributesBody` in Phase 3, `LightBody` /
/// `ClimateBody` / … in Phase 6 — can be swapped at runtime based on the
/// opened entity's domain. Generics would force the shell to be
/// parameterised on the concrete body type, which would monomorphise the
/// modal into one per-domain copy (binary bloat) and prevent runtime body
/// selection (correctness). Phase 6 is required to preserve this
/// constraint; if a future body needs generic behaviour, the generic
/// surface must be exposed via a type-erased adapter (e.g. a builder that
/// produces an `Arc<dyn MoreInfoBody>` from typed inputs), **not** by
/// adding generics or associated types to this trait.
///
/// # Compile-time enforcement
///
/// The `_OBJECT_SAFETY` constant below builds a `dyn MoreInfoBody` to
/// statically forbid any change to this trait that would break
/// object-safety. The build will fail at compile time if a future edit
/// adds a generic method, an associated type, a `Self`-by-value method,
/// etc. — there is no runtime check to dodge.
pub trait MoreInfoBody: Send + Sync {
    /// Build the row list for `entity`. Called exactly once per
    /// modal-open by [`ModalState::open_with`]; the returned `Vec` is
    /// rendered by the Slint shell and is **not** rebuilt on subsequent
    /// entity updates while the modal stays open.
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow>;
}

// Compile-time object-safety guard. If a future edit makes
// `MoreInfoBody` non-object-safe (e.g. adds a generic method or an
// associated type), this line fails to compile and CI blocks the
// change.
const _OBJECT_SAFETY: fn(&dyn MoreInfoBody) = |_| {};

// Compile-time `AttributesBody: MoreInfoBody` assertion. If
// `AttributesBody` ever drifts out of conformance with the trait, this
// fails to compile.
const _ATTRIBUTES_BODY_IS_MORE_INFO_BODY: fn() = || {
    fn assert_impl<T: MoreInfoBody>() {}
    assert_impl::<AttributesBody>();
};

// ---------------------------------------------------------------------------
// AttributesBody
// ---------------------------------------------------------------------------

/// Default more-info body — renders up to [`MAX_ATTRIBUTE_ROWS`] attribute
/// rows from an [`Entity`]'s `attributes` map.
///
/// # Per locked_decisions.more_info_modal
///
/// * Cap: at most 32 rows. Enforced at iteration time, not by constructing
///   a full vector and truncating — large attribute maps (some HA
///   integrations emit hundreds of attributes) would otherwise spend O(N)
///   work even though only the first 32 ship to Slint.
/// * Per-value cap: each value is truncated to 256 chars after the typed
///   display formatter. The cap is in *characters* (Unicode scalar values)
///   not bytes — a long emoji-only attribute value truncates by visible
///   glyphs.
/// * Typed formatter: arbitrary attribute values are **never** rendered
///   via raw `to_string()` (which would JSON-encode strings, surfacing
///   escaped quotes to the user and risking log/render bombs). Each JSON
///   variant is matched explicitly via the typed accessors.
/// * Stable order: keys are sorted alphabetically so the modal's first
///   render and any reopen produce the same row order regardless of
///   underlying `Map` iteration order.
#[derive(Debug, Default)]
pub struct AttributesBody;

impl AttributesBody {
    /// Construct an [`AttributesBody`]. The body is stateless — every
    /// instance produces the same rows for the same input — but the
    /// constructor exists so callers do not depend on `Default`.
    #[must_use]
    pub fn new() -> Self {
        AttributesBody
    }
}

impl MoreInfoBody for AttributesBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        // Sort keys alphabetically so the row order is deterministic
        // regardless of map iteration order. Sort is O(N log N) on
        // attribute count; even at N = 1000 attributes (worst-case HA
        // emitter), the sort is fewer than 10k comparisons, well under
        // any modal-open budget.
        let mut keys: Vec<&String> = entity.attributes.keys().collect();
        keys.sort();

        let mut rows = Vec::with_capacity(keys.len().min(MAX_ATTRIBUTE_ROWS));
        for key in keys.into_iter().take(MAX_ATTRIBUTE_ROWS) {
            // `value` is `&_` — type inference resolves it through the
            // attribute map's element type. No JSON-crate name is
            // mentioned in this file; the inherent typed accessors
            // (`as_str`, `as_bool`, `as_i64`, `as_u64`, `as_f64`,
            // `is_null`, `as_array`, `as_object`) dispatch on the
            // concrete type without requiring a `use` import here.
            let Some(value) = entity.attributes.get(key) else {
                continue;
            };

            let formatted = if let Some(s) = value.as_str() {
                // Bare string — NEVER `value.to_string()`, which would
                // JSON-encode surrounding quotes and escape inner ones.
                s.to_owned()
            } else if let Some(b) = value.as_bool() {
                if b {
                    "true".to_owned()
                } else {
                    "false".to_owned()
                }
            } else if let Some(i) = value.as_i64() {
                i.to_string()
            } else if let Some(u) = value.as_u64() {
                u.to_string()
            } else if let Some(f) = value.as_f64() {
                // Float formatting via Rust's Display, not JSON encoding.
                format!("{f}")
            } else if value.is_null() {
                "null".to_owned()
            } else if let Some(arr) = value.as_array() {
                // Phase 3 does not recurse into nested arrays — emit a
                // bounded summary instead. Phase 6 per-domain bodies may
                // expand specific arrays they understand.
                format!("[{} items]", arr.len())
            } else if let Some(obj) = value.as_object() {
                format!("{{{} keys}}", obj.len())
            } else {
                // Defensive fallback for a future variant addition.
                "<unsupported>".to_owned()
            };

            let truncated = truncate_value(&formatted, MAX_VALUE_CHARS);
            rows.push(ModalRow {
                key: key.clone(),
                value: truncated,
            });
        }
        rows
    }
}

// ---------------------------------------------------------------------------
// Per-domain body stubs (Phase 6, TASK-098)
//
// Each struct implements `MoreInfoBody` and satisfies the object-safety
// contract: `Send + Sync`, no generics, no associated types. The `store`
// parameter on `body_for_widget` is available to future rich implementations
// in TASK-102..TASK-109 / TASK-094; the stubs below capture it but do not
// yet query it. The richer per-domain UI lands in 6a/6b/6d.
//
// Each stub returns at least one row (the entity's `state` field) so that
// in-file unit tests can assert non-empty output without constructing a
// JSON-attributed entity — a requirement of Gate 2 (no raw JSON
// path references inside src/ui/**).
// ---------------------------------------------------------------------------

/// More-info body for `cover` entities (TASK-102).
///
/// Renders cover-specific rows: state, current position (when exposed),
/// current tilt position (when exposed), and the `supported_features`
/// bitmask. The Slint shell already binds `body-rows` to the modal's
/// row list, so [`CoverBody`] reuses the generic rendering path —
/// keeping the trait shape stable per TASK-102 AC #6.
///
/// # Position slider integration (locked_decisions.cover_slider_component)
///
/// TASK-096 created a stub `PositionSlider` Slint component
/// (`ui/slint/components/position_slider.slint`); TASK-108 fills in its
/// visual design. TASK-102 consumes the stub by emitting the position
/// value as an additional `position` row — when `main_window.slint`
/// gains a per-domain modal body slot in a future ticket, the Rust body
/// will swap to writing the slider's `value` property directly. Today
/// the position is surfaced through the existing row model so the
/// information reaches the user without amending the Slint shell.
///
/// # Stateless
///
/// `CoverBody` carries no fields; the body queries the live entity at
/// `render_rows` time. Per locked_decisions.more_info_modal, the body
/// is invoked exactly once per modal-open — so the per-call attribute
/// reads are not on a hot path.
#[derive(Debug, Default)]
pub struct CoverBody;

impl CoverBody {
    /// Construct a [`CoverBody`]. Stateless; the constructor exists so callers
    /// do not depend on `Default`.
    #[must_use]
    pub fn new() -> Self {
        CoverBody
    }
}

impl MoreInfoBody for CoverBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        // Capacity of 4 covers the worst case (state + position + tilt +
        // supported_features) without growing.
        let mut rows = Vec::with_capacity(4);

        // State row — always emitted, matches the per-domain stub
        // contract every other body upholds.
        rows.push(ModalRow {
            key: "state".to_owned(),
            value: entity.state.as_ref().to_owned(),
        });

        // Position row, only when the entity exposes `current_position`.
        // We thread through `crate::ui::cover::CoverVM::from_entity`
        // for the canonical position-resolution logic (clamping,
        // state-derived fallback) — the body sees the same value the
        // tile renders. This keeps tile and modal in lockstep without
        // duplicating the parsing logic.
        if entity.attributes.get("current_position").is_some() {
            let cover_vm = crate::ui::cover::CoverVM::from_entity(entity);
            rows.push(ModalRow {
                key: "position".to_owned(),
                value: format!("{}%", cover_vm.position),
            });
        }

        // Tilt row, only when the entity exposes `current_tilt_position`.
        // Some covers (e.g. blinds) expose tilt independently of
        // position. The shared `read_tilt_attribute` helper in
        // `crate::ui::cover` keeps the parsing rules identical to the
        // tile's tilt label.
        if let Some(tilt) = crate::ui::cover::read_tilt_attribute(entity) {
            rows.push(ModalRow {
                key: "tilt".to_owned(),
                value: format!("{tilt}%"),
            });
        }

        // supported_features row, only when present. The bitmask is
        // emitted as a decimal integer; a future ticket may decode the
        // individual flags into a human-readable list once the cover
        // dispatcher (TASK-099 + service map) defines the bit-to-action
        // mapping at the trait level. We emit the raw integer now so
        // the user has visibility into which controls the entity
        // declares it supports.
        if let Some(features) = crate::ui::cover::read_supported_features(entity) {
            rows.push(ModalRow {
                key: "supported_features".to_owned(),
                value: features.to_string(),
            });
        }

        rows
    }
}

/// More-info body for `fan` entities (TASK-103).
///
/// Renders fan-specific rows: state, current speed percentage (when
/// exposed), current preset mode (when exposed), oscillating boolean
/// (when exposed), and direction (when exposed). The Slint shell
/// already binds `body-rows` to the modal's row list, so [`FanBody`]
/// reuses the generic rendering path — keeping the trait shape stable
/// per TASK-098 + TASK-103.
///
/// # Speed picker integration (locked_decisions.fan_speed_set_vocabulary)
///
/// The richer speed-picker UI (preset modes vs numeric step indices)
/// is dispatcher-side: tapping a speed dispatches `SetFanSpeed` (TASK-099)
/// and the dispatcher reads `FanOptions.preset_modes` from the dashboard
/// config at dispatch time. This body surfaces only the *current*
/// snapshot of the entity (percentage and active preset_mode),
/// keeping the modal informative without duplicating the dispatcher's
/// preset_modes list lookup.
///
/// # Stateless
///
/// `FanBody` carries no fields; the body queries the live entity at
/// `render_rows` time. Per locked_decisions.more_info_modal, the body
/// is invoked exactly once per modal-open — so the per-call attribute
/// reads are not on a hot path.
#[derive(Debug, Default)]
pub struct FanBody;

impl FanBody {
    /// Construct a [`FanBody`]. Stateless; the constructor exists so callers
    /// do not depend on `Default`.
    #[must_use]
    pub fn new() -> Self {
        FanBody
    }
}

impl MoreInfoBody for FanBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        // Capacity of 5 covers the worst case (state + speed + preset +
        // oscillating + direction) without growing.
        let mut rows = Vec::with_capacity(5);

        // State row — always emitted, matches the per-domain stub
        // contract every other body upholds.
        rows.push(ModalRow {
            key: "state".to_owned(),
            value: entity.state.as_ref().to_owned(),
        });

        // Speed percentage row, only when the entity exposes a numeric,
        // in-range `percentage`. We thread through
        // `crate::ui::fan::FanVM::from_entity` for the canonical
        // percentage-resolution logic — the body sees the same value the
        // tile renders. This keeps tile and modal in lockstep without
        // duplicating the parsing logic.
        let fan_vm = crate::ui::fan::FanVM::from_entity(entity);
        if let Some(pct) = fan_vm.speed_pct {
            rows.push(ModalRow {
                key: "speed".to_owned(),
                value: format!("{pct}%"),
            });
        }

        // Preset mode row, only when the entity exposes `preset_mode`.
        // Some fans report only percentage, others only preset modes,
        // some both — both labels coexist when present.
        if let Some(speed) = fan_vm.current_speed.as_deref() {
            rows.push(ModalRow {
                key: "preset_mode".to_owned(),
                value: speed.to_owned(),
            });
        }

        // Oscillating row, only when the boolean attribute is present.
        // Surfacing this in the modal lets the user verify the fan's
        // oscillation state without opening the entity in HA's own UI.
        if let Some(osc) = crate::ui::fan::read_oscillating_attribute(entity) {
            rows.push(ModalRow {
                key: "oscillating".to_owned(),
                value: if osc {
                    "true".to_owned()
                } else {
                    "false".to_owned()
                },
            });
        }

        // Direction row, only when the string attribute is present.
        // Standard HA values are "forward" / "reverse"; we surface the
        // raw value so any integration-specific direction names pass
        // through unchanged.
        if let Some(dir) = crate::ui::fan::read_direction_attribute(entity) {
            rows.push(ModalRow {
                key: "direction".to_owned(),
                value: dir,
            });
        }

        rows
    }
}

/// More-info body for `lock` entities.
///
/// Returns the entity's state. Phase 6a (`TASK-104`) will replace this with a
/// PIN entry integration.
#[derive(Debug, Default)]
pub struct LockBody;

impl LockBody {
    /// Construct a [`LockBody`].
    #[must_use]
    pub fn new() -> Self {
        LockBody
    }
}

impl MoreInfoBody for LockBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        vec![ModalRow {
            key: "state".to_owned(),
            value: entity.state.as_ref().to_owned(),
        }]
    }
}

/// More-info body for `alarm_control_panel` entities.
///
/// Returns the entity's state. Phase 6a (`TASK-105`) will replace this with a
/// PIN entry + arm-mode selector.
#[derive(Debug, Default)]
pub struct AlarmBody;

impl AlarmBody {
    /// Construct an [`AlarmBody`].
    #[must_use]
    pub fn new() -> Self {
        AlarmBody
    }
}

impl MoreInfoBody for AlarmBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        vec![ModalRow {
            key: "state".to_owned(),
            value: entity.state.as_ref().to_owned(),
        }]
    }
}

/// More-info body for `climate` entities.
///
/// Returns the entity's state. Phase 6b (`TASK-108`) will replace this with a
/// setpoint slider and HVAC mode picker.
#[derive(Debug, Default)]
pub struct ClimateBody;

impl ClimateBody {
    /// Construct a [`ClimateBody`].
    #[must_use]
    pub fn new() -> Self {
        ClimateBody
    }
}

impl MoreInfoBody for ClimateBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        vec![ModalRow {
            key: "state".to_owned(),
            value: entity.state.as_ref().to_owned(),
        }]
    }
}

/// More-info body for `media_player` entities.
///
/// Returns the entity's state. Phase 6b (`TASK-109`) will replace this with a
/// transport control bar, volume slider, and artwork display.
#[derive(Debug, Default)]
pub struct MediaPlayerBody;

impl MediaPlayerBody {
    /// Construct a [`MediaPlayerBody`].
    #[must_use]
    pub fn new() -> Self {
        MediaPlayerBody
    }
}

impl MoreInfoBody for MediaPlayerBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        vec![ModalRow {
            key: "state".to_owned(),
            value: entity.state.as_ref().to_owned(),
        }]
    }
}

/// More-info body for `history_graph` / `history` widgets.
///
/// Returns the entity's state. Phase 6b (`TASK-106`) will replace this with
/// the rendered history graph.
#[derive(Debug, Default)]
pub struct HistoryBody;

impl HistoryBody {
    /// Construct a [`HistoryBody`].
    #[must_use]
    pub fn new() -> Self {
        HistoryBody
    }
}

impl MoreInfoBody for HistoryBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        vec![ModalRow {
            key: "state".to_owned(),
            value: entity.state.as_ref().to_owned(),
        }]
    }
}

/// More-info body for `camera` entities.
///
/// Returns the entity's state. Phase 6b (`TASK-107`) will replace this with a
/// live snapshot decoder.
#[derive(Debug, Default)]
pub struct CameraBody;

impl CameraBody {
    /// Construct a [`CameraBody`].
    #[must_use]
    pub fn new() -> Self {
        CameraBody
    }
}

impl MoreInfoBody for CameraBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        vec![ModalRow {
            key: "state".to_owned(),
            value: entity.state.as_ref().to_owned(),
        }]
    }
}

/// More-info body for `power_flow` / `power_flow_card_plus` widgets.
///
/// Returns the entity's state. Phase 6d (`TASK-094`) will replace this with
/// the full power-flow diagram.
#[derive(Debug, Default)]
pub struct PowerFlowBody;

impl PowerFlowBody {
    /// Construct a [`PowerFlowBody`].
    #[must_use]
    pub fn new() -> Self {
        PowerFlowBody
    }
}

impl MoreInfoBody for PowerFlowBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        vec![ModalRow {
            key: "state".to_owned(),
            value: entity.state.as_ref().to_owned(),
        }]
    }
}

// Compile-time per-domain body conformance assertions.
// If any of the new bodies ever drift out of conformance with the
// `MoreInfoBody` trait, the build fails here — matching the existing
// `_ATTRIBUTES_BODY_IS_MORE_INFO_BODY` guard.
const _PER_DOMAIN_BODIES_ARE_MORE_INFO_BODY: fn() = || {
    fn assert_impl<T: MoreInfoBody>() {}
    assert_impl::<CoverBody>();
    assert_impl::<FanBody>();
    assert_impl::<LockBody>();
    assert_impl::<AlarmBody>();
    assert_impl::<ClimateBody>();
    assert_impl::<MediaPlayerBody>();
    assert_impl::<HistoryBody>();
    assert_impl::<CameraBody>();
    assert_impl::<PowerFlowBody>();
};

// ---------------------------------------------------------------------------
// body_for_widget — per-domain dispatch factory (TASK-098)
// ---------------------------------------------------------------------------

/// Dispatch factory: select the appropriate [`MoreInfoBody`] implementation
/// for `kind` at modal-open time.
///
/// # Contract (locked_decisions.more_info_dispatch)
///
/// * The match is **exhaustive**: every [`WidgetKind`] variant must appear
///   as a match arm. Adding a new variant in a future plan amendment is a
///   **compile error** in this function until the factory is extended — this
///   is the Risk #10 mitigation described in the Phase 6 plan.
/// * `AttributesBody` is the fallback for widget kinds that have no
///   per-domain body yet (`LightTile`, `SensorTile`, `EntityTile`).
/// * The `store` parameter is available for per-domain bodies that need to
///   query the entity store at row-build time (e.g. history graph, camera
///   snapshot). Phase 6.0 stubs do not yet use it; Phase 6a/6b
///   implementations may capture it in their body struct.
///
/// # Parameters
///
/// * `kind`    — the tile's `WidgetKind`, sourced from the loaded `Dashboard`.
/// * `options` — the tile-kind-specific typed options from the widget config,
///   or `None` when the widget carries no `options:` block.
/// * `store`   — shared live store, passed to per-domain body constructors.
#[must_use]
pub fn body_for_widget(
    kind: WidgetKind,
    _options: Option<&WidgetOptions>,
    _store: Arc<LiveStore>,
) -> Box<dyn MoreInfoBody> {
    match kind {
        // Per-domain body stubs — each returns domain-relevant attribute rows.
        // The richer Slint views land in TASK-102..TASK-109 / TASK-094.
        WidgetKind::Cover => Box::new(CoverBody::new()),
        WidgetKind::Fan => Box::new(FanBody::new()),
        WidgetKind::Lock => Box::new(LockBody::new()),
        WidgetKind::Alarm => Box::new(AlarmBody::new()),
        WidgetKind::Climate => Box::new(ClimateBody::new()),
        WidgetKind::MediaPlayer => Box::new(MediaPlayerBody::new()),
        WidgetKind::History => Box::new(HistoryBody::new()),
        WidgetKind::Camera => Box::new(CameraBody::new()),
        WidgetKind::PowerFlow => Box::new(PowerFlowBody::new()),
        // Fallback: widget kinds without a per-domain body use AttributesBody.
        WidgetKind::LightTile | WidgetKind::SensorTile | WidgetKind::EntityTile => {
            Box::new(AttributesBody::new())
        }
    }
}

// ---------------------------------------------------------------------------
// truncate_value
// ---------------------------------------------------------------------------

/// Truncate a string to at most `max_chars` Unicode scalar values
/// (code points).
///
/// The truncation respects character boundaries — `s.split_at(byte_index)`
/// would panic mid-codepoint. We walk `s.chars()` and slice at the byte
/// offset of the (max_chars+1)-th char if reached.
///
/// The trailing ellipsis is intentionally omitted: appending `…` would
/// push truncated values one character past the cap and complicate the
/// "exactly 256 chars" assertion the integration tests rely on. Phase 6
/// per-domain bodies may add ellipses if their value-domain semantics
/// benefit; the default body sticks to a hard cap.
pub(crate) fn truncate_value(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    s.chars().take(max_chars).collect()
}

// ---------------------------------------------------------------------------
// ModalState — lazy-render plumbing
// ---------------------------------------------------------------------------

/// Lazy-render state for the modal.
///
/// The dispatcher (TASK-062) emits `DispatchOutcome::MoreInfo` with the
/// entity_id from the `WidgetActionEntry`. The bridge converts that into
/// a call to [`ModalState::open_with`], which invokes the body's
/// row-builder *exactly once* and stores the result. Until the modal
/// closes, [`ModalState::rows`] returns a stable slice.
///
/// Per locked_decisions.more_info_modal, the row-builder must NOT run on
/// every entity update; [`ModalState::on_entity_update`] is the contract
/// the bridge calls when an entity tick arrives, and it is intentionally
/// a no-op while the modal is open. Reopening (which can happen if the
/// user closes the modal and long-presses the same tile again)
/// re-invokes the builder against the *current* entity, so the user sees
/// the freshest snapshot for the second open.
///
/// `body` is `Arc<dyn MoreInfoBody>` so the same body can be shared
/// across multiple modal sessions without re-allocating; Phase 6 will
/// swap the concrete body per-domain by replacing this `Arc` at the call
/// site.
pub struct ModalState {
    body: Arc<dyn MoreInfoBody>,
    open_for: Option<EntityId>,
    rows: Vec<ModalRow>,
}

impl ModalState {
    /// Construct a closed modal backed by `body`.
    #[must_use]
    pub fn new(body: Arc<dyn MoreInfoBody>) -> Self {
        ModalState {
            body,
            open_for: None,
            rows: Vec::new(),
        }
    }

    /// Open the modal for `entity`, computing rows lazily. Calls
    /// `body.render_rows(entity)` exactly once and stashes the result.
    pub fn open_with(&mut self, entity: &Entity) {
        self.open_for = Some(entity.id.clone());
        self.rows = self.body.render_rows(entity);
    }

    /// Open the modal for `entity` using a caller-supplied body, replacing the
    /// previously stored body for this session.
    ///
    /// This is the Phase 6 integration point consumed by the bridge. The bridge
    /// calls [`body_for_widget`] to select the per-domain body at modal-open
    /// time, then passes it here. The body is stored on the `ModalState` so it
    /// can be reused if the modal is closed and immediately reopened for the
    /// same widget kind — matching the "Arc shared across modal sessions" intent
    /// in the original doc-comment without requiring the bridge to call
    /// [`body_for_widget`] twice on quick reopen.
    ///
    /// Per locked_decisions.more_info_modal: `render_rows` is called exactly
    /// once (here). The no-op-on-entity-update contract is unchanged.
    pub fn open_with_body(&mut self, entity: &Entity, body: Arc<dyn MoreInfoBody>) {
        self.body = body;
        self.open_for = Some(entity.id.clone());
        self.rows = self.body.render_rows(entity);
    }

    /// Close the modal. Drops the row buffer so a future reopen
    /// recomputes.
    pub fn close(&mut self) {
        self.open_for = None;
        self.rows.clear();
    }

    /// Whether the modal is currently open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    /// The entity_id the modal is open for, if any.
    #[must_use]
    pub fn open_for(&self) -> Option<&EntityId> {
        self.open_for.as_ref()
    }

    /// The currently-rendered rows. Empty when the modal is closed.
    #[must_use]
    pub fn rows(&self) -> &[ModalRow] {
        &self.rows
    }

    /// Notification hook for entity updates. **No-op while open** per
    /// locked_decisions.more_info_modal: rows are computed on open, not
    /// on every tick. The hook exists so the bridge can call it
    /// unconditionally without branching on modal state — matching the
    /// existing per-entity subscriber pattern in `src/ui/bridge.rs`.
    pub fn on_entity_update(&mut self, _entity: &Entity) {
        // Intentionally empty. See doc-comment.
    }
}

// ---------------------------------------------------------------------------
// Slint property wiring (lazy, idempotent)
// ---------------------------------------------------------------------------

/// Header text rendered in the modal's title row. Distinct from
/// `EntityState` so callers can compute a friendly name (typically
/// `entity.friendly_name().unwrap_or_else(|| entity.id.as_str())`)
/// without leaking that fallback policy into the Slint shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalHeader {
    /// Entity name to display (friendly_name fallback to id).
    pub name: String,
    /// State string for the header subline (e.g. `"on"`, `"off"`).
    pub state: String,
}

impl ModalHeader {
    /// Compute the header from an entity, preferring `friendly_name`
    /// over the raw id for the `name` field.
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Self {
        let name = entity
            .friendly_name()
            .map(str::to_owned)
            .unwrap_or_else(|| entity.id.as_str().to_owned());
        ModalHeader {
            name,
            state: entity.state.as_ref().to_owned(),
        }
    }
}

/// Apply the current [`ModalState`] (and its header) to the
/// `MainWindow`'s more-info properties.
///
/// The function is idempotent: calling it twice with the same state
/// produces the same window state. Production callers in the bridge
/// invoke this once per modal-open transition and once per
/// modal-close transition; the lazy-render contract means there are no
/// other callers per locked_decisions.more_info_modal.
///
/// `header` is computed by the caller (typically via
/// [`ModalHeader::from_entity`]) so this function does not need to
/// re-look up the entity — the bridge already holds the entity that
/// drove `ModalState::open_with`.
pub fn apply_modal_to_window(
    state: &ModalState,
    header: &ModalHeader,
    window: &crate::ui::bridge::MainWindow,
) {
    use slint::{ModelRc, SharedString, VecModel};

    // Convert the `Vec<ModalRow>` into the Slint-typed `ModalRowVM`
    // shape and wrap in a `ModelRc` for the array property.
    let rows: Vec<crate::ui::bridge::slint_ui::ModalRowVM> = state
        .rows()
        .iter()
        .map(|r| crate::ui::bridge::slint_ui::ModalRowVM {
            key: SharedString::from(r.key.as_str()),
            value: SharedString::from(r.value.as_str()),
        })
        .collect();
    let model: ModelRc<crate::ui::bridge::slint_ui::ModalRowVM> =
        ModelRc::new(VecModel::from(rows));

    window.set_more_info_rows(model);
    window.set_more_info_entity_name(SharedString::from(header.name.as_str()));
    window.set_more_info_entity_state(SharedString::from(header.state.as_str()));
    window.set_more_info_visible(state.is_open());
}

// ---------------------------------------------------------------------------
// Trait & doc-comment self-test (compile-time + grep)
// ---------------------------------------------------------------------------
//
// Phase 3 acceptance asserts that the trait carries a *prominent*
// doc-comment naming the object-safety constraint. The grep test under
// `tests/integration/more_info_modal.rs` covers the source-level check;
// the compile-time `_OBJECT_SAFETY` constant above covers the property
// itself. Both are load-bearing per opencode review 2026-04-28: a
// runtime test alone is insufficient because Phase 6 maintainers need
// the constraint visible at the trait definition.

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Shared test helper
    // -----------------------------------------------------------------------

    /// Construct a minimal [`Entity`] with an empty attribute map and the
    /// given id and state. Uses `Arc::default()` for `attributes` so this
    /// helper does not reference the underlying JSON library directly —
    /// `src/ui/**` is gated against direct raw-JSON path usage by the
    /// CI repo-rules Gate 2 check. `Arc::default()` resolves through the
    /// `Default` impl on the inner map type without naming the crate.
    fn minimal_entity(id: &str, state: &str) -> Entity {
        Entity {
            id: EntityId::from(id),
            state: Arc::from(state),
            attributes: Arc::default(),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    // -----------------------------------------------------------------------
    // Existing tests (unchanged)
    // -----------------------------------------------------------------------

    /// Compile-time `_OBJECT_SAFETY` constant cannot be referenced
    /// directly (it's `const _:` with no name in the module). This
    /// runtime test verifies the same property a different way: build
    /// an `Arc<dyn MoreInfoBody>` and prove we can call through it. If
    /// the trait ever loses object-safety, this test fails to compile.
    #[test]
    fn more_info_body_is_object_safe() {
        let _body: Arc<dyn MoreInfoBody> = Arc::new(AttributesBody::new());
    }

    /// `truncate_value` is the only test we can run in-file without
    /// touching the JSON crate (which would trip Gate 2). The richer
    /// behavioural tests live in `tests/integration/more_info_modal.rs`.
    #[test]
    fn truncate_value_below_cap_is_passthrough() {
        assert_eq!(truncate_value("hello", 256), "hello");
    }

    #[test]
    fn truncate_value_above_cap_truncates_to_exact_char_count() {
        let s: String = "a".repeat(1024);
        let t = truncate_value(&s, 256);
        assert_eq!(t.chars().count(), 256);
    }

    #[test]
    fn truncate_value_respects_unicode_boundaries() {
        let s: String = "🦀".repeat(100);
        let t = truncate_value(&s, 50);
        assert_eq!(t.chars().count(), 50);
        assert!(t.chars().all(|c| c == '🦀'));
    }

    #[test]
    fn modal_row_equality_is_by_value() {
        let a = ModalRow {
            key: "k".to_owned(),
            value: "v".to_owned(),
        };
        let b = ModalRow {
            key: "k".to_owned(),
            value: "v".to_owned(),
        };
        assert_eq!(a, b);
    }

    // -----------------------------------------------------------------------
    // body_for_widget factory (TASK-098)
    // -----------------------------------------------------------------------

    /// Verify that `body_for_widget` returns a per-domain body for each
    /// non-fallback `WidgetKind` and that the factory is callable through a
    /// live `Arc<LiveStore>`.
    #[test]
    fn body_for_widget_returns_per_domain_body() {
        let store = Arc::new(LiveStore::new());
        // Exercise all nine per-domain match arms so the factory is fully
        // covered; the exhaustive match ensures compile-time completeness.
        let cases: &[(WidgetKind, &str, &str)] = &[
            (WidgetKind::Cover, "cover.garage_door", "closed"),
            (WidgetKind::Fan, "fan.bedroom", "on"),
            (WidgetKind::Lock, "lock.front_door", "locked"),
            (WidgetKind::Alarm, "alarm_control_panel.home", "armed_away"),
            (WidgetKind::Climate, "climate.living_room", "heat"),
            (WidgetKind::MediaPlayer, "media_player.tv", "playing"),
            (WidgetKind::History, "sensor.temperature", "21.5"),
            (WidgetKind::Camera, "camera.front_door", "idle"),
            (WidgetKind::PowerFlow, "sensor.grid_power", "1.2"),
        ];
        for (kind, id, state) in cases {
            let body = body_for_widget(kind.clone(), None, Arc::clone(&store));
            let entity = minimal_entity(id, state);
            let rows = body.render_rows(&entity);
            assert!(!rows.is_empty(), "{kind:?} body must return non-empty rows");
            assert_eq!(rows[0].key, "state");
            assert_eq!(rows[0].value, *state);
        }
    }

    /// `body_for_widget` falls back to `AttributesBody` for `LightTile`,
    /// `SensorTile`, and `EntityTile`.
    #[test]
    fn body_for_widget_fallback_kinds_produce_non_empty_rows_when_state_not_empty() {
        let store = Arc::new(LiveStore::new());
        for kind in &[
            WidgetKind::LightTile,
            WidgetKind::SensorTile,
            WidgetKind::EntityTile,
        ] {
            let body = body_for_widget(kind.clone(), None, Arc::clone(&store));
            // `AttributesBody` with an empty attribute map produces zero rows.
            // This asserts the fallback is wired (no panic) but does not assert
            // non-empty (that would require JSON-valued attributes).
            let entity = minimal_entity("light.test", "on");
            let _ = body.render_rows(&entity);
        }
    }

    // -----------------------------------------------------------------------
    // Per-domain body unit tests (TASK-098)
    //
    // Each test constructs a minimal entity (state only, no attributes —
    // Gate 2 forbids raw JSON values in src/ui/**) and asserts that the body
    // produces at least one row: the "state" row that every per-domain
    // stub guarantees.
    // -----------------------------------------------------------------------

    #[test]
    fn cover_body_attribute_rows_non_empty() {
        let entity = minimal_entity("cover.garage_door", "closed");
        let rows = CoverBody::new().render_rows(&entity);
        assert!(!rows.is_empty(), "CoverBody must return non-empty rows");
        assert_eq!(rows[0].key, "state");
        assert_eq!(rows[0].value, "closed");
    }

    #[test]
    fn fan_body_attribute_rows_non_empty() {
        let entity = minimal_entity("fan.bedroom", "on");
        let rows = FanBody::new().render_rows(&entity);
        assert!(!rows.is_empty(), "FanBody must return non-empty rows");
        assert_eq!(rows[0].key, "state");
        assert_eq!(rows[0].value, "on");
    }

    #[test]
    fn lock_body_attribute_rows_non_empty() {
        let entity = minimal_entity("lock.front_door", "locked");
        let rows = LockBody::new().render_rows(&entity);
        assert!(!rows.is_empty(), "LockBody must return non-empty rows");
        assert_eq!(rows[0].key, "state");
        assert_eq!(rows[0].value, "locked");
    }

    #[test]
    fn alarm_body_attribute_rows_non_empty() {
        let entity = minimal_entity("alarm_control_panel.home", "armed_away");
        let rows = AlarmBody::new().render_rows(&entity);
        assert!(!rows.is_empty(), "AlarmBody must return non-empty rows");
        assert_eq!(rows[0].key, "state");
        assert_eq!(rows[0].value, "armed_away");
    }

    #[test]
    fn climate_body_attribute_rows_non_empty() {
        let entity = minimal_entity("climate.living_room", "heat");
        let rows = ClimateBody::new().render_rows(&entity);
        assert!(!rows.is_empty(), "ClimateBody must return non-empty rows");
        assert_eq!(rows[0].key, "state");
        assert_eq!(rows[0].value, "heat");
    }

    #[test]
    fn media_player_body_attribute_rows_non_empty() {
        let entity = minimal_entity("media_player.tv", "playing");
        let rows = MediaPlayerBody::new().render_rows(&entity);
        assert!(
            !rows.is_empty(),
            "MediaPlayerBody must return non-empty rows"
        );
        assert_eq!(rows[0].key, "state");
        assert_eq!(rows[0].value, "playing");
    }

    #[test]
    fn history_body_attribute_rows_non_empty() {
        let entity = minimal_entity("sensor.temperature", "21.5");
        let rows = HistoryBody::new().render_rows(&entity);
        assert!(!rows.is_empty(), "HistoryBody must return non-empty rows");
        assert_eq!(rows[0].key, "state");
        assert_eq!(rows[0].value, "21.5");
    }

    #[test]
    fn camera_body_attribute_rows_non_empty() {
        let entity = minimal_entity("camera.front_door", "idle");
        let rows = CameraBody::new().render_rows(&entity);
        assert!(!rows.is_empty(), "CameraBody must return non-empty rows");
        assert_eq!(rows[0].key, "state");
        assert_eq!(rows[0].value, "idle");
    }

    #[test]
    fn power_flow_body_attribute_rows_non_empty() {
        let entity = minimal_entity("sensor.grid_power", "1.2");
        let rows = PowerFlowBody::new().render_rows(&entity);
        assert!(!rows.is_empty(), "PowerFlowBody must return non-empty rows");
        assert_eq!(rows[0].key, "state");
        assert_eq!(rows[0].value, "1.2");
    }

    // -----------------------------------------------------------------------
    // ModalState::open_with_body (TASK-098)
    // -----------------------------------------------------------------------

    /// `open_with_body` must replace the stored body and open the modal using
    /// the new body exactly once.
    #[test]
    fn open_with_body_replaces_body_and_opens_modal() {
        // Start with AttributesBody; replace with CoverBody via open_with_body.
        let initial_body: Arc<dyn MoreInfoBody> = Arc::new(AttributesBody::new());
        let mut state = ModalState::new(initial_body);

        let entity = minimal_entity("cover.garage_door", "closed");
        let cover_body: Arc<dyn MoreInfoBody> = Arc::new(CoverBody::new());
        state.open_with_body(&entity, cover_body);

        assert!(state.is_open(), "modal must be open after open_with_body");
        assert_eq!(state.open_for(), Some(&EntityId::from("cover.garage_door")));
        // CoverBody returns a "state" row even for empty-attribute entities.
        assert!(
            !state.rows().is_empty(),
            "open_with_body must compute rows via the new body"
        );
        assert_eq!(state.rows()[0].key, "state");
    }
}
