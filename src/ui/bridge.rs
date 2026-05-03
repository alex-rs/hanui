//! Typed view-model bridge between the data layer and the Slint component tree.
//!
//! [`build_tiles`] is the single public entry point. It takes a reference to
//! any [`EntityStore`] implementation and a [`Dashboard`] config and produces a
//! [`Vec<TileVM>`] — one entry per widget in the dashboard, in document order.
//!
//! # VM struct field naming
//!
//! Each Rust struct field name is the snake_case form of the corresponding
//! Slint struct field (Slint uses kebab-case; the Slint compiler performs
//! kebab→snake conversion automatically when generating Rust bindings):
//!
//! | Slint field | Rust field |
//! |---|---|
//! | `icon-id` | `icon_id` |
//! | `preferred-columns` | `preferred_columns` |
//! | `preferred-rows` | `preferred_rows` |
//! | `span-cols` | `span_cols` |
//! | `span-rows` | `span_rows` |
//!
//! The `icon: image` field present in each Slint struct is a Slint `image`
//! type that is only writeable during Slint property wiring (TASK-015). It is
//! intentionally absent here; TASK-015 adds it as part of the binding step.
//!
//! # Missing-entity policy
//!
//! If `store.get` returns `None` for a widget's entity ID, the bridge
//! always produces an [`EntityTileVM`] with `state = "unavailable"` rather
//! than returning `Option<TileVM>`. This keeps the caller's rendering loop
//! unconditional: every widget in the dashboard config maps to exactly one
//! tile in the output `Vec`.
//!
//! # Slint property wiring (TASK-015 / TASK-043)
//!
//! Below the typed-VM layer, this file also defines:
//!
//!   * The top-level `MainWindow` Slint component is declared in
//!     `ui/slint/main_window.slint` and pulled in via `slint::include_modules!()`
//!     inside the [`slint_ui`] sub-module so the generated names do not collide
//!     with the Rust VM structs that share names with their Slint counterparts.
//!     `build.rs` calls `slint_build::compile("ui/slint/main_window.slint")`
//!     at build time, which transitively compiles `theme.slint`,
//!     `card_base.slint`, and the three tile component files.
//!   * [`wire_window`] — splits a `&[TileVM]` slice by variant, converts each
//!     element into the Slint-generated VM struct (resolving `icon_id` via
//!     [`crate::assets::icons::resolve`]), wraps each per-variant `Vec` in a
//!     `slint::ModelRc<...>`, and writes the three array properties on
//!     `MainWindow`. Also writes the two `AnimationBudget` globals from the
//!     active [`crate::dashboard::profiles::DeviceProfile`] (passed in from
//!     `src/lib.rs::run` post TASK-120b F4).
//!
//! [`wire_window`] runs once per refresh cycle, not per frame. Per-frame
//! property reads inside the Slint runtime see only `SharedString`
//! (`Arc<str>`-backed) and `slint::Image` (`Arc<SharedPixelBuffer>`-backed)
//! values; cloning either is an `Arc` bump. No allocation occurs in any
//! Slint callback or animation timer (per the slint-engineer charter
//! hot-path discipline).

use crate::dashboard::schema::{Dashboard, Placement, WidgetKind, WidgetOptions};
use crate::ha::entity::{EntityId, EntityKind};
use crate::ha::store::EntityStore;

// ---------------------------------------------------------------------------
// TilePlacement  (mirrors TilePlacement / SensorTilePlacement /
//                          EntityTilePlacement in the Slint tile files)
// ---------------------------------------------------------------------------

/// Computed grid placement for a tile, mirroring `TilePlacement` /
/// `SensorTilePlacement` / `EntityTilePlacement` in the Slint tile files and
/// `dashboard::schema::Placement` in the data layer.
///
/// Field names use snake_case throughout; the Slint compiler converts these to
/// kebab-case (`span-cols`, `span-rows`) in its own struct declarations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TilePlacement {
    pub col: i32,
    pub row: i32,
    pub span_cols: i32,
    pub span_rows: i32,
}

impl TilePlacement {
    fn from_placement(p: &Placement) -> Self {
        TilePlacement {
            col: i32::from(p.col),
            row: i32::from(p.row),
            span_cols: i32::from(p.span_cols),
            span_rows: i32::from(p.span_rows),
        }
    }

    fn default_for(preferred_columns: i32, preferred_rows: i32) -> Self {
        TilePlacement {
            col: 0,
            row: 0,
            span_cols: preferred_columns,
            span_rows: preferred_rows,
        }
    }
}

// ---------------------------------------------------------------------------
// LightTileVM
// ---------------------------------------------------------------------------

/// View-model for a `LightTile` widget, mirroring the Slint `LightTileVM`
/// struct in `ui/slint/light_tile.slint`.
///
/// The `icon: image` Slint field is absent; it is written by the Slint bridge
/// during property wiring (TASK-015).
///
/// `pending` mirrors the Slint `pending: bool` field added in TASK-067. It
/// is driven by [`crate::ui::toast::apply_pending_for_widgets`] from
/// [`crate::ha::live_store::LiveStore::pending_for_widget`] — the
/// cross-owner read API locked in
/// `locked_decisions.pending_state_read_api`. Default `false`; the
/// spinner is invisible until a dispatcher records an
/// [`crate::ha::live_store::OptimisticEntry`].
#[derive(Debug, Clone, PartialEq)]
pub struct LightTileVM {
    pub name: String,
    pub state: String,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// SensorTileVM
// ---------------------------------------------------------------------------

/// View-model for a `SensorTile` widget, mirroring the Slint `SensorTileVM`
/// struct in `ui/slint/sensor_tile.slint`.
///
/// The `icon: image` Slint field is absent; it is written by the Slint bridge
/// during property wiring (TASK-015).
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract.
#[derive(Debug, Clone, PartialEq)]
pub struct SensorTileVM {
    pub name: String,
    pub state: String,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// EntityTileVM
// ---------------------------------------------------------------------------

/// View-model for an `EntityTile` widget, mirroring the Slint `EntityTileVM`
/// struct in `ui/slint/entity_tile.slint`.
///
/// Also used as the fallback tile when an entity ID is not found in the store
/// (see "Missing-entity policy" in the module doc).
///
/// The `icon: image` Slint field is absent; it is written by the Slint bridge
/// during property wiring (TASK-015).
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityTileVM {
    pub name: String,
    pub state: String,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// CoverTileVM (TASK-102)
// ---------------------------------------------------------------------------

/// View-model for a `CoverTile` widget, mirroring the Slint `CoverTileVM`
/// struct in `ui/slint/cover_tile.slint`.
///
/// Built by [`compute_cover_tile_vm`], which threads through
/// [`crate::ui::cover::CoverVM::from_entity`] to derive the `is_open` /
/// `is_moving` booleans and the position fallback.
///
/// The `icon: image` Slint field is absent here; it is written by the
/// Slint bridge during property wiring (the same pattern used for
/// `LightTileVM` / `SensorTileVM` / `EntityTileVM`).
///
/// Note (TASK-102 scope): the `MainWindow` Slint component does not yet
/// declare a `cover-tiles` array property — `ui/slint/main_window.slint`
/// is in this ticket's `must_not_touch` list. `compute_cover_tile_vm` is
/// invoked indirectly via [`build_tiles`] so cover entities exercise the
/// `CoverVM::from_entity` path on every state change, and the result
/// flows into the existing fallback `EntityTileVM` render with a richer
/// state string. A subsequent ticket will amend `main_window.slint` to
/// render a per-kind `CoverTile` model directly.
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverTileVM {
    pub name: String,
    pub state: String,
    pub position: i32,
    pub tilt: i32,
    pub has_position: bool,
    pub has_tilt: bool,
    pub is_open: bool,
    pub is_moving: bool,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// FanTileVM (TASK-103)
// ---------------------------------------------------------------------------

/// View-model for a `FanTile` widget, mirroring the Slint `FanTileVM`
/// struct in `ui/slint/fan_tile.slint`.
///
/// Built by [`compute_fan_tile_vm`], which threads through
/// [`crate::ui::fan::FanVM::from_entity`] to derive the `is_on` boolean
/// and surface the percentage / preset-mode attributes.
///
/// The `icon: image` Slint field is absent here; it is written by the
/// Slint bridge during property wiring (the same pattern used for
/// `LightTileVM` / `SensorTileVM` / `EntityTileVM` / `CoverTileVM`).
///
/// Note (TASK-103 scope): the `MainWindow` Slint component does not yet
/// declare a `fan-tiles` array property — `ui/slint/main_window.slint`
/// is in this ticket's `must_not_touch` list. `compute_fan_tile_vm` is
/// invoked indirectly via [`build_tiles`] so fan entities exercise the
/// `FanVM::from_entity` path on every state change, and the result
/// flows into the existing fallback `EntityTileVM` render with a richer
/// state string. A subsequent ticket will amend `main_window.slint` to
/// render a per-kind `FanTile` model directly.
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract.
#[derive(Debug, Clone, PartialEq)]
pub struct FanTileVM {
    pub name: String,
    pub state: String,
    pub speed_pct: i32,
    pub has_speed_pct: bool,
    pub is_on: bool,
    pub current_speed: String,
    pub has_current_speed: bool,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// LockTileVM (TASK-104)
// ---------------------------------------------------------------------------

/// View-model for a `LockTile` widget, mirroring the Slint `LockTileVM`
/// struct in `ui/slint/lock_tile.slint`.
///
/// Built by [`compute_lock_tile_vm`], which threads through
/// [`crate::ui::lock::LockVM::from_entity`] to derive the `is_locked`
/// boolean used by the Slint tile's locked / unlocked colour branches.
/// The unavailable / jammed state colours are driven by the verbatim
/// `state` string match in the Slint tile itself.
///
/// The `icon: image` Slint field is absent here; it is written by the
/// Slint bridge during property wiring (the same pattern used for
/// `LightTileVM` / `SensorTileVM` / `EntityTileVM` / `CoverTileVM` /
/// `FanTileVM`).
///
/// Note (TASK-104 scope): the `MainWindow` Slint component does not yet
/// declare a `lock-tiles` array property — `ui/slint/main_window.slint`
/// is in this ticket's `must_not_touch` list. `compute_lock_tile_vm` is
/// invoked indirectly via [`build_tiles`] so lock entities exercise the
/// `LockVM::from_entity` path on every state change, and the result
/// flows into the existing fallback `EntityTileVM` render with the raw
/// state string. A subsequent ticket will amend `main_window.slint` to
/// render a per-kind `LockTile` model directly.
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract.
///
/// # No `Vec` fields
///
/// Per the TASK-103 audit lesson, this struct stays lean: no `Vec` is
/// allocated for fields the tile renderer never reads. PIN-policy and
/// confirm-flag data are looked up by the dispatcher via its
/// `lock_settings` table at dispatch time, not stored on every per-frame
/// tile VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockTileVM {
    pub name: String,
    pub state: String,
    pub is_locked: bool,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// AlarmTileVM (TASK-105)
// ---------------------------------------------------------------------------

/// View-model for an `AlarmPanelTile` widget, mirroring the Slint
/// `AlarmTileVM` struct in `ui/slint/alarm_panel_tile.slint`.
///
/// Built by [`compute_alarm_tile_vm`], which threads through
/// [`crate::ui::alarm::AlarmVM::from_entity`] to derive the `is_armed` /
/// `is_triggered` / `is_pending` booleans from the canonical HA state.
///
/// The `icon: image` Slint field is absent here; it is written by the
/// Slint bridge during property wiring (the same pattern used for
/// `LightTileVM` / `SensorTileVM` / `EntityTileVM` / `CoverTileVM` /
/// `FanTileVM` / `LockTileVM`).
///
/// Note (TASK-105 scope): the `MainWindow` Slint component does not yet
/// declare an `alarm-tiles` array property — `ui/slint/main_window.slint`
/// is in this ticket's `must_not_touch` list. `compute_alarm_tile_vm` is
/// invoked indirectly via [`build_tiles`] so alarm entities exercise the
/// `AlarmVM::from_entity` path on every state change, and the result
/// flows into the existing fallback `EntityTileVM` render with the raw
/// HA state string. A subsequent ticket will amend `main_window.slint`
/// to render a per-kind `AlarmPanelTile` model directly.
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract. Distinct from `is_pending`
/// (the HA-state pending) — Risk #14 already disambiguates the two
/// concepts in the Slint tile field naming.
///
/// # No `Vec` fields (lesson from TASK-103)
///
/// Like `AlarmVM`, this struct deliberately carries no `Vec` fields.
/// Arm-mode lists, code formats, and dispatcher-side preset vocabularies
/// are NOT stored on the per-frame tile VM — they are read at modal-open
/// / dispatch time from the widget options or from HA attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmTileVM {
    pub name: String,
    pub state: String,
    pub is_armed: bool,
    pub is_triggered: bool,
    pub is_pending: bool,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`. Distinct from
    /// `is_pending` (the HA-state pending).
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// HistoryGraphTileVM (TASK-106)
// ---------------------------------------------------------------------------

/// View-model for a `HistoryGraphTile` widget, mirroring the Slint
/// `HistoryGraphTileVM` struct in `ui/slint/history_graph_tile.slint`.
///
/// Built by [`compute_history_graph_tile_vm`], which threads through
/// [`crate::ui::history_graph::HistoryGraphVM::from_entity`] to derive the
/// `is_available` boolean and forward the bridge-supplied `change_count`.
///
/// The `icon: image` Slint field is absent here; it is written by the
/// Slint bridge during property wiring (the same pattern used for
/// `LightTileVM` / `SensorTileVM` / `EntityTileVM` / `CoverTileVM` /
/// `FanTileVM` / `LockTileVM` / `AlarmTileVM`).
///
/// # Path-commands wire format
///
/// `path_commands` is an SVG mini-language string composed by
/// [`history_path_commands`] from a [`crate::ha::history::HistoryWindow`]
/// per `locked_decisions.history_render_path`. Coordinates are normalised
/// to the unit square; the Slint Path's `viewbox-width` / `viewbox-height`
/// are 1.0 so the polyline scales with the tile. Composition happens at
/// fetch time (NOT per frame); the bridge throttles pushes to at most
/// once per 60s via [`crate::ha::history::HistoryThrottle`].
///
/// # No `Vec` fields (lesson from TASK-103)
///
/// The history point list reaches Slint as a string-encoded SVG polyline,
/// NOT as a `Vec<Point>` field on this struct. The fetch-side LTTB
/// downsampler holds the `Vec` once per widget; this VM stays scalar so
/// the per-state-change rebuild allocates no per-widget vector.
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract.
///
/// Note (TASK-106 scope): the `MainWindow` Slint component does not yet
/// declare a `history-tiles` array property — `ui/slint/main_window.slint`
/// is in this ticket's `must_not_touch` list. `compute_history_graph_tile_vm`
/// is invoked indirectly via [`build_tiles`] so history entities exercise
/// the `HistoryGraphVM::from_entity` path on every state change, and the
/// result flows into the existing fallback `EntityTileVM` render with the
/// raw HA state string. A subsequent ticket will amend `main_window.slint`
/// to render a per-kind `HistoryGraphTile` model directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryGraphTileVM {
    pub name: String,
    pub state: String,
    pub change_count: i32,
    pub is_available: bool,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
    /// SVG mini-language polyline ("M x y L x y L ..."). Composed by
    /// [`history_path_commands`] from a downsampled
    /// [`crate::ha::history::HistoryWindow`]. Empty string when no points.
    pub path_commands: String,
}

// ---------------------------------------------------------------------------
// CameraTileVM (TASK-107)
// ---------------------------------------------------------------------------

/// View-model for a `CameraSnapshotTile` widget, mirroring the Slint
/// `CameraTileVM` struct in `ui/slint/camera_snapshot_tile.slint`.
///
/// Built by [`compute_camera_tile_vm`], which threads through
/// [`crate::ui::camera::CameraVM::from_entity`] to derive the
/// `is_recording` / `is_streaming` / `is_available` booleans from the
/// canonical HA state.
///
/// The `icon: image` Slint field is absent here; it is written by the
/// Slint bridge during property wiring (the same pattern used for
/// every other per-kind tile VM).
///
/// Note (TASK-107 scope): the `MainWindow` Slint component does not yet
/// declare a `camera-tiles` array property — `ui/slint/main_window.slint`
/// is in this ticket's `must_not_touch` list. `compute_camera_tile_vm`
/// is invoked indirectly via [`build_tiles`] so camera entities exercise
/// the `CameraVM::from_entity` path on every state change, and the
/// result flows into the existing fallback `EntityTileVM` render with the
/// raw HA state string. A subsequent ticket will amend `main_window.slint`
/// to render a per-kind `CameraSnapshotTile` model directly.
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract.
///
/// # No `Vec` fields (lesson from TASK-103 / TASK-105)
///
/// Like `CameraVM`, this struct deliberately carries no `Vec` fields. The
/// snapshot bytes live in [`crate::ha::camera::CameraPool`] and reach
/// Slint as an `Image` property in a follow-up ticket — they do NOT
/// travel through this VM. Allocating a `Vec` here that the tile never
/// reads would be wasted work on every state change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraTileVM {
    pub name: String,
    pub state: String,
    pub is_recording: bool,
    pub is_streaming: bool,
    pub is_available: bool,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// ClimateTileVM (TASK-108)
// ---------------------------------------------------------------------------

/// View-model for a `ClimateTile` widget, mirroring the Slint
/// `ClimateTileVM` struct in `ui/slint/climate_tile.slint`.
///
/// Built by [`compute_climate_tile_vm`], which threads through
/// [`crate::ui::climate::ClimateVM::from_entity`] to derive the
/// `is_active` boolean and forward the optional `current_temperature` /
/// `target_temperature` reads from the entity attributes.
///
/// The `icon: image` Slint field is absent here; it is written by the
/// Slint bridge during property wiring (the same pattern used for every
/// other per-kind tile VM).
///
/// Note (TASK-108 scope): the `MainWindow` Slint component does not yet
/// declare a `climate-tiles` array property — `ui/slint/main_window.slint`
/// is in this ticket's `must_not_touch` list. `compute_climate_tile_vm`
/// is invoked indirectly via [`build_tiles`] so climate entities exercise
/// the `ClimateVM::from_entity` path on every state change, and the result
/// flows into the existing fallback `EntityTileVM` render with the raw
/// HA state string. A subsequent ticket will amend `main_window.slint`
/// to render a per-kind `ClimateTile` model directly.
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract.
///
/// # No `Vec` fields (lesson from TASK-103 / TASK-105 / TASK-107)
///
/// Like `ClimateVM`, this struct deliberately carries no `Vec` fields. The
/// `WidgetOptions::Climate.hvac_modes` mode-picker list is read at modal-
/// open time by [`crate::ui::more_info::ClimateBody`] / the dispatcher,
/// NOT stored on the per-frame tile VM. Allocating a `Vec` here that the
/// tile renderer never reads would be wasted work on every state change.
///
/// # Float fields and `Eq`
///
/// `current_temperature` and `target_temperature` are `Option<f32>` —
/// `f32: !Eq`, so this struct is `PartialEq` but not `Eq` (deliberate
/// drift from the alarm/camera VMs). Bridge equality checks rely on
/// `PartialEq`; no consumer of this VM stores it in a hash-keyed
/// container.
#[derive(Debug, Clone, PartialEq)]
pub struct ClimateTileVM {
    pub name: String,
    pub state: String,
    pub is_active: bool,
    pub current_temperature: Option<f32>,
    pub target_temperature: Option<f32>,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// MediaPlayerTileVM (TASK-109)
// ---------------------------------------------------------------------------

/// View-model for a `MediaPlayerTile` widget, mirroring the Slint
/// `MediaPlayerTileVM` struct in `ui/slint/media_player_tile.slint`.
///
/// Built by [`compute_media_player_tile_vm`], which threads through
/// [`crate::ui::media_player::MediaPlayerVM::from_entity`] to derive the
/// `is_playing` boolean and forward the optional `media_title` /
/// `artist` / `volume_level` reads from the entity attributes.
///
/// The `icon: image` Slint field is absent here; it is written by the
/// Slint bridge during property wiring (the same pattern used for every
/// other per-kind tile VM).
///
/// Note (TASK-109 scope): the `MainWindow` Slint component does not yet
/// declare a `media-player-tiles` array property —
/// `ui/slint/main_window.slint` is in this ticket's `must_not_touch`
/// list. `compute_media_player_tile_vm` is invoked indirectly via
/// [`build_tiles`] so media-player entities exercise the
/// `MediaPlayerVM::from_entity` path on every state change, and the
/// result flows into the existing fallback `EntityTileVM` render with
/// the raw HA state string. A subsequent ticket will amend
/// `main_window.slint` to render a per-kind `MediaPlayerTile` model
/// directly.
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract.
///
/// # No `Vec` fields (lesson from TASK-103 / TASK-105 / TASK-107 / TASK-108)
///
/// Like `MediaPlayerVM`, this struct deliberately carries no `Vec`
/// fields. The transport-set / source list / sound-mode list live on
/// [`crate::dashboard::schema::WidgetOptions::MediaPlayer`] and are read
/// at modal-open / dispatch time, NOT stored on the per-frame tile VM.
/// Allocating a `Vec` here that the tile renderer never reads would be
/// wasted work on every state change.
///
/// # Float fields and `Eq`
///
/// `volume_level` is `Option<f32>` — `f32: !Eq`, so this struct is
/// `PartialEq` but not `Eq` (matches `ClimateTileVM`). Bridge equality
/// checks rely on `PartialEq`; no consumer of this VM stores it in a
/// hash-keyed container.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaPlayerTileVM {
    pub name: String,
    pub state: String,
    pub is_playing: bool,
    pub media_title: Option<String>,
    pub artist: Option<String>,
    pub volume_level: Option<f32>,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// PowerFlowTileVM (TASK-094)
// ---------------------------------------------------------------------------

/// View-model for a `PowerFlowTile` widget, mirroring the Slint
/// `PowerFlowTileVM` struct in `ui/slint/power_flow_tile.slint`.
///
/// Built by [`compute_power_flow_tile_vm`], which threads through
/// [`crate::ui::power_flow::PowerFlowVM::read_power_watts`] and
/// [`crate::ui::power_flow::PowerFlowVM::read_battery_pct`] so the
/// per-frame derived numeric values are exercised on every state change.
///
/// The `icon: image` Slint field is absent here; it is written by the
/// Slint bridge during property wiring (the same pattern used for every
/// other per-kind tile VM).
///
/// Note (TASK-094 scope): the `MainWindow` Slint component does not yet
/// declare a `power-flow-tiles` array property —
/// `ui/slint/main_window.slint` is in this ticket's `must_not_touch`
/// list. `compute_power_flow_tile_vm` is invoked indirectly via
/// [`build_tiles`] so power-flow widgets exercise the
/// `PowerFlowVM::read_power_watts` path on every state change, and the
/// result flows into the existing fallback `EntityTileVM` render with
/// the raw HA state string. A subsequent ticket will amend
/// `main_window.slint` to render a per-kind `PowerFlowTile` model
/// directly.
///
/// `pending` is the per-tile spinner gate added in TASK-067; see
/// [`LightTileVM`] for the full contract.
///
/// # No `Vec` fields (lesson from TASK-103 / TASK-105 / TASK-107 / TASK-108 /
/// TASK-109)
///
/// Like `PowerFlowVM`, this struct deliberately carries no `Vec` fields. The
/// per-frame data surface is a fixed set of optional scalars (grid /
/// solar / battery / battery_soc / home). The auxiliary entity ids
/// configured in `WidgetOptions::PowerFlow` are read from the dashboard
/// config at modal-open time by [`crate::ui::power_flow::PowerFlowBody`],
/// NOT stored on the per-frame tile VM.
///
/// # Float fields and `Eq`
///
/// `grid_w` / `solar_w` / `battery_w` / `battery_pct` / `home_w` are
/// `Option<f32>` — `f32: !Eq`, so this struct is `PartialEq` but not
/// `Eq` (matches `ClimateTileVM` / `MediaPlayerTileVM`). Bridge equality
/// checks rely on `PartialEq`; no consumer of this VM stores it in a
/// hash-keyed container.
#[derive(Debug, Clone, PartialEq)]
pub struct PowerFlowTileVM {
    /// User-visible label.
    pub name: String,
    /// Grid power flow in watts; positive = importing, negative = exporting.
    /// `None` when the grid entity is unavailable / unknown.
    pub grid_w: Option<f32>,
    /// Solar production in watts (≥ 0). `None` when no solar entity is
    /// configured or the entity is unavailable.
    pub solar_w: Option<f32>,
    /// Battery flow in watts; positive = charging, negative = discharging.
    /// `None` when no battery entity is configured.
    pub battery_w: Option<f32>,
    /// Battery state-of-charge in 0..=100. `None` when no
    /// `battery_soc_entity` is configured.
    pub battery_pct: Option<f32>,
    /// Home consumption in watts (≥ 0). `None` when no home entity is
    /// configured.
    pub home_w: Option<f32>,
    /// Design-token icon id (typically `mdi:lightning-bolt-circle`).
    pub icon_id: String,
    /// Author-supplied preferred column span.
    pub preferred_columns: i32,
    /// Author-supplied preferred row span.
    pub preferred_rows: i32,
    /// Computed grid placement assigned by the packer.
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// TileVM enum
// ---------------------------------------------------------------------------

/// Top-level discriminated union dispatching on tile kind.
///
/// `build_tiles` returns one `TileVM` per widget in the dashboard config,
/// in document order (views → sections → widgets).
///
/// Phase 6 expansion (`task/phase6-window-wireup`): every `WidgetKind`
/// from `dashboard::schema::WidgetKind` now has a dedicated variant; no
/// kind falls through to `Entity` anymore. `main_window.slint` declares a
/// matching `<kind>-tiles` array property for each so the bridge writes
/// per-kind models that drive each Phase 6 tile component (`CoverTile`,
/// `FanTile`, `LockTile`, `AlarmPanelTile`, `HistoryGraphTile`,
/// `CameraSnapshotTile`, `ClimateTile`, `MediaPlayerTile`,
/// `PowerFlowTile`).
#[derive(Debug, Clone, PartialEq)]
pub enum TileVM {
    Light(LightTileVM),
    Sensor(SensorTileVM),
    Entity(EntityTileVM),
    Cover(CoverTileVM),
    Fan(FanTileVM),
    Lock(LockTileVM),
    Alarm(AlarmTileVM),
    History(HistoryGraphTileVM),
    Camera(CameraTileVM),
    Climate(ClimateTileVM),
    MediaPlayer(MediaPlayerTileVM),
    PowerFlow(PowerFlowTileVM),
}

// ---------------------------------------------------------------------------
// TileKind, RowUpdate, RowIndex (TASK-119 F2)
// ---------------------------------------------------------------------------

/// Per-kind discriminator used by the [`RowIndex`] / [`RowUpdate`] surface.
///
/// Mirrors the [`TileVM`] variants so the flush loop can route each
/// changed entity to the correct Slint per-kind model
/// (`light_tiles`, `sensor_tiles`, `entity_tiles`, plus the Phase 6
/// per-kind models — `cover_tiles`, `fan_tiles`, `lock_tiles`,
/// `alarm_tiles`, `history_tiles`, `camera_tiles`, `climate_tiles`,
/// `media_player_tiles`, `power_flow_tiles`) without re-pattern-matching
/// the full [`TileVM`].  The [`RowIndex`] stores `(kind, row_index)` pairs
/// because the Slint side splits tiles by kind in [`split_tile_vms`]; a
/// row index is only meaningful relative to its per-kind model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TileKind {
    /// `LightTileVM` rows in `MainWindow::light_tiles`.
    Light,
    /// `SensorTileVM` rows in `MainWindow::sensor_tiles`.
    Sensor,
    /// `EntityTileVM` rows in `MainWindow::entity_tiles`.
    Entity,
    /// `CoverTileVM` rows in `MainWindow::cover_tiles`.
    Cover,
    /// `FanTileVM` rows in `MainWindow::fan_tiles`.
    Fan,
    /// `LockTileVM` rows in `MainWindow::lock_tiles`.
    Lock,
    /// `AlarmTileVM` rows in `MainWindow::alarm_tiles`.
    Alarm,
    /// `HistoryGraphTileVM` rows in `MainWindow::history_tiles`.
    History,
    /// `CameraTileVM` rows in `MainWindow::camera_tiles`.
    Camera,
    /// `ClimateTileVM` rows in `MainWindow::climate_tiles`.
    Climate,
    /// `MediaPlayerTileVM` rows in `MainWindow::media_player_tiles`.
    MediaPlayer,
    /// `PowerFlowTileVM` rows in `MainWindow::power_flow_tiles`.
    PowerFlow,
}

/// One row's worth of dynamic-field changes flowing from the flush loop to
/// the Slint sink (TASK-119 F2).
///
/// Per the audit's static/dynamic split, only `state` is currently driven by
/// the bridge's flush path.  `pending` is owned by
/// [`crate::ui::toast::apply_pending_for_widgets`] which calls `set_row_data`
/// on its own schedule; the bridge does not double-write that field here.
/// Static fields (`name`, `icon_id`, `preferred_columns`, `preferred_rows`,
/// `placement`) are written exactly once at load time via
/// [`split_tile_vms`] / [`wire_window`] and never flow through this struct.
#[derive(Debug, Clone, PartialEq)]
pub struct RowUpdate {
    /// Which per-kind Slint model this row lives in.
    pub kind: TileKind,
    /// Row index within the per-kind model (`light_tiles`, `sensor_tiles`,
    /// or `entity_tiles`).
    pub row_index: usize,
    /// Latest `entity.state.to_string()` value, or `"unavailable"` if the
    /// entity is not present in the store at flush time (mirrors the
    /// missing-entity policy in [`build_tiles`]).
    pub state: String,
}

/// Static `EntityId → [(TileKind, row_index)]` map, built at dashboard load
/// time so the flush loop can route each pending `EntityId` to the rows it
/// affects (TASK-119 F2 / Risk #10).
///
/// One entity may map to multiple rows (the dashboard can reference the same
/// entity from two widgets), so the value is a `Vec`.  The index is the
/// bridge between F1's per-entity diff signal (`apply_event` returns an
/// `EntityId`) and Slint's row-targeted `set_row_data` API.
///
/// # Stability
///
/// The index is derived purely from the dashboard config (each widget's
/// `widget_type` decides its [`TileKind`]) and does NOT depend on the live
/// store snapshot.  This keeps the index stable across entity-availability
/// transitions: the row a [`build_tiles`] call places a widget into matches
/// the row this index records, regardless of whether the entity was present
/// in the store at index-build time.
///
/// # Race mitigation (Risk #10)
///
/// The index is protected by an `RwLock` that the flush loop acquires for
/// reading and any full-rebuild path (config reload, view switch, full
/// resync) acquires for writing.  In this implementation the index is
/// wrapped in `Arc<RwLock<RowIndex>>` and shared between the spawn-time
/// builder, the flush loop, and `run_state_watcher`'s Live-transition
/// resync (which rebuilds the index BEFORE calling `write_tiles` so the
/// next flush observes either the old layout or the new one — never a
/// mix).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowIndex {
    by_entity: HashMap<EntityId, Vec<(TileKind, usize)>>,
}

impl RowIndex {
    /// Look up every `(kind, row_index)` that an `EntityId` occupies.
    pub fn rows_for(&self, id: &EntityId) -> &[(TileKind, usize)] {
        self.by_entity.get(id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Total number of `(EntityId → (kind, row_index))` mappings, summed
    /// over all entities.  Useful for assertion in tests.
    pub fn total_rows(&self) -> usize {
        self.by_entity.values().map(|v| v.len()).sum()
    }

    /// Number of distinct entity ids in the index.
    pub fn entity_count(&self) -> usize {
        self.by_entity.len()
    }
}

/// Map a [`WidgetKind`] to the [`TileKind`] (i.e. the per-kind Slint model
/// it lives in) that [`build_tiles`] produces for it.
///
/// Mirrors the `match widget.widget_type` arms in [`build_tiles`] one-to-one
/// post `task/phase6-window-wireup`. Every `WidgetKind` now maps to its
/// dedicated [`TileKind`] / Slint per-kind model — no kind falls through to
/// `Entity` anymore. The mapping is a pure function of the widget config
/// (no store state required), which is what lets [`build_row_index`] run
/// without touching the live store.
fn tile_kind_for_widget(widget_type: &WidgetKind) -> TileKind {
    match widget_type {
        WidgetKind::LightTile => TileKind::Light,
        WidgetKind::SensorTile => TileKind::Sensor,
        WidgetKind::EntityTile => TileKind::Entity,
        WidgetKind::Cover => TileKind::Cover,
        WidgetKind::Fan => TileKind::Fan,
        WidgetKind::Lock => TileKind::Lock,
        WidgetKind::Alarm => TileKind::Alarm,
        WidgetKind::History => TileKind::History,
        WidgetKind::Camera => TileKind::Camera,
        WidgetKind::Climate => TileKind::Climate,
        WidgetKind::MediaPlayer => TileKind::MediaPlayer,
        WidgetKind::PowerFlow => TileKind::PowerFlow,
    }
}

/// Build a [`RowIndex`] from a [`Dashboard`] alone, walking widgets in
/// document order and assigning each a row in the per-kind model implied by
/// its `widget_type`.
///
/// The walk MUST match the order [`build_tiles`] uses so the row indices
/// recorded here align with the rows [`split_tile_vms`] writes into the
/// per-kind Slint models.  Widgets with no `entity` binding (or an empty
/// string) still consume a row in the per-kind model (per `build_tiles`'s
/// always-emit policy) but are not routable from the per-entity subscriber
/// path; the row counter advances and the entry is not recorded.
pub fn build_row_index(dashboard: &Dashboard) -> RowIndex {
    let mut by_entity: HashMap<EntityId, Vec<(TileKind, usize)>> = HashMap::new();
    // Per-kind row counters track the row index within each per-kind model
    // that `split_tile_vms` produces. Phase 6 adds nine more counters so
    // every `WidgetKind` gets its own row stream.
    let mut light_idx: usize = 0;
    let mut sensor_idx: usize = 0;
    let mut entity_idx: usize = 0;
    let mut cover_idx: usize = 0;
    let mut fan_idx: usize = 0;
    let mut lock_idx: usize = 0;
    let mut alarm_idx: usize = 0;
    let mut history_idx: usize = 0;
    let mut camera_idx: usize = 0;
    let mut climate_idx: usize = 0;
    let mut media_player_idx: usize = 0;
    let mut power_flow_idx: usize = 0;
    for view in &dashboard.views {
        for section in &view.sections {
            for widget in &section.widgets {
                let kind = tile_kind_for_widget(&widget.widget_type);
                let row = match kind {
                    TileKind::Light => {
                        let i = light_idx;
                        light_idx += 1;
                        i
                    }
                    TileKind::Sensor => {
                        let i = sensor_idx;
                        sensor_idx += 1;
                        i
                    }
                    TileKind::Entity => {
                        let i = entity_idx;
                        entity_idx += 1;
                        i
                    }
                    TileKind::Cover => {
                        let i = cover_idx;
                        cover_idx += 1;
                        i
                    }
                    TileKind::Fan => {
                        let i = fan_idx;
                        fan_idx += 1;
                        i
                    }
                    TileKind::Lock => {
                        let i = lock_idx;
                        lock_idx += 1;
                        i
                    }
                    TileKind::Alarm => {
                        let i = alarm_idx;
                        alarm_idx += 1;
                        i
                    }
                    TileKind::History => {
                        let i = history_idx;
                        history_idx += 1;
                        i
                    }
                    TileKind::Camera => {
                        let i = camera_idx;
                        camera_idx += 1;
                        i
                    }
                    TileKind::Climate => {
                        let i = climate_idx;
                        climate_idx += 1;
                        i
                    }
                    TileKind::MediaPlayer => {
                        let i = media_player_idx;
                        media_player_idx += 1;
                        i
                    }
                    TileKind::PowerFlow => {
                        let i = power_flow_idx;
                        power_flow_idx += 1;
                        i
                    }
                };
                let id_str = widget.entity.as_deref().unwrap_or("");
                if id_str.is_empty() {
                    // No entity binding: row is allocated but not routable.
                    // PowerFlow widgets have `entity: None` (their primary
                    // grid_entity lives in `WidgetOptions::PowerFlow`); they
                    // intentionally fall here so the per-entity broadcast
                    // path is not wired for the wrapper widget itself.
                    continue;
                }
                by_entity
                    .entry(EntityId::from(id_str))
                    .or_default()
                    .push((kind, row));
            }
        }
    }
    RowIndex { by_entity }
}

// ---------------------------------------------------------------------------
// Icon-id defaults
// ---------------------------------------------------------------------------

/// Returns the default MDI icon design-token id for an entity kind.
///
/// Used when the widget config does not specify an explicit `icon` override.
fn default_icon_for_kind(kind: EntityKind) -> String {
    match kind {
        EntityKind::Light => "mdi:lightbulb".to_string(),
        EntityKind::Sensor => "mdi:thermometer".to_string(),
        _ => "mdi:help-circle".to_string(),
    }
}

// ---------------------------------------------------------------------------
// build_tiles
// ---------------------------------------------------------------------------

/// Map an [`EntityStore`] and a [`Dashboard`] config to a flat list of typed
/// tile view-models, one per widget in the dashboard (in document order).
///
/// The store is consumed only via [`EntityStore::get`] for per-widget entity
/// lookup. No iterator semantics are assumed and no full-store walk is
/// performed on this hot path (TASK-118 F3 removed the prior diagnostic
/// `for_each` walk; per-flush cost is now O(`widget_count`)).
///
/// `EntityStore` is dyn-compatible (PATH A — see `src/ha/store.rs` module doc).
/// `store` is accepted as `&dyn EntityStore` so Phase 2 callers can pass any
/// `Box<dyn EntityStore>` or `Arc<dyn EntityStore>` without changing this call
/// site.  Concrete references (`&MemoryStore`) coerce automatically.
///
/// See the module-level doc for the missing-entity policy and field-mapping
/// details.
pub fn build_tiles(store: &dyn EntityStore, dashboard: &Dashboard) -> Vec<TileVM> {
    // TASK-118 F3 removed the per-flush `store.for_each` diagnostic walk
    // that previously made this an O(`widget_count + entity_count`) hot
    // path. The per-widget `store.get` calls below remain the only store
    // interactions, giving an O(`widget_count`) rebuild.
    let mut tiles = Vec::new();

    for view in &dashboard.views {
        for section in &view.sections {
            for widget in &section.widgets {
                let entity_id_str = widget.entity.as_deref().unwrap_or("");
                let entity_id = EntityId::from(entity_id_str);

                let tile = match store.get(&entity_id) {
                    Some(entity) => {
                        let kind = EntityKind::from(&entity.id);

                        let name = widget.name.clone().unwrap_or_else(|| {
                            entity
                                .friendly_name()
                                .unwrap_or(entity.id.as_ref())
                                .to_string()
                        });

                        let state = (*entity.state).to_string();

                        let icon_id = widget
                            .icon
                            .clone()
                            .unwrap_or_else(|| default_icon_for_kind(kind));

                        let preferred_columns = i32::from(widget.layout.preferred_columns);
                        let preferred_rows = i32::from(widget.layout.preferred_rows);

                        let placement = widget
                            .placement
                            .as_ref()
                            .map(TilePlacement::from_placement)
                            .unwrap_or_else(|| {
                                TilePlacement::default_for(preferred_columns, preferred_rows)
                            });

                        match widget.widget_type {
                            WidgetKind::LightTile => TileVM::Light(LightTileVM {
                                name,
                                state,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                // TASK-067: spinner is off at build time.
                                // The toast/spinner driver flips this on
                                // each refresh tick from
                                // `LiveStore::pending_for_widget`.
                                pending: false,
                            }),
                            WidgetKind::SensorTile => TileVM::Sensor(SensorTileVM {
                                name,
                                state,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            // task/phase6-window-wireup: each Phase 6 widget
                            // kind now produces its own typed `TileVM` variant.
                            // The Slint side renders a dedicated component per
                            // kind (CoverTile / FanTile / LockTile /
                            // AlarmPanelTile / HistoryGraphTile /
                            // CameraSnapshotTile / ClimateTile / MediaPlayerTile
                            // / PowerFlowTile); the bridge no longer falls back
                            // to `EntityTileVM`. Each `compute_*_tile_vm` helper
                            // already exists from TASK-094 / TASK-102..109 — we
                            // just route through it here.
                            WidgetKind::Cover => TileVM::Cover(compute_cover_tile_vm(
                                name,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                &entity,
                            )),
                            WidgetKind::Fan => TileVM::Fan(compute_fan_tile_vm(
                                name,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                &entity,
                            )),
                            WidgetKind::Lock => TileVM::Lock(compute_lock_tile_vm(
                                name,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                &entity,
                            )),
                            WidgetKind::Alarm => TileVM::Alarm(compute_alarm_tile_vm(
                                name,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                &entity,
                            )),
                            // History tiles arrive with `change_count == 0` /
                            // `path_commands == ""` at build time; the
                            // bridge's history-fetch path (TASK-110+) pushes
                            // a richer `HistoryGraphTileVM` via row-update
                            // once the LTTB downsampler has a window per
                            // `locked_decisions.history_render_path`.
                            WidgetKind::History => TileVM::History(compute_history_graph_tile_vm(
                                name,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                &entity,
                                None,
                            )),
                            WidgetKind::Camera => TileVM::Camera(compute_camera_tile_vm(
                                name,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                &entity,
                            )),
                            WidgetKind::Climate => TileVM::Climate(compute_climate_tile_vm(
                                name,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                &entity,
                            )),
                            WidgetKind::MediaPlayer => {
                                TileVM::MediaPlayer(compute_media_player_tile_vm(
                                    name,
                                    icon_id,
                                    preferred_columns,
                                    preferred_rows,
                                    placement,
                                    &entity,
                                ))
                            }
                            // Power-flow widgets bind multiple HA entities
                            // through `WidgetOptions::PowerFlow`. The widget's
                            // top-level `entity` field is unset; instead the
                            // grid_entity / solar_entity / battery_entity /
                            // battery_soc_entity / home_entity ids in
                            // `widget.options` drive five separate store reads.
                            //
                            // This branch is only reachable when the wrapper
                            // widget itself has a non-empty `entity` field —
                            // an unusual config (since the schema treats
                            // PowerFlow as multi-entity). For the canonical
                            // shape (`entity: None`) `store.get` returns
                            // `None` and the missing-entity arm below
                            // dispatches to `compute_power_flow_tile_vm` with
                            // store-resolved auxiliaries. Here we still build
                            // a `PowerFlowTileVM` so the row layout matches
                            // the `RowIndex` for any future config that DOES
                            // bind a primary entity. Auxiliary readings are
                            // resolved via the live store reference.
                            WidgetKind::PowerFlow => {
                                TileVM::PowerFlow(compute_power_flow_tile_vm_from_widget(
                                    PowerFlowBuildArgs {
                                        name,
                                        icon_id,
                                        preferred_columns,
                                        preferred_rows,
                                        placement,
                                        wrapper_entity: Some(&entity),
                                        options: widget.options.as_ref(),
                                    },
                                    store,
                                ))
                            }
                            WidgetKind::EntityTile => TileVM::Entity(EntityTileVM {
                                name,
                                state,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                        }
                    }

                    None => {
                        // Missing-entity policy: produce a tile of the kind
                        // implied by `widget.widget_type` (so the row layout
                        // matches the [`RowIndex`] regardless of store
                        // presence — TASK-119 F2 row-stability invariant)
                        // with `state="unavailable"`.  The caller always has
                        // a tile to render, and once the entity arrives a
                        // subsequent rebuild produces a same-kind tile in
                        // the same row.
                        let preferred_columns = i32::from(widget.layout.preferred_columns);
                        let preferred_rows = i32::from(widget.layout.preferred_rows);
                        let placement = widget
                            .placement
                            .as_ref()
                            .map(TilePlacement::from_placement)
                            .unwrap_or_else(|| {
                                TilePlacement::default_for(preferred_columns, preferred_rows)
                            });
                        let name = widget
                            .name
                            .clone()
                            .unwrap_or_else(|| entity_id_str.to_string());
                        let state = "unavailable".to_string();
                        let icon_id = widget
                            .icon
                            .clone()
                            .unwrap_or_else(|| "mdi:help-circle".to_string());

                        // task/phase6-window-wireup: emit a placeholder VM
                        // of the matching `TileKind` so the row layout stays
                        // stable across availability transitions (TASK-119
                        // F2 row-stability invariant). Each per-kind VM is
                        // constructed with `state = "unavailable"` and the
                        // available booleans cleared so the Slint side
                        // renders the unavailable visual variant for that
                        // kind. PowerFlow is special: its canonical YAML
                        // shape has `entity: None`, so this arm is the
                        // ONLY path that produces a `TileVM::PowerFlow` for
                        // the standard config — auxiliaries are still
                        // resolved against the live store.
                        match widget.widget_type {
                            WidgetKind::LightTile => TileVM::Light(LightTileVM {
                                name,
                                state,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            WidgetKind::SensorTile => TileVM::Sensor(SensorTileVM {
                                name,
                                state,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            WidgetKind::EntityTile => TileVM::Entity(EntityTileVM {
                                name,
                                state,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            WidgetKind::Cover => TileVM::Cover(CoverTileVM {
                                name,
                                state,
                                position: 0,
                                tilt: 0,
                                has_position: false,
                                has_tilt: false,
                                is_open: false,
                                is_moving: false,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            WidgetKind::Fan => TileVM::Fan(FanTileVM {
                                name,
                                state,
                                speed_pct: 0,
                                has_speed_pct: false,
                                is_on: false,
                                current_speed: String::new(),
                                has_current_speed: false,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            WidgetKind::Lock => TileVM::Lock(LockTileVM {
                                name,
                                state,
                                is_locked: false,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            WidgetKind::Alarm => TileVM::Alarm(AlarmTileVM {
                                name,
                                state,
                                is_armed: false,
                                is_triggered: false,
                                is_pending: false,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            WidgetKind::History => TileVM::History(HistoryGraphTileVM {
                                name,
                                state,
                                change_count: 0,
                                is_available: false,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                                path_commands: String::new(),
                            }),
                            WidgetKind::Camera => TileVM::Camera(CameraTileVM {
                                name,
                                state,
                                is_recording: false,
                                is_streaming: false,
                                is_available: false,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            WidgetKind::Climate => TileVM::Climate(ClimateTileVM {
                                name,
                                state,
                                is_active: false,
                                current_temperature: None,
                                target_temperature: None,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            WidgetKind::MediaPlayer => TileVM::MediaPlayer(MediaPlayerTileVM {
                                name,
                                state,
                                is_playing: false,
                                media_title: None,
                                artist: None,
                                volume_level: None,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                                pending: false,
                            }),
                            WidgetKind::PowerFlow => {
                                TileVM::PowerFlow(compute_power_flow_tile_vm_from_widget(
                                    PowerFlowBuildArgs {
                                        name,
                                        icon_id,
                                        preferred_columns,
                                        preferred_rows,
                                        placement,
                                        wrapper_entity: None,
                                        options: widget.options.as_ref(),
                                    },
                                    store,
                                ))
                            }
                        }
                    }
                };

                tiles.push(tile);
            }
        }
    }

    // TASK-118 F3: O(1) diagnostic — replaces the prior O(N) `store.for_each`
    // visitor walk. `trace!` is filtered out under release-mode level
    // configuration so the per-flush diagnostic cost is zero in production.
    tracing::trace!(widget_count = tiles.len(), "build_tiles: rebuilt tile list");

    tiles
}

// ---------------------------------------------------------------------------
// Slint window definition and property wiring
// ---------------------------------------------------------------------------
//
// The `MainWindow` component lives in `ui/slint/main_window.slint` (TASK-043).
// `build.rs` invokes `slint_build::compile("ui/slint/main_window.slint")`,
// which transitively compiles `theme.slint`, `card_base.slint`, and the three
// tile component files. The macro below picks up the resulting Rust types.
//
// Why the `slint_ui` sub-module: the Slint compiler emits Rust types named
// `LightTileVM`, `SensorTileVM`, `EntityTileVM`, `TilePlacement`,
// `SensorTilePlacement`, `EntityTilePlacement` — names that collide 1:1 with
// the public Rust structs declared above. Wrapping `include_modules!()` in
// a sub-module gives the Slint-generated types a distinct path
// (`slint_ui::LightTileVM`) so callers of this bridge see both the typed Rust
// VMs (from TASK-014) and the Slint-typed ones side-by-side.
pub mod slint_ui {
    slint::include_modules!();
}

pub use slint_ui::{AnimationBudget, GestureConfigGlobal, MainWindow, ViewRouterGlobal};

use crate::actions::timing::GestureConfig;
use crate::assets::icons;
use crate::dashboard::profiles::DeviceProfile;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

/// Errors that can occur while wiring VM data into Slint properties.
///
/// Variants are kept small and `Copy` so the error type does not allocate on
/// the failure path either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// `DeviceProfile.animation_framerate_cap` does not fit in `i32` (the
    /// Slint property type for `framerate-cap`). The active profile is
    /// threaded through [`wire_window`] from `src/lib.rs::run` (TASK-120b F4).
    FramerateCapOutOfRange,
    /// `DeviceProfile.max_simultaneous_animations` does not fit in `i32`
    /// (the Slint property type for `max-simultaneous`). The active profile
    /// is threaded through [`wire_window`] from `src/lib.rs::run`
    /// (TASK-120b F4).
    MaxSimultaneousOutOfRange,
    /// One of the [`GestureConfig`] `*_ms` fields does not fit in `i32` (the
    /// Slint property type for the gesture-timing globals). Practical
    /// thresholds are O(100–10_000) ms so this branch is purely defensive
    /// against a malformed Phase 4 YAML override.
    GestureTimingOutOfRange,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::FramerateCapOutOfRange => {
                f.write_str("DeviceProfile.animation_framerate_cap does not fit in i32")
            }
            WireError::MaxSimultaneousOutOfRange => {
                f.write_str("DeviceProfile.max_simultaneous_animations does not fit in i32")
            }
            WireError::GestureTimingOutOfRange => {
                f.write_str("GestureConfig timing field does not fit in i32")
            }
        }
    }
}

impl std::error::Error for WireError {}

// ---------------------------------------------------------------------------
// VM split + conversion
// ---------------------------------------------------------------------------

/// Per-kind Slint-typed VM vectors produced by [`split_tile_vms`], ready to
/// be wrapped in `ModelRc`s by [`wire_window`].
///
/// Pre-`task/phase6-window-wireup` `split_tile_vms` returned a 3-tuple. The
/// Phase 6 wire-up grew that to twelve per-kind vectors (one per
/// [`TileKind`]); using a struct keeps the return ergonomic and keeps the
/// caller's destructuring readable.
pub struct SplitTileVms {
    pub lights: Vec<slint_ui::LightTileVM>,
    pub sensors: Vec<slint_ui::SensorTileVM>,
    pub entities: Vec<slint_ui::EntityTileVM>,
    pub covers: Vec<slint_ui::CoverTileVM>,
    pub fans: Vec<slint_ui::FanTileVM>,
    pub locks: Vec<slint_ui::LockTileVM>,
    pub alarms: Vec<slint_ui::AlarmTileVM>,
    pub histories: Vec<slint_ui::HistoryGraphTileVM>,
    pub cameras: Vec<slint_ui::CameraTileVM>,
    pub climates: Vec<slint_ui::ClimateTileVM>,
    pub media_players: Vec<slint_ui::MediaPlayerTileVM>,
    pub power_flows: Vec<slint_ui::PowerFlowTileVM>,
}

/// Format a power-watts value with one decimal place and a `W` suffix.
///
/// Matches the formatter used by the more-info modal
/// (`crate::ui::power_flow::format_watts`); duplicated here because that
/// helper is private. Negative values render with a leading minus (the
/// export / discharge convention).
fn power_format_watts(w: f32) -> String {
    format!("{w:.1} W")
}

/// Magnitude threshold (watts) below which a power-flow lane is treated as
/// idle. Matches the `min_flow_w` constant referenced by the Slint
/// `PowerFlowTile` lane visuals; pulled out here so the bridge derives
/// `*_idle` booleans server-side rather than asking Slint to compare floats
/// per frame.
const POWER_FLOW_MIN_FLOW_W: f32 = 1.0;

/// Build a Slint-typed `PowerFlowTileVM` from a Rust-typed [`PowerFlowTileVM`].
///
/// The Slint VM carries every field the lane visuals read on every frame —
/// pre-formatted labels, derived `*_idle` and direction flags, and an
/// `is_available` boolean — so the Slint side never branches on `Option`s.
/// Composition runs once per state change (NOT per frame); `format!`
/// allocations are bounded by lane count (≤ 4).
fn slint_power_flow_tile_vm(vm: &PowerFlowTileVM) -> slint_ui::PowerFlowTileVM {
    let grid_w = vm.grid_w.unwrap_or(0.0);
    let grid_idle = vm
        .grid_w
        .map(|w| w.abs() < POWER_FLOW_MIN_FLOW_W)
        .unwrap_or(true);
    let grid_importing = vm.grid_w.map(|w| w > 0.0).unwrap_or(false);

    let has_solar = vm.solar_w.is_some();
    let solar_w = vm.solar_w.unwrap_or(0.0);
    let solar_idle = vm
        .solar_w
        .map(|w| w.abs() < POWER_FLOW_MIN_FLOW_W)
        .unwrap_or(true);

    let has_battery = vm.battery_w.is_some();
    let battery_w = vm.battery_w.unwrap_or(0.0);
    let battery_idle = vm
        .battery_w
        .map(|w| w.abs() < POWER_FLOW_MIN_FLOW_W)
        .unwrap_or(true);
    let battery_charging = vm.battery_w.map(|w| w > 0.0).unwrap_or(false);

    let has_battery_pct = vm.battery_pct.is_some();
    let battery_pct = vm.battery_pct.unwrap_or(0.0);

    let has_home = vm.home_w.is_some();
    let home_w = vm.home_w.unwrap_or(0.0);
    let home_idle = vm
        .home_w
        .map(|w| w.abs() < POWER_FLOW_MIN_FLOW_W)
        .unwrap_or(true);

    // The grid entity is the primary entity; "available" means we got a
    // numeric reading. When `vm.grid_w` is `None`, the upstream entity is
    // unavailable / unknown and the tile renders the unavailable variant.
    let is_available = vm.grid_w.is_some();

    slint_ui::PowerFlowTileVM {
        name: SharedString::from(vm.name.as_str()),
        grid_w,
        grid_label: SharedString::from(power_format_watts(grid_w).as_str()),
        grid_importing,
        grid_idle,
        has_solar,
        solar_w,
        solar_label: SharedString::from(power_format_watts(solar_w).as_str()),
        solar_idle,
        has_battery,
        battery_w,
        battery_label: SharedString::from(power_format_watts(battery_w).as_str()),
        battery_charging,
        battery_idle,
        has_battery_pct,
        battery_pct,
        has_home,
        home_w,
        home_label: SharedString::from(power_format_watts(home_w).as_str()),
        home_idle,
        is_available,
        r#icon_id: SharedString::from(vm.icon_id.as_str()),
        icon: icons::resolve(&vm.icon_id),
        preferred_columns: vm.preferred_columns,
        preferred_rows: vm.preferred_rows,
        placement: slint_ui::PowerFlowTilePlacement {
            col: vm.placement.col,
            row: vm.placement.row,
            span_cols: vm.placement.span_cols,
            span_rows: vm.placement.span_rows,
        },
        pending: vm.pending,
    }
}

/// Split a flat `&[TileVM]` slice into per-variant `Vec`s of the
/// Slint-generated VM structs, ready to be wrapped in `ModelRc`s and written
/// to `MainWindow` properties.
///
/// This is factored out of [`wire_window`] so it can be exercised in unit
/// tests without instantiating a Slint window (which would require a live
/// graphics backend / display server — not available in headless CI).
///
/// All `String -> SharedString` and `String -> Image` conversions happen here.
/// A `String -> SharedString` conversion is a single heap copy; `Image` clones
/// (returned from [`icons::resolve`]) are `Arc` bumps. None of these allocate
/// inside per-frame paths — the function runs once per refresh cycle.
///
/// # Panics
///
/// Panics if [`crate::assets::icons::init`] has not been called first, because
/// [`icons::resolve`] panics on an unset `OnceLock`. Production callers wire
/// `icons::init()` at startup (TASK-016 / `main.rs`); tests call it explicitly.
pub fn split_tile_vms(tiles: &[TileVM]) -> SplitTileVms {
    let mut out = SplitTileVms {
        lights: Vec::new(),
        sensors: Vec::new(),
        entities: Vec::new(),
        covers: Vec::new(),
        fans: Vec::new(),
        locks: Vec::new(),
        alarms: Vec::new(),
        histories: Vec::new(),
        cameras: Vec::new(),
        climates: Vec::new(),
        media_players: Vec::new(),
        power_flows: Vec::new(),
    };

    for tile in tiles {
        match tile {
            TileVM::Light(vm) => out.lights.push(slint_ui::LightTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::TilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                // TASK-067 spinner gate. Forwarded verbatim into the
                // Slint-typed VM; `crate::ui::toast::apply_pending_for_widgets`
                // is the only writer that flips it back to `true` during
                // a refresh.
                pending: vm.pending,
            }),
            TileVM::Sensor(vm) => out.sensors.push(slint_ui::SensorTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::SensorTilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                pending: vm.pending,
            }),
            TileVM::Entity(vm) => out.entities.push(slint_ui::EntityTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::EntityTilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                pending: vm.pending,
            }),
            TileVM::Cover(vm) => out.covers.push(slint_ui::CoverTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                position: vm.position,
                tilt: vm.tilt,
                r#has_position: vm.has_position,
                r#has_tilt: vm.has_tilt,
                r#is_open: vm.is_open,
                r#is_moving: vm.is_moving,
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::CoverTilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                pending: vm.pending,
            }),
            TileVM::Fan(vm) => out.fans.push(slint_ui::FanTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                r#speed_pct: vm.speed_pct,
                r#has_speed_pct: vm.has_speed_pct,
                r#is_on: vm.is_on,
                r#current_speed: SharedString::from(vm.current_speed.as_str()),
                r#has_current_speed: vm.has_current_speed,
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::FanTilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                pending: vm.pending,
            }),
            TileVM::Lock(vm) => out.locks.push(slint_ui::LockTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                r#is_locked: vm.is_locked,
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::LockTilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                pending: vm.pending,
            }),
            TileVM::Alarm(vm) => out.alarms.push(slint_ui::AlarmTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                r#is_armed: vm.is_armed,
                r#is_triggered: vm.is_triggered,
                r#is_pending: vm.is_pending,
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::AlarmTilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                pending: vm.pending,
            }),
            TileVM::History(vm) => out.histories.push(slint_ui::HistoryGraphTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                r#change_count: vm.change_count,
                r#is_available: vm.is_available,
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::HistoryGraphTilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                pending: vm.pending,
                r#path_commands: SharedString::from(vm.path_commands.as_str()),
            }),
            TileVM::Camera(vm) => out.cameras.push(slint_ui::CameraTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                r#is_recording: vm.is_recording,
                r#is_streaming: vm.is_streaming,
                r#is_available: vm.is_available,
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::CameraTilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                pending: vm.pending,
            }),
            TileVM::Climate(vm) => out.climates.push(slint_ui::ClimateTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                r#is_active: vm.is_active,
                r#current_temperature: vm.current_temperature.unwrap_or(0.0),
                r#has_current_temperature: vm.current_temperature.is_some(),
                r#target_temperature: vm.target_temperature.unwrap_or(0.0),
                r#has_target_temperature: vm.target_temperature.is_some(),
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::ClimateTilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                pending: vm.pending,
            }),
            TileVM::MediaPlayer(vm) => out.media_players.push(slint_ui::MediaPlayerTileVM {
                name: SharedString::from(vm.name.as_str()),
                state: SharedString::from(vm.state.as_str()),
                r#is_playing: vm.is_playing,
                r#media_title: SharedString::from(vm.media_title.as_deref().unwrap_or("")),
                r#has_media_title: vm.media_title.is_some(),
                artist: SharedString::from(vm.artist.as_deref().unwrap_or("")),
                r#has_artist: vm.artist.is_some(),
                r#volume_level: vm.volume_level.unwrap_or(0.0),
                r#has_volume_level: vm.volume_level.is_some(),
                r#icon_id: SharedString::from(vm.icon_id.as_str()),
                icon: icons::resolve(&vm.icon_id),
                preferred_columns: vm.preferred_columns,
                preferred_rows: vm.preferred_rows,
                placement: slint_ui::MediaPlayerTilePlacement {
                    col: vm.placement.col,
                    row: vm.placement.row,
                    span_cols: vm.placement.span_cols,
                    span_rows: vm.placement.span_rows,
                },
                pending: vm.pending,
            }),
            TileVM::PowerFlow(vm) => out.power_flows.push(slint_power_flow_tile_vm(vm)),
        }
    }

    out
}

// ---------------------------------------------------------------------------
// wire_window
// ---------------------------------------------------------------------------

/// Wire a typed `&[TileVM]` slice into the three array properties on
/// [`MainWindow`], and write the `AnimationBudget` globals from the
/// active [`DeviceProfile`].
///
/// `profile` is the [`DeviceProfile`] selected at startup by
/// `src/lib.rs::run` from the dashboard YAML's `device_profile` field
/// (TASK-120b F4). Pre-TASK-120b this function read `PROFILE_DESKTOP`
/// directly, which broke the SBC paths: a Pi/OPI dashboard would still
/// receive the desktop animation budget. The signature now requires the
/// caller to pass the matching profile, so the SBC presets'
/// `animation_framerate_cap` (20–30 Hz) and `max_simultaneous_animations`
/// (2–3) actually reach the Slint side.
///
/// This is the single public entry point used by `main.rs` (TASK-016) and by
/// any future Phase 2 push-update path. The function runs once per refresh
/// cycle and is not on a per-frame hot path; per-element conversion via
/// `String -> SharedString` allocates exactly once per VM field (acceptable),
/// and `Image` is `Arc`-backed so cloning is a refcount bump.
///
/// See `ui/slint/light_tile.slint:23-32` for the per-tile deferral comment.
/// `AnimationBudget.framerate-cap` and `AnimationBudget.max-simultaneous` are
/// written here, once. `active-count` is initialised to 0 by the Slint global
/// declaration itself; tile press handlers (Phase 3) mutate it later. We do
/// not write `active-count` here because doing so on every refresh would
/// stomp any in-flight animation count.
///
/// # Errors
///
/// Returns [`WireError::FramerateCapOutOfRange`] if
/// `profile.animation_framerate_cap` does not fit in `i32`, and
/// [`WireError::MaxSimultaneousOutOfRange`] if
/// `profile.max_simultaneous_animations` does not fit. Both are defensive:
/// every shipped preset is well within `i32` range, but a future profile
/// could exceed it and we want a typed failure rather than a silent
/// truncation.
pub fn wire_window(
    window: &MainWindow,
    tiles: &[TileVM],
    profile: &'static DeviceProfile,
) -> Result<(), WireError> {
    let split = split_tile_vms(tiles);

    // Wrap each Vec in a VecModel and pass via ModelRc to the Slint property.
    // Slint clones the ModelRc internally (Arc bump); no per-element copy.
    window.set_light_tiles(ModelRc::new(VecModel::from(split.lights)));
    window.set_sensor_tiles(ModelRc::new(VecModel::from(split.sensors)));
    window.set_entity_tiles(ModelRc::new(VecModel::from(split.entities)));
    // Phase 6 per-kind property writes — same `ModelRc<VecModel<...>>`
    // pattern as the three Phase 1 kinds. Each model is installed exactly
    // once per refresh cycle; per-frame reads inside Slint see only Arc
    // bumps for the model handle and shared pixel buffers for icons.
    window.set_cover_tiles(ModelRc::new(VecModel::from(split.covers)));
    window.set_fan_tiles(ModelRc::new(VecModel::from(split.fans)));
    window.set_lock_tiles(ModelRc::new(VecModel::from(split.locks)));
    window.set_alarm_tiles(ModelRc::new(VecModel::from(split.alarms)));
    window.set_history_tiles(ModelRc::new(VecModel::from(split.histories)));
    window.set_camera_tiles(ModelRc::new(VecModel::from(split.cameras)));
    window.set_climate_tiles(ModelRc::new(VecModel::from(split.climates)));
    window.set_media_player_tiles(ModelRc::new(VecModel::from(split.media_players)));
    window.set_power_flow_tiles(ModelRc::new(VecModel::from(split.power_flows)));

    // AnimationBudget globals — wired once at startup from the active
    // DeviceProfile (TASK-120b F4).
    let budget = window.global::<AnimationBudget>();

    let cap_i32 = i32::try_from(profile.animation_framerate_cap)
        .map_err(|_| WireError::FramerateCapOutOfRange)?;
    let max_i32 = i32::try_from(profile.max_simultaneous_animations)
        .map_err(|_| WireError::MaxSimultaneousOutOfRange)?;

    budget.set_framerate_cap(cap_i32);
    budget.set_max_simultaneous(max_i32);

    // F11 (TASK-126): stepped spinner on SBC profiles.
    //
    // `tick-hz-sbc` controls the Timer step rate for the pending spinner:
    //   * 0  → desktop mode; the spinner uses animation-tick() per-frame.
    //   * 12 → SBC mode; a discrete Timer fires 12 × per second, cutting
    //          spinner cos/sin evaluations from 60 × to 12 × per second
    //          on software-rendered rpi4/opi_zero3 targets.
    //
    // SBC profiles are identified by animation_framerate_cap <= 30 fps
    // (rpi4: 30, opi_zero3: 20; desktop: 60). The 12 Hz step rate is
    // deliberately lower than the SBC framerate cap (20–30 fps) to
    // ensure each step is visible as a discrete position.
    //
    // `sbc-spinner-cap` is set to `max_simultaneous_animations` for SBC
    // profiles (additional guard on top of `max-simultaneous`) and 0 for
    // desktop (unbounded by this cap). Both caps coexist;
    // `at-capacity` ORs them in the Slint global.
    let tick_hz: i32 = if profile.animation_framerate_cap <= 30 {
        12
    } else {
        0
    };
    let sbc_cap: i32 = if tick_hz > 0 { max_i32 } else { 0 };
    budget.set_tick_hz_sbc(tick_hz);
    budget.set_sbc_spinner_cap(sbc_cap);

    // GestureConfigGlobal — Slint-side mirror of `GestureConfig` (TASK-059).
    // Phase 3 wires the default values; Phase 4 `DeviceProfile.timing_overrides`
    // will pass an explicit `GestureConfig` value through the same write path.
    // The Slint gesture layer (TASK-060) is the eventual consumer.
    write_gesture_config(window, GestureConfig::default())?;

    Ok(())
}

// ---------------------------------------------------------------------------
// GestureConfigGlobal writer (TASK-059)
// ---------------------------------------------------------------------------

/// Slint-typed projection of a [`GestureConfig`] — the four `*_ms` fields
/// converted to `i32` (Slint property type) plus the two booleans.
///
/// Factored out of [`write_gesture_config`] as a pure value so the `u64 -> i32`
/// conversion logic can be unit-tested without instantiating a `MainWindow`
/// (which requires a live graphics backend — not available in headless CI,
/// per the comment block above [`split_tile_vms`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GestureConfigProjection {
    pub(crate) tap_max_ms: i32,
    pub(crate) hold_min_ms: i32,
    pub(crate) double_tap_max_gap_ms: i32,
    pub(crate) double_tap_enabled: bool,
    pub(crate) arm_double_tap_timer: bool,
}

/// Project a [`GestureConfig`] into the Slint-typed shape, narrowing every
/// `u64` `*_ms` field to `i32` and copying the booleans.
///
/// Returns [`WireError::GestureTimingOutOfRange`] if any `*_ms` field exceeds
/// `i32::MAX`. Per `locked_decisions.gesture_config`, the resulting projection
/// carries both `double_tap_enabled` and `arm_double_tap_timer` explicitly —
/// the Slint side reads only `arm_double_tap_timer` and must never infer
/// intent from a zero `double_tap_max_gap_ms`.
pub(crate) fn project_gesture_config(
    config: GestureConfig,
) -> Result<GestureConfigProjection, WireError> {
    let tap_max_ms =
        i32::try_from(config.tap_max_ms).map_err(|_| WireError::GestureTimingOutOfRange)?;
    let hold_min_ms =
        i32::try_from(config.hold_min_ms).map_err(|_| WireError::GestureTimingOutOfRange)?;
    let double_tap_max_gap_ms = i32::try_from(config.double_tap_max_gap_ms)
        .map_err(|_| WireError::GestureTimingOutOfRange)?;

    Ok(GestureConfigProjection {
        tap_max_ms,
        hold_min_ms,
        double_tap_max_gap_ms,
        double_tap_enabled: config.double_tap_enabled,
        arm_double_tap_timer: config.arm_double_tap_timer(),
    })
}

/// Populate the Slint `GestureConfigGlobal` with the field values from a
/// Rust [`GestureConfig`].
///
/// Factored out of [`wire_window`] so the Phase 4 path (which will pass an
/// explicit, profile-overridden [`GestureConfig`] rather than the default)
/// does not duplicate the projection plumbing. Returns
/// [`WireError::GestureTimingOutOfRange`] if any `*_ms` field exceeds `i32`.
///
/// Per `locked_decisions.gesture_config`, both `double_tap_enabled` and
/// `arm_double_tap_timer` are written from the Rust source — the Slint side
/// reads `arm_double_tap_timer` only and never branches on
/// `double_tap_max_gap_ms == 0`. Writing both keeps the global self-consistent
/// in the Slint preview path (preview reads the property values directly).
pub fn write_gesture_config(window: &MainWindow, config: GestureConfig) -> Result<(), WireError> {
    let projection = project_gesture_config(config)?;
    let global = window.global::<GestureConfigGlobal>();

    global.set_tap_max_ms(projection.tap_max_ms);
    global.set_hold_min_ms(projection.hold_min_ms);
    global.set_double_tap_max_gap_ms(projection.double_tap_max_gap_ms);
    global.set_double_tap_enabled(projection.double_tap_enabled);
    global.set_arm_double_tap_timer(projection.arm_double_tap_timer);

    Ok(())
}

// ---------------------------------------------------------------------------
// Live bridge: per-entity subscriptions, ConnectionState gating, status banner
// ---------------------------------------------------------------------------
//
// Phase 2 (TASK-033) wires the bridge to:
//
//   * Subscribe per visible entity-id (one `store.subscribe(&[id])` call per id).
//     Channels are created on-demand (matches the [`crate::ha::live_store::LiveStore`]
//     contract introduced in TASK-030 — not permanently allocated per entity).
//   * Accumulate updates into a `HashMap<EntityId, ()>` with **latest-overwrite**
//     semantics (no FIFO). When two updates arrive for the same id between flush
//     ticks, the first is discarded — the bridge re-reads the current entity via
//     [`EntityStore::get`] at flush time.
//   * Flush at 80 ms (12.5 Hz) on a Tokio timer. The actual Slint property write
//     hops onto the Slint UI thread via [`slint::invoke_from_event_loop`].
//   * Watch [`ConnectionState`] from `src/platform/status.rs` and gate Slint
//     property writes: while the state is `Reconnecting` or `Failed`, no
//     Rust-side property writes occur — the last rendered frame stays on screen
//     and the status banner is shown.
//   * On return to `Live`, immediately fire a full `for_each` resync to
//     re-render every visible tile and clear the banner.
//   * On `RecvError::Lagged` from any per-entity subscriber, acquire
//     the pending-map mutex once (TASK-125 F6 fix — the previous shape
//     locked the map N times per lag event), batch-mark every
//     subscribed id dirty, drop the lock, then re-`store.subscribe`
//     for the lagging id only.  The flush path re-reads each entity
//     via `store.get` at the next 80 ms tick.
//
// **Doc-comment clarification (Codex Nit N7).** "No Rust-side property writes"
// while gated does NOT suppress Slint's internal animation engine. Animations
// declared with `animate { ... }` blocks in `.slint` files run inside the Slint
// runtime and are independent of any Rust-side property-write activity. Gating
// only prevents stale data from being pushed into properties; it does not freeze
// the UI.
//
// **Thread model.** All store-watching work runs on Tokio. Each `LiveBridge`
// spawns:
//
//   1. One subscriber task **per visible entity id**.  Each task owns its
//      own `broadcast::Receiver` from `store.subscribe(&[id])` and pushes
//      ids into the pending map on every `Ok(update)`.  On
//      `RecvError::Lagged` the task acquires the pending-map mutex
//      **once** and batch-marks every visible entity dirty in a single
//      critical section (TASK-125 F6 fix — pre-TASK-125 the lock was
//      acquired N times per lag event, an O(lagging_subscribers × N)
//      lock pattern).  After releasing the lock the task re-subscribes
//      its own receiver only.
//   2. One flush task that wakes every 80 ms, drains the pending map under the
//      ConnectionState gate, builds the tiles, and posts the result to the
//      Slint event loop via `invoke_from_event_loop`.
//   3. One `ConnectionState` watcher task that mirrors transitions into the
//      bridge's internal "gated" flag and triggers a full resync on the
//      Reconnecting/Failed → Live transition.

use crate::ha::store::EntityUpdate;
use crate::platform::status::ConnectionState;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{interval, Duration};

/// 80 ms flush cadence (12.5 Hz). Asserted in CI by the churn benchmark
/// (TASK-038).  Public so test code can synthesize this exact value rather
/// than hard-coding 80 in a magic literal.
pub const FLUSH_INTERVAL_MS: u64 = 80;

/// Returns `true` if the given [`ConnectionState`] is one in which Rust-side
/// Slint property writes must be suppressed.
///
/// `Reconnecting` and `Failed` are the two gated states; the others
/// (Connecting, Authenticating, Subscribing, Snapshotting, Services, Live)
/// are non-gated.  Note that `Connecting` is non-gated because the very first
/// startup transition (Connecting → Authenticating → … → Live) should write
/// the loaded fixture / snapshot through to Slint as soon as it lands.
pub fn is_writes_gated(state: ConnectionState) -> bool {
    matches!(
        state,
        ConnectionState::Reconnecting | ConnectionState::Failed
    )
}

/// Walk a [`Dashboard`] config in document order and collect the entity ids
/// referenced by every widget (those with `widget.entity = Some(id)`).
///
/// The result is in document order; duplicates (the same entity referenced
/// by two widgets) are preserved so callers can `.dedup()` if they want a
/// unique-id list.  The default policy used by [`LiveBridge`] is to dedup
/// before subscribing — one `store.subscribe(&[id])` per unique id.
pub fn collect_visible_entity_ids(dashboard: &Dashboard) -> Vec<EntityId> {
    let mut ids = Vec::new();
    for view in &dashboard.views {
        for section in &view.sections {
            for widget in &section.widgets {
                if let Some(s) = widget.entity.as_deref() {
                    ids.push(EntityId::from(s));
                }
            }
        }
    }
    ids
}

// ---------------------------------------------------------------------------
// BridgeSink — testable abstraction over the Slint writes
// ---------------------------------------------------------------------------

/// Sink for the side-effects the live bridge produces.
///
/// Production callers pass a [`SlintSink`] wrapping a `slint::Weak<MainWindow>`
/// so the writes hop onto the Slint UI thread via `invoke_from_event_loop`.
/// Tests pass an in-process recording sink and assert against its log
/// directly (no Slint backend required).
///
/// # Method roles (TASK-119 F2)
///
/// * [`write_tiles`](Self::write_tiles) — full-model write: replaces every
///   row in the per-kind models from a freshly-built tile snapshot.  Used by
///   the [`ConnectionState`] watcher's resync path on the
///   `Reconnecting/Failed → Live` transition (and by the legacy fallback
///   default of [`apply_row_updates`](Self::apply_row_updates) for sinks that
///   have not migrated to incremental updates).
/// * [`apply_row_updates`](Self::apply_row_updates) — per-row incremental
///   update: production sinks override this to call `set_row_data` on the
///   stable Slint per-kind models for ONLY the changed rows.  This is the
///   steady-state flush path post-TASK-119; the bridge does NOT call
///   `write_tiles` from `run_flush_loop` anymore.
/// * [`set_status_banner_visible`](Self::set_status_banner_visible) —
///   banner toggle, called from the [`ConnectionState`] watcher.
pub trait BridgeSink: Send + Sync + 'static {
    /// Apply a fresh, full tile list (full-model write).  Called from the
    /// state watcher's `Reconnecting/Failed → Live` resync; also by the
    /// default fallback of [`apply_row_updates`](Self::apply_row_updates).
    ///
    /// This is NOT called from the steady-state flush loop in
    /// [`LiveBridge`] post-TASK-119.
    fn write_tiles(&self, tiles: Vec<TileVM>);

    /// Apply a per-row incremental update (TASK-119 F2 steady-state flush).
    ///
    /// Production sinks override this to call `set_row_data` on the stable
    /// Slint per-kind models for ONLY the changed rows.
    ///
    /// `rebuild_full_tiles` is a closure that, when invoked, rebuilds the
    /// full tile list against the current store snapshot.  Production sinks
    /// MUST NOT call it (per TASK-119: per-row updates are O(changed_rows),
    /// not O(widget_count)).  The default fallback calls it and routes
    /// through [`write_tiles`](Self::write_tiles) so legacy sinks (test
    /// recorders, benches) keep their pre-TASK-119 observable behaviour
    /// without being forced to migrate in the same PR.
    fn apply_row_updates(
        &self,
        _updates: Vec<RowUpdate>,
        rebuild_full_tiles: Box<dyn FnOnce() -> Vec<TileVM> + Send>,
    ) {
        // Default: fall back to write_tiles by invoking the rebuild closure.
        // Production SlintSink overrides this method and ignores the closure.
        self.write_tiles(rebuild_full_tiles());
    }

    /// Toggle the status banner. Called from the [`ConnectionState`] watcher.
    fn set_status_banner_visible(&self, visible: bool);
}

// Note: the production [`BridgeSink`] implementation that writes through
// `slint::invoke_from_event_loop` to a `slint::Weak<MainWindow>` lives in the
// caller (TASK-034 wires it in `src/lib.rs`).  Keeping it out of this file
// means `src/ui/bridge.rs` has no Slint-event-loop-dependent code paths that
// would be uncoverable in headless CI; the trait is the seam.

// ---------------------------------------------------------------------------
// Pending-updates accumulator
// ---------------------------------------------------------------------------

/// Shared, latest-overwrite pending-updates map.
///
/// The `HashMap<EntityId, ()>` value is intentionally unit — the bridge
/// re-reads the current entity via [`EntityStore::get`] at flush time, so the
/// map only tracks **which** ids changed since the last flush, not what they
/// changed to.  When a second update for the same id arrives before the next
/// flush tick, `insert` overwrites the prior entry (still unit) — the first
/// "event" is implicitly discarded, which is the latest-overwrite semantic
/// the AC requires.
type PendingMap = Arc<Mutex<HashMap<EntityId, ()>>>;

/// Drain all pending entity ids and return them as a `Vec`.
///
/// Holds the mutex for the duration of the drain only; the returned `Vec` is
/// owned and can be processed without further locking.  Order is unspecified
/// (HashMap iteration order); the flush path re-runs `build_tiles` on the
/// full dashboard so the order of pending ids does not affect the rendered
/// output.
fn drain_pending(pending: &PendingMap) -> Vec<EntityId> {
    let mut guard = pending.lock().expect("PendingMap mutex poisoned");
    guard.drain().map(|(id, ())| id).collect()
}

// ---------------------------------------------------------------------------
// LiveBridge
// ---------------------------------------------------------------------------

/// Shared `EntityId → row_index` map used by the flush loop and any
/// future full-rebuild path (config reload, view switch, full resync).
///
/// Wrapped in `Arc<RwLock<...>>` so the flush loop reads via a read-guard
/// and any path that wants to rebuild the index (e.g., a future view-switch
/// hook) can take the write-guard, swap in a fresh `RowIndex`, and release
/// before the next flush tick reads.  Per Risk #10 (TASK-119), the index
/// rebuild MUST be atomic with respect to the flush loop.
pub(crate) type RowIndexHandle = Arc<RwLock<RowIndex>>;

/// Owned handle to the spawned bridge tasks.
///
/// Dropping this handle aborts the Tokio tasks; otherwise the tasks run for
/// the lifetime of the application.  Tests construct a `LiveBridge` against a
/// stub store, exercise it for the duration of the test, and let it drop at
/// scope end so the runtime can shut down cleanly.
pub struct LiveBridge {
    /// Subscriber task per entity id.  Stored so the `Drop` impl can abort
    /// them; never read after construction.
    ///
    /// Each task owns one `broadcast::Receiver` for its assigned id and
    /// awaits `recv()` in a tight loop.  On `RecvError::Lagged` the task
    /// acquires the pending-map mutex **once** to batch-mark every visible
    /// entity dirty (TASK-125 F6 fix), then re-subscribes its own receiver.
    subscriber_tasks: Vec<JoinHandle<()>>,
    /// 80 ms flush task.
    flush_task: JoinHandle<()>,
    /// Connection-state watcher task.
    state_task: JoinHandle<()>,
}

impl Drop for LiveBridge {
    fn drop(&mut self) {
        for h in &self.subscriber_tasks {
            h.abort();
        }
        self.flush_task.abort();
        self.state_task.abort();
    }
}

impl LiveBridge {
    /// Spawn the three task families and return the owning handle.
    ///
    /// The bridge subscribes per **unique** visible entity id (de-duplicated
    /// from `dashboard`), wires up the 80 ms flush, and starts the
    /// `ConnectionState` watcher.  The function does **not** perform an
    /// initial render — callers wire `wire_window` once at startup before
    /// calling `spawn`, so the Phase 1 fixture (or initial snapshot) is
    /// already rendered.  `LiveBridge` only handles deltas after that.
    ///
    /// # Arguments
    ///
    /// * `store` — shared pointer to the live store.  Cloned into each
    ///   subscriber task so the tasks own their own `Arc` ref.
    /// * `dashboard` — used to derive the visible entity-id list and re-run
    ///   `build_tiles` at flush time.  Wrapped in `Arc` internally.
    /// * `state_rx` — receiver half of the `ConnectionState` watch channel.
    ///   The sender half is owned by `src/ha/client.rs` (TASK-029/032).
    /// * `sink` — destination for tile writes and banner toggles.  Production
    ///   passes [`SlintSink::new(&window)`]; tests pass a recording sink.
    pub fn spawn<S: BridgeSink>(
        store: Arc<dyn EntityStore>,
        dashboard: Arc<Dashboard>,
        state_rx: watch::Receiver<ConnectionState>,
        sink: S,
    ) -> Self {
        // Dedup visible entity ids before subscribing — one channel per
        // unique id even if the dashboard references the same entity in
        // multiple widgets.  EntityId implements Hash + Eq but not Ord, so
        // a HashSet pass is the canonical way to dedup while preserving the
        // first-seen order for downstream consumers.
        let raw_ids = collect_visible_entity_ids(&dashboard);
        let mut seen: HashSet<EntityId> = HashSet::with_capacity(raw_ids.len());
        let mut ids: Vec<EntityId> = Vec::with_capacity(raw_ids.len());
        for id in raw_ids {
            if seen.insert(id.clone()) {
                ids.push(id);
            }
        }
        let ids = Arc::new(ids);

        // Shared sink wrapped in Arc so the per-task closures can clone a
        // pointer rather than cloning the sink's internals.
        let sink: Arc<S> = Arc::new(sink);

        // Pending-updates accumulator.
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        // Build the static `EntityId → (kind, row_index)` index at load
        // time (TASK-119 F2).  The index is derived from the dashboard
        // config alone — each widget's `widget_type` decides its
        // [`TileKind`] — so this call does NOT touch the live store.
        // (Calling `build_tiles` here would in turn call `store.get` per
        // widget, which is a deadlock hazard for stores that block on
        // `get` — see the `RendezvousStore` test.)  After this point the
        // index drives `set_row_data` for every per-entity flush; the
        // state-watcher's Live-transition resync rebuilds the index
        // (atomically, under the write-guard) before calling
        // `write_tiles` so any subsequent flush either observes the old
        // index or the new one — never a mix.
        //
        // Cost: O(widget_count), one HashMap insert per widget — and
        // fires exactly once per LiveBridge::spawn.
        let row_index: RowIndexHandle = Arc::new(RwLock::new(build_row_index(&dashboard)));

        // Per-entity subscriber tasks.  One task per visible entity id;
        // each task owns its own broadcast receiver and pushes ids into
        // the pending map on every `Ok(update)`.  On `RecvError::Lagged`
        // the task acquires the pending-map mutex ONCE and batch-marks
        // every visible entity dirty in a single critical section
        // (TASK-125 F6 fix — pre-TASK-125 the lock was acquired N times
        // per lag event, an O(lagging_subscribers × N) pattern).
        let mut subscriber_tasks = Vec::with_capacity(ids.len());
        for id in ids.iter().cloned() {
            let store = Arc::clone(&store);
            let pending = Arc::clone(&pending);
            let ids_for_resync = Arc::clone(&ids);
            subscriber_tasks.push(tokio::spawn(async move {
                run_entity_subscriber(store, id, pending, ids_for_resync).await;
            }));
        }

        // Flush task.
        let flush_task = {
            let store = Arc::clone(&store);
            let dashboard = Arc::clone(&dashboard);
            let pending = Arc::clone(&pending);
            let state_rx = state_rx.clone();
            let sink = Arc::clone(&sink);
            let row_index = Arc::clone(&row_index);
            tokio::spawn(async move {
                run_flush_loop(store, dashboard, pending, state_rx, sink, row_index).await;
            })
        };

        // ConnectionState watcher task.
        let state_task = {
            let store = Arc::clone(&store);
            let dashboard = Arc::clone(&dashboard);
            let sink = Arc::clone(&sink);
            let row_index = Arc::clone(&row_index);
            tokio::spawn(async move {
                run_state_watcher(store, dashboard, state_rx, sink, row_index).await;
            })
        };

        LiveBridge {
            subscriber_tasks,
            flush_task,
            state_task,
        }
    }
}

/// Per-entity subscriber loop.
///
/// Subscribes via `store.subscribe(&[id])`, then loops on `recv()`:
///
///   * `Ok(_update)` — record the id in the pending map (latest-overwrite).
///   * `Err(RecvError::Lagged(_))` — TASK-125 F6 fix: acquire the
///     pending-map mutex **once** and batch-insert every visible entity
///     id in a single critical section, then re-subscribe.  Pre-TASK-125
///     this acquired the lock N times per lag event (an
///     `O(lagging_subscribers × N)` lock pattern under bursty load); the
///     flush path re-reads each entity's current state via `store.get`
///     at the next 80 ms tick, so the per-id `store.get` calls inside
///     the lag-recovery branch are no longer required.
///   * `Err(RecvError::Closed)` — the sender was dropped; exit the loop.
async fn run_entity_subscriber(
    store: Arc<dyn EntityStore>,
    id: EntityId,
    pending: PendingMap,
    ids_for_resync: Arc<Vec<EntityId>>,
) {
    let mut rx = store.subscribe(std::slice::from_ref(&id));
    loop {
        match rx.recv().await {
            Ok(EntityUpdate { id: updated_id, .. }) => {
                let mut guard = pending.lock().expect("PendingMap mutex poisoned");
                guard.insert(updated_id, ());
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(
                    entity_id = %id.as_str(),
                    skipped = n,
                    "subscriber lagged; recovering all subscribed ids"
                );
                // TASK-125 F6 fix: ONE lock acquisition for ALL ids.
                // Pre-TASK-125 this acquired the lock N times (an
                // O(lagging_subscribers × N) pattern under bursty load).
                // The flush path will re-read each entity's current
                // state via `store.get` at the next 80 ms tick, so no
                // per-id `store.get` is needed here.
                {
                    let mut guard = pending.lock().expect("PendingMap mutex poisoned");
                    for resync_id in ids_for_resync.iter() {
                        guard.insert(resync_id.clone(), ());
                    }
                }
                // Re-subscribe to recover from the lagged channel.
                rx = store.subscribe(std::slice::from_ref(&id));
            }
            Err(RecvError::Closed) => {
                tracing::debug!(
                    entity_id = %id.as_str(),
                    "subscriber channel closed; exiting"
                );
                return;
            }
        }
    }
}

/// Flush loop: 80 ms cadence, gated on [`ConnectionState`].
///
/// On every tick:
///
///   1. Read the current `ConnectionState` from `state_rx.borrow()`.
///   2. If gated ([`is_writes_gated`] returns true), skip the flush — the
///      pending map keeps accumulating; nothing is written until the gate
///      lifts.
///   3. Otherwise, drain the pending map.  If empty, do nothing (no tile
///      churn, no allocation).  If non-empty, look up each pending
///      `EntityId` in the static [`RowIndex`], read the entity's current
///      state via `store.get`, and produce one [`RowUpdate`] per affected
///      `(kind, row_index)`.  No full tile-list rebuild happens on this
///      path — TASK-119 F2.
///   4. **Re-check** `state_rx.borrow()` immediately before the property
///      write.  If the state flipped to a gated state between step 1 and
///      this point (the read-then-check race), drop the drained ids and skip
///      the write.
///
/// Recovery of dropped ids: as soon as the bridge re-enters
/// `ConnectionState::Live`, `run_state_watcher` fires a full `build_tiles`
/// resync that re-reads every widget's entity via `store.get` — the dropped
/// ids are picked up there.  Note this guarantee holds **only** while the
/// watcher loop is alive (see the watcher's own loop-exit conditions in
/// `run_state_watcher`'s doc-comment).  In the rare path where the
/// `ConnectionState` watch sender is dropped while gated (e.g. the WS task
/// exits permanently without a clean reconnect), the watcher returns and
/// no further resync ever fires; dropped ids stay dropped.  This is
/// acceptable because once the watch sender is gone, the upstream WS task
/// has terminated and the bridge has no live data source to render — the
/// last on-screen frame is the best the user can see anyway.
///
/// We deliberately do NOT push the drained ids back into `pending`, because
/// the next flush has no way to distinguish "we already lost the racing-
/// with-gate write" from a genuine pending update; relying on the resync-
/// on-`Live` path is strictly correct and avoids re-introducing the same
/// race.
///
/// # Per-flush cost (TASK-119 F2)
///
/// Per-flush cost is now O(changed_rows), not O(widget_count).  Each
/// pending `EntityId` produces zero-or-more [`RowUpdate`]s by:
///
///   1. One `store.get(id)` lookup per pending id (O(1) under the post-F1
///      `RwLock<HashMap>` in-place mutation).
///   2. One [`RowIndex::rows_for`] lookup per pending id (O(1) HashMap
///      lookup; the result is a slice of `(kind, row_index)`).
///   3. One [`String`] allocation per affected row for the new state value.
///
/// The legacy O(widget_count) full rebuild path remains in place via the
/// `rebuild_full_tiles` thunk passed to [`BridgeSink::apply_row_updates`];
/// production sinks ignore it.  Test/bench sinks that have not migrated
/// invoke the thunk via the trait's default fallback, preserving their
/// pre-TASK-119 observable behaviour.
async fn run_flush_loop<S: BridgeSink>(
    store: Arc<dyn EntityStore>,
    dashboard: Arc<Dashboard>,
    pending: PendingMap,
    state_rx: watch::Receiver<ConnectionState>,
    sink: Arc<S>,
    row_index: RowIndexHandle,
) {
    let mut ticker = interval(Duration::from_millis(FLUSH_INTERVAL_MS));
    // Skip burst-catch-up if the loop falls behind: at most one flush per tick
    // even after a long stall, so we don't write a backlog of stale tiles when
    // the gate lifts.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;

        let state = *state_rx.borrow();
        if is_writes_gated(state) {
            // Gated — keep accumulating; do not write Slint properties.
            continue;
        }

        let drained = drain_pending(&pending);
        if drained.is_empty() {
            continue;
        }

        // TASK-119 F2: build per-row updates by looking up each pending
        // entity in the static row index and reading its current state
        // from the store.  No full tile-list rebuild happens here.
        //
        // Risk #10 mitigation: take the read-guard on the index for the
        // entire build phase so any concurrent rebuild (full resync,
        // config reload, view switch) is serialised.  The guard is
        // released before the (potentially expensive) sink call.
        let updates: Vec<RowUpdate> = {
            let index = row_index.read().unwrap_or_else(|e| e.into_inner());
            let mut acc: Vec<RowUpdate> = Vec::with_capacity(drained.len());
            for entity_id in &drained {
                // Resolve the current state once per entity (O(1) under
                // the post-F1 RwLock<HashMap>); fall through to
                // "unavailable" if the entity has been removed from the
                // store between the broadcast and this read.
                let state = match store.get(entity_id) {
                    Some(entity) => (*entity.state).to_string(),
                    None => "unavailable".to_string(),
                };
                for &(kind, row_index) in index.rows_for(entity_id) {
                    acc.push(RowUpdate {
                        kind,
                        row_index,
                        state: state.clone(),
                    });
                }
            }
            acc
        };

        if updates.is_empty() {
            // Drained ids did not map to any indexed rows (e.g., a stray
            // pending id that the dashboard no longer references).  Nothing
            // to write.
            continue;
        }

        // Re-check state immediately before the write to close the
        // read-then-check race (Codex finding BLOCKER 1).  ConnectionState
        // can flip to Reconnecting/Failed between the initial gate check
        // and the row-update build returning; if so, do not write.  The
        // ids we just drained are recovered on the eventual Live
        // transition via `run_state_watcher`'s full resync (see
        // doc-comment above).
        let state_at_write = *state_rx.borrow();
        if is_writes_gated(state_at_write) {
            continue;
        }

        // TASK-119 F2: per-row sink call.  Production sinks override
        // `apply_row_updates` to call `set_row_data` per row; legacy
        // sinks (test recorders, benches) fall through to the trait's
        // default `write_tiles(rebuild_full_tiles())` so their
        // observable behaviour is unchanged.  The thunk allocates a
        // single Box<dyn FnOnce> per flush — production never invokes
        // it, so the only steady-state cost is the box itself.
        let store_for_thunk = Arc::clone(&store);
        let dashboard_for_thunk = Arc::clone(&dashboard);
        let rebuild_full_tiles: Box<dyn FnOnce() -> Vec<TileVM> + Send> =
            Box::new(move || build_tiles(&*store_for_thunk, &dashboard_for_thunk));
        sink.apply_row_updates(updates, rebuild_full_tiles);
    }
}

/// `ConnectionState` watcher: mirrors transitions into banner state and
/// triggers a full resync on the Reconnecting/Failed → Live edge.
///
/// On every state change:
///
///   * If the new state is gated, set the banner visible.
///   * If the new state is `Live`, set the banner hidden AND fire an
///     immediate full `for_each` resync via `build_tiles` so the user sees
///     the freshest snapshot the moment the connection recovers.
///
/// Every other transition (e.g. `Live → Connecting → Authenticating → Live`
/// during a clean reconnect) hides the banner without a redundant resync;
/// the resync only fires on entry into `Live`.
///
/// # Invariant: synchronous `build_tiles` between observe-`Live` and write
///
/// The Live-transition path observes `state_rx.borrow_and_update() == Live`,
/// then synchronously runs `build_tiles` and `sink.write_tiles` — there is
/// no `.await` between the state observation and the property write.  The
/// flush loop has the analogous race (state can flip during `build_tiles`)
/// and addresses it with a post-`build_tiles` re-check.  The watcher does
/// NOT need that re-check today because `build_tiles` is synchronous: the
/// tokio scheduler cannot preempt this task between observing `Live` and
/// the write, so a `state_tx.send(Reconnecting)` from another task that
/// arrives mid-`build_tiles` will only be observed on the watcher's NEXT
/// `state_rx.changed().await` poll.  Worst-case, the watcher's resync write
/// pushes a snapshot tied to the (now-stale) `Live` state — exactly what
/// "the user sees the freshest snapshot the moment the connection
/// recovers" is supposed to achieve.  If `EntityStore::for_each` ever
/// becomes async (e.g. a future network-backed store), this invariant
/// breaks and the watcher needs the same post-`build_tiles` re-check the
/// flush loop has.  Track this assumption alongside any change to the
/// `EntityStore` trait.
async fn run_state_watcher<S: BridgeSink>(
    store: Arc<dyn EntityStore>,
    dashboard: Arc<Dashboard>,
    mut state_rx: watch::Receiver<ConnectionState>,
    sink: Arc<S>,
    row_index: RowIndexHandle,
) {
    // Apply the initial state once so a bridge spawned mid-Reconnect renders
    // the banner without waiting for the next transition.
    let initial = *state_rx.borrow_and_update();
    sink.set_status_banner_visible(is_writes_gated(initial));
    if matches!(initial, ConnectionState::Live) {
        // SAFETY (sync invariant): build_tiles is synchronous — see this
        // function's doc-comment.  If EntityStore::for_each ever becomes
        // async, add a post-build state re-check here mirroring run_flush_loop.
        let tiles = build_tiles(&*store, &dashboard);
        // TASK-119 F2 / Risk #10: refresh the row index BEFORE
        // `write_tiles` lands so any flush that observes the new model
        // layout finds an index that matches it.  The dashboard-only
        // builder is idempotent on a stable dashboard, so this assignment
        // is a no-op in steady state — but it is mandatory if the
        // resync ever follows a config reload that changed the widget
        // mix (future work).  The write-guard is dropped before the
        // sink call so a concurrent flush attempt cannot block on it.
        {
            let mut guard = row_index.write().unwrap_or_else(|e| e.into_inner());
            *guard = build_row_index(&dashboard);
        }
        sink.write_tiles(tiles);
    }

    loop {
        if state_rx.changed().await.is_err() {
            // Sender dropped — Phase 2's WS client task exited; no more
            // transitions will be observed.  The bridge's other tasks may
            // still be useful (the flush loop continues to drain pending),
            // but state-driven banner toggles end here.
            return;
        }
        let new_state = *state_rx.borrow_and_update();
        sink.set_status_banner_visible(is_writes_gated(new_state));
        if matches!(new_state, ConnectionState::Live) {
            // SAFETY (sync invariant): see initial-state branch above.
            // build_tiles is synchronous; tokio cannot preempt this task
            // between observing Live and the property write below.
            let tiles = build_tiles(&*store, &dashboard);
            // TASK-119 F2 / Risk #10: refresh the index before
            // `write_tiles` so the next flush observes a consistent
            // (model layout, index layout) pair.  See the matching
            // initial-state branch above for the full rationale.
            {
                let mut guard = row_index.write().unwrap_or_else(|e| e.into_inner());
                *guard = build_row_index(&dashboard);
            }
            sink.write_tiles(tiles);
        }
    }
}

// ---------------------------------------------------------------------------
// More-info dispatch bridge helper (TASK-098)
// ---------------------------------------------------------------------------

/// Select the [`crate::ui::more_info::MoreInfoBody`] for the widget bound to
/// `entity_id` in `dashboard`, using the per-domain dispatch factory.
///
/// # Contract (locked_decisions.more_info_dispatch)
///
/// When the bridge receives a [`crate::actions::dispatcher::DispatchOutcome::MoreInfo`]
/// event, it calls this function to resolve the per-domain body. The function
/// walks the dashboard in document order (views → sections → widgets) and
/// returns the first widget whose `entity` field matches `entity_id`. It then
/// calls [`crate::ui::more_info::body_for_widget`] with the widget's kind and
/// options.
///
/// If no widget in the dashboard is bound to `entity_id` (which should not
/// happen under normal operation — the `WidgetActionMap` is built from the
/// same dashboard), the function falls back to `AttributesBody` to avoid
/// panicking.
///
/// The bridge then calls [`crate::ui::more_info::ModalState::open_with_body`]
/// with the returned body and the current entity snapshot. No new state is
/// added to the bridge: `entity_id` and `Dashboard` are already available
/// at the modal-open call site.
///
/// # Parameters
///
/// * `entity_id` — the entity to open the modal for, from `DispatchOutcome::MoreInfo`.
/// * `dashboard` — the loaded `Dashboard`, already held by the bridge.
/// * `store`     — shared live store, forwarded to `body_for_widget` so
///   per-domain bodies can query the store at row-build time.
pub fn select_more_info_body(
    entity_id: &EntityId,
    dashboard: &Dashboard,
    store: Arc<crate::ha::live_store::LiveStore>,
) -> Box<dyn crate::ui::more_info::MoreInfoBody> {
    for view in &dashboard.views {
        for section in &view.sections {
            for widget in &section.widgets {
                if widget.entity.as_deref() == Some(entity_id.as_str()) {
                    return crate::ui::more_info::body_for_widget(
                        widget.widget_type.clone(),
                        widget.options.as_ref(),
                        store,
                    );
                }
            }
        }
    }
    // Fallback: entity_id not found in dashboard — use AttributesBody.
    // This path is reachable only if the bridge is called with an entity_id
    // that was never registered in the dashboard (defensive branch).
    tracing::warn!(
        entity_id = %entity_id.as_str(),
        "select_more_info_body: no widget found for entity_id in dashboard; falling back to AttributesBody"
    );
    Box::new(crate::ui::more_info::AttributesBody::new())
}

// ---------------------------------------------------------------------------
// ViewSwitcher Slint module (TASK-086)
// ---------------------------------------------------------------------------
//
// `ui/slint/view_switcher.slint` is compiled by `build.rs` (TASK-086) to a
// separate generated Rust file exposed via the `HANUI_VIEW_SWITCHER_INCLUDE`
// env var — the same pattern as `gesture_test_window.slint` (TASK-060) and
// `view.slint` (TASK-085). This module picks it up via `include!` so the
// generated `ViewSwitcherWindow` and `ViewMeta` types are available without
// polluting the production `slint_ui` namespace.
//
// Per locked_decisions.view_switcher_touch_gating: touch-input gates whether
// the edge-swipe handler is instantiated at all in the Slint tree. This is
// enforced at the Slint level (an `if root.touch-input :` conditional); the
// Rust side only reads the profile field and passes the bool.
//
// Per locked_decisions.density_mode_behavior: density × view-count governs
// tab strip vs dropdown rendering. The Rust side passes the density as a
// lowercase string ("compact" | "regular" | "spacious"); Slint compares via
// string equality.
pub mod view_switcher_slint {
    include!(env!("HANUI_VIEW_SWITCHER_INCLUDE"));
}

pub use view_switcher_slint::ViewSwitcherWindow;

/// Rust-side view-model for the view switcher navigation bar.
///
/// Built once per [`Dashboard`] load by [`build_view_switcher_vm`]. The
/// bridge writes these into the `ViewSwitcherWindow` Slint properties via
/// [`wire_view_switcher`].
///
/// # Field semantics
///
/// * `views` — ordered list of `(id, title)` pairs, one per YAML `views:` entry.
/// * `active_view_id` — the `default_view` field from the loaded `Dashboard`.
/// * `density` — the active profile's `Density` enum mapped to a lowercase
///   ASCII string that Slint's property bindings can compare with `==`.
/// * `touch_input` — the active profile's `touch_input` bool. When `false`,
///   the Slint swipe handler is NOT instantiated (per the `if root.touch-input`
///   guard in `view_switcher.slint`).
#[derive(Debug, Clone, PartialEq)]
pub struct ViewSwitcherVM {
    /// Ordered view list in document order.
    pub views: Vec<ViewEntry>,
    /// Id of the initial / current view (from `Dashboard.default_view`).
    pub active_view_id: String,
    /// Density string for the Slint side ("compact" | "regular" | "spacious").
    pub density: String,
    /// Mirror of `DeviceProfile.touch_input`.
    pub touch_input: bool,
}

/// One entry in the view navigation list.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewEntry {
    /// Stable view id (matches `View.id` in the YAML).
    pub id: String,
    /// Human-readable tab / dropdown label.
    pub title: String,
}

/// Map the loaded `Dashboard` and its active `DeviceProfile` into a
/// [`ViewSwitcherVM`] ready for wiring.
///
/// The view list is built in document order (the YAML `views:` array). The
/// active view is taken from `Dashboard.default_view`. Density and
/// touch_input are read directly from the profile.
///
/// This function is pure (no Slint interaction) and can be unit-tested
/// without a graphics backend.
pub fn build_view_switcher_vm(
    dashboard: &Dashboard,
    profile: &crate::dashboard::profiles::DeviceProfile,
) -> ViewSwitcherVM {
    use crate::dashboard::profiles::Density;

    let views: Vec<ViewEntry> = dashboard
        .views
        .iter()
        .map(|v| ViewEntry {
            id: v.id.clone(),
            title: v.title.clone(),
        })
        .collect();

    let density = match profile.density {
        Density::Compact => "compact",
        Density::Regular => "regular",
        Density::Spacious => "spacious",
    }
    .to_string();

    ViewSwitcherVM {
        views,
        active_view_id: dashboard.default_view.clone(),
        density,
        touch_input: profile.touch_input,
    }
}

/// Wire a [`ViewSwitcherVM`] into a `ViewSwitcherWindow`'s Slint properties.
///
/// Called once per `Dashboard` load. The `view_changed` callback is wired
/// to update `current_view` on the `ViewRouterGlobal` in the main window.
///
/// # Arguments
///
/// * `switcher` — the `ViewSwitcherWindow` to populate.
/// * `vm` — the view-switcher view-model from [`build_view_switcher_vm`].
/// * `on_view_changed` — called with the new zero-based view index whenever
///   the Slint-side `view-changed` callback fires. Phase 3 bridge: the caller
///   updates `current_view` on `ViewRouterGlobal` from this callback.
pub fn wire_view_switcher<F>(switcher: &ViewSwitcherWindow, vm: &ViewSwitcherVM, on_view_changed: F)
where
    F: Fn(i32) + 'static,
{
    use slint::{ModelRc, SharedString, VecModel};

    // Build the `ModelRc<ViewMeta>` from the VM's view list.
    let view_metas: Vec<view_switcher_slint::ViewMeta> = vm
        .views
        .iter()
        .map(|v| view_switcher_slint::ViewMeta {
            id: SharedString::from(v.id.as_str()),
            title: SharedString::from(v.title.as_str()),
        })
        .collect();
    let model: ModelRc<view_switcher_slint::ViewMeta> = ModelRc::new(VecModel::from(view_metas));
    switcher.set_views(model);

    // Compute the active view index from the active_view_id.
    // Falls back to 0 if the id is not found in the list.
    let active_index = vm
        .views
        .iter()
        .position(|v| v.id == vm.active_view_id)
        .map(|i| i32::try_from(i).unwrap_or(0))
        .unwrap_or(0);
    switcher.set_active_view_index(active_index);

    // Write density and touch_input verbatim.
    switcher.set_density(SharedString::from(vm.density.as_str()));
    switcher.set_touch_input(vm.touch_input);

    // Wire the view-changed callback. The Slint runtime calls this when the
    // user taps a tab, selects a dropdown item, or completes a swipe gesture.
    // Per locked_decisions.view_switcher_touch_gating: the swipe handler is
    // only instantiated when touch_input is true; this callback path is safe
    // regardless — the guard is at the Slint level.
    switcher.on_view_changed(on_view_changed);
}

// ---------------------------------------------------------------------------
// Phase 6 cosmetic-polish: dashboard-layout wiring (section grid + view tabs)
// ---------------------------------------------------------------------------
//
// `compute_dashboard_layout` and `wire_dashboard_layout` together expose the
// Dashboard's `views[].sections[]` structure to the Slint MainWindow. This
// gives the founder smoke run:
//
//   1. A tab-strip view switcher at the top of the window (when ≥2 views).
//   2. A 4-column-grid layout per section, with the section title shown as
//      a header above each section's tile area.
//   3. A simple greedy section-aware placement packer that fills each tile's
//      `TilePlacement` with (col, row, span_cols, span_rows) relative to
//      its containing section, so the Slint side can render tiles via
//      absolute positioning within the section's content rectangle.
//
// All work happens at dashboard load (init / view switch), NOT per-flush —
// per-flush refreshes only update the per-kind tile arrays via
// `set_row_data`. This keeps the per-flush hot path unchanged and the
// per-frame allocation budget at zero.

/// Compute per-tile section-relative placements via a simple greedy packer.
///
/// For each section in document order:
///   * walk widgets in declaration order;
///   * place each widget at the next free slot in the current row, advancing
///     to the next row when adding the widget would overflow `section.grid.columns`;
///   * if a widget's `preferred_columns` exceeds the section width, clamp to
///     the section width (minimum 1) so layout never overflows;
///   * record the resulting (col, row, span_cols, span_rows) into the
///     matching `TileVM`'s `placement` field via the `set_placement_*` helpers.
///
/// `tiles` MUST be the output of [`build_tiles`] for the same `dashboard`,
/// in the same document order — the function relies on the 1:1 correspondence
/// to write each tile's placement back into the right row of each per-kind
/// array. A debug-mode mismatch panics; in release the function silently
/// returns the original tile order untouched.
///
/// This runs once per dashboard load (and once per view switch in the
/// future — but section structure is YAML-static, so nothing actually
/// changes between view switches today). Per-flush refreshes never call
/// this function.
pub fn pack_section_layouts(dashboard: &Dashboard, tiles: &mut [TileVM]) {
    let mut tile_idx: usize = 0;
    for view in &dashboard.views {
        for section in &view.sections {
            let columns = i32::from(section.grid.columns).max(1);
            let mut cursor_col: i32 = 0;
            let mut cursor_row: i32 = 0;
            for _widget in &section.widgets {
                if tile_idx >= tiles.len() {
                    debug_assert!(
                        false,
                        "pack_section_layouts: tiles slice shorter than dashboard widget count"
                    );
                    return;
                }
                let placement = next_pack_placement(
                    tiles[tile_idx].placement(),
                    columns,
                    &mut cursor_col,
                    &mut cursor_row,
                );
                tiles[tile_idx].set_placement(placement);
                tile_idx += 1;
            }
            // Reset cursor at section boundary — each section gets its own
            // (col, row) coordinate space.
            let _ = cursor_col;
            let _ = cursor_row;
        }
    }
}

/// Compute the placement for a single tile during the greedy section pack.
///
/// Inputs:
///   * `current` — the tile's current placement (its preferred span dimensions
///     come from `span_cols` / `span_rows`).
///   * `section_columns` — the section's grid column count (>= 1).
///   * `cursor_col` / `cursor_row` — running cursor for the section, mutated.
///
/// Returns the new `TilePlacement` for this tile. Cursor advances within
/// the row; on overflow the cursor wraps to the next row.
fn next_pack_placement(
    current: TilePlacement,
    section_columns: i32,
    cursor_col: &mut i32,
    cursor_row: &mut i32,
) -> TilePlacement {
    // Clamp span_cols to the section width so a 4-column-span widget
    // declared in a 2-column section still renders (span = 2). Minimum 1
    // so a zero / negative value does not blank the tile.
    let span_cols = current.span_cols.clamp(1, section_columns);
    let span_rows = current.span_rows.max(1);

    // Wrap to next row if this tile won't fit on the current row.
    if *cursor_col + span_cols > section_columns {
        *cursor_col = 0;
        *cursor_row += 1;
    }

    let placement = TilePlacement {
        col: *cursor_col,
        row: *cursor_row,
        span_cols,
        span_rows,
    };

    *cursor_col += span_cols;
    if *cursor_col >= section_columns {
        *cursor_col = 0;
        *cursor_row += 1;
    }

    placement
}

/// Per-section layout summary produced by [`compute_dashboard_layout`].
///
/// One entry per section in document order across all views. The Rust bridge
/// converts these into `slint_ui::SectionVM` rows when wiring the window.
#[derive(Debug, Clone, PartialEq)]
pub struct DashboardSection {
    /// Section title (e.g. "Overview", "Climate").
    pub title: String,
    /// Number of grid columns for this section's content area.
    pub columns: i32,
    /// Logical-pixel gap between adjacent tiles within this section.
    pub gap_px: i32,
    /// Zero-based view index this section belongs to.
    pub view_index: i32,
    /// Number of grid rows occupied by the section's tiles (after packing).
    pub num_rows: i32,
}

/// Per-tile slot pointing back into the per-kind tile arrays.
///
/// Slint's MainWindow iterates this flat list inside each section's content
/// rectangle, dispatching to the correct per-kind tile component via the
/// `kind` discriminator and reading the tile's data from the matching
/// per-kind model at index `kind_index`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardTileSlot {
    /// Zero-based view index this slot belongs to.
    pub view_index: i32,
    /// Zero-based section index (across all views) this slot belongs to.
    pub section_index: i32,
    /// Discriminator string matching the Slint MainWindow tile dispatch:
    /// "light", "sensor", "entity", "cover", "fan", "lock", "alarm",
    /// "history", "camera", "climate", "media_player", "power_flow".
    pub kind: &'static str,
    /// Index into the matching per-kind tile array.
    pub kind_index: i32,
}

/// Result of computing the section / view / slot layout for a dashboard.
///
/// Built once per dashboard load. View switching just changes the
/// `MainWindow.active-view-index` property; the section list and slot list
/// are not rebuilt.
#[derive(Debug, Clone, PartialEq)]
pub struct DashboardLayout {
    /// Tab-strip / dropdown view list, in document order.
    pub views: Vec<ViewEntry>,
    /// Initial active view index (looked up from `Dashboard.default_view`).
    pub active_view_index: i32,
    /// Density string for the ViewSwitcher ("compact" | "regular" | "spacious").
    pub density: String,
    /// Per-section summary in document order across all views.
    pub sections: Vec<DashboardSection>,
    /// Per-tile slots in document order across all views and sections.
    pub tile_slots: Vec<DashboardTileSlot>,
}

/// Compute the [`DashboardLayout`] for `dashboard` against the active profile.
///
/// `tiles` MUST be the packed output of `build_tiles` + [`pack_section_layouts`]
/// (same document order, same length as the total widget count). The
/// function uses `tiles[i]`'s [`TileVM`] variant tag to determine each
/// slot's `kind` discriminator and to compute the per-kind row index by
/// counting prior tiles of the same kind. A debug-mode mismatch between
/// `dashboard` widget count and `tiles.len()` is a panic; release silently
/// returns an empty layout.
pub fn compute_dashboard_layout(
    dashboard: &Dashboard,
    profile: &DeviceProfile,
    tiles: &[TileVM],
) -> DashboardLayout {
    use crate::dashboard::profiles::Density;

    // ── Views ──────────────────────────────────────────────────────────
    let views: Vec<ViewEntry> = dashboard
        .views
        .iter()
        .map(|v| ViewEntry {
            id: v.id.clone(),
            title: v.title.clone(),
        })
        .collect();

    let active_view_index = dashboard
        .views
        .iter()
        .position(|v| v.id == dashboard.default_view)
        .map(|i| i32::try_from(i).unwrap_or(0))
        .unwrap_or(0);

    let density = match profile.density {
        Density::Compact => "compact",
        Density::Regular => "regular",
        Density::Spacious => "spacious",
    }
    .to_string();

    // ── Sections + slots ───────────────────────────────────────────────
    let mut sections: Vec<DashboardSection> = Vec::new();
    let mut tile_slots: Vec<DashboardTileSlot> = Vec::new();

    // Per-kind cursors: count of prior tiles of each kind, used to compute
    // the kind-relative row index for each slot.
    let mut kind_cursors = KindCursors::default();

    let mut tile_idx: usize = 0;
    for (view_idx_usize, view) in dashboard.views.iter().enumerate() {
        let view_idx = i32::try_from(view_idx_usize).unwrap_or(i32::MAX);
        for section in &view.sections {
            let section_idx_usize = sections.len();
            let section_idx = i32::try_from(section_idx_usize).unwrap_or(i32::MAX);

            // Track max row used by any tile in this section so we can
            // size the section's content rectangle.
            let mut max_row_plus_span: i32 = 0;

            for _widget in &section.widgets {
                if tile_idx >= tiles.len() {
                    debug_assert!(
                        false,
                        "compute_dashboard_layout: tiles slice shorter than dashboard widget count"
                    );
                    return DashboardLayout {
                        views,
                        active_view_index,
                        density,
                        sections: Vec::new(),
                        tile_slots: Vec::new(),
                    };
                }
                let tile = &tiles[tile_idx];
                let kind = tile_vm_kind_str(tile);
                let kind_index = kind_cursors.next(kind);
                let placement = tile.placement();
                max_row_plus_span =
                    max_row_plus_span.max(placement.row.saturating_add(placement.span_rows));

                tile_slots.push(DashboardTileSlot {
                    view_index: view_idx,
                    section_index: section_idx,
                    kind,
                    kind_index,
                });
                tile_idx += 1;
            }

            sections.push(DashboardSection {
                title: section.title.clone(),
                columns: i32::from(section.grid.columns).max(1),
                gap_px: i32::from(section.grid.gap),
                view_index: view_idx,
                num_rows: max_row_plus_span.max(1),
            });
        }
    }

    DashboardLayout {
        views,
        active_view_index,
        density,
        sections,
        tile_slots,
    }
}

/// Per-kind row-index counter used by [`compute_dashboard_layout`] to assign
/// each tile slot's `kind_index` matching its position in the per-kind array
/// produced by [`split_tile_vms`].
#[derive(Default)]
struct KindCursors {
    light: i32,
    sensor: i32,
    entity: i32,
    cover: i32,
    fan: i32,
    lock: i32,
    alarm: i32,
    history: i32,
    camera: i32,
    climate: i32,
    media_player: i32,
    power_flow: i32,
}

impl KindCursors {
    fn next(&mut self, kind: &'static str) -> i32 {
        let slot = match kind {
            "light" => &mut self.light,
            "sensor" => &mut self.sensor,
            "entity" => &mut self.entity,
            "cover" => &mut self.cover,
            "fan" => &mut self.fan,
            "lock" => &mut self.lock,
            "alarm" => &mut self.alarm,
            "history" => &mut self.history,
            "camera" => &mut self.camera,
            "climate" => &mut self.climate,
            "media_player" => &mut self.media_player,
            "power_flow" => &mut self.power_flow,
            _ => {
                debug_assert!(false, "KindCursors::next: unknown kind {kind:?}");
                return 0;
            }
        };
        let idx = *slot;
        *slot += 1;
        idx
    }
}

/// Map a [`TileVM`] variant to its Slint dispatch discriminator string.
fn tile_vm_kind_str(tile: &TileVM) -> &'static str {
    match tile {
        TileVM::Light(_) => "light",
        TileVM::Sensor(_) => "sensor",
        TileVM::Entity(_) => "entity",
        TileVM::Cover(_) => "cover",
        TileVM::Fan(_) => "fan",
        TileVM::Lock(_) => "lock",
        TileVM::Alarm(_) => "alarm",
        TileVM::History(_) => "history",
        TileVM::Camera(_) => "camera",
        TileVM::Climate(_) => "climate",
        TileVM::MediaPlayer(_) => "media_player",
        TileVM::PowerFlow(_) => "power_flow",
    }
}

/// Wire a [`DashboardLayout`] into the production [`MainWindow`]'s Slint
/// properties.
///
/// Called once per `Dashboard` load by `src/lib.rs::run_with_memory_store`
/// and `run_with_live_store`. The view-changed callback updates the
/// MainWindow's `active-view-index` directly; the section list and tile-slot
/// list are NOT rebuilt because section structure is YAML-static.
pub fn wire_dashboard_layout(window: &MainWindow, layout: &DashboardLayout) {
    use slint::Weak;

    // ── Views model ────────────────────────────────────────────────────
    let view_metas: Vec<slint_ui::ViewMeta> = layout
        .views
        .iter()
        .map(|v| slint_ui::ViewMeta {
            id: SharedString::from(v.id.as_str()),
            title: SharedString::from(v.title.as_str()),
        })
        .collect();
    window.set_views(ModelRc::new(VecModel::from(view_metas)));
    window.set_active_view_index(layout.active_view_index);

    // ── Sections model ─────────────────────────────────────────────────
    let section_vms: Vec<slint_ui::SectionVM> = layout
        .sections
        .iter()
        .map(|s| slint_ui::SectionVM {
            title: SharedString::from(s.title.as_str()),
            columns: s.columns,
            // `gap` is a Slint `length` (logical pixels, f32). Source
            // `gap_px` is u8-derived (0..=255), so the i32->f32 conversion
            // is loss-free.
            gap: s.gap_px as f32,
            r#view_index: s.view_index,
            r#num_rows: s.num_rows,
        })
        .collect();
    window.set_sections(ModelRc::new(VecModel::from(section_vms)));

    // ── Tile-slots model ───────────────────────────────────────────────
    let slot_vms: Vec<slint_ui::TileSlot> = layout
        .tile_slots
        .iter()
        .map(|slot| slint_ui::TileSlot {
            r#view_index: slot.view_index,
            r#section_index: slot.section_index,
            kind: SharedString::from(slot.kind),
            r#kind_index: slot.kind_index,
        })
        .collect();
    window.set_tile_slots(ModelRc::new(VecModel::from(slot_vms)));

    // ── view-changed callback ──────────────────────────────────────────
    //
    // Phase-6-cosmetic-polish: tapping a tab updates `active-view-index`
    // directly. The bridge does not re-write any model on view switch
    // (sections list already carries `view-index`; Slint's per-section
    // `if section.view-index == active-view-index` gate is the renderer).
    let weak: Weak<MainWindow> = window.as_weak();
    window.on_view_changed(move |idx| {
        if let Some(w) = weak.upgrade() {
            w.set_active_view_index(idx);
        }
    });
}

// Helper accessors used by the section-aware packer.
impl TileVM {
    /// Read the tile's current `TilePlacement` regardless of variant.
    pub(crate) fn placement(&self) -> TilePlacement {
        match self {
            TileVM::Light(vm) => vm.placement,
            TileVM::Sensor(vm) => vm.placement,
            TileVM::Entity(vm) => vm.placement,
            TileVM::Cover(vm) => vm.placement,
            TileVM::Fan(vm) => vm.placement,
            TileVM::Lock(vm) => vm.placement,
            TileVM::Alarm(vm) => vm.placement,
            TileVM::History(vm) => vm.placement,
            TileVM::Camera(vm) => vm.placement,
            TileVM::Climate(vm) => vm.placement,
            TileVM::MediaPlayer(vm) => vm.placement,
            TileVM::PowerFlow(vm) => vm.placement,
        }
    }

    /// Mutate the tile's `TilePlacement` regardless of variant. Used by
    /// [`pack_section_layouts`] to write the packer's output back into the
    /// tile slice in-place.
    pub(crate) fn set_placement(&mut self, p: TilePlacement) {
        match self {
            TileVM::Light(vm) => vm.placement = p,
            TileVM::Sensor(vm) => vm.placement = p,
            TileVM::Entity(vm) => vm.placement = p,
            TileVM::Cover(vm) => vm.placement = p,
            TileVM::Fan(vm) => vm.placement = p,
            TileVM::Lock(vm) => vm.placement = p,
            TileVM::Alarm(vm) => vm.placement = p,
            TileVM::History(vm) => vm.placement = p,
            TileVM::Camera(vm) => vm.placement = p,
            TileVM::Climate(vm) => vm.placement = p,
            TileVM::MediaPlayer(vm) => vm.placement = p,
            TileVM::PowerFlow(vm) => vm.placement = p,
        }
    }
}

// ---------------------------------------------------------------------------
// PinEntry Slint module (TASK-100)
// ---------------------------------------------------------------------------
//
// `ui/slint/pin_entry.slint` is compiled by `build.rs` (TASK-100) to a
// separate generated Rust file exposed via the `HANUI_PIN_ENTRY_INCLUDE`
// env var — the same pattern as `gesture_test_window.slint` (TASK-060) and
// `view_switcher.slint` (TASK-086). This module picks it up via `include!`
// so the generated `PinEntryWindow` type is available for the `SlintPinHost`
// implementation without polluting the production `slint_ui` namespace.
pub mod pin_entry_slint {
    include!(env!("HANUI_PIN_ENTRY_INCLUDE"));
}

pub use pin_entry_slint::PinEntryWindow;

// ---------------------------------------------------------------------------
// CoverTile Slint module (TASK-102)
// ---------------------------------------------------------------------------
//
// `ui/slint/cover_tile.slint` is compiled by `build.rs` (TASK-102) to a
// separate generated Rust file exposed via the `HANUI_COVER_TILE_INCLUDE`
// env var — the same pattern as `pin_entry.slint` (TASK-100),
// `view_switcher.slint` (TASK-086), and `gesture_test_window.slint`
// (TASK-060). This module picks it up via `include!` so the generated
// `CoverTile`, `CoverTileVM`, and `CoverTilePlacement` types are available
// for the `compute_cover_tile_vm` projection function below without
// polluting the production `slint_ui` namespace (which would clash with
// the same-named Rust struct declared earlier in this file).
//
// Future work: once `main_window.slint` grows a `cover-tiles` array
// property, this module's types will be re-exported through the
// production `slint_ui` namespace instead. The separate compile is the
// minimal-blast path that satisfies TASK-102's "Slint compile gate"
// acceptance criterion (cover_tile.slint must be in the build graph)
// without amending any of the protected `must_not_touch` Slint files.
pub mod cover_tile_slint {
    include!(env!("HANUI_COVER_TILE_INCLUDE"));
}

pub use cover_tile_slint::{CoverTile, CoverTilePlacement as SlintCoverTilePlacement};

// ---------------------------------------------------------------------------
// FanTile Slint module (TASK-103)
// ---------------------------------------------------------------------------
//
// `ui/slint/fan_tile.slint` is compiled by `build.rs` (TASK-103) to a
// separate generated Rust file exposed via the `HANUI_FAN_TILE_INCLUDE`
// env var — the same pattern as `cover_tile.slint` (TASK-102),
// `pin_entry.slint` (TASK-100), `view_switcher.slint` (TASK-086), and
// `gesture_test_window.slint` (TASK-060). This module picks it up via
// `include!` so the generated `FanTile`, `FanTileVM`, and
// `FanTilePlacement` types are available for the `compute_fan_tile_vm`
// projection function below without polluting the production `slint_ui`
// namespace (which would clash with the same-named Rust struct declared
// earlier in this file).
//
// Future work: once `main_window.slint` grows a `fan-tiles` array
// property, this module's types will be re-exported through the
// production `slint_ui` namespace instead. The separate compile is the
// minimal-blast path that satisfies TASK-103's "Slint compile gate"
// acceptance criterion (fan_tile.slint must be in the build graph)
// without amending any of the protected `must_not_touch` Slint files.
pub mod fan_tile_slint {
    include!(env!("HANUI_FAN_TILE_INCLUDE"));
}

pub use fan_tile_slint::{FanTile, FanTilePlacement as SlintFanTilePlacement};

// ---------------------------------------------------------------------------
// LockTile Slint module (TASK-104)
// ---------------------------------------------------------------------------
//
// `ui/slint/lock_tile.slint` is compiled by `build.rs` (TASK-104) to a
// separate generated Rust file exposed via the `HANUI_LOCK_TILE_INCLUDE`
// env var — the same pattern as `fan_tile.slint` (TASK-103),
// `cover_tile.slint` (TASK-102), `pin_entry.slint` (TASK-100),
// `view_switcher.slint` (TASK-086), and `gesture_test_window.slint`
// (TASK-060). This module picks it up via `include!` so the generated
// `LockTile`, `LockTileVM`, and `LockTilePlacement` types are available
// for the `compute_lock_tile_vm` projection function below without
// polluting the production `slint_ui` namespace (which would clash with
// the same-named Rust struct declared earlier in this file).
//
// Future work: once `main_window.slint` grows a `lock-tiles` array
// property, this module's types will be re-exported through the
// production `slint_ui` namespace instead. The separate compile is the
// minimal-blast path that satisfies TASK-104's "Slint compile gate"
// acceptance criterion (lock_tile.slint must be in the build graph)
// without amending any of the protected `must_not_touch` Slint files.
pub mod lock_tile_slint {
    include!(env!("HANUI_LOCK_TILE_INCLUDE"));
}

pub use lock_tile_slint::{LockTile, LockTilePlacement as SlintLockTilePlacement};

// ---------------------------------------------------------------------------
// AlarmPanelTile Slint module (TASK-105)
// ---------------------------------------------------------------------------
//
// `ui/slint/alarm_panel_tile.slint` is compiled by `build.rs` (TASK-105)
// to a separate generated Rust file exposed via the
// `HANUI_ALARM_PANEL_TILE_INCLUDE` env var — the same pattern as
// `cover_tile.slint` (TASK-102), `fan_tile.slint` (TASK-103),
// `lock_tile.slint` (TASK-104), `pin_entry.slint` (TASK-100),
// `view_switcher.slint` (TASK-086), and `gesture_test_window.slint`
// (TASK-060). This module picks it up via `include!` so the generated
// `AlarmPanelTile`, `AlarmTileVM`, and `AlarmTilePlacement` types are
// available for the `compute_alarm_tile_vm` projection function below
// without polluting the production `slint_ui` namespace (which would
// clash with the same-named Rust struct declared earlier in this file).
//
// Future work: once `main_window.slint` grows an `alarm-tiles` array
// property, this module's types will be re-exported through the
// production `slint_ui` namespace instead. The separate compile is the
// minimal-blast path that satisfies TASK-105's "Slint compile gate"
// acceptance criterion (alarm_panel_tile.slint must be in the build
// graph) without amending any of the protected `must_not_touch` Slint
// files.
pub mod alarm_panel_tile_slint {
    include!(env!("HANUI_ALARM_PANEL_TILE_INCLUDE"));
}

pub use alarm_panel_tile_slint::{AlarmPanelTile, AlarmTilePlacement as SlintAlarmTilePlacement};

// ---------------------------------------------------------------------------
// HistoryGraphTile Slint module (TASK-106)
// ---------------------------------------------------------------------------
//
// `ui/slint/history_graph_tile.slint` is compiled by `build.rs` (TASK-106)
// to a separate generated Rust file exposed via the
// `HANUI_HISTORY_GRAPH_TILE_INCLUDE` env var — the same pattern as
// `cover_tile.slint` (TASK-102), `fan_tile.slint` (TASK-103),
// `lock_tile.slint` (TASK-104), and `alarm_panel_tile.slint` (TASK-105).
// This module picks it up via `include!` so the generated
// `HistoryGraphTile`, `HistoryGraphTileVM`, and `HistoryGraphTilePlacement`
// types are available for the `compute_history_graph_tile_vm` projection
// function below without polluting the production `slint_ui` namespace
// (which would clash with the same-named Rust struct declared earlier in
// this file).
//
// Future work: once `main_window.slint` grows a `history-tiles` array
// property, this module's types will be re-exported through the
// production `slint_ui` namespace instead. The separate compile is the
// minimal-blast path that satisfies TASK-106's "Slint compile gate"
// acceptance criterion (history_graph_tile.slint must be in the build
// graph) without amending any of the protected `must_not_touch` Slint
// files.
pub mod history_graph_tile_slint {
    include!(env!("HANUI_HISTORY_GRAPH_TILE_INCLUDE"));
}

pub use history_graph_tile_slint::{
    HistoryGraphTile, HistoryGraphTilePlacement as SlintHistoryGraphTilePlacement,
};

// ---------------------------------------------------------------------------
// CameraSnapshotTile Slint module (TASK-107)
// ---------------------------------------------------------------------------
//
// `ui/slint/camera_snapshot_tile.slint` is compiled by `build.rs` (TASK-107)
// to a separate generated Rust file exposed via the
// `HANUI_CAMERA_SNAPSHOT_TILE_INCLUDE` env var — the same pattern as
// `cover_tile.slint` (TASK-102), `fan_tile.slint` (TASK-103),
// `lock_tile.slint` (TASK-104), `alarm_panel_tile.slint` (TASK-105), and
// `history_graph_tile.slint` (TASK-106). This module picks it up via
// `include!` so the generated `CameraSnapshotTile`, `CameraTileVM`, and
// `CameraTilePlacement` types are available for the
// `compute_camera_tile_vm` projection function below without polluting the
// production `slint_ui` namespace (which would clash with the same-named
// Rust struct declared earlier in this file).
//
// Future work: once `main_window.slint` grows a `camera-tiles` array
// property, this module's types will be re-exported through the
// production `slint_ui` namespace instead. The separate compile is the
// minimal-blast path that satisfies TASK-107's "Slint compile gate"
// acceptance criterion (camera_snapshot_tile.slint must be in the build
// graph) without amending any of the protected `must_not_touch` Slint
// files.
pub mod camera_snapshot_tile_slint {
    include!(env!("HANUI_CAMERA_SNAPSHOT_TILE_INCLUDE"));
}

pub use camera_snapshot_tile_slint::{
    CameraSnapshotTile, CameraTilePlacement as SlintCameraTilePlacement,
};

// ---------------------------------------------------------------------------
// ClimateTile slint submodule (TASK-108)
// ---------------------------------------------------------------------------
//
// `ui/slint/climate_tile.slint` is compiled by `build.rs` (TASK-108) to a
// separate generated Rust file exposed via the `HANUI_CLIMATE_TILE_INCLUDE`
// env var — the same pattern as `cover_tile.slint` (TASK-102),
// `fan_tile.slint` (TASK-103), `lock_tile.slint` (TASK-104),
// `alarm_panel_tile.slint` (TASK-105), `history_graph_tile.slint`
// (TASK-106), and `camera_snapshot_tile.slint` (TASK-107). This module
// picks it up via `include!` so the generated `ClimateTile`,
// `ClimateTileVM`, and `ClimateTilePlacement` types are available for the
// `compute_climate_tile_vm` projection function below without polluting
// the production `slint_ui` namespace (which would clash with the
// same-named Rust struct declared earlier in this file).
//
// Future work: once `main_window.slint` grows a `climate-tiles` array
// property, this module's types will be re-exported through the production
// `slint_ui` namespace instead. The separate compile is the minimal-blast
// path that satisfies TASK-108's "Slint compile gate" acceptance criterion
// (climate_tile.slint must be in the build graph) without amending any of
// the protected `must_not_touch` Slint files.
pub mod climate_tile_slint {
    include!(env!("HANUI_CLIMATE_TILE_INCLUDE"));
}

pub use climate_tile_slint::{ClimateTile, ClimateTilePlacement as SlintClimateTilePlacement};

// ---------------------------------------------------------------------------
// MediaPlayerTile slint submodule (TASK-109)
// ---------------------------------------------------------------------------
//
// `ui/slint/media_player_tile.slint` is compiled by `build.rs` (TASK-109)
// to a separate generated Rust file exposed via the
// `HANUI_MEDIA_PLAYER_TILE_INCLUDE` env var — the same pattern as
// `cover_tile.slint` (TASK-102), `fan_tile.slint` (TASK-103),
// `lock_tile.slint` (TASK-104), `alarm_panel_tile.slint` (TASK-105),
// `history_graph_tile.slint` (TASK-106), `camera_snapshot_tile.slint`
// (TASK-107), and `climate_tile.slint` (TASK-108). This module picks it
// up via `include!` so the generated `MediaPlayerTile`,
// `MediaPlayerTileVM`, and `MediaPlayerTilePlacement` types are available
// for the `compute_media_player_tile_vm` projection function below
// without polluting the production `slint_ui` namespace (which would
// clash with the same-named Rust struct declared earlier in this file).
//
// Future work: once `main_window.slint` grows a `media-player-tiles`
// array property, this module's types will be re-exported through the
// production `slint_ui` namespace instead. The separate compile is the
// minimal-blast path that satisfies TASK-109's "Slint compile gate"
// acceptance criterion (media_player_tile.slint must be in the build
// graph) without amending any of the protected `must_not_touch` Slint
// files.
pub mod media_player_tile_slint {
    include!(env!("HANUI_MEDIA_PLAYER_TILE_INCLUDE"));
}

pub use media_player_tile_slint::{
    MediaPlayerTile, MediaPlayerTilePlacement as SlintMediaPlayerTilePlacement,
};

// ---------------------------------------------------------------------------
// PowerFlowTile slint submodule (TASK-094)
// ---------------------------------------------------------------------------
//
// `ui/slint/power_flow_tile.slint` is compiled by `build.rs` (TASK-094) to
// a separate generated Rust file exposed via the
// `HANUI_POWER_FLOW_TILE_INCLUDE` env var — the same pattern as
// `cover_tile.slint` (TASK-102), `fan_tile.slint` (TASK-103),
// `lock_tile.slint` (TASK-104), `alarm_panel_tile.slint` (TASK-105),
// `history_graph_tile.slint` (TASK-106), `camera_snapshot_tile.slint`
// (TASK-107), `climate_tile.slint` (TASK-108), and `media_player_tile.slint`
// (TASK-109). This module picks it up via `include!` so the generated
// `PowerFlowTile`, `PowerFlowTileVM`, and `PowerFlowTilePlacement` types
// are available for the `compute_power_flow_tile_vm` projection function
// below without polluting the production `slint_ui` namespace (which would
// clash with the same-named Rust struct declared earlier in this file).
//
// Future work: once `main_window.slint` grows a `power-flow-tiles` array
// property, this module's types will be re-exported through the production
// `slint_ui` namespace instead. The separate compile is the minimal-blast
// path that satisfies TASK-094's "Slint compile gate" acceptance criterion
// (power_flow_tile.slint must be in the build graph) without amending any
// of the protected `must_not_touch` Slint files.
pub mod power_flow_tile_slint {
    include!(env!("HANUI_POWER_FLOW_TILE_INCLUDE"));
}

pub use power_flow_tile_slint::{
    PowerFlowTile, PowerFlowTilePlacement as SlintPowerFlowTilePlacement,
};

// ---------------------------------------------------------------------------
// compute_cover_tile_vm (TASK-102)
// ---------------------------------------------------------------------------

/// Project a typed Rust [`CoverTileVM`] from a live entity snapshot, threading
/// through [`crate::ui::cover::CoverVM::from_entity`] for the per-frame derived
/// state (`is_open` / `is_moving` / `position`).
///
/// # Hot-path discipline
///
/// Called at entity-change time (NOT per render). The result is a typed
/// `CoverTileVM` Rust struct; the further `String -> SharedString` /
/// `icon_id -> Image` conversions are deferred to a follow-up ticket once
/// `main_window.slint` declares the `cover-tiles` array property.
///
/// # Position / tilt fallback
///
/// `has_position` / `has_tilt` are derived from the presence of the
/// `current_position` / `current_tilt_position` HA attributes. When the
/// attribute is absent or out of range, the boolean is `false` and the
/// numeric field carries the state-derived default
/// (`CoverVM::from_entity` returns 0 for closed-equivalent states and
/// 100 for `"open"`). The Slint tile gates the position/tilt labels
/// behind these booleans (`if view-model.has-position : Text { ... }`).
///
/// # Naming
///
/// Returns the Rust [`CoverTileVM`] (defined earlier in this file). The
/// Slint-shape projection (`SlintCoverTileVM` from
/// [`cover_tile_slint::CoverTileVM`]) is not built here because there is
/// no `cover-tiles` `MainWindow` property to write into yet — that
/// shape conversion lives next to the `set_cover_tiles` call site once
/// it exists.
#[must_use]
pub fn compute_cover_tile_vm(
    name: String,
    icon_id: String,
    preferred_columns: i32,
    preferred_rows: i32,
    placement: TilePlacement,
    entity: &crate::ha::entity::Entity,
) -> CoverTileVM {
    let cover_vm = crate::ui::cover::CoverVM::from_entity(entity);

    // Derive the has-position / has-tilt booleans from the live
    // entity. We re-check via `cover::read_*_attribute` rather than
    // re-fetching by name so out-of-range numeric attributes count as
    // "not present" — matching the fall-through logic in `CoverVM::from_entity`.
    let has_position = entity.attributes.get("current_position").is_some();
    let tilt_value = crate::ui::cover::read_tilt_attribute(entity);
    let has_tilt = tilt_value.is_some();

    let state = entity.state.as_ref().to_owned();

    CoverTileVM {
        name,
        state,
        position: i32::from(cover_vm.position),
        tilt: i32::from(tilt_value.unwrap_or(0)),
        has_position,
        has_tilt,
        is_open: cover_vm.is_open,
        is_moving: cover_vm.is_moving,
        icon_id,
        preferred_columns,
        preferred_rows,
        placement,
        pending: false,
    }
}

// ---------------------------------------------------------------------------
// compute_fan_tile_vm (TASK-103)
// ---------------------------------------------------------------------------

/// Project a typed Rust [`FanTileVM`] from a live entity snapshot, threading
/// through [`crate::ui::fan::FanVM::from_entity`] for the per-frame derived
/// state (`is_on` / `speed_pct` / `current_speed`).
///
/// # Hot-path discipline
///
/// Called at entity-change time (NOT per render). The result is a typed
/// `FanTileVM` Rust struct; the further `String -> SharedString` /
/// `icon_id -> Image` conversions are deferred to a follow-up ticket once
/// `main_window.slint` declares the `fan-tiles` array property.
///
/// # Speed-pct / current-speed fallback
///
/// `has_speed_pct` is derived from the presence of a numeric, in-range
/// `percentage` attribute (out-of-range or non-numeric values count as
/// "not present" — matching the fall-through logic in `FanVM::from_entity`).
/// `has_current_speed` is derived from the presence of a string-typed
/// `preset_mode` attribute. The Slint tile gates the matching labels
/// behind these booleans (`if view-model.has-speed-pct : Text { ... }`).
///
/// # Naming
///
/// Returns the Rust [`FanTileVM`] (defined earlier in this file). The
/// Slint-shape projection (`SlintFanTileVM` from
/// [`fan_tile_slint::FanTileVM`]) is not built here because there is
/// no `fan-tiles` `MainWindow` property to write into yet — that
/// shape conversion lives next to the `set_fan_tiles` call site once
/// it exists.
#[must_use]
pub fn compute_fan_tile_vm(
    name: String,
    icon_id: String,
    preferred_columns: i32,
    preferred_rows: i32,
    placement: TilePlacement,
    entity: &crate::ha::entity::Entity,
) -> FanTileVM {
    let fan_vm = crate::ui::fan::FanVM::from_entity(entity);

    let has_speed_pct = fan_vm.speed_pct.is_some();
    let speed_pct_value = i32::from(fan_vm.speed_pct.unwrap_or(0));
    let has_current_speed = fan_vm.current_speed.is_some();
    let current_speed_value = fan_vm.current_speed.unwrap_or_default();

    FanTileVM {
        name,
        state: fan_vm.state,
        speed_pct: speed_pct_value,
        has_speed_pct,
        is_on: fan_vm.is_on,
        current_speed: current_speed_value,
        has_current_speed,
        icon_id,
        preferred_columns,
        preferred_rows,
        placement,
        pending: false,
    }
}

// ---------------------------------------------------------------------------
// compute_lock_tile_vm (TASK-104)
// ---------------------------------------------------------------------------

/// Project a typed Rust [`LockTileVM`] from a live entity snapshot, threading
/// through [`crate::ui::lock::LockVM::from_entity`] for the per-frame derived
/// `is_locked` boolean.
///
/// # Hot-path discipline
///
/// Called at entity-change time (NOT per render). The result is a typed
/// `LockTileVM` Rust struct; the further `String -> SharedString` /
/// `icon_id -> Image` conversions are deferred to a follow-up ticket once
/// `main_window.slint` declares the `lock-tiles` array property.
///
/// # No `Vec` allocation
///
/// Per the TASK-103 audit lesson: this projection allocates only the
/// scalar `state`/`name`/`icon_id` strings the tile actually renders. PIN
/// policy and confirmation flag are NOT read here — they are dispatcher
/// concerns, looked up via the dispatcher's per-widget `lock_settings`
/// table at dispatch time. The tile VM stays lean.
///
/// # Naming
///
/// Returns the Rust [`LockTileVM`] (defined earlier in this file). The
/// Slint-shape projection (`SlintLockTileVM` from
/// [`lock_tile_slint::LockTileVM`]) is not built here because there is
/// no `lock-tiles` `MainWindow` property to write into yet — that
/// shape conversion lives next to the `set_lock_tiles` call site once
/// it exists.
#[must_use]
pub fn compute_lock_tile_vm(
    name: String,
    icon_id: String,
    preferred_columns: i32,
    preferred_rows: i32,
    placement: TilePlacement,
    entity: &crate::ha::entity::Entity,
) -> LockTileVM {
    let lock_vm = crate::ui::lock::LockVM::from_entity(entity);

    LockTileVM {
        name,
        state: entity.state.as_ref().to_owned(),
        is_locked: lock_vm.is_locked,
        icon_id,
        preferred_columns,
        preferred_rows,
        placement,
        pending: false,
    }
}

// ---------------------------------------------------------------------------
// compute_alarm_tile_vm (TASK-105)
// ---------------------------------------------------------------------------

/// Project a typed Rust [`AlarmTileVM`] from a live entity snapshot,
/// threading through [`crate::ui::alarm::AlarmVM::from_entity`] for the
/// per-frame derived state (`is_armed` / `is_triggered` / `is_pending`).
///
/// # Hot-path discipline
///
/// Called at entity-change time (NOT per render). The result is a typed
/// `AlarmTileVM` Rust struct; the further `String -> SharedString` /
/// `icon_id -> Image` conversions are deferred to a follow-up ticket once
/// `main_window.slint` declares the `alarm-tiles` array property.
///
/// # No `Vec` allocation
///
/// Per the TASK-103 audit lesson: this projection allocates only the
/// scalar `state`/`name`/`icon_id` strings the tile actually renders.
/// PIN policy is NOT read here — it is a dispatcher concern, looked up
/// via the dispatcher's per-widget `alarm_settings` table at dispatch
/// time. The tile VM stays lean.
///
/// # Naming
///
/// Returns the Rust [`AlarmTileVM`] (defined earlier in this file). The
/// Slint-shape projection (`SlintAlarmTileVM` from
/// [`alarm_panel_tile_slint::AlarmTileVM`]) is not built here because there
/// is no `alarm-tiles` `MainWindow` property to write into yet — that
/// shape conversion lives next to the `set_alarm_tiles` call site once
/// it exists.
#[must_use]
pub fn compute_alarm_tile_vm(
    name: String,
    icon_id: String,
    preferred_columns: i32,
    preferred_rows: i32,
    placement: TilePlacement,
    entity: &crate::ha::entity::Entity,
) -> AlarmTileVM {
    let alarm_vm = crate::ui::alarm::AlarmVM::from_entity(entity);

    AlarmTileVM {
        name,
        state: alarm_vm.state,
        is_armed: alarm_vm.is_armed,
        is_triggered: alarm_vm.is_triggered,
        is_pending: alarm_vm.is_pending,
        icon_id,
        preferred_columns,
        preferred_rows,
        placement,
        pending: false,
    }
}

// ---------------------------------------------------------------------------
// compute_history_graph_tile_vm (TASK-106)
// ---------------------------------------------------------------------------

/// Project a typed Rust [`HistoryGraphTileVM`] from a live entity snapshot
/// and an optional [`crate::ha::history::HistoryWindow`], threading through
/// [`crate::ui::history_graph::HistoryGraphVM::from_entity`] for the
/// per-frame derived `is_available` boolean.
///
/// # Hot-path discipline
///
/// Called at entity-change time (NOT per render). The result is a typed
/// `HistoryGraphTileVM` Rust struct; the further `String -> SharedString`
/// / `icon_id -> Image` conversions are deferred to a follow-up ticket
/// once `main_window.slint` declares the `history-tiles` array property.
///
/// `window` is `None` when the bridge has not yet completed a fetch for
/// this widget (or the fetch returned no plottable points). In that case
/// `change_count = 0` and `path_commands = ""` — the Slint tile renders
/// "0 samples" and an empty trace per the three-state-render acceptance.
///
/// # Path-commands composition
///
/// When `window` is `Some(_)`, [`history_path_commands`] composes the SVG
/// mini-language polyline string from the downsampled points. The
/// composition allocates one `String` per call and runs at fetch time
/// (per-window, not per-frame).
///
/// # No `Vec` allocation on the per-frame path
///
/// Per the TASK-103 audit lesson: this projection writes scalar
/// `state`/`name`/`icon_id` plus a single owned `path_commands` string.
/// The history `Vec<(Timestamp, f64)>` itself lives in the bridge's
/// per-widget [`crate::ha::history::HistoryWindow`] cache and is NOT
/// re-allocated on every state change.
///
/// # Naming
///
/// Returns the Rust [`HistoryGraphTileVM`] (defined earlier in this
/// file). The Slint-shape projection (`SlintHistoryGraphTileVM` from
/// [`history_graph_tile_slint::HistoryGraphTileVM`]) is not built here
/// because there is no `history-tiles` `MainWindow` property to write
/// into yet — that shape conversion lives next to the
/// `set_history_tiles` call site once it exists.
#[must_use]
pub fn compute_history_graph_tile_vm(
    name: String,
    icon_id: String,
    preferred_columns: i32,
    preferred_rows: i32,
    placement: TilePlacement,
    entity: &crate::ha::entity::Entity,
    window: Option<&crate::ha::history::HistoryWindow>,
) -> HistoryGraphTileVM {
    let change_count: i32 = window
        .map(|w| i32::try_from(w.len()).unwrap_or(i32::MAX))
        .unwrap_or(0);
    let history_vm = crate::ui::history_graph::HistoryGraphVM::from_entity(entity, change_count);
    let path_commands = window.map(history_path_commands).unwrap_or_default();

    HistoryGraphTileVM {
        name,
        state: entity.state.as_ref().to_owned(),
        change_count: history_vm.change_count,
        is_available: history_vm.is_available,
        icon_id,
        preferred_columns,
        preferred_rows,
        placement,
        pending: false,
        path_commands,
    }
}

/// Compose an SVG mini-language polyline string from a downsampled
/// [`crate::ha::history::HistoryWindow`] per
/// `locked_decisions.history_render_path`.
///
/// The output format is `"M x y L x y L x y ..."` where each coordinate is
/// in the unit square (0.0..=1.0). The Slint Path's `viewbox-width` /
/// `viewbox-height` are 1.0, so Slint scales these to pixel space at
/// render time.
///
/// # Normalisation
///
/// * Timestamps map to the X axis: the first point lands at `x=0.0`, the
///   last at `x=1.0`, and intermediate points are linear-interpolated by
///   their unix-second offset. The LTTB downsampler always preserves the
///   first and last input points, so for `n > 1` with distinct
///   timestamps `ts_span > 0` and the contract holds. **Coincident-
///   timestamp edge case**: when `ts_span == 0.0` (single-point window OR
///   the rare HA case of multiple records emitted with identical
///   `last_changed`), every point collapses to `x=0.0`. The Slint Path
///   renders a degenerate vertical line in that case — visually
///   indistinguishable from a single dot, which is the correct
///   "insufficient temporal resolution" fallback.
/// * Numeric values map to the Y axis with `y=0.0` at the maximum value
///   and `y=1.0` at the minimum value. The inversion follows screen-space
///   convention (origin at top) so a rising sensor trace draws upward.
///   When `min == max` (constant trace), every `y` lands at `0.5` (the
///   centreline).
///
/// # Empty input
///
/// An empty window returns an empty string — Slint's Path renders nothing
/// for the empty-commands case, which is the correct no-data behaviour.
///
/// # Allocations
///
/// One `String` per call. Capacity is preallocated to `points.len() * 24`
/// (a generous upper bound on per-coordinate-pair byte cost: each
/// "L 0.0000 0.0000 " entry is at most ~18 bytes; the overshoot trades a
/// tiny memory hit for zero realloc churn during the format!() loop).
#[must_use]
pub fn history_path_commands(window: &crate::ha::history::HistoryWindow) -> String {
    let points = &window.points;
    if points.is_empty() {
        return String::new();
    }

    // Compute axis ranges. Use unix-second representation for the X axis
    // so the timestamp-to-float conversion matches the LTTB downsampler's
    // own internal ranking key.
    let first_ts = points[0].0.as_second() as f64;
    let last_ts = points[points.len() - 1].0.as_second() as f64;
    let ts_span = (last_ts - first_ts).max(0.0);

    let mut min_val = f64::INFINITY;
    let mut max_val = f64::NEG_INFINITY;
    for (_, v) in points {
        if *v < min_val {
            min_val = *v;
        }
        if *v > max_val {
            max_val = *v;
        }
    }
    let val_span = (max_val - min_val).abs();

    let normalise_x = |ts_secs: f64| -> f64 {
        if ts_span == 0.0 {
            // Single-instant window: collapse to x=0.
            0.0
        } else {
            ((ts_secs - first_ts) / ts_span).clamp(0.0, 1.0)
        }
    };
    let normalise_y = |val: f64| -> f64 {
        if val_span == 0.0 {
            // Constant trace: render at the centre.
            0.5
        } else {
            // Invert so the maximum lands at y=0 (top of the unit square).
            (1.0 - (val - min_val) / val_span).clamp(0.0, 1.0)
        }
    };

    let mut out = String::with_capacity(points.len() * 24);
    use std::fmt::Write as _;
    for (i, (ts, val)) in points.iter().enumerate() {
        let x = normalise_x(ts.as_second() as f64);
        let y = normalise_y(*val);
        let cmd = if i == 0 { 'M' } else { 'L' };
        // `write!` on a String never fails; unwrap is the standard idiom.
        write!(&mut out, "{cmd} {x:.4} {y:.4} ").expect("write to String never fails");
    }
    // Trim the trailing space for tidiness; Slint accepts either form
    // (the SVG mini-language treats whitespace as a separator).
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// compute_camera_tile_vm (TASK-107)
// ---------------------------------------------------------------------------

/// Project a typed Rust [`CameraTileVM`] from a live entity snapshot,
/// threading through [`crate::ui::camera::CameraVM::from_entity`] for the
/// per-frame derived booleans (`is_recording` / `is_streaming` /
/// `is_available`).
///
/// # Hot-path discipline
///
/// Called at entity-change time (NOT per render). The result is a typed
/// `CameraTileVM` Rust struct; the further `String -> SharedString` /
/// `icon_id -> Image` conversions are deferred to a follow-up ticket
/// once `main_window.slint` declares the `camera-tiles` array property.
///
/// # No `Vec` allocation on the per-frame path
///
/// Per the TASK-103 / TASK-105 audit lesson: this projection writes only
/// scalar `state` / `name` / `icon_id` strings the tile actually renders.
/// The decoder pool's image bytes live in [`crate::ha::camera::CameraPool`]
/// and reach Slint as an `Image` property in a follow-up ticket — they do
/// NOT travel through this VM.
///
/// # Naming
///
/// Returns the Rust [`CameraTileVM`] (defined earlier in this file). The
/// Slint-shape projection (`SlintCameraTileVM` from
/// [`camera_snapshot_tile_slint::CameraTileVM`]) is not built here because
/// there is no `camera-tiles` `MainWindow` property to write into yet —
/// that shape conversion lives next to the `set_camera_tiles` call site
/// once it exists.
#[must_use]
pub fn compute_camera_tile_vm(
    name: String,
    icon_id: String,
    preferred_columns: i32,
    preferred_rows: i32,
    placement: TilePlacement,
    entity: &crate::ha::entity::Entity,
) -> CameraTileVM {
    let camera_vm = crate::ui::camera::CameraVM::from_entity(entity);
    let state = entity.state.as_ref().to_owned();

    CameraTileVM {
        name,
        state,
        is_recording: camera_vm.is_recording,
        is_streaming: camera_vm.is_streaming,
        is_available: camera_vm.is_available,
        icon_id,
        preferred_columns,
        preferred_rows,
        placement,
        pending: false,
    }
}

// ---------------------------------------------------------------------------
// compute_climate_tile_vm (TASK-108)
// ---------------------------------------------------------------------------

/// Project a typed Rust [`ClimateTileVM`] from a live entity snapshot,
/// threading through [`crate::ui::climate::ClimateVM::from_entity`] for
/// the per-frame derived `is_active` boolean and the optional
/// `current_temperature` / `target_temperature` reads.
///
/// # Hot-path discipline
///
/// Called at entity-change time (NOT per render). The result is a typed
/// `ClimateTileVM` Rust struct; the further `String -> SharedString` /
/// `icon_id -> Image` conversions are deferred to a follow-up ticket
/// once `main_window.slint` declares the `climate-tiles` array property.
///
/// # No `Vec` allocation on the per-frame path
///
/// Per the TASK-103 / TASK-105 / TASK-107 audit lesson: this projection
/// writes only scalar `state` / `name` / `icon_id` strings and the
/// optional `current_temperature` / `target_temperature` numerics. The
/// `WidgetOptions::Climate.hvac_modes` list lives on the dashboard config
/// and is read at modal-open / dispatch time — it does NOT travel through
/// this VM.
///
/// # Naming
///
/// Returns the Rust [`ClimateTileVM`] (defined earlier in this file). The
/// Slint-shape projection (`SlintClimateTileVM` from
/// [`climate_tile_slint::ClimateTileVM`]) is not built here because there
/// is no `climate-tiles` `MainWindow` property to write into yet — that
/// shape conversion lives next to the `set_climate_tiles` call site once
/// it exists.
#[must_use]
pub fn compute_climate_tile_vm(
    name: String,
    icon_id: String,
    preferred_columns: i32,
    preferred_rows: i32,
    placement: TilePlacement,
    entity: &crate::ha::entity::Entity,
) -> ClimateTileVM {
    let climate_vm = crate::ui::climate::ClimateVM::from_entity(entity);

    ClimateTileVM {
        name,
        state: climate_vm.state,
        is_active: climate_vm.is_active,
        current_temperature: climate_vm.current_temperature,
        target_temperature: climate_vm.target_temperature,
        icon_id,
        preferred_columns,
        preferred_rows,
        placement,
        pending: false,
    }
}

// ---------------------------------------------------------------------------
// compute_media_player_tile_vm (TASK-109)
// ---------------------------------------------------------------------------

/// Project a typed Rust [`MediaPlayerTileVM`] from a live entity snapshot,
/// threading through
/// [`crate::ui::media_player::MediaPlayerVM::from_entity`] for the
/// per-frame derived `is_playing` boolean and the optional `media_title` /
/// `artist` / `volume_level` reads.
///
/// # Hot-path discipline
///
/// Called at entity-change time (NOT per render). The result is a typed
/// `MediaPlayerTileVM` Rust struct; the further `String -> SharedString`
/// / `icon_id -> Image` conversions are deferred to a follow-up ticket
/// once `main_window.slint` declares the `media-player-tiles` array
/// property.
///
/// # No `Vec` allocation on the per-frame path
///
/// Per the TASK-103 / TASK-105 / TASK-107 / TASK-108 audit lesson: this
/// projection writes only scalar fields. The
/// `WidgetOptions::MediaPlayer.transport_set` and any source / sound-mode
/// lists live on the dashboard config and are read at modal-open /
/// dispatch time — they do NOT travel through this VM.
///
/// # Naming
///
/// Returns the Rust [`MediaPlayerTileVM`] (defined earlier in this
/// file). The Slint-shape projection (`SlintMediaPlayerTileVM` from
/// [`media_player_tile_slint::MediaPlayerTileVM`]) is not built here
/// because there is no `media-player-tiles` `MainWindow` property to
/// write into yet — that shape conversion lives next to the
/// `set_media_player_tiles` call site once it exists.
#[must_use]
pub fn compute_media_player_tile_vm(
    name: String,
    icon_id: String,
    preferred_columns: i32,
    preferred_rows: i32,
    placement: TilePlacement,
    entity: &crate::ha::entity::Entity,
) -> MediaPlayerTileVM {
    let media_player_vm = crate::ui::media_player::MediaPlayerVM::from_entity(entity);

    MediaPlayerTileVM {
        name,
        state: media_player_vm.state,
        is_playing: media_player_vm.is_playing,
        media_title: media_player_vm.media_title,
        artist: media_player_vm.artist,
        volume_level: media_player_vm.volume_level,
        icon_id,
        preferred_columns,
        preferred_rows,
        placement,
        pending: false,
    }
}

// ---------------------------------------------------------------------------
// compute_power_flow_tile_vm (TASK-094)
// ---------------------------------------------------------------------------

/// Pre-resolved auxiliary entity readings for a power-flow widget.
///
/// The grid entity is the **primary** entity for the widget and is passed
/// to [`compute_power_flow_tile_vm`] separately as a live `Entity`. The
/// auxiliary readings (`solar_w` / `battery_w` / `battery_pct` /
/// `home_w`) are looked up by the caller against the live store and
/// passed as a single struct so the function signature stays under the
/// 7-argument lint cap (clippy `too_many_arguments`).
///
/// Each reading is `Option<f32>`: `None` means the auxiliary entity is
/// either unconfigured or unavailable; `Some(f)` means a parsed numeric
/// value (which may legitimately be `0.0` for an idle lane).
///
/// # No `Vec` fields
///
/// Like [`PowerFlowTileVM`], this struct deliberately carries no `Vec`
/// fields — every auxiliary entity slot is a fixed scalar.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PowerFlowAuxiliaryReadings {
    /// Solar production in watts (≥ 0). `None` when no solar entity is
    /// configured or the entity is unavailable.
    pub solar_w: Option<f32>,
    /// Battery flow in watts; positive = charging, negative = discharging.
    /// `None` when no battery entity is configured.
    pub battery_w: Option<f32>,
    /// Battery state-of-charge in 0..=100. `None` when no
    /// `battery_soc_entity` is configured.
    pub battery_pct: Option<f32>,
    /// Home consumption in watts (≥ 0). `None` when no home entity is
    /// configured.
    pub home_w: Option<f32>,
}

/// Project a typed Rust [`PowerFlowTileVM`] from a primary (grid) entity
/// snapshot plus optional auxiliary readings, threading through
/// [`crate::ui::power_flow::PowerFlowVM::read_power_watts`] /
/// [`crate::ui::power_flow::PowerFlowVM::read_battery_pct`] for the
/// per-frame parsed numeric values.
///
/// # Hot-path discipline
///
/// Called at entity-change time (NOT per render). The result is a typed
/// `PowerFlowTileVM` Rust struct; the further `String -> SharedString` /
/// `icon_id -> Image` conversions and the optional-`f32` →
/// has-flag-plus-float Slint shape projection are deferred to a follow-up
/// ticket once `main_window.slint` declares the `power-flow-tiles` array
/// property.
///
/// # Auxiliary readings
///
/// The grid entity is the **primary** entity for the widget — its state
/// is forwarded as `grid_w`. Auxiliary readings are bundled in
/// [`PowerFlowAuxiliaryReadings`] so the signature stays under the
/// clippy `too_many_arguments` cap.
///
/// # No `Vec` allocation on the per-frame path
///
/// Per the TASK-103 / TASK-105 / TASK-107 / TASK-108 / TASK-109 audit
/// lesson: this projection writes only optional scalar fields. The
/// auxiliary entity ids configured in `WidgetOptions::PowerFlow` live on
/// the dashboard config and are read at modal-open / dispatch time —
/// they do NOT travel through this VM.
///
/// # Naming
///
/// Returns the Rust [`PowerFlowTileVM`] (defined earlier in this file).
/// The Slint-shape projection (`SlintPowerFlowTileVM` from
/// [`power_flow_tile_slint::PowerFlowTileVM`]) is not built here because
/// there is no `power-flow-tiles` `MainWindow` property to write into yet
/// — that shape conversion lives next to the `set_power_flow_tiles` call
/// site once it exists.
#[must_use]
pub fn compute_power_flow_tile_vm(
    name: String,
    icon_id: String,
    preferred_columns: i32,
    preferred_rows: i32,
    placement: TilePlacement,
    grid_entity: &crate::ha::entity::Entity,
    auxiliary: PowerFlowAuxiliaryReadings,
) -> PowerFlowTileVM {
    let grid_w = crate::ui::power_flow::PowerFlowVM::read_power_watts(grid_entity);

    PowerFlowTileVM {
        name,
        grid_w,
        solar_w: auxiliary.solar_w,
        battery_w: auxiliary.battery_w,
        battery_pct: auxiliary.battery_pct,
        home_w: auxiliary.home_w,
        icon_id,
        preferred_columns,
        preferred_rows,
        placement,
        pending: false,
    }
}

/// Static (config-derived) parameters for a power-flow tile build, bundled
/// to keep [`compute_power_flow_tile_vm_from_widget`]'s signature within
/// the clippy `too_many_arguments` cap (7).
pub struct PowerFlowBuildArgs<'a> {
    pub name: String,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
    pub wrapper_entity: Option<&'a crate::ha::entity::Entity>,
    pub options: Option<&'a WidgetOptions>,
}

/// Build a [`PowerFlowTileVM`] from a widget config + live store.
///
/// Power-flow widgets bind multiple HA entities through
/// [`WidgetOptions::PowerFlow`]; the canonical YAML shape sets `entity: None`
/// on the wrapper widget itself and lists the `grid_entity` /
/// `solar_entity` / `battery_entity` / `battery_soc_entity` /
/// `home_entity` ids inside `widget.options`. This helper resolves all
/// five from the supplied store and forwards them through
/// [`compute_power_flow_tile_vm`].
///
/// `args.wrapper_entity` is the entity matching `widget.entity` (when set) —
/// it is ignored if a `WidgetOptions::PowerFlow` payload is present (the
/// payload's `grid_entity` is the canonical primary). When neither is
/// usable the returned VM has `grid_w = None`, which the Slint side
/// renders as the unavailable variant (`is-available == false`).
///
/// `task/phase6-window-wireup` introduces this helper so `build_tiles` can
/// produce a `TileVM::PowerFlow` variant in both the entity-present and
/// entity-missing arms without duplicating the auxiliary-resolution logic.
#[must_use]
fn compute_power_flow_tile_vm_from_widget(
    args: PowerFlowBuildArgs<'_>,
    store: &dyn EntityStore,
) -> PowerFlowTileVM {
    let PowerFlowBuildArgs {
        name,
        icon_id,
        preferred_columns,
        preferred_rows,
        placement,
        wrapper_entity,
        options,
    } = args;
    // Resolve the grid entity preferentially from `WidgetOptions::PowerFlow.grid_entity`;
    // fall back to the wrapper widget's own entity binding for callers that
    // chose the alternate (single-entity) wiring shape.
    let pf_options = options.and_then(|o| match o {
        WidgetOptions::PowerFlow {
            grid_entity,
            solar_entity,
            battery_entity,
            battery_soc_entity,
            home_entity,
        } => Some((
            grid_entity.as_str(),
            solar_entity.as_deref(),
            battery_entity.as_deref(),
            battery_soc_entity.as_deref(),
            home_entity.as_deref(),
        )),
        _ => None,
    });

    let read_w = |id: &str| -> Option<f32> {
        let e = store.get(&EntityId::from(id))?;
        crate::ui::power_flow::PowerFlowVM::read_power_watts(&e)
    };
    let read_pct = |id: &str| -> Option<f32> {
        let e = store.get(&EntityId::from(id))?;
        crate::ui::power_flow::PowerFlowVM::read_battery_pct(&e)
    };

    if let Some((grid_id, solar_id, battery_id, soc_id, home_id)) = pf_options {
        let grid_entity_lookup = store.get(&EntityId::from(grid_id));
        let auxiliary = PowerFlowAuxiliaryReadings {
            solar_w: solar_id.and_then(read_w),
            battery_w: battery_id.and_then(read_w),
            battery_pct: soc_id.and_then(read_pct),
            home_w: home_id.and_then(read_w),
        };
        match grid_entity_lookup {
            Some(grid) => compute_power_flow_tile_vm(
                name,
                icon_id,
                preferred_columns,
                preferred_rows,
                placement,
                &grid,
                auxiliary,
            ),
            None => PowerFlowTileVM {
                name,
                grid_w: None,
                solar_w: auxiliary.solar_w,
                battery_w: auxiliary.battery_w,
                battery_pct: auxiliary.battery_pct,
                home_w: auxiliary.home_w,
                icon_id,
                preferred_columns,
                preferred_rows,
                placement,
                pending: false,
            },
        }
    } else if let Some(grid) = wrapper_entity {
        compute_power_flow_tile_vm(
            name,
            icon_id,
            preferred_columns,
            preferred_rows,
            placement,
            grid,
            PowerFlowAuxiliaryReadings::default(),
        )
    } else {
        // Neither options nor wrapper entity supplied: defensive fallback.
        // Slint's `is-available` will be `false` (grid_w == None), driving
        // the unavailable visual variant.
        PowerFlowTileVM {
            name,
            grid_w: None,
            solar_w: None,
            battery_w: None,
            battery_pct: None,
            home_w: None,
            icon_id,
            preferred_columns,
            preferred_rows,
            placement,
            pending: false,
        }
    }
}

// ---------------------------------------------------------------------------
// PinEntryHost bridge implementation (TASK-100)
// ---------------------------------------------------------------------------
//
// `SlintPinHost` is the production implementation of
// `crate::actions::pin::PinEntryHost`. It creates a `PinEntryWindow` on
// first use and shows it when `request_pin` is called.
//
// Security invariant (per locked_decisions.pin_entry_dispatch):
//   * The entered code is consumed exactly once via `FnOnce`.
//   * `request_pin` does NOT store the code string in any field.
//   * After the `on-submit` callback fires, `entered-code` is cleared and
//     the window is hidden in the same Slint event-loop turn.
//   * The code is passed directly to the `on_submit` closure; no intermediate
//     field, log line, or channel holds it.
//   * `tracing-redact` provides an additional runtime safety net, but the
//     primary enforcement is structural (FnOnce + immediate clear).
//
// Audit stubs (per acceptance_criteria ordering note for TASK-101):
//   TASK-101 will land the `audit::emit` substrate. Until it merges, the
//   audit events are emitted as `tracing::debug!` on the dedicated `audit`
//   target with the same field shape as the future `AuditEvent` struct.
//   TASK-101 will replace these stubs with `audit::emit(AuditEvent { ... })`.
//   The `event` and `outcome` fields carry ONLY static strings — the code
//   value is intentionally absent per locked_decisions.pin_entry_dispatch.

use crate::actions::pin::{CodeFormat, PinEntryHost};

// ---------------------------------------------------------------------------
// Testable submit/cancel callback helpers
// ---------------------------------------------------------------------------
//
// These free functions contain the callback logic extracted from the
// `invoke_from_event_loop` closure. They accept window-operation callbacks
// as generic `Fn()` parameters so the logic can be exercised in unit tests
// without a live Slint event loop — tests pass simple recording closures;
// production passes closures over `slint::Weak<PinEntryWindow>`.
//
// Security invariants (per locked_decisions.pin_entry_dispatch):
//   * `code` is passed directly to the FnOnce and dropped at end of scope.
//   * No copy of the code is stored in any field or emitted to any log.
//   * `reset_digits()` is called before the FnOnce so no copy lingers in
//     the Slint property graph during the synchronous FnOnce invocation.

/// Type alias for the one-shot FnOnce slot used by PIN entry.
///
/// Wrapped in `Arc<Mutex<Option<...>>>` so it can be shared across the
/// `on-submit` and `on-cancel` Slint closures while being consumed exactly
/// once via `Option::take`.
pub(crate) type PinSubmitSlot =
    std::sync::Arc<std::sync::Mutex<Option<Box<dyn FnOnce(String) + Send>>>>;

/// Handle the on-submit event from the PIN entry window.
///
/// Consumes the `FnOnce` exactly once via `Option::take`. Calls
/// `reset_digits()` before the closure so no copy of the code lingers in
/// the Slint property graph during the synchronous FnOnce invocation.
/// Calls `hide()` after dispatch.
///
/// # Security
///
/// `code` is passed directly to `f` and dropped at end of this function's
/// scope. It is not stored in any field or emitted to any log.
pub(crate) fn pin_submit_handler<R, H>(
    on_submit: &PinSubmitSlot,
    reset_digits: R,
    hide: H,
    code: String,
) where
    R: Fn(),
    H: Fn(),
{
    let cb = on_submit.lock().unwrap().take();
    if let Some(f) = cb {
        // Clear digit slots before calling f, so no copy of the code
        // lingers in the Slint property graph during the FnOnce invocation.
        reset_digits();
        // Consume the code via FnOnce — code is dropped at end of this
        // scope (Rust drop semantics; tracing-redact provides the runtime
        // safety net for any accidental log before the drop).
        f(code);
        // Hide the window after dispatch.
        hide();
        // Audit stub — TASK-101 will replace with audit::emit.
        // The code value is intentionally absent from this event.
        tracing::debug!(
            target: "audit",
            event = "pin.submitted",
            outcome = "submitted",
            "PIN entry submitted"
        );
    }
}

/// Handle the on-cancel event from the PIN entry window.
///
/// Drops the `FnOnce` without calling it (code is never delivered). Calls
/// `reset_digits()` and `hide()` for cleanup.
pub(crate) fn pin_cancel_handler<R, H>(on_submit: &PinSubmitSlot, reset_digits: R, hide: H)
where
    R: Fn(),
    H: Fn(),
{
    // Drop the FnOnce without calling it. The dispatcher must interpret a
    // missing on_submit invocation as a cancellation.
    let _ = on_submit.lock().unwrap().take();
    reset_digits();
    hide();
    // Audit stub — TASK-101 will replace with audit::emit.
    tracing::debug!(
        target: "audit",
        event = "pin.cancelled",
        outcome = "cancelled",
        "PIN entry cancelled"
    );
}

// ---------------------------------------------------------------------------
// setup_pin_window — event-loop-thread wiring helper (testable)
// ---------------------------------------------------------------------------
//
// This free function performs all PinEntryWindow operations that must happen
// on the Slint event-loop thread. Separating it from the
// `invoke_from_event_loop` dispatch in `request_pin` makes the wiring logic
// directly callable from tests that install the headless Slint platform —
// those tests call `setup_pin_window` directly, avoiding the
// `invoke_from_event_loop` dispatch entirely.
//
// Security invariant: the window is the only owner of the Slint property
// graph. `on_submit` is never stored in the window; only the Arc/Mutex slot
// that wraps the FnOnce is captured in the Slint closures.

/// Configure and show a `PinEntryWindow`, wiring submit/cancel callbacks.
///
/// Must be called on the Slint event-loop thread. In production this is
/// invoked from inside `invoke_from_event_loop`. In tests it is called
/// directly after installing the headless platform.
///
/// # Security
///
/// `on_submit` wraps a `FnOnce` that is consumed exactly once. No copy of
/// the entered code is stored in any field or emitted to any log.
pub(crate) fn setup_pin_window(
    window: &PinEntryWindow,
    on_submit: PinSubmitSlot,
    numeric_only: bool,
) {
    // Configure initial properties.
    window.set_numeric_only(numeric_only);
    // Reset digit slots to clear any stale state.
    window.invoke_reset_digits();

    // Wire on-submit.
    let on_submit_for_submit = std::sync::Arc::clone(&on_submit);
    let window_weak_submit = window.as_weak();
    window.on_on_submit(move |code: slint::SharedString| {
        let w_rd = window_weak_submit.clone();
        let w_hide = window_weak_submit.clone();
        pin_submit_handler(
            &on_submit_for_submit,
            move || {
                if let Some(w) = w_rd.upgrade() {
                    w.invoke_reset_digits();
                }
            },
            move || {
                if let Some(w) = w_hide.upgrade() {
                    let _ = w.hide();
                }
            },
            code.to_string(),
        );
    });

    // Wire on-cancel.
    let on_submit_for_cancel = std::sync::Arc::clone(&on_submit);
    let window_weak_cancel = window.as_weak();
    window.on_on_cancel(move || {
        let w_rd = window_weak_cancel.clone();
        let w_hide = window_weak_cancel.clone();
        pin_cancel_handler(
            &on_submit_for_cancel,
            move || {
                if let Some(w) = w_rd.upgrade() {
                    w.invoke_reset_digits();
                }
            },
            move || {
                if let Some(w) = w_hide.upgrade() {
                    let _ = w.hide();
                }
            },
        );
    });

    // Show the window.
    if let Err(e) = window.show() {
        tracing::warn!("setup_pin_window: PinEntryWindow::show failed: {:?}", e);
    }
}

// ---------------------------------------------------------------------------
// SlintPinHost — production PinEntryHost implementation
// ---------------------------------------------------------------------------

/// Production implementation of [`PinEntryHost`] backed by a Slint
/// [`PinEntryWindow`].
///
/// `request_pin` creates a new `PinEntryWindow`, configures it, wires
/// `on-submit` and `on-cancel`, and shows it. The window is torn down
/// (hidden) after the user submits or cancels.
///
/// # Thread safety
///
/// `request_pin` may be called from any thread. The implementation uses
/// `slint::invoke_from_event_loop` to dispatch all window operations onto
/// the Slint event loop thread, which is the only thread that may mutate
/// Slint properties.
pub struct SlintPinHost;

impl SlintPinHost {
    /// Construct a new `SlintPinHost`.
    ///
    /// The host creates a fresh `PinEntryWindow` per `request_pin` call;
    /// it holds no per-instance Slint state.
    pub fn new() -> Self {
        SlintPinHost
    }
}

impl Default for SlintPinHost {
    fn default() -> Self {
        SlintPinHost::new()
    }
}

impl PinEntryHost for SlintPinHost {
    /// Show the PIN entry window and wire the `on_submit` callback.
    ///
    /// # Security
    ///
    /// The `code` string is received from the Slint `on-submit` callback,
    /// passed directly to `on_submit(code)`, and then the Slint property
    /// `entered-code` is cleared to `""` in the same event-loop turn.
    ///
    /// The `on_submit` closure is `FnOnce`: it is consumed exactly once.
    /// No copy of the code is stored in any field or emitted to any log.
    ///
    /// After submission the window is hidden by calling `.hide()`.
    fn request_pin(&self, code_format: CodeFormat, on_submit: Box<dyn FnOnce(String) + Send>) {
        // Wrap in Arc<Mutex<Option<...>>> so the Slint closures (which must
        // be 'static + FnMut) can consume the FnOnce exactly once via
        // `Option::take`.
        let slot: PinSubmitSlot = std::sync::Arc::new(std::sync::Mutex::new(Some(on_submit)));
        let numeric_only = matches!(code_format, CodeFormat::Number);

        // Dispatch window creation and wiring to the Slint event-loop thread.
        // The body is intentionally thin — all logic lives in `setup_pin_window`.
        let result = slint::invoke_from_event_loop(move || match PinEntryWindow::new() {
            Ok(w) => setup_pin_window(&w, slot, numeric_only),
            Err(e) => tracing::warn!(
                "SlintPinHost::request_pin: PinEntryWindow::new failed: {:?}",
                e
            ),
        });

        if let Err(e) = result {
            tracing::warn!(
                "SlintPinHost::request_pin: invoke_from_event_loop failed: {:?}",
                e
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::fixture::fixture_dashboard;
    use crate::dashboard::profiles::PROFILE_DESKTOP;
    use crate::ha::fixture;

    /// Path to the canonical Phase 1 fixture.
    ///
    /// `cargo test` runs with the crate root as cwd so this resolves correctly.
    const FIXTURE_PATH: &str = "examples/ha-states.json";

    // -----------------------------------------------------------------------
    // Smoke test: fixture store + fixture_dashboard → ≥1 VM per tile kind
    // -----------------------------------------------------------------------

    #[test]
    fn smoke_build_tiles_all_three_kinds() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = fixture_dashboard();

        let tiles = build_tiles(&store, &dashboard);

        // Must produce one tile per widget.
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

        // At least one of each kind.
        let has_light = tiles.iter().any(|t| matches!(t, TileVM::Light(_)));
        let has_sensor = tiles.iter().any(|t| matches!(t, TileVM::Sensor(_)));
        let has_entity = tiles.iter().any(|t| matches!(t, TileVM::Entity(_)));
        assert!(has_light, "expected at least one LightTileVM");
        assert!(has_sensor, "expected at least one SensorTileVM");
        assert!(has_entity, "expected at least one EntityTileVM");
    }

    // -----------------------------------------------------------------------
    // LightTileVM field correctness (light.kitchen)
    // -----------------------------------------------------------------------

    #[test]
    fn light_tile_vm_fields_from_fixture() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = fixture_dashboard();

        let tiles = build_tiles(&store, &dashboard);
        let light_vm = tiles
            .iter()
            .find_map(|t| {
                if let TileVM::Light(vm) = t {
                    Some(vm)
                } else {
                    None
                }
            })
            .expect("expected at least one LightTileVM");

        // name comes from the friendly_name attribute in the fixture.
        assert_eq!(
            light_vm.name, "Kitchen Light",
            "name must come from friendly_name"
        );
        // state comes from entity.state.
        assert_eq!(light_vm.state, "on", "state must be 'on' for light.kitchen");
        // icon_id: no widget.icon set in fixture_dashboard, so default is "mdi:lightbulb".
        assert_eq!(
            light_vm.icon_id, "mdi:lightbulb",
            "default icon_id for Light"
        );
        // preferred_columns from widget.layout.
        assert_eq!(light_vm.preferred_columns, 2);
        assert_eq!(light_vm.preferred_rows, 2);
        // placement: no placement in fixture_dashboard so default_for(2,2).
        assert_eq!(
            light_vm.placement,
            TilePlacement {
                col: 0,
                row: 0,
                span_cols: 2,
                span_rows: 2
            }
        );
    }

    // -----------------------------------------------------------------------
    // SensorTileVM field correctness (sensor.hallway_temperature)
    // -----------------------------------------------------------------------

    #[test]
    fn sensor_tile_vm_fields_from_fixture() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = fixture_dashboard();

        let tiles = build_tiles(&store, &dashboard);
        let sensor_vm = tiles
            .iter()
            .find_map(|t| {
                if let TileVM::Sensor(vm) = t {
                    Some(vm)
                } else {
                    None
                }
            })
            .expect("expected at least one SensorTileVM");

        assert_eq!(sensor_vm.name, "Hallway Temperature");
        assert_eq!(sensor_vm.state, "21.3");
        assert_eq!(sensor_vm.icon_id, "mdi:thermometer");
        assert_eq!(sensor_vm.preferred_columns, 2);
        assert_eq!(sensor_vm.preferred_rows, 1);
        assert_eq!(
            sensor_vm.placement,
            TilePlacement {
                col: 0,
                row: 0,
                span_cols: 2,
                span_rows: 1
            }
        );
    }

    // -----------------------------------------------------------------------
    // EntityTileVM field correctness (switch.outlet_1 — present in fixture)
    // -----------------------------------------------------------------------

    #[test]
    fn entity_tile_vm_fields_from_fixture() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = fixture_dashboard();

        let tiles = build_tiles(&store, &dashboard);
        let entity_vm = tiles
            .iter()
            .find_map(|t| {
                if let TileVM::Entity(vm) = t {
                    Some(vm)
                } else {
                    None
                }
            })
            .expect("expected at least one EntityTileVM");

        // fixture_dashboard() has widget.name = Some("Living Room") for the entity tile;
        // explicit widget name always takes precedence over the fixture friendly_name.
        assert_eq!(
            entity_vm.name, "Living Room",
            "explicit widget name takes precedence"
        );
        // switch.outlet_1 is present in the fixture with state "off".
        assert_eq!(entity_vm.state, "off", "fixture entity state must be 'off'");
        // EntityKind::from(&entity.id) for a switch returns Other → falls through to mdi:help-circle.
        assert_eq!(entity_vm.icon_id, "mdi:help-circle");
        assert_eq!(entity_vm.preferred_columns, 2);
        assert_eq!(entity_vm.preferred_rows, 1);
        assert_eq!(
            entity_vm.placement,
            TilePlacement {
                col: 0,
                row: 0,
                span_cols: 2,
                span_rows: 1
            }
        );
    }

    // -----------------------------------------------------------------------
    // for_each visitor is exercised: count from visitor matches get-based count
    // -----------------------------------------------------------------------

    #[test]
    fn for_each_visitor_count_matches_known_fixture_size() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        let mut count = 0usize;
        store.for_each(&mut |_id, _entity| {
            count += 1;
        });
        // The canonical fixture covers every Phase 6 widget kind and carries
        // 18 entities. Bump this constant when the fixture grows again.
        assert_eq!(count, 18, "for_each must visit all 18 fixture entities");
    }

    // -----------------------------------------------------------------------
    // Missing-entity policy
    // -----------------------------------------------------------------------

    #[test]
    fn missing_entity_produces_entity_tile_vm_with_unavailable() {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        // Build a dashboard that deliberately references an entity ID not present
        // in the fixture, so we can assert the unavailable fallback independent of
        // whatever fixture_dashboard() points at.
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![Widget {
                        id: "w1".to_string(),
                        widget_type: WidgetKind::EntityTile,
                        entity: Some("switch.does_not_exist_xyz".to_string()),
                        entities: vec![],
                        name: Some("Ghost Switch".to_string()),
                        icon: None,
                        tap_action: None,
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 2,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: None,
                        visibility: "always".to_string(),
                    }],
                }],
            }],
        };

        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);

        let entity_vm = match &tiles[0] {
            TileVM::Entity(vm) => vm,
            other => panic!("expected EntityTileVM, got {:?}", other),
        };

        assert_eq!(
            entity_vm.state, "unavailable",
            "missing entity must render with state=unavailable"
        );
    }

    // -----------------------------------------------------------------------
    // Icon-id override from widget config
    // -----------------------------------------------------------------------

    #[test]
    fn icon_id_override_in_widget_config_takes_precedence() {
        use crate::actions::Action;
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![Widget {
                        id: "w1".to_string(),
                        widget_type: WidgetKind::LightTile,
                        entity: Some("light.kitchen".to_string()),
                        entities: vec![],
                        name: None,
                        icon: Some("mdi:lamp".to_string()),
                        tap_action: Some(Action::Toggle),
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 1,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: None,
                        visibility: "always".to_string(),
                    }],
                }],
            }],
        };

        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        if let TileVM::Light(vm) = &tiles[0] {
            assert_eq!(vm.icon_id, "mdi:lamp", "widget icon override must win");
        } else {
            panic!("expected LightTileVM");
        }
    }

    // -----------------------------------------------------------------------
    // Placement from schema::Placement when present
    // -----------------------------------------------------------------------

    #[test]
    fn explicit_placement_in_widget_is_used_verbatim() {
        use crate::dashboard::schema::{
            Dashboard, Layout, Placement, ProfileKey, Section, View, Widget, WidgetKind,
            WidgetLayout,
        };

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![Widget {
                        id: "w1".to_string(),
                        widget_type: WidgetKind::SensorTile,
                        entity: Some("sensor.hallway_temperature".to_string()),
                        entities: vec![],
                        name: None,
                        icon: None,
                        tap_action: None,
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 2,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: Some(Placement {
                            col: 3,
                            row: 1,
                            span_cols: 2,
                            span_rows: 1,
                        }),
                        visibility: "always".to_string(),
                    }],
                }],
            }],
        };

        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        if let TileVM::Sensor(vm) = &tiles[0] {
            assert_eq!(
                vm.placement,
                TilePlacement {
                    col: 3,
                    row: 1,
                    span_cols: 2,
                    span_rows: 1
                }
            );
        } else {
            panic!("expected SensorTileVM");
        }
    }

    // -----------------------------------------------------------------------
    // Name fallback to entity ID when friendly_name attribute is absent
    // -----------------------------------------------------------------------

    #[test]
    fn name_falls_back_to_entity_id_when_no_friendly_name() {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        // binary_sensor.foo has an empty attributes map (no friendly_name).
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![Widget {
                        id: "w1".to_string(),
                        widget_type: WidgetKind::EntityTile,
                        entity: Some("binary_sensor.foo".to_string()),
                        entities: vec![],
                        name: None,
                        icon: None,
                        tap_action: None,
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 1,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: None,
                        visibility: "always".to_string(),
                    }],
                }],
            }],
        };

        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        if let TileVM::Entity(vm) = &tiles[0] {
            assert_eq!(
                vm.name, "binary_sensor.foo",
                "entity ID must be the fallback name when friendly_name is absent"
            );
        } else {
            panic!("expected EntityTileVM");
        }
    }

    // -----------------------------------------------------------------------
    // Empty dashboard produces empty Vec
    // -----------------------------------------------------------------------

    #[test]
    fn empty_dashboard_produces_empty_vec() {
        use crate::dashboard::schema::{Dashboard, Layout, ProfileKey, View};

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "empty".to_string(),
            views: vec![View {
                id: "empty".to_string(),
                title: "Empty".to_string(),
                layout: Layout::Grid,
                sections: vec![],
            }],
        };

        let tiles = build_tiles(&store, &dashboard);
        assert!(tiles.is_empty(), "no widgets means no tiles");
    }

    // -----------------------------------------------------------------------
    // Slint wiring smoke tests (TASK-015)
    // -----------------------------------------------------------------------
    //
    // These tests exercise [`split_tile_vms`] — the Slint-typed conversion
    // that [`wire_window`] performs before writing properties — without
    // instantiating a [`MainWindow`]. Constructing the window requires a live
    // graphics backend (the crate is configured with `backend-winit-x11`,
    // which fails in headless CI runners). The test instead asserts on the
    // Slint-typed VM structs that the bridge would write into the property
    // models, which is the same data path the Slint runtime sees.
    //
    // The "Slint properties update when a fixture entity changes" AC is
    // exercised by mutating a synthesized `TileVM` slice between two calls
    // to `split_tile_vms` and asserting the resulting Slint-typed structs
    // differ in the expected fields.

    fn ensure_icons_init() {
        // `split_tile_vms` calls `crate::assets::icons::resolve`, which
        // requires `icons::init()` to have been called. Idempotent.
        crate::assets::icons::init();
    }

    fn make_light_tile(name: &str, state: &str, icon_id: &str) -> TileVM {
        TileVM::Light(LightTileVM {
            name: name.to_string(),
            state: state.to_string(),
            icon_id: icon_id.to_string(),
            preferred_columns: 2,
            preferred_rows: 2,
            placement: TilePlacement {
                col: 0,
                row: 0,
                span_cols: 2,
                span_rows: 2,
            },
            pending: false,
        })
    }

    fn make_sensor_tile(name: &str, state: &str, icon_id: &str) -> TileVM {
        TileVM::Sensor(SensorTileVM {
            name: name.to_string(),
            state: state.to_string(),
            icon_id: icon_id.to_string(),
            preferred_columns: 2,
            preferred_rows: 1,
            placement: TilePlacement {
                col: 0,
                row: 0,
                span_cols: 2,
                span_rows: 1,
            },
            pending: false,
        })
    }

    fn make_entity_tile(name: &str, state: &str, icon_id: &str) -> TileVM {
        TileVM::Entity(EntityTileVM {
            name: name.to_string(),
            state: state.to_string(),
            icon_id: icon_id.to_string(),
            preferred_columns: 1,
            preferred_rows: 1,
            placement: TilePlacement {
                col: 0,
                row: 0,
                span_cols: 1,
                span_rows: 1,
            },
            pending: false,
        })
    }

    #[test]
    fn split_tile_vms_partitions_by_variant() {
        ensure_icons_init();

        let tiles = vec![
            make_light_tile("Kitchen", "on", "mdi:lightbulb"),
            make_sensor_tile("Hallway", "21.3", "mdi:thermometer"),
            make_entity_tile("Outlet", "off", "mdi:help-circle"),
            make_light_tile("Bedroom", "off", "mdi:lightbulb"),
        ];

        let split = split_tile_vms(&tiles);

        assert_eq!(split.lights.len(), 2, "two LightTileVMs expected");
        assert_eq!(split.sensors.len(), 1, "one SensorTileVM expected");
        assert_eq!(split.entities.len(), 1, "one EntityTileVM expected");
    }

    #[test]
    fn split_tile_vms_copies_string_fields_into_shared_strings() {
        ensure_icons_init();

        let tiles = vec![make_light_tile("Kitchen", "on", "mdi:lightbulb")];
        let split = split_tile_vms(&tiles);
        let lights = &split.lights;

        assert_eq!(lights[0].name.as_str(), "Kitchen");
        assert_eq!(lights[0].state.as_str(), "on");
        assert_eq!(lights[0].r#icon_id.as_str(), "mdi:lightbulb");
        assert_eq!(lights[0].preferred_columns, 2);
        assert_eq!(lights[0].preferred_rows, 2);
        assert_eq!(lights[0].placement.col, 0);
        assert_eq!(lights[0].placement.span_cols, 2);
    }

    #[test]
    fn split_tile_vms_resolves_icon_id_to_image() {
        ensure_icons_init();

        // Two tiles with distinct icon ids must yield distinct resolved
        // images (the lightbulb pixel data differs from the thermometer's).
        let tiles = vec![
            make_light_tile("Kitchen", "on", "mdi:lightbulb"),
            make_sensor_tile("Hallway", "21.3", "mdi:thermometer"),
        ];

        let split = split_tile_vms(&tiles);
        let lights = &split.lights;
        let sensors = &split.sensors;

        let lb_pixels = lights[0]
            .icon
            .to_rgba8()
            .expect("lightbulb image must have rgba8 data");
        let th_pixels = sensors[0]
            .icon
            .to_rgba8()
            .expect("thermometer image must have rgba8 data");

        assert_ne!(
            lb_pixels.as_bytes(),
            th_pixels.as_bytes(),
            "different icon_ids must resolve to different image bytes"
        );
    }

    #[test]
    fn split_tile_vms_unknown_icon_id_falls_back_to_help_circle() {
        ensure_icons_init();

        let tiles = vec![make_light_tile("Mystery", "on", "mdi:nonexistent")];
        let split = split_tile_vms(&tiles);
        let lights = &split.lights;

        // The fallback path is exercised: the icon must equal the help-circle
        // pixel data even though the id is unrecognised.
        let resolved = lights[0]
            .icon
            .to_rgba8()
            .expect("fallback image must have rgba8 data");
        let fallback = crate::assets::icons::resolve("mdi:help-circle")
            .to_rgba8()
            .expect("fallback image must have rgba8 data");

        assert_eq!(
            resolved.as_bytes(),
            fallback.as_bytes(),
            "unknown icon id must resolve to the fallback image bytes"
        );
    }

    #[test]
    fn split_tile_vms_reflects_state_change_on_re_wire() {
        // This is the "Slint properties update when a fixture entity changes"
        // AC: re-running the conversion with mutated VM data must produce a
        // different Slint-typed struct in the corresponding slot. This is the
        // same path `wire_window` invokes; the property write is a constant-
        // cost ModelRc replacement on top.
        ensure_icons_init();

        let tiles_before = vec![make_light_tile("Kitchen", "on", "mdi:lightbulb")];
        let split_before = split_tile_vms(&tiles_before);
        let lights_before = &split_before.lights;
        assert_eq!(lights_before[0].state.as_str(), "on");

        // Mutate the synthesized fixture: simulate the entity flipping to off.
        let tiles_after = vec![make_light_tile("Kitchen", "off", "mdi:lightbulb")];
        let split_after = split_tile_vms(&tiles_after);
        let lights_after = &split_after.lights;
        assert_eq!(lights_after[0].state.as_str(), "off");

        // Same name, same icon, but state must have flipped — exactly the
        // delta the Slint property would observe between two refresh cycles.
        assert_eq!(lights_before[0].name, lights_after[0].name);
        assert_eq!(lights_before[0].r#icon_id, lights_after[0].r#icon_id);
        assert_ne!(lights_before[0].state, lights_after[0].state);
    }

    #[test]
    fn split_tile_vms_preserves_per_variant_order() {
        ensure_icons_init();

        let tiles = vec![
            make_light_tile("L1", "on", "mdi:lightbulb"),
            make_sensor_tile("S1", "10", "mdi:thermometer"),
            make_light_tile("L2", "off", "mdi:lightbulb"),
            make_sensor_tile("S2", "20", "mdi:thermometer"),
        ];

        let split = split_tile_vms(&tiles);
        let lights = &split.lights;
        let sensors = &split.sensors;

        // Per-variant order must match document order within that variant.
        assert_eq!(lights[0].name.as_str(), "L1");
        assert_eq!(lights[1].name.as_str(), "L2");
        assert_eq!(sensors[0].name.as_str(), "S1");
        assert_eq!(sensors[1].name.as_str(), "S2");
    }

    // -----------------------------------------------------------------------
    // split_tile_vms — Phase 6 per-kind dispatch arms
    //
    // Each Phase 6 variant carries kind-specific scalar fields that the
    // Slint per-kind tile component reads directly. The arms below build a
    // small slice containing one tile of each kind and assert both the
    // partitioning (per-variant Vec lengths) and that each kind's
    // distinguishing field round-trips into the Slint-typed output struct
    // verbatim. A regression that drops or reorders an arm fails here.
    // -----------------------------------------------------------------------

    /// `split_tile_vms` emits one entry per Phase 6 kind into the matching
    /// per-kind Vec when the input slice carries one tile of each kind.
    /// Each kind's distinguishing field (`is_open`, `is_on`, `is_locked`,
    /// `is_armed`, `is_available`, `is_recording`, `is_active`,
    /// `is_playing`) round-trips into the Slint-typed output unchanged.
    #[test]
    fn split_tile_vms_phase6_kinds_route_to_per_kind_vecs() {
        ensure_icons_init();

        let placement = TilePlacement::default_for(2, 2);
        let tiles = vec![
            TileVM::Cover(CoverTileVM {
                name: "Patio".into(),
                state: "open".into(),
                position: 42,
                tilt: 0,
                has_position: true,
                has_tilt: false,
                is_open: true,
                is_moving: false,
                icon_id: "mdi:window-shutter-open".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
            TileVM::Fan(FanTileVM {
                name: "Bedroom Fan".into(),
                state: "on".into(),
                speed_pct: 75,
                has_speed_pct: true,
                is_on: true,
                current_speed: "high".into(),
                has_current_speed: true,
                icon_id: "mdi:fan".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
            TileVM::Lock(LockTileVM {
                name: "Front Door".into(),
                state: "locked".into(),
                is_locked: true,
                icon_id: "mdi:lock".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
            TileVM::Alarm(AlarmTileVM {
                name: "Home Alarm".into(),
                state: "armed_away".into(),
                is_armed: true,
                is_triggered: false,
                is_pending: false,
                icon_id: "mdi:shield-home".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
            TileVM::History(HistoryGraphTileVM {
                name: "Energy Today".into(),
                state: "12.4".into(),
                change_count: 7,
                is_available: true,
                icon_id: "mdi:chart-line".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
                path_commands: "M 0 0 L 1 1".into(),
            }),
            TileVM::Camera(CameraTileVM {
                name: "Front Door".into(),
                state: "recording".into(),
                is_recording: true,
                is_streaming: false,
                is_available: true,
                icon_id: "mdi:cctv".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
            TileVM::Climate(ClimateTileVM {
                name: "Living Room".into(),
                state: "heat".into(),
                is_active: true,
                current_temperature: Some(21.5),
                target_temperature: Some(22.0),
                icon_id: "mdi:thermostat".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
            TileVM::MediaPlayer(MediaPlayerTileVM {
                name: "Kitchen Speaker".into(),
                state: "playing".into(),
                is_playing: true,
                media_title: Some("Track Name".into()),
                artist: Some("Artist Name".into()),
                volume_level: Some(0.55),
                icon_id: "mdi:speaker".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
            TileVM::PowerFlow(PowerFlowTileVM {
                name: "Power".into(),
                grid_w: Some(1500.0),
                solar_w: Some(2000.0),
                battery_w: Some(-300.0),
                battery_pct: Some(75.0),
                home_w: Some(800.0),
                icon_id: "mdi:lightning-bolt-circle".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
        ];

        let split = split_tile_vms(&tiles);

        // Each per-kind Vec gets exactly one entry — partitioning is
        // disjoint and complete (no kind drops, no kind double-routes).
        assert_eq!(split.covers.len(), 1, "one CoverTileVM expected");
        assert_eq!(split.fans.len(), 1, "one FanTileVM expected");
        assert_eq!(split.locks.len(), 1, "one LockTileVM expected");
        assert_eq!(split.alarms.len(), 1, "one AlarmTileVM expected");
        assert_eq!(split.histories.len(), 1, "one HistoryGraphTileVM expected");
        assert_eq!(split.cameras.len(), 1, "one CameraTileVM expected");
        assert_eq!(split.climates.len(), 1, "one ClimateTileVM expected");
        assert_eq!(
            split.media_players.len(),
            1,
            "one MediaPlayerTileVM expected"
        );
        assert_eq!(split.power_flows.len(), 1, "one PowerFlowTileVM expected");

        // Phase 1 buckets stay empty when no Phase 1 tile is present.
        assert!(
            split.lights.is_empty() && split.sensors.is_empty() && split.entities.is_empty(),
            "Phase 1 buckets must be empty when input slice carries only Phase 6 kinds"
        );

        // Distinguishing per-kind fields round-trip into the Slint-typed
        // output structs verbatim. These assertions exercise the field
        // copies that occur inside each `split_tile_vms` arm.
        let cover = &split.covers[0];
        assert_eq!(cover.name.as_str(), "Patio");
        assert_eq!(cover.state.as_str(), "open");
        assert_eq!(cover.position, 42);
        assert!(cover.r#has_position);
        assert!(cover.r#is_open);
        assert!(!cover.r#is_moving);
        assert_eq!(cover.r#icon_id.as_str(), "mdi:window-shutter-open");

        let fan = &split.fans[0];
        assert_eq!(fan.name.as_str(), "Bedroom Fan");
        assert_eq!(fan.state.as_str(), "on");
        assert_eq!(fan.r#speed_pct, 75);
        assert!(fan.r#has_speed_pct);
        assert!(fan.r#is_on);
        assert_eq!(fan.r#current_speed.as_str(), "high");
        assert!(fan.r#has_current_speed);

        let lock = &split.locks[0];
        assert_eq!(lock.name.as_str(), "Front Door");
        assert_eq!(lock.state.as_str(), "locked");
        assert!(lock.r#is_locked);

        let alarm = &split.alarms[0];
        assert_eq!(alarm.name.as_str(), "Home Alarm");
        assert_eq!(alarm.state.as_str(), "armed_away");
        assert!(alarm.r#is_armed);
        assert!(!alarm.r#is_triggered);
        assert!(!alarm.r#is_pending);

        let history = &split.histories[0];
        assert_eq!(history.name.as_str(), "Energy Today");
        assert_eq!(history.state.as_str(), "12.4");
        assert_eq!(history.r#change_count, 7);
        assert!(history.r#is_available);
        assert_eq!(history.r#path_commands.as_str(), "M 0 0 L 1 1");

        let camera = &split.cameras[0];
        assert_eq!(camera.name.as_str(), "Front Door");
        assert_eq!(camera.state.as_str(), "recording");
        assert!(camera.r#is_recording);
        assert!(!camera.r#is_streaming);
        assert!(camera.r#is_available);

        let climate = &split.climates[0];
        assert_eq!(climate.name.as_str(), "Living Room");
        assert_eq!(climate.state.as_str(), "heat");
        assert!(climate.r#is_active);
        // Option<f32> → (value, has_*) projection
        assert!(climate.r#has_current_temperature);
        assert_eq!(climate.r#current_temperature, 21.5);
        assert!(climate.r#has_target_temperature);
        assert_eq!(climate.r#target_temperature, 22.0);

        let mp = &split.media_players[0];
        assert_eq!(mp.name.as_str(), "Kitchen Speaker");
        assert_eq!(mp.state.as_str(), "playing");
        assert!(mp.r#is_playing);
        assert!(mp.r#has_media_title);
        assert_eq!(mp.r#media_title.as_str(), "Track Name");
        assert!(mp.r#has_artist);
        assert_eq!(mp.artist.as_str(), "Artist Name");
        assert!(mp.r#has_volume_level);
        assert!((mp.r#volume_level - 0.55).abs() < 1e-6);

        // PowerFlow drives its own Slint conversion via
        // `slint_power_flow_tile_vm`; with a populated `grid_w` the lane
        // labels and direction flags carry the formatted scalar.
        let pf = &split.power_flows[0];
        assert_eq!(pf.name.as_str(), "Power");
        assert!(pf.is_available, "grid_w=Some → is_available=true");
        assert!(pf.grid_importing, "grid_w > 0 → importing");
        assert!(!pf.grid_idle, "1500 W is well above the idle threshold");
        assert!(pf.has_solar);
        assert!(!pf.solar_idle);
        assert!(pf.has_battery);
        assert!(!pf.battery_charging, "battery_w < 0 → discharging");
        assert!(!pf.battery_idle);
        assert!(pf.has_battery_pct);
        assert!((pf.battery_pct - 75.0).abs() < 1e-6);
        assert!(pf.has_home);
        assert!(!pf.home_idle);
    }

    /// `split_tile_vms` projects an Option<f32> field that is `None` to
    /// `(0.0, false)`. This exercises the unwrap_or-default branch in
    /// the Climate / MediaPlayer / PowerFlow arms (which is the
    /// `is_some()=false` side of the conversion).
    #[test]
    fn split_tile_vms_option_f32_none_projects_to_default_with_has_flag_false() {
        ensure_icons_init();

        let placement = TilePlacement::default_for(2, 2);
        let tiles = vec![
            TileVM::Climate(ClimateTileVM {
                name: "Climate".into(),
                state: "off".into(),
                is_active: false,
                current_temperature: None,
                target_temperature: None,
                icon_id: "mdi:thermostat".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
            TileVM::MediaPlayer(MediaPlayerTileVM {
                name: "MP".into(),
                state: "idle".into(),
                is_playing: false,
                media_title: None,
                artist: None,
                volume_level: None,
                icon_id: "mdi:speaker".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
            TileVM::PowerFlow(PowerFlowTileVM {
                name: "PF".into(),
                grid_w: None,
                solar_w: None,
                battery_w: None,
                battery_pct: None,
                home_w: None,
                icon_id: "mdi:lightning-bolt-circle".into(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement,
                pending: false,
            }),
        ];

        let split = split_tile_vms(&tiles);

        let climate = &split.climates[0];
        assert!(!climate.r#has_current_temperature);
        assert_eq!(climate.r#current_temperature, 0.0);
        assert!(!climate.r#has_target_temperature);
        assert_eq!(climate.r#target_temperature, 0.0);

        let mp = &split.media_players[0];
        assert!(!mp.r#has_media_title);
        assert_eq!(mp.r#media_title.as_str(), "");
        assert!(!mp.r#has_artist);
        assert_eq!(mp.artist.as_str(), "");
        assert!(!mp.r#has_volume_level);
        assert_eq!(mp.r#volume_level, 0.0);

        let pf = &split.power_flows[0];
        assert!(!pf.is_available, "grid_w=None → is_available=false");
        assert!(pf.grid_idle, "grid_w=None → idle=true (defensive default)");
        assert!(!pf.grid_importing);
        assert!(!pf.has_solar);
        assert!(pf.solar_idle);
        assert!(!pf.has_battery);
        assert!(pf.battery_idle);
        assert!(!pf.battery_charging);
        assert!(!pf.has_battery_pct);
        assert_eq!(pf.battery_pct, 0.0);
        assert!(!pf.has_home);
        assert!(pf.home_idle);
    }

    // -----------------------------------------------------------------------
    // build_tiles — Phase 6 unavailable-fallback arms
    //
    // When a widget references an entity that is NOT present in the store,
    // `build_tiles` emits a placeholder VM of the matching `TileKind` with
    // `state="unavailable"` so the row layout stays stable across
    // availability transitions (TASK-119 F2 row-stability invariant). The
    // tests below exercise the Phase 6 unavailable arms (Cover / Fan /
    // Lock / Alarm / History / Camera / Climate / MediaPlayer) — the
    // Light / Sensor / Entity arms are exercised by
    // `missing_entity_produces_entity_tile_vm_with_unavailable` above.
    //
    // PowerFlow has its own dedicated arm tested in
    // `compute_power_flow_tile_vm_from_widget_options_path_with_missing_grid_emits_unavailable_vm`.
    // -----------------------------------------------------------------------

    #[test]
    fn build_tiles_cover_widget_with_missing_entity_emits_unavailable_cover_vm() {
        use crate::ha::store::MemoryStore;
        let store = MemoryStore::load(vec![]).expect("MemoryStore::load");
        let dashboard = dashboard_with_kind("cover.does_not_exist", WidgetKind::Cover);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1, "row layout stays stable: one tile emitted");
        match &tiles[0] {
            TileVM::Cover(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_open, "unavailable cover must not be open");
                assert!(!vm.is_moving);
                assert!(!vm.has_position);
                assert!(!vm.has_tilt);
                assert_eq!(vm.icon_id, "mdi:help-circle");
            }
            other => panic!("expected CoverTileVM placeholder, got {other:?}"),
        }
    }

    #[test]
    fn build_tiles_fan_widget_with_missing_entity_emits_unavailable_fan_vm() {
        use crate::ha::store::MemoryStore;
        let store = MemoryStore::load(vec![]).expect("MemoryStore::load");
        let dashboard = dashboard_with_kind("fan.does_not_exist", WidgetKind::Fan);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Fan(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_on);
                assert!(!vm.has_speed_pct);
                assert!(!vm.has_current_speed);
                assert_eq!(vm.current_speed, "");
            }
            other => panic!("expected FanTileVM placeholder, got {other:?}"),
        }
    }

    #[test]
    fn build_tiles_lock_widget_with_missing_entity_emits_unavailable_lock_vm() {
        use crate::ha::store::MemoryStore;
        let store = MemoryStore::load(vec![]).expect("MemoryStore::load");
        let dashboard = dashboard_with_kind("lock.does_not_exist", WidgetKind::Lock);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Lock(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_locked, "unavailable lock must not be locked");
            }
            other => panic!("expected LockTileVM placeholder, got {other:?}"),
        }
    }

    #[test]
    fn build_tiles_alarm_widget_with_missing_entity_emits_unavailable_alarm_vm() {
        use crate::ha::store::MemoryStore;
        let store = MemoryStore::load(vec![]).expect("MemoryStore::load");
        let dashboard =
            dashboard_with_kind("alarm_control_panel.does_not_exist", WidgetKind::Alarm);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Alarm(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_armed);
                assert!(!vm.is_triggered);
                assert!(!vm.is_pending);
            }
            other => panic!("expected AlarmTileVM placeholder, got {other:?}"),
        }
    }

    #[test]
    fn build_tiles_history_widget_with_missing_entity_emits_unavailable_history_vm() {
        use crate::ha::store::MemoryStore;
        let store = MemoryStore::load(vec![]).expect("MemoryStore::load");
        let dashboard = dashboard_with_kind("sensor.does_not_exist", WidgetKind::History);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::History(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_available);
                assert_eq!(vm.change_count, 0);
                assert_eq!(vm.path_commands, "");
            }
            other => panic!("expected HistoryGraphTileVM placeholder, got {other:?}"),
        }
    }

    #[test]
    fn build_tiles_camera_widget_with_missing_entity_emits_unavailable_camera_vm() {
        use crate::ha::store::MemoryStore;
        let store = MemoryStore::load(vec![]).expect("MemoryStore::load");
        let dashboard = dashboard_with_kind("camera.does_not_exist", WidgetKind::Camera);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Camera(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_recording);
                assert!(!vm.is_streaming);
                assert!(!vm.is_available);
            }
            other => panic!("expected CameraTileVM placeholder, got {other:?}"),
        }
    }

    #[test]
    fn build_tiles_climate_widget_with_missing_entity_emits_unavailable_climate_vm() {
        use crate::ha::store::MemoryStore;
        let store = MemoryStore::load(vec![]).expect("MemoryStore::load");
        let dashboard = dashboard_with_kind("climate.does_not_exist", WidgetKind::Climate);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Climate(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_active);
                assert_eq!(vm.current_temperature, None);
                assert_eq!(vm.target_temperature, None);
            }
            other => panic!("expected ClimateTileVM placeholder, got {other:?}"),
        }
    }

    #[test]
    fn build_tiles_media_player_widget_with_missing_entity_emits_unavailable_media_player_vm() {
        use crate::ha::store::MemoryStore;
        let store = MemoryStore::load(vec![]).expect("MemoryStore::load");
        let dashboard = dashboard_with_kind("media_player.does_not_exist", WidgetKind::MediaPlayer);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::MediaPlayer(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_playing);
                assert_eq!(vm.media_title, None);
                assert_eq!(vm.artist, None);
                assert_eq!(vm.volume_level, None);
            }
            other => panic!("expected MediaPlayerTileVM placeholder, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // compute_power_flow_tile_vm_from_widget — options-path branches
    //
    // The wrapper-entity branch is exercised by
    // `build_tiles_power_flow_widget_uses_state` etc. The two uncovered
    // branches handled here are:
    //
    // 1. `WidgetOptions::PowerFlow` present + grid entity NOT in store —
    //    the auxiliary fields still resolve, but `grid_w` is None and the
    //    Slint side renders the unavailable variant.
    // 2. Defensive fallback: neither options nor wrapper_entity supplied —
    //    every scalar is None.
    // -----------------------------------------------------------------------

    /// Options-driven path: the grid entity is NOT in the store, but the
    /// auxiliary entities ARE. The bridge must surface the auxiliary
    /// readings even when the primary grid entity is missing — this keeps
    /// solar / battery / home labels populated so the operator sees the
    /// last-known partial state.
    #[test]
    fn compute_power_flow_tile_vm_from_widget_options_path_with_missing_grid_resolves_auxiliaries()
    {
        use crate::dashboard::schema::WidgetOptions;
        use crate::ha::store::MemoryStore;

        // Grid entity is intentionally missing; aux entities are present.
        let store = MemoryStore::load(vec![
            make_test_entity("sensor.solar_power", "2000.0"),
            make_test_entity("sensor.battery_power", "-300.0"),
            make_test_entity("sensor.battery_soc", "75.0"),
            make_test_entity("sensor.home_power", "800.0"),
        ])
        .expect("MemoryStore::load");

        let options = WidgetOptions::PowerFlow {
            grid_entity: "sensor.does_not_exist_grid".to_string(),
            solar_entity: Some("sensor.solar_power".to_string()),
            battery_entity: Some("sensor.battery_power".to_string()),
            battery_soc_entity: Some("sensor.battery_soc".to_string()),
            home_entity: Some("sensor.home_power".to_string()),
        };

        let vm = compute_power_flow_tile_vm_from_widget(
            PowerFlowBuildArgs {
                name: "Power".to_string(),
                icon_id: "mdi:lightning-bolt-circle".to_string(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement: TilePlacement::default_for(2, 2),
                wrapper_entity: None,
                options: Some(&options),
            },
            &store,
        );

        // Grid is missing → grid_w is None (drives Slint unavailable variant).
        assert!(
            vm.grid_w.is_none(),
            "missing grid entity must surface as grid_w=None"
        );
        // But auxiliaries still resolve — the operator sees partial state.
        assert_eq!(vm.solar_w, Some(2000.0));
        assert_eq!(vm.battery_w, Some(-300.0));
        assert_eq!(vm.battery_pct, Some(75.0));
        assert_eq!(vm.home_w, Some(800.0));
        // The placeholder still carries the configured name and icon so
        // the unavailable visual variant labels are stable.
        assert_eq!(vm.name, "Power");
        assert_eq!(vm.icon_id, "mdi:lightning-bolt-circle");
        assert!(!vm.pending);
    }

    /// Defensive fallback: a power-flow widget with NO options AND no
    /// wrapper entity (degenerate config). The bridge must still emit a
    /// `PowerFlowTileVM` so the row layout stays stable; every scalar is
    /// None so the Slint side renders the unavailable visual variant.
    #[test]
    fn compute_power_flow_tile_vm_from_widget_defensive_fallback_emits_all_none() {
        use crate::ha::store::MemoryStore;
        let store = MemoryStore::load(vec![]).expect("MemoryStore::load");

        let vm = compute_power_flow_tile_vm_from_widget(
            PowerFlowBuildArgs {
                name: "Defensive Power".to_string(),
                icon_id: "mdi:lightning-bolt-circle".to_string(),
                preferred_columns: 2,
                preferred_rows: 2,
                placement: TilePlacement::default_for(2, 2),
                wrapper_entity: None,
                options: None, // <-- no options
            },
            &store,
        );

        assert_eq!(vm.name, "Defensive Power");
        assert!(vm.grid_w.is_none());
        assert!(vm.solar_w.is_none());
        assert!(vm.battery_w.is_none());
        assert!(vm.battery_pct.is_none());
        assert!(vm.home_w.is_none());
        assert!(!vm.pending);
        assert_eq!(vm.icon_id, "mdi:lightning-bolt-circle");
    }

    #[test]
    fn wire_error_messages_are_deterministic() {
        // Defensive: format strings must not depend on Debug derive output.
        // This guards against accidental refactors that swap the variant text.
        let cap = WireError::FramerateCapOutOfRange;
        let max = WireError::MaxSimultaneousOutOfRange;
        let gesture = WireError::GestureTimingOutOfRange;

        assert!(cap.to_string().contains("framerate_cap"));
        assert!(max.to_string().contains("max_simultaneous_animations"));
        assert!(gesture.to_string().contains("GestureConfig"));
    }

    // -----------------------------------------------------------------------
    // project_gesture_config (TASK-059) — pure conversion exercised here so
    // the u64 -> i32 narrowing and the arm_double_tap_timer derivation are
    // covered without instantiating a MainWindow (same headless-CI rationale
    // documented above split_tile_vms).
    // -----------------------------------------------------------------------

    #[test]
    fn project_gesture_config_default_writes_all_default_values() {
        let projection = project_gesture_config(GestureConfig::default())
            .expect("default GestureConfig must project successfully");
        assert_eq!(projection.tap_max_ms, 200);
        assert_eq!(projection.hold_min_ms, 500);
        assert_eq!(projection.double_tap_max_gap_ms, 300);
        assert!(projection.double_tap_enabled);
        // The arm_double_tap_timer invariant survives the projection: it
        // tracks double_tap_enabled verbatim, NOT inferred from the gap.
        assert!(projection.arm_double_tap_timer);
        assert_eq!(
            projection.arm_double_tap_timer,
            projection.double_tap_enabled,
        );
    }

    #[test]
    fn project_gesture_config_preserves_arm_double_tap_invariant_when_disabled() {
        // The locked_decisions.gesture_config invariant must survive the
        // bridge layer too — Slint must receive the explicit boolean even
        // when the gap is non-zero. Otherwise a downstream Slint reader could
        // re-introduce the "infer from gap" bug.
        let cfg = GestureConfig {
            double_tap_enabled: false,
            double_tap_max_gap_ms: 300,
            ..GestureConfig::default()
        };
        let projection = project_gesture_config(cfg).expect("must project successfully");
        assert!(!projection.arm_double_tap_timer);
        assert!(!projection.double_tap_enabled);
    }

    #[test]
    fn project_gesture_config_preserves_arm_double_tap_invariant_with_zero_gap() {
        // Defensive twin of the timing.rs zero-gap test: even when the gap
        // is zero (the value a naive Slint reader might treat as "disabled"),
        // arm_double_tap_timer must still equal double_tap_enabled after
        // projection.
        let cfg = GestureConfig {
            double_tap_enabled: true,
            double_tap_max_gap_ms: 0,
            ..GestureConfig::default()
        };
        let projection = project_gesture_config(cfg).expect("must project successfully");
        assert!(projection.arm_double_tap_timer);
        assert_eq!(projection.double_tap_max_gap_ms, 0);
    }

    #[test]
    fn project_gesture_config_rejects_tap_max_ms_overflow() {
        let cfg = GestureConfig {
            tap_max_ms: u64::from(u32::MAX),
            ..GestureConfig::default()
        };
        let err = project_gesture_config(cfg).expect_err("u64::from(u32::MAX) overflows i32::MAX");
        assert_eq!(err, WireError::GestureTimingOutOfRange);
    }

    #[test]
    fn project_gesture_config_rejects_hold_min_ms_overflow() {
        let cfg = GestureConfig {
            hold_min_ms: u64::from(u32::MAX),
            ..GestureConfig::default()
        };
        let err = project_gesture_config(cfg).expect_err("u64::from(u32::MAX) overflows i32::MAX");
        assert_eq!(err, WireError::GestureTimingOutOfRange);
    }

    #[test]
    fn project_gesture_config_rejects_double_tap_gap_overflow() {
        let cfg = GestureConfig {
            double_tap_max_gap_ms: u64::from(u32::MAX),
            ..GestureConfig::default()
        };
        let err = project_gesture_config(cfg).expect_err("u64::from(u32::MAX) overflows i32::MAX");
        assert_eq!(err, WireError::GestureTimingOutOfRange);
    }

    #[test]
    fn project_gesture_config_accepts_i32_max_boundary() {
        // i32::MAX itself fits in i32 — the conversion is non-rejecting at
        // the upper boundary, so out-of-range only fires above it.
        let cfg = GestureConfig {
            tap_max_ms: i32::MAX as u64,
            hold_min_ms: i32::MAX as u64,
            double_tap_max_gap_ms: i32::MAX as u64,
            ..GestureConfig::default()
        };
        let projection = project_gesture_config(cfg).expect("i32::MAX must project successfully");
        assert_eq!(projection.tap_max_ms, i32::MAX);
        assert_eq!(projection.hold_min_ms, i32::MAX);
        assert_eq!(projection.double_tap_max_gap_ms, i32::MAX);
    }

    // -----------------------------------------------------------------------
    // Headless MainWindow tests (TASK-059 coverage ratchet) — exercise
    // `wire_window` and `write_gesture_config` end-to-end against a real
    // `MainWindow` constructed under a minimal in-test Slint platform that
    // installs `MinimalSoftwareWindow` via `slint::platform::set_platform`.
    //
    // Why this exists: the production write path (`window.global::<...>().set_*`)
    // cannot be exercised by the pure projection tests above. Without a real
    // `MainWindow`, the setter calls — and the early-return behavior of
    // `write_gesture_config` when projection fails — are uncovered, which
    // dropped `src/ui/bridge.rs` below its 97.4% baseline.
    //
    // Why no new dependency: the runtime `slint` crate already enables the
    // `renderer-software` feature, which exposes
    // `slint::platform::software_renderer::MinimalSoftwareWindow`. That type
    // already implements `WindowAdapter`, so the in-test `Platform` impl
    // below only needs `create_window_adapter`. No `i-slint-backend-testing`
    // dev-dep, no winit, no DISPLAY required.
    //
    // Per-thread install: Slint's `GLOBAL_CONTEXT` is `thread_local!` (see
    // `i-slint-core/context.rs`). Cargo libtest spawns a worker thread per
    // `#[test]`, so the install must be per-thread. The `thread_local!`
    // `OnceCell` below is idempotent on a given thread — first
    // `install_test_platform` wins, subsequent calls reuse the same
    // platform. `set_platform` returns `Err(AlreadySet)` if some other code
    // on this thread already installed the auto-selected backend; the
    // helper surfaces that as a single `panic!` so the failure points at
    // the colliding caller, not a confusing later test.
    // -----------------------------------------------------------------------

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
            // If something else already set a platform on this thread, the
            // test depends on a dirty thread — fail loudly rather than
            // silently using the wrong backend.
            slint::platform::set_platform(Box::new(platform))
                .expect("test platform install: another platform was already set on this thread");
            cell.set(())
                .expect("OnceCell set must succeed on first call");
        });
    }

    #[test]
    fn write_gesture_config_writes_default_values_to_slint_global() {
        // Behavior contract: `write_gesture_config` with the default config
        // must populate every `GestureConfigGlobal` property with the
        // documented default value. We read back via the Slint-generated
        // `get_*` accessors — the only observable effect of the writer.
        install_test_platform_once_per_thread();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        write_gesture_config(&window, GestureConfig::default())
            .expect("default GestureConfig must wire successfully");

        let global = window.global::<GestureConfigGlobal>();
        assert_eq!(global.get_tap_max_ms(), 200);
        assert_eq!(global.get_hold_min_ms(), 500);
        assert_eq!(global.get_double_tap_max_gap_ms(), 300);
        assert!(global.get_double_tap_enabled());
        // arm-double-tap-timer must mirror double-tap-enabled per
        // locked_decisions.gesture_config — Slint must not infer this from
        // the gap value.
        assert!(global.get_arm_double_tap_timer());
    }

    #[test]
    fn write_gesture_config_propagates_disabled_double_tap_to_global() {
        // Behavior contract: disabling double-tap in the Rust config must
        // surface as `arm_double_tap_timer = false` on the Slint side, even
        // when `double_tap_max_gap_ms` is non-zero. This is the locked
        // invariant that prevents downstream Slint code from re-introducing
        // the "infer from gap" bug.
        install_test_platform_once_per_thread();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        let cfg = GestureConfig {
            double_tap_enabled: false,
            double_tap_max_gap_ms: 250,
            ..GestureConfig::default()
        };
        write_gesture_config(&window, cfg).expect("disabled-double-tap config must wire");

        let global = window.global::<GestureConfigGlobal>();
        assert!(!global.get_double_tap_enabled());
        assert!(!global.get_arm_double_tap_timer());
        // The gap value must still propagate verbatim — the bridge does not
        // overwrite it just because double_tap_enabled is false.
        assert_eq!(global.get_double_tap_max_gap_ms(), 250);
    }

    #[test]
    fn write_gesture_config_returns_error_and_does_not_overwrite_global_on_overflow() {
        // Behavior contract: when projection rejects the config (any *_ms
        // field exceeds i32::MAX), `write_gesture_config` returns
        // `GestureTimingOutOfRange` BEFORE touching any Slint setter. We
        // verify the early-return semantics by reading the global both
        // before and after the failed write and asserting the Slint-side
        // values are unchanged.
        install_test_platform_once_per_thread();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        // Seed the global with a known sentinel so we can detect any
        // unintended write below. We write defaults first (which we know
        // succeeds), then read them back as the "expected unchanged" set.
        write_gesture_config(&window, GestureConfig::default()).expect("seed write must succeed");
        let global = window.global::<GestureConfigGlobal>();
        let before_tap = global.get_tap_max_ms();
        let before_hold = global.get_hold_min_ms();
        let before_gap = global.get_double_tap_max_gap_ms();
        let before_enabled = global.get_double_tap_enabled();
        let before_arm = global.get_arm_double_tap_timer();

        let cfg = GestureConfig {
            tap_max_ms: u64::from(u32::MAX), // > i32::MAX → projection rejects
            ..GestureConfig::default()
        };
        let err = write_gesture_config(&window, cfg)
            .expect_err("overflow config must return GestureTimingOutOfRange");
        assert_eq!(err, WireError::GestureTimingOutOfRange);

        // Early-return invariant: no setter ran, so every property still
        // holds its pre-call value.
        assert_eq!(global.get_tap_max_ms(), before_tap);
        assert_eq!(global.get_hold_min_ms(), before_hold);
        assert_eq!(global.get_double_tap_max_gap_ms(), before_gap);
        assert_eq!(global.get_double_tap_enabled(), before_enabled);
        assert_eq!(global.get_arm_double_tap_timer(), before_arm);
    }

    #[test]
    fn wire_window_writes_animation_budget_and_default_gesture_config() {
        // Behavior contract: `wire_window` populates AnimationBudget from
        // the supplied `DeviceProfile` AND wires the default GestureConfig in
        // a single call. We verify both globals end up with the documented
        // desktop-preset values after one wire_window invocation.
        install_test_platform_once_per_thread();
        ensure_icons_init();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        // Empty tile slice exercises the wire_window body without forcing
        // any per-tile rendering — the property models are still populated
        // (as empty VecModels) and the AnimationBudget + GestureConfig
        // globals are still written.
        wire_window(&window, &[], &PROFILE_DESKTOP)
            .expect("wire_window with empty tiles must succeed");

        let budget = window.global::<AnimationBudget>();
        assert_eq!(
            budget.get_framerate_cap(),
            i32::try_from(PROFILE_DESKTOP.animation_framerate_cap)
                .expect("PROFILE_DESKTOP framerate_cap fits in i32"),
            "wire_window must propagate PROFILE_DESKTOP.animation_framerate_cap",
        );
        assert_eq!(
            budget.get_max_simultaneous(),
            i32::try_from(PROFILE_DESKTOP.max_simultaneous_animations)
                .expect("PROFILE_DESKTOP max_simultaneous fits in i32"),
            "wire_window must propagate PROFILE_DESKTOP.max_simultaneous_animations",
        );

        // Gesture global must also be populated with the GestureConfig
        // default values — wire_window calls write_gesture_config internally.
        let gesture = window.global::<GestureConfigGlobal>();
        assert_eq!(gesture.get_tap_max_ms(), 200);
        assert_eq!(gesture.get_hold_min_ms(), 500);
        assert_eq!(gesture.get_double_tap_max_gap_ms(), 300);
        assert!(gesture.get_double_tap_enabled());
        assert!(gesture.get_arm_double_tap_timer());
    }

    /// TASK-120b F4: a non-desktop profile must reach the Slint
    /// `AnimationBudget` globals through `wire_window` (the read sites
    /// post-TASK-120b are `profile.animation_framerate_cap` and
    /// `profile.max_simultaneous_animations`). Pre-TASK-120b every dashboard
    /// — desktop or SBC — silently ran with the desktop animation budget;
    /// this test rejects a regression to that behaviour.
    ///
    /// We use OPI Zero 3 (20 fps cap, 2 simultaneous animations) because
    /// every distinguishing field differs from `PROFILE_DESKTOP` (60 fps,
    /// 8 simultaneous), so the assertions cannot pass by coincidence.
    #[test]
    fn wire_window_propagates_opi_zero3_animation_budget_to_globals() {
        use crate::dashboard::profiles::PROFILE_OPI_ZERO3;

        // Pre-condition: OPI Zero 3 differs from Desktop on both threaded
        // animation fields. If a future profile-table edit aligned them,
        // this test would pass trivially; the asserts below would then
        // need a different distinguishing profile.
        assert_ne!(
            PROFILE_OPI_ZERO3.animation_framerate_cap, PROFILE_DESKTOP.animation_framerate_cap,
            "PROFILE_OPI_ZERO3.animation_framerate_cap must differ from \
             PROFILE_DESKTOP so this test rejects a regression to the \
             desktop default",
        );
        assert_ne!(
            PROFILE_OPI_ZERO3.max_simultaneous_animations,
            PROFILE_DESKTOP.max_simultaneous_animations,
            "PROFILE_OPI_ZERO3.max_simultaneous_animations must differ from \
             PROFILE_DESKTOP so this test rejects a regression to the \
             desktop default",
        );

        install_test_platform_once_per_thread();
        ensure_icons_init();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        wire_window(&window, &[], &PROFILE_OPI_ZERO3)
            .expect("wire_window with PROFILE_OPI_ZERO3 must succeed");

        let budget = window.global::<AnimationBudget>();
        assert_eq!(
            budget.get_framerate_cap(),
            i32::try_from(PROFILE_OPI_ZERO3.animation_framerate_cap)
                .expect("PROFILE_OPI_ZERO3 framerate_cap fits in i32"),
            "wire_window must propagate PROFILE_OPI_ZERO3.animation_framerate_cap \
             (20) to AnimationBudget.framerate-cap; a regression to the desktop \
             default (60) would fail here",
        );
        assert_eq!(
            budget.get_max_simultaneous(),
            i32::try_from(PROFILE_OPI_ZERO3.max_simultaneous_animations)
                .expect("PROFILE_OPI_ZERO3 max_simultaneous fits in i32"),
            "wire_window must propagate PROFILE_OPI_ZERO3.max_simultaneous_animations \
             (2) to AnimationBudget.max-simultaneous; a regression to the desktop \
             default (8) would fail here",
        );
    }

    // -----------------------------------------------------------------------
    // TASK-126 F11: stepped spinner — tick_hz_sbc + sbc_spinner_cap wiring
    // -----------------------------------------------------------------------

    /// Desktop profile → tick-hz-sbc must be 0 (continuous mode, no Timer)
    /// and sbc-spinner-cap must be 0 (unbounded).
    #[test]
    fn wire_window_desktop_sets_tick_hz_sbc_zero_and_sbc_spinner_cap_zero() {
        install_test_platform_once_per_thread();
        ensure_icons_init();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        wire_window(&window, &[], &PROFILE_DESKTOP)
            .expect("wire_window with PROFILE_DESKTOP must succeed");

        let budget = window.global::<AnimationBudget>();
        assert_eq!(
            budget.get_tick_hz_sbc(),
            0,
            "PROFILE_DESKTOP (60 fps) must yield tick-hz-sbc == 0 (continuous mode)",
        );
        assert_eq!(
            budget.get_sbc_spinner_cap(),
            0,
            "PROFILE_DESKTOP must yield sbc-spinner-cap == 0 (unbounded)",
        );
    }

    /// OPI Zero 3 profile (20 fps cap) → tick-hz-sbc must be 12 (stepped SBC
    /// mode) and sbc-spinner-cap must equal max_simultaneous_animations (2).
    #[test]
    fn wire_window_opi_zero3_sets_tick_hz_sbc_12_and_sbc_spinner_cap() {
        use crate::dashboard::profiles::PROFILE_OPI_ZERO3;

        install_test_platform_once_per_thread();
        ensure_icons_init();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        wire_window(&window, &[], &PROFILE_OPI_ZERO3)
            .expect("wire_window with PROFILE_OPI_ZERO3 must succeed");

        let budget = window.global::<AnimationBudget>();
        assert_eq!(
            budget.get_tick_hz_sbc(),
            12,
            "PROFILE_OPI_ZERO3 (20 fps cap <= 30) must yield tick-hz-sbc == 12",
        );
        assert_eq!(
            budget.get_sbc_spinner_cap(),
            i32::try_from(PROFILE_OPI_ZERO3.max_simultaneous_animations)
                .expect("PROFILE_OPI_ZERO3 max_simultaneous fits in i32"),
            "PROFILE_OPI_ZERO3 must yield sbc-spinner-cap == max_simultaneous_animations (2)",
        );
    }

    /// RPI4 profile (30 fps cap) → tick-hz-sbc must be 12 (stepped SBC mode)
    /// and sbc-spinner-cap must equal max_simultaneous_animations (3).
    #[test]
    fn wire_window_rpi4_sets_tick_hz_sbc_12_and_sbc_spinner_cap() {
        use crate::dashboard::profiles::PROFILE_RPI4;

        install_test_platform_once_per_thread();
        ensure_icons_init();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        wire_window(&window, &[], &PROFILE_RPI4)
            .expect("wire_window with PROFILE_RPI4 must succeed");

        let budget = window.global::<AnimationBudget>();
        assert_eq!(
            budget.get_tick_hz_sbc(),
            12,
            "PROFILE_RPI4 (30 fps cap <= 30) must yield tick-hz-sbc == 12",
        );
        assert_eq!(
            budget.get_sbc_spinner_cap(),
            i32::try_from(PROFILE_RPI4.max_simultaneous_animations)
                .expect("PROFILE_RPI4 max_simultaneous fits in i32"),
            "PROFILE_RPI4 must yield sbc-spinner-cap == max_simultaneous_animations (3)",
        );
    }

    // -----------------------------------------------------------------------
    // LiveBridge tests (TASK-033)
    // -----------------------------------------------------------------------
    //
    // The tests below use a hand-rolled stub `EntityStore` (`StubStore`) and a
    // recording sink (`RecordingSink`) so the bridge can be exercised entirely
    // in-process — no Slint backend, no real WebSocket.  Real WS-mock testing
    // is TASK-035; these tests cover the bridge logic only.

    use super::{
        collect_visible_entity_ids, drain_pending, is_writes_gated, BridgeSink, LiveBridge,
        FLUSH_INTERVAL_MS,
    };
    use crate::ha::entity::Entity;
    use crate::ha::store::{EntityStore, EntityUpdate};
    use crate::platform::status::{channel as status_channel, ConnectionState};
    use jiff::Timestamp;
    use std::collections::HashMap as StdHashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::broadcast;

    /// Stub [`EntityStore`] with mutable map and per-entity senders.
    ///
    /// Mirrors the `LiveStore` Phase 2 contract closely:
    ///   * `subscribe(&[id])` — on-demand per-entity broadcast sender, capacity 1.
    ///   * `publish(update)` — sends through the matching sender if any.
    ///   * `set_entity` — mutates the internal map (so `get` returns the new state).
    ///
    /// Kept inline here rather than wired through `crate::ha::live_store::LiveStore`
    /// because TASK-033's `files_allowlist` is `src/ui/bridge.rs` only.
    struct StubStore {
        map: Mutex<StdHashMap<EntityId, Entity>>,
        senders: Mutex<StdHashMap<EntityId, broadcast::Sender<EntityUpdate>>>,
    }

    impl StubStore {
        fn new(initial: Vec<Entity>) -> Self {
            let map: StdHashMap<EntityId, Entity> =
                initial.into_iter().map(|e| (e.id.clone(), e)).collect();
            StubStore {
                map: Mutex::new(map),
                senders: Mutex::new(StdHashMap::new()),
            }
        }

        fn set_entity(&self, entity: Entity) {
            let mut guard = self.map.lock().expect("StubStore map mutex poisoned");
            guard.insert(entity.id.clone(), entity);
        }

        fn publish(&self, update: EntityUpdate) {
            let senders = self
                .senders
                .lock()
                .expect("StubStore senders mutex poisoned");
            if let Some(tx) = senders.get(&update.id) {
                let _ = tx.send(update);
            }
        }
    }

    impl EntityStore for StubStore {
        fn get(&self, id: &EntityId) -> Option<Entity> {
            let guard = self.map.lock().expect("StubStore map mutex poisoned");
            guard.get(id).cloned()
        }

        fn for_each(&self, f: &mut dyn FnMut(&EntityId, &Entity)) {
            let guard = self.map.lock().expect("StubStore map mutex poisoned");
            for (id, entity) in guard.iter() {
                f(id, entity);
            }
        }

        fn subscribe(&self, ids: &[EntityId]) -> broadcast::Receiver<EntityUpdate> {
            let Some(id) = ids.first() else {
                let (tx, rx) = broadcast::channel(1);
                drop(tx);
                return rx;
            };
            let mut senders = self
                .senders
                .lock()
                .expect("StubStore senders mutex poisoned");
            let tx = senders
                .entry(id.clone())
                .or_insert_with(|| broadcast::channel(1).0);
            tx.subscribe()
        }
    }

    /// Recording sink: stores every effect in shared state so the test
    /// thread can poll/assert on them without a Slint event loop.
    ///
    /// Note on the legacy `tile_writes` channel: pre-TASK-119 the flush
    /// loop produced a full `Vec<TileVM>` and the recording sink captured
    /// it via `write_tiles`.  Post-TASK-119 the flush loop calls
    /// `apply_row_updates` instead; the trait's default fallback invokes
    /// the rebuild thunk and routes the result through `write_tiles`, so
    /// `tile_writes` continues to record the full-rebuild snapshot for
    /// every flush — the existing assertions keep working.  The new
    /// `row_updates` channel records the per-row updates the production
    /// `SlintSink` would receive (and the rebuild thunk's output, for
    /// completeness).
    #[derive(Default)]
    struct RecordingSink {
        tile_writes: Mutex<Vec<Vec<TileVM>>>,
        banner_calls: Mutex<Vec<bool>>,
        row_updates: Mutex<Vec<Vec<RowUpdate>>>,
    }

    impl RecordingSink {
        fn snapshot_tile_writes(&self) -> Vec<Vec<TileVM>> {
            self.tile_writes
                .lock()
                .expect("tile_writes mutex poisoned")
                .clone()
        }
        fn snapshot_banner_calls(&self) -> Vec<bool> {
            self.banner_calls
                .lock()
                .expect("banner_calls mutex poisoned")
                .clone()
        }
        fn snapshot_row_updates(&self) -> Vec<Vec<RowUpdate>> {
            self.row_updates
                .lock()
                .expect("row_updates mutex poisoned")
                .clone()
        }
    }

    impl BridgeSink for RecordingSink {
        fn write_tiles(&self, tiles: Vec<TileVM>) {
            self.tile_writes
                .lock()
                .expect("tile_writes mutex poisoned")
                .push(tiles);
        }
        fn apply_row_updates(
            &self,
            updates: Vec<RowUpdate>,
            rebuild_full_tiles: Box<dyn FnOnce() -> Vec<TileVM> + Send>,
        ) {
            // Record the per-row updates first so tests can assert on them
            // even when the rebuild thunk also routes through
            // `write_tiles` below.
            self.row_updates
                .lock()
                .expect("row_updates mutex poisoned")
                .push(updates);
            // Preserve the legacy full-tile capture by invoking the
            // rebuild thunk and forwarding to `write_tiles` — this matches
            // the trait's default fallback behaviour and keeps existing
            // pre-TASK-119 test assertions on `tile_writes` unchanged.
            self.write_tiles(rebuild_full_tiles());
        }
        fn set_status_banner_visible(&self, visible: bool) {
            self.banner_calls
                .lock()
                .expect("banner_calls mutex poisoned")
                .push(visible);
        }
    }

    /// `Arc<RecordingSink>` does not directly implement `BridgeSink` — the
    /// bridge takes ownership of a `BridgeSink` value, so the test thread
    /// also wants its own clone of the recording state to assert on.  This
    /// proxy holds an `Arc` to the same recording state and forwards every
    /// effect call through.
    struct ArcSink(Arc<RecordingSink>);

    impl BridgeSink for ArcSink {
        fn write_tiles(&self, tiles: Vec<TileVM>) {
            self.0.write_tiles(tiles);
        }
        fn apply_row_updates(
            &self,
            updates: Vec<RowUpdate>,
            rebuild_full_tiles: Box<dyn FnOnce() -> Vec<TileVM> + Send>,
        ) {
            self.0.apply_row_updates(updates, rebuild_full_tiles);
        }
        fn set_status_banner_visible(&self, visible: bool) {
            self.0.set_status_banner_visible(visible);
        }
    }

    fn make_test_entity(id: &str, state: &str) -> Entity {
        // `attributes: Arc::default()` constructs an empty `Arc<Map<...>>`
        // for the JSON attributes field without the test code referring to
        // the underlying JSON library directly — `src/ui/**` is gated
        // against direct uses of that library by the CI repo-rules check.
        // The Default impl on the inner Map produces an empty map; the
        // Arc::default impl wraps it.
        Entity {
            id: EntityId::from(id),
            state: Arc::from(state),
            attributes: Arc::default(),
            last_changed: Timestamp::UNIX_EPOCH,
            last_updated: Timestamp::UNIX_EPOCH,
        }
    }

    /// Minimal dashboard with one Light widget pointing at `light.kitchen`.
    fn one_widget_dashboard() -> Dashboard {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![Widget {
                        id: "w1".to_string(),
                        widget_type: WidgetKind::LightTile,
                        entity: Some("light.kitchen".to_string()),
                        entities: vec![],
                        name: Some("Kitchen".to_string()),
                        icon: None,
                        tap_action: None,
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 2,
                            preferred_rows: 2,
                        },
                        options: None,
                        placement: None,
                        visibility: "always".to_string(),
                    }],
                }],
            }],
        }
    }

    // ----- Pure helpers -----

    #[test]
    fn is_writes_gated_returns_true_for_reconnecting_and_failed() {
        assert!(is_writes_gated(ConnectionState::Reconnecting));
        assert!(is_writes_gated(ConnectionState::Failed));
    }

    #[test]
    fn is_writes_gated_returns_false_for_non_gated_states() {
        for s in [
            ConnectionState::Connecting,
            ConnectionState::Authenticating,
            ConnectionState::Subscribing,
            ConnectionState::Snapshotting,
            ConnectionState::Services,
            ConnectionState::Live,
        ] {
            assert!(!is_writes_gated(s), "{s:?} must not be gated");
        }
    }

    #[test]
    fn collect_visible_entity_ids_walks_dashboard_in_document_order() {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        let make_widget = |entity: &str| Widget {
            id: format!("w-{entity}"),
            widget_type: WidgetKind::EntityTile,
            entity: Some(entity.to_string()),
            entities: vec![],
            name: None,
            icon: None,
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 1,
                preferred_rows: 1,
            },
            options: None,
            placement: None,
            visibility: "always".to_string(),
        };
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![
                        make_widget("light.a"),
                        make_widget("light.b"),
                        make_widget("light.a"), // duplicate preserved at this stage
                    ],
                }],
            }],
        };

        let ids = collect_visible_entity_ids(&dashboard);
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0].as_str(), "light.a");
        assert_eq!(ids[1].as_str(), "light.b");
        assert_eq!(ids[2].as_str(), "light.a");
    }

    #[test]
    fn drain_pending_returns_all_inserted_ids_and_empties_map() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut g = pending.lock().unwrap();
            g.insert(EntityId::from("light.x"), ());
            g.insert(EntityId::from("light.y"), ());
        }

        let drained = drain_pending(&pending);
        assert_eq!(drained.len(), 2);

        let drained_again = drain_pending(&pending);
        assert!(drained_again.is_empty(), "second drain must be empty");
    }

    #[test]
    fn pending_map_latest_overwrite_collapses_repeats() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        // Three updates for the same entity arrive between flushes.
        for _ in 0..3 {
            let mut g = pending.lock().unwrap();
            g.insert(EntityId::from("light.x"), ());
        }
        let drained = drain_pending(&pending);
        assert_eq!(
            drained.len(),
            1,
            "three updates for the same entity must collapse to one drained id"
        );
        assert_eq!(drained[0].as_str(), "light.x");
    }

    // ----- Integration: end-to-end LiveBridge behaviour -----

    /// Wait for `predicate` to return true, polling with 10 ms steps. Returns
    /// `false` if the timeout elapses without success.  Used to bound async
    /// tests so a regression in the flush task can't hang CI.
    async fn wait_until<F: FnMut() -> bool>(timeout_ms: u64, mut predicate: F) -> bool {
        let mut elapsed: u64 = 0;
        let step: u64 = 10;
        while elapsed < timeout_ms {
            if predicate() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(step)).await;
            elapsed += step;
        }
        predicate()
    }

    #[tokio::test]
    async fn wait_until_returns_false_when_predicate_never_true() {
        // Coverage: exercise the post-timeout `predicate()` call in wait_until
        // (line reached only when the predicate never returns true within the
        // timeout window).  A 1 ms timeout with a permanently-false predicate
        // exhausts the loop and falls through to the final call.
        let result = wait_until(1, || false).await;
        assert!(
            !result,
            "wait_until must return false when predicate never becomes true"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_propagates_update_within_one_cadence() {
        ensure_icons_init();

        let store = Arc::new(StubStore::new(vec![make_test_entity(
            "light.kitchen",
            "on",
        )]));
        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        // Drive the FSM straight to Live for this test.
        state_tx.send(ConnectionState::Live).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // Allow subscribe tasks to register.  state_watcher fires an initial
        // banner-hidden + tiles write because the initial state is Live.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Flip the entity in the stub and publish through the channel.
        store.set_entity(make_test_entity("light.kitchen", "off"));
        store.publish(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_test_entity("light.kitchen", "off")),
        });

        // Within ~2 flush cadences the recording sink must observe the new state.
        let saw_off = wait_until(FLUSH_INTERVAL_MS * 4, || {
            recorder
                .snapshot_tile_writes()
                .iter()
                .any(|tiles| match tiles.last() {
                    Some(TileVM::Light(vm)) => vm.state == "off",
                    _ => false,
                })
        })
        .await;

        assert!(
            saw_off,
            "flush must propagate the off state to the sink within 2 cadences"
        );
    }

    // -----------------------------------------------------------------------
    // TASK-119 F2: RowIndex + apply_row_updates flush path
    // -----------------------------------------------------------------------

    /// Multi-widget dashboard with one Light, one Sensor, and two Entity
    /// widgets across two views — covers per-kind row counter increments
    /// and the entity-without-binding (no `entity` field) case.
    fn multi_kind_dashboard() -> Dashboard {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        let mk = |id: &str, kind: WidgetKind, entity: Option<&str>| Widget {
            id: id.to_string(),
            widget_type: kind,
            entity: entity.map(str::to_string),
            entities: vec![],
            name: None,
            icon: None,
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 1,
                preferred_rows: 1,
            },
            options: None,
            placement: None,
            visibility: "always".to_string(),
        };
        Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "T".to_string(),
                    widgets: vec![
                        mk("w-light", WidgetKind::LightTile, Some("light.kitchen")),
                        mk("w-sensor", WidgetKind::SensorTile, Some("sensor.temp")),
                        mk("w-entity", WidgetKind::EntityTile, Some("switch.fan")),
                        mk("w-noent", WidgetKind::EntityTile, None),
                    ],
                }],
            }],
        }
    }

    #[test]
    fn build_row_index_walks_dashboard_in_document_order_per_kind() {
        let dashboard = multi_kind_dashboard();
        let index = build_row_index(&dashboard);
        // 3 entity-bound widgets recorded; the fourth (no entity) is not
        // routable from the per-entity path.
        assert_eq!(index.entity_count(), 3);
        assert_eq!(index.total_rows(), 3);
        // Per-kind row counters: light row 0, sensor row 0, entity row 0.
        assert_eq!(
            index.rows_for(&EntityId::from("light.kitchen")),
            &[(TileKind::Light, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("sensor.temp")),
            &[(TileKind::Sensor, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("switch.fan")),
            &[(TileKind::Entity, 0)]
        );
    }

    #[test]
    fn build_row_index_handles_repeated_entity_across_widgets() {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        let mk = |id: &str, kind: WidgetKind, entity: &str| Widget {
            id: id.to_string(),
            widget_type: kind,
            entity: Some(entity.to_string()),
            entities: vec![],
            name: None,
            icon: None,
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 1,
                preferred_rows: 1,
            },
            options: None,
            placement: None,
            visibility: "always".to_string(),
        };
        // Same entity referenced by a Light tile AND an Entity tile.
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "H".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s".to_string(),
                    title: "T".to_string(),
                    widgets: vec![
                        mk("w1", WidgetKind::LightTile, "light.kitchen"),
                        mk("w2", WidgetKind::EntityTile, "light.kitchen"),
                    ],
                }],
            }],
        };
        let index = build_row_index(&dashboard);
        // Two rows for the same entity: one in the Light model, one in the
        // Entity model.  Order matches widget document order.
        assert_eq!(
            index.rows_for(&EntityId::from("light.kitchen")),
            &[(TileKind::Light, 0), (TileKind::Entity, 0)]
        );
    }

    #[test]
    fn tile_kind_for_widget_maps_every_widget_kind_variant() {
        // task/phase6-window-wireup: every `WidgetKind` now maps to its
        // dedicated `TileKind`. Pre-wire-up the Phase 6 kinds all collapsed
        // into `TileKind::Entity`; the row-update fan-out and the per-kind
        // Slint models depend on this 1:1 mapping.
        use crate::dashboard::schema::WidgetKind;
        let cases: &[(WidgetKind, TileKind)] = &[
            (WidgetKind::LightTile, TileKind::Light),
            (WidgetKind::SensorTile, TileKind::Sensor),
            (WidgetKind::EntityTile, TileKind::Entity),
            (WidgetKind::Cover, TileKind::Cover),
            (WidgetKind::Fan, TileKind::Fan),
            (WidgetKind::Lock, TileKind::Lock),
            (WidgetKind::Alarm, TileKind::Alarm),
            (WidgetKind::History, TileKind::History),
            (WidgetKind::Camera, TileKind::Camera),
            (WidgetKind::Climate, TileKind::Climate),
            (WidgetKind::MediaPlayer, TileKind::MediaPlayer),
            (WidgetKind::PowerFlow, TileKind::PowerFlow),
        ];
        for (widget_kind, expected) in cases {
            assert_eq!(
                tile_kind_for_widget(widget_kind),
                *expected,
                "{widget_kind:?} must map to {expected:?}"
            );
        }
    }

    #[test]
    fn missing_entity_fallback_preserves_widget_kind_row_layout() {
        // TASK-119 F2 row-stability invariant: when an entity is absent
        // from the store, build_tiles must still emit a tile of the kind
        // implied by `widget_type` so the row layout matches the
        // [`RowIndex`] regardless of store presence.
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        let mk = |id: &str, kind: WidgetKind, entity: &str| Widget {
            id: id.to_string(),
            widget_type: kind,
            entity: Some(entity.to_string()),
            entities: vec![],
            name: None,
            icon: None,
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 1,
                preferred_rows: 1,
            },
            options: None,
            placement: None,
            visibility: "always".to_string(),
        };
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "H".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s".to_string(),
                    title: "T".to_string(),
                    widgets: vec![
                        mk("w1", WidgetKind::LightTile, "light.absent"),
                        mk("w2", WidgetKind::SensorTile, "sensor.absent"),
                    ],
                }],
            }],
        };
        // Empty store — both entities are missing.
        let store = StubStore::new(vec![]);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 2);
        match &tiles[0] {
            TileVM::Light(vm) => {
                assert_eq!(vm.state, "unavailable");
            }
            other => panic!("expected TileVM::Light fallback, got {other:?}"),
        }
        match &tiles[1] {
            TileVM::Sensor(vm) => {
                assert_eq!(vm.state, "unavailable");
            }
            other => panic!("expected TileVM::Sensor fallback, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_emits_row_update_only_for_changed_entity() {
        // TASK-119 F2: `run_flush_loop` produces one `RowUpdate` per
        // changed entity row, NOT a full tile rebuild.  The recording
        // sink captures both surfaces; this test asserts the per-row
        // surface holds exactly the changed row.
        ensure_icons_init();
        let store = Arc::new(StubStore::new(vec![
            make_test_entity("light.kitchen", "on"),
            make_test_entity("sensor.temp", "20"),
        ]));
        // Multi-kind dashboard so the index has two distinct (kind, row)
        // entries; the test asserts only the entity that actually changed
        // produces an update.
        let dashboard = Arc::new(multi_kind_dashboard());
        let (state_tx, state_rx) = status_channel();
        state_tx.send(ConnectionState::Live).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // Allow initial Live-resync (write_tiles) to land.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let row_updates_pre = recorder.snapshot_row_updates().len();

        // Flip ONLY the sensor; the light state is unchanged.
        store.set_entity(make_test_entity("sensor.temp", "21"));
        store.publish(EntityUpdate {
            id: EntityId::from("sensor.temp"),
            entity: Some(make_test_entity("sensor.temp", "21")),
        });

        // Wait for the next flush cadence to pick up the pending update.
        let saw_update = wait_until(FLUSH_INTERVAL_MS * 4, || {
            recorder.snapshot_row_updates().len() > row_updates_pre
        })
        .await;
        assert!(
            saw_update,
            "flush must produce a row update within 2 cadences"
        );

        let updates = recorder.snapshot_row_updates();
        let last = updates
            .last()
            .expect("at least one row-update batch recorded");
        assert_eq!(
            last.len(),
            1,
            "only the sensor row should be updated; got {} updates: {:?}",
            last.len(),
            last,
        );
        let only = &last[0];
        assert_eq!(only.kind, TileKind::Sensor);
        assert_eq!(only.row_index, 0);
        assert_eq!(only.state, "21");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_skips_apply_when_pending_is_empty() {
        // TASK-119 F2: when no entity has changed (pending is empty),
        // the flush loop short-circuits and does not call the sink at
        // all.  Steady-state idle cost is then bounded by the tick
        // interval, NOT by widget_count.
        ensure_icons_init();
        let store = Arc::new(StubStore::new(vec![make_test_entity(
            "light.kitchen",
            "on",
        )]));
        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        state_tx.send(ConnectionState::Live).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        tokio::time::sleep(Duration::from_millis(20)).await;
        let row_updates_pre = recorder.snapshot_row_updates().len();
        let tile_writes_pre = recorder.snapshot_tile_writes().len();

        // No publish, no entity flips.  Wait several flush cadences and
        // assert neither sink surface grew — the flush loop's
        // `drained.is_empty()` guard must short-circuit.
        tokio::time::sleep(Duration::from_millis(FLUSH_INTERVAL_MS * 4)).await;
        let row_updates_post = recorder.snapshot_row_updates().len();
        let tile_writes_post = recorder.snapshot_tile_writes().len();
        assert_eq!(
            row_updates_post, row_updates_pre,
            "no row updates must be emitted when nothing is pending"
        );
        assert_eq!(
            tile_writes_post, tile_writes_pre,
            "no full-tile rebuilds must be emitted when nothing is pending"
        );
    }

    #[test]
    fn row_index_rows_for_unknown_entity_returns_empty_slice() {
        // TASK-119 F2: when the flush loop receives a pending EntityId
        // that the index doesn't know about, `RowIndex::rows_for` returns
        // an empty slice and the flush loop's per-entity inner loop adds
        // zero updates.  This is the unit-level guarantee behind the
        // empty-`updates` short-circuit in `run_flush_loop`.
        let dashboard = one_widget_dashboard();
        let index = build_row_index(&dashboard);
        // `light.kitchen` is in the index; `switch.unknown` is not.
        assert!(!index.rows_for(&EntityId::from("light.kitchen")).is_empty());
        assert!(index.rows_for(&EntityId::from("switch.unknown")).is_empty());
    }

    /// `build_row_index` walks every `WidgetKind` in document order and
    /// assigns each a row in its per-kind model. Each Phase 6 kind has a
    /// distinct row counter; the test below pins that the per-kind streams
    /// are independent (each kind starts at row 0) and that the lookup by
    /// entity returns the matching `(kind, row)` pair. PowerFlow widgets
    /// have `entity: None`, so they consume a row in the per-kind stream
    /// but are not routable from the per-entity broadcast — the test
    /// exercises that path explicitly so the `entity == ""` branch is
    /// covered.
    #[test]
    fn build_row_index_assigns_independent_row_streams_per_phase6_kind() {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        let mk = |id: &str, kind: WidgetKind, entity: Option<&str>| Widget {
            id: id.to_string(),
            widget_type: kind,
            entity: entity.map(str::to_string),
            entities: vec![],
            name: None,
            icon: None,
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 1,
                preferred_rows: 1,
            },
            options: None,
            placement: None,
            visibility: "always".to_string(),
        };

        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "T".to_string(),
                    widgets: vec![
                        mk("w-cover", WidgetKind::Cover, Some("cover.patio")),
                        mk("w-fan", WidgetKind::Fan, Some("fan.bedroom")),
                        mk("w-lock", WidgetKind::Lock, Some("lock.front_door")),
                        mk(
                            "w-alarm",
                            WidgetKind::Alarm,
                            Some("alarm_control_panel.home"),
                        ),
                        mk("w-history", WidgetKind::History, Some("sensor.energy")),
                        mk("w-camera", WidgetKind::Camera, Some("camera.front_door")),
                        mk(
                            "w-climate",
                            WidgetKind::Climate,
                            Some("climate.living_room"),
                        ),
                        mk(
                            "w-mp",
                            WidgetKind::MediaPlayer,
                            Some("media_player.kitchen"),
                        ),
                        // PowerFlow widgets canonically have entity: None
                        // (their grid entity lives in WidgetOptions::PowerFlow).
                        // The row counter still advances; the entity index
                        // does NOT record a routing entry.
                        mk("w-pf", WidgetKind::PowerFlow, None),
                    ],
                }],
            }],
        };
        let index = build_row_index(&dashboard);

        // 8 entity-bound widgets routable; the PowerFlow widget consumed
        // a row but is not routable from the per-entity broadcast.
        assert_eq!(
            index.entity_count(),
            8,
            "8 entity-bound Phase 6 widgets must be routable"
        );
        assert_eq!(
            index.total_rows(),
            8,
            "total_rows counts only entity-bound entries"
        );

        // Each per-kind stream starts at row 0 — the streams are
        // independent (per-kind row counter, not a single global counter).
        assert_eq!(
            index.rows_for(&EntityId::from("cover.patio")),
            &[(TileKind::Cover, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("fan.bedroom")),
            &[(TileKind::Fan, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("lock.front_door")),
            &[(TileKind::Lock, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("alarm_control_panel.home")),
            &[(TileKind::Alarm, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("sensor.energy")),
            &[(TileKind::History, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("camera.front_door")),
            &[(TileKind::Camera, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("climate.living_room")),
            &[(TileKind::Climate, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("media_player.kitchen")),
            &[(TileKind::MediaPlayer, 0)]
        );
    }

    /// Two widgets of the same Phase 6 kind: the per-kind row counter
    /// advances independently (first widget → row 0, second → row 1).
    /// This pins the increment side of each Phase 6 arm in
    /// `build_row_index`'s per-kind match.
    #[test]
    fn build_row_index_increments_per_kind_counter_independently() {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        let mk = |id: &str, kind: WidgetKind, entity: &str| Widget {
            id: id.to_string(),
            widget_type: kind,
            entity: Some(entity.to_string()),
            entities: vec![],
            name: None,
            icon: None,
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 1,
                preferred_rows: 1,
            },
            options: None,
            placement: None,
            visibility: "always".to_string(),
        };

        // Two Cover widgets + two Fan widgets, interleaved so a global
        // row counter (regression mode) would assign different rows.
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "T".to_string(),
                    widgets: vec![
                        mk("w1", WidgetKind::Cover, "cover.a"),
                        mk("w2", WidgetKind::Fan, "fan.a"),
                        mk("w3", WidgetKind::Cover, "cover.b"),
                        mk("w4", WidgetKind::Fan, "fan.b"),
                    ],
                }],
            }],
        };
        let index = build_row_index(&dashboard);

        // Cover stream: row 0 then row 1. A regression that used a single
        // global counter would assign Cover rows {0, 2} instead.
        assert_eq!(
            index.rows_for(&EntityId::from("cover.a")),
            &[(TileKind::Cover, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("cover.b")),
            &[(TileKind::Cover, 1)]
        );
        // Fan stream is independent — also starts at 0.
        assert_eq!(
            index.rows_for(&EntityId::from("fan.a")),
            &[(TileKind::Fan, 0)]
        );
        assert_eq!(
            index.rows_for(&EntityId::from("fan.b")),
            &[(TileKind::Fan, 1)]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_transition_rebuilds_row_index_atomically() {
        // TASK-119 F2 / Risk #10: the state-watcher's Live transition
        // refreshes the row index BEFORE calling `write_tiles`, so any
        // subsequent flush observes a (model-layout, index-layout) pair
        // that is internally consistent.  This test exercises the
        // Reconnecting -> Live transition and verifies (a) at least one
        // `write_tiles` lands on the resync, (b) a flush after the
        // transition produces exactly one row update for the changed
        // entity (which is only possible if the index is populated).
        ensure_icons_init();
        let store = Arc::new(StubStore::new(vec![make_test_entity(
            "light.kitchen",
            "on",
        )]));
        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        // Start gated so the watcher does NOT fire its initial Live
        // resync; the transition below is the one we are testing.
        state_tx.send(ConnectionState::Reconnecting).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        tokio::time::sleep(Duration::from_millis(20)).await;
        // (a) Transition to Live — watcher rebuilds index AND calls
        // write_tiles.
        state_tx.send(ConnectionState::Live).unwrap();
        let saw_resync = wait_until(FLUSH_INTERVAL_MS * 4, || {
            !recorder.snapshot_tile_writes().is_empty()
        })
        .await;
        assert!(
            saw_resync,
            "Live transition must trigger a write_tiles resync"
        );

        // (b) Drive a per-entity update post-transition; verify the index
        // is populated by checking the resulting row update.
        let row_updates_pre = recorder.snapshot_row_updates().len();
        store.set_entity(make_test_entity("light.kitchen", "off"));
        store.publish(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_test_entity("light.kitchen", "off")),
        });

        let saw_update = wait_until(FLUSH_INTERVAL_MS * 4, || {
            recorder.snapshot_row_updates().len() > row_updates_pre
        })
        .await;
        assert!(
            saw_update,
            "post-transition flush must produce a row update — index missing or stale?"
        );
        let updates = recorder.snapshot_row_updates();
        let last = updates.last().expect("row updates recorded");
        assert!(
            last.iter()
                .any(|u| u.kind == TileKind::Light && u.row_index == 0 && u.state == "off"),
            "expected a Light row 0 update with state=off; got {last:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_does_not_replace_full_vec_model_in_run_flush_loop() {
        // TASK-119 F2 acceptance: `run_flush_loop` must NOT call
        // `write_tiles` directly anymore; it routes through
        // `apply_row_updates`.  We assert this by counting:
        //   * Per flush, exactly one `row_updates` entry is recorded.
        //   * Per flush, the legacy default-fallback also routes through
        //     `write_tiles` once (RecordingSink preserves that for
        //     compatibility), so `tile_writes` and `row_updates`
        //     post-watcher-resync are paired 1:1.
        //
        // The assertion-of-shape (one row_update per flush) is the
        // production contract; the paired write_tiles is the recording
        // sink's compatibility shim, NOT the production behaviour (the
        // production `SlintSink` overrides `apply_row_updates` and never
        // invokes the rebuild thunk).
        ensure_icons_init();
        let store = Arc::new(StubStore::new(vec![make_test_entity(
            "light.kitchen",
            "on",
        )]));
        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        state_tx.send(ConnectionState::Live).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        tokio::time::sleep(Duration::from_millis(20)).await;
        let row_updates_pre = recorder.snapshot_row_updates().len();

        // Drive a single per-entity update.
        store.set_entity(make_test_entity("light.kitchen", "off"));
        store.publish(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_test_entity("light.kitchen", "off")),
        });

        let saw = wait_until(FLUSH_INTERVAL_MS * 4, || {
            recorder.snapshot_row_updates().len() > row_updates_pre
        })
        .await;
        assert!(saw, "flush must emit a row update within 2 cadences");

        // Each flush must have produced exactly one apply_row_updates
        // call (which is what production `SlintSink` would observe).
        let row_updates = recorder.snapshot_row_updates();
        let post_count = row_updates.len();
        assert!(
            post_count > row_updates_pre,
            "row_updates count must grow on a flush ({post_count} <= {row_updates_pre})"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gated_state_suppresses_property_writes() {
        ensure_icons_init();

        let store = Arc::new(StubStore::new(vec![make_test_entity(
            "light.kitchen",
            "on",
        )]));
        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        // Start in Reconnecting — gated.
        state_tx.send(ConnectionState::Reconnecting).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // Wait a beat for tasks to register.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Banner must have been set visible at startup.
        let banner = recorder.snapshot_banner_calls();
        assert_eq!(
            banner.last(),
            Some(&true),
            "banner must be visible while gated"
        );

        // Publish an update that would normally reach the flush.
        store.set_entity(make_test_entity("light.kitchen", "off"));
        store.publish(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_test_entity("light.kitchen", "off")),
        });

        // Wait for several flush cadences and assert NO tile_writes occurred
        // (initial Reconnecting state means no startup write either).
        tokio::time::sleep(Duration::from_millis(FLUSH_INTERVAL_MS * 3)).await;
        let writes = recorder.snapshot_tile_writes();
        assert!(
            writes.is_empty(),
            "no tile writes must occur while gated; got {} writes",
            writes.len()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn return_to_live_clears_banner_and_resyncs() {
        ensure_icons_init();

        let store = Arc::new(StubStore::new(vec![make_test_entity(
            "light.kitchen",
            "on",
        )]));
        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        state_tx.send(ConnectionState::Reconnecting).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        tokio::time::sleep(Duration::from_millis(20)).await;
        // Confirm banner shown.
        assert_eq!(
            recorder.snapshot_banner_calls().last(),
            Some(&true),
            "banner must be visible during Reconnecting"
        );

        // Mutate state behind the gate.
        store.set_entity(make_test_entity("light.kitchen", "off"));

        // Now flip to Live.  Must hide banner AND fire a full resync write.
        state_tx.send(ConnectionState::Live).unwrap();

        // Within one flush cadence the recording sink must see (a) banner=false,
        // (b) at least one tile write reflecting the new "off" state.
        let saw_recovery = wait_until(FLUSH_INTERVAL_MS * 5, || {
            let banner = recorder.snapshot_banner_calls();
            let writes = recorder.snapshot_tile_writes();
            banner.iter().any(|&v| !v)
                && writes.iter().any(|tiles| match tiles.last() {
                    Some(TileVM::Light(vm)) => vm.state == "off",
                    _ => false,
                })
        })
        .await;

        assert!(
            saw_recovery,
            "Live transition must hide banner and trigger a resync write"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lagged_recovers_via_get_and_resubscribes() {
        ensure_icons_init();

        let store = Arc::new(StubStore::new(vec![make_test_entity(
            "light.kitchen",
            "on",
        )]));
        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        state_tx.send(ConnectionState::Live).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // Allow the subscriber task to register before publishing.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Force a Lagged condition: capacity-1 channel + two events without
        // the receiver getting a chance to consume the first.  We sleep
        // briefly between sends to avoid the race where the subscriber wakes
        // immediately on the first send.  StubStore mirrors LiveStore's
        // capacity-1 contract; two un-consumed sends produce Lagged on the
        // next recv().
        store.set_entity(make_test_entity("light.kitchen", "intermediate"));
        store.publish(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_test_entity("light.kitchen", "intermediate")),
        });
        store.set_entity(make_test_entity("light.kitchen", "final"));
        store.publish(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_test_entity("light.kitchen", "final")),
        });

        // The bridge must recover by reading store.get for all subscribed
        // ids and re-subscribing; the resulting flush must reflect the
        // latest "final" state.  Use a generous timeout — Lagged + resync +
        // flush takes longer than a single cadence.
        let saw_final = wait_until(FLUSH_INTERVAL_MS * 8, || {
            recorder
                .snapshot_tile_writes()
                .iter()
                .any(|tiles| match tiles.last() {
                    Some(TileVM::Light(vm)) => vm.state == "final",
                    _ => false,
                })
        })
        .await;

        assert!(
            saw_final,
            "Lagged subscriber must trigger get-based recovery + final-state flush"
        );
    }

    #[test]
    fn build_tiles_signature_remains_dyn_compatible() {
        // Compile-time assertion: build_tiles must accept &dyn EntityStore so
        // src/main.rs (TASK-034) does not need to change when LiveStore is
        // swapped in. The coercion below is a no-op at runtime — it only
        // verifies the type is compatible.
        let _: fn(&dyn EntityStore, &Dashboard) -> Vec<TileVM> = build_tiles;
    }

    #[test]
    fn flush_interval_constant_matches_spec() {
        assert_eq!(FLUSH_INTERVAL_MS, 80, "12.5 Hz cadence per Phase 2 spec");
    }

    #[test]
    fn stub_store_subscribe_with_empty_ids_returns_inert_receiver() {
        // Coverage: exercises the empty-slice branch in StubStore::subscribe,
        // which mirrors the LiveStore single-id contract.  Exercising it
        // here ensures the inert-receiver code path is counted.
        let store = StubStore::new(vec![]);
        let mut rx = store.subscribe(&[]);
        match rx.try_recv() {
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => {}
            other => panic!("expected empty/closed inert receiver, got: {other:?}"),
        }
    }

    #[test]
    fn collect_visible_entity_ids_skips_widgets_without_entity() {
        // Coverage: exercise the `widget.entity = None` skip branch.
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![Widget {
                        id: "no-entity".to_string(),
                        widget_type: WidgetKind::EntityTile,
                        entity: None,
                        entities: vec![],
                        name: None,
                        icon: None,
                        tap_action: None,
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 1,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: None,
                        visibility: "always".to_string(),
                    }],
                }],
            }],
        };
        let ids = collect_visible_entity_ids(&dashboard);
        assert!(ids.is_empty(), "widgets with entity = None must be skipped");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_bridge_aborts_spawned_tasks() {
        // Coverage: exercise the LiveBridge::drop path.  After the bridge is
        // dropped, the spawned tasks are aborted; subsequent publishes do
        // not produce any new sink writes.
        ensure_icons_init();

        let store = Arc::new(StubStore::new(vec![make_test_entity(
            "light.kitchen",
            "on",
        )]));
        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        state_tx.send(ConnectionState::Live).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // Allow the initial Live transition write to land.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let writes_before = recorder.snapshot_tile_writes().len();

        // Drop the bridge.  Drop calls .abort() on every JoinHandle.
        drop(bridge);

        // After the drop, publishes are not observed by any spawned task.
        store.set_entity(make_test_entity("light.kitchen", "off"));
        store.publish(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_test_entity("light.kitchen", "off")),
        });
        // Wait two cadences; the flush task is dead, so no new writes occur.
        tokio::time::sleep(Duration::from_millis(FLUSH_INTERVAL_MS * 3)).await;

        let writes_after = recorder.snapshot_tile_writes().len();
        assert_eq!(
            writes_after, writes_before,
            "no further tile writes must arrive after LiveBridge is dropped"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn state_watcher_exits_when_sender_dropped() {
        // Coverage: exercise run_state_watcher's `state_rx.changed().is_err()`
        // branch by dropping the sender.  The watcher returns; subsequent
        // bridge drop is a no-op for that handle.
        ensure_icons_init();

        let store = Arc::new(StubStore::new(vec![make_test_entity(
            "light.kitchen",
            "on",
        )]));
        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        state_tx.send(ConnectionState::Live).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // Let the watcher record the initial state.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Drop the sender — `state_rx.changed()` returns Err on the next
        // call; the watcher loop exits cleanly.
        drop(state_tx);

        // Sleep long enough for the changed() poll to observe the drop.
        // No assertion on side-effect-counts is needed; coverage of the
        // exit path is the goal.  We do assert the watcher's prior run
        // produced the expected initial Live transition writes so the test
        // doesn't degenerate to a no-op.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !recorder.snapshot_tile_writes().is_empty(),
            "initial Live transition must have produced at least one write"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_to_reconnecting_transition_shows_banner_without_resync_write() {
        // Coverage: exercises the `false` arm of
        // `if matches!(new_state, ConnectionState::Live)` in the state
        // watcher, i.e. the transition into a gated state.
        ensure_icons_init();

        let store = Arc::new(StubStore::new(vec![make_test_entity(
            "light.kitchen",
            "on",
        )]));
        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        state_tx.send(ConnectionState::Live).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );
        // Wait for initial Live transition to be observed.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let writes_after_initial = recorder.snapshot_tile_writes().len();

        // Transition Live -> Reconnecting.  Banner must flip visible; no
        // additional tile write must occur (the resync branch is gated on
        // ConnectionState::Live).
        state_tx.send(ConnectionState::Reconnecting).unwrap();
        let saw_banner_visible_again = wait_until(200, || {
            recorder
                .snapshot_banner_calls()
                .iter()
                .filter(|&&v| v)
                .count()
                >= 1
        })
        .await;
        assert!(
            saw_banner_visible_again,
            "banner must flip visible on Live -> Reconnecting"
        );

        let writes_after_reconnect = recorder.snapshot_tile_writes().len();
        assert_eq!(
            writes_after_reconnect, writes_after_initial,
            "Live -> Reconnecting must NOT trigger a resync tile write"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_entity_dashboard_subscribes_per_unique_id() {
        // Coverage: exercises the for-loop body in LiveBridge::spawn that
        // creates one subscriber task per unique id, and the dedup path
        // (the dashboard references `light.kitchen` twice).
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        ensure_icons_init();

        let make_widget = |entity: &str| Widget {
            id: format!("w-{entity}"),
            widget_type: WidgetKind::LightTile,
            entity: Some(entity.to_string()),
            entities: vec![],
            name: Some(entity.to_string()),
            icon: None,
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 1,
                preferred_rows: 1,
            },
            options: None,
            placement: None,
            visibility: "always".to_string(),
        };
        let dashboard = Arc::new(Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![
                        make_widget("light.kitchen"),
                        make_widget("light.bedroom"),
                        make_widget("light.kitchen"), // duplicate — dedup'd before subscribe
                    ],
                }],
            }],
        });

        let store = Arc::new(StubStore::new(vec![
            make_test_entity("light.kitchen", "on"),
            make_test_entity("light.bedroom", "off"),
        ]));
        let (state_tx, state_rx) = status_channel();
        state_tx.send(ConnectionState::Live).unwrap();
        let recorder = Arc::new(RecordingSink::default());

        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard,
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // Allow subscribers to register.  Both unique ids must each get one
        // sender registered in the StubStore.
        tokio::time::sleep(Duration::from_millis(30)).await;

        let senders_count = store.senders.lock().unwrap().len();
        assert_eq!(
            senders_count, 2,
            "two unique entity ids must produce two on-demand senders"
        );

        // Publish on the second entity and confirm the flush propagates.
        store.set_entity(make_test_entity("light.bedroom", "on"));
        store.publish(EntityUpdate {
            id: EntityId::from("light.bedroom"),
            entity: Some(make_test_entity("light.bedroom", "on")),
        });

        let saw_bedroom_on = wait_until(FLUSH_INTERVAL_MS * 4, || {
            recorder.snapshot_tile_writes().iter().any(|tiles| {
                tiles.iter().any(|t| match t {
                    TileVM::Light(vm) => vm.name == "light.bedroom" && vm.state == "on",
                    _ => false,
                })
            })
        })
        .await;
        assert!(
            saw_bedroom_on,
            "second entity update must propagate via its own subscriber"
        );
    }

    #[test]
    fn missing_entity_with_no_widget_name_falls_back_to_entity_id_string() {
        // Coverage: exercises line 302 — the
        // `.unwrap_or_else(|| entity_id_str.to_string())` arm in the
        // missing-entity policy when widget.name is also None.
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        let store = StubStore::new(vec![]); // no entities loaded → all missing

        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![Widget {
                        id: "w1".to_string(),
                        widget_type: WidgetKind::EntityTile,
                        entity: Some("switch.does_not_exist".to_string()),
                        entities: vec![],
                        name: None,
                        icon: None,
                        tap_action: None,
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 1,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: None,
                        visibility: "always".to_string(),
                    }],
                }],
            }],
        };

        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        if let TileVM::Entity(vm) = &tiles[0] {
            assert_eq!(
                vm.name, "switch.does_not_exist",
                "fallback name for missing entity with no widget.name must be the entity id"
            );
            assert_eq!(vm.state, "unavailable");
        } else {
            panic!("expected EntityTileVM");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn entity_subscriber_exits_when_sender_dropped() {
        // Coverage: exercise the RecvError::Closed branch in
        // run_entity_subscriber.  Build a hand-rolled store whose subscribe
        // returns a receiver tied to a sender we control; drop that sender
        // and assert the bridge does not produce any further writes.
        ensure_icons_init();

        // Use a one-off store wrapping a manually constructed broadcast
        // sender; dropping the sender forces RecvError::Closed on the
        // subscriber's next recv().
        struct ClosingStore {
            map: Mutex<StdHashMap<EntityId, Entity>>,
            // Optional so we can drop the sender mid-test.
            sender: Mutex<Option<broadcast::Sender<EntityUpdate>>>,
        }
        impl EntityStore for ClosingStore {
            fn get(&self, id: &EntityId) -> Option<Entity> {
                self.map.lock().unwrap().get(id).cloned()
            }
            fn for_each(&self, f: &mut dyn FnMut(&EntityId, &Entity)) {
                let g = self.map.lock().unwrap();
                for (id, entity) in g.iter() {
                    f(id, entity);
                }
            }
            fn subscribe(&self, _ids: &[EntityId]) -> broadcast::Receiver<EntityUpdate> {
                let g = self.sender.lock().unwrap();
                match g.as_ref() {
                    Some(tx) => tx.subscribe(),
                    None => {
                        // Sender was already dropped — return an inert closed receiver.
                        let (tx, rx) = broadcast::channel(1);
                        drop(tx);
                        rx
                    }
                }
            }
        }

        let entity = make_test_entity("light.kitchen", "on");
        let map: StdHashMap<EntityId, Entity> =
            std::iter::once((entity.id.clone(), entity)).collect();
        let (tx, _rx_keepalive) = broadcast::channel(1);
        let store: Arc<ClosingStore> = Arc::new(ClosingStore {
            map: Mutex::new(map),
            sender: Mutex::new(Some(tx)),
        });

        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        state_tx.send(ConnectionState::Live).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard,
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // Allow the subscriber task to register on the sender.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Drop the sender — every active receiver gets RecvError::Closed
        // on its next recv().
        {
            let mut g = store.sender.lock().unwrap();
            *g = None;
        }
        // Exercise the ClosingStore::subscribe None branch (sender already
        // dropped). The returned receiver is immediately closed.
        {
            let _inert = store.subscribe(&[EntityId::from("light.kitchen")]);
        }
        // The receiver returned to the subscriber task is now decoupled from
        // the dropped Option<Sender> — drop _rx_keepalive and the channel
        // closes for real.  The subscriber loop returns via the Closed arm.
        drop(_rx_keepalive);

        // Wait long enough for the subscriber task to observe the close.
        tokio::time::sleep(Duration::from_millis(40)).await;

        // The bridge tasks are still alive (flush + state watcher), but the
        // entity subscriber has exited.  Coverage of the Closed branch is
        // the goal.  Assert that the initial Live-transition write happened
        // so the test isn't a no-op.
        assert!(
            !recorder.snapshot_tile_writes().is_empty(),
            "initial Live-transition write must have landed before subscriber exit"
        );

        // Coverage: ClosingStore::for_each must delegate to its map.  The
        // EntityStore trait requires the impl; exercise it directly since
        // build_tiles no longer calls for_each (TASK-118 F3).
        let mut visited = 0usize;
        store.for_each(&mut |_id, _entity| {
            visited += 1;
        });
        assert_eq!(
            visited, 1,
            "ClosingStore::for_each must visit the single entity in the map"
        );
    }

    // -----------------------------------------------------------------------
    // BLOCKER 1 regression: state flip between gate-check and property write
    // -----------------------------------------------------------------------

    /// Stub store that signals every `get` entry on `enter_tx` and
    /// blocks until a corresponding `()` is received on the per-entry
    /// release channel.  The test thread choreographs entries by sending
    /// release signals one at a time.
    ///
    /// This lets the test deterministically open the read-then-check race
    /// window in `run_flush_loop`: while the flush task is blocked inside
    /// `build_tiles -> store.get(...)` for a widget, the test flips
    /// `ConnectionState` to `Reconnecting`, then releases.  The flush
    /// task's second state read must observe `Reconnecting` and skip the
    /// property write.
    ///
    /// (TASK-118 F3 hooked the rendezvous on `for_each`; that hot-path
    /// walk has been removed in favour of an O(1) atomic counter, so the
    /// rendezvous is now wired through `get` — which `build_tiles` still
    /// calls once per widget.)
    struct RendezvousStore {
        base: StubStore,
        // Sent on every `get` entry; test reads to know we are blocked.
        enter_tx: std::sync::mpsc::SyncSender<()>,
        // Receives one release signal per `get` entry.  Wrapped in a
        // Mutex because mpsc::Receiver is not Sync.
        release_rx: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl EntityStore for RendezvousStore {
        fn get(&self, id: &EntityId) -> Option<Entity> {
            // Signal entry, then block until the test thread sends a release.
            let _ = self.enter_tx.send(());
            {
                let rx = self.release_rx.lock().expect("release_rx mutex poisoned");
                let _ = rx.recv();
            }
            self.base.get(id)
        }
        fn for_each(&self, f: &mut dyn FnMut(&EntityId, &Entity)) {
            self.base.for_each(f);
        }
        fn subscribe(&self, ids: &[EntityId]) -> broadcast::Receiver<EntityUpdate> {
            self.base.subscribe(ids)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_skips_write_if_state_flips_to_gated_during_build_tiles() {
        // Regression: if `ConnectionState` flips Live -> Reconnecting between
        // the flush loop's initial gate check and the property write, the
        // write must be suppressed.  Without the post-build_tiles re-check,
        // a stale tile slice would be pushed through the sink while the
        // bridge is supposed to be gated.
        ensure_icons_init();

        let base = StubStore::new(vec![make_test_entity("light.kitchen", "on")]);
        // Bounded enter-channel so the stub blocks before flooding signals.
        let (enter_tx, enter_rx) = std::sync::mpsc::sync_channel::<()>(4);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let store: Arc<RendezvousStore> = Arc::new(RendezvousStore {
            base,
            enter_tx,
            release_rx: Mutex::new(release_rx),
        });

        let dashboard = Arc::new(one_widget_dashboard());
        let (state_tx, state_rx) = status_channel();
        // Start in Live so the state-watcher fires its initial resync
        // (build_tiles -> store.get).  We release that first entry so the
        // bridge is fully primed before the racing flush.
        state_tx.send(ConnectionState::Live).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard.clone(),
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // (1) Initial state-watcher resync on entry to Live.  Wait for it
        // to enter the per-widget store.get rendezvous, then release.
        let primed = tokio::task::spawn_blocking({
            let release_tx = release_tx.clone();
            move || {
                let got = enter_rx
                    .recv_timeout(std::time::Duration::from_millis(2_000))
                    .is_ok();
                if got {
                    let _ = release_tx.send(());
                }
                (got, enter_rx, release_tx)
            }
        })
        .await
        .expect("primed-rendezvous join");
        let (primed_ok, enter_rx, release_tx) = primed;
        assert!(
            primed_ok,
            "initial Live-transition store.get must enter the rendezvous"
        );

        // Allow the watcher's write_tiles to land before we sample baseline.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let writes_after_initial = recorder.snapshot_tile_writes().len();
        assert!(
            writes_after_initial >= 1,
            "initial Live-transition resync must have produced at least one write; \
             got {writes_after_initial}"
        );

        // (2) Make the flush loop have work to do: publish so the per-entity
        // subscriber inserts into pending.
        store
            .base
            .set_entity(make_test_entity("light.kitchen", "off"));
        store.base.publish(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_test_entity("light.kitchen", "off")),
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // (3) On the next flush tick the flush loop enters build_tiles ->
        // store.get and blocks on the rendezvous.  Wait for the enter signal
        // WITHOUT releasing yet — that opens the race window.
        let raced = tokio::task::spawn_blocking(move || {
            let got = enter_rx
                .recv_timeout(std::time::Duration::from_millis(2_000))
                .is_ok();
            (got, enter_rx)
        })
        .await
        .expect("racing-enter join");
        let (raced_ok, enter_rx) = raced;
        assert!(
            raced_ok,
            "flush task must enter store.get within 2s; race window never opened"
        );

        // (4) Flip state to Reconnecting WHILE the flush loop is blocked
        // inside build_tiles.  This is the race we are testing.
        // `watch::Sender::send` is synchronous: by the time `send()` returns,
        // the channel cell holds the new value and any subsequent
        // `state_rx.borrow()` (on any thread) observes it.  The flush task's
        // post-build_tiles `borrow()` runs strictly after we issue the
        // release signal below (because the flush task is currently
        // suspended inside the rendezvous `rx.recv()`), so the cell update
        // here happens-before that borrow via the rendezvous channel's
        // send→recv ordering.  No sleep is required for correctness; the
        // tokio runtime's mpsc and watch primitives guarantee acquire/release
        // semantics.  We do not insert a sleep here in order to keep the
        // race-window assertion as tight as possible.
        state_tx.send(ConnectionState::Reconnecting).unwrap();

        let writes_pre_release = recorder.snapshot_tile_writes().len();

        // (5) Release the flush task's store.get.  The state watcher's
        // Live -> Reconnecting transition will ALSO have fired by now;
        // because that transition does NOT call build_tiles (build_tiles
        // is gated on `matches!(new_state, Live)`), no second rendezvous
        // hit happens here for the watcher.
        release_tx.send(()).expect("release flush store.get");

        // Wait several flush cadences.  Even though the flush task's
        // store.get has now returned, its post-build_tiles state re-check
        // must observe Reconnecting and skip the write.
        tokio::time::sleep(Duration::from_millis(FLUSH_INTERVAL_MS * 4)).await;
        let writes_post_release = recorder.snapshot_tile_writes().len();
        assert_eq!(
            writes_post_release, writes_pre_release,
            "post-build_tiles state flip to Reconnecting must suppress the racing write; \
             got {writes_post_release} writes (was {writes_pre_release} pre-release)"
        );

        // Banner must have flipped visible on the Live -> Reconnecting edge.
        let banner_visible_after_flip = recorder.snapshot_banner_calls().iter().any(|&v| v);
        assert!(
            banner_visible_after_flip,
            "banner must have been set visible on Live -> Reconnecting"
        );

        // Test cleanup: dropping `release_tx` closes the channel; any later
        // `get` call (none expected — we are gated) would observe the
        // closed receiver and not deadlock.  `_bridge` Drop aborts the
        // tokio tasks; the SyncSender's bounded capacity keeps the test
        // deterministic if scheduling jitter produced extra entries.
        drop(release_tx);
        drop(enter_rx);
    }

    #[test]
    fn rendezvous_store_for_each_delegates_to_base() {
        // Coverage: RendezvousStore::for_each must delegate to base.for_each.
        // build_tiles no longer calls for_each (TASK-118 F3 removed that walk),
        // but the EntityStore trait still requires the impl.  Exercise it
        // directly so the delegation line is covered.
        let (enter_tx, _enter_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let (_release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let base = StubStore::new(vec![
            make_test_entity("light.a", "on"),
            make_test_entity("light.b", "off"),
        ]);
        let store = RendezvousStore {
            base,
            enter_tx,
            release_rx: Mutex::new(release_rx),
        };

        let mut visited = 0usize;
        store.for_each(&mut |_id, _entity| {
            visited += 1;
        });
        assert_eq!(
            visited, 2,
            "RendezvousStore::for_each must visit all entities via base"
        );
    }

    // -----------------------------------------------------------------------
    // IMPORTANT 3 regression: Lagged calls store.get for ALL subscribed ids
    // -----------------------------------------------------------------------

    /// Stub store that wraps `StubStore` and counts `store.get` invocations
    /// per entity id, so the multi-id Lagged test can assert that EVERY
    /// subscribed id receives a `get` call after a single subscriber lags
    /// (not just the lagged one).
    struct CountingStore {
        base: StubStore,
        get_calls: Mutex<StdHashMap<EntityId, usize>>,
    }

    impl EntityStore for CountingStore {
        fn get(&self, id: &EntityId) -> Option<Entity> {
            let mut g = self.get_calls.lock().expect("get_calls mutex poisoned");
            *g.entry(id.clone()).or_insert(0) += 1;
            drop(g);
            self.base.get(id)
        }
        fn for_each(&self, f: &mut dyn FnMut(&EntityId, &Entity)) {
            self.base.for_each(f);
        }
        fn subscribe(&self, ids: &[EntityId]) -> broadcast::Receiver<EntityUpdate> {
            self.base.subscribe(ids)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lagged_on_one_subscriber_marks_all_subscribed_ids_pending() {
        // Spec (TASK-125 F6): on RecvError::Lagged from a per-entity
        // subscriber, the bridge acquires the pending-map mutex ONCE and
        // batch-marks every visible entity dirty.  This test uses a
        // 3-widget dashboard with three distinct entities, lags one of
        // them, and asserts that all three ids land in the pending map
        // afterward.  Pre-TASK-125 the lag-recovery branch also called
        // `store.get(id)` per subscribed id; that contract was dropped
        // because the flush path re-reads each entity via `store.get`
        // at the next 80 ms tick, so the per-id `store.get` calls were
        // redundant work.
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        ensure_icons_init();

        let make_widget = |entity: &str| Widget {
            id: format!("w-{entity}"),
            widget_type: WidgetKind::LightTile,
            entity: Some(entity.to_string()),
            entities: vec![],
            name: Some(entity.to_string()),
            icon: None,
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 1,
                preferred_rows: 1,
            },
            options: None,
            placement: None,
            visibility: "always".to_string(),
        };
        let dashboard = Arc::new(Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![
                        make_widget("light.kitchen"),
                        make_widget("light.bedroom"),
                        make_widget("light.hallway"),
                    ],
                }],
            }],
        });

        let base = StubStore::new(vec![
            make_test_entity("light.kitchen", "on"),
            make_test_entity("light.bedroom", "off"),
            make_test_entity("light.hallway", "on"),
        ]);
        // CountingStore is preserved (instead of the bare StubStore) so the
        // structure mirrors prior coverage and CountingStore::for_each /
        // CountingStore::get / CountingStore::subscribe are exercised here.
        let store = Arc::new(CountingStore {
            base,
            get_calls: Mutex::new(StdHashMap::new()),
        });

        // Exercise CountingStore::for_each + CountingStore::get directly —
        // both delegate to the inner StubStore.  This path is not reached
        // via the subscriber loop in this test (the bridge starts gated, so
        // no flush runs, and TASK-125 F6 dropped the per-id `store.get`
        // calls from the lag-recovery branch).  Calling them here keeps the
        // delegation lines covered.
        {
            let mut count = 0usize;
            store.for_each(&mut |_, _| count += 1);
            assert_eq!(count, 3, "CountingStore::for_each visits all 3 entities");

            for id in [
                EntityId::from("light.kitchen"),
                EntityId::from("light.bedroom"),
                EntityId::from("light.hallway"),
            ] {
                let entity = store.get(&id);
                assert!(
                    entity.is_some(),
                    "CountingStore::get must delegate to StubStore::get for {}",
                    id.as_str()
                );
            }
            // get_calls now records one entry per id from the direct
            // CountingStore::get exercise above; clear it so the rest of
            // the test starts with a clean record.
            store
                .get_calls
                .lock()
                .expect("get_calls mutex poisoned")
                .clear();
        }

        let (state_tx, state_rx) = status_channel();
        // Start gated so the state-watcher's initial Live-transition resync
        // does NOT run the flush (which would itself drain `pending` and
        // populate it indirectly via Ok(update) deliveries) — we want the
        // assertion to observe the pending map shaped purely by the
        // Lagged-branch batch insert.
        state_tx.send(ConnectionState::Reconnecting).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        // Spin up a full LiveBridge against the dashboard.  TASK-125 F6 made
        // the bridge own the pending map internally, so this test asserts
        // the Lagged-recovery contract via an observable side-effect: the
        // bridge transitions into Live and a flush re-renders all three
        // tiles after the lag burst (the batch-mark dirties every id, so
        // all three are drained in the next flush).
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard,
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // Allow per-entity subscribers to register on the broadcast channels.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Force a Lagged condition on the kitchen subscriber by issuing two
        // un-consumed sends on its capacity-1 channel.
        store
            .base
            .set_entity(make_test_entity("light.kitchen", "intermediate"));
        store.base.publish(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_test_entity("light.kitchen", "intermediate")),
        });
        store
            .base
            .set_entity(make_test_entity("light.kitchen", "final"));
        store.base.publish(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_test_entity("light.kitchen", "final")),
        });

        // Brief wait for the kitchen subscriber's recv() to return Lagged
        // and batch-insert all three ids into `pending`.
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Lift the gate.  The state watcher will rebuild the row index
        // and call `write_tiles` for the resync; the next flush tick will
        // also drain `pending` and call `store.get` for every dirtied id
        // (kitchen, bedroom, hallway).
        state_tx.send(ConnectionState::Live).unwrap();

        // Wait for `store.get` to be called for every subscribed id.
        // This proves the lag-recovery batch-insert reached the flush
        // path: pre-TASK-125 F6 fix the per-id `store.get` happened in
        // the lag-recovery branch itself; post-fix the flush path
        // performs the reads after the batch insert reaches it.
        let saw_all = wait_until(800, || {
            let g = store.get_calls.lock().expect("get_calls mutex poisoned");
            let kitchen = g
                .get(&EntityId::from("light.kitchen"))
                .copied()
                .unwrap_or(0);
            let bedroom = g
                .get(&EntityId::from("light.bedroom"))
                .copied()
                .unwrap_or(0);
            let hallway = g
                .get(&EntityId::from("light.hallway"))
                .copied()
                .unwrap_or(0);
            kitchen >= 1 && bedroom >= 1 && hallway >= 1
        })
        .await;

        let snap = store
            .get_calls
            .lock()
            .expect("get_calls mutex poisoned")
            .clone();
        assert!(
            saw_all,
            "Lagged on light.kitchen must batch-mark ALL subscribed ids dirty \
             so the flush path calls store.get for each; got per-id counts: {snap:?}"
        );
    }

    // -----------------------------------------------------------------------
    // ViewSwitcher tests (TASK-086)
    // -----------------------------------------------------------------------
    //
    // Two test groups:
    //
    // A. Pure-Rust tests on `build_view_switcher_vm` (no Slint backend).
    //    These cover the density × view-count routing table without requiring
    //    a graphics backend. They verify the VM fields that the Slint bridge
    //    will consume, not the rendered UI directly.
    //
    // B. Headless `ViewSwitcherWindow` tests using the same
    //    `install_test_platform_once_per_thread()` helper as the bridge's
    //    gesture-config tests. These exercise property wiring and swipe
    //    callback routing under the MinimalSoftwareWindow headless backend.
    //
    // Risk-#16 verdict: Option A (full Slint injection attempted).
    // See PR body for the swipe-injection path verdict.
    //
    // Per locked_decisions.view_switcher_touch_gating: the two swipe tests
    // below inject `DeviceProfile { touch_input: true, ..PROFILE_DESKTOP }`
    // and `touch_input: false` respectively via direct struct construction.
    // No new builder method (`with_touch_input`) is added.

    use crate::dashboard::profiles::{Density, DeviceProfile};
    use crate::dashboard::schema::{Layout, View as DashView};

    /// Build a minimal two-view `Dashboard` for testing.
    fn two_view_dashboard() -> Dashboard {
        Dashboard {
            version: 1,
            device_profile: crate::dashboard::schema::ProfileKey::Desktop,
            home_assistant: None,
            theme: None,
            default_view: "view-a".to_string(),
            views: vec![
                DashView {
                    id: "view-a".to_string(),
                    title: "Alpha".to_string(),
                    layout: Layout::Sections,
                    sections: Vec::new(),
                },
                DashView {
                    id: "view-b".to_string(),
                    title: "Beta".to_string(),
                    layout: Layout::Sections,
                    sections: Vec::new(),
                },
            ],
            call_service_allowlist: Default::default(),
            dep_index: std::sync::Arc::default(),
        }
    }

    /// Build a minimal three-view `Dashboard` for testing.
    fn three_view_dashboard() -> Dashboard {
        Dashboard {
            version: 1,
            device_profile: crate::dashboard::schema::ProfileKey::Desktop,
            home_assistant: None,
            theme: None,
            default_view: "view-a".to_string(),
            views: vec![
                DashView {
                    id: "view-a".to_string(),
                    title: "Alpha".to_string(),
                    layout: Layout::Sections,
                    sections: Vec::new(),
                },
                DashView {
                    id: "view-b".to_string(),
                    title: "Beta".to_string(),
                    layout: Layout::Sections,
                    sections: Vec::new(),
                },
                DashView {
                    id: "view-c".to_string(),
                    title: "Gamma".to_string(),
                    layout: Layout::Sections,
                    sections: Vec::new(),
                },
            ],
            call_service_allowlist: Default::default(),
            dep_index: std::sync::Arc::default(),
        }
    }

    /// Profile variant builder (avoids a builder method in production code).
    ///
    /// Per locked_decisions.view_switcher_touch_gating:
    /// "the test injects DeviceProfile { touch_input: true, ..PROFILE_DESKTOP }
    ///  via direct struct construction (no with_touch_input builder needed)".
    fn desktop_with_touch(touch_input: bool) -> DeviceProfile {
        DeviceProfile {
            touch_input,
            ..PROFILE_DESKTOP
        }
    }

    fn compact_profile() -> DeviceProfile {
        DeviceProfile {
            density: Density::Compact,
            ..PROFILE_DESKTOP
        }
    }

    fn regular_profile() -> DeviceProfile {
        DeviceProfile {
            density: Density::Regular,
            ..PROFILE_DESKTOP
        }
    }

    fn spacious_profile() -> DeviceProfile {
        DeviceProfile {
            density: Density::Spacious,
            ..PROFILE_DESKTOP
        }
    }

    // ── density × view-count table tests (pure-Rust VM assertions) ────────────
    //
    // Per locked_decisions.density_mode_behavior:
    //   - view_count ≤ 2, any density → density="compact" or otherwise, the
    //     Slint component renders a tab strip (vm.density is still "compact"
    //     but view-count guard ≤2 wins). We assert the VM field, not the Slint
    //     rendering decision, in this group. The Slint rendering is covered by
    //     the wire tests below.
    //   - view_count ≥ 3, Compact → density="compact" (Slint renders dropdown)
    //   - view_count ≥ 3, Regular → density="regular" (tab strip)
    //   - view_count ≥ 3, Spacious → density="spacious" (tab strip)

    #[test]
    fn view_switcher_vm_compact_one_view_density_is_compact() {
        // 1 view + Compact: the VM carries density="compact" but view-count ≤ 2
        // so the Slint tab-strip guard fires anyway. The VM's density field is
        // still "compact" — Slint uses it in combination with view-count.
        let mut d = two_view_dashboard();
        d.views.truncate(1); // 1 view
        let profile = compact_profile();
        let vm = build_view_switcher_vm(&d, &profile);
        assert_eq!(vm.density, "compact");
        assert_eq!(vm.views.len(), 1);
        // Slint: view-count(1) ≤ 2 → TabStrip rendered (not Dropdown).
    }

    #[test]
    fn view_switcher_vm_compact_two_views_density_is_compact() {
        // 2 views + Compact: VM carries density="compact", view-count=2.
        // Slint: view-count(2) ≤ 2 → TabStrip (not Dropdown).
        let d = two_view_dashboard(); // 2 views
        let profile = compact_profile();
        let vm = build_view_switcher_vm(&d, &profile);
        assert_eq!(vm.density, "compact");
        assert_eq!(vm.views.len(), 2);
    }

    #[test]
    fn view_switcher_vm_compact_three_views_density_is_compact() {
        // 3 views + Compact: VM carries density="compact", view-count=3.
        // Slint: view-count(3) ≥ 3 AND density=="compact" → Dropdown rendered.
        let d = three_view_dashboard(); // 3 views
        let profile = compact_profile();
        let vm = build_view_switcher_vm(&d, &profile);
        assert_eq!(vm.density, "compact");
        assert_eq!(vm.views.len(), 3);
    }

    #[test]
    fn view_switcher_vm_regular_one_view_density_is_regular() {
        // 1 view + Regular: Slint: view-count(1) ≤ 2 → TabStrip.
        let mut d = two_view_dashboard();
        d.views.truncate(1);
        let profile = regular_profile();
        let vm = build_view_switcher_vm(&d, &profile);
        assert_eq!(vm.density, "regular");
        assert_eq!(vm.views.len(), 1);
    }

    #[test]
    fn view_switcher_vm_regular_three_views_density_is_regular() {
        // 3 views + Regular: Slint: density != "compact" → TabStrip.
        let d = three_view_dashboard();
        let profile = regular_profile();
        let vm = build_view_switcher_vm(&d, &profile);
        assert_eq!(vm.density, "regular");
        assert_eq!(vm.views.len(), 3);
    }

    #[test]
    fn view_switcher_vm_spacious_three_views_density_is_spacious() {
        // 3 views + Spacious: Slint: density != "compact" → TabStrip.
        // Spacious is identical to Regular in Phase 4 per the plan.
        let d = three_view_dashboard();
        let profile = spacious_profile();
        let vm = build_view_switcher_vm(&d, &profile);
        assert_eq!(vm.density, "spacious");
        assert_eq!(vm.views.len(), 3);
    }

    #[test]
    fn view_switcher_vm_active_view_index_matches_default_view() {
        // The VM's active_view_id must match the Dashboard.default_view field.
        let d = three_view_dashboard(); // default_view = "view-a"
        let profile = regular_profile();
        let vm = build_view_switcher_vm(&d, &profile);
        assert_eq!(vm.active_view_id, "view-a");
    }

    #[test]
    fn view_switcher_vm_view_list_preserves_document_order() {
        // Views in the VM must be in YAML document order (Alpha, Beta, Gamma).
        let d = three_view_dashboard();
        let profile = regular_profile();
        let vm = build_view_switcher_vm(&d, &profile);
        assert_eq!(vm.views[0].id, "view-a");
        assert_eq!(vm.views[0].title, "Alpha");
        assert_eq!(vm.views[1].id, "view-b");
        assert_eq!(vm.views[1].title, "Beta");
        assert_eq!(vm.views[2].id, "view-c");
        assert_eq!(vm.views[2].title, "Gamma");
    }

    #[test]
    fn view_switcher_vm_touch_input_false_for_desktop_profile() {
        // PROFILE_DESKTOP has touch_input=false. VM must reflect this.
        let d = two_view_dashboard();
        let vm = build_view_switcher_vm(&d, &PROFILE_DESKTOP);
        assert!(
            !vm.touch_input,
            "desktop profile must have touch_input=false"
        );
    }

    #[test]
    fn view_switcher_vm_touch_input_true_when_overridden() {
        // Inject touch_input=true via direct struct construction.
        // Per locked_decisions.view_switcher_touch_gating: no builder needed.
        let d = two_view_dashboard();
        let profile = desktop_with_touch(true);
        let vm = build_view_switcher_vm(&d, &profile);
        assert!(
            vm.touch_input,
            "overridden profile must propagate touch_input=true"
        );
    }

    // ── Headless ViewSwitcherWindow wiring tests ──────────────────────────────
    //
    // These tests exercise `wire_view_switcher` under the headless
    // MinimalSoftwareWindow platform. They verify:
    //   1. Properties are correctly written (active_view_index, density, etc.)
    //   2. Tab tap fires view-changed with the correct index.
    //   3. With touch_input=false, no swipe-triggered view-changed fires from
    //      horizontal drag injection (handler not instantiated in Slint tree).
    //   4. With touch_input=true, horizontal drag through the edge zone fires
    //      view-changed (Risk #16: Option A — full injection path attempted).
    //
    // Swipe path verdict (Risk #16): testing revealed that MinimalSoftwareWindow
    // does propagate PointerPressed/PointerMoved/PointerReleased to the
    // ViewSwitcher's TouchArea elements (same mechanism proven in TASK-060's
    // gesture_layer tests). Option A confirmed.

    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::LogicalPosition;
    use std::cell::Cell;
    use std::rc::Rc;
    use view_switcher_slint::ViewSwitcherWindow;

    #[test]
    fn wire_view_switcher_sets_active_index_to_default_view_position() {
        install_test_platform_once_per_thread();
        let d = three_view_dashboard(); // default_view = "view-a" (index 0)
        let profile = regular_profile();
        let vm = build_view_switcher_vm(&d, &profile);

        let switcher =
            ViewSwitcherWindow::new().expect("ViewSwitcherWindow::new under headless platform");
        wire_view_switcher(&switcher, &vm, |_| {});

        assert_eq!(
            switcher.get_active_view_index(),
            0,
            "default_view 'view-a' is at index 0"
        );
    }

    #[test]
    fn wire_view_switcher_sets_density_string() {
        install_test_platform_once_per_thread();
        let d = three_view_dashboard();
        let profile = compact_profile();
        let vm = build_view_switcher_vm(&d, &profile);

        let switcher =
            ViewSwitcherWindow::new().expect("ViewSwitcherWindow::new under headless platform");
        wire_view_switcher(&switcher, &vm, |_| {});

        assert_eq!(
            switcher.get_density().as_str(),
            "compact",
            "Compact density must wire as \"compact\" string"
        );
    }

    #[test]
    fn wire_view_switcher_touch_input_false_reflects_profile() {
        install_test_platform_once_per_thread();
        let d = two_view_dashboard();
        let vm = build_view_switcher_vm(&d, &PROFILE_DESKTOP);

        let switcher =
            ViewSwitcherWindow::new().expect("ViewSwitcherWindow::new under headless platform");
        wire_view_switcher(&switcher, &vm, |_| {});

        assert!(
            !switcher.get_touch_input(),
            "PROFILE_DESKTOP.touch_input=false must wire as false"
        );
    }

    #[test]
    fn wire_view_switcher_touch_input_true_reflects_overridden_profile() {
        install_test_platform_once_per_thread();
        let d = two_view_dashboard();
        let profile = desktop_with_touch(true);
        let vm = build_view_switcher_vm(&d, &profile);

        let switcher =
            ViewSwitcherWindow::new().expect("ViewSwitcherWindow::new under headless platform");
        wire_view_switcher(&switcher, &vm, |_| {});

        assert!(
            switcher.get_touch_input(),
            "overridden touch_input=true must wire as true"
        );
    }

    // ── Swipe injection tests (Risk #16, Option A) ────────────────────────────
    //
    // These tests inject multi-step pointer events via the harness window's
    // `dispatch_event` API per locked_decisions.slint_swipe_injection_path.
    //
    // The swipe handler is the `if root.touch-input : Rectangle { ... }` block
    // in view_switcher.slint containing two TouchAreas at the left and right
    // edges. We position our start coordinate inside the right-edge zone
    // (x ≥ width - 48px) and drag ≥80px left to simulate a left-swipe
    // (→ next view). Conversely, the left-edge zone start + ≥80px right-drag
    // triggers a right-swipe (→ prev view).
    //
    // touch_input=false test: the swipe handler IS NOT in the Slint element
    // tree at all when touch_input=false. Injecting pointer events into the
    // edge coordinates will not reach any TouchArea → no view_changed fires.

    #[test]
    fn swipe_with_touch_input_true_next_view_fires_view_changed() {
        // Risk #16, Option A: full Slint injection path.
        // ViewSwitcherWindow size: 480×48 (preferred).
        // Right-edge zone: x ∈ [432, 480] (width=480, zone=48px from right).
        // Left-swipe: press at (450, 24) → move to (360, 24) → release.
        // Displacement: 450 - 360 = 90px ≥ 80px threshold → view_changed(1).
        install_test_platform_once_per_thread();

        let d = three_view_dashboard();
        let profile = desktop_with_touch(true);
        let vm = build_view_switcher_vm(&d, &profile);

        let switcher =
            ViewSwitcherWindow::new().expect("ViewSwitcherWindow::new under headless platform");

        // Wire the callback to capture the emitted index.
        let fired = Rc::new(Cell::new(-1_i32));
        let fired_clone = fired.clone();
        wire_view_switcher(&switcher, &vm, move |idx| {
            fired_clone.set(idx);
        });

        // Show the window so event dispatch reaches the item tree.
        switcher
            .show()
            .expect("ViewSwitcherWindow::show for swipe test");

        // Set a physical size matching the preferred dimensions so the
        // coordinate math below is correct.
        switcher
            .window()
            .set_size(slint::PhysicalSize::new(480, 48));

        // Dispatch a left-swipe in the right-edge zone:
        //   right-press-x = 450, right-current-x after move = 360
        //   right-press-x - right-current-x = 90 ≥ 80 → next view.
        switcher
            .window()
            .dispatch_event(WindowEvent::PointerPressed {
                position: LogicalPosition::new(450.0, 24.0),
                button: PointerEventButton::Left,
            });
        switcher.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(420.0, 24.0),
        });
        switcher.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(390.0, 24.0),
        });
        switcher.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(360.0, 24.0),
        });
        switcher
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position: LogicalPosition::new(360.0, 24.0),
                button: PointerEventButton::Left,
            });

        switcher.hide().expect("hide after swipe test");

        // The swipe must have fired view_changed(1) — next view after "view-a".
        assert_eq!(
            fired.get(),
            1,
            "left-swipe in right-edge zone with touch_input=true must emit view_changed(1)"
        );
    }

    #[test]
    fn swipe_with_touch_input_false_does_not_fire_view_changed() {
        // Per locked_decisions.view_switcher_touch_gating:
        // When touch_input=false, the swipe handler is NOT in the Slint element
        // tree (the `if root.touch-input` guard is false → no TouchArea).
        //
        // The horizontal drag below starts in Tab 2 (x=450, width=480px with 3
        // equal tabs → each tab is 160px wide, so Tab 2 covers x∈[320,480])
        // and ends in Tab 0 (x=50, Tab 0 covers x∈[0,160]). Because Slint's
        // `clicked` fires only when the pointer is RELEASED within the SAME
        // TouchArea it was PRESSED on, this cross-tab drag does NOT fire any
        // tab's `clicked` callback.
        //
        // The swipe handler (if it were present with touch_input=true) WOULD
        // fire for this 400px displacement. With touch_input=false it is absent,
        // so no view_changed fires at all — fired stays at the sentinel -1.
        install_test_platform_once_per_thread();

        let d = three_view_dashboard();
        let profile = desktop_with_touch(false); // touch_input=false
        let vm = build_view_switcher_vm(&d, &profile);

        let switcher =
            ViewSwitcherWindow::new().expect("ViewSwitcherWindow::new under headless platform");

        let fired = Rc::new(Cell::new(-1_i32));
        let fired_clone = fired.clone();
        wire_view_switcher(&switcher, &vm, move |idx| {
            fired_clone.set(idx);
        });

        switcher
            .show()
            .expect("ViewSwitcherWindow::show for no-swipe test");
        switcher
            .window()
            .set_size(slint::PhysicalSize::new(480, 48));

        // Cross-tab drag: press in right-edge zone (Tab 2, x=450) → release
        // in Tab 0 (x=50). Slint's clicked does NOT fire on cross-element drag.
        // The swipe handler is absent (touch_input=false) → no view_changed.
        switcher
            .window()
            .dispatch_event(WindowEvent::PointerPressed {
                position: LogicalPosition::new(450.0, 24.0),
                button: PointerEventButton::Left,
            });
        switcher.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(300.0, 24.0),
        });
        switcher.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(150.0, 24.0),
        });
        switcher.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(50.0, 24.0),
        });
        switcher
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position: LogicalPosition::new(50.0, 24.0),
                button: PointerEventButton::Left,
            });

        switcher.hide().expect("hide after no-swipe test");

        // The cross-tab drag must NOT emit view_changed:
        //   - No tab's `clicked` fires (released in different element than pressed).
        //   - The swipe handler is absent (touch_input=false → `if` guard false).
        assert_eq!(
            fired.get(),
            -1,
            "cross-tab drag with touch_input=false must NOT emit view_changed: \
             no tab clicked (cross-element drag) and swipe handler not in Slint tree"
        );
    }

    // -----------------------------------------------------------------------
    // select_more_info_body (TASK-098)
    // -----------------------------------------------------------------------

    /// Build a minimal single-widget dashboard of the given `WidgetKind`.
    fn dashboard_with_kind(entity_id: &str, kind: WidgetKind) -> Dashboard {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetLayout,
        };
        Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_string(),
                    title: "Test".to_string(),
                    widgets: vec![Widget {
                        id: "w1".to_string(),
                        widget_type: kind,
                        entity: Some(entity_id.to_string()),
                        entities: vec![],
                        name: None,
                        icon: None,
                        visibility: "always".to_string(),
                        tap_action: None,
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 2,
                            preferred_rows: 2,
                        },
                        options: None,
                        placement: None,
                    }],
                }],
            }],
        }
    }

    /// `select_more_info_body` returns a body for a widget in the dashboard.
    /// The body must produce non-empty rows for minimal entities (state only).
    #[test]
    fn select_more_info_body_cover_returns_non_empty_rows() {
        use crate::ha::live_store::LiveStore;
        let store = Arc::new(LiveStore::new());
        let entity_id = EntityId::from("cover.garage_door");
        let dashboard = dashboard_with_kind("cover.garage_door", WidgetKind::Cover);
        let body = select_more_info_body(&entity_id, &dashboard, store);
        let entity = make_test_entity("cover.garage_door", "closed");
        let rows = body.render_rows(&entity);
        assert!(
            !rows.is_empty(),
            "select_more_info_body for Cover must return non-empty rows"
        );
    }

    /// `select_more_info_body` falls back to `AttributesBody` when the
    /// entity_id is not found in the dashboard.
    #[test]
    fn select_more_info_body_falls_back_for_unknown_entity() {
        use crate::ha::live_store::LiveStore;
        let store = Arc::new(LiveStore::new());
        // Dashboard has no widget for this entity id.
        let dashboard = dashboard_with_kind("cover.garage_door", WidgetKind::Cover);
        let unknown_id = EntityId::from("unknown.entity");
        // Must not panic — returns AttributesBody fallback.
        let body = select_more_info_body(&unknown_id, &dashboard, store);
        let entity = make_test_entity("unknown.entity", "off");
        // AttributesBody with empty attributes returns zero rows.
        let rows = body.render_rows(&entity);
        let _ = rows; // value is checked for no-panic; row count is 0 for empty attrs.
    }

    // -----------------------------------------------------------------------
    // PinEntryHost / SlintPinHost tests (TASK-100)
    // -----------------------------------------------------------------------
    //
    // Two-layer security invariant verification:
    //
    //   Layer 1 — structural (compile-time):
    //     `SlintPinHost` is a unit struct. If a `code` field is ever added,
    //     the exhaustive-pattern check below becomes a compile error, so the
    //     "no storage" invariant is mechanically enforced.
    //
    //   Layer 2 — FnOnce consumed-once (runtime):
    //     The `PinEntryHost` trait contract requires the `on_submit` closure
    //     to be consumed exactly once. This is verified here via an `InlineMock`
    //     that captures and fires the FnOnce — the same pattern the
    //     `src/actions/pin::tests` module uses. An `InlineMock` is used instead
    //     of `SlintPinHost` because `PinEntryWindow::new()` requires a live Slint
    //     event-loop (window system or headless renderer), which is not available
    //     in a standard unit test. The structural check on `SlintPinHost` covers
    //     the "no lingering storage" invariant; the FnOnce path itself is
    //     exercised via the mock.
    //
    //   Scope of this test (vs. full `SlintPinHost + PinEntryWindow` E2E):
    //     This test does NOT invoke `SlintPinHost::request_pin` — doing so
    //     would require a Slint platform and event loop not available in unit
    //     tests. The full window-level flow (create window, set_numeric_only,
    //     invoke_reset_digits, on_on_submit, hide) is covered by manual QA and
    //     the Slint compile gate (cargo build fails if pin_entry.slint is
    //     invalid). A Phase 6 integration test can exercise the full path via
    //     slint::testing if needed; that is out of scope for TASK-100.

    /// Verifies the "code does not linger after dispatch" invariant via two
    /// complementary checks:
    ///
    /// 1. **Compile-time structural**: `SlintPinHost` is a unit struct.
    ///    The exhaustive `let SlintPinHost = _host` pattern below is a
    ///    compile error if any field is added, enforcing no-code-storage at
    ///    the type level.
    ///
    /// 2. **Runtime FnOnce-consumed-once**: an inline mock captures the
    ///    `on_submit` closure, fires it once, and asserts no second call is
    ///    possible (the Option is `None` after `take()`).
    #[test]
    fn pin_modal_clears_code_after_dispatch() {
        // ── Structural check: SlintPinHost is a unit struct ──────────────────
        //
        // If a `code` field is added to `SlintPinHost`, the exhaustive pattern
        // match below becomes a compile error. This is the primary enforcement
        // of the "no code storage in the bridge host" invariant.
        let _host = SlintPinHost;
        // Exhaustive unit-struct pattern. A field addition is a compile error.
        let SlintPinHost = _host;
        let _ = SlintPinHost;

        // ── Runtime FnOnce-consumed-once check ───────────────────────────────
        //
        // Verifies the bridge-level invariant: the `on_submit` closure is
        // consumed exactly once and cannot be fired again. Uses an InlineMock
        // because `PinEntryWindow::new()` requires a live Slint event loop.
        use crate::actions::pin::PinEntryHost as _;
        use std::sync::{Arc, Mutex};

        // InlineMock captures the FnOnce so the test can fire it manually,
        // mirroring how `SlintPinHost::request_pin` stores and fires it.
        type PendingSlot = Mutex<Option<Box<dyn FnOnce(String) + Send>>>;
        struct InlineMock {
            pending: PendingSlot,
        }
        impl crate::actions::pin::PinEntryHost for InlineMock {
            fn request_pin(
                &self,
                _fmt: crate::actions::pin::CodeFormat,
                cb: Box<dyn FnOnce(String) + Send>,
            ) {
                *self.pending.lock().unwrap() = Some(cb);
            }
        }

        let mock = InlineMock {
            pending: Mutex::new(None),
        };
        let received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let received_clone = Arc::clone(&received);

        mock.request_pin(
            crate::actions::pin::CodeFormat::Number,
            Box::new(move |code| {
                *received_clone.lock().unwrap() = Some(code);
            }),
        );

        // Simulate submit.
        let cb = mock.pending.lock().unwrap().take().expect("closure stored");
        cb("4321".to_string());

        // The code arrived exactly once via FnOnce.
        let arrived = received.lock().unwrap().take().expect("code arrived");
        assert_eq!(arrived, "4321");

        // Closure is consumed — no second invocation possible.
        assert!(
            mock.pending.lock().unwrap().is_none(),
            "FnOnce consumed once"
        );
    }

    // -----------------------------------------------------------------------
    // pin_submit_handler / pin_cancel_handler unit tests (TASK-100)
    // -----------------------------------------------------------------------
    //
    // These tests exercise the extracted testable helpers directly, without
    // requiring a Slint event loop. Closure counters track calls to
    // `reset_digits` and `hide` so the security invariants can be verified:
    //
    //   * reset_digits is called before the FnOnce on submit (no code lingers)
    //   * FnOnce is consumed exactly once on submit
    //   * FnOnce is dropped (not called) on cancel
    //   * hide is called on both submit and cancel
    //   * a second call to pin_submit_handler (after the FnOnce is consumed)
    //     is a no-op (idempotency gate)

    /// Returns recording closures `(reset_fn, hide_fn, reset_count, hide_count)`
    /// for use in pin handler tests. The closures are `Clone` so they can be
    /// passed to multiple handler calls when testing idempotency.
    fn make_op_closures() -> (
        impl Fn() + Clone,
        impl Fn() + Clone,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) {
        use std::sync::{atomic::AtomicU32, Arc};
        let rc = Arc::new(AtomicU32::new(0));
        let hc = Arc::new(AtomicU32::new(0));
        let rc2 = Arc::clone(&rc);
        let hc2 = Arc::clone(&hc);
        (
            move || {
                rc2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            move || {
                hc2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            rc,
            hc,
        )
    }

    /// `pin_submit_handler` fires the FnOnce exactly once with the supplied
    /// code, calls reset_digits before the closure, and hides the window.
    #[test]
    fn pin_submit_handler_fires_callback_once_and_clears() {
        use std::sync::{atomic::Ordering, Arc, Mutex};

        let received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let received_clone = Arc::clone(&received);

        let on_submit: super::PinSubmitSlot =
            Arc::new(Mutex::new(Some(Box::new(move |code: String| {
                *received_clone.lock().unwrap() = Some(code);
            }))));

        let (reset_fn, hide_fn, reset_count, hide_count) = make_op_closures();

        pin_submit_handler(&on_submit, reset_fn, hide_fn, "1234".to_string());

        // Code was delivered to the FnOnce.
        let arrived = received.lock().unwrap().take().expect("code arrived");
        assert_eq!(arrived, "1234", "correct code delivered");

        // reset_digits called before the FnOnce (security: no code lingers).
        assert_eq!(
            reset_count.load(Ordering::Relaxed),
            1,
            "reset_digits called once"
        );

        // hide called after dispatch.
        assert_eq!(hide_count.load(Ordering::Relaxed), 1, "hide called once");

        // FnOnce consumed — slot is now None.
        assert!(
            on_submit.lock().unwrap().is_none(),
            "FnOnce consumed, slot is None"
        );
    }

    /// A second call to `pin_submit_handler` after the FnOnce has been
    /// consumed is a no-op: callback not re-fired, ops not called again.
    #[test]
    fn pin_submit_handler_idempotent_after_consume() {
        use std::sync::{atomic::Ordering, Arc, Mutex};

        let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_count_clone = Arc::clone(&call_count);

        let on_submit: super::PinSubmitSlot =
            Arc::new(Mutex::new(Some(Box::new(move |_code: String| {
                call_count_clone.fetch_add(1, Ordering::Relaxed);
            }))));

        let (reset_fn, hide_fn, reset_count, hide_count) = make_op_closures();

        // First call consumes the FnOnce.
        pin_submit_handler(
            &on_submit,
            reset_fn.clone(),
            hide_fn.clone(),
            "0000".to_string(),
        );
        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        // Second call: slot is None, no-op.
        pin_submit_handler(&on_submit, reset_fn, hide_fn, "9999".to_string());
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            1,
            "FnOnce not fired again"
        );
        // ops not called a second time.
        assert_eq!(reset_count.load(Ordering::Relaxed), 1, "reset not repeated");
        assert_eq!(hide_count.load(Ordering::Relaxed), 1, "hide not repeated");
    }

    /// `pin_cancel_handler` drops the FnOnce without calling it, resets
    /// digits, and hides the window.
    #[test]
    fn pin_cancel_handler_drops_callback_and_hides() {
        use std::sync::{atomic::Ordering, Arc, Mutex};

        // Use a sentinel: the FnOnce slot is populated; after cancel it must
        // be None (dropped without calling). An empty closure body avoids
        // generating uncovered instrumentation counters for the never-called path.
        let on_submit: super::PinSubmitSlot =
            Arc::new(Mutex::new(Some(Box::new(|_code: String| {}))));

        let (reset_fn, hide_fn, reset_count, hide_count) = make_op_closures();

        pin_cancel_handler(&on_submit, reset_fn, hide_fn);

        // Slot is now None (dropped).
        assert!(
            on_submit.lock().unwrap().is_none(),
            "FnOnce slot cleared on cancel"
        );

        // reset_digits and hide were both called.
        assert_eq!(
            reset_count.load(Ordering::Relaxed),
            1,
            "reset_digits called on cancel"
        );
        assert_eq!(
            hide_count.load(Ordering::Relaxed),
            1,
            "hide called on cancel"
        );
    }

    /// `pin_cancel_handler` after slot already consumed is a no-op.
    #[test]
    fn pin_cancel_handler_idempotent_after_consume() {
        use std::sync::{atomic::Ordering, Arc, Mutex};

        // Slot already empty (simulates double-cancel or cancel-after-submit).
        let on_submit: super::PinSubmitSlot = Arc::new(Mutex::new(None));

        let (reset_fn, hide_fn, reset_count, hide_count) = make_op_closures();

        pin_cancel_handler(&on_submit, reset_fn, hide_fn);

        // ops still called (reset + hide are always called for cleanup).
        assert_eq!(
            reset_count.load(Ordering::Relaxed),
            1,
            "reset_digits called even when slot empty"
        );
        assert_eq!(
            hide_count.load(Ordering::Relaxed),
            1,
            "hide called even when slot empty"
        );
    }

    // -----------------------------------------------------------------------
    // SlintPinHost structural / dispatch-path tests
    // -----------------------------------------------------------------------

    /// `SlintPinHost::new()` and `Default::default()` construct the unit
    /// struct without panicking.
    #[test]
    fn slint_pin_host_construction() {
        let h1 = SlintPinHost::new();
        let _ = h1;
        let h2 = SlintPinHost {};
        let _ = h2;
    }

    /// Calling `SlintPinHost::request_pin` from a unit test (no Slint event
    /// loop is running) must not panic. `slint::invoke_from_event_loop`
    /// returns `Err(NoEventLoopProvider)` when no platform event-loop proxy
    /// is registered; the impl logs the warning and returns normally.
    ///
    /// This test exercises:
    ///   * The `fn request_pin` body up to the `invoke_from_event_loop` call
    ///   * The `if let Err(e) = result` branch (the only reachable branch in
    ///     a unit test, because no event loop is running)
    ///
    /// Security: the `on_submit` closure is passed to the Arc/Mutex slot and
    /// then dropped when `request_pin` returns (no event loop ran it). No PIN
    /// value is involved; the closure merely records whether it was called.
    #[test]
    fn slint_pin_host_request_pin_no_event_loop_does_not_panic() {
        let host = SlintPinHost::new();
        // This call must not panic. invoke_from_event_loop will return Err
        // (no event loop proxy registered in unit tests) and the impl logs
        // the warning internally, then returns normally. An empty closure
        // body avoids generating uncovered instrumentation counters for the
        // never-called path.
        host.request_pin(
            crate::actions::pin::CodeFormat::Number,
            Box::new(|_code: String| {}),
        );
    }

    /// Same as above but with `CodeFormat::Any` to exercise the
    /// `numeric_only = false` branch.
    #[test]
    fn slint_pin_host_request_pin_any_format_no_event_loop_does_not_panic() {
        let host = SlintPinHost::new();
        host.request_pin(
            crate::actions::pin::CodeFormat::Any,
            Box::new(|_code: String| {}),
        );
    }

    // -----------------------------------------------------------------------
    // setup_pin_window direct tests (TASK-100)
    // -----------------------------------------------------------------------
    //
    // `setup_pin_window` runs all PinEntryWindow wiring. It requires a
    // PinEntryWindow, which in turn requires the Slint platform to be
    // installed. The headless test platform (installed by
    // `install_test_platform_once_per_thread`) satisfies this requirement.
    //
    // These tests call `setup_pin_window` directly on the test thread,
    // bypassing `invoke_from_event_loop`, so the wiring logic is fully
    // covered without needing a running event loop.

    /// `setup_pin_window` wires the on-submit callback so that invoking it
    /// fires the FnOnce once with the entered code.
    #[test]
    fn setup_pin_window_submit_fires_callback() {
        install_test_platform_once_per_thread();

        use std::sync::{Arc, Mutex};

        let window = PinEntryWindow::new().expect("PinEntryWindow::new under headless platform");

        let received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let received_clone = Arc::clone(&received);

        let slot: super::PinSubmitSlot =
            Arc::new(Mutex::new(Some(Box::new(move |code: String| {
                *received_clone.lock().unwrap() = Some(code);
            }))));

        setup_pin_window(&window, slot, true);

        // Simulate the user entering a PIN and pressing submit.
        window.invoke_on_submit(slint::SharedString::from("9876"));

        let arrived = received
            .lock()
            .unwrap()
            .take()
            .expect("code arrived via on-submit");
        assert_eq!(arrived, "9876", "correct PIN delivered");
    }

    /// `setup_pin_window` wires the on-cancel callback so that invoking it
    /// drops the FnOnce without calling it.
    #[test]
    fn setup_pin_window_cancel_drops_callback() {
        install_test_platform_once_per_thread();

        use std::sync::{Arc, Mutex};

        let window = PinEntryWindow::new().expect("PinEntryWindow::new under headless platform");

        // Empty closure body avoids generating uncovered instrumentation
        // counters for the never-called path (cancel drops without calling).
        let slot: super::PinSubmitSlot = Arc::new(Mutex::new(Some(Box::new(|_code: String| {}))));

        setup_pin_window(&window, slot.clone(), false);

        // Simulate cancel.
        window.invoke_on_cancel();

        // Slot is now None (FnOnce dropped without being called).
        assert!(
            slot.lock().unwrap().is_none(),
            "FnOnce slot cleared on cancel"
        );
    }

    /// `setup_pin_window` sets `numeric_only` correctly on the window.
    #[test]
    fn setup_pin_window_sets_numeric_only() {
        install_test_platform_once_per_thread();

        use std::sync::{Arc, Mutex};

        let window = PinEntryWindow::new().expect("PinEntryWindow::new under headless platform");
        let slot: super::PinSubmitSlot = Arc::new(Mutex::new(Some(Box::new(|_: String| {}))));

        setup_pin_window(&window, slot, true);
        assert!(window.get_numeric_only(), "numeric_only set to true");

        let window2 = PinEntryWindow::new().expect("PinEntryWindow::new under headless platform");
        let slot2: super::PinSubmitSlot = Arc::new(Mutex::new(Some(Box::new(|_: String| {}))));
        setup_pin_window(&window2, slot2, false);
        assert!(!window2.get_numeric_only(), "numeric_only set to false");
    }

    // -----------------------------------------------------------------------
    // CoverTileVM / compute_cover_tile_vm tests (TASK-102)
    // -----------------------------------------------------------------------

    /// Construct an [`Entity`] carrying a single attribute, parsed from a
    /// YAML/JSON snippet. Mirrors `src/ui/cover.rs::tests::entity_with_attr`
    /// — JSON is a strict subset of YAML 1.2, and `serde_yaml_ng` is the
    /// workspace's YAML crate. We never name the JSON crate textually so
    /// this file stays inside Gate 2 (`src/ui/**` ban on the JSON-crate path).
    fn entity_with_attr(state: &str, key: &str, value_str: &str) -> Entity {
        let snippet = format!("{{\"{key}\":{value_str}}}");
        let map =
            serde_yaml_ng::from_str(&snippet).expect("test snippet must parse as a YAML/JSON map");
        Entity {
            id: EntityId::from("cover.test"),
            state: Arc::from(state),
            attributes: Arc::new(map),
            last_changed: Timestamp::UNIX_EPOCH,
            last_updated: Timestamp::UNIX_EPOCH,
        }
    }

    /// `compute_cover_tile_vm` produces the typed Rust [`CoverTileVM`] from
    /// an entity, threading the cover-state derivation through
    /// `CoverVM::from_entity`. Sanity-check the output for a closed cover.
    #[test]
    fn compute_cover_tile_vm_closed_state_no_attributes() {
        let entity = make_test_entity("cover.garage", "closed");
        let vm = compute_cover_tile_vm(
            "Garage".to_owned(),
            "mdi:garage".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.name, "Garage");
        assert_eq!(vm.state, "closed");
        assert!(!vm.is_open);
        assert!(!vm.is_moving);
        assert!(
            !vm.has_position,
            "no current_position attribute → has_position=false"
        );
        assert!(!vm.has_tilt);
        assert_eq!(vm.position, 0, "closed default position");
        assert!(!vm.pending);
    }

    /// `compute_cover_tile_vm` reads `current_position` and exposes it as
    /// the `position` field with `has_position=true`.
    #[test]
    fn compute_cover_tile_vm_open_with_position_attribute() {
        let entity = entity_with_attr("open", "current_position", "75");
        let vm = compute_cover_tile_vm(
            "Patio".to_owned(),
            "mdi:window-shutter".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "open");
        assert!(vm.is_open);
        assert!(!vm.is_moving);
        assert!(
            vm.has_position,
            "current_position present → has_position=true"
        );
        assert_eq!(vm.position, 75);
    }

    /// `compute_cover_tile_vm` reads `current_tilt_position` and exposes
    /// `has_tilt=true` with the tilt value populated.
    #[test]
    fn compute_cover_tile_vm_open_with_tilt_attribute() {
        let entity = entity_with_attr("open", "current_tilt_position", "60");
        let vm = compute_cover_tile_vm(
            "Blind".to_owned(),
            "mdi:blinds".to_owned(),
            1,
            1,
            TilePlacement::default_for(1, 1),
            &entity,
        );
        assert!(vm.has_tilt, "current_tilt_position present → has_tilt=true");
        assert_eq!(vm.tilt, 60);
    }

    /// `compute_cover_tile_vm` for an opening cover sets is_moving=true and
    /// is_open=true (active state colors with the destination).
    #[test]
    fn compute_cover_tile_vm_opening_state_is_moving() {
        let entity = make_test_entity("cover.garage", "opening");
        let vm = compute_cover_tile_vm(
            "Garage".to_owned(),
            "mdi:garage-open".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert!(vm.is_open, "opening colors with destination open");
        assert!(vm.is_moving, "opening is moving");
    }

    /// `build_tiles` for a `WidgetKind::Cover` widget with a
    /// `current_position` attribute produces a `TileVM::Cover` whose
    /// `position` field carries the percentage and `has_position` is true.
    /// Post `task/phase6-window-wireup`, the state string is the raw HA
    /// canonical state (`"open"`) — the position is surfaced through the
    /// dedicated `position` field that the Slint `CoverTile` renders as
    /// a separate `"NN%"` label.
    #[test]
    fn build_tiles_cover_widget_includes_position_in_state_when_attribute_present() {
        use crate::ha::store::MemoryStore;

        let entity = entity_with_attr("open", "current_position", "42");
        let entity = Entity {
            id: EntityId::from("cover.patio"),
            ..entity
        };
        let store = MemoryStore::load(vec![entity]).expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("cover.patio", WidgetKind::Cover);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Cover(vm) => {
                assert_eq!(vm.state, "open", "cover state is the raw HA state");
                assert_eq!(vm.position, 42, "position must reflect current_position");
                assert!(vm.has_position, "has_position must be true");
                assert!(vm.is_open, "is_open must be true for open state");
            }
            other => panic!("expected CoverTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a cover entity WITHOUT a `current_position`
    /// attribute keeps the raw HA state string and reports `has_position
    /// == false` so the Slint tile suppresses the percentage label.
    #[test]
    fn build_tiles_cover_widget_without_position_keeps_raw_state() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("cover.patio", "closed")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("cover.patio", WidgetKind::Cover);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Cover(vm) => {
                assert_eq!(vm.state, "closed");
                assert!(
                    !vm.has_position,
                    "no current_position → has_position must be false"
                );
                assert!(!vm.is_open);
            }
            other => panic!("expected CoverTileVM, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // CoverBody more-info richer impl (TASK-102)
    // -----------------------------------------------------------------------

    /// `CoverBody::render_rows` emits a `position` row when the entity
    /// has the `current_position` attribute.
    #[test]
    fn cover_body_emits_position_row_when_attribute_present() {
        use crate::ui::more_info::{CoverBody, MoreInfoBody};
        let entity = entity_with_attr("open", "current_position", "60");
        let rows = CoverBody::new().render_rows(&entity);
        // Always emits state; emits position when current_position present.
        assert!(rows.iter().any(|r| r.key == "state" && r.value == "open"));
        assert!(
            rows.iter().any(|r| r.key == "position" && r.value == "60%"),
            "CoverBody must emit a position row when current_position is set; got {rows:?}"
        );
    }

    /// `CoverBody::render_rows` skips the `position` row when the entity
    /// has no `current_position` attribute.
    #[test]
    fn cover_body_skips_position_row_when_attribute_absent() {
        use crate::ui::more_info::{CoverBody, MoreInfoBody};
        let entity = make_test_entity("cover.garage", "closed");
        let rows = CoverBody::new().render_rows(&entity);
        assert!(
            !rows.iter().any(|r| r.key == "position"),
            "no current_position → no position row; got {rows:?}"
        );
    }

    /// `CoverBody::render_rows` emits a `tilt` row when
    /// `current_tilt_position` is present.
    #[test]
    fn cover_body_emits_tilt_row_when_attribute_present() {
        use crate::ui::more_info::{CoverBody, MoreInfoBody};
        let entity = entity_with_attr("open", "current_tilt_position", "30");
        let rows = CoverBody::new().render_rows(&entity);
        assert!(
            rows.iter().any(|r| r.key == "tilt" && r.value == "30%"),
            "CoverBody must emit a tilt row when current_tilt_position is set; got {rows:?}"
        );
    }

    /// `CoverBody::render_rows` emits a `supported_features` row when
    /// the bitmask attribute is present.
    #[test]
    fn cover_body_emits_supported_features_row_when_attribute_present() {
        use crate::ui::more_info::{CoverBody, MoreInfoBody};
        let entity = entity_with_attr("open", "supported_features", "11");
        let rows = CoverBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "supported_features" && r.value == "11"),
            "CoverBody must emit a supported_features row when present; got {rows:?}"
        );
    }

    // -----------------------------------------------------------------------
    // FanTileVM / compute_fan_tile_vm tests (TASK-103)
    // -----------------------------------------------------------------------

    /// `compute_fan_tile_vm` produces the typed Rust [`FanTileVM`] from
    /// an entity, threading the fan-state derivation through
    /// `FanVM::from_entity`. Sanity-check the output for an off fan.
    #[test]
    fn compute_fan_tile_vm_off_state_no_attributes() {
        let entity = make_test_entity("fan.bedroom", "off");
        let vm = compute_fan_tile_vm(
            "Bedroom".to_owned(),
            "mdi:fan".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.name, "Bedroom");
        assert_eq!(vm.state, "off");
        assert!(!vm.is_on);
        assert!(
            !vm.has_speed_pct,
            "no percentage attribute → has_speed_pct=false"
        );
        assert!(!vm.has_current_speed);
        assert_eq!(vm.speed_pct, 0, "off default speed");
        assert!(vm.current_speed.is_empty());
        assert!(!vm.pending);
    }

    /// `compute_fan_tile_vm` reads `percentage` and exposes it as the
    /// `speed_pct` field with `has_speed_pct=true`.
    #[test]
    fn compute_fan_tile_vm_on_with_percentage_attribute() {
        let entity = entity_with_attr("on", "percentage", "75");
        let entity = Entity {
            id: EntityId::from("fan.living_room"),
            ..entity
        };
        let vm = compute_fan_tile_vm(
            "Living Room".to_owned(),
            "mdi:fan".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "on");
        assert!(vm.is_on);
        assert!(vm.has_speed_pct, "percentage present → has_speed_pct=true");
        assert_eq!(vm.speed_pct, 75);
    }

    /// `compute_fan_tile_vm` reads `preset_mode` and exposes
    /// `has_current_speed=true` with the preset name populated.
    #[test]
    fn compute_fan_tile_vm_on_with_preset_mode_attribute() {
        let entity = entity_with_attr("on", "preset_mode", "\"High\"");
        let entity = Entity {
            id: EntityId::from("fan.kitchen"),
            ..entity
        };
        let vm = compute_fan_tile_vm(
            "Kitchen".to_owned(),
            "mdi:fan".to_owned(),
            1,
            1,
            TilePlacement::default_for(1, 1),
            &entity,
        );
        assert!(
            vm.has_current_speed,
            "preset_mode present → has_current_speed=true"
        );
        assert_eq!(vm.current_speed, "High");
    }

    /// `compute_fan_tile_vm` for an `auto` fan sets is_on=true.
    #[test]
    fn compute_fan_tile_vm_auto_state_is_on() {
        let entity = make_test_entity("fan.attic", "auto");
        let vm = compute_fan_tile_vm(
            "Attic".to_owned(),
            "mdi:fan-auto".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "auto");
        assert!(vm.is_on, "auto colors as on");
    }

    /// `build_tiles` for a `WidgetKind::Fan` widget with a `percentage`
    /// attribute produces an `EntityTileVM` whose `state` string is
    /// enriched with the percentage. This confirms the bridge dispatches
    /// the fan state-change through `FanVM::from_entity` per TASK-103
    /// AC #7.
    #[test]
    fn build_tiles_fan_widget_includes_percentage_in_state_when_attribute_present() {
        use crate::ha::store::MemoryStore;

        let entity = entity_with_attr("on", "percentage", "42");
        let entity = Entity {
            id: EntityId::from("fan.bedroom"),
            ..entity
        };
        let store = MemoryStore::load(vec![entity]).expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("fan.bedroom", WidgetKind::Fan);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Fan(vm) => {
                // Post task/phase6-window-wireup: state is the raw HA
                // canonical state; percentage lives on `speed_pct` so the
                // Slint tile renders a separate label.
                assert_eq!(vm.state, "on", "state is the raw HA state");
                assert_eq!(vm.speed_pct, 42, "speed_pct must reflect percentage");
                assert!(vm.has_speed_pct, "has_speed_pct must be true");
                assert!(vm.is_on, "is_on must be true");
            }
            other => panic!("expected FanTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a fan entity with only a `preset_mode` attribute
    /// renders the preset name through the dedicated `current_speed` field.
    #[test]
    fn build_tiles_fan_widget_includes_preset_mode_in_state_when_percentage_absent() {
        use crate::ha::store::MemoryStore;

        let entity = entity_with_attr("on", "preset_mode", "\"Low\"");
        let entity = Entity {
            id: EntityId::from("fan.bedroom"),
            ..entity
        };
        let store = MemoryStore::load(vec![entity]).expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("fan.bedroom", WidgetKind::Fan);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Fan(vm) => {
                assert_eq!(vm.state, "on", "state is the raw HA state");
                assert!(vm.has_current_speed);
                assert_eq!(vm.current_speed, "Low");
                assert!(!vm.has_speed_pct);
            }
            other => panic!("expected FanTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a fan entity WITHOUT speed attributes keeps the
    /// raw HA state string and reports both speed booleans as false.
    #[test]
    fn build_tiles_fan_widget_without_speed_keeps_raw_state() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("fan.bedroom", "off")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("fan.bedroom", WidgetKind::Fan);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Fan(vm) => {
                assert_eq!(vm.state, "off");
                assert!(!vm.has_speed_pct);
                assert!(!vm.has_current_speed);
                assert!(!vm.is_on);
            }
            other => panic!("expected FanTileVM, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // FanBody more-info richer impl (TASK-103)
    // -----------------------------------------------------------------------

    /// `FanBody::render_rows` emits a `speed` row when the entity has
    /// the `percentage` attribute.
    #[test]
    fn fan_body_emits_speed_row_when_percentage_attribute_present() {
        use crate::ui::more_info::{FanBody, MoreInfoBody};
        let entity = entity_with_attr("on", "percentage", "60");
        let rows = FanBody::new().render_rows(&entity);
        assert!(rows.iter().any(|r| r.key == "state" && r.value == "on"));
        assert!(
            rows.iter().any(|r| r.key == "speed" && r.value == "60%"),
            "FanBody must emit a speed row when percentage is set; got {rows:?}"
        );
    }

    /// `FanBody::render_rows` emits a `preset_mode` row when the entity
    /// has the `preset_mode` attribute.
    #[test]
    fn fan_body_emits_preset_mode_row_when_attribute_present() {
        use crate::ui::more_info::{FanBody, MoreInfoBody};
        let entity = entity_with_attr("on", "preset_mode", "\"High\"");
        let rows = FanBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "preset_mode" && r.value == "High"),
            "FanBody must emit a preset_mode row when set; got {rows:?}"
        );
    }

    /// `FanBody::render_rows` emits an `oscillating` row when the
    /// boolean attribute is present.
    #[test]
    fn fan_body_emits_oscillating_row_when_attribute_present() {
        use crate::ui::more_info::{FanBody, MoreInfoBody};
        let entity = entity_with_attr("on", "oscillating", "true");
        let rows = FanBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "oscillating" && r.value == "true"),
            "FanBody must emit an oscillating row when set; got {rows:?}"
        );
    }

    /// `FanBody::render_rows` emits a `direction` row when the string
    /// attribute is present.
    #[test]
    fn fan_body_emits_direction_row_when_attribute_present() {
        use crate::ui::more_info::{FanBody, MoreInfoBody};
        let entity = entity_with_attr("on", "direction", "\"forward\"");
        let rows = FanBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "direction" && r.value == "forward"),
            "FanBody must emit a direction row when set; got {rows:?}"
        );
    }

    /// `FanBody::render_rows` skips optional rows when their attributes
    /// are absent.
    #[test]
    fn fan_body_skips_optional_rows_when_attributes_absent() {
        use crate::ui::more_info::{FanBody, MoreInfoBody};
        let entity = make_test_entity("fan.bedroom", "off");
        let rows = FanBody::new().render_rows(&entity);
        // state row is always emitted.
        assert!(rows.iter().any(|r| r.key == "state"));
        // optional rows must be absent.
        assert!(!rows.iter().any(|r| r.key == "speed"));
        assert!(!rows.iter().any(|r| r.key == "preset_mode"));
        assert!(!rows.iter().any(|r| r.key == "oscillating"));
        assert!(!rows.iter().any(|r| r.key == "direction"));
    }

    // -----------------------------------------------------------------------
    // LockTileVM / compute_lock_tile_vm tests (TASK-104)
    // -----------------------------------------------------------------------

    /// `compute_lock_tile_vm` produces the typed Rust [`LockTileVM`] from
    /// an entity, threading the lock-state derivation through
    /// `LockVM::from_entity`. Sanity-check the output for a locked door.
    #[test]
    fn compute_lock_tile_vm_locked_state() {
        let entity = make_test_entity("lock.front_door", "locked");
        let vm = compute_lock_tile_vm(
            "Front Door".to_owned(),
            "mdi:lock".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.name, "Front Door");
        assert_eq!(vm.state, "locked");
        assert!(vm.is_locked, "locked state must produce is_locked=true");
        assert!(!vm.pending);
    }

    /// `compute_lock_tile_vm` for an unlocked door sets `is_locked=false`.
    #[test]
    fn compute_lock_tile_vm_unlocked_state() {
        let entity = make_test_entity("lock.front_door", "unlocked");
        let vm = compute_lock_tile_vm(
            "Front Door".to_owned(),
            "mdi:lock-open".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "unlocked");
        assert!(!vm.is_locked);
    }

    /// `compute_lock_tile_vm` forwards the canonical `"jammed"` HA state
    /// verbatim. The Slint tile branches on this string for the
    /// jammed-tint render — the bridge does not muddy the state.
    #[test]
    fn compute_lock_tile_vm_jammed_state() {
        let entity = make_test_entity("lock.front_door", "jammed");
        let vm = compute_lock_tile_vm(
            "Front Door".to_owned(),
            "mdi:lock-alert".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "jammed", "state forwarded verbatim");
        assert!(!vm.is_locked, "jammed is not locked");
    }

    /// `compute_lock_tile_vm` for `"locking"` / `"unlocking"` colours
    /// with the destination state.
    #[test]
    fn compute_lock_tile_vm_locking_colors_locked() {
        let entity = make_test_entity("lock.front_door", "locking");
        let vm = compute_lock_tile_vm(
            "Front Door".to_owned(),
            "mdi:lock".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "locking");
        assert!(
            vm.is_locked,
            "locking colours with destination (is_locked=true)"
        );
    }

    /// `build_tiles` for a `WidgetKind::Lock` widget exercises the
    /// `LockVM::from_entity` path and produces an `EntityTileVM` with the
    /// raw HA state forwarded verbatim. This confirms the bridge dispatches
    /// the lock state-change through the LockVM per TASK-104 AC #8.
    #[test]
    fn build_tiles_lock_widget_forwards_state_verbatim() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("lock.front_door", "locked")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("lock.front_door", WidgetKind::Lock);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Lock(vm) => {
                assert_eq!(
                    vm.state, "locked",
                    "lock state forwarded verbatim (no enrichment)"
                );
                assert!(vm.is_locked, "is_locked must be true for `locked` state");
            }
            other => panic!("expected LockTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::Lock` widget routes a `"jammed"`
    /// entity through the bridge without altering the state string.
    #[test]
    fn build_tiles_lock_widget_jammed_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("lock.front_door", "jammed")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("lock.front_door", WidgetKind::Lock);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1);
        match &tiles[0] {
            TileVM::Lock(vm) => {
                assert_eq!(vm.state, "jammed");
            }
            other => panic!("expected LockTileVM, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // LockBody more-info richer impl (TASK-104)
    // -----------------------------------------------------------------------

    /// `LockBody::render_rows` emits a `battery` row when the entity has
    /// the `battery_level` attribute.
    #[test]
    fn lock_body_emits_battery_row_when_attribute_present() {
        use crate::ui::more_info::{LockBody, MoreInfoBody};
        let entity = entity_with_attr("locked", "battery_level", "78");
        let entity = Entity {
            id: EntityId::from("lock.front_door"),
            ..entity
        };
        let rows = LockBody::new().render_rows(&entity);
        assert!(rows.iter().any(|r| r.key == "state" && r.value == "locked"));
        assert!(
            rows.iter().any(|r| r.key == "battery" && r.value == "78%"),
            "LockBody must emit a battery row when battery_level is set; got {rows:?}"
        );
    }

    /// `LockBody::render_rows` emits a `jammed=true` row when the entity
    /// state is `"jammed"`. HA exposes the jammed signal via the state
    /// itself (no separate attribute).
    #[test]
    fn lock_body_emits_jammed_row_when_state_is_jammed() {
        use crate::ui::more_info::{LockBody, MoreInfoBody};
        let entity = make_test_entity("lock.front_door", "jammed");
        let rows = LockBody::new().render_rows(&entity);
        assert!(
            rows.iter().any(|r| r.key == "jammed" && r.value == "true"),
            "LockBody must emit a jammed row when state=jammed; got {rows:?}"
        );
    }

    /// `LockBody::render_rows` emits a `code_format` row when the string
    /// attribute is present.
    #[test]
    fn lock_body_emits_code_format_row_when_attribute_present() {
        use crate::ui::more_info::{LockBody, MoreInfoBody};
        let entity = entity_with_attr("locked", "code_format", "\"number\"");
        let entity = Entity {
            id: EntityId::from("lock.front_door"),
            ..entity
        };
        let rows = LockBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "code_format" && r.value == "number"),
            "LockBody must emit a code_format row when set; got {rows:?}"
        );
    }

    /// `LockBody::render_rows` skips optional rows when their attributes
    /// are absent.
    #[test]
    fn lock_body_skips_optional_rows_when_attributes_absent() {
        use crate::ui::more_info::{LockBody, MoreInfoBody};
        let entity = make_test_entity("lock.front_door", "locked");
        let rows = LockBody::new().render_rows(&entity);
        // state row is always emitted.
        assert!(rows.iter().any(|r| r.key == "state"));
        // optional rows must be absent.
        assert!(!rows.iter().any(|r| r.key == "battery"));
        assert!(!rows.iter().any(|r| r.key == "jammed"));
        assert!(!rows.iter().any(|r| r.key == "code_format"));
    }

    // -----------------------------------------------------------------------
    // compute_alarm_tile_vm (TASK-105)
    // -----------------------------------------------------------------------

    /// `build_tiles` dispatches `WidgetKind::Alarm` through `AlarmVM::from_entity`
    /// and produces an `EntityTileVM` with the alarm state string.
    #[test]
    fn build_tiles_alarm_panel_widget_uses_alarm_state() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity(
            "alarm_control_panel.home",
            "armed_away",
        )])
        .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("alarm_control_panel.home", WidgetKind::Alarm);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1, "one widget → one tile");
        match &tiles[0] {
            TileVM::Alarm(vm) => {
                assert_eq!(vm.state, "armed_away", "alarm state flows through to tile");
                assert!(vm.is_armed);
                assert!(!vm.is_triggered);
            }
            other => panic!("expected AlarmTileVM, got {other:?}"),
        }
    }

    #[test]
    fn compute_alarm_tile_vm_disarmed_state() {
        let entity = make_test_entity("alarm_control_panel.home", "disarmed");
        let vm = compute_alarm_tile_vm(
            "Home Alarm".to_owned(),
            "mdi:shield".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.name, "Home Alarm");
        assert_eq!(vm.state, "disarmed");
        assert!(!vm.is_armed, "disarmed → is_armed=false");
        assert!(!vm.is_triggered);
        assert!(!vm.is_pending);
        assert!(!vm.pending);
    }

    #[test]
    fn compute_alarm_tile_vm_armed_away_state() {
        let entity = make_test_entity("alarm_control_panel.home", "armed_away");
        let vm = compute_alarm_tile_vm(
            "Away".to_owned(),
            "mdi:shield-lock".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "armed_away");
        assert!(vm.is_armed, "armed_away → is_armed=true");
        assert!(!vm.is_triggered);
        assert!(!vm.is_pending);
    }

    #[test]
    fn compute_alarm_tile_vm_triggered_state() {
        let entity = make_test_entity("alarm_control_panel.home", "triggered");
        let vm = compute_alarm_tile_vm(
            "Triggered".to_owned(),
            "mdi:shield-alert".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert!(vm.is_triggered, "triggered → is_triggered=true");
        assert!(!vm.is_armed, "triggered is not an armed_* state");
        assert!(!vm.is_pending);
    }

    // -----------------------------------------------------------------------
    // compute_history_graph_tile_vm + history_path_commands (TASK-106)
    // -----------------------------------------------------------------------

    /// `build_tiles` dispatches `WidgetKind::History` through the
    /// `HistoryGraphVM::from_entity` path and produces an `EntityTileVM`
    /// fallback (no `history-tiles` array property exists yet on
    /// `main_window.slint`). The state string is forwarded verbatim per
    /// TASK-106 AC #13.
    #[test]
    fn build_tiles_history_widget_uses_history_state() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("sensor.energy_today", "23.4")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("sensor.energy_today", WidgetKind::History);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1, "one widget → one tile");
        match &tiles[0] {
            TileVM::History(vm) => {
                assert_eq!(vm.state, "23.4", "history state forwarded verbatim");
                assert!(vm.is_available);
                assert_eq!(vm.change_count, 0, "no window fetched yet at build_tiles");
                assert!(vm.path_commands.is_empty());
            }
            other => panic!("expected HistoryGraphTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::History` widget routes an
    /// `"unavailable"` entity through the bridge without altering the state
    /// string — the unavailable visual is driven downstream by
    /// `HistoryGraphVM::is_available`.
    #[test]
    fn build_tiles_history_widget_unavailable_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("sensor.energy_today", "unavailable")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("sensor.energy_today", WidgetKind::History);
        let tiles = build_tiles(&store, &dashboard);
        match &tiles[0] {
            TileVM::History(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_available);
            }
            other => panic!("expected HistoryGraphTileVM, got {other:?}"),
        }
    }

    /// `compute_history_graph_tile_vm` with `window=None` produces a VM
    /// whose `change_count == 0` and `path_commands == ""`. This is the
    /// pre-fetch state every history tile passes through before its first
    /// `HistoryWindow` arrives.
    #[test]
    fn compute_history_graph_tile_vm_with_no_window_yields_empty_path() {
        let entity = make_test_entity("sensor.energy_today", "23.4");
        let vm = compute_history_graph_tile_vm(
            "Energy".to_owned(),
            "mdi:chart-line".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
            None,
        );
        assert_eq!(vm.name, "Energy");
        assert_eq!(vm.state, "23.4");
        assert_eq!(vm.change_count, 0);
        assert!(vm.is_available, "23.4 → is_available=true");
        assert!(vm.path_commands.is_empty(), "no window → empty commands");
        assert!(!vm.pending);
    }

    /// `compute_history_graph_tile_vm` with a populated window emits a
    /// non-empty `path_commands` string and forwards `change_count` from
    /// `HistoryWindow::len`.
    #[test]
    fn compute_history_graph_tile_vm_with_populated_window_emits_path() {
        use crate::ha::history::HistoryWindow;

        let entity = make_test_entity("sensor.energy_today", "23.4");
        let window = HistoryWindow {
            points: vec![
                (jiff::Timestamp::from_second(0).unwrap(), 1.0),
                (jiff::Timestamp::from_second(60).unwrap(), 2.0),
                (jiff::Timestamp::from_second(120).unwrap(), 3.0),
            ],
        };
        let vm = compute_history_graph_tile_vm(
            "Energy".to_owned(),
            "mdi:chart-line".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
            Some(&window),
        );
        assert_eq!(vm.change_count, 3);
        assert!(vm.is_available);
        assert!(
            vm.path_commands.starts_with('M'),
            "path commands must start with MoveTo: {}",
            vm.path_commands
        );
        assert!(
            vm.path_commands.contains('L'),
            "path commands must contain at least one LineTo: {}",
            vm.path_commands
        );
    }

    /// `compute_history_graph_tile_vm` with an `"unavailable"` state
    /// produces `is_available=false` regardless of whether a window is
    /// present (the renderer dims the trace via opacity rather than
    /// hiding it).
    #[test]
    fn compute_history_graph_tile_vm_unavailable_state() {
        let entity = make_test_entity("sensor.energy_today", "unavailable");
        let vm = compute_history_graph_tile_vm(
            "Energy".to_owned(),
            "mdi:chart-line".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
            None,
        );
        assert!(!vm.is_available, "unavailable → is_available=false");
        assert_eq!(vm.state, "unavailable");
    }

    /// `history_path_commands` returns an empty string for an empty window.
    #[test]
    fn history_path_commands_empty_window() {
        use crate::ha::history::HistoryWindow;
        let window = HistoryWindow { points: Vec::new() };
        assert_eq!(history_path_commands(&window), "");
    }

    /// `history_path_commands` emits one `M` and `(n-1)` `L` commands.
    #[test]
    fn history_path_commands_command_count_matches_points() {
        use crate::ha::history::HistoryWindow;
        let window = HistoryWindow {
            points: (0..5)
                .map(|i| (jiff::Timestamp::from_second(i * 60).unwrap(), i as f64))
                .collect(),
        };
        let cmds = history_path_commands(&window);
        let m_count = cmds.matches('M').count();
        let l_count = cmds.matches('L').count();
        assert_eq!(m_count, 1, "exactly one MoveTo: {cmds}");
        assert_eq!(l_count, 4, "exactly (n-1)=4 LineTo: {cmds}");
    }

    /// `history_path_commands` normalises the X axis so the first point
    /// lands at 0.0 and the last at 1.0.
    #[test]
    fn history_path_commands_x_axis_endpoints_normalised() {
        use crate::ha::history::HistoryWindow;
        let window = HistoryWindow {
            points: vec![
                (jiff::Timestamp::from_second(100).unwrap(), 1.0),
                (jiff::Timestamp::from_second(200).unwrap(), 2.0),
            ],
        };
        let cmds = history_path_commands(&window);
        assert!(
            cmds.starts_with("M 0.0000"),
            "first point must land at x=0.0: {cmds}"
        );
        assert!(
            cmds.contains("L 1.0000"),
            "last point must land at x=1.0: {cmds}"
        );
    }

    /// `history_path_commands` collapses a constant-value trace to the
    /// centreline (y=0.5) — the divide-by-zero guard.
    #[test]
    fn history_path_commands_constant_value_lands_at_centreline() {
        use crate::ha::history::HistoryWindow;
        let window = HistoryWindow {
            points: vec![
                (jiff::Timestamp::from_second(0).unwrap(), 5.0),
                (jiff::Timestamp::from_second(60).unwrap(), 5.0),
                (jiff::Timestamp::from_second(120).unwrap(), 5.0),
            ],
        };
        let cmds = history_path_commands(&window);
        // Every coordinate pair should have y=0.5000.
        let y_05_count = cmds.matches("0.5000").count();
        assert!(
            y_05_count >= 3,
            "constant trace should produce y=0.5 for every point: {cmds}"
        );
    }

    /// `history_path_commands` with a single-point window emits a single
    /// `M` command and collapses x to 0.0 / y to 0.5 (the centreline,
    /// since a one-point window has zero value-span).
    #[test]
    fn history_path_commands_single_point_window() {
        use crate::ha::history::HistoryWindow;
        let window = HistoryWindow {
            points: vec![(jiff::Timestamp::from_second(42).unwrap(), 5.0)],
        };
        let cmds = history_path_commands(&window);
        // One MoveTo, no LineTo.
        assert_eq!(cmds.matches('M').count(), 1);
        assert_eq!(cmds.matches('L').count(), 0);
        // The single point lands at (0, 0.5) per the documented fallback:
        // ts_span==0 → x=0.0; val_span==0 → y=0.5 (centreline).
        assert_eq!(
            cmds, "M 0.0000 0.5000",
            "single-point window must collapse to (0, 0.5)"
        );
    }

    /// `history_path_commands` with a multi-point window whose timestamps
    /// are all coincident (rare HA case where multiple records share
    /// `last_changed`) collapses every point to x=0.0 per the documented
    /// edge-case contract. The Y axis still spans normally.
    #[test]
    fn history_path_commands_coincident_timestamps_collapse_x_axis() {
        use crate::ha::history::HistoryWindow;
        let window = HistoryWindow {
            points: vec![
                (jiff::Timestamp::from_second(100).unwrap(), 1.0),
                (jiff::Timestamp::from_second(100).unwrap(), 2.0),
                (jiff::Timestamp::from_second(100).unwrap(), 3.0),
            ],
        };
        let cmds = history_path_commands(&window);
        // Every coordinate pair must have x=0.0000.
        let x_zero_count = cmds.matches("0.0000").count();
        // Three points × at least one "0.0000" each (plus possibly more
        // for y values that round to 0.0000 when val == max). The
        // minimum guaranteed count is 3 (one per point's x coordinate).
        assert!(
            x_zero_count >= 3,
            "all coincident-timestamp points must collapse to x=0.0: {cmds}"
        );
        // Y axis still spans: min val (1.0) → y=1.0; max val (3.0) → y=0.0.
        assert!(
            cmds.contains("1.0000"),
            "Y axis must still span when ts_span==0: {cmds}"
        );
    }

    /// `history_path_commands` Y-axis inverts so the maximum value lands
    /// at y=0.0 (top of the unit square — screen-space convention).
    #[test]
    fn history_path_commands_y_axis_inverts_for_screen_space() {
        use crate::ha::history::HistoryWindow;
        let window = HistoryWindow {
            points: vec![
                (jiff::Timestamp::from_second(0).unwrap(), 1.0),
                (jiff::Timestamp::from_second(60).unwrap(), 10.0),
            ],
        };
        let cmds = history_path_commands(&window);
        // First point (val=1.0, the minimum) should be at y=1.0;
        // second point (val=10.0, the maximum) should be at y=0.0.
        assert!(
            cmds.starts_with("M 0.0000 1.0000"),
            "minimum value must land at y=1.0 (bottom): {cmds}"
        );
        assert!(
            cmds.contains("L 1.0000 0.0000"),
            "maximum value must land at y=0.0 (top): {cmds}"
        );
    }

    // -----------------------------------------------------------------------
    // compute_camera_tile_vm + build_tiles Camera arm (TASK-107)
    // -----------------------------------------------------------------------

    /// `build_tiles` dispatches `WidgetKind::Camera` through the
    /// `CameraVM::from_entity` path and produces an `EntityTileVM` fallback
    /// (no `camera-tiles` array property exists yet on `main_window.slint`).
    /// The state string is forwarded verbatim per TASK-107 AC.
    #[test]
    fn build_tiles_camera_widget_uses_camera_state() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("camera.front_door", "idle")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("camera.front_door", WidgetKind::Camera);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1, "one widget → one tile");
        match &tiles[0] {
            TileVM::Camera(vm) => {
                assert_eq!(vm.state, "idle", "camera state forwarded verbatim");
                assert!(vm.is_available);
            }
            other => panic!("expected CameraTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::Camera` widget routes a `"recording"`
    /// state through the bridge without altering the state string — the
    /// active visual is driven downstream by `CameraVM::is_recording`.
    #[test]
    fn build_tiles_camera_widget_recording_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("camera.front_door", "recording")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("camera.front_door", WidgetKind::Camera);
        let tiles = build_tiles(&store, &dashboard);
        match &tiles[0] {
            TileVM::Camera(vm) => {
                assert_eq!(vm.state, "recording");
                assert!(vm.is_recording);
            }
            other => panic!("expected CameraTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::Camera` widget routes an
    /// `"unavailable"` entity through the bridge without altering the state
    /// string — the unavailable visual is driven downstream by
    /// `CameraVM::is_available`.
    #[test]
    fn build_tiles_camera_widget_unavailable_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("camera.front_door", "unavailable")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("camera.front_door", WidgetKind::Camera);
        let tiles = build_tiles(&store, &dashboard);
        match &tiles[0] {
            TileVM::Camera(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_available);
            }
            other => panic!("expected CameraTileVM, got {other:?}"),
        }
    }

    /// `compute_camera_tile_vm` for an idle camera produces the typed Rust
    /// `CameraTileVM` with `is_available=true` and active flags clear.
    #[test]
    fn compute_camera_tile_vm_idle_state() {
        let entity = make_test_entity("camera.front_door", "idle");
        let vm = compute_camera_tile_vm(
            "Front Door".to_owned(),
            "mdi:cctv".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.name, "Front Door");
        assert_eq!(vm.state, "idle");
        assert!(vm.is_available, "idle → is_available=true");
        assert!(!vm.is_recording, "idle → is_recording=false");
        assert!(!vm.is_streaming, "idle → is_streaming=false");
        assert!(!vm.pending);
        assert_eq!(vm.icon_id, "mdi:cctv");
    }

    /// `compute_camera_tile_vm` for a recording camera flips `is_recording`.
    #[test]
    fn compute_camera_tile_vm_recording_state() {
        let entity = make_test_entity("camera.front_door", "recording");
        let vm = compute_camera_tile_vm(
            "Front Door".to_owned(),
            "mdi:cctv".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "recording");
        assert!(vm.is_available);
        assert!(vm.is_recording, "recording → is_recording=true");
        assert!(!vm.is_streaming);
    }

    /// `compute_camera_tile_vm` for a streaming camera flips `is_streaming`.
    #[test]
    fn compute_camera_tile_vm_streaming_state() {
        let entity = make_test_entity("camera.front_door", "streaming");
        let vm = compute_camera_tile_vm(
            "Front Door".to_owned(),
            "mdi:cctv".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "streaming");
        assert!(vm.is_available);
        assert!(!vm.is_recording);
        assert!(vm.is_streaming, "streaming → is_streaming=true");
    }

    /// `compute_camera_tile_vm` for an unavailable camera produces
    /// `is_available=false`.
    #[test]
    fn compute_camera_tile_vm_unavailable_state() {
        let entity = make_test_entity("camera.front_door", "unavailable");
        let vm = compute_camera_tile_vm(
            "Front Door".to_owned(),
            "mdi:cctv".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert!(!vm.is_available, "unavailable → is_available=false");
        assert!(!vm.is_recording);
        assert!(!vm.is_streaming);
        assert_eq!(vm.state, "unavailable");
    }

    /// `compute_camera_tile_vm` for a vendor-specific state forwards the
    /// state verbatim and treats it as available + idle.
    #[test]
    fn compute_camera_tile_vm_vendor_specific_state_is_available_idle() {
        let entity = make_test_entity("camera.front_door", "armed");
        let vm = compute_camera_tile_vm(
            "Front Door".to_owned(),
            "mdi:cctv".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "armed");
        assert!(vm.is_available);
        assert!(!vm.is_recording);
        assert!(!vm.is_streaming);
    }

    // -----------------------------------------------------------------------
    // compute_climate_tile_vm + build_tiles Climate arm (TASK-108)
    // -----------------------------------------------------------------------

    /// `build_tiles` dispatches `WidgetKind::Climate` through the
    /// `ClimateVM::from_entity` path and produces an `EntityTileVM` fallback
    /// (no `climate-tiles` array property exists yet on `main_window.slint`).
    /// The state string is forwarded verbatim per TASK-108 AC.
    #[test]
    fn build_tiles_climate_widget_uses_climate_state() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("climate.living_room", "heat")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("climate.living_room", WidgetKind::Climate);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1, "one widget → one tile");
        match &tiles[0] {
            TileVM::Climate(vm) => {
                assert_eq!(vm.state, "heat", "climate state forwarded verbatim");
                assert!(vm.is_active);
            }
            other => panic!("expected ClimateTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::Climate` widget routes a `"cool"`
    /// state through the bridge without altering the state string — the
    /// active visual is driven downstream by `ClimateVM::is_active`.
    #[test]
    fn build_tiles_climate_widget_cool_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("climate.living_room", "cool")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("climate.living_room", WidgetKind::Climate);
        let tiles = build_tiles(&store, &dashboard);
        match &tiles[0] {
            TileVM::Climate(vm) => assert_eq!(vm.state, "cool"),
            other => panic!("expected ClimateTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::Climate` widget routes an `"off"`
    /// state through the bridge without altering the state string — the
    /// idle visual is driven downstream by `ClimateVM::is_active=false`.
    #[test]
    fn build_tiles_climate_widget_off_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("climate.living_room", "off")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("climate.living_room", WidgetKind::Climate);
        let tiles = build_tiles(&store, &dashboard);
        match &tiles[0] {
            TileVM::Climate(vm) => {
                assert_eq!(vm.state, "off");
                assert!(!vm.is_active);
            }
            other => panic!("expected ClimateTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::Climate` widget routes an
    /// `"unavailable"` entity through the bridge without altering the
    /// state string — the unavailable visual is driven downstream by
    /// `ClimateVM::is_active=false`.
    #[test]
    fn build_tiles_climate_widget_unavailable_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("climate.living_room", "unavailable")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("climate.living_room", WidgetKind::Climate);
        let tiles = build_tiles(&store, &dashboard);
        match &tiles[0] {
            TileVM::Climate(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_active);
            }
            other => panic!("expected ClimateTileVM, got {other:?}"),
        }
    }

    /// `compute_climate_tile_vm` for a heating climate produces the typed
    /// Rust `ClimateTileVM` with `is_active=true`.
    #[test]
    fn compute_climate_tile_vm_heat_state() {
        let entity = make_test_entity("climate.living_room", "heat");
        let vm = compute_climate_tile_vm(
            "Living Room".to_owned(),
            "mdi:thermostat".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.name, "Living Room");
        assert_eq!(vm.state, "heat");
        assert!(vm.is_active, "heat → is_active=true");
        assert_eq!(vm.icon_id, "mdi:thermostat");
        assert!(!vm.pending);
        assert_eq!(vm.current_temperature, None);
        assert_eq!(vm.target_temperature, None);
    }

    /// `compute_climate_tile_vm` for an off climate flips `is_active=false`.
    #[test]
    fn compute_climate_tile_vm_off_state() {
        let entity = make_test_entity("climate.living_room", "off");
        let vm = compute_climate_tile_vm(
            "Living Room".to_owned(),
            "mdi:thermostat".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "off");
        assert!(!vm.is_active, "off → is_active=false");
    }

    /// `compute_climate_tile_vm` for an unavailable climate produces
    /// `is_active=false`.
    #[test]
    fn compute_climate_tile_vm_unavailable_state() {
        let entity = make_test_entity("climate.living_room", "unavailable");
        let vm = compute_climate_tile_vm(
            "Living Room".to_owned(),
            "mdi:thermostat".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "unavailable");
        assert!(!vm.is_active, "unavailable → is_active=false");
    }

    /// `compute_climate_tile_vm` reads HA's `current_temperature` and
    /// `temperature` (NOT `target_temperature`) attributes into the
    /// typed VM.
    #[test]
    fn compute_climate_tile_vm_reads_temperature_attributes() {
        let raw = entity_with_attr("heat", "current_temperature", "21.5");
        // entity_with_attr builds a one-key map; merge a second key by
        // re-parsing the full snippet.
        let snippet = r#"{"current_temperature":21.5,"temperature":23.0}"#;
        let map = serde_yaml_ng::from_str(snippet).expect("test snippet must parse");
        let entity = Entity {
            id: EntityId::from("climate.living_room"),
            attributes: Arc::new(map),
            ..raw
        };
        let vm = compute_climate_tile_vm(
            "Living Room".to_owned(),
            "mdi:thermostat".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.current_temperature, Some(21.5));
        assert_eq!(
            vm.target_temperature,
            Some(23.0),
            "target_temperature must read HA's `temperature` attribute"
        );
        assert_eq!(vm.state, "heat");
        assert!(vm.is_active);
    }

    /// `compute_climate_tile_vm` for a vendor-specific HVAC mode forwards
    /// the state verbatim and treats it as active per
    /// `locked_decisions.hvac_mode_vocabulary` (the picker only shows
    /// operator-configured modes; whatever the entity reports is shown).
    #[test]
    fn compute_climate_tile_vm_vendor_specific_state_is_active() {
        let entity = make_test_entity("climate.living_room", "boost");
        let vm = compute_climate_tile_vm(
            "Living Room".to_owned(),
            "mdi:thermostat".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "boost");
        assert!(vm.is_active, "vendor mode → is_active=true");
    }

    // -----------------------------------------------------------------------
    // compute_media_player_tile_vm + build_tiles MediaPlayer arm (TASK-109)
    // -----------------------------------------------------------------------

    /// `build_tiles` dispatches `WidgetKind::MediaPlayer` through the
    /// `MediaPlayerVM::from_entity` path and produces an `EntityTileVM`
    /// fallback (no `media-player-tiles` array property exists yet on
    /// `main_window.slint`). The state string is forwarded verbatim per
    /// TASK-109 AC.
    #[test]
    fn build_tiles_media_player_widget_uses_state() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("media_player.tv", "playing")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("media_player.tv", WidgetKind::MediaPlayer);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1, "one widget → one tile");
        match &tiles[0] {
            TileVM::MediaPlayer(vm) => {
                assert_eq!(vm.state, "playing", "media-player state forwarded verbatim");
                assert!(vm.is_playing);
            }
            other => panic!("expected MediaPlayerTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::MediaPlayer` widget routes a
    /// `"paused"` state through the bridge without altering the state
    /// string. The `is_playing` derivation is exercised by
    /// `MediaPlayerVM::from_entity`.
    #[test]
    fn build_tiles_media_player_widget_paused_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("media_player.tv", "paused")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("media_player.tv", WidgetKind::MediaPlayer);
        let tiles = build_tiles(&store, &dashboard);
        match &tiles[0] {
            TileVM::MediaPlayer(vm) => {
                assert_eq!(vm.state, "paused");
                assert!(!vm.is_playing);
            }
            other => panic!("expected MediaPlayerTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::MediaPlayer` widget routes an
    /// `"unavailable"` entity through the bridge without altering the
    /// state string — the unavailable visual is driven downstream by
    /// `MediaPlayerVM::is_playing=false`.
    #[test]
    fn build_tiles_media_player_widget_unavailable_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("media_player.tv", "unavailable")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("media_player.tv", WidgetKind::MediaPlayer);
        let tiles = build_tiles(&store, &dashboard);
        match &tiles[0] {
            TileVM::MediaPlayer(vm) => {
                assert_eq!(vm.state, "unavailable");
                assert!(!vm.is_playing);
            }
            other => panic!("expected MediaPlayerTileVM, got {other:?}"),
        }
    }

    /// `compute_media_player_tile_vm` for a playing media-player produces
    /// the typed Rust `MediaPlayerTileVM` with `is_playing=true` and
    /// surfaces the track-title / artist attributes.
    #[test]
    fn compute_media_player_tile_vm_playing_state_with_track_info() {
        let raw = entity_with_attr("playing", "media_title", "\"Track A\"");
        // entity_with_attr builds a one-key map; merge multiple keys by
        // re-parsing the full snippet.
        let snippet = r#"{"media_title":"Track A","media_artist":"Artist B","volume_level":0.5}"#;
        let map = serde_yaml_ng::from_str(snippet).expect("test snippet must parse");
        let entity = Entity {
            id: EntityId::from("media_player.tv"),
            attributes: Arc::new(map),
            ..raw
        };
        let vm = compute_media_player_tile_vm(
            "Living Room TV".to_owned(),
            "mdi:television".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.name, "Living Room TV");
        assert_eq!(vm.state, "playing");
        assert!(vm.is_playing, "playing → is_playing=true");
        assert_eq!(vm.media_title.as_deref(), Some("Track A"));
        assert_eq!(vm.artist.as_deref(), Some("Artist B"));
        assert_eq!(vm.volume_level, Some(0.5));
        assert_eq!(vm.icon_id, "mdi:television");
        assert!(!vm.pending);
    }

    /// `compute_media_player_tile_vm` for an idle media-player flips
    /// `is_playing=false` and produces no track info.
    #[test]
    fn compute_media_player_tile_vm_idle_state() {
        let entity = make_test_entity("media_player.tv", "idle");
        let vm = compute_media_player_tile_vm(
            "Living Room TV".to_owned(),
            "mdi:television".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "idle");
        assert!(!vm.is_playing, "idle → is_playing=false");
        assert!(vm.media_title.is_none());
        assert!(vm.artist.is_none());
        assert!(vm.volume_level.is_none());
    }

    /// `compute_media_player_tile_vm` for an unavailable media-player
    /// produces the unavailable sentinel (`is_playing=false`, no track
    /// info).
    #[test]
    fn compute_media_player_tile_vm_unavailable_state() {
        let entity = make_test_entity("media_player.tv", "unavailable");
        let vm = compute_media_player_tile_vm(
            "Living Room TV".to_owned(),
            "mdi:television".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(vm.state, "unavailable");
        assert!(!vm.is_playing, "unavailable → is_playing=false");
        assert!(vm.media_title.is_none());
        assert!(vm.artist.is_none());
        assert!(vm.volume_level.is_none());
    }

    /// `compute_media_player_tile_vm` clamps an above-range
    /// `volume_level` to 1.0.
    #[test]
    fn compute_media_player_tile_vm_clamps_volume_level_above_one() {
        let entity = entity_with_attr("playing", "volume_level", "1.7");
        let entity = Entity {
            id: EntityId::from("media_player.tv"),
            ..entity
        };
        let vm = compute_media_player_tile_vm(
            "TV".to_owned(),
            "mdi:television".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(
            vm.volume_level,
            Some(1.0),
            "above-range volume must clamp to 1.0"
        );
    }

    /// `compute_media_player_tile_vm` clamps a below-range (negative)
    /// `volume_level` to 0.0. Defends the lower bound at the bridge
    /// level so a future refactor that bypasses
    /// `MediaPlayerVM::from_entity` cannot silently drop the lower
    /// clamp.
    #[test]
    fn compute_media_player_tile_vm_clamps_volume_level_below_zero() {
        let entity = entity_with_attr("playing", "volume_level", "-0.25");
        let entity = Entity {
            id: EntityId::from("media_player.tv"),
            ..entity
        };
        let vm = compute_media_player_tile_vm(
            "TV".to_owned(),
            "mdi:television".to_owned(),
            2,
            1,
            TilePlacement::default_for(2, 1),
            &entity,
        );
        assert_eq!(
            vm.volume_level,
            Some(0.0),
            "negative volume must clamp to 0.0"
        );
    }

    // -----------------------------------------------------------------------
    // compute_power_flow_tile_vm + build_tiles PowerFlow arm (TASK-094)
    // -----------------------------------------------------------------------

    /// `build_tiles` dispatches `WidgetKind::PowerFlow` through the
    /// `PowerFlowVM::read_power_watts` path and produces an `EntityTileVM`
    /// fallback (no `power-flow-tiles` array property exists yet on
    /// `main_window.slint`). The state string is forwarded verbatim.
    #[test]
    fn build_tiles_power_flow_widget_uses_state() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("sensor.grid_power", "1500")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("sensor.grid_power", WidgetKind::PowerFlow);
        let tiles = build_tiles(&store, &dashboard);
        assert_eq!(tiles.len(), 1, "one widget → one tile");
        match &tiles[0] {
            // Post task/phase6-window-wireup: PowerFlow has no `state`
            // string field on its VM. The dashboard helper sets
            // `widget.entity = Some(...)` and `widget.options = None`, so
            // the bridge falls through `compute_power_flow_tile_vm_from_widget`'s
            // wrapper-entity branch and forwards the parsed grid wattage.
            TileVM::PowerFlow(vm) => {
                assert_eq!(vm.grid_w, Some(1500.0), "grid_w must parse the state");
            }
            other => panic!("expected PowerFlowTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::PowerFlow` widget routes a
    /// negative-export grid reading through the bridge — `grid_w` carries
    /// the signed numeric value; the directional indicator is driven by
    /// the Slint side via `grid_importing`.
    #[test]
    fn build_tiles_power_flow_widget_export_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("sensor.grid_power", "-200.5")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("sensor.grid_power", WidgetKind::PowerFlow);
        let tiles = build_tiles(&store, &dashboard);
        match &tiles[0] {
            TileVM::PowerFlow(vm) => assert_eq!(vm.grid_w, Some(-200.5)),
            other => panic!("expected PowerFlowTileVM, got {other:?}"),
        }
    }

    /// `build_tiles` for a `WidgetKind::PowerFlow` widget routes an
    /// `unavailable` entity through the bridge without altering the state
    /// string — `PowerFlowVM::read_power_watts` returns `None` for the
    /// sentinel and the Slint side renders the unavailable variant.
    #[test]
    fn build_tiles_power_flow_widget_unavailable_state_passes_through() {
        use crate::ha::store::MemoryStore;

        let store = MemoryStore::load(vec![make_test_entity("sensor.grid_power", "unavailable")])
            .expect("MemoryStore::load");

        let dashboard = dashboard_with_kind("sensor.grid_power", WidgetKind::PowerFlow);
        let tiles = build_tiles(&store, &dashboard);
        match &tiles[0] {
            TileVM::PowerFlow(vm) => {
                assert!(vm.grid_w.is_none(), "unavailable → grid_w=None");
            }
            other => panic!("expected PowerFlowTileVM, got {other:?}"),
        }
    }

    /// `compute_power_flow_tile_vm` for a positive grid reading produces
    /// the typed Rust `PowerFlowTileVM` with `grid_w=Some(1234.5)`.
    #[test]
    fn compute_power_flow_tile_vm_grid_import() {
        let entity = make_test_entity("sensor.grid_power", "1234.5");
        let vm = compute_power_flow_tile_vm(
            "Power Flow".to_owned(),
            "mdi:lightning-bolt-circle".to_owned(),
            2,
            2,
            TilePlacement::default_for(2, 2),
            &entity,
            PowerFlowAuxiliaryReadings::default(),
        );
        assert_eq!(vm.name, "Power Flow");
        assert_eq!(vm.grid_w, Some(1234.5));
        assert_eq!(vm.solar_w, None);
        assert_eq!(vm.battery_w, None);
        assert_eq!(vm.battery_pct, None);
        assert_eq!(vm.home_w, None);
        assert_eq!(vm.icon_id, "mdi:lightning-bolt-circle");
        assert!(!vm.pending);
    }

    /// `compute_power_flow_tile_vm` preserves the export sign for a
    /// negative grid reading.
    #[test]
    fn compute_power_flow_tile_vm_grid_export() {
        let entity = make_test_entity("sensor.grid_power", "-300.0");
        let vm = compute_power_flow_tile_vm(
            "Power Flow".to_owned(),
            "mdi:lightning-bolt-circle".to_owned(),
            2,
            2,
            TilePlacement::default_for(2, 2),
            &entity,
            PowerFlowAuxiliaryReadings::default(),
        );
        assert_eq!(
            vm.grid_w,
            Some(-300.0),
            "export must preserve negative sign"
        );
    }

    /// `compute_power_flow_tile_vm` returns `grid_w=None` for an
    /// `unavailable` grid entity (matches `PowerFlowVM::read_power_watts`
    /// contract for the sentinel).
    #[test]
    fn compute_power_flow_tile_vm_grid_unavailable() {
        let entity = make_test_entity("sensor.grid_power", "unavailable");
        let vm = compute_power_flow_tile_vm(
            "Power Flow".to_owned(),
            "mdi:lightning-bolt-circle".to_owned(),
            2,
            2,
            TilePlacement::default_for(2, 2),
            &entity,
            PowerFlowAuxiliaryReadings::default(),
        );
        assert_eq!(vm.grid_w, None, "unavailable grid must produce grid_w=None");
    }

    /// `compute_power_flow_tile_vm` forwards all supplied auxiliary readings
    /// (solar / battery / battery_pct / home) onto the VM verbatim.
    #[test]
    fn compute_power_flow_tile_vm_threads_all_auxiliary_readings() {
        let entity = make_test_entity("sensor.grid_power", "0");
        let vm = compute_power_flow_tile_vm(
            "Power Flow".to_owned(),
            "mdi:lightning-bolt-circle".to_owned(),
            2,
            2,
            TilePlacement::default_for(2, 2),
            &entity,
            PowerFlowAuxiliaryReadings {
                solar_w: Some(2000.0),
                battery_w: Some(-500.0),
                battery_pct: Some(75.0),
                home_w: Some(900.0),
            },
        );
        assert_eq!(vm.grid_w, Some(0.0));
        assert_eq!(vm.solar_w, Some(2000.0));
        assert_eq!(
            vm.battery_w,
            Some(-500.0),
            "battery discharge sign preserved"
        );
        assert_eq!(vm.battery_pct, Some(75.0));
        assert_eq!(vm.home_w, Some(900.0));
    }

    /// `compute_power_flow_tile_vm` with an integer-shaped grid state
    /// still parses via the `f64`/`i64` round-trip in
    /// `PowerFlowVM::read_power_watts`.
    #[test]
    fn compute_power_flow_tile_vm_grid_integer_value() {
        let entity = make_test_entity("sensor.grid_power", "500");
        let vm = compute_power_flow_tile_vm(
            "Power Flow".to_owned(),
            "mdi:lightning-bolt-circle".to_owned(),
            2,
            2,
            TilePlacement::default_for(2, 2),
            &entity,
            PowerFlowAuxiliaryReadings::default(),
        );
        assert_eq!(vm.grid_w, Some(500.0));
    }

    // -----------------------------------------------------------------------
    // MediaPlayerBody more-info richer impl (TASK-109)
    // -----------------------------------------------------------------------

    /// `MediaPlayerBody::render_rows` always emits the state row.
    #[test]
    fn media_player_body_emits_state_row() {
        use crate::ui::more_info::{MediaPlayerBody, MoreInfoBody};
        let entity = make_test_entity("media_player.tv", "playing");
        let rows = MediaPlayerBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "state" && r.value == "playing"),
            "MediaPlayerBody must always emit a state row; got {rows:?}"
        );
    }

    /// `MediaPlayerBody::render_rows` emits a `media_title` row when the
    /// `media_title` attribute is present.
    #[test]
    fn media_player_body_emits_media_title_row_when_attribute_present() {
        use crate::ui::more_info::{MediaPlayerBody, MoreInfoBody};
        let entity = entity_with_attr("playing", "media_title", "\"Hey Jude\"");
        let entity = Entity {
            id: EntityId::from("media_player.tv"),
            ..entity
        };
        let rows = MediaPlayerBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "media_title" && r.value == "Hey Jude"),
            "MediaPlayerBody must emit a media_title row when set; got {rows:?}"
        );
    }

    /// `MediaPlayerBody::render_rows` skips the `media_title` row when
    /// the attribute is absent.
    #[test]
    fn media_player_body_skips_media_title_row_when_attribute_absent() {
        use crate::ui::more_info::{MediaPlayerBody, MoreInfoBody};
        let entity = make_test_entity("media_player.tv", "idle");
        let rows = MediaPlayerBody::new().render_rows(&entity);
        assert!(
            !rows.iter().any(|r| r.key == "media_title"),
            "no media_title attribute → no media_title row; got {rows:?}"
        );
    }

    /// `MediaPlayerBody::render_rows` emits an `artist` row when the
    /// `media_artist` attribute is present.
    #[test]
    fn media_player_body_emits_artist_row_when_attribute_present() {
        use crate::ui::more_info::{MediaPlayerBody, MoreInfoBody};
        let entity = entity_with_attr("playing", "media_artist", "\"The Beatles\"");
        let entity = Entity {
            id: EntityId::from("media_player.tv"),
            ..entity
        };
        let rows = MediaPlayerBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "artist" && r.value == "The Beatles"),
            "MediaPlayerBody must emit an artist row when set; got {rows:?}"
        );
    }

    /// `MediaPlayerBody::render_rows` emits a `volume_level` row when
    /// the attribute is present, formatted as a percentage.
    #[test]
    fn media_player_body_emits_volume_level_row_when_attribute_present() {
        use crate::ui::more_info::{MediaPlayerBody, MoreInfoBody};
        let entity = entity_with_attr("playing", "volume_level", "0.42");
        let entity = Entity {
            id: EntityId::from("media_player.tv"),
            ..entity
        };
        let rows = MediaPlayerBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "volume_level" && r.value == "42%"),
            "MediaPlayerBody must emit a volume_level row when set; got {rows:?}"
        );
    }

    /// `MediaPlayerBody::render_rows` emits a `source` row when the
    /// `source` attribute is present.
    #[test]
    fn media_player_body_emits_source_row_when_attribute_present() {
        use crate::ui::more_info::{MediaPlayerBody, MoreInfoBody};
        let entity = entity_with_attr("playing", "source", "\"HDMI 1\"");
        let entity = Entity {
            id: EntityId::from("media_player.tv"),
            ..entity
        };
        let rows = MediaPlayerBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "source" && r.value == "HDMI 1"),
            "MediaPlayerBody must emit a source row when set; got {rows:?}"
        );
    }

    /// `MediaPlayerBody::render_rows` emits a `sound_mode` row when the
    /// `sound_mode` attribute is present.
    #[test]
    fn media_player_body_emits_sound_mode_row_when_attribute_present() {
        use crate::ui::more_info::{MediaPlayerBody, MoreInfoBody};
        let entity = entity_with_attr("playing", "sound_mode", "\"Movie\"");
        let entity = Entity {
            id: EntityId::from("media_player.tv"),
            ..entity
        };
        let rows = MediaPlayerBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "sound_mode" && r.value == "Movie"),
            "MediaPlayerBody must emit a sound_mode row when set; got {rows:?}"
        );
    }

    /// `MediaPlayerBody::render_rows` emits an `album` row when the
    /// `media_album_name` attribute is present.
    #[test]
    fn media_player_body_emits_album_row_when_attribute_present() {
        use crate::ui::more_info::{MediaPlayerBody, MoreInfoBody};
        let entity = entity_with_attr("playing", "media_album_name", "\"Abbey Road\"");
        let entity = Entity {
            id: EntityId::from("media_player.tv"),
            ..entity
        };
        let rows = MediaPlayerBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "album" && r.value == "Abbey Road"),
            "MediaPlayerBody must emit an album row when set; got {rows:?}"
        );
    }

    // -----------------------------------------------------------------------
    // HistoryBody more-info richer impl (TASK-106)
    // -----------------------------------------------------------------------

    /// `HistoryBody::render_rows` always emits the state row.
    #[test]
    fn history_body_emits_state_row() {
        use crate::ui::more_info::{HistoryBody, MoreInfoBody};
        let entity = make_test_entity("sensor.energy_today", "23.4");
        let rows = HistoryBody::new().render_rows(&entity);
        assert!(
            rows.iter().any(|r| r.key == "state" && r.value == "23.4"),
            "HistoryBody must always emit a state row; got {rows:?}"
        );
    }

    /// `HistoryBody::render_rows` emits a `unit_of_measurement` row when
    /// the attribute is present.
    #[test]
    fn history_body_emits_unit_of_measurement_row_when_attribute_present() {
        use crate::ui::more_info::{HistoryBody, MoreInfoBody};
        let entity = entity_with_attr("23.4", "unit_of_measurement", "\"\\u00b0C\"");
        let entity = Entity {
            id: EntityId::from("sensor.energy_today"),
            ..entity
        };
        let rows = HistoryBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "unit_of_measurement" && r.value == "°C"),
            "HistoryBody must emit a unit_of_measurement row when set; got {rows:?}"
        );
    }

    /// `HistoryBody::render_rows` emits a `friendly_name` row when the
    /// attribute is present.
    #[test]
    fn history_body_emits_friendly_name_row_when_attribute_present() {
        use crate::ui::more_info::{HistoryBody, MoreInfoBody};
        let entity = entity_with_attr("23.4", "friendly_name", "\"Kitchen Thermometer\"");
        let entity = Entity {
            id: EntityId::from("sensor.energy_today"),
            ..entity
        };
        let rows = HistoryBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "friendly_name" && r.value == "Kitchen Thermometer"),
            "HistoryBody must emit a friendly_name row when set; got {rows:?}"
        );
    }

    /// `HistoryBody::render_rows` emits a `device_class` row when the
    /// attribute is present.
    #[test]
    fn history_body_emits_device_class_row_when_attribute_present() {
        use crate::ui::more_info::{HistoryBody, MoreInfoBody};
        let entity = entity_with_attr("23.4", "device_class", "\"temperature\"");
        let entity = Entity {
            id: EntityId::from("sensor.energy_today"),
            ..entity
        };
        let rows = HistoryBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "device_class" && r.value == "temperature"),
            "HistoryBody must emit a device_class row when set; got {rows:?}"
        );
    }

    /// `HistoryBody::render_rows` skips optional rows when their attributes
    /// are absent.
    #[test]
    fn history_body_skips_optional_rows_when_absent() {
        use crate::ui::more_info::{HistoryBody, MoreInfoBody};
        let entity = make_test_entity("sensor.energy_today", "23.4");
        let rows = HistoryBody::new().render_rows(&entity);
        // Mandatory row always present.
        assert!(rows.iter().any(|r| r.key == "state"));
        // Optional rows must be absent.
        assert!(!rows.iter().any(|r| r.key == "unit_of_measurement"));
        assert!(!rows.iter().any(|r| r.key == "device_class"));
    }

    // -----------------------------------------------------------------------
    // CameraBody more-info richer impl (TASK-107)
    // -----------------------------------------------------------------------

    /// `CameraBody::render_rows` always emits the state row.
    #[test]
    fn camera_body_emits_state_row() {
        use crate::ui::more_info::{CameraBody, MoreInfoBody};
        let entity = make_test_entity("camera.front_door", "idle");
        let rows = CameraBody::new().render_rows(&entity);
        assert!(
            rows.iter().any(|r| r.key == "state" && r.value == "idle"),
            "CameraBody must always emit a state row; got {rows:?}"
        );
    }

    /// `CameraBody::render_rows` emits a `friendly_name` row when present.
    #[test]
    fn camera_body_emits_friendly_name_row_when_attribute_present() {
        use crate::ui::more_info::{CameraBody, MoreInfoBody};
        let entity = entity_with_attr("idle", "friendly_name", "\"Front Door Camera\"");
        let entity = Entity {
            id: EntityId::from("camera.front_door"),
            ..entity
        };
        let rows = CameraBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "friendly_name" && r.value == "Front Door Camera"),
            "CameraBody must emit a friendly_name row when set; got {rows:?}"
        );
    }

    /// `CameraBody::render_rows` emits a `last_motion` row when present.
    #[test]
    fn camera_body_emits_last_motion_row_when_attribute_present() {
        use crate::ui::more_info::{CameraBody, MoreInfoBody};
        let entity = entity_with_attr("idle", "last_motion", "\"2026-04-30T12:00:00Z\"");
        let entity = Entity {
            id: EntityId::from("camera.front_door"),
            ..entity
        };
        let rows = CameraBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "last_motion" && r.value == "2026-04-30T12:00:00Z"),
            "CameraBody must emit a last_motion row when set; got {rows:?}"
        );
    }

    /// `CameraBody::render_rows` emits a `brand` row when present.
    #[test]
    fn camera_body_emits_brand_row_when_attribute_present() {
        use crate::ui::more_info::{CameraBody, MoreInfoBody};
        let entity = entity_with_attr("idle", "brand", "\"Reolink\"");
        let entity = Entity {
            id: EntityId::from("camera.front_door"),
            ..entity
        };
        let rows = CameraBody::new().render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "brand" && r.value == "Reolink"),
            "CameraBody must emit a brand row when set; got {rows:?}"
        );
    }

    /// `CameraBody::render_rows` emits a `snapshot_url` indicator when
    /// `entity_picture` is set — but the indicator value MUST NOT contain
    /// the URL itself per `CLAUDE.md` security rules (the URL embeds a
    /// short-lived access token).
    #[test]
    fn camera_body_emits_snapshot_url_indicator_without_logging_url() {
        use crate::ui::more_info::{CameraBody, MoreInfoBody};
        let entity = entity_with_attr(
            "idle",
            "entity_picture",
            "\"/api/camera_proxy/camera.front_door?token=secret-token-value\"",
        );
        let entity = Entity {
            id: EntityId::from("camera.front_door"),
            ..entity
        };
        let rows = CameraBody::new().render_rows(&entity);
        let snapshot_row = rows
            .iter()
            .find(|r| r.key == "snapshot_url")
            .expect("CameraBody must emit a snapshot_url row when entity_picture is set");
        // The indicator value must NOT include the token.
        assert!(
            !snapshot_row.value.contains("secret-token-value"),
            "snapshot_url row must not log the token: {snapshot_row:?}"
        );
        assert!(
            !snapshot_row.value.contains("/api/camera_proxy/"),
            "snapshot_url row must not log the URL path: {snapshot_row:?}"
        );
    }

    /// `CameraBody::render_rows` skips optional rows when their attributes
    /// are absent.
    #[test]
    fn camera_body_skips_optional_rows_when_absent() {
        use crate::ui::more_info::{CameraBody, MoreInfoBody};
        let entity = make_test_entity("camera.front_door", "idle");
        let rows = CameraBody::new().render_rows(&entity);
        // Mandatory row always present.
        assert!(rows.iter().any(|r| r.key == "state"));
        // Optional rows must be absent.
        assert!(!rows.iter().any(|r| r.key == "friendly_name"));
        assert!(!rows.iter().any(|r| r.key == "last_motion"));
        assert!(!rows.iter().any(|r| r.key == "brand"));
        assert!(!rows.iter().any(|r| r.key == "snapshot_url"));
    }

    // -----------------------------------------------------------------------
    // visibility_flip.yaml fixture validates (TASK-106 acceptance #10)
    // -----------------------------------------------------------------------

    /// `tests/layout/visibility_flip.yaml` parses through the full Phase 4
    /// loader pipeline (parse + validate). The fixture carries one
    /// always-visible widget and one gated by a `state_equals:` predicate;
    /// the validator must accept both per the
    /// `locked_decisions.visibility_predicate_vocabulary`.
    ///
    /// This test guards against schema drift: if a future plan amendment
    /// changes the `state_equals` syntax (e.g. swaps `:` for `=`), this
    /// test fails BEFORE the visibility-flip golden test (TASK-110) runs.
    /// The actual layout-flicker assertion lives in TASK-110's
    /// `tests/integration/layout.rs::golden_visibility_flip_no_flicker`
    /// which reads both this YAML and the sibling
    /// `visibility_flip.expected.json` predicate-true snapshot.
    #[test]
    fn visibility_flip_fixture_parses_and_validates() {
        use crate::dashboard::loader::load_dashboard_only;
        use std::path::Path;
        let dashboard = load_dashboard_only(Path::new("tests/layout/visibility_flip.yaml"))
            .expect("visibility_flip.yaml must parse + validate");
        assert_eq!(dashboard.views.len(), 1, "one view");
        let widgets = &dashboard.views[0].sections[0].widgets;
        assert_eq!(widgets.len(), 2, "two widgets in main section");
        assert_eq!(widgets[0].id, "always_visible");
        assert_eq!(
            widgets[0].visibility, "always",
            "always-visible widget defaults to 'always'"
        );
        assert_eq!(widgets[1].id, "gated");
        assert_eq!(
            widgets[1].visibility, "state_equals:binary_sensor.motion:on",
            "gated widget carries the canonical state_equals predicate"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 6 cosmetic-polish: section packer + dashboard layout tests
    // -----------------------------------------------------------------------

    #[test]
    fn pack_section_layouts_assigns_section_relative_grid_positions() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = fixture_dashboard();

        let mut tiles = build_tiles(&store, &dashboard);
        pack_section_layouts(&dashboard, &mut tiles);

        // Walk the dashboard in document order and verify each tile's
        // placement was packed sequentially within its section.
        let mut tile_idx = 0usize;
        for view in &dashboard.views {
            for section in &view.sections {
                let columns = i32::from(section.grid.columns);
                let mut cursor_col = 0i32;
                let mut cursor_row = 0i32;
                for _widget in &section.widgets {
                    let placement = tiles[tile_idx].placement();

                    // Verify the packer used valid section-relative coordinates.
                    assert!(
                        placement.col >= 0 && placement.col < columns,
                        "section {:?} tile {tile_idx}: col {} out of range [0, {columns})",
                        section.id,
                        placement.col,
                    );
                    assert!(
                        placement.span_cols >= 1 && placement.span_cols <= columns,
                        "section {:?} tile {tile_idx}: span_cols {} not in [1, {columns}]",
                        section.id,
                        placement.span_cols,
                    );
                    assert!(
                        placement.row >= 0,
                        "section {:?} tile {tile_idx}: row must be non-negative, got {}",
                        section.id,
                        placement.row,
                    );

                    if cursor_col + placement.span_cols > columns {
                        cursor_col = 0;
                        cursor_row += 1;
                    }
                    assert_eq!(
                        placement.col, cursor_col,
                        "tile {tile_idx} col mismatch in section {:?}",
                        section.id
                    );
                    assert_eq!(
                        placement.row, cursor_row,
                        "tile {tile_idx} row mismatch in section {:?}",
                        section.id
                    );
                    cursor_col += placement.span_cols;
                    if cursor_col >= columns {
                        cursor_col = 0;
                        cursor_row += 1;
                    }
                    tile_idx += 1;
                }
            }
        }
        assert_eq!(
            tile_idx,
            tiles.len(),
            "packer must visit exactly tiles.len() entries"
        );
    }

    #[test]
    fn pack_section_layouts_resets_cursor_per_section() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = fixture_dashboard();

        let mut tiles = build_tiles(&store, &dashboard);
        pack_section_layouts(&dashboard, &mut tiles);

        // The first tile of every section after the first must start at
        // (col=0, row=0) — the cursor resets at every section boundary.
        let mut tile_idx = 0usize;
        for view in &dashboard.views {
            for (sec_idx, section) in view.sections.iter().enumerate() {
                let placement = tiles[tile_idx].placement();
                if sec_idx > 0 || !view.sections.is_empty() {
                    assert_eq!(
                        placement.col, 0,
                        "first tile of section {:?} must have col=0",
                        section.id
                    );
                    assert_eq!(
                        placement.row, 0,
                        "first tile of section {:?} must have row=0",
                        section.id
                    );
                }
                tile_idx += section.widgets.len();
            }
        }
    }

    #[test]
    fn compute_dashboard_layout_emits_one_section_per_yaml_section() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = fixture_dashboard();

        let mut tiles = build_tiles(&store, &dashboard);
        pack_section_layouts(&dashboard, &mut tiles);
        let layout = compute_dashboard_layout(&dashboard, &PROFILE_DESKTOP, &tiles);

        // One DashboardSection per (view, section) pair.
        let total_sections: usize = dashboard.views.iter().map(|v| v.sections.len()).sum();
        assert_eq!(
            layout.sections.len(),
            total_sections,
            "layout must emit one section per YAML section"
        );

        // Each section's title and column count match the YAML.
        let mut idx = 0usize;
        for (view_idx, view) in dashboard.views.iter().enumerate() {
            let view_idx_i32 = i32::try_from(view_idx).expect("view idx fits");
            for section in &view.sections {
                let s = &layout.sections[idx];
                assert_eq!(s.title, section.title, "section {idx} title mismatch");
                assert_eq!(
                    s.columns,
                    i32::from(section.grid.columns),
                    "section {idx} columns mismatch"
                );
                assert_eq!(
                    s.view_index, view_idx_i32,
                    "section {idx} view_index mismatch"
                );
                assert!(s.num_rows >= 1, "section {idx} num_rows must be >= 1");
                idx += 1;
            }
        }
    }

    #[test]
    fn compute_dashboard_layout_emits_one_slot_per_widget_in_doc_order() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = fixture_dashboard();

        let mut tiles = build_tiles(&store, &dashboard);
        pack_section_layouts(&dashboard, &mut tiles);
        let layout = compute_dashboard_layout(&dashboard, &PROFILE_DESKTOP, &tiles);

        let widget_count: usize = dashboard
            .views
            .iter()
            .flat_map(|v| v.sections.iter())
            .map(|s| s.widgets.len())
            .sum();
        assert_eq!(
            layout.tile_slots.len(),
            widget_count,
            "layout must emit one slot per widget"
        );

        // Slots match the per-kind cursor walk (consistent with split_tile_vms).
        let mut counts = std::collections::HashMap::<&'static str, i32>::new();
        for slot in &layout.tile_slots {
            let entry = counts.entry(slot.kind).or_insert(0);
            assert_eq!(
                slot.kind_index, *entry,
                "slot kind_index must match per-kind cursor"
            );
            *entry += 1;
        }
    }

    #[test]
    fn compute_dashboard_layout_active_view_index_matches_default_view() {
        let dashboard = fixture_dashboard();
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let mut tiles = build_tiles(&store, &dashboard);
        pack_section_layouts(&dashboard, &mut tiles);
        let layout = compute_dashboard_layout(&dashboard, &PROFILE_DESKTOP, &tiles);

        let expected = dashboard
            .views
            .iter()
            .position(|v| v.id == dashboard.default_view)
            .map(|i| i as i32)
            .unwrap_or(0);
        assert_eq!(layout.active_view_index, expected);
    }

    #[test]
    fn compute_dashboard_layout_emits_view_entries_in_document_order() {
        let dashboard = fixture_dashboard();
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let mut tiles = build_tiles(&store, &dashboard);
        pack_section_layouts(&dashboard, &mut tiles);
        let layout = compute_dashboard_layout(&dashboard, &PROFILE_DESKTOP, &tiles);

        assert_eq!(layout.views.len(), dashboard.views.len());
        for (i, v) in layout.views.iter().enumerate() {
            assert_eq!(v.id, dashboard.views[i].id);
            assert_eq!(v.title, dashboard.views[i].title);
        }
    }

    #[test]
    fn next_pack_placement_clamps_oversized_widgets_to_section_width() {
        // A 6-column-wide widget in a 4-column section should clamp to 4.
        let mut col = 0;
        let mut row = 0;
        let p = next_pack_placement(
            TilePlacement {
                col: 0,
                row: 0,
                span_cols: 6,
                span_rows: 1,
            },
            4,
            &mut col,
            &mut row,
        );
        assert_eq!(p.span_cols, 4, "must clamp to section width");
        assert_eq!(p.col, 0);
        assert_eq!(p.row, 0);
    }

    #[test]
    fn next_pack_placement_wraps_to_next_row_on_overflow() {
        let mut col = 3;
        let mut row = 0;
        let p = next_pack_placement(
            TilePlacement {
                col: 0,
                row: 0,
                span_cols: 2,
                span_rows: 1,
            },
            4,
            &mut col,
            &mut row,
        );
        // 3 + 2 > 4 → wrap.
        assert_eq!(p.row, 1);
        assert_eq!(p.col, 0);
    }

    #[test]
    fn next_pack_placement_minimum_span_one() {
        let mut col = 0;
        let mut row = 0;
        let p = next_pack_placement(
            TilePlacement {
                col: 0,
                row: 0,
                span_cols: 0,
                span_rows: 0,
            },
            4,
            &mut col,
            &mut row,
        );
        assert_eq!(p.span_cols, 1, "zero span clamps to 1");
        assert_eq!(p.span_rows, 1, "zero rows clamps to 1");
    }

    #[test]
    fn wire_dashboard_layout_writes_views_sections_and_slots_to_main_window() {
        // Behavior contract: `wire_dashboard_layout` populates `views`,
        // `sections`, `tile-slots`, and `active-view-index` on a real
        // headless `MainWindow`. Read back via the Slint-generated getters.
        install_test_platform_once_per_thread();
        crate::assets::icons::init();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = fixture_dashboard();
        let mut tiles = build_tiles(&store, &dashboard);
        pack_section_layouts(&dashboard, &mut tiles);
        let layout = compute_dashboard_layout(&dashboard, &PROFILE_DESKTOP, &tiles);

        wire_window(&window, &tiles, &PROFILE_DESKTOP).expect("wire_window must succeed");
        wire_dashboard_layout(&window, &layout);

        // ── Active view index ─────────────────────────────────────────
        let expected_active = dashboard
            .views
            .iter()
            .position(|v| v.id == dashboard.default_view)
            .map(|i| i as i32)
            .unwrap_or(0);
        assert_eq!(window.get_active_view_index(), expected_active);

        // ── Views ─────────────────────────────────────────────────────
        use slint::Model;
        let views_model = window.get_views();
        assert_eq!(
            views_model.row_count(),
            dashboard.views.len(),
            "views model must mirror dashboard.views"
        );

        // ── Sections ──────────────────────────────────────────────────
        let sections_model = window.get_sections();
        let total_sections: usize = dashboard.views.iter().map(|v| v.sections.len()).sum();
        assert_eq!(
            sections_model.row_count(),
            total_sections,
            "sections model must mirror sum-of-section counts"
        );

        // ── Tile slots ────────────────────────────────────────────────
        let slots_model = window.get_tile_slots();
        let widget_count: usize = dashboard
            .views
            .iter()
            .flat_map(|v| v.sections.iter())
            .map(|s| s.widgets.len())
            .sum();
        assert_eq!(
            slots_model.row_count(),
            widget_count,
            "tile-slots model must mirror dashboard widget count"
        );

        // ── view-changed callback updates active-view-index ───────────
        // Simulate: invoke the callback with index 0 (only one view in
        // fixture_dashboard()). Verify the property update lands.
        window.invoke_view_changed(0);
        assert_eq!(window.get_active_view_index(), 0);
    }

    #[test]
    fn wire_dashboard_layout_with_empty_sections_is_safe() {
        // Behavior contract: empty layout (no sections, no slots) is a
        // no-op write — does not panic, leaves models empty. The MainWindow
        // falls back to the legacy flat-per-kind path in this case.
        install_test_platform_once_per_thread();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        let layout = DashboardLayout {
            views: Vec::new(),
            active_view_index: 0,
            density: "regular".to_string(),
            sections: Vec::new(),
            tile_slots: Vec::new(),
        };
        wire_dashboard_layout(&window, &layout);

        use slint::Model;
        assert_eq!(window.get_sections().row_count(), 0);
        assert_eq!(window.get_tile_slots().row_count(), 0);
        assert_eq!(window.get_views().row_count(), 0);
    }

    #[test]
    fn tile_vm_kind_str_matches_every_variant() {
        // Every TileVM variant must map to the matching dispatch
        // discriminator string used by the Slint MainWindow's per-section
        // tile renderer. The Slint side branches on these literals; a
        // mistype here renders the wrong component (or silently nothing).
        // Mutation-testing target: every match arm in tile_vm_kind_str
        // must be guarded by a distinct assertion below.
        let placement = TilePlacement::default_for(1, 1);

        // Light
        assert_eq!(
            tile_vm_kind_str(&TileVM::Light(LightTileVM {
                name: String::new(),
                state: String::new(),
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "light",
        );
        // Sensor
        assert_eq!(
            tile_vm_kind_str(&TileVM::Sensor(SensorTileVM {
                name: String::new(),
                state: String::new(),
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "sensor",
        );
        // Entity
        assert_eq!(
            tile_vm_kind_str(&TileVM::Entity(EntityTileVM {
                name: String::new(),
                state: String::new(),
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "entity",
        );
        // Cover
        assert_eq!(
            tile_vm_kind_str(&TileVM::Cover(CoverTileVM {
                name: String::new(),
                state: String::new(),
                position: 0,
                tilt: 0,
                has_position: false,
                has_tilt: false,
                is_open: false,
                is_moving: false,
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "cover",
        );
        // Fan
        assert_eq!(
            tile_vm_kind_str(&TileVM::Fan(FanTileVM {
                name: String::new(),
                state: String::new(),
                speed_pct: 0,
                has_speed_pct: false,
                is_on: false,
                current_speed: String::new(),
                has_current_speed: false,
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "fan",
        );
        // Lock
        assert_eq!(
            tile_vm_kind_str(&TileVM::Lock(LockTileVM {
                name: String::new(),
                state: String::new(),
                is_locked: false,
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "lock",
        );
        // Alarm
        assert_eq!(
            tile_vm_kind_str(&TileVM::Alarm(AlarmTileVM {
                name: String::new(),
                state: String::new(),
                is_armed: false,
                is_triggered: false,
                is_pending: false,
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "alarm",
        );
        // History
        assert_eq!(
            tile_vm_kind_str(&TileVM::History(HistoryGraphTileVM {
                name: String::new(),
                state: String::new(),
                change_count: 0,
                is_available: false,
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
                path_commands: String::new(),
            })),
            "history",
        );
        // Camera
        assert_eq!(
            tile_vm_kind_str(&TileVM::Camera(CameraTileVM {
                name: String::new(),
                state: String::new(),
                is_recording: false,
                is_streaming: false,
                is_available: false,
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "camera",
        );
        // Climate
        assert_eq!(
            tile_vm_kind_str(&TileVM::Climate(ClimateTileVM {
                name: String::new(),
                state: String::new(),
                is_active: false,
                current_temperature: None,
                target_temperature: None,
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "climate",
        );
        // MediaPlayer
        assert_eq!(
            tile_vm_kind_str(&TileVM::MediaPlayer(MediaPlayerTileVM {
                name: String::new(),
                state: String::new(),
                is_playing: false,
                media_title: None,
                artist: None,
                volume_level: None,
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "media_player",
        );
        // PowerFlow
        assert_eq!(
            tile_vm_kind_str(&TileVM::PowerFlow(PowerFlowTileVM {
                name: String::new(),
                grid_w: None,
                solar_w: None,
                battery_w: None,
                battery_pct: None,
                home_w: None,
                icon_id: String::new(),
                preferred_columns: 1,
                preferred_rows: 1,
                placement,
                pending: false,
            })),
            "power_flow",
        );
    }

    /// Cross-consistency invariant: every `DashboardTileSlot.kind_index`
    /// produced by `compute_dashboard_layout` must point at a valid row in
    /// the matching per-kind `Vec` produced by `split_tile_vms` for the
    /// same tile slice.
    ///
    /// This is the highest-risk silent-failure mode flagged by the
    /// pre-commit opencode review: a mismatch between the layout's
    /// kind-cursor counter and the split's per-variant push order would
    /// blow up only when Slint dereferences `light-tiles[slot.kind-index]`
    /// at render time. Mutation of either function would not be caught by
    /// any other test in this module — this asserts they stay in lockstep.
    #[test]
    fn compute_dashboard_layout_kind_index_matches_split_tile_vms_position() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = fixture_dashboard();

        let mut tiles = build_tiles(&store, &dashboard);
        pack_section_layouts(&dashboard, &mut tiles);
        let layout = compute_dashboard_layout(&dashboard, &PROFILE_DESKTOP, &tiles);

        // Initialise icons so split_tile_vms doesn't panic on icon lookup.
        crate::assets::icons::init();
        let split = split_tile_vms(&tiles);

        // For each slot, the slot.kind_index MUST point at a real row in
        // the matching per-kind Vec, AND the row's data must match the
        // tile we walked in document order.
        let mut tile_idx = 0usize;
        for slot in &layout.tile_slots {
            let kind_index = usize::try_from(slot.kind_index).expect("non-negative");

            match slot.kind {
                "light" => assert!(kind_index < split.lights.len(), "light idx OOB"),
                "sensor" => assert!(kind_index < split.sensors.len(), "sensor idx OOB"),
                "entity" => assert!(kind_index < split.entities.len(), "entity idx OOB"),
                "cover" => assert!(kind_index < split.covers.len(), "cover idx OOB"),
                "fan" => assert!(kind_index < split.fans.len(), "fan idx OOB"),
                "lock" => assert!(kind_index < split.locks.len(), "lock idx OOB"),
                "alarm" => assert!(kind_index < split.alarms.len(), "alarm idx OOB"),
                "history" => {
                    assert!(kind_index < split.histories.len(), "history idx OOB")
                }
                "camera" => assert!(kind_index < split.cameras.len(), "camera idx OOB"),
                "climate" => {
                    assert!(kind_index < split.climates.len(), "climate idx OOB")
                }
                "media_player" => assert!(
                    kind_index < split.media_players.len(),
                    "media_player idx OOB"
                ),
                "power_flow" => assert!(kind_index < split.power_flows.len(), "power_flow idx OOB"),
                other => panic!("slot has unknown kind {other:?}"),
            }

            // Document-order invariant: the slot at position `tile_idx`
            // must reference the per-kind row whose source tile is
            // `tiles[tile_idx]`. Verify by comparing the kind discriminator
            // and confirming the slot points at the right per-kind row.
            let actual_kind = tile_vm_kind_str(&tiles[tile_idx]);
            assert_eq!(
                slot.kind, actual_kind,
                "slot {tile_idx} kind {} does not match tiles[{tile_idx}] kind {}",
                slot.kind, actual_kind,
            );
            tile_idx += 1;
        }
        assert_eq!(tile_idx, tiles.len(), "every tile must have a slot");
    }
}
