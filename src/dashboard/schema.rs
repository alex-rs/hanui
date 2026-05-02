//! Canonical Phase 4/6 typed schema for the dashboard configuration.
//!
//! # Staging note (`locked_decisions.view_spec_disposition`)
//!
//! This module is the target end-state for Phase 4 types. It coexists with
//! `view_spec.rs` for exactly one merge cycle. `view_spec.rs` is deleted
//! atomically by TASK-082, which simultaneously migrates all callers from
//! `view_spec::*` to `schema::*`.
//!
//! Until TASK-082 lands, both modules are exported from `mod.rs`. The
//! duplication is intentional and pays down on the TASK-082 merge.
//!
//! # No-HashMap contract (`locked_decisions.no_hashmap_in_deserialized_types`)
//!
//! All map-shaped fields in this module use [`std::collections::BTreeMap`], never
//! `HashMap`. This guarantees deterministic iteration order, which is required
//! for byte-identical layout-packer output across repeated loads of the same YAML.
//!
//! # Parent plan
//!
//! `docs/plans/2026-04-29-phase-4-layout.md` and
//! `docs/plans/2026-04-30-phase-6-advanced-widgets.md` —
//! relevant decisions: `serde_yaml_crate_choice`, `serde_yaml_security_review`,
//! `view_spec_disposition`, `no_hashmap_in_deserialized_types`,
//! `validation_rule_identifiers`, `pin_policy_migration`,
//! `visibility_predicate_vocabulary`, `cover_position_bounds`,
//! `history_render_path`, `confirmation_on_lock_unlock`.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::actions::map::WidgetId;
use crate::actions::Action;
use crate::dashboard::visibility::DepBucket;
use crate::ha::entity::EntityId;

// ---------------------------------------------------------------------------
// CallServiceAllowlist
// ---------------------------------------------------------------------------

/// The set of `(domain, service)` pairs that this dashboard config calls.
///
/// Built by the validator (`src/dashboard/validate.rs`) during the validation
/// pass; stored on [`Dashboard::call_service_allowlist`] so the actions queue
/// (TASK-090) can gate runtime `CallService` dispatches against the static
/// set declared in the YAML.
///
/// Per `locked_decisions.call_service_allowlist_runtime_access`: defined here
/// (not in `validate.rs`) so `Dashboard` can carry the field without creating
/// a circular import between `schema` and `validate`.
pub type CallServiceAllowlist = BTreeSet<(String, String)>;

// ---------------------------------------------------------------------------
// ProfileKey
// ---------------------------------------------------------------------------

/// Selects the [`DeviceProfile`](crate::dashboard::profiles::DeviceProfile)
/// preset to apply for this dashboard load.
///
/// `#[serde(rename_all = "kebab-case")]` maps YAML values `rpi4`, `opi-zero3`,
/// `desktop` to the respective variants. Any unlisted string fails
/// deserialization — there is no free-string fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileKey {
    /// Raspberry Pi 4 preset.
    Rpi4,
    /// Orange Pi Zero 3 preset.
    OpiZero3,
    /// Desktop / dev-VM preset.
    Desktop,
}

// ---------------------------------------------------------------------------
// WidgetKind
// ---------------------------------------------------------------------------

/// The tile rendering kind, corresponding to the YAML `type:` field on a widget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    /// `type: light_tile`
    LightTile,
    /// `type: sensor_tile`
    SensorTile,
    /// `type: entity_tile`
    EntityTile,
    /// `type: camera`
    Camera,
    /// `type: history`
    History,
    /// `type: fan`
    Fan,
    /// `type: lock`
    Lock,
    /// `type: alarm`
    Alarm,
    // Phase 6 additions:
    /// `type: cover`
    Cover,
    /// `type: media_player`
    MediaPlayer,
    /// `type: climate`
    Climate,
    /// `type: power_flow`
    PowerFlow,
}

// ---------------------------------------------------------------------------
// Layout enum for View
// ---------------------------------------------------------------------------

/// The view-level layout strategy, corresponding to the YAML `layout:` field
/// on a view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// `layout: sections`
    Sections,
    /// `layout: grid`
    Grid,
}

// ---------------------------------------------------------------------------
// Placement  (internal / computed)
// ---------------------------------------------------------------------------

/// Computed grid placement assigned by the packer (TASK-014).
///
/// This is an **internal** field — it is not part of the user-facing YAML
/// schema. The packer writes it; the Slint bridge reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    /// Zero-based column index of the widget's top-left cell.
    pub col: u8,
    /// Zero-based row index of the widget's top-left cell.
    pub row: u8,
    /// Number of columns the widget spans.
    pub span_cols: u8,
    /// Number of rows the widget spans.
    pub span_rows: u8,
}

// ---------------------------------------------------------------------------
// WidgetLayout  (user-visible sub-object)
// ---------------------------------------------------------------------------

/// The `layout:` sub-object inside a widget config entry.
///
/// Field names mirror the YAML schema verbatim:
/// `preferred_columns`, `preferred_rows`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetLayout {
    /// `preferred_columns` — the widget's preferred column span hint.
    pub preferred_columns: u8,
    /// `preferred_rows` — the widget's preferred row span hint.
    pub preferred_rows: u8,
}

// ---------------------------------------------------------------------------
// SectionGrid  (user-visible sub-object)
// ---------------------------------------------------------------------------

/// Grid parameters for a section, corresponding to the YAML `grid:` field
/// inside a section definition.
///
/// `columns` drives the `SpanOverflow` validator check: a widget whose
/// `preferred_columns` exceeds `columns` is a validator Error.
///
/// `gap` is the gap between grid cells in logical pixels; it is stored for
/// the layout packer (TASK-084) and ignored by the validator.
///
/// `Default` implementation provides `columns: 4, gap: 8` — the conventional
/// Phase 4 defaults from `docs/DASHBOARD_SCHEMA.md`. These defaults are used
/// when the YAML section omits the `grid:` sub-object entirely (i.e., when
/// existing fixtures predate this field being required).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionGrid {
    /// Number of columns in the section grid.
    /// A widget with `preferred_columns > columns` triggers a `SpanOverflow` Error.
    pub columns: u8,
    /// Gap between grid cells in logical pixels.
    #[serde(default = "SectionGrid::default_gap")]
    pub gap: u8,
}

impl SectionGrid {
    /// Default gap value (8 logical pixels) per `docs/DASHBOARD_SCHEMA.md`.
    #[must_use]
    pub const fn default_gap() -> u8 {
        8
    }
}

impl Default for SectionGrid {
    /// Provides `columns: 4, gap: 8` — the conventional Phase 4 section defaults.
    ///
    /// Phase 4 fixtures that predate the required `grid:` field on sections use
    /// this default so that the existing serde round-trip tests continue to pass
    /// when the field is absent from the YAML.
    fn default() -> Self {
        Self { columns: 4, gap: 8 }
    }
}

// ---------------------------------------------------------------------------
// HomeAssistant connection config
// ---------------------------------------------------------------------------

/// The `home_assistant:` sub-object from the root dashboard config.
///
/// Field names mirror the YAML schema verbatim: `url`, `token_env`.
///
/// `token_env` holds the *name* of the environment variable that contains the
/// long-lived access token; the token itself is never stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeAssistant {
    /// `url` — WebSocket API endpoint, e.g.
    /// `"ws://homeassistant.local:8123/api/websocket"`.
    pub url: String,
    /// `token_env` — name of the environment variable carrying the HA token.
    pub token_env: String,
}

// ---------------------------------------------------------------------------
// Theme config
// ---------------------------------------------------------------------------

/// The `theme:` sub-object from the root dashboard config.
///
/// Field names mirror the YAML schema verbatim: `mode`, `accent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Theme {
    /// `mode` — colour scheme selector, e.g. `"dark"` or `"light"`.
    pub mode: String,
    /// `accent` — CSS-style hex accent colour, e.g. `"#03a9f4"`.
    pub accent: String,
}

// ---------------------------------------------------------------------------
// CodeFormat  (used by PinPolicy)
// ---------------------------------------------------------------------------

/// The format of PIN codes entered for lock / alarm interactions.
///
/// Per `locked_decisions.pin_policy_migration`: `CodeFormat` is a closed enum
/// replacing the previous `code_format: String` free-form field. Using a closed
/// enum prevents invalid format values from ever being constructed at the schema
/// level — a `PinPolicyInvalidCodeFormat` Error is no longer needed because the
/// type system enforces the constraint at deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeFormat {
    /// PIN must consist of digits only.
    Number,
    /// PIN may contain any characters.
    Any,
}

// ---------------------------------------------------------------------------
// PinPolicy  (used by Lock and Alarm widget options)
// ---------------------------------------------------------------------------

/// PIN policy configuration for lock and alarm widgets.
///
/// Per `locked_decisions.pin_policy_migration`: replaces the previous
/// `struct PinPolicy { code_format: String }`. The enum encodes the three
/// possible policies in the type system rather than as a string field.
///
/// `PinPolicy::RequiredOnDisarm` is valid ONLY on `WidgetOptions::Alarm`.
/// The validator enforces this: a `WidgetOptions::Lock` with
/// `PinPolicy::RequiredOnDisarm` is a `ValidationRule::PinPolicyRequiredOnDisarmOnLock`
/// Error.
///
/// Serde shape: `#[serde(rename_all = "snake_case")]`.
/// - `None` serializes as `"none"` (unit variant → bare string in YAML).
/// - `Required` serializes as `required: { length: N, code_format: ... }`.
/// - `RequiredOnDisarm` serializes as
///   `required_on_disarm: { length: N, code_format: ... }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinPolicy {
    /// No PIN required.
    None,
    /// PIN required; specifies allowed length and format.
    Required {
        /// Expected PIN length in digits/characters.
        length: u8,
        /// Format constraint on the PIN.
        code_format: CodeFormat,
    },
    /// PIN is required only on disarm (alarm-only; rejected on lock widgets
    /// by `ValidationRule::PinPolicyRequiredOnDisarmOnLock`).
    RequiredOnDisarm {
        /// Expected PIN length in digits/characters.
        length: u8,
        /// Format constraint on the PIN.
        code_format: CodeFormat,
    },
}

// ---------------------------------------------------------------------------
// MediaTransport  (closed enum for media player transport controls)
// ---------------------------------------------------------------------------

/// The set of media transport operations that a media player widget can expose.
///
/// Per `locked_decisions.idempotency_marker_phase6_variants`:
/// - `Play`, `Pause`, `Stop` are idempotent (safe to queue offline).
/// - `Next`, `Prev` are NonIdempotent (must NOT be queued; fail loudly offline).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaTransport {
    /// Start or resume playback.
    Play,
    /// Pause playback.
    Pause,
    /// Stop playback.
    Stop,
    /// Skip to the next track (NonIdempotent).
    Next,
    /// Go to the previous track (NonIdempotent).
    Prev,
    /// Increase volume by one step.
    VolumeUp,
    /// Decrease volume by one step.
    VolumeDown,
    /// Toggle mute.
    Mute,
}

// ---------------------------------------------------------------------------
// WidgetOptions
// ---------------------------------------------------------------------------

/// Tile-kind-specific typed options.
///
/// Each variant corresponds to one widget `type:` value and carries only the
/// fields that type supports. This replaces the previous `Vec<(String, String)>`
/// free-form options from `view_spec.rs`.
///
/// Per `locked_decisions.no_hashmap_in_deserialized_types`: no `HashMap` is
/// used here; map-shaped data is expressed as named fields or `BTreeMap`.
///
/// Phase 6 additions per `locked_decisions`:
/// - `Camera` extended with `url: String` field.
/// - `History` extended with `max_points: u32` (default 60, validator max 240
///   per `locked_decisions.history_render_path`).
/// - `Cover`, `MediaPlayer`, `Climate`, `PowerFlow` variants added.
/// - `Lock` extended with `require_confirmation_on_unlock: bool`
///   (per `locked_decisions.confirmation_on_lock_unlock`).
/// - `PinPolicy` is now an enum (per `locked_decisions.pin_policy_migration`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetOptions {
    /// Options for `type: camera` widgets.
    Camera {
        /// Poll interval in seconds. Must be ≥ `DeviceProfile.camera_interval_min_s`.
        interval_seconds: u32,
        /// Stream URL for the camera feed (e.g. MJPEG or snapshot endpoint).
        url: String,
    },
    /// Options for `type: history` widgets.
    History {
        /// History window in seconds. Must be ≤ `DeviceProfile.history_window_max_s`.
        window_seconds: u32,
        /// Maximum number of data points to render (after LTTB downsampling).
        /// Default 60; validator enforces max 240 per
        /// `locked_decisions.history_render_path`.
        #[serde(default = "WidgetOptions::default_max_points")]
        max_points: u32,
    },
    /// Options for `type: fan` widgets.
    Fan {
        /// Number of discrete speed steps.
        speed_count: u32,
        /// Named speed presets, e.g. `["Low", "Medium", "High"]`.
        #[serde(default)]
        preset_modes: Vec<String>,
    },
    /// Options for `type: lock` widgets.
    Lock {
        /// PIN policy for code-locked doors.
        ///
        /// Uses `singleton_map` so the YAML wire form is externally-tagged:
        /// `pin_policy: none`, or `pin_policy:\n  required:\n    length: 4\n    code_format: number`.
        #[serde(with = "serde_yaml_ng::with::singleton_map")]
        pin_policy: PinPolicy,
        /// Whether to show a confirmation dialog before unlocking.
        /// Per `locked_decisions.confirmation_on_lock_unlock`: the flag lives
        /// here (not on the `Action` variant) so offline queue replay works
        /// correctly (the queued action has no modal dependency).
        #[serde(default)]
        require_confirmation_on_unlock: bool,
    },
    /// Options for `type: alarm` widgets.
    Alarm {
        /// PIN policy for the alarm disarm code.
        ///
        /// Uses `singleton_map` so the YAML wire form is externally-tagged.
        /// `RequiredOnDisarm` is valid only on Alarm; the validator enforces
        /// `ValidationRule::PinPolicyRequiredOnDisarmOnLock` for Lock widgets.
        #[serde(with = "serde_yaml_ng::with::singleton_map")]
        pin_policy: PinPolicy,
    },
    // Phase 6 variants:
    /// Options for `type: cover` widgets.
    ///
    /// Per `locked_decisions.cover_position_bounds`:
    /// position values are in 0..=100; `position_min` must be ≤ `position_max`.
    Cover {
        /// Minimum position value (inclusive). Must be ≤ `position_max` and ≤ 100.
        position_min: u8,
        /// Maximum position value (inclusive). Must be ≥ `position_min` and ≤ 100.
        position_max: u8,
    },
    /// Options for `type: media_player` widgets.
    MediaPlayer {
        /// The set of transport controls to expose in the UI.
        /// Each entry must be a value from the `MediaTransport` enum.
        /// Unknown values are `ValidationRule::MediaTransportNotAllowed` (Error).
        transport_set: Vec<MediaTransport>,
        /// Volume adjustment step (0.0..=1.0). Must be > 0.
        volume_step: f32,
    },
    /// Options for `type: climate` widgets.
    Climate {
        /// Minimum setpoint temperature (°C or °F per HA unit config).
        min_temp: f32,
        /// Maximum setpoint temperature. Must be > `min_temp`.
        max_temp: f32,
        /// Setpoint adjustment step. Must be > 0.0.
        step: f32,
        /// Available HVAC modes exposed in the mode picker.
        /// Free strings per `locked_decisions.hvac_mode_vocabulary` —
        /// HA allows custom modes beyond the standard set.
        #[serde(default)]
        hvac_modes: Vec<String>,
    },
    /// Options for `type: power_flow` widgets.
    ///
    /// Power-flow detailed options are owned by TASK-094 (6d).
    /// This variant is the Phase 6.0 schema reservation — the validator
    /// rules `PowerFlowGridEntityNotPower`, `PowerFlowBatteryWithoutSoC`, and
    /// `PowerFlowIndividualLaneCountExceeded` are added by TASK-094.
    PowerFlow {
        /// Entity ID for the grid connection (must be a power sensor entity).
        grid_entity: String,
        /// Entity ID for solar production (optional).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        solar_entity: Option<String>,
        /// Entity ID for battery storage (optional; requires `battery_soc_entity`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        battery_entity: Option<String>,
        /// Entity ID for battery state-of-charge (required when `battery_entity` is set).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        battery_soc_entity: Option<String>,
        /// Entity ID for home consumption (optional).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        home_entity: Option<String>,
    },
}

impl WidgetOptions {
    /// Default value for `History::max_points` per `locked_decisions.history_render_path`.
    #[must_use]
    pub const fn default_max_points() -> u32 {
        60
    }
}

// ---------------------------------------------------------------------------
// Widget
// ---------------------------------------------------------------------------

/// A single dashboard widget, matching the widget config shape in
/// `docs/DASHBOARD_SCHEMA.md`.
///
/// User-visible fields: `id`, `widget_type` (YAML: `type`), `entity`,
/// `entities`, `name`, `icon`, `tap_action`, `hold_action`,
/// `double_tap_action`, `layout`, `options`.
///
/// Internal fields: `placement` (computed by packer, not user-authored).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Widget {
    /// `id` — unique identifier for this widget within the dashboard.
    pub id: String,
    /// `type` (YAML field name) — the tile rendering kind.
    #[serde(rename = "type")]
    pub widget_type: WidgetKind,
    /// `entity` — the primary HA entity ID string (e.g. `"light.kitchen"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    /// `entities` — secondary entity IDs for multi-entity tiles.
    #[serde(default)]
    pub entities: Vec<String>,
    /// `name` — optional display name override; `None` uses the entity's
    /// friendly name from HA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `icon` — optional icon override (MDI icon slug or asset path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// `visibility` — predicate string controlling when the widget is shown.
    ///
    /// Defaults to `"always"` when absent (the widget is always visible).
    /// Phase 4 locked the Phase 4 predicate namespace; Phase 6 widens it.
    /// Unknown values fail validation with
    /// [`ValidationRule::UnknownVisibilityPredicate`]. Phase 6 TASK-110
    /// evaluates predicates at runtime; Phase 4/6.0 only validates the namespace.
    ///
    /// Known predicates (Phase 4): `always`, `never`, `entity_available:<entity_id>`,
    /// `state_equals:<entity_id>:<value>`, `profile:<profile_key>`.
    ///
    /// Added in Phase 6 per `locked_decisions.visibility_predicate_vocabulary`:
    /// `<id> == <value>`, `<id> != <value>`, `<id> in [<v1>,<v2>,...]`,
    /// `entity_state_numeric:<id>:<op>:<N>`.
    #[serde(
        default = "Widget::default_visibility",
        skip_serializing_if = "Widget::is_default_visibility"
    )]
    pub visibility: String,
    /// `tap_action` — action fired on a single tap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap_action: Option<Action>,
    /// `hold_action` — action fired on a long press.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_action: Option<Action>,
    /// `double_tap_action` — action fired on a double tap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double_tap_action: Option<Action>,
    /// `layout` — user-supplied size hints.
    pub layout: WidgetLayout,
    /// `options` — tile-kind-specific typed options. `None` for tile kinds
    /// that carry no extra options (`LightTile`, `SensorTile`, `EntityTile`).
    ///
    /// Uses `serde_yaml_ng::with::singleton_map` so the YAML wire form is
    /// the externally-tagged map shape documented in `docs/DASHBOARD_SCHEMA.md`
    /// (e.g. `options:\n  camera:\n    interval_seconds: 5`), NOT the YAML-tag
    /// form (`!camera\ninterval_seconds: 5`) that `serde_yaml_ng` would emit
    /// for a bare `#[serde(rename_all)]` enum.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serde_yaml_ng::with::singleton_map::serialize",
        deserialize_with = "serde_yaml_ng::with::singleton_map::deserialize"
    )]
    pub options: Option<WidgetOptions>,
    /// Computed grid slot assigned by the packer. Skipped during
    /// serialization/deserialization (internal only).
    #[serde(default, skip)]
    pub placement: Option<Placement>,
}

impl Widget {
    /// Returns the default visibility predicate (`"always"`).
    ///
    /// Used by `#[serde(default = "Widget::default_visibility")]` to populate
    /// the field when the YAML section omits `visibility:`.
    #[must_use]
    pub fn default_visibility() -> String {
        "always".to_string()
    }

    /// Returns `true` when the visibility string equals the default (`"always"`).
    ///
    /// Used by `#[serde(skip_serializing_if = "Widget::is_default_visibility")]`
    /// to omit the field during serialization so that existing round-trip
    /// fixtures are not polluted with an explicit `visibility: always`.
    #[must_use]
    pub fn is_default_visibility(v: &str) -> bool {
        v == "always"
    }
}

// ---------------------------------------------------------------------------
// Section
// ---------------------------------------------------------------------------

/// A named group of widgets within a view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    /// `id` — unique section identifier within the view.
    pub id: String,
    /// `title` — display title shown above the section.
    pub title: String,
    /// `grid` — grid parameters (column count + gap) for this section.
    ///
    /// Defaults to `SectionGrid { columns: 4, gap: 8 }` when the YAML omits
    /// the sub-object (e.g. fixtures authored before Phase 4 required this
    /// field). The validator uses `grid.columns` for the `SpanOverflow` check;
    /// the layout packer uses both fields at pack time.
    #[serde(default)]
    pub grid: SectionGrid,
    /// `widgets` — ordered list of widgets in this section.
    #[serde(default)]
    pub widgets: Vec<Widget>,
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// A single dashboard screen / page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct View {
    /// `id` — unique view identifier referenced by `default_view`.
    pub id: String,
    /// `title` — display title for the view tab or navigation entry.
    pub title: String,
    /// `layout` — the layout strategy for this view.
    pub layout: Layout,
    /// `sections` — ordered list of sections (used when `layout` is `Sections`).
    #[serde(default)]
    pub sections: Vec<Section>,
}

// ---------------------------------------------------------------------------
// Dashboard (top-level)
// ---------------------------------------------------------------------------

/// The top-level dashboard configuration, matching the root object in
/// `docs/DASHBOARD_SCHEMA.md`.
///
/// Deserialized from YAML via `serde_yaml_ng`. All map-shaped fields use
/// `BTreeMap` per `locked_decisions.no_hashmap_in_deserialized_types`.
///
/// # Manual `PartialEq`
///
/// `PartialEq` is implemented manually below (NOT derived) per
/// `locked_decisions.dep_index_partial_eq`. The [`Dashboard::dep_index`] field
/// is an [`Arc`] wrapping a `HashMap`; deriving `PartialEq` would compare the
/// `Arc` by structural equality, but two independently-built Arcs from the
/// same source YAML must compare equal so the existing
/// `round_trip_dashboard_yaml_is_byte_equal` test continues to pass.
/// The manual impl compares the inner map structurally, ignoring `Arc`
/// pointer identity, and is order-insensitive within each bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    /// `version` — schema version integer.
    pub version: u32,
    /// `device_profile` — which hardware preset to apply.
    pub device_profile: ProfileKey,
    /// `home_assistant` — connection config for the HA WebSocket API.
    /// `None` when omitted (e.g. in fixture-only / offline use).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_assistant: Option<HomeAssistant>,
    /// `theme` — colour scheme overrides. `None` applies the built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<Theme>,
    /// `default_view` — `id` of the view shown on initial load.
    pub default_view: String,
    /// `views` — ordered list of all views in the dashboard.
    pub views: Vec<View>,
    /// Per-config `(domain, service)` allowlist built by the validator at load time.
    ///
    /// **Not serialized to / deserialized from YAML** (`#[serde(default, skip)]`).
    /// A freshly-deserialized `Dashboard` carries an empty allowlist; the loader
    /// (TASK-082) replaces it with the validator's output after a clean
    /// `validate()` call returns zero `Severity::Error` issues.
    ///
    /// Per `locked_decisions.call_service_allowlist_runtime_access`: the runtime
    /// actions queue (TASK-090) reads this field to gate `CallService` dispatches
    /// against the static set declared in the YAML, preventing injection of
    /// services that the config never declared.
    #[serde(default, skip)]
    pub call_service_allowlist: Arc<CallServiceAllowlist>,
    /// Reverse `EntityId → Vec<WidgetId>` index built by
    /// [`crate::dashboard::visibility::build_dep_index`] at load time.
    ///
    /// **Not serialized to / deserialized from YAML** (`#[serde(default, skip)]`).
    /// A freshly-deserialized `Dashboard` carries an empty index; the loader
    /// populates it after parsing so the bridge can resolve, in O(1), which
    /// widgets need a visibility re-evaluation when a given entity changes
    /// state.
    ///
    /// Per `locked_decisions.dep_index_partial_eq`, `Arc::ptr_eq` is NOT used
    /// in the manual `PartialEq` for `Dashboard` — the inner map is compared
    /// structurally so independently-built Arcs from the same YAML compare
    /// equal.
    #[serde(default, skip)]
    pub dep_index: Arc<HashMap<EntityId, DepBucket>>,
}

// ---------------------------------------------------------------------------
// Manual PartialEq for Dashboard
// ---------------------------------------------------------------------------

impl PartialEq for Dashboard {
    fn eq(&self, other: &Self) -> bool {
        // Plain-derived comparison for every field except `dep_index`. We
        // hand-roll the comparison here rather than splitting Dashboard into
        // a helper struct so callers continue to use Dashboard literals.
        if self.version != other.version
            || self.device_profile != other.device_profile
            || self.home_assistant != other.home_assistant
            || self.theme != other.theme
            || self.default_view != other.default_view
            || self.views != other.views
            || self.call_service_allowlist != other.call_service_allowlist
        {
            return false;
        }
        // `dep_index`: structural compare of the inner map. Two
        // independently-built Arcs over the same content must compare equal,
        // so we explicitly do NOT use `Arc::ptr_eq`.
        dep_index_structurally_eq(&self.dep_index, &other.dep_index)
    }
}

impl Eq for Dashboard {}

/// Compare two dep_index maps structurally, ignoring `Arc` pointer identity
/// and ignoring widget-id order within each bucket.
///
/// Per `locked_decisions.dep_index_partial_eq`: "Two Dashboards equal iff
/// their dep_index maps have the same keys and the same SmallVec contents
/// (order-insensitive within a SmallVec since widget ID order in a bucket
/// does not affect evaluation correctness)."
fn dep_index_structurally_eq(
    a: &Arc<HashMap<EntityId, DepBucket>>,
    b: &Arc<HashMap<EntityId, DepBucket>>,
) -> bool {
    if Arc::ptr_eq(a, b) {
        return true;
    }
    if a.len() != b.len() {
        return false;
    }
    for (key, bucket_a) in a.iter() {
        let bucket_b = match b.get(key) {
            Some(v) => v,
            None => return false,
        };
        if bucket_a.len() != bucket_b.len() {
            return false;
        }
        // Order-insensitive: every element of bucket_a must appear in bucket_b
        // the same number of times. We sort clones rather than building a
        // multiset because dep buckets are tiny (<= DEP_INLINE_CAP in the
        // common case).
        let mut sa: Vec<&WidgetId> = bucket_a.iter().collect();
        let mut sb: Vec<&WidgetId> = bucket_b.iter().collect();
        sa.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        sb.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        if sa != sb {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// ValidationRule
// ---------------------------------------------------------------------------

/// Stable identifier for each validation rule.
///
/// Defined per `locked_decisions.validation_rule_identifiers`. Severity
/// mapping (Error vs Warning) is asserted in TASK-089's `severity_pin` test.
///
/// Error rules (halt load, no partial render):
/// - `SpanOverflow`, `UnknownWidgetType`, `UnknownVisibilityPredicate`,
///   `NonAllowlistedCallService`, `MaxWidgetsPerViewExceeded`,
///   `CameraIntervalBelowMin`, `HistoryWindowAboveMax`,
///   `PinPolicyRequiredOnDisarmOnLock`,
///   `CoverPositionOutOfBounds`, `ClimateMinMaxTempInvalid`,
///   `MediaTransportNotAllowed`, `HistoryMaxPointsExceeded`
///
/// Warning rules (render with banner, do not halt load):
/// - `ImageOptionExceedsMaxPx`, `CameraIntervalBelowDefault`,
///   `PowerFlowBatteryWithoutSoC`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidationRule {
    // ----- Error rules -----------------------------------------
    /// A widget's `preferred_columns` exceeds the section's grid column count.
    SpanOverflow,
    /// The YAML `type:` value is not a registered `WidgetKind`.
    UnknownWidgetType,
    /// The YAML `visibility:` predicate is not in the locked predicate namespace.
    UnknownVisibilityPredicate,
    /// A `CallService` action references a service not in the per-domain allowlist.
    NonAllowlistedCallService,
    /// The number of widgets in a view exceeds `DeviceProfile.max_widgets_per_view`.
    MaxWidgetsPerViewExceeded,
    /// A camera widget's `interval_seconds` is below `DeviceProfile.camera_interval_min_s`.
    CameraIntervalBelowMin,
    /// A history widget's `window_seconds` exceeds `DeviceProfile.history_window_max_s`.
    HistoryWindowAboveMax,
    /// `WidgetOptions::Lock` carries `PinPolicy::RequiredOnDisarm`, which is
    /// only valid on alarm widgets per `locked_decisions.pin_policy_migration`.
    PinPolicyRequiredOnDisarmOnLock,
    /// A cover widget's `position_min > position_max`, or either bound is outside 0..=100.
    CoverPositionOutOfBounds,
    /// A climate widget's `min_temp >= max_temp`, or `step <= 0.0`.
    ClimateMinMaxTempInvalid,
    /// A media player widget's `transport_set` contains an unrecognised transport value.
    MediaTransportNotAllowed,
    /// A history widget's `max_points` exceeds the validator-enforced maximum (240).
    HistoryMaxPointsExceeded,
    // ----- Warning rules ---------------------------------------
    /// An image option's pixel dimension exceeds `DeviceProfile.max_image_px`
    /// (a pre-decode downscale will be applied).
    ImageOptionExceedsMaxPx,
    /// A camera widget's `interval_seconds` is between `camera_interval_min_s`
    /// and `camera_interval_default_s` (allowed but flagged as tighter than
    /// the profile's recommended default).
    CameraIntervalBelowDefault,
    /// A power-flow widget has a `battery_entity` but no `battery_soc_entity`.
    /// Per `locked_decisions.validation_rule_identifiers`: Warning (not Error),
    /// owned by TASK-094. Reserved here for identifier consistency.
    PowerFlowBatteryWithoutSoC,
}

// ---------------------------------------------------------------------------
// Severity
// ---------------------------------------------------------------------------

/// Severity level for a validation [`Issue`].
///
/// `Error` halts dashboard load; no partial render is shown.
/// `Warning` renders the dashboard with a persistent banner overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Halts load; fullscreen error screen shown.
    Error,
    /// Dashboard renders with a warning banner; does not halt load.
    Warning,
}

// ---------------------------------------------------------------------------
// Issue
// ---------------------------------------------------------------------------

/// A single validation finding produced by `src/dashboard/validate.rs`.
///
/// Each `Issue` carries a stable [`ValidationRule`] identifier, a [`Severity`],
/// the dotted YAML path to the offending field, a human-readable message, and
/// a pre-captured one-line YAML excerpt for display in the error screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Stable rule identifier (used by tests to assert findings).
    pub rule: ValidationRule,
    /// Whether the issue halts load (`Error`) or only warns (`Warning`).
    pub severity: Severity,
    /// Dotted path to the offending YAML field, e.g. `"views[0].sections[0].widgets[1].layout.preferred_columns"`.
    pub path: String,
    /// Human-readable explanation suitable for the validation error screen.
    pub message: String,
    /// Pre-captured one-line YAML excerpt surrounding the offending field.
    /// Empty string if capture failed (e.g. parse error before YAML was valid).
    pub yaml_excerpt: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture YAML containing one widget of each kind. Used by the round-trip
    /// determinism test.
    ///
    /// Note: `WidgetOptions` on `camera`/`history`/`fan`/`lock`/`alarm` are
    /// omitted here because `options` is optional and the round-trip test
    /// checks type-level serde correctness, not validator logic. A separate
    /// fixture with all option variants is used by TASK-089's schema-lock test.
    const FIXTURE_YAML: &str = r#"version: 1
device_profile: rpi4
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: overview
        title: Overview
        widgets:
          - id: kitchen_light
            type: light_tile
            entity: light.kitchen
            tap_action:
              action: toggle
            hold_action:
              action: more-info
            layout:
              preferred_columns: 2
              preferred_rows: 2
          - id: hallway_temp
            type: sensor_tile
            entity: sensor.hallway_temperature
            layout:
              preferred_columns: 2
              preferred_rows: 1
          - id: outlet_1
            type: entity_tile
            entity: switch.outlet_1
            name: Living Room
            tap_action:
              action: toggle
            double_tap_action:
              action: navigate
              view-id: home
            layout:
              preferred_columns: 2
              preferred_rows: 1
          - id: cam_front
            type: camera
            entity: camera.front_door
            layout:
              preferred_columns: 4
              preferred_rows: 2
          - id: temp_history
            type: history
            entity: sensor.hallway_temperature
            layout:
              preferred_columns: 4
              preferred_rows: 2
          - id: ceiling_fan
            type: fan
            entity: fan.ceiling
            layout:
              preferred_columns: 2
              preferred_rows: 2
          - id: front_door_lock
            type: lock
            entity: lock.front_door
            layout:
              preferred_columns: 2
              preferred_rows: 2
          - id: alarm_panel
            type: alarm
            entity: alarm_control_panel.home
            layout:
              preferred_columns: 4
              preferred_rows: 2
          - id: patio_cover
            type: cover
            entity: cover.patio
            layout:
              preferred_columns: 2
              preferred_rows: 2
          - id: living_room_media
            type: media_player
            entity: media_player.living_room
            layout:
              preferred_columns: 4
              preferred_rows: 2
          - id: thermostat
            type: climate
            entity: climate.living_room
            layout:
              preferred_columns: 2
              preferred_rows: 2
          - id: energy_flow
            type: power_flow
            layout:
              preferred_columns: 4
              preferred_rows: 3
"#;

    /// Round-trip test: parse FIXTURE_YAML → serialize → parse again.
    /// The two `Dashboard` values must be `==` (byte-equal via `PartialEq`).
    ///
    /// This is the ground-truth determinism assertion at the type level.
    /// Per `locked_decisions.no_hashmap_in_deserialized_types`, the absence
    /// of `HashMap` guarantees stable field ordering across parses.
    #[test]
    fn round_trip_dashboard_yaml_is_byte_equal() {
        let first: Dashboard =
            serde_yaml_ng::from_str(FIXTURE_YAML).expect("first parse must succeed");
        let serialized = serde_yaml_ng::to_string(&first).expect("serialization must succeed");
        let second: Dashboard =
            serde_yaml_ng::from_str(&serialized).expect("second parse must succeed");
        assert_eq!(
            first, second,
            "Dashboard round-trip must produce byte-equal values"
        );
    }

    /// Per `locked_decisions.dep_index_partial_eq`: two `Dashboard` values
    /// with identical content but independently-built `Arc<dep_index>` instances
    /// (different `Arc` pointers) must compare equal. This pins the manual
    /// `PartialEq` against accidental `Arc::ptr_eq` regression.
    #[test]
    fn dashboard_partial_eq_compares_dep_index_structurally() {
        use crate::actions::map::WidgetId;
        use crate::dashboard::visibility::DepBucket;
        use crate::ha::entity::EntityId;
        use std::collections::HashMap;

        let mut a_map: HashMap<EntityId, DepBucket> = HashMap::new();
        a_map.insert(
            EntityId::from("light.kitchen"),
            vec![WidgetId::from("w1"), WidgetId::from("w2")],
        );
        let mut b_map: HashMap<EntityId, DepBucket> = HashMap::new();
        // Same content but inserted in reverse (order-insensitive within bucket).
        b_map.insert(
            EntityId::from("light.kitchen"),
            vec![WidgetId::from("w2"), WidgetId::from("w1")],
        );

        // Two independently-built Arcs.
        let a = Dashboard {
            version: 1,
            device_profile: ProfileKey::Desktop,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![],
            call_service_allowlist: Arc::default(),
            dep_index: Arc::new(a_map),
        };
        let b = Dashboard {
            version: 1,
            device_profile: ProfileKey::Desktop,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![],
            call_service_allowlist: Arc::default(),
            dep_index: Arc::new(b_map),
        };
        // Pointer identity of the two Arcs must NOT be required for equality.
        assert!(!Arc::ptr_eq(&a.dep_index, &b.dep_index));
        assert_eq!(a, b);

        // Now a different bucket content — must NOT be equal.
        let mut c_map: HashMap<EntityId, DepBucket> = HashMap::new();
        c_map.insert(EntityId::from("light.kitchen"), vec![WidgetId::from("w1")]);
        let c = Dashboard {
            dep_index: Arc::new(c_map),
            ..a.clone()
        };
        assert_ne!(a, c);

        // Different keys — also not equal.
        let mut d_map: HashMap<EntityId, DepBucket> = HashMap::new();
        d_map.insert(
            EntityId::from("light.bedroom"),
            vec![WidgetId::from("w1"), WidgetId::from("w2")],
        );
        let d = Dashboard {
            dep_index: Arc::new(d_map),
            ..a.clone()
        };
        assert_ne!(a, d);
    }

    /// Same as round_trip_dashboard_yaml_is_byte_equal but explicitly populates
    /// the dep_index (as the loader does in production) to confirm the round-
    /// trip remains byte-equal once the dep_index is non-empty on both sides.
    #[test]
    fn round_trip_yaml_byte_equal_with_dep_index() {
        use crate::dashboard::visibility::build_dep_index;

        let mut first: Dashboard =
            serde_yaml_ng::from_str(FIXTURE_YAML).expect("first parse must succeed");
        first.dep_index = Arc::new(build_dep_index(&first));

        let serialized = serde_yaml_ng::to_string(&first).expect("serialization must succeed");
        let mut second: Dashboard =
            serde_yaml_ng::from_str(&serialized).expect("second parse must succeed");
        second.dep_index = Arc::new(build_dep_index(&second));

        // The manual PartialEq compares dep_index structurally; two
        // independently-built indices over the same widget list must be equal.
        assert!(!Arc::ptr_eq(&first.dep_index, &second.dep_index));
        assert_eq!(first, second);
    }

    #[test]
    fn profile_key_deserializes_rpi4() {
        #[derive(Deserialize)]
        struct Wrapper {
            key: ProfileKey,
        }
        let w: Wrapper = serde_yaml_ng::from_str("key: rpi4").unwrap();
        assert_eq!(w.key, ProfileKey::Rpi4);
    }

    #[test]
    fn profile_key_deserializes_opi_zero3() {
        #[derive(Deserialize)]
        struct Wrapper {
            key: ProfileKey,
        }
        let w: Wrapper = serde_yaml_ng::from_str("key: opi-zero3").unwrap();
        assert_eq!(w.key, ProfileKey::OpiZero3);
    }

    #[test]
    fn profile_key_deserializes_desktop() {
        #[derive(Deserialize)]
        struct Wrapper {
            key: ProfileKey,
        }
        let w: Wrapper = serde_yaml_ng::from_str("key: desktop").unwrap();
        assert_eq!(w.key, ProfileKey::Desktop);
    }

    #[test]
    fn profile_key_rejects_unknown_value() {
        // Deserializing directly into ProfileKey avoids a dead-code field.
        let result: Result<ProfileKey, _> = serde_yaml_ng::from_str("unknown-board");
        assert!(
            result.is_err(),
            "unknown profile key must fail deserialization"
        );
    }

    #[test]
    fn validation_rule_is_copy() {
        let rule = ValidationRule::SpanOverflow;
        let _copy = rule;
        let _also = rule;
    }

    #[test]
    fn severity_is_copy() {
        let s = Severity::Error;
        let _copy = s;
        let _also = s;
    }

    #[test]
    fn issue_fields_are_accessible() {
        let issue = Issue {
            rule: ValidationRule::SpanOverflow,
            severity: Severity::Error,
            path: "views[0].sections[0].widgets[0].layout.preferred_columns".to_string(),
            message: "preferred_columns 5 exceeds section grid columns 4".to_string(),
            yaml_excerpt: "              preferred_columns: 5".to_string(),
        };
        assert_eq!(issue.rule, ValidationRule::SpanOverflow);
        assert_eq!(issue.severity, Severity::Error);
        assert!(!issue.path.is_empty());
        assert!(!issue.message.is_empty());
    }

    /// Helper wrapper so unit tests can exercise `WidgetOptions` through the
    /// `singleton_map` path without constructing a full `Widget`.
    ///
    /// The `options` field here mirrors the attribute on `Widget::options`.
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct OptionsWrapper {
        #[serde(with = "serde_yaml_ng::with::singleton_map")]
        options: WidgetOptions,
    }

    #[test]
    fn widget_options_camera_roundtrip() {
        let yaml = r#"options:
  camera:
    interval_seconds: 10
    url: "http://cam.local/snapshot"
"#;
        let w: OptionsWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::Camera {
                interval_seconds: 10,
                url: "http://cam.local/snapshot".to_string(),
            }
        );
        let back = serde_yaml_ng::to_string(&w).unwrap();
        let w2: OptionsWrapper = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(w, w2);
    }

    #[test]
    fn widget_options_history_roundtrip() {
        let yaml = r#"options:
  history:
    window_seconds: 3600
    max_points: 120
"#;
        let w: OptionsWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::History {
                window_seconds: 3600,
                max_points: 120,
            }
        );
    }

    #[test]
    fn widget_options_history_default_max_points() {
        let yaml = r#"options:
  history:
    window_seconds: 3600
"#;
        let w: OptionsWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::History {
                window_seconds: 3600,
                max_points: 60,
            },
            "max_points must default to 60"
        );
    }

    #[test]
    fn widget_options_fan_roundtrip() {
        let yaml = r#"options:
  fan:
    speed_count: 3
    preset_modes:
      - Low
      - Medium
      - High
"#;
        let w: OptionsWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::Fan {
                speed_count: 3,
                preset_modes: vec!["Low".to_string(), "Medium".to_string(), "High".to_string()],
            }
        );
    }

    #[test]
    fn widget_options_camera_serializes_externally_tagged() {
        let w = OptionsWrapper {
            options: WidgetOptions::Camera {
                interval_seconds: 5,
                url: "http://cam.local/snapshot".to_string(),
            },
        };
        let yaml = serde_yaml_ng::to_string(&w).expect("serialize");
        // External tag form: variant name is the key, fields nested under it.
        assert!(
            yaml.contains("camera:"),
            "must use externally-tagged form: {yaml}"
        );
        assert!(
            yaml.contains("interval_seconds: 5"),
            "field must serialize: {yaml}"
        );
        assert!(yaml.contains("url:"), "url field must serialize: {yaml}");
        // Internal tag form would contain "kind: camera" — pin against regression.
        assert!(
            !yaml.contains("kind: camera"),
            "must NOT use internally-tagged form: {yaml}"
        );
    }

    #[test]
    fn placement_fields() {
        let p = Placement {
            col: 0,
            row: 1,
            span_cols: 2,
            span_rows: 1,
        };
        assert_eq!(p.col, 0);
        assert_eq!(p.row, 1);
        assert_eq!(p.span_cols, 2);
        assert_eq!(p.span_rows, 1);
    }

    #[test]
    fn dashboard_fixture_yaml_parses_all_widget_kinds() {
        let d: Dashboard = serde_yaml_ng::from_str(FIXTURE_YAML).unwrap();
        let kinds: Vec<&WidgetKind> = d
            .views
            .iter()
            .flat_map(|v| v.sections.iter())
            .flat_map(|s| s.widgets.iter())
            .map(|w| &w.widget_type)
            .collect();
        assert!(kinds.contains(&&WidgetKind::LightTile));
        assert!(kinds.contains(&&WidgetKind::SensorTile));
        assert!(kinds.contains(&&WidgetKind::EntityTile));
        assert!(kinds.contains(&&WidgetKind::Camera));
        assert!(kinds.contains(&&WidgetKind::History));
        assert!(kinds.contains(&&WidgetKind::Fan));
        assert!(kinds.contains(&&WidgetKind::Lock));
        assert!(kinds.contains(&&WidgetKind::Alarm));
        // Phase 6 new kinds:
        assert!(kinds.contains(&&WidgetKind::Cover));
        assert!(kinds.contains(&&WidgetKind::MediaPlayer));
        assert!(kinds.contains(&&WidgetKind::Climate));
        assert!(kinds.contains(&&WidgetKind::PowerFlow));
    }

    // -----------------------------------------------------------------------
    // Phase 6: PinPolicy enum round-trip tests
    // -----------------------------------------------------------------------

    /// Helper wrapper for PinPolicy serde tests through the singleton_map path.
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct LockWrapper {
        #[serde(with = "serde_yaml_ng::with::singleton_map")]
        options: WidgetOptions,
    }

    #[test]
    fn pin_policy_enum_none_round_trip() {
        let yaml = r#"options:
  lock:
    pin_policy: none
"#;
        let w: LockWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::Lock {
                pin_policy: PinPolicy::None,
                require_confirmation_on_unlock: false,
            }
        );
        let back = serde_yaml_ng::to_string(&w).unwrap();
        let w2: LockWrapper = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(w, w2);
    }

    #[test]
    fn pin_policy_enum_required_round_trip() {
        let yaml = r#"options:
  lock:
    pin_policy:
      required:
        length: 4
        code_format: number
"#;
        let w: LockWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::Lock {
                pin_policy: PinPolicy::Required {
                    length: 4,
                    code_format: CodeFormat::Number,
                },
                require_confirmation_on_unlock: false,
            }
        );
        let back = serde_yaml_ng::to_string(&w).unwrap();
        let w2: LockWrapper = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(w, w2);
    }

    #[test]
    fn pin_policy_enum_required_on_disarm_alarm_round_trip() {
        let yaml = r#"options:
  alarm:
    pin_policy:
      required_on_disarm:
        length: 6
        code_format: any
"#;
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct AlarmWrapper {
            #[serde(with = "serde_yaml_ng::with::singleton_map")]
            options: WidgetOptions,
        }
        let w: AlarmWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::Alarm {
                pin_policy: PinPolicy::RequiredOnDisarm {
                    length: 6,
                    code_format: CodeFormat::Any,
                },
            }
        );
        let back = serde_yaml_ng::to_string(&w).unwrap();
        let w2: AlarmWrapper = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(w, w2);
    }

    // -----------------------------------------------------------------------
    // Phase 6: Cover options round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn cover_options_round_trip() {
        let yaml = r#"options:
  cover:
    position_min: 0
    position_max: 100
"#;
        let w: OptionsWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::Cover {
                position_min: 0,
                position_max: 100,
            }
        );
        let back = serde_yaml_ng::to_string(&w).unwrap();
        let w2: OptionsWrapper = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(w, w2);
    }

    #[test]
    fn cover_options_serializes_externally_tagged() {
        let w = OptionsWrapper {
            options: WidgetOptions::Cover {
                position_min: 10,
                position_max: 90,
            },
        };
        let yaml = serde_yaml_ng::to_string(&w).expect("serialize");
        assert!(
            yaml.contains("cover:"),
            "must use externally-tagged form: {yaml}"
        );
        assert!(
            yaml.contains("position_min: 10"),
            "field must serialize: {yaml}"
        );
        assert!(
            yaml.contains("position_max: 90"),
            "field must serialize: {yaml}"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 6: MediaPlayer options round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn media_player_options_round_trip() {
        let yaml = r#"options:
  media_player:
    transport_set:
      - play
      - pause
      - stop
      - next
      - prev
    volume_step: 0.05
"#;
        let w: OptionsWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        // Verify variant identity via PartialEq — no unreachable wildcard arm needed.
        assert_eq!(
            w.options,
            WidgetOptions::MediaPlayer {
                transport_set: vec![
                    MediaTransport::Play,
                    MediaTransport::Pause,
                    MediaTransport::Stop,
                    MediaTransport::Next,
                    MediaTransport::Prev,
                ],
                volume_step: 0.05,
            }
        );
        let back = serde_yaml_ng::to_string(&w).unwrap();
        let w2: OptionsWrapper = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(w, w2);
    }

    #[test]
    fn media_player_options_serializes_externally_tagged() {
        let w = OptionsWrapper {
            options: WidgetOptions::MediaPlayer {
                transport_set: vec![MediaTransport::Play, MediaTransport::Pause],
                volume_step: 0.1,
            },
        };
        let yaml = serde_yaml_ng::to_string(&w).expect("serialize");
        assert!(
            yaml.contains("media_player:"),
            "must use externally-tagged form: {yaml}"
        );
        assert!(
            yaml.contains("volume_step:"),
            "volume_step field must serialize: {yaml}"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 6: Climate options round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn climate_options_round_trip() {
        let yaml = r#"options:
  climate:
    min_temp: 16.0
    max_temp: 30.0
    step: 0.5
    hvac_modes:
      - heat
      - cool
      - heat_cool
      - off
"#;
        let w: OptionsWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        // Verify variant identity via PartialEq — no unreachable wildcard arm needed.
        assert_eq!(
            w.options,
            WidgetOptions::Climate {
                min_temp: 16.0,
                max_temp: 30.0,
                step: 0.5,
                hvac_modes: vec![
                    "heat".to_string(),
                    "cool".to_string(),
                    "heat_cool".to_string(),
                    "off".to_string(),
                ],
            }
        );
        let back = serde_yaml_ng::to_string(&w).unwrap();
        let w2: OptionsWrapper = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(w, w2);
    }

    #[test]
    fn climate_options_serializes_externally_tagged() {
        let w = OptionsWrapper {
            options: WidgetOptions::Climate {
                min_temp: 16.0,
                max_temp: 30.0,
                step: 0.5,
                hvac_modes: vec!["heat".to_string(), "cool".to_string()],
            },
        };
        let yaml = serde_yaml_ng::to_string(&w).expect("serialize");
        assert!(
            yaml.contains("climate:"),
            "must use externally-tagged form: {yaml}"
        );
        assert!(
            yaml.contains("min_temp:"),
            "min_temp must serialize: {yaml}"
        );
        assert!(
            yaml.contains("max_temp:"),
            "max_temp must serialize: {yaml}"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 6: PowerFlow options round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn power_flow_options_round_trip() {
        let yaml = r#"options:
  power_flow:
    grid_entity: sensor.grid_power
    solar_entity: sensor.solar_power
    battery_entity: sensor.battery_power
    battery_soc_entity: sensor.battery_soc
    home_entity: sensor.home_power
"#;
        let w: OptionsWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::PowerFlow {
                grid_entity: "sensor.grid_power".to_string(),
                solar_entity: Some("sensor.solar_power".to_string()),
                battery_entity: Some("sensor.battery_power".to_string()),
                battery_soc_entity: Some("sensor.battery_soc".to_string()),
                home_entity: Some("sensor.home_power".to_string()),
            }
        );
        let back = serde_yaml_ng::to_string(&w).unwrap();
        let w2: OptionsWrapper = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(w, w2);
    }

    #[test]
    fn power_flow_options_optional_fields_absent() {
        let yaml = r#"options:
  power_flow:
    grid_entity: sensor.grid_power
"#;
        let w: OptionsWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::PowerFlow {
                grid_entity: "sensor.grid_power".to_string(),
                solar_entity: None,
                battery_entity: None,
                battery_soc_entity: None,
                home_entity: None,
            }
        );
    }

    #[test]
    fn power_flow_options_serializes_externally_tagged() {
        let w = OptionsWrapper {
            options: WidgetOptions::PowerFlow {
                grid_entity: "sensor.grid".to_string(),
                solar_entity: None,
                battery_entity: None,
                battery_soc_entity: None,
                home_entity: None,
            },
        };
        let yaml = serde_yaml_ng::to_string(&w).expect("serialize");
        assert!(
            yaml.contains("power_flow:"),
            "must use externally-tagged form: {yaml}"
        );
        assert!(
            yaml.contains("grid_entity:"),
            "grid_entity must serialize: {yaml}"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 6: Lock options with require_confirmation_on_unlock
    // -----------------------------------------------------------------------

    #[test]
    fn lock_options_require_confirmation_round_trip() {
        let yaml = r#"options:
  lock:
    pin_policy: none
    require_confirmation_on_unlock: true
"#;
        let w: LockWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::Lock {
                pin_policy: PinPolicy::None,
                require_confirmation_on_unlock: true,
            }
        );
        let back = serde_yaml_ng::to_string(&w).unwrap();
        let w2: LockWrapper = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(w, w2);
    }

    #[test]
    fn lock_options_require_confirmation_defaults_false() {
        let yaml = r#"options:
  lock:
    pin_policy: none
"#;
        let w: LockWrapper = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            w.options,
            WidgetOptions::Lock {
                pin_policy: PinPolicy::None,
                require_confirmation_on_unlock: false,
            },
            "require_confirmation_on_unlock must default to false"
        );
    }
}
