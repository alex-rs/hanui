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
//! # Slint property wiring (TASK-015)
//!
//! Below the typed-VM layer, this file also defines:
//!
//!   * The top-level `MainWindow` Slint component (declared inline via
//!     `slint::slint!{}` inside the [`slint_ui`] sub-module so the generated
//!     names do not collide with the Rust VM structs that share names with
//!     their Slint counterparts).
//!   * [`wire_window`] — splits a `&[TileVM]` slice by variant, converts each
//!     element into the Slint-generated VM struct (resolving `icon_id` via
//!     [`crate::assets::icons::resolve`]), wraps each per-variant `Vec` in a
//!     `slint::ModelRc<...>`, and writes the three array properties on
//!     `MainWindow`. Also writes the two `AnimationBudget` globals from
//!     [`crate::dashboard::profiles::DEFAULT_PROFILE`].
//!
//! [`wire_window`] runs once per refresh cycle, not per frame. Per-frame
//! property reads inside the Slint runtime see only `SharedString`
//! (`Arc<str>`-backed) and `slint::Image` (`Arc<SharedPixelBuffer>`-backed)
//! values; cloning either is an `Arc` bump. No allocation occurs in any
//! Slint callback or animation timer (per the slint-engineer charter
//! hot-path discipline).

use crate::dashboard::view_spec::{Dashboard, Placement, WidgetKind};
use crate::ha::entity::{EntityId, EntityKind};
use crate::ha::store::EntityStore;

// ---------------------------------------------------------------------------
// TilePlacement  (mirrors TilePlacement / SensorTilePlacement /
//                          EntityTilePlacement in the Slint tile files)
// ---------------------------------------------------------------------------

/// Computed grid placement for a tile, mirroring `TilePlacement` /
/// `SensorTilePlacement` / `EntityTilePlacement` in the Slint tile files and
/// `dashboard::view_spec::Placement` in the data layer.
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
    fn from_view_spec(p: &Placement) -> Self {
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
#[derive(Debug, Clone, PartialEq)]
pub struct LightTileVM {
    pub name: String,
    pub state: String,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
}

// ---------------------------------------------------------------------------
// SensorTileVM
// ---------------------------------------------------------------------------

/// View-model for a `SensorTile` widget, mirroring the Slint `SensorTileVM`
/// struct in `ui/slint/sensor_tile.slint`.
///
/// The `icon: image` Slint field is absent; it is written by the Slint bridge
/// during property wiring (TASK-015).
#[derive(Debug, Clone, PartialEq)]
pub struct SensorTileVM {
    pub name: String,
    pub state: String,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
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
#[derive(Debug, Clone, PartialEq)]
pub struct EntityTileVM {
    pub name: String,
    pub state: String,
    pub icon_id: String,
    pub preferred_columns: i32,
    pub preferred_rows: i32,
    pub placement: TilePlacement,
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
                            .map(TilePlacement::from_view_spec)
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
                            }),
                            WidgetKind::SensorTile => TileVM::Sensor(SensorTileVM {
                                name,
                                state,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
                            }),
                            WidgetKind::EntityTile => TileVM::Entity(EntityTileVM {
                                name,
                                state,
                                icon_id,
                                preferred_columns,
                                preferred_rows,
                                placement,
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
                            .map(TilePlacement::from_view_spec)
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
// Slint window definition (inline) and property wiring
// ---------------------------------------------------------------------------
//
// The MainWindow component is defined inline via `slint::slint!{}` because the
// `files_allowlist` for TASK-015 limits .slint authoring to the existing tile
// files only (`ui/slint/{theme,card_base,light_tile,sensor_tile,entity_tile}.slint`).
// Keeping the top-level window inline in `bridge.rs` also means TASK-016
// (`main.rs` wiring) needs to know about a single Rust artefact, not a separate
// `.slint` file.
//
// The macro is wrapped in a `slint_ui` sub-module to namespace the generated
// types: the Slint compiler emits Rust types named `LightTileVM`, `SensorTileVM`,
// `EntityTileVM`, `TilePlacement`, `SensorTilePlacement`, `EntityTilePlacement`
// — names that collide 1:1 with the public Rust structs declared above. The
// sub-module gives them a distinct path (`slint_ui::LightTileVM`) so callers of
// the bridge see both: the typed Rust VMs (from TASK-014) and the Slint-typed
// ones (from this task).
//
// Path note: with rustc >= 1.88, `slint::slint!{}` resolves `import` paths
// relative to the source file containing the macro. From `src/ui/bridge.rs`
// that is `../../ui/slint/...`.
pub mod slint_ui {
    slint::slint! {
        import { Theme } from "../../ui/slint/theme.slint";
        import { CardBase } from "../../ui/slint/card_base.slint";
        import { LightTile, LightTileVM, TilePlacement } from "../../ui/slint/light_tile.slint";
        import { SensorTile, SensorTileVM, SensorTilePlacement } from "../../ui/slint/sensor_tile.slint";
        import { EntityTile, EntityTileVM, EntityTilePlacement } from "../../ui/slint/entity_tile.slint";

        // Re-export `AnimationBudget` from this compilation root so the Slint
        // compiler emits a public Rust handle for it. Without this re-export,
        // the global is in-scope for binding expressions but not surfaced as
        // a `pub struct AnimationBudget<'a>` on the generated API — the Rust
        // bridge cannot then call `window.global::<AnimationBudget>()` to
        // write `framerate-cap` and `max-simultaneous` from `DEFAULT_PROFILE`.
        export { AnimationBudget } from "../../ui/slint/card_base.slint";

        // MainWindow — the top-level Phase 1 window. It exposes three array
        // properties (one per tile kind) that `wire_window` writes from the
        // typed VM slices, and renders each tile in a vertical flow. The
        // layout is intentionally minimal: TASK-016 wires this window into
        // `main.rs`; the Phase 1 design is a stack of tiles, not the final
        // grid layout.
        //
        // Property naming uses kebab-case per Slint convention; the
        // generated Rust setters are `set_light_tiles`, `set_sensor_tiles`,
        // `set_entity_tiles` (kebab → snake automatically).
        export component MainWindow inherits Window {
            in property <[LightTileVM]> light-tiles;
            in property <[SensorTileVM]> sensor-tiles;
            in property <[EntityTileVM]> entity-tiles;

            title: "hanui";
            background: Theme.background;
            preferred-width: 480px;
            preferred-height: 600px;

            VerticalLayout {
                padding: Theme.space-3;
                spacing: Theme.space-2;

                for tile[i] in root.light-tiles : LightTile {
                    view-model: tile;
                }
                for tile[i] in root.sensor-tiles : SensorTile {
                    view-model: tile;
                }
                for tile[i] in root.entity-tiles : EntityTile {
                    view-model: tile;
                }
            }
        }
    }
}

pub use slint_ui::{AnimationBudget, MainWindow};

use crate::assets::icons;
use crate::dashboard::profiles::DEFAULT_PROFILE;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

/// Errors that can occur while wiring VM data into Slint properties.
///
/// Variants are kept small and `Copy` so the error type does not allocate on
/// the failure path either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// `DEFAULT_PROFILE.animation_framerate_cap` does not fit in `i32` (the
    /// Slint property type for `framerate-cap`).
    FramerateCapOutOfRange,
    /// `DEFAULT_PROFILE.max_simultaneous_animations` does not fit in `i32`
    /// (the Slint property type for `max-simultaneous`).
    MaxSimultaneousOutOfRange,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::FramerateCapOutOfRange => {
                f.write_str("DEFAULT_PROFILE.animation_framerate_cap does not fit in i32")
            }
            WireError::MaxSimultaneousOutOfRange => {
                f.write_str("DEFAULT_PROFILE.max_simultaneous_animations does not fit in i32")
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
/// [`DEFAULT_PROFILE`].
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
/// `DEFAULT_PROFILE.animation_framerate_cap` does not fit in `i32`, and
/// [`WireError::MaxSimultaneousOutOfRange`] if
/// `DEFAULT_PROFILE.max_simultaneous_animations` does not fit. Both are
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

    // AnimationBudget globals — wired once at startup from DEFAULT_PROFILE.
    let budget = window.global::<AnimationBudget>();

    let cap_i32 = i32::try_from(DEFAULT_PROFILE.animation_framerate_cap)
        .map_err(|_| WireError::FramerateCapOutOfRange)?;
    let max_i32 = i32::try_from(DEFAULT_PROFILE.max_simultaneous_animations)
        .map_err(|_| WireError::MaxSimultaneousOutOfRange)?;

    budget.set_framerate_cap(cap_i32);
    budget.set_max_simultaneous(max_i32);

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::view_spec::default_dashboard;
    use crate::ha::fixture;

    /// Path to the canonical Phase 1 fixture.
    ///
    /// `cargo test` runs with the crate root as cwd so this resolves correctly.
    const FIXTURE_PATH: &str = "examples/ha-states.json";

    // -----------------------------------------------------------------------
    // Smoke test: fixture store + default_dashboard → ≥1 VM per tile kind
    // -----------------------------------------------------------------------

    #[test]
    fn smoke_build_tiles_all_three_kinds() {
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = default_dashboard();

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
        let dashboard = default_dashboard();

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
        // icon_id: no widget.icon set in default_dashboard, so default is "mdi:lightbulb".
        assert_eq!(
            light_vm.icon_id, "mdi:lightbulb",
            "default icon_id for Light"
        );
        // preferred_columns from widget.layout.
        assert_eq!(light_vm.preferred_columns, 2);
        assert_eq!(light_vm.preferred_rows, 2);
        // placement: no placement in default_dashboard so default_for(2,2).
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
        let dashboard = default_dashboard();

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
        let dashboard = default_dashboard();

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

        // default_dashboard() has widget.name = Some("Living Room") for the entity tile;
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
        use crate::dashboard::view_spec::{
            Dashboard, Layout, Section, View, Widget, WidgetKind, WidgetLayout,
        };

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        // Build a dashboard that deliberately references an entity ID not present
        // in the fixture, so we can assert the unavailable fallback independent of
        // whatever default_dashboard() points at.
        let dashboard = Dashboard {
            version: 1,
            device_profile: "rpi4".to_string(),
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
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
                        options: vec![],
                        placement: None,
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
        use crate::dashboard::view_spec::{
            Action, Dashboard, Layout, Section, View, Widget, WidgetKind, WidgetLayout,
        };

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        let dashboard = Dashboard {
            version: 1,
            device_profile: "rpi4".to_string(),
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
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
                        options: vec![],
                        placement: None,
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
    // Placement from view_spec::Placement when present
    // -----------------------------------------------------------------------

    #[test]
    fn explicit_placement_in_widget_is_used_verbatim() {
        use crate::dashboard::view_spec::{
            Dashboard, Layout, Placement, Section, View, Widget, WidgetKind, WidgetLayout,
        };

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        let dashboard = Dashboard {
            version: 1,
            device_profile: "rpi4".to_string(),
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
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
                        options: vec![],
                        placement: Some(Placement {
                            col: 3,
                            row: 1,
                            span_cols: 2,
                            span_rows: 1,
                        }),
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
        use crate::dashboard::view_spec::{
            Dashboard, Layout, Section, View, Widget, WidgetKind, WidgetLayout,
        };

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        // binary_sensor.foo has an empty attributes map (no friendly_name).
        let dashboard = Dashboard {
            version: 1,
            device_profile: "rpi4".to_string(),
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
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
                        options: vec![],
                        placement: None,
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
        use crate::dashboard::view_spec::{Dashboard, Layout, View};

        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");

        let dashboard = Dashboard {
            version: 1,
            device_profile: "rpi4".to_string(),
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

        assert!(cap.to_string().contains("framerate_cap"));
        assert!(max.to_string().contains("max_simultaneous_animations"));
    }
}
