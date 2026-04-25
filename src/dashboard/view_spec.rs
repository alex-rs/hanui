//! Typed view-spec structs for the dashboard configuration.
//!
//! These types mirror `docs/DASHBOARD_SCHEMA.md` verbatim for all user-visible
//! field names. They are constructed in Rust for Phase 1; YAML deserialization
//! is Phase 4 (out of scope here). No `serde` derives are added yet.
//!
//! Action fields (`tap_action`, `hold_action`, `double_tap_action`) are fully
//! typed but not dispatched; dispatch wiring is Phase 3.
//!
//! The computed `placement` field on `Widget` is populated by the grid packer
//! (TASK-014). It is present here as `Option<Placement>` so the bridge can
//! read it without needing a separate type.

// ---------------------------------------------------------------------------
// Action
// ---------------------------------------------------------------------------

/// A UI interaction action, as named in `docs/DASHBOARD_SCHEMA.md`.
///
/// Variants correspond 1-to-1 with the YAML `action:` strings. Phase 3 will
/// wire these to actual dispatch; for now they are typed and constructible only.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// `action: toggle` -- toggle the entity's primary state.
    Toggle,
    /// `action: more-info` -- open the entity's detail panel.
    MoreInfo,
    /// `action: none` -- suppress the default interaction.
    None,
    /// `action: navigate` -- push a named view onto the navigation stack.
    Navigate(String),
    /// `action: call-service` -- invoke a Home Assistant service.
    CallService {
        domain: String,
        service: String,
        /// Optional entity target; `None` means the service uses its own target
        /// selection (e.g. an area or label target defined in `options`).
        target: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// WidgetKind
// ---------------------------------------------------------------------------

/// The tile rendering kind, corresponding to the YAML `type:` field on a widget.
///
/// Phase 1 defines the three tile kinds present in `examples/dashboard.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidgetKind {
    /// `type: light_tile`
    LightTile,
    /// `type: sensor_tile`
    SensorTile,
    /// `type: entity_tile`
    EntityTile,
}

// ---------------------------------------------------------------------------
// Layout enum for View
// ---------------------------------------------------------------------------

/// The view-level layout strategy, corresponding to the YAML `layout:` field
/// on a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    /// `layout: sections` -- the view is divided into named sections, each
    /// containing a vertical list of widgets.
    Sections,
    /// `layout: grid` -- the view is a flat grid; section grouping is ignored.
    Grid,
}

// ---------------------------------------------------------------------------
// Placement  (internal / computed)
// ---------------------------------------------------------------------------

/// Computed grid placement assigned by the packer (TASK-014).
///
/// This is an **internal** field -- it is not part of the user-facing YAML
/// schema. The packer writes it; the Slint bridge reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidgetLayout {
    /// `preferred_columns` -- the widget's preferred column span hint.
    pub preferred_columns: u8,
    /// `preferred_rows` -- the widget's preferred row span hint.
    pub preferred_rows: u8,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeAssistant {
    /// `url` -- WebSocket API endpoint, e.g.
    /// `"ws://homeassistant.local:8123/api/websocket"`.
    pub url: String,
    /// `token_env` -- name of the environment variable carrying the HA token.
    pub token_env: String,
}

// ---------------------------------------------------------------------------
// Theme config
// ---------------------------------------------------------------------------

/// The `theme:` sub-object from the root dashboard config.
///
/// Field names mirror the YAML schema verbatim: `mode`, `accent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// `mode` -- colour scheme selector, e.g. `"dark"` or `"light"`.
    pub mode: String,
    /// `accent` -- CSS-style hex accent colour, e.g. `"#03a9f4"`.
    pub accent: String,
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
#[derive(Debug, Clone, PartialEq)]
pub struct Widget {
    /// `id` -- unique identifier for this widget within the dashboard.
    pub id: String,
    /// `type` (YAML field name) -- the tile rendering kind.
    pub widget_type: WidgetKind,
    /// `entity` -- the primary HA entity ID string (e.g. `"light.kitchen"`).
    pub entity: Option<String>,
    /// `entities` -- secondary entity IDs for multi-entity tiles.
    pub entities: Vec<String>,
    /// `name` -- optional display name override; `None` uses the entity's
    /// friendly name from HA.
    pub name: Option<String>,
    /// `icon` -- optional icon override (MDI icon slug or asset path).
    pub icon: Option<String>,
    /// `tap_action` -- action fired on a single tap.
    pub tap_action: Option<Action>,
    /// `hold_action` -- action fired on a long press.
    pub hold_action: Option<Action>,
    /// `double_tap_action` -- action fired on a double tap.
    pub double_tap_action: Option<Action>,
    /// `layout` -- user-supplied size hints.
    pub layout: WidgetLayout,
    /// `options` -- tile-kind-specific extra key/value pairs; kept as
    /// `Vec<(String, String)>` until Phase 4 provides a typed options model.
    pub options: Vec<(String, String)>,
    /// Computed grid slot assigned by the packer (TASK-014). `None` until the
    /// packer runs.
    pub placement: Option<Placement>,
}

// ---------------------------------------------------------------------------
// Section
// ---------------------------------------------------------------------------

/// A named group of widgets within a view.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// `id` -- unique section identifier within the view.
    pub id: String,
    /// `title` -- display title shown above the section.
    pub title: String,
    /// `widgets` -- ordered list of widgets in this section.
    pub widgets: Vec<Widget>,
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// A single dashboard screen / page.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// `id` -- unique view identifier referenced by `default_view`.
    pub id: String,
    /// `title` -- display title for the view tab or navigation entry.
    pub title: String,
    /// `layout` -- the layout strategy for this view.
    pub layout: Layout,
    /// `sections` -- ordered list of sections (used when `layout` is
    /// `Sections`).
    pub sections: Vec<Section>,
}

// ---------------------------------------------------------------------------
// Dashboard (top-level)
// ---------------------------------------------------------------------------

/// The top-level dashboard configuration, matching the root object in
/// `docs/DASHBOARD_SCHEMA.md`.
#[derive(Debug, Clone, PartialEq)]
pub struct Dashboard {
    /// `version` -- schema version integer.
    pub version: u32,
    /// `device_profile` -- profile key (e.g. `"rpi4"`, `"desktop"`).
    pub device_profile: String,
    /// `home_assistant` -- connection config for the HA WebSocket API.
    /// `None` when omitted (e.g. in fixture-only / offline use).
    pub home_assistant: Option<HomeAssistant>,
    /// `theme` -- colour scheme overrides. `None` applies the built-in default.
    pub theme: Option<Theme>,
    /// `default_view` -- `id` of the view shown on initial load.
    pub default_view: String,
    /// `views` -- ordered list of all views in the dashboard.
    pub views: Vec<View>,
}

// ---------------------------------------------------------------------------
// default_dashboard()
// ---------------------------------------------------------------------------

/// Returns a hand-built [`Dashboard`] covering one widget per tile kind.
///
/// This fixture mirrors the shape of `examples/dashboard.yaml` and provides a
/// known-good value for tests and dev harness use until the YAML loader
/// (Phase 4) is wired in.
///
/// Tile kinds covered: `LightTile`, `SensorTile`, `EntityTile`.
/// `home_assistant` and `theme` are `None` here; they are optional in the
/// schema and absent from `examples/dashboard.yaml`.
pub fn default_dashboard() -> Dashboard {
    Dashboard {
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
                        options: vec![],
                        placement: None,
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
                        options: vec![],
                        placement: None,
                    },
                    Widget {
                        id: "living_room_entity".to_string(),
                        widget_type: WidgetKind::EntityTile,
                        entity: Some("switch.living_room".to_string()),
                        entities: vec![],
                        name: Some("Living Room".to_string()),
                        icon: None,
                        tap_action: Some(Action::Toggle),
                        hold_action: Some(Action::MoreInfo),
                        double_tap_action: Some(Action::Navigate("home".to_string())),
                        layout: WidgetLayout {
                            preferred_columns: 2,
                            preferred_rows: 1,
                        },
                        options: vec![],
                        placement: None,
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

    #[test]
    fn default_dashboard_has_correct_version_and_profile() {
        let d = default_dashboard();
        assert_eq!(d.version, 1);
        assert_eq!(d.device_profile, "rpi4");
        assert_eq!(d.default_view, "home");
    }

    #[test]
    fn default_dashboard_contains_one_view() {
        let d = default_dashboard();
        assert_eq!(d.views.len(), 1);
        let view = &d.views[0];
        assert_eq!(view.id, "home");
        assert_eq!(view.layout, Layout::Sections);
    }

    #[test]
    fn default_dashboard_covers_all_three_tile_kinds() {
        let d = default_dashboard();
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
        let d = default_dashboard();
        let light = d.views[0].sections[0]
            .widgets
            .iter()
            .find(|w| w.widget_type == WidgetKind::LightTile)
            .expect("LightTile widget must exist in default_dashboard");

        assert_eq!(light.tap_action, Some(Action::Toggle));
        assert_eq!(light.hold_action, Some(Action::MoreInfo));
        assert_eq!(light.double_tap_action, None);
    }

    #[test]
    fn sensor_tile_has_layout_hints() {
        let d = default_dashboard();
        let sensor = d.views[0].sections[0]
            .widgets
            .iter()
            .find(|w| w.widget_type == WidgetKind::SensorTile)
            .expect("SensorTile widget must exist in default_dashboard");

        assert_eq!(sensor.layout.preferred_columns, 2);
        assert_eq!(sensor.layout.preferred_rows, 1);
    }

    #[test]
    fn entity_tile_has_double_tap_navigate() {
        let d = default_dashboard();
        let entity = d.views[0].sections[0]
            .widgets
            .iter()
            .find(|w| w.widget_type == WidgetKind::EntityTile)
            .expect("EntityTile widget must exist in default_dashboard");

        assert_eq!(
            entity.double_tap_action,
            Some(Action::Navigate("home".to_string()))
        );
    }

    #[test]
    fn widget_placement_is_none_before_packer_runs() {
        let d = default_dashboard();
        for widget in d
            .views
            .iter()
            .flat_map(|v| v.sections.iter())
            .flat_map(|s| s.widgets.iter())
        {
            assert!(
                widget.placement.is_none(),
                "placement must be None before TASK-014 packer runs (widget: {})",
                widget.id
            );
        }
    }

    #[test]
    fn placement_struct_fields_are_correct() {
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
    fn call_service_action_carries_domain_service_target() {
        let action = Action::CallService {
            domain: "light".to_string(),
            service: "turn_on".to_string(),
            target: Some("light.kitchen".to_string()),
        };
        if let Action::CallService {
            domain,
            service,
            target,
        } = &action
        {
            assert_eq!(domain, "light");
            assert_eq!(service, "turn_on");
            assert_eq!(target.as_deref(), Some("light.kitchen"));
        } else {
            panic!("expected CallService variant");
        }
    }

    #[test]
    fn action_none_variant_is_distinct_from_option_none() {
        let a: Option<Action> = Some(Action::None);
        assert!(a.is_some());
        assert_eq!(a, Some(Action::None));
    }

    #[test]
    fn dashboard_clone_is_independent() {
        let d1 = default_dashboard();
        let mut d2 = d1.clone();
        d2.device_profile = "desktop".to_string();
        assert_eq!(d1.device_profile, "rpi4");
        assert_eq!(d2.device_profile, "desktop");
    }

    #[test]
    fn home_assistant_config_fields() {
        let ha = HomeAssistant {
            url: "ws://homeassistant.local:8123/api/websocket".to_string(),
            token_env: "HA_TOKEN".to_string(),
        };
        assert_eq!(ha.url, "ws://homeassistant.local:8123/api/websocket");
        assert_eq!(ha.token_env, "HA_TOKEN");
    }

    #[test]
    fn theme_config_fields() {
        let theme = Theme {
            mode: "dark".to_string(),
            accent: "#03a9f4".to_string(),
        };
        assert_eq!(theme.mode, "dark");
        assert_eq!(theme.accent, "#03a9f4");
    }

    #[test]
    fn dashboard_with_full_config() {
        let d = Dashboard {
            version: 1,
            device_profile: "rpi4".to_string(),
            home_assistant: Some(HomeAssistant {
                url: "ws://homeassistant.local:8123/api/websocket".to_string(),
                token_env: "HA_TOKEN".to_string(),
            }),
            theme: Some(Theme {
                mode: "dark".to_string(),
                accent: "#03a9f4".to_string(),
            }),
            default_view: "home".to_string(),
            views: vec![],
        };
        assert!(d.home_assistant.is_some());
        assert!(d.theme.is_some());
        assert_eq!(d.home_assistant.as_ref().unwrap().token_env, "HA_TOKEN");
        assert_eq!(d.theme.as_ref().unwrap().mode, "dark");
    }
}
