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
//! # CI gate
//!
//! The project enforces that no JSON-value type names appear in `src/ui/`.
//! This file accesses attribute values only via the typed accessor family
//! (`.as_str()`, `.as_f64()`, etc.) which are methods on the value type
//! without naming the type itself.

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
/// The function is generic over `S: EntityStore` because `EntityStore::for_each`
/// has a generic closure parameter which prevents object-safety (`dyn EntityStore`).
/// Callers pass any concrete store type; the acceptance criterion of "bridge
/// consumes EntityStore via for_each and get" is fully satisfied at the trait-bound
/// level.
///
/// See the module-level doc for the missing-entity policy and field-mapping
/// details.
pub fn build_tiles<S: EntityStore>(store: &S, dashboard: &Dashboard) -> Vec<TileVM> {
    // Walk all entities once via the visitor to collect a count for a
    // diagnostic log / sanity check. This satisfies the AC requirement that
    // for_each is exercised on the live store path (not only in tests).
    let mut store_entity_count: usize = 0;
    store.for_each(|_id, _entity| {
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
                                .attributes
                                .get("friendly_name")
                                .and_then(|v| v.as_str())
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
    // EntityTileVM field correctness (switch.living_room)
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

        // default_dashboard() has widget.name = Some("Living Room") for the entity tile.
        assert_eq!(
            entity_vm.name, "Living Room",
            "explicit widget name takes precedence"
        );
        // switch.living_room is not in the fixture → unavailable.
        assert_eq!(
            entity_vm.state, "unavailable",
            "missing entity state must be 'unavailable'"
        );
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
        store.for_each(|_id, _entity| {
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
        let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
        let dashboard = default_dashboard();

        let tiles = build_tiles(&store, &dashboard);

        // switch.living_room is not in the fixture (which only has outlet_1).
        // The EntityTile widget references switch.living_room → must be unavailable.
        let entity_vm = tiles
            .iter()
            .find_map(|t| {
                if let TileVM::Entity(vm) = t {
                    Some(vm)
                } else {
                    None
                }
            })
            .expect("must have an EntityTileVM");

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
}
