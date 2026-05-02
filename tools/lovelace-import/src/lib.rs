//! Lovelace → hanui dashboard YAML importer (library crate).
//!
//! Phase 6 sub-phase 6c (TASK-111). The CLI front-end lives in `src/main.rs`
//! and re-uses the [`convert`] entry point exposed here. Splitting the logic
//! into a `lib` target lets the `tests/e2e.rs` integration suite drive the
//! conversion without spawning the binary as a subprocess.
//!
//! # Locked decisions referenced
//!
//! - `lovelace_workspace_cargo_shape` — separate workspace member, direct
//!   `hanui` lib dependency, no subprocess.
//! - `lovelace_minimum_card_set` — eight locked card types in
//!   [`mappings::LovelaceCard`].
//! - `lovelace_import_output_path` — `--output` / `--force` / `--stdout`
//!   semantics enforced by the CLI binary; library API is path-agnostic.
//!
//! # Risk #5 (Lovelace YAML schema instability)
//!
//! Lovelace YAML is not formally versioned. The importer treats unknown card
//! types and unknown widget fields as warnings: they are surfaced via the
//! [`Conversion::unmapped`] log so the user can hand-edit them, but they do
//! NOT abort the conversion. The converted dashboard is run through
//! `hanui::dashboard::validate::validate` before [`convert`] returns, so the
//! emitted YAML is guaranteed to deserialise + pass schema validation against
//! `PROFILE_DESKTOP` (the most permissive preset — the user picks their real
//! profile during hand-editing).

pub mod mappings;

use hanui::dashboard::profiles::PROFILE_DESKTOP;
use hanui::dashboard::schema::{
    Dashboard, Layout, ProfileKey, Section, SectionGrid, Severity, View, Widget, WidgetKind,
    WidgetLayout,
};
use hanui::dashboard::validate::validate;
use hanui::dashboard::visibility::build_dep_index;
use serde_yaml_ng::Value;
use thiserror::Error;

use crate::mappings::{MappedKind, MappingTable};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// The result of a successful conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    /// Hanui dashboard YAML, ready to write to disk.
    ///
    /// Validated against [`PROFILE_DESKTOP`] before [`convert`] returns; the
    /// importer never produces a YAML the schema validator would reject.
    pub yaml: String,
    /// Human-readable log of every Lovelace card the importer could not map
    /// to a hanui [`WidgetKind`]. Each entry is one line in the format
    /// `view=<title> card=<index> type=<lovelace-type>`. The CLI writes this
    /// list to a sidecar file (and to the YAML as a `# UNMAPPED:` comment
    /// block) so the user can fix-up by hand. Empty when every card mapped.
    pub unmapped: Vec<String>,
}

/// Errors produced by [`convert`].
#[derive(Debug, Error)]
pub enum ImportError {
    /// The Lovelace input could not be parsed as YAML.
    #[error("lovelace input is not valid YAML: {0}")]
    InputParse(String),

    /// The Lovelace input is YAML but not the expected shape (e.g. `views:`
    /// is absent or not a sequence).
    #[error("lovelace input has unexpected shape: {0}")]
    InputShape(String),

    /// The hanui dashboard the importer constructed failed schema validation.
    /// This is an internal error — it indicates a bug in the importer, not
    /// user input. The contained string is a multiline list of validation
    /// issues for diagnosis.
    #[error("importer produced an invalid hanui dashboard:\n{0}")]
    OutputValidation(String),

    /// The hanui dashboard could not be re-serialised to YAML.
    #[error("could not serialise hanui dashboard: {0}")]
    OutputSerialise(String),
}

/// Convert a Lovelace dashboard YAML string into hanui dashboard YAML.
///
/// # Errors
///
/// See [`ImportError`].
pub fn convert(input_yaml: &str) -> Result<Conversion, ImportError> {
    let table = MappingTable::new();

    let lovelace: Value =
        serde_yaml_ng::from_str(input_yaml).map_err(|e| ImportError::InputParse(e.to_string()))?;

    let mut unmapped: Vec<String> = Vec::new();
    let views = build_views(&lovelace, &table, &mut unmapped)?;

    let mut dashboard = Dashboard {
        version: 1,
        device_profile: ProfileKey::Desktop,
        home_assistant: None,
        theme: None,
        default_view: views
            .first()
            .map(|v| v.id.clone())
            .unwrap_or_else(|| "default".to_string()),
        views,
        call_service_allowlist: std::sync::Arc::new(Default::default()),
        // `dep_index` is internal (skipped during YAML serialisation) but the
        // struct literal still needs it. Build it from the views we just
        // produced so the validator's downstream consumers see the same shape
        // as a normally-loaded dashboard.
        dep_index: std::sync::Arc::new(Default::default()),
    };
    let dep_index = build_dep_index(&dashboard);
    dashboard.dep_index = std::sync::Arc::new(dep_index);

    // Validate before emitting — the importer must never produce YAML the
    // hanui validator rejects.
    let (issues, _allow) = validate(&dashboard, &PROFILE_DESKTOP);
    let errors: Vec<String> = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .map(|i| format!("- {:?} at {}: {}", i.rule, i.path, i.message))
        .collect();
    if !errors.is_empty() {
        return Err(ImportError::OutputValidation(errors.join("\n")));
    }

    let yaml = serde_yaml_ng::to_string(&dashboard)
        .map_err(|e| ImportError::OutputSerialise(e.to_string()))?;

    Ok(Conversion { yaml, unmapped })
}

// ---------------------------------------------------------------------------
// Internal: view + widget construction
// ---------------------------------------------------------------------------

/// Build the `views` list for the output dashboard.
///
/// Lovelace shape: a top-level `views:` sequence; each entry has a `title`,
/// optional `path`, and a `cards:` sequence. We emit one hanui [`View`] per
/// Lovelace view, with a single section that contains every mapped card.
fn build_views(
    lovelace: &Value,
    table: &MappingTable,
    unmapped: &mut Vec<String>,
) -> Result<Vec<View>, ImportError> {
    let views_value = lovelace.get("views").ok_or_else(|| {
        ImportError::InputShape("top-level `views:` field is missing".to_string())
    })?;
    let views_seq = views_value
        .as_sequence()
        .ok_or_else(|| ImportError::InputShape("`views:` is not a sequence".to_string()))?;

    if views_seq.is_empty() {
        return Err(ImportError::InputShape(
            "`views:` is empty — at least one view is required".to_string(),
        ));
    }

    let mut out: Vec<View> = Vec::with_capacity(views_seq.len());
    for (view_idx, view_yaml) in views_seq.iter().enumerate() {
        let title = view_yaml
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("View")
            .to_string();
        // Lovelace `path:` is the URL slug; reuse it as the hanui view id when
        // present (HA dashboards typically set it). Fall back to a synthesised
        // id when absent so two title-collision views still get distinct ids.
        let id = view_yaml
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("view_{view_idx}"));

        let mut widgets: Vec<Widget> = Vec::new();
        if let Some(cards) = view_yaml.get("cards").and_then(Value::as_sequence) {
            for (card_idx, card_yaml) in cards.iter().enumerate() {
                walk_card(card_yaml, table, &title, card_idx, &mut widgets, unmapped);
            }
        }

        out.push(View {
            id,
            title,
            layout: Layout::Sections,
            sections: vec![Section {
                id: "main".to_string(),
                title: "Imported".to_string(),
                grid: SectionGrid { columns: 4, gap: 8 },
                widgets,
            }],
        });
    }

    Ok(out)
}

/// Walk a single Lovelace card and append the resulting hanui widget(s) to
/// `widgets`. Container cards (`vertical-stack`, `horizontal-stack`) recurse
/// into their `cards:` field.
fn walk_card(
    card: &Value,
    table: &MappingTable,
    view_title: &str,
    card_idx: usize,
    widgets: &mut Vec<Widget>,
    unmapped: &mut Vec<String>,
) {
    match table.map_yaml_card(card) {
        MappedKind::Widget(kind) => {
            widgets.push(build_widget(kind, card, widgets.len()));
        }
        MappedKind::Container => {
            // Recurse into nested `cards:` if present. A stack with no cards
            // is a no-op (no widgets emitted, no UNMAPPED entry — Lovelace
            // permits empty stacks).
            if let Some(children) = card.get("cards").and_then(Value::as_sequence) {
                for (child_idx, child) in children.iter().enumerate() {
                    walk_card(child, table, view_title, child_idx, widgets, unmapped);
                }
            }
        }
        MappedKind::Unmapped(type_str) => {
            unmapped.push(format!("view={view_title} card={card_idx} type={type_str}"));
        }
    }
}

/// Construct a hanui [`Widget`] from a Lovelace card with a known mapping.
///
/// Only the universal fields (`entity`, `name`) are forwarded; tile-kind-
/// specific options (`Camera::interval_seconds`, `Climate::min_temp`, etc.)
/// are NOT inferred — they have no Lovelace equivalent in the locked
/// vocabulary. The user fills them in by hand after import. The hanui
/// validator accepts widgets without `options:` for every kind in the
/// minimum-viable mapping set (the validator's per-option rules — e.g.
/// `CameraIntervalBelowMin` — only fire when `options:` is present).
fn build_widget(kind: WidgetKind, card: &Value, seq: usize) -> Widget {
    // Lovelace `entity:` is the canonical single-entity field. Some cards use
    // `entities:` (a list) — when both are absent the importer emits the
    // widget with no entity (the user must fill it in).
    let entity = card
        .get("entity")
        .and_then(Value::as_str)
        .map(str::to_string);

    let name = card.get("name").and_then(Value::as_str).map(str::to_string);

    // Multi-entity Lovelace cards (`entities:`, `glance.entities:`) emit a
    // single hanui widget with the list copied into `entities:`. Each entry
    // can be a bare `"sensor.foo"` string or a `{ entity: "sensor.foo" }`
    // mapping — the importer normalises both forms.
    let entities: Vec<String> = card
        .get("entities")
        .and_then(Value::as_sequence)
        .map(|seq| {
            seq.iter()
                .filter_map(|e| {
                    if let Some(s) = e.as_str() {
                        Some(s.to_string())
                    } else {
                        e.get("entity").and_then(Value::as_str).map(str::to_string)
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // Synthesised id: `imported_{n}` is unique within the section.
    let id = format!("imported_{seq}");

    Widget {
        id,
        widget_type: kind,
        entity,
        entities,
        name,
        icon: None,
        visibility: Widget::default_visibility(),
        tap_action: None,
        hold_action: None,
        double_tap_action: None,
        layout: WidgetLayout {
            preferred_columns: 2,
            preferred_rows: 2,
        },
        options: None,
        placement: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest possible valid Lovelace input for unit tests: one view with
    /// one `light` card.
    const MINIMAL_LOVELACE: &str = r#"title: Demo
views:
  - title: Home
    path: home
    cards:
      - type: light
        entity: light.kitchen
        name: Kitchen
"#;

    #[test]
    fn minimal_lovelace_round_trips_through_validator() {
        let out = convert(MINIMAL_LOVELACE).expect("conversion must succeed");
        assert!(out.unmapped.is_empty(), "no UNMAPPED expected");
        // Re-parse the emitted YAML to prove it is a valid Dashboard.
        let _: Dashboard = serde_yaml_ng::from_str(&out.yaml).expect("output is valid YAML");
        assert!(out.yaml.contains("light_tile"));
        assert!(out.yaml.contains("light.kitchen"));
    }

    #[test]
    fn unknown_card_type_is_logged_not_dropped() {
        let yaml = r#"title: Demo
views:
  - title: Home
    path: home
    cards:
      - type: button
        entity: light.kitchen
"#;
        let out = convert(yaml).expect("conversion must succeed even with UNMAPPED");
        assert_eq!(out.unmapped.len(), 1);
        assert!(out.unmapped[0].contains("type=button"));
    }

    #[test]
    fn vertical_stack_recurses_into_children() {
        let yaml = r#"title: Demo
views:
  - title: Home
    path: home
    cards:
      - type: vertical-stack
        cards:
          - type: light
            entity: light.kitchen
          - type: light
            entity: light.bedroom
"#;
        let out = convert(yaml).expect("conversion must succeed");
        assert!(
            out.unmapped.is_empty(),
            "stacks themselves are not UNMAPPED"
        );
        let dashboard: Dashboard = serde_yaml_ng::from_str(&out.yaml).unwrap();
        let widgets = &dashboard.views[0].sections[0].widgets;
        assert_eq!(widgets.len(), 2, "both child lights must be emitted");
        assert!(widgets
            .iter()
            .all(|w| w.widget_type == WidgetKind::LightTile));
    }

    #[test]
    fn horizontal_stack_recurses_into_children() {
        let yaml = r#"title: Demo
views:
  - title: Home
    path: home
    cards:
      - type: horizontal-stack
        cards:
          - type: glance
            entities:
              - sensor.temp
              - sensor.humidity
"#;
        let out = convert(yaml).expect("conversion must succeed");
        let dashboard: Dashboard = serde_yaml_ng::from_str(&out.yaml).unwrap();
        let widgets = &dashboard.views[0].sections[0].widgets;
        assert_eq!(widgets.len(), 1);
        assert_eq!(widgets[0].widget_type, WidgetKind::EntityTile);
        assert_eq!(widgets[0].entities.len(), 2);
    }

    #[test]
    fn missing_views_field_returns_input_shape_error() {
        let yaml = "title: Demo\n";
        match convert(yaml) {
            Err(ImportError::InputShape(_)) => {}
            other => panic!("expected InputShape, got {other:?}"),
        }
    }

    #[test]
    fn empty_views_returns_input_shape_error() {
        let yaml = "views: []\n";
        match convert(yaml) {
            Err(ImportError::InputShape(_)) => {}
            other => panic!("expected InputShape, got {other:?}"),
        }
    }

    #[test]
    fn invalid_yaml_returns_input_parse_error() {
        let yaml = "this: is: not: valid: yaml: {{{";
        match convert(yaml) {
            Err(ImportError::InputParse(_)) => {}
            other => panic!("expected InputParse, got {other:?}"),
        }
    }

    #[test]
    fn entities_card_normalises_object_form() {
        let yaml = r#"views:
  - title: Home
    path: home
    cards:
      - type: entities
        entities:
          - entity: sensor.temp
          - sensor.humidity
"#;
        let out = convert(yaml).expect("conversion must succeed");
        let dashboard: Dashboard = serde_yaml_ng::from_str(&out.yaml).unwrap();
        let widget = &dashboard.views[0].sections[0].widgets[0];
        assert_eq!(widget.entities, vec!["sensor.temp", "sensor.humidity"]);
    }

    #[test]
    fn glance_card_maps_to_entity_tile() {
        let yaml = r#"views:
  - title: Home
    path: home
    cards:
      - type: glance
        entities:
          - sensor.temp
"#;
        let out = convert(yaml).expect("conversion must succeed");
        let dashboard: Dashboard = serde_yaml_ng::from_str(&out.yaml).unwrap();
        assert_eq!(
            dashboard.views[0].sections[0].widgets[0].widget_type,
            WidgetKind::EntityTile
        );
    }

    #[test]
    fn thermostat_card_maps_to_climate() {
        let yaml = r#"views:
  - title: Home
    path: home
    cards:
      - type: thermostat
        entity: climate.living
"#;
        let out = convert(yaml).expect("conversion must succeed");
        let dashboard: Dashboard = serde_yaml_ng::from_str(&out.yaml).unwrap();
        assert_eq!(
            dashboard.views[0].sections[0].widgets[0].widget_type,
            WidgetKind::Climate
        );
    }
}
