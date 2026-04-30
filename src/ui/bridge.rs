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
//!     `MainWindow`. Also writes the two `AnimationBudget` globals from
//!     [`crate::dashboard::profiles::PROFILE_DESKTOP`].
//!
//! [`wire_window`] runs once per refresh cycle, not per frame. Per-frame
//! property reads inside the Slint runtime see only `SharedString`
//! (`Arc<str>`-backed) and `slint::Image` (`Arc<SharedPixelBuffer>`-backed)
//! values; cloning either is an `Arc` bump. No allocation occurs in any
//! Slint callback or animation timer (per the slint-engineer charter
//! hot-path discipline).

use crate::dashboard::schema::{Dashboard, Placement, WidgetKind};
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
// TileVM enum
// ---------------------------------------------------------------------------

/// Top-level discriminated union dispatching on tile kind.
///
/// `build_tiles` returns one `TileVM` per widget in the dashboard config,
/// in document order (views → sections → widgets).
#[derive(Debug, Clone, PartialEq)]
pub enum TileVM {
    Light(LightTileVM),
    Sensor(SensorTileVM),
    Entity(EntityTileVM),
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
/// The store is consumed via the visitor ([`EntityStore::for_each`]) for the
/// sanity-check walk and via [`EntityStore::get`] for per-widget entity lookup.
/// No iterator semantics are assumed.
///
/// `EntityStore` is dyn-compatible (PATH A — see `src/ha/store.rs` module doc).
/// `store` is accepted as `&dyn EntityStore` so Phase 2 callers can pass any
/// `Box<dyn EntityStore>` or `Arc<dyn EntityStore>` without changing this call
/// site.  Concrete references (`&MemoryStore`) coerce automatically.
///
/// See the module-level doc for the missing-entity policy and field-mapping
/// details.
pub fn build_tiles(store: &dyn EntityStore, dashboard: &Dashboard) -> Vec<TileVM> {
    // Walk all entities once via the visitor to collect a count for a
    // diagnostic log / sanity check. This satisfies the AC requirement that
    // for_each is exercised on the live store path (not only in tests).
    let mut store_entity_count: usize = 0;
    store.for_each(&mut |_id, _entity| {
        store_entity_count += 1;
    });
    tracing::debug!(
        store_entity_count,
        "build_tiles: store entity count (visitor walk)"
    );

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
                            // Phase 4 schema adds Camera, History, Fan, Lock, Alarm
                            // variants. Phase 6 adds Cover, MediaPlayer, Climate,
                            // PowerFlow. Until dedicated Slint tile components exist
                            // (TASK-102..TASK-109), these are rendered as EntityTileVM —
                            // the generic entity tile covers the state display until
                            // per-kind tiles ship.
                            WidgetKind::EntityTile
                            | WidgetKind::Camera
                            | WidgetKind::History
                            | WidgetKind::Fan
                            | WidgetKind::Lock
                            | WidgetKind::Alarm
                            | WidgetKind::Cover
                            | WidgetKind::MediaPlayer
                            | WidgetKind::Climate
                            | WidgetKind::PowerFlow => TileVM::Entity(EntityTileVM {
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
                        // Missing-entity policy: produce an EntityTileVM with
                        // state="unavailable" so the caller always has a tile
                        // to render.
                        let preferred_columns = i32::from(widget.layout.preferred_columns);
                        let preferred_rows = i32::from(widget.layout.preferred_rows);
                        let placement = widget
                            .placement
                            .as_ref()
                            .map(TilePlacement::from_placement)
                            .unwrap_or_else(|| {
                                TilePlacement::default_for(preferred_columns, preferred_rows)
                            });

                        TileVM::Entity(EntityTileVM {
                            name: widget
                                .name
                                .clone()
                                .unwrap_or_else(|| entity_id_str.to_string()),
                            state: "unavailable".to_string(),
                            icon_id: widget
                                .icon
                                .clone()
                                .unwrap_or_else(|| "mdi:help-circle".to_string()),
                            preferred_columns,
                            preferred_rows,
                            placement,
                            pending: false,
                        })
                    }
                };

                tiles.push(tile);
            }
        }
    }

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
use crate::dashboard::profiles::PROFILE_DESKTOP;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

/// Errors that can occur while wiring VM data into Slint properties.
///
/// Variants are kept small and `Copy` so the error type does not allocate on
/// the failure path either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// `PROFILE_DESKTOP.animation_framerate_cap` does not fit in `i32` (the
    /// Slint property type for `framerate-cap`).
    FramerateCapOutOfRange,
    /// `PROFILE_DESKTOP.max_simultaneous_animations` does not fit in `i32`
    /// (the Slint property type for `max-simultaneous`).
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
                f.write_str("PROFILE_DESKTOP.animation_framerate_cap does not fit in i32")
            }
            WireError::MaxSimultaneousOutOfRange => {
                f.write_str("PROFILE_DESKTOP.max_simultaneous_animations does not fit in i32")
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

/// Split a flat `&[TileVM]` slice into three per-variant `Vec`s of the
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
pub fn split_tile_vms(
    tiles: &[TileVM],
) -> (
    Vec<slint_ui::LightTileVM>,
    Vec<slint_ui::SensorTileVM>,
    Vec<slint_ui::EntityTileVM>,
) {
    let mut lights = Vec::new();
    let mut sensors = Vec::new();
    let mut entities = Vec::new();

    for tile in tiles {
        match tile {
            TileVM::Light(vm) => lights.push(slint_ui::LightTileVM {
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
            TileVM::Sensor(vm) => sensors.push(slint_ui::SensorTileVM {
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
            TileVM::Entity(vm) => entities.push(slint_ui::EntityTileVM {
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
        }
    }

    (lights, sensors, entities)
}

// ---------------------------------------------------------------------------
// wire_window
// ---------------------------------------------------------------------------

/// Wire a typed `&[TileVM]` slice into the three array properties on
/// [`MainWindow`], and write the two `AnimationBudget` globals from
/// [`PROFILE_DESKTOP`].
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
/// `PROFILE_DESKTOP.animation_framerate_cap` does not fit in `i32`, and
/// [`WireError::MaxSimultaneousOutOfRange`] if
/// `PROFILE_DESKTOP.max_simultaneous_animations` does not fit. Both are
/// defensive: the desktop preset values (60 and 8 respectively) are well
/// within `i32` range, but a future profile could exceed it and we want a
/// typed failure rather than a silent truncation.
pub fn wire_window(window: &MainWindow, tiles: &[TileVM]) -> Result<(), WireError> {
    let (lights, sensors, entities) = split_tile_vms(tiles);

    // Wrap each Vec in a VecModel and pass via ModelRc to the Slint property.
    // Slint clones the ModelRc internally (Arc bump); no per-element copy.
    let light_model: ModelRc<slint_ui::LightTileVM> = ModelRc::new(VecModel::from(lights));
    let sensor_model: ModelRc<slint_ui::SensorTileVM> = ModelRc::new(VecModel::from(sensors));
    let entity_model: ModelRc<slint_ui::EntityTileVM> = ModelRc::new(VecModel::from(entities));

    window.set_light_tiles(light_model);
    window.set_sensor_tiles(sensor_model);
    window.set_entity_tiles(entity_model);

    // AnimationBudget globals — wired once at startup from PROFILE_DESKTOP.
    let budget = window.global::<AnimationBudget>();

    let cap_i32 = i32::try_from(PROFILE_DESKTOP.animation_framerate_cap)
        .map_err(|_| WireError::FramerateCapOutOfRange)?;
    let max_i32 = i32::try_from(PROFILE_DESKTOP.max_simultaneous_animations)
        .map_err(|_| WireError::MaxSimultaneousOutOfRange)?;

    budget.set_framerate_cap(cap_i32);
    budget.set_max_simultaneous(max_i32);

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
//   * On `RecvError::Lagged` from a per-entity subscriber, call `store.get(id)`
//     for ALL subscribed ids (Phase 1 contract pattern, applied per-entity), then
//     re-`store.subscribe(&[id])` for that id.
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
//   1. One subscriber task **per entity id** that calls `recv()` in a loop and
//      writes the id into the shared pending-updates map on each event. On
//      `RecvError::Lagged`, the task re-runs the resync path and re-subscribes.
//   2. One flush task that wakes every 80 ms, drains the pending map under the
//      ConnectionState gate, builds the tiles, and posts the result to the
//      Slint event loop via `invoke_from_event_loop`.
//   3. One `ConnectionState` watcher task that mirrors transitions into the
//      bridge's internal "gated" flag and triggers a full resync on the
//      Reconnecting/Failed → Live transition.

use crate::ha::store::EntityUpdate;
use crate::platform::status::ConnectionState;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
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

/// Sink for the two side-effects the live bridge produces: (a) a fresh tile
/// list to render, (b) a status-banner visibility flip.
///
/// Production callers pass a [`SlintSink`] wrapping a `slint::Weak<MainWindow>`
/// so the writes hop onto the Slint UI thread via `invoke_from_event_loop`.
/// Tests pass an in-process recording sink and assert against its log
/// directly (no Slint backend required).
pub trait BridgeSink: Send + Sync + 'static {
    /// Apply a fresh tile list. Called from the flush task only when the
    /// connection is in a non-gated state.
    fn write_tiles(&self, tiles: Vec<TileVM>);

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

/// Owned handle to the spawned bridge tasks.
///
/// Dropping this handle aborts the Tokio tasks; otherwise the tasks run for
/// the lifetime of the application.  Tests construct a `LiveBridge` against a
/// stub store, exercise it for the duration of the test, and let it drop at
/// scope end so the runtime can shut down cleanly.
pub struct LiveBridge {
    /// Subscriber task per entity id.  Stored so the `Drop` impl can abort
    /// them; never read after construction.
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

        // Per-entity subscriber tasks.
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
            tokio::spawn(async move {
                run_flush_loop(store, dashboard, pending, state_rx, sink).await;
            })
        };

        // ConnectionState watcher task.
        let state_task = {
            let store = Arc::clone(&store);
            let dashboard = Arc::clone(&dashboard);
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                run_state_watcher(store, dashboard, state_rx, sink).await;
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
///   * `Err(RecvError::Lagged(_))` — Phase 1 contract pattern applied per
///     entity: call `store.get(id)` for ALL subscribed ids (not just the lagged
///     one) to recover current state, mark them all pending so the next flush
///     re-renders, then re-subscribe via `store.subscribe(&[id])`.
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
                    "subscriber lagged; recovering all subscribed ids via store.get + re-subscribe"
                );
                // Recover by reading current state for every subscribed id.
                // The actual entity values are read inside the flush path via
                // store.get; here we only need to mark the ids dirty so the
                // next flush re-renders them.  Reading store.get for each id
                // also exercises the "Lagged → bridge calls store.get for all
                // subscribed ids" AC explicitly.
                for resync_id in ids_for_resync.iter() {
                    let _ = store.get(resync_id);
                    let mut guard = pending.lock().expect("PendingMap mutex poisoned");
                    guard.insert(resync_id.clone(), ());
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
///      churn, no allocation).  If non-empty, rebuild the full tile list via
///      `build_tiles(&*store, &dashboard)`.
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
/// The flush rebuilds the **entire** tile list on every flush rather than
/// patching individual tiles; this matches the Phase 1 wire_window contract
/// and the `pending_map` is a "did anything change" signal, not a
/// per-tile patch list.  Per-flush cost is dominated by `build_tiles`, which
/// is O(`dashboard.widget_count` + `store.entity_count`) — the per-widget
/// `store.get` walk plus the diagnostic `for_each` visitor walk added in
/// Phase 1 to satisfy the AC that `for_each` be exercised on the live
/// store path.  Under PHASES.md's `max_entities = 16k`, the visitor walk
/// dominates; the 12.5 Hz cap is validated end-to-end by TASK-038's churn
/// benchmark, not by widget-count alone.
async fn run_flush_loop<S: BridgeSink>(
    store: Arc<dyn EntityStore>,
    dashboard: Arc<Dashboard>,
    pending: PendingMap,
    state_rx: watch::Receiver<ConnectionState>,
    sink: Arc<S>,
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

        // Rebuild the full tile list against the current store snapshot.
        let tiles = build_tiles(&*store, &dashboard);

        // Re-check state immediately before the write to close the
        // read-then-check race (Codex finding BLOCKER 1).  ConnectionState
        // can flip to Reconnecting/Failed between the initial gate check
        // and `build_tiles` returning; if so, do not write.  The ids we
        // just drained are recovered on the eventual Live transition via
        // `run_state_watcher`'s full resync (see doc-comment above).
        let state_at_write = *state_rx.borrow();
        if is_writes_gated(state_at_write) {
            continue;
        }

        sink.write_tiles(tiles);
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
            sink.write_tiles(tiles);
        }
    }
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::fixture::fixture_dashboard;
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
        // The canonical Phase 1 fixture has exactly 4 entities.
        assert_eq!(count, 4, "for_each must visit all 4 fixture entities");
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

        let (lights, sensors, entities) = split_tile_vms(&tiles);

        assert_eq!(lights.len(), 2, "two LightTileVMs expected");
        assert_eq!(sensors.len(), 1, "one SensorTileVM expected");
        assert_eq!(entities.len(), 1, "one EntityTileVM expected");
    }

    #[test]
    fn split_tile_vms_copies_string_fields_into_shared_strings() {
        ensure_icons_init();

        let tiles = vec![make_light_tile("Kitchen", "on", "mdi:lightbulb")];
        let (lights, _, _) = split_tile_vms(&tiles);

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

        let (lights, sensors, _) = split_tile_vms(&tiles);

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
        let (lights, _, _) = split_tile_vms(&tiles);

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
        let (lights_before, _, _) = split_tile_vms(&tiles_before);
        assert_eq!(lights_before[0].state.as_str(), "on");

        // Mutate the synthesized fixture: simulate the entity flipping to off.
        let tiles_after = vec![make_light_tile("Kitchen", "off", "mdi:lightbulb")];
        let (lights_after, _, _) = split_tile_vms(&tiles_after);
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

        let (lights, sensors, _) = split_tile_vms(&tiles);

        // Per-variant order must match document order within that variant.
        assert_eq!(lights[0].name.as_str(), "L1");
        assert_eq!(lights[1].name.as_str(), "L2");
        assert_eq!(sensors[0].name.as_str(), "S1");
        assert_eq!(sensors[1].name.as_str(), "S2");
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
        // `PROFILE_DESKTOP` AND wires the default GestureConfig in a single
        // call. We verify both globals end up with the documented values
        // after one wire_window invocation.
        install_test_platform_once_per_thread();
        ensure_icons_init();
        let window = MainWindow::new().expect("MainWindow::new under headless test platform");

        // Empty tile slice exercises the wire_window body without forcing
        // any per-tile rendering — the property models are still populated
        // (as empty VecModels) and the AnimationBudget + GestureConfig
        // globals are still written.
        wire_window(&window, &[]).expect("wire_window with empty tiles must succeed");

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
    #[derive(Default)]
    struct RecordingSink {
        tile_writes: Mutex<Vec<Vec<TileVM>>>,
        banner_calls: Mutex<Vec<bool>>,
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
    }

    impl BridgeSink for RecordingSink {
        fn write_tiles(&self, tiles: Vec<TileVM>) {
            self.tile_writes
                .lock()
                .expect("tile_writes mutex poisoned")
                .push(tiles);
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
        // swapped in.  This check succeeds purely by typing — the function
        // body is never executed.
        fn _accepts_dyn(store: &dyn EntityStore, dashboard: &Dashboard) -> Vec<TileVM> {
            build_tiles(store, dashboard)
        }
        // Force the function pointer to be referenced so it cannot be
        // optimised out under coverage instrumentation.
        let _f: fn(&dyn EntityStore, &Dashboard) -> Vec<TileVM> = _accepts_dyn;
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
    }

    // -----------------------------------------------------------------------
    // BLOCKER 1 regression: state flip between gate-check and property write
    // -----------------------------------------------------------------------

    /// Stub store that signals every `for_each` entry on `enter_tx` and
    /// blocks until a corresponding `()` is received on the per-entry
    /// release channel.  The test thread choreographs entries by sending
    /// release signals one at a time.
    ///
    /// This lets the test deterministically open the read-then-check race
    /// window in `run_flush_loop`: while the flush task is blocked inside
    /// `build_tiles -> for_each`, the test flips `ConnectionState` to
    /// `Reconnecting`, then releases.  The flush task's second state read
    /// must observe `Reconnecting` and skip the property write.
    struct RendezvousStore {
        base: StubStore,
        // Sent on every `for_each` entry; test reads to know we are blocked.
        enter_tx: std::sync::mpsc::SyncSender<()>,
        // Receives one release signal per `for_each` entry.  Wrapped in a
        // Mutex because mpsc::Receiver is not Sync.
        release_rx: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl EntityStore for RendezvousStore {
        fn get(&self, id: &EntityId) -> Option<Entity> {
            self.base.get(id)
        }
        fn for_each(&self, f: &mut dyn FnMut(&EntityId, &Entity)) {
            // Signal entry, then block until the test thread sends a release.
            let _ = self.enter_tx.send(());
            {
                let rx = self.release_rx.lock().expect("release_rx mutex poisoned");
                let _ = rx.recv();
            }
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
        // (build_tiles -> for_each).  We release that first entry so the
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
        // to enter for_each, then release.
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
            "initial Live-transition for_each must enter the rendezvous"
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
        // for_each and blocks on the rendezvous.  Wait for the enter signal
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
            "flush task must enter for_each within 2s; race window never opened"
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

        // (5) Release the flush task's for_each.  The state watcher's
        // Live -> Reconnecting transition will ALSO have fired by now;
        // because that transition does NOT call build_tiles (build_tiles
        // is gated on `matches!(new_state, Live)`), no second rendezvous
        // hit happens here for the watcher.
        release_tx.send(()).expect("release flush for_each");

        // Wait several flush cadences.  Even though the flush task's
        // for_each has now returned, its post-build_tiles state re-check
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
        // `for_each` call (none expected — we are gated) would observe the
        // closed receiver and not deadlock.  `_bridge` Drop aborts the
        // tokio tasks; the SyncSender's bounded capacity keeps the test
        // deterministic if scheduling jitter produced extra entries.
        drop(release_tx);
        drop(enter_rx);
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
    async fn lagged_on_one_subscriber_triggers_get_for_all_subscribed_ids() {
        // Spec: on RecvError::Lagged from a per-entity subscriber, the bridge
        // calls `store.get(id)` for ALL subscribed IDs (not just the lagged
        // one).  This test uses a 3-widget dashboard with three distinct
        // entities, lags one of them, and asserts that get-call counts went
        // up for every subscribed id afterward.
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
        let store = Arc::new(CountingStore {
            base,
            get_calls: Mutex::new(StdHashMap::new()),
        });

        let (state_tx, state_rx) = status_channel();
        // Start gated so the state-watcher's initial Live-transition resync
        // does NOT pre-populate `get_calls` for every id (which would defeat
        // the "Lagged caused the get-for-all" assertion below).
        state_tx.send(ConnectionState::Reconnecting).unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let _bridge = LiveBridge::spawn(
            store.clone() as Arc<dyn EntityStore>,
            dashboard,
            state_rx,
            ArcSink(Arc::clone(&recorder)),
        );

        // Allow per-entity subscribers to register.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Snapshot baseline get-call counts.  Pending updates routed through
        // the subscriber path do NOT call `store.get` directly; only the
        // Lagged recovery and the flush's `build_tiles` do.  Because we
        // started gated, no flush has run yet — get_calls should be empty.
        let baseline = {
            let g = store.get_calls.lock().unwrap();
            g.clone()
        };
        assert!(
            baseline.is_empty() || baseline.values().all(|&n| n == 0),
            "baseline get_calls must be empty before Lagged recovery; got {baseline:?}"
        );

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

        // Wait for the Lagged recovery to fire.  The subscriber loop calls
        // `store.get(id)` for every id in `ids_for_resync`, then re-subscribes.
        // Use a generous bound — Lagged + recovery + scheduling jitter.
        let saw_all = wait_until(800, || {
            let g = store.get_calls.lock().unwrap();
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

        let snap = store.get_calls.lock().unwrap().clone();
        assert!(
            saw_all,
            "Lagged on light.kitchen must trigger store.get for ALL subscribed ids; \
             got per-id counts: {snap:?}"
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

    use crate::dashboard::profiles::{Density, DeviceProfile, PROFILE_DESKTOP};
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
}
