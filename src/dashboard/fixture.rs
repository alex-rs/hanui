//! Fixture dashboard for the `--fixture` development path.
//!
//! `fixture_dashboard()` returns a hand-built [`Dashboard`] that mirrors the
//! shape of `examples/ha-states.json` and provides a known-good value for tests
//! and the `--fixture` dev harness. The YAML loader (`loader::load`) is used
//! instead on the production / live-HA path.
//!
//! # Why a separate module
//!
//! `src/dashboard/view_spec.rs` previously housed both the schema types and
//! this fixture helper. Per `locked_decisions.view_spec_disposition`, Phase 4
//! promotes the types to `src/dashboard/schema.rs` (with full serde support)
//! and moves this helper to `src/dashboard/fixture.rs`. The split keeps the
//! production type module free of test-only construction helpers.
//!
//! Tile kinds covered: `LightTile`, `SensorTile`, `EntityTile`.
//! `home_assistant` and `theme` are `None` — they are optional in the schema
//! and absent from the fixture use case.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::actions::Action;
use crate::dashboard::schema::{
    Dashboard, Layout, ProfileKey, Section, SectionGrid, View, Widget, WidgetKind, WidgetLayout,
};

const FIXTURE_VISIBILITY: &str = "always";

/// Build a hand-constructed [`Dashboard`] suitable for the `--fixture` path.
///
/// Covers one widget of each tile kind present in `examples/ha-states.json`:
/// `LightTile` (light.kitchen), `SensorTile` (sensor.hallway_temperature),
/// `EntityTile` (switch.outlet_1).
///
/// This function is called only from `src/lib.rs`'s `run_with_memory_store`
/// path (i.e. `--fixture <file>`) and from unit tests. The production
/// live-HA path calls `loader::load(...)` instead.
pub fn fixture_dashboard() -> Dashboard {
    Dashboard {
        call_service_allowlist: Arc::new(BTreeSet::new()),
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
                grid: SectionGrid::default(),
                id: "overview".to_string(),
                title: "Overview".to_string(),
                widgets: vec![
                    Widget {
                        id: "kitchen_light".to_string(),
                        widget_type: WidgetKind::LightTile,
                        entity: Some("light.kitchen".to_string()),
                        entities: vec![],
                        name: None,
                        icon: None,
                        tap_action: Some(Action::Toggle),
                        hold_action: Some(Action::MoreInfo),
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 2,
                            preferred_rows: 2,
                        },
                        options: None,
                        placement: None,
                        visibility: FIXTURE_VISIBILITY.to_string(),
                    },
                    Widget {
                        id: "hallway_temperature".to_string(),
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
                        placement: None,
                        visibility: FIXTURE_VISIBILITY.to_string(),
                    },
                    Widget {
                        id: "living_room_entity".to_string(),
                        widget_type: WidgetKind::EntityTile,
                        entity: Some("switch.outlet_1".to_string()),
                        entities: vec![],
                        name: Some("Living Room".to_string()),
                        icon: None,
                        tap_action: Some(Action::Toggle),
                        hold_action: Some(Action::MoreInfo),
                        double_tap_action: Some(Action::Navigate {
                            view_id: "home".to_string(),
                        }),
                        layout: WidgetLayout {
                            preferred_columns: 2,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: None,
                        visibility: FIXTURE_VISIBILITY.to_string(),
                    },
                ],
            }],
        }],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Action;

    #[test]
    fn fixture_dashboard_has_correct_version_and_profile() {
        let d = fixture_dashboard();
        assert_eq!(d.version, 1);
        assert_eq!(d.device_profile, ProfileKey::Rpi4);
        assert_eq!(d.default_view, "home");
    }

    #[test]
    fn fixture_dashboard_contains_one_view() {
        let d = fixture_dashboard();
        assert_eq!(d.views.len(), 1);
        let view = &d.views[0];
        assert_eq!(view.id, "home");
        assert_eq!(view.layout, Layout::Sections);
    }

    #[test]
    fn fixture_dashboard_covers_all_three_tile_kinds() {
        let d = fixture_dashboard();
        let widgets: Vec<&Widget> = d
            .views
            .iter()
            .flat_map(|v| v.sections.iter())
            .flat_map(|s| s.widgets.iter())
            .collect();

        let kinds: Vec<&WidgetKind> = widgets.iter().map(|w| &w.widget_type).collect();
        assert!(kinds.contains(&&WidgetKind::LightTile), "missing LightTile");
        assert!(
            kinds.contains(&&WidgetKind::SensorTile),
            "missing SensorTile"
        );
        assert!(
            kinds.contains(&&WidgetKind::EntityTile),
            "missing EntityTile"
        );
    }

    #[test]
    fn light_tile_has_tap_and_hold_actions() {
        let d = fixture_dashboard();
        let light = d.views[0].sections[0]
            .widgets
            .iter()
            .find(|w| w.widget_type == WidgetKind::LightTile)
            .expect("LightTile widget must exist in fixture_dashboard");

        assert_eq!(light.tap_action, Some(Action::Toggle));
        assert_eq!(light.hold_action, Some(Action::MoreInfo));
        assert_eq!(light.double_tap_action, None);
    }

    #[test]
    fn sensor_tile_has_layout_hints() {
        let d = fixture_dashboard();
        let sensor = d.views[0].sections[0]
            .widgets
            .iter()
            .find(|w| w.widget_type == WidgetKind::SensorTile)
            .expect("SensorTile widget must exist in fixture_dashboard");

        assert_eq!(sensor.layout.preferred_columns, 2);
        assert_eq!(sensor.layout.preferred_rows, 1);
    }

    #[test]
    fn entity_tile_has_double_tap_navigate() {
        let d = fixture_dashboard();
        let entity = d.views[0].sections[0]
            .widgets
            .iter()
            .find(|w| w.widget_type == WidgetKind::EntityTile)
            .expect("EntityTile widget must exist in fixture_dashboard");

        assert_eq!(
            entity.double_tap_action,
            Some(Action::Navigate {
                view_id: "home".to_string()
            })
        );
    }

    #[test]
    fn widget_placement_is_none_before_packer_runs() {
        let d = fixture_dashboard();
        for widget in d
            .views
            .iter()
            .flat_map(|v| v.sections.iter())
            .flat_map(|s| s.widgets.iter())
        {
            assert!(
                widget.placement.is_none(),
                "placement must be None before packer runs (widget: {})",
                widget.id
            );
        }
    }

    #[test]
    fn fixture_dashboard_clone_is_independent() {
        let d1 = fixture_dashboard();
        let mut d2 = d1.clone();
        d2.version = 2;
        assert_eq!(d1.version, 1);
        assert_eq!(d2.version, 2);
    }
}
