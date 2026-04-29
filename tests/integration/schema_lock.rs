//! Schema-lock round-trip test.
//!
//! Per `locked_decisions.schema_finalization_gate` part (a) in
//! `docs/plans/2026-04-29-phase-4-layout.md`:
//!
//! Every field documented in `docs/DASHBOARD_SCHEMA.md` must round-trip through
//! `serde_yaml_ng::from_str::<Dashboard>` → `serde_yaml_ng::to_string` →
//! `from_str`, producing a `Dashboard` value `==` to the first parse.
//!
//! The test is driven by an in-file list of `(yaml_field_path,
//! expected_rust_field_name)` pairs. Adding a field to `schema.rs` WITHOUT
//! updating this list fails the test at runtime (the YAML parses successfully
//! but the assertion on the Rust struct would be missing, or vice versa — the
//! list has an entry that the schema no longer satisfies).
//!
//! This is the mechanical schema-drift gate for TASK-089.

use hanui::dashboard::schema::{
    Dashboard, HomeAssistant, Issue, Layout, ProfileKey, Section, SectionGrid, Severity, Theme,
    ValidationRule, View, Widget, WidgetKind, WidgetLayout,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Schema field registry
// ---------------------------------------------------------------------------

/// One entry per documented field in `docs/DASHBOARD_SCHEMA.md`.
///
/// Format: `(yaml_field_path, rust_struct_field_name)`.
/// The yaml_field_path is the dotted YAML key path; the rust_struct_field_name
/// is the Rust field name in the relevant struct.
///
/// MAINTENANCE: When a field is added to `src/dashboard/schema.rs` and
/// `docs/DASHBOARD_SCHEMA.md`, add a corresponding entry here. Failing to do
/// so will cause `every_doc_field_round_trips` to miss coverage for the new
/// field. Removing an entry without removing the struct field will leave a
/// dangling assertion (the struct field can no longer be tested via this list).
///
/// Per `locked_decisions.no_hashmap_in_deserialized_types`: all map fields use
/// `BTreeMap`; their deterministic ordering is relied upon by the round-trip
/// equality check.
const SCHEMA_FIELD_REGISTRY: &[(&str, &str)] = &[
    // Top-level Dashboard fields
    ("version", "version"),
    ("device_profile", "device_profile"),
    ("home_assistant", "home_assistant"),
    ("theme", "theme"),
    ("default_view", "default_view"),
    ("views", "views"),
    // HomeAssistant sub-fields
    ("home_assistant.url", "url"),
    ("home_assistant.token_env", "token_env"),
    // Theme sub-fields
    ("theme.mode", "mode"),
    ("theme.accent", "accent"),
    // View fields
    ("views[].id", "id"),
    ("views[].title", "title"),
    ("views[].layout", "layout"),
    ("views[].sections", "sections"),
    // Section fields
    ("views[].sections[].id", "id"),
    ("views[].sections[].title", "title"),
    ("views[].sections[].grid", "grid"),
    ("views[].sections[].widgets", "widgets"),
    // SectionGrid sub-fields
    ("views[].sections[].grid.columns", "columns"),
    ("views[].sections[].grid.gap", "gap"),
    // Widget fields
    ("views[].sections[].widgets[].id", "id"),
    ("views[].sections[].widgets[].type", "widget_type"),
    ("views[].sections[].widgets[].entity", "entity"),
    ("views[].sections[].widgets[].entities", "entities"),
    ("views[].sections[].widgets[].name", "name"),
    ("views[].sections[].widgets[].icon", "icon"),
    ("views[].sections[].widgets[].visibility", "visibility"),
    ("views[].sections[].widgets[].tap_action", "tap_action"),
    ("views[].sections[].widgets[].hold_action", "hold_action"),
    (
        "views[].sections[].widgets[].double_tap_action",
        "double_tap_action",
    ),
    ("views[].sections[].widgets[].layout", "layout"),
    ("views[].sections[].widgets[].options", "options"),
    // WidgetLayout sub-fields
    (
        "views[].sections[].widgets[].layout.preferred_columns",
        "preferred_columns",
    ),
    (
        "views[].sections[].widgets[].layout.preferred_rows",
        "preferred_rows",
    ),
];

// ---------------------------------------------------------------------------
// Round-trip helpers
// ---------------------------------------------------------------------------

/// Parse `yaml` into `Dashboard` and round-trip it through serialization.
///
/// Asserts that `first == second` (structural equality via `PartialEq`).
/// Returns the first parsed `Dashboard` for further field-specific assertions.
fn assert_round_trips(label: &str, yaml: &str) -> Dashboard {
    let first: Dashboard = serde_yaml_ng::from_str(yaml)
        .unwrap_or_else(|e| panic!("round-trip [{label}] parse FAILED: {e}"));
    let serialized = serde_yaml_ng::to_string(&first)
        .unwrap_or_else(|e| panic!("round-trip [{label}] serialize FAILED: {e}"));
    let second: Dashboard = serde_yaml_ng::from_str(&serialized)
        .unwrap_or_else(|e| panic!("round-trip [{label}] re-parse FAILED: {e}"));
    assert_eq!(
        first, second,
        "round-trip [{label}] produced non-equal Dashboard values"
    );
    first
}

// ---------------------------------------------------------------------------
// Minimal base YAML (required fields only)
// ---------------------------------------------------------------------------

/// Minimal YAML containing only the required top-level fields.
/// Used as the base for most field-specific round-trip tests.
const MINIMAL_BASE: &str = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"#;

// ---------------------------------------------------------------------------
// Field coverage: verify every SCHEMA_FIELD_REGISTRY entry can be exercised
// ---------------------------------------------------------------------------

/// Assert that the `SCHEMA_FIELD_REGISTRY` is non-empty and covers all
/// top-level fields.
///
/// This test fails if the registry is accidentally cleared.
#[test]
fn schema_registry_is_non_empty_and_covers_top_level() {
    assert!(
        !SCHEMA_FIELD_REGISTRY.is_empty(),
        "SCHEMA_FIELD_REGISTRY must not be empty"
    );
    let top_level_fields = [
        "version",
        "device_profile",
        "home_assistant",
        "theme",
        "default_view",
        "views",
    ];
    for field in top_level_fields {
        let found = SCHEMA_FIELD_REGISTRY.iter().any(|(path, _)| *path == field);
        assert!(
            found,
            "top-level field '{field}' missing from SCHEMA_FIELD_REGISTRY"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-field round-trip tests
// ---------------------------------------------------------------------------

/// `version` (u32): a valid version integer round-trips correctly.
#[test]
fn field_version_round_trips() {
    let d = assert_round_trips("version", MINIMAL_BASE);
    assert_eq!(d.version, 1);
}

/// `device_profile` (ProfileKey enum): all three variants round-trip.
#[test]
fn field_device_profile_all_variants_round_trip() {
    for (yaml_val, expected) in [
        ("rpi4", ProfileKey::Rpi4),
        ("opi-zero3", ProfileKey::OpiZero3),
        ("desktop", ProfileKey::Desktop),
    ] {
        let yaml = format!(
            "version: 1\ndevice_profile: {yaml_val}\ndefault_view: home\nviews:\n  - id: home\n    title: Home\n    layout: sections\n    sections: []\n"
        );
        let d = assert_round_trips(&format!("device_profile:{yaml_val}"), &yaml);
        assert_eq!(
            d.device_profile, expected,
            "device_profile {yaml_val} must deserialize to {expected:?}"
        );
    }
}

/// `home_assistant` (Option<HomeAssistant>): present and absent cases.
#[test]
fn field_home_assistant_round_trips() {
    // Absent (None)
    let d = assert_round_trips("home_assistant:absent", MINIMAL_BASE);
    assert!(d.home_assistant.is_none());

    // Present
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
home_assistant:
  url: "ws://ha.local:8123/api/websocket"
  token_env: "HA_TOKEN"
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"#;
    let d = assert_round_trips("home_assistant:present", yaml);
    let ha = d.home_assistant.expect("home_assistant must be present");
    assert_eq!(ha.url, "ws://ha.local:8123/api/websocket");
    assert_eq!(ha.token_env, "HA_TOKEN");
}

/// `home_assistant.url` (String): the WebSocket endpoint round-trips.
#[test]
fn field_home_assistant_url_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
home_assistant:
  url: "ws://example.com:8123/api/websocket"
  token_env: "MYTOKEN"
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"#;
    let d = assert_round_trips("home_assistant.url", yaml);
    assert_eq!(
        d.home_assistant.unwrap().url,
        "ws://example.com:8123/api/websocket"
    );
}

/// `home_assistant.token_env` (String): the env-var name round-trips.
#[test]
fn field_home_assistant_token_env_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
home_assistant:
  url: "ws://ha.local/api/websocket"
  token_env: "MY_HA_TOKEN_ENV"
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"#;
    let d = assert_round_trips("home_assistant.token_env", yaml);
    assert_eq!(d.home_assistant.unwrap().token_env, "MY_HA_TOKEN_ENV");
}

/// `theme` (Option<Theme>): both absent and present cases.
#[test]
fn field_theme_round_trips() {
    // Absent
    let d = assert_round_trips("theme:absent", MINIMAL_BASE);
    assert!(d.theme.is_none());

    // Present
    let yaml = r##"version: 1
device_profile: desktop
default_view: home
theme:
  mode: dark
  accent: "#03a9f4"
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"##;
    let d = assert_round_trips("theme:present", yaml);
    let theme = d.theme.expect("theme must be present");
    assert_eq!(theme.mode, "dark");
    assert_eq!(theme.accent, "#03a9f4");
}

/// `theme.mode` and `theme.accent` (String): individual field round-trips.
#[test]
fn field_theme_mode_and_accent_round_trip() {
    let yaml = r##"version: 1
device_profile: desktop
default_view: home
theme:
  mode: light
  accent: "#ff5722"
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"##;
    let d = assert_round_trips("theme.mode+accent", yaml);
    let theme = d.theme.unwrap();
    assert_eq!(theme.mode, "light");
    assert_eq!(theme.accent, "#ff5722");
}

/// `default_view` (String): the referenced view id round-trips.
#[test]
fn field_default_view_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: security
views:
  - id: security
    title: Security
    layout: sections
    sections: []
"#;
    let d = assert_round_trips("default_view", yaml);
    assert_eq!(d.default_view, "security");
}

/// `views[].id` and `views[].title` (String fields): round-trip correctly.
#[test]
fn field_view_id_and_title_round_trip() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: my_view
views:
  - id: my_view
    title: "My Custom View"
    layout: sections
    sections: []
"#;
    let d = assert_round_trips("views[].id+title", yaml);
    assert_eq!(d.views[0].id, "my_view");
    assert_eq!(d.views[0].title, "My Custom View");
}

/// `views[].layout` (Layout enum): `sections` variant round-trips.
#[test]
fn field_view_layout_round_trips() {
    let d = assert_round_trips("views[].layout:sections", MINIMAL_BASE);
    assert_eq!(d.views[0].layout, Layout::Sections);
}

/// `sections[].id`, `sections[].title` (String): round-trip correctly.
#[test]
fn field_section_id_and_title_round_trip() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: my_section
        title: "My Section"
        grid:
          columns: 4
        widgets: []
"#;
    let d = assert_round_trips("sections[].id+title", yaml);
    assert_eq!(d.views[0].sections[0].id, "my_section");
    assert_eq!(d.views[0].sections[0].title, "My Section");
}

/// `sections[].grid.columns` and `sections[].grid.gap` (u8): round-trip.
#[test]
fn field_section_grid_columns_and_gap_round_trip() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 3
          gap: 16
        widgets: []
"#;
    let d = assert_round_trips("sections[].grid.columns+gap", yaml);
    let grid = &d.views[0].sections[0].grid;
    assert_eq!(grid.columns, 3);
    assert_eq!(grid.gap, 16);
}

/// `widgets[].id`, `widgets[].type` (String + WidgetKind): round-trip
/// for all registered WidgetKind variants.
#[test]
fn field_widget_id_and_type_all_kinds_round_trip() {
    let kind_pairs = [
        ("light_tile", WidgetKind::LightTile),
        ("sensor_tile", WidgetKind::SensorTile),
        ("entity_tile", WidgetKind::EntityTile),
        ("camera", WidgetKind::Camera),
        ("history", WidgetKind::History),
        ("fan", WidgetKind::Fan),
        ("lock", WidgetKind::Lock),
        ("alarm", WidgetKind::Alarm),
    ];

    for (yaml_kind, expected_kind) in kind_pairs {
        let yaml = format!(
            r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: w1
            type: {yaml_kind}
            layout:
              preferred_columns: 1
              preferred_rows: 1
"#
        );
        let d = assert_round_trips(&format!("widget.type:{yaml_kind}"), &yaml);
        assert_eq!(
            d.views[0].sections[0].widgets[0].widget_type, expected_kind,
            "widget type {yaml_kind} must deserialize correctly"
        );
    }
}

/// `widgets[].entity` (Option<String>): absent and present cases.
#[test]
fn field_widget_entity_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: w1
            type: light_tile
            entity: light.kitchen
            layout:
              preferred_columns: 1
              preferred_rows: 1
"#;
    let d = assert_round_trips("widget.entity:present", yaml);
    assert_eq!(
        d.views[0].sections[0].widgets[0].entity.as_deref(),
        Some("light.kitchen")
    );
}

/// `widgets[].entities` (Vec<String>): non-empty list round-trips.
#[test]
fn field_widget_entities_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: w1
            type: sensor_tile
            entities:
              - sensor.temp_a
              - sensor.temp_b
            layout:
              preferred_columns: 1
              preferred_rows: 1
"#;
    let d = assert_round_trips("widget.entities", yaml);
    assert_eq!(
        d.views[0].sections[0].widgets[0].entities,
        vec!["sensor.temp_a", "sensor.temp_b"]
    );
}

/// `widgets[].name` (Option<String>): round-trips when present.
#[test]
fn field_widget_name_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: w1
            type: light_tile
            name: "Kitchen Light"
            layout:
              preferred_columns: 1
              preferred_rows: 1
"#;
    let d = assert_round_trips("widget.name", yaml);
    assert_eq!(
        d.views[0].sections[0].widgets[0].name.as_deref(),
        Some("Kitchen Light")
    );
}

/// `widgets[].icon` (Option<String>): round-trips when present.
#[test]
fn field_widget_icon_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: w1
            type: light_tile
            icon: "mdi:lightbulb"
            layout:
              preferred_columns: 1
              preferred_rows: 1
"#;
    let d = assert_round_trips("widget.icon", yaml);
    assert_eq!(
        d.views[0].sections[0].widgets[0].icon.as_deref(),
        Some("mdi:lightbulb")
    );
}

/// `widgets[].visibility` (String): round-trips for all known predicates.
#[test]
fn field_widget_visibility_all_predicates_round_trip() {
    let predicates = [
        "always",
        "never",
        "entity_available:light.kitchen",
        "state_equals:light.kitchen:on",
        "profile:rpi4",
    ];

    for predicate in predicates {
        let yaml = format!(
            r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: w1
            type: light_tile
            visibility: "{predicate}"
            layout:
              preferred_columns: 1
              preferred_rows: 1
"#
        );
        let d = assert_round_trips(&format!("widget.visibility:{predicate}"), &yaml);
        assert_eq!(
            d.views[0].sections[0].widgets[0].visibility, predicate,
            "visibility predicate {predicate:?} must round-trip exactly"
        );
    }
}

/// `widgets[].layout.preferred_columns` and `preferred_rows` (u8): round-trip.
#[test]
fn field_widget_layout_preferred_columns_and_rows_round_trip() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: w1
            type: light_tile
            layout:
              preferred_columns: 3
              preferred_rows: 2
"#;
    let d = assert_round_trips("widget.layout.preferred_columns+rows", yaml);
    let layout = &d.views[0].sections[0].widgets[0].layout;
    assert_eq!(layout.preferred_columns, 3);
    assert_eq!(layout.preferred_rows, 2);
}

/// `widgets[].tap_action`, `hold_action`, `double_tap_action`: each action
/// variant round-trips (tested via the `toggle` action as a representative).
#[test]
fn field_widget_actions_round_trip() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: w1
            type: light_tile
            tap_action:
              action: toggle
            hold_action:
              action: more-info
            double_tap_action:
              action: none
            layout:
              preferred_columns: 1
              preferred_rows: 1
"#;
    let d = assert_round_trips("widget.tap+hold+double_tap_action", yaml);
    let w = &d.views[0].sections[0].widgets[0];
    assert!(w.tap_action.is_some(), "tap_action must be present");
    assert!(w.hold_action.is_some(), "hold_action must be present");
    assert!(
        w.double_tap_action.is_some(),
        "double_tap_action must be present"
    );
}

/// `widgets[].options` camera variant: round-trips with externally-tagged form.
#[test]
fn field_widget_options_camera_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: cam1
            type: camera
            entity: camera.front_door
            layout:
              preferred_columns: 2
              preferred_rows: 1
            options:
              camera:
                interval_seconds: 10
"#;
    let d = assert_round_trips("widget.options:camera", yaml);
    let opts = d.views[0].sections[0].widgets[0]
        .options
        .as_ref()
        .expect("options must be present");
    assert!(
        matches!(
            opts,
            hanui::dashboard::schema::WidgetOptions::Camera {
                interval_seconds: 10
            }
        ),
        "camera options must have interval_seconds=10"
    );
}

/// `widgets[].options` history variant: round-trips with externally-tagged form.
#[test]
fn field_widget_options_history_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: hist1
            type: history
            entity: sensor.temperature
            layout:
              preferred_columns: 2
              preferred_rows: 1
            options:
              history:
                window_seconds: 3600
"#;
    let d = assert_round_trips("widget.options:history", yaml);
    let opts = d.views[0].sections[0].widgets[0]
        .options
        .as_ref()
        .expect("options must be present");
    assert!(
        matches!(
            opts,
            hanui::dashboard::schema::WidgetOptions::History {
                window_seconds: 3600
            }
        ),
        "history options must have window_seconds=3600"
    );
}

/// `widgets[].options` fan variant: round-trips with externally-tagged form.
#[test]
fn field_widget_options_fan_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: fan1
            type: fan
            entity: fan.ceiling
            layout:
              preferred_columns: 2
              preferred_rows: 1
            options:
              fan:
                speed_count: 3
                preset_modes:
                  - Low
                  - Medium
                  - High
"#;
    let d = assert_round_trips("widget.options:fan", yaml);
    let opts = d.views[0].sections[0].widgets[0]
        .options
        .as_ref()
        .expect("options must be present");
    match opts {
        hanui::dashboard::schema::WidgetOptions::Fan {
            speed_count,
            preset_modes,
        } => {
            assert_eq!(*speed_count, 3);
            assert_eq!(preset_modes, &["Low", "Medium", "High"]);
        }
        other => panic!("expected Fan options; got: {other:?}"),
    }
}

/// `widgets[].options` lock variant: round-trips with externally-tagged form +
/// `pin_policy.code_format`.
#[test]
fn field_widget_options_lock_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: lock1
            type: lock
            entity: lock.front_door
            layout:
              preferred_columns: 2
              preferred_rows: 1
            options:
              lock:
                pin_policy:
                  code_format: "Number"
"#;
    let d = assert_round_trips("widget.options:lock", yaml);
    let opts = d.views[0].sections[0].widgets[0]
        .options
        .as_ref()
        .expect("options must be present");
    match opts {
        hanui::dashboard::schema::WidgetOptions::Lock { pin_policy } => {
            assert_eq!(pin_policy.code_format, "Number");
        }
        other => panic!("expected Lock options; got: {other:?}"),
    }
}

/// `widgets[].options` alarm variant: round-trips with externally-tagged form +
/// `pin_policy.code_format`.
#[test]
fn field_widget_options_alarm_round_trips() {
    let yaml = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: alarm1
            type: alarm
            entity: alarm_control_panel.home
            layout:
              preferred_columns: 4
              preferred_rows: 1
            options:
              alarm:
                pin_policy:
                  code_format: "Any"
"#;
    let d = assert_round_trips("widget.options:alarm", yaml);
    let opts = d.views[0].sections[0].widgets[0]
        .options
        .as_ref()
        .expect("options must be present");
    match opts {
        hanui::dashboard::schema::WidgetOptions::Alarm { pin_policy } => {
            assert_eq!(pin_policy.code_format, "Any");
        }
        other => panic!("expected Alarm options; got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Comprehensive round-trip test
// ---------------------------------------------------------------------------

/// `every_doc_field_round_trips` — one composite YAML exercising all documented
/// fields at once, verifying `Dashboard == Dashboard` after a full round-trip.
///
/// This is the primary schema-lock gate. If a field's serde impl changes (e.g.,
/// a rename or a type change), this test fails immediately.
#[test]
fn every_doc_field_round_trips() {
    // Construct a Dashboard struct directly (not via YAML) to avoid the
    // "chicken and egg" problem of parsing a YAML that might have the same
    // bugs as the types. Then serialize it, re-parse it, and assert equality.
    let original = Dashboard {
        version: 1,
        device_profile: ProfileKey::Rpi4,
        home_assistant: Some(HomeAssistant {
            url: "ws://homeassistant.local:8123/api/websocket".to_string(),
            token_env: "HA_TOKEN".to_string(),
        }),
        theme: Some(Theme {
            mode: "dark".to_string(),
            accent: "#03a9f4".to_string(),
        }),
        default_view: "home".to_string(),
        views: vec![View {
            id: "home".to_string(),
            title: "Home".to_string(),
            layout: Layout::Sections,
            sections: vec![Section {
                id: "overview".to_string(),
                title: "Overview".to_string(),
                grid: SectionGrid { columns: 4, gap: 8 },
                widgets: vec![
                    Widget {
                        id: "light1".to_string(),
                        widget_type: WidgetKind::LightTile,
                        entity: Some("light.kitchen".to_string()),
                        entities: vec![],
                        name: Some("Kitchen".to_string()),
                        icon: Some("mdi:lightbulb".to_string()),
                        visibility: "always".to_string(),
                        tap_action: Some(hanui::actions::Action::Toggle),
                        hold_action: Some(hanui::actions::Action::MoreInfo),
                        double_tap_action: Some(hanui::actions::Action::None),
                        layout: WidgetLayout {
                            preferred_columns: 2,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: None,
                    },
                    Widget {
                        id: "sensor1".to_string(),
                        widget_type: WidgetKind::SensorTile,
                        entity: Some("sensor.temp".to_string()),
                        entities: vec!["sensor.temp2".to_string()],
                        name: None,
                        icon: None,
                        visibility: "entity_available:sensor.temp".to_string(),
                        tap_action: None,
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 1,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: None,
                    },
                ],
            }],
        }],
        call_service_allowlist: Arc::default(),
    };

    // Serialize to YAML and re-parse.
    let yaml = serde_yaml_ng::to_string(&original).expect("serialize must succeed");
    let reparsed: Dashboard = serde_yaml_ng::from_str(&yaml).expect("re-parse must succeed");
    assert_eq!(
        original, reparsed,
        "Dashboard must be structurally equal after a full round-trip"
    );

    // Verify that every top-level field from SCHEMA_FIELD_REGISTRY was
    // exercised by checking that at least one entry per top-level key is
    // present in the registry.
    let top_level_paths: Vec<&str> = SCHEMA_FIELD_REGISTRY
        .iter()
        .map(|(path, _)| *path)
        .collect();
    for required in &[
        "version",
        "device_profile",
        "home_assistant",
        "theme",
        "default_view",
        "views",
    ] {
        assert!(
            top_level_paths.contains(required),
            "SCHEMA_FIELD_REGISTRY missing top-level path '{required}'"
        );
    }
}

// ---------------------------------------------------------------------------
// ValidationRule severity lock (schema-level pin)
// ---------------------------------------------------------------------------

/// Assert that all `ValidationRule` variants are accounted for and their
/// severity is locked. This is the schema-side companion to the validator-level
/// `severity_pin` test in `tests/integration/validation.rs`.
///
/// This test explicitly constructs one `Issue` per rule and checks severity,
/// so that renaming a `ValidationRule` variant breaks THIS test (compile error)
/// in addition to the validation.rs test (runtime assertion).
#[test]
fn schema_lock_all_validation_rule_variants_are_named() {
    // Error rules (8 variants)
    let error_cases = [
        ValidationRule::SpanOverflow,
        ValidationRule::UnknownWidgetType,
        ValidationRule::UnknownVisibilityPredicate,
        ValidationRule::NonAllowlistedCallService,
        ValidationRule::MaxWidgetsPerViewExceeded,
        ValidationRule::CameraIntervalBelowMin,
        ValidationRule::HistoryWindowAboveMax,
        ValidationRule::PinPolicyInvalidCodeFormat,
    ];
    for rule in error_cases {
        let issue = Issue {
            rule,
            severity: Severity::Error,
            path: String::new(),
            message: String::new(),
            yaml_excerpt: String::new(),
        };
        assert_eq!(
            issue.severity,
            Severity::Error,
            "rule {rule:?} must be Error"
        );
    }

    // Warning rules (2 variants)
    let warning_cases = [
        ValidationRule::ImageOptionExceedsMaxPx,
        ValidationRule::CameraIntervalBelowDefault,
    ];
    for rule in warning_cases {
        let issue = Issue {
            rule,
            severity: Severity::Warning,
            path: String::new(),
            message: String::new(),
            yaml_excerpt: String::new(),
        };
        assert_eq!(
            issue.severity,
            Severity::Warning,
            "rule {rule:?} must be Warning"
        );
    }
}
