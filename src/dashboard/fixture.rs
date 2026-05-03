//! Fixture dashboard for the `--fixture` development path.
//!
//! [`fixture_dashboard`] returns a hand-built [`Dashboard`] that mirrors the
//! shape of `examples/ha-states.json` and provides a known-good value for
//! tests and the `--fixture` dev harness. The YAML loader (`loader::load`) is
//! used instead on the production / live-HA path.
//!
//! # Why a separate module
//!
//! `src/dashboard/view_spec.rs` previously housed both the schema types and
//! this fixture helper. Per `locked_decisions.view_spec_disposition`, Phase 4
//! promotes the types to `src/dashboard/schema.rs` (with full serde support)
//! and moves this helper to `src/dashboard/fixture.rs`. The split keeps the
//! production type module free of test-only construction helpers.
//!
//! # Phase 6 widget coverage
//!
//! Originally the fixture exposed only the three Phase 1 tile kinds (Light,
//! Sensor, Entity). Running `target/release/hanui --fixture
//! examples/ha-states.json` therefore visually rendered exactly three widgets,
//! which made the no-HA smoke path useless for previewing Phase 6 widgets.
//!
//! As of `task/fixture-phase6-widgets-showcase`, the fixture mirrors
//! `examples/dashboard.yaml` and exercises one (or more) widget per
//! `WidgetKind` variant: `LightTile`, `SensorTile`, `EntityTile`, `Cover`,
//! `Fan`, `Lock` (both `PinPolicy::Required` and `PinPolicy::None`), `Alarm`,
//! `History`, `Camera`, `Climate`, `MediaPlayer`, and `PowerFlow`. The three
//! original Phase 1 widgets (`kitchen_light`, `hallway_temperature`,
//! `living_room_entity`) are preserved verbatim so existing callers in
//! `src/actions/map.rs` and `src/ui/bridge.rs` that look those widget IDs up
//! by name continue to pass.
//!
//! `home_assistant` and `theme` remain `None` — they are optional in the
//! schema and absent from the fixture use case.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::actions::Action;
use crate::dashboard::schema::{
    CodeFormat, Dashboard, Layout, MediaTransport, PinPolicy, ProfileKey, Section, SectionGrid,
    View, Widget, WidgetKind, WidgetLayout, WidgetOptions,
};

const FIXTURE_VISIBILITY: &str = "always";

/// Default `SectionGrid` used for every fixture section: 4-column, 8 px gap.
///
/// Mirrors `examples/dashboard.yaml`. Local helper rather than
/// `SectionGrid::default()` (which produces the same value) so the fixture's
/// intent — "match the YAML reference exactly" — is explicit at the call site.
fn fixture_grid() -> SectionGrid {
    SectionGrid { columns: 4, gap: 8 }
}

/// Returns the canonical Phase 1 "overview" section — the three widgets that
/// MUST remain stable so that
/// `src/actions/map.rs::from_dashboard_populates_kitchen_light_with_toggle_and_more_info`
/// and friends keep matching by widget ID.
fn overview_section() -> Section {
    Section {
        grid: fixture_grid(),
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
    }
}

/// Climate / camera / history / fan section. Mirrors the `climate` section of
/// `examples/dashboard.yaml`. Covers `Fan`, `Camera`, `History`, `Climate`.
fn climate_section() -> Section {
    Section {
        grid: fixture_grid(),
        id: "climate".to_string(),
        title: "Climate".to_string(),
        widgets: vec![
            Widget {
                id: "living_room_fan".to_string(),
                widget_type: WidgetKind::Fan,
                entity: Some("fan.living_room".to_string()),
                entities: vec![],
                name: Some("Living Room Fan".to_string()),
                icon: Some("mdi:fan".to_string()),
                tap_action: Some(Action::Toggle),
                hold_action: Some(Action::MoreInfo),
                double_tap_action: None,
                layout: WidgetLayout {
                    preferred_columns: 2,
                    preferred_rows: 2,
                },
                options: Some(WidgetOptions::Fan {
                    speed_count: 3,
                    preset_modes: vec!["low".to_string(), "medium".to_string(), "high".to_string()],
                }),
                placement: None,
                visibility: FIXTURE_VISIBILITY.to_string(),
            },
            Widget {
                id: "bedroom_camera".to_string(),
                widget_type: WidgetKind::Camera,
                entity: Some("camera.bedroom".to_string()),
                entities: vec![],
                name: Some("Bedroom Camera".to_string()),
                icon: Some("mdi:camera".to_string()),
                tap_action: Some(Action::MoreInfo),
                hold_action: None,
                double_tap_action: None,
                layout: WidgetLayout {
                    preferred_columns: 2,
                    preferred_rows: 2,
                },
                options: Some(WidgetOptions::Camera {
                    interval_seconds: 10,
                    url: "http://homeassistant.local:8123/api/camera_proxy/camera.bedroom"
                        .to_string(),
                }),
                placement: None,
                visibility: FIXTURE_VISIBILITY.to_string(),
            },
            Widget {
                id: "energy_history".to_string(),
                widget_type: WidgetKind::History,
                entity: Some("sensor.energy_consumption".to_string()),
                entities: vec![],
                name: Some("Energy (24h)".to_string()),
                icon: Some("mdi:lightning-bolt".to_string()),
                tap_action: Some(Action::MoreInfo),
                hold_action: None,
                double_tap_action: None,
                layout: WidgetLayout {
                    preferred_columns: 4,
                    preferred_rows: 2,
                },
                options: Some(WidgetOptions::History {
                    window_seconds: 86_400,
                    max_points: 120,
                }),
                placement: None,
                visibility: FIXTURE_VISIBILITY.to_string(),
            },
            Widget {
                id: "living_room_thermostat".to_string(),
                widget_type: WidgetKind::Climate,
                entity: Some("climate.living_room".to_string()),
                entities: vec![],
                name: Some("Living Room".to_string()),
                icon: Some("mdi:thermostat".to_string()),
                tap_action: Some(Action::MoreInfo),
                hold_action: None,
                double_tap_action: None,
                layout: WidgetLayout {
                    preferred_columns: 2,
                    preferred_rows: 2,
                },
                options: Some(WidgetOptions::Climate {
                    min_temp: 16.0,
                    max_temp: 30.0,
                    step: 0.5,
                    hvac_modes: vec![
                        "heat".to_string(),
                        "cool".to_string(),
                        "heat_cool".to_string(),
                        "off".to_string(),
                    ],
                }),
                placement: None,
                visibility: FIXTURE_VISIBILITY.to_string(),
            },
        ],
    }
}

/// Access / cover / lock section. Mirrors the `access` section of
/// `examples/dashboard.yaml`. Covers `Lock` (with `PinPolicy::Required`),
/// `Lock` (with `PinPolicy::None`), and `Cover`.
fn access_section() -> Section {
    Section {
        grid: fixture_grid(),
        id: "access".to_string(),
        title: "Access Control".to_string(),
        widgets: vec![
            Widget {
                id: "front_door_lock".to_string(),
                widget_type: WidgetKind::Lock,
                entity: Some("lock.front_door".to_string()),
                entities: vec![],
                name: Some("Front Door".to_string()),
                icon: Some("mdi:door-closed-lock".to_string()),
                tap_action: Some(Action::MoreInfo),
                hold_action: Some(Action::MoreInfo),
                double_tap_action: None,
                layout: WidgetLayout {
                    preferred_columns: 2,
                    preferred_rows: 1,
                },
                options: Some(WidgetOptions::Lock {
                    pin_policy: PinPolicy::Required {
                        length: 4,
                        code_format: CodeFormat::Number,
                    },
                    require_confirmation_on_unlock: true,
                }),
                placement: None,
                visibility: FIXTURE_VISIBILITY.to_string(),
            },
            Widget {
                id: "garage_door_lock".to_string(),
                widget_type: WidgetKind::Lock,
                entity: Some("lock.garage_door".to_string()),
                entities: vec![],
                name: Some("Garage Door".to_string()),
                icon: Some("mdi:garage".to_string()),
                tap_action: Some(Action::MoreInfo),
                hold_action: Some(Action::MoreInfo),
                double_tap_action: None,
                layout: WidgetLayout {
                    preferred_columns: 2,
                    preferred_rows: 1,
                },
                options: Some(WidgetOptions::Lock {
                    pin_policy: PinPolicy::None,
                    require_confirmation_on_unlock: false,
                }),
                placement: None,
                visibility: FIXTURE_VISIBILITY.to_string(),
            },
            Widget {
                id: "patio_cover".to_string(),
                widget_type: WidgetKind::Cover,
                entity: Some("cover.patio".to_string()),
                entities: vec![],
                name: Some("Patio Cover".to_string()),
                icon: Some("mdi:window-shutter".to_string()),
                tap_action: Some(Action::MoreInfo),
                hold_action: None,
                double_tap_action: None,
                layout: WidgetLayout {
                    preferred_columns: 2,
                    preferred_rows: 1,
                },
                options: Some(WidgetOptions::Cover {
                    position_min: 0,
                    position_max: 100,
                }),
                placement: None,
                visibility: FIXTURE_VISIBILITY.to_string(),
            },
        ],
    }
}

/// Alarm section. Mirrors the `alarm` section of `examples/dashboard.yaml`.
/// Covers `Alarm` with `PinPolicy::RequiredOnDisarm`.
fn alarm_section() -> Section {
    Section {
        grid: fixture_grid(),
        id: "alarm".to_string(),
        title: "Alarm".to_string(),
        widgets: vec![Widget {
            id: "home_alarm".to_string(),
            widget_type: WidgetKind::Alarm,
            entity: Some("alarm_control_panel.home".to_string()),
            entities: vec![],
            name: Some("Home Alarm".to_string()),
            icon: Some("mdi:shield-home".to_string()),
            tap_action: Some(Action::MoreInfo),
            hold_action: Some(Action::MoreInfo),
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 4,
                preferred_rows: 1,
            },
            options: Some(WidgetOptions::Alarm {
                pin_policy: PinPolicy::RequiredOnDisarm {
                    length: 4,
                    code_format: CodeFormat::Number,
                },
            }),
            placement: None,
            visibility: FIXTURE_VISIBILITY.to_string(),
        }],
    }
}

/// Media-player section. Covers `MediaPlayer`.
fn media_section() -> Section {
    Section {
        grid: fixture_grid(),
        id: "media".to_string(),
        title: "Media".to_string(),
        widgets: vec![Widget {
            id: "living_room_tv".to_string(),
            widget_type: WidgetKind::MediaPlayer,
            entity: Some("media_player.living_room_tv".to_string()),
            entities: vec![],
            name: Some("Living Room TV".to_string()),
            icon: Some("mdi:television-play".to_string()),
            tap_action: Some(Action::MoreInfo),
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 4,
                preferred_rows: 2,
            },
            options: Some(WidgetOptions::MediaPlayer {
                transport_set: vec![
                    MediaTransport::Play,
                    MediaTransport::Pause,
                    MediaTransport::Stop,
                    MediaTransport::Next,
                    MediaTransport::Prev,
                ],
                volume_step: 0.05,
            }),
            placement: None,
            visibility: FIXTURE_VISIBILITY.to_string(),
        }],
    }
}

/// Energy / power-flow section. Covers `PowerFlow`. Note that PowerFlow uses
/// the multi-entity `WidgetOptions::PowerFlow` payload rather than the
/// single-entity `widget.entity` field.
fn energy_section() -> Section {
    Section {
        grid: fixture_grid(),
        id: "power_overview".to_string(),
        title: "Power Overview".to_string(),
        widgets: vec![Widget {
            id: "home_power_flow".to_string(),
            widget_type: WidgetKind::PowerFlow,
            entity: None,
            entities: vec![],
            name: Some("Power Flow".to_string()),
            icon: Some("mdi:lightning-bolt-circle".to_string()),
            tap_action: Some(Action::MoreInfo),
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 4,
                preferred_rows: 3,
            },
            options: Some(WidgetOptions::PowerFlow {
                grid_entity: "sensor.grid_power".to_string(),
                solar_entity: Some("sensor.solar_power".to_string()),
                battery_entity: Some("sensor.battery_power".to_string()),
                battery_soc_entity: Some("sensor.battery_soc".to_string()),
                home_entity: Some("sensor.home_power".to_string()),
            }),
            placement: None,
            visibility: FIXTURE_VISIBILITY.to_string(),
        }],
    }
}

/// Build a hand-constructed [`Dashboard`] suitable for the `--fixture` path.
///
/// Produces four Views — `home`, `security`, `media`, `energy` — that mirror
/// the structure of `examples/dashboard.yaml` so the no-HA smoke run
/// exercises the ViewSwitcher tab strip in addition to all Phase 6 widget
/// kinds. Covers every Phase 6 [`WidgetKind`] variant; see the module-level
/// docs for the full list.
///
/// History: prior to the May 2026 founder smoke run this fixture collapsed
/// every section into a single `home` view. The single-view shape silently
/// hid the ViewSwitcher (which auto-suppresses for `views.length < 2`),
/// making it impossible to preview view-switching without a live HA
/// backend. The multi-view layout below restores parity with
/// `examples/dashboard.yaml` so visual smoke covers the tab-strip path.
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
        views: vec![
            // Home view — Phase 1 overview tiles + climate widgets.
            View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![overview_section(), climate_section()],
            },
            // Security view — locks, cover, alarm.
            View {
                id: "security".to_string(),
                title: "Security".to_string(),
                layout: Layout::Sections,
                sections: vec![access_section(), alarm_section()],
            },
            // Media view — TV / media-player tile.
            View {
                id: "media".to_string(),
                title: "Media".to_string(),
                layout: Layout::Sections,
                sections: vec![media_section()],
            },
            // Energy view — power-flow tile.
            View {
                id: "energy".to_string(),
                title: "Energy".to_string(),
                layout: Layout::Sections,
                sections: vec![energy_section()],
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Action;

    /// Walk every widget in the fixture in a single linear pass.
    fn all_widgets(d: &Dashboard) -> Vec<&Widget> {
        d.views
            .iter()
            .flat_map(|v| v.sections.iter())
            .flat_map(|s| s.widgets.iter())
            .collect()
    }

    #[test]
    fn fixture_dashboard_has_correct_version_and_profile() {
        let d = fixture_dashboard();
        assert_eq!(d.version, 1);
        assert_eq!(d.device_profile, ProfileKey::Rpi4);
        assert_eq!(d.default_view, "home");
    }

    #[test]
    fn fixture_dashboard_contains_four_views() {
        // Mirrors `examples/dashboard.yaml`: home / security / media /
        // energy. Multi-view shape exercises the ViewSwitcher tab strip
        // in the `--fixture` smoke run, which auto-suppresses on
        // single-view dashboards.
        let d = fixture_dashboard();
        assert_eq!(d.views.len(), 4);
        let ids: Vec<&str> = d.views.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, vec!["home", "security", "media", "energy"]);
        let titles: Vec<&str> = d.views.iter().map(|v| v.title.as_str()).collect();
        assert_eq!(titles, vec!["Home", "Security", "Media", "Energy"]);
        for view in &d.views {
            assert_eq!(
                view.layout,
                Layout::Sections,
                "every fixture view must use Layout::Sections"
            );
        }
    }

    #[test]
    fn fixture_dashboard_has_multiple_sections() {
        // Phase 6 widget showcase requires multiple sections grouped by
        // function (overview, climate, access, alarm, media, energy)
        // distributed across four views.
        let d = fixture_dashboard();
        let total_sections: usize = d.views.iter().map(|v| v.sections.len()).sum();
        assert_eq!(
            total_sections, 6,
            "fixture must have 6 sections total to group all Phase 6 widget kinds"
        );
    }

    #[test]
    fn fixture_dashboard_default_view_resolves_to_a_real_view() {
        // The default-view id must point at a real view; otherwise the
        // ViewSwitcher cannot select an active tab at startup.
        let d = fixture_dashboard();
        assert!(
            d.views.iter().any(|v| v.id == d.default_view),
            "default_view {:?} must match an existing view id",
            d.default_view
        );
    }

    #[test]
    fn fixture_dashboard_view_section_ids_match_yaml_grouping() {
        // Sanity-check the founder-visible grouping: home owns overview +
        // climate; security owns access + alarm; media owns media; energy
        // owns power_overview. Mirrors `examples/dashboard.yaml`.
        let d = fixture_dashboard();
        let by_view: Vec<Vec<&str>> = d
            .views
            .iter()
            .map(|v| v.sections.iter().map(|s| s.id.as_str()).collect())
            .collect();
        assert_eq!(by_view[0], vec!["overview", "climate"]);
        assert_eq!(by_view[1], vec!["access", "alarm"]);
        assert_eq!(by_view[2], vec!["media"]);
        assert_eq!(by_view[3], vec!["power_overview"]);
    }

    #[test]
    fn fixture_dashboard_covers_every_phase6_widget_kind() {
        let d = fixture_dashboard();
        let widgets = all_widgets(&d);
        let kinds: Vec<&WidgetKind> = widgets.iter().map(|w| &w.widget_type).collect();

        // Phase 1 kinds (must remain).
        assert!(kinds.contains(&&WidgetKind::LightTile), "missing LightTile");
        assert!(
            kinds.contains(&&WidgetKind::SensorTile),
            "missing SensorTile"
        );
        assert!(
            kinds.contains(&&WidgetKind::EntityTile),
            "missing EntityTile"
        );

        // Phase 6 kinds (newly required).
        assert!(kinds.contains(&&WidgetKind::Cover), "missing Cover");
        assert!(kinds.contains(&&WidgetKind::Fan), "missing Fan");
        assert!(kinds.contains(&&WidgetKind::Lock), "missing Lock");
        assert!(kinds.contains(&&WidgetKind::Alarm), "missing Alarm");
        assert!(kinds.contains(&&WidgetKind::History), "missing History");
        assert!(kinds.contains(&&WidgetKind::Camera), "missing Camera");
        assert!(kinds.contains(&&WidgetKind::Climate), "missing Climate");
        assert!(
            kinds.contains(&&WidgetKind::MediaPlayer),
            "missing MediaPlayer"
        );
        assert!(kinds.contains(&&WidgetKind::PowerFlow), "missing PowerFlow");
    }

    #[test]
    fn fixture_dashboard_widget_count_is_at_least_twelve() {
        // Founder-visible regression: the no-HA smoke run advertises
        // `tile_count` in its log, and the previous 3-widget fixture made
        // Phase 6 widgets invisible from `--fixture`. Count guards future
        // accidental shrinkage of the fixture.
        //
        // Length is bound to a local so coverage attributes the call to a
        // line that always executes (vs. the assert-message format args,
        // which only evaluate on assertion failure and would leave the call
        // line marked uncovered).
        let d = fixture_dashboard();
        let widget_count = all_widgets(&d).len();
        assert!(
            widget_count >= 12,
            "fixture must showcase at least 12 widgets (saw {widget_count})"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 1 widgets must remain stable: action-map tests and bridge tests
    // look these up by ID.
    // -----------------------------------------------------------------------

    #[test]
    fn light_tile_has_tap_and_hold_actions() {
        let d = fixture_dashboard();
        let light = all_widgets(&d)
            .into_iter()
            .find(|w| w.widget_type == WidgetKind::LightTile)
            .expect("LightTile widget must exist in fixture_dashboard");

        assert_eq!(light.tap_action, Some(Action::Toggle));
        assert_eq!(light.hold_action, Some(Action::MoreInfo));
        assert_eq!(light.double_tap_action, None);
    }

    #[test]
    fn sensor_tile_has_layout_hints() {
        let d = fixture_dashboard();
        let sensor = all_widgets(&d)
            .into_iter()
            .find(|w| w.widget_type == WidgetKind::SensorTile)
            .expect("SensorTile widget must exist in fixture_dashboard");

        assert_eq!(sensor.layout.preferred_columns, 2);
        assert_eq!(sensor.layout.preferred_rows, 1);
    }

    #[test]
    fn entity_tile_has_double_tap_navigate() {
        let d = fixture_dashboard();
        let entity = all_widgets(&d)
            .into_iter()
            .find(|w| w.id == "living_room_entity")
            .expect("living_room_entity widget must exist in fixture_dashboard");

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
        for widget in all_widgets(&d) {
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

    // -----------------------------------------------------------------------
    // Per-kind options payload assertions
    // -----------------------------------------------------------------------

    fn find_widget<'a>(d: &'a Dashboard, id: &str) -> &'a Widget {
        all_widgets(d)
            .into_iter()
            .find(|w| w.id == id)
            .unwrap_or_else(|| panic!("widget {id} must exist in fixture_dashboard"))
    }

    /// Covers the `find_widget` `unwrap_or_else` closure (the panic path is
    /// otherwise unreachable because all other tests pass valid widget ids).
    #[test]
    #[should_panic(expected = "widget no_such_widget must exist in fixture_dashboard")]
    fn find_widget_panics_when_id_is_missing() {
        let d = fixture_dashboard();
        let _ = find_widget(&d, "no_such_widget");
    }

    #[test]
    fn cover_widget_carries_cover_options() {
        let d = fixture_dashboard();
        let w = find_widget(&d, "patio_cover");
        assert_eq!(w.widget_type, WidgetKind::Cover);
        // Whole-options equality (PartialEq is derived). Avoids `if let`
        // destructuring whose implicit else-branch would leave the closing
        // brace marked as an uncovered region.
        assert_eq!(
            w.options,
            Some(WidgetOptions::Cover {
                position_min: 0,
                position_max: 100,
            })
        );
    }

    #[test]
    fn fan_widget_carries_fan_options() {
        let d = fixture_dashboard();
        let w = find_widget(&d, "living_room_fan");
        assert_eq!(w.widget_type, WidgetKind::Fan);
        assert_eq!(
            w.options,
            Some(WidgetOptions::Fan {
                speed_count: 3,
                preset_modes: vec!["low".to_string(), "medium".to_string(), "high".to_string(),],
            })
        );
    }

    #[test]
    fn lock_required_widget_carries_required_pin_policy() {
        let d = fixture_dashboard();
        let w = find_widget(&d, "front_door_lock");
        assert_eq!(w.widget_type, WidgetKind::Lock);
        assert_eq!(
            w.options,
            Some(WidgetOptions::Lock {
                pin_policy: PinPolicy::Required {
                    length: 4,
                    code_format: CodeFormat::Number,
                },
                require_confirmation_on_unlock: true,
            })
        );
    }

    #[test]
    fn lock_none_widget_carries_none_pin_policy() {
        let d = fixture_dashboard();
        let w = find_widget(&d, "garage_door_lock");
        assert_eq!(w.widget_type, WidgetKind::Lock);
        assert_eq!(
            w.options,
            Some(WidgetOptions::Lock {
                pin_policy: PinPolicy::None,
                require_confirmation_on_unlock: false,
            })
        );
    }

    #[test]
    fn alarm_widget_carries_required_on_disarm_pin_policy() {
        let d = fixture_dashboard();
        let w = find_widget(&d, "home_alarm");
        assert_eq!(w.widget_type, WidgetKind::Alarm);
        assert_eq!(
            w.options,
            Some(WidgetOptions::Alarm {
                pin_policy: PinPolicy::RequiredOnDisarm {
                    length: 4,
                    code_format: CodeFormat::Number,
                },
            })
        );
    }

    #[test]
    fn history_widget_carries_history_options() {
        let d = fixture_dashboard();
        let w = find_widget(&d, "energy_history");
        assert_eq!(w.widget_type, WidgetKind::History);
        assert_eq!(
            w.options,
            Some(WidgetOptions::History {
                window_seconds: 86_400,
                max_points: 120,
            })
        );
    }

    #[test]
    fn camera_widget_carries_camera_options() {
        let d = fixture_dashboard();
        let w = find_widget(&d, "bedroom_camera");
        assert_eq!(w.widget_type, WidgetKind::Camera);
        assert_eq!(
            w.options,
            Some(WidgetOptions::Camera {
                interval_seconds: 10,
                url: "http://homeassistant.local:8123/api/camera_proxy/camera.bedroom".to_string(),
            })
        );
    }

    #[test]
    fn climate_widget_carries_climate_options() {
        let d = fixture_dashboard();
        let w = find_widget(&d, "living_room_thermostat");
        assert_eq!(w.widget_type, WidgetKind::Climate);
        assert_eq!(
            w.options,
            Some(WidgetOptions::Climate {
                min_temp: 16.0,
                max_temp: 30.0,
                step: 0.5,
                hvac_modes: vec![
                    "heat".to_string(),
                    "cool".to_string(),
                    "heat_cool".to_string(),
                    "off".to_string(),
                ],
            })
        );
    }

    #[test]
    fn media_player_widget_carries_media_player_options() {
        let d = fixture_dashboard();
        let w = find_widget(&d, "living_room_tv");
        assert_eq!(w.widget_type, WidgetKind::MediaPlayer);
        assert_eq!(
            w.options,
            Some(WidgetOptions::MediaPlayer {
                transport_set: vec![
                    MediaTransport::Play,
                    MediaTransport::Pause,
                    MediaTransport::Stop,
                    MediaTransport::Next,
                    MediaTransport::Prev,
                ],
                volume_step: 0.05,
            })
        );
    }

    #[test]
    fn power_flow_widget_carries_power_flow_options() {
        let d = fixture_dashboard();
        let w = find_widget(&d, "home_power_flow");
        assert_eq!(w.widget_type, WidgetKind::PowerFlow);
        // PowerFlow uses the multi-entity `WidgetOptions::PowerFlow` payload,
        // not the single-entity `widget.entity` field.
        assert!(w.entity.is_none());
        assert_eq!(
            w.options,
            Some(WidgetOptions::PowerFlow {
                grid_entity: "sensor.grid_power".to_string(),
                solar_entity: Some("sensor.solar_power".to_string()),
                battery_entity: Some("sensor.battery_power".to_string()),
                battery_soc_entity: Some("sensor.battery_soc".to_string()),
                home_entity: Some("sensor.home_power".to_string()),
            })
        );
    }

    // -----------------------------------------------------------------------
    // Every widget visibility defaults to "always" — the fixture intentionally
    // does NOT exercise the predicate evaluator.
    // -----------------------------------------------------------------------

    #[test]
    fn every_fixture_widget_visibility_is_always() {
        let d = fixture_dashboard();
        for widget in all_widgets(&d) {
            assert_eq!(
                widget.visibility, FIXTURE_VISIBILITY,
                "fixture widget {} must have visibility=\"always\"",
                widget.id
            );
        }
    }
}
