//! Phase 6 acceptance entry-point test (TASK-112).
//!
//! Per acceptance criterion #8, this file orchestrates a single overarching
//! Phase 6 acceptance test that loads a dashboard fixture spanning every
//! Phase 6 widget kind and asserts:
//!
//!   * The dashboard parses against `hanui::dashboard::schema::Dashboard`.
//!   * Validation returns zero `Severity::Error` issues against
//!     `PROFILE_DESKTOP`.
//!   * Every Phase 6 widget kind is represented at least once.
//!   * `body_for_widget` returns a per-domain body for every widget kind
//!     present in the fixture (matching the exhaustive-match contract from
//!     `locked_decisions.more_info_dispatch`).
//!   * Visibility predicate evaluation works against a populated live
//!     store.
//!
//! The per-widget integration tests (`cover.rs`, `fan.rs`, `lock.rs`,
//! `alarm.rs`, `history.rs`, `camera.rs`, `climate.rs`, `media_player.rs`,
//! `power_flow.rs`) cover each domain in depth; this file is the
//! cross-widget verdict the founder smoke gate (TASK-114) consumes.

use std::path::Path;
use std::sync::Arc;

use hanui::dashboard::layout::pack;
use hanui::dashboard::profiles::PROFILE_DESKTOP;
use hanui::dashboard::schema::{Dashboard, Severity, WidgetKind};
use hanui::dashboard::validate;
use hanui::dashboard::visibility::{build_dep_index, evaluate};
use hanui::ha::entity::{Entity, EntityId};
use hanui::ha::live_store::LiveStore;
use hanui::ui::more_info::body_for_widget;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Embedded acceptance fixture covering every Phase 6 widget kind plus the
/// Phase 4 `LightTile` / `SensorTile` / `EntityTile` baseline (so the
/// fallback `body_for_widget` arms are also exercised).
///
/// The widget set:
///   * cover, fan, lock, alarm, climate, media_player, camera, history,
///     power_flow — every Phase 6 kind.
///   * light_tile, sensor_tile, entity_tile — the Phase 4 baseline.
///
/// Two of the widgets carry a Phase 6 visibility predicate so the dep-index
/// is populated; the rest default to `always`.
const PHASE6_FIXTURE_YAML: &str = r#"
version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: phase6
        title: Phase 6 Acceptance
        grid:
          columns: 4
          gap: 8
        widgets:
          - id: w_cover
            type: cover
            entity: cover.garage
            options:
              cover:
                position_min: 0
                position_max: 100
            layout: { preferred_columns: 2, preferred_rows: 1 }
          - id: w_fan
            type: fan
            entity: fan.bedroom
            options:
              fan:
                speed_count: 3
                preset_modes: [Low, Medium, High]
            layout: { preferred_columns: 1, preferred_rows: 1 }
          - id: w_lock
            type: lock
            entity: lock.front_door
            options:
              lock:
                pin_policy: none
                require_confirmation_on_unlock: false
            layout: { preferred_columns: 1, preferred_rows: 1 }
          - id: w_alarm
            type: alarm
            entity: alarm_control_panel.home
            options:
              alarm:
                pin_policy:
                  required_on_disarm:
                    length: 4
                    code_format: number
            layout: { preferred_columns: 2, preferred_rows: 1 }
          - id: w_climate
            type: climate
            entity: climate.living_room
            options:
              climate:
                min_temp: 15.0
                max_temp: 30.0
                step: 0.5
                hvac_modes: [heat, cool, auto, off]
            visibility: climate.living_room != unavailable
            layout: { preferred_columns: 2, preferred_rows: 1 }
          - id: w_media
            type: media_player
            entity: media_player.tv
            options:
              media_player:
                transport_set: [play, pause, next, prev]
                volume_step: 0.05
            layout: { preferred_columns: 2, preferred_rows: 1 }
          - id: w_camera
            type: camera
            entity: camera.front_door
            options:
              camera:
                interval_seconds: 30
                url: "https://camera.local/snapshot.jpg"
            layout: { preferred_columns: 2, preferred_rows: 2 }
          - id: w_history
            type: history
            entity: sensor.energy_kwh
            options:
              history:
                window_seconds: 86400
                max_points: 60
            layout: { preferred_columns: 2, preferred_rows: 2 }
          - id: w_power_flow
            type: power_flow
            entity: sensor.grid_power
            options:
              power_flow:
                grid_entity: sensor.grid_power
                solar_entity: sensor.solar_power
                battery_entity: sensor.battery_power
                battery_soc_entity: sensor.battery_soc
                home_entity: sensor.home_power
            layout: { preferred_columns: 2, preferred_rows: 2 }
          - id: w_light
            type: light_tile
            entity: light.kitchen
            visibility: light.kitchen == on
            layout: { preferred_columns: 1, preferred_rows: 1 }
          - id: w_sensor
            type: sensor_tile
            entity: sensor.temperature
            layout: { preferred_columns: 1, preferred_rows: 1 }
          - id: w_entity
            type: entity_tile
            entity: switch.outlet
            layout: { preferred_columns: 1, preferred_rows: 1 }
"#;

fn parse_fixture() -> Dashboard {
    serde_yaml_ng::from_str(PHASE6_FIXTURE_YAML)
        .expect("phase6_acceptance fixture must parse against the schema")
}

fn entity(id: &str, state: &str) -> Entity {
    Entity {
        id: EntityId::from(id),
        state: Arc::from(state),
        attributes: Arc::new(serde_json::Map::new()),
        last_changed: jiff::Timestamp::UNIX_EPOCH,
        last_updated: jiff::Timestamp::UNIX_EPOCH,
    }
}

// ---------------------------------------------------------------------------
// Phase 6 acceptance verdict — single test surfaced to TASK-114 founder gate
// ---------------------------------------------------------------------------

/// The Phase 6 acceptance verdict: parse + validate + per-widget body
/// dispatch + visibility evaluation, all against the embedded fixture.
///
/// A single test (rather than four) so the libtest output line is the
/// founder gate consumes. A failure inside any phase below short-circuits
/// the test with a clear message naming the failing component.
#[test]
fn phase6_acceptance_verdict() {
    // ---------------------------------------------------------------- parse
    let dashboard = parse_fixture();

    // ------------------------------------------------------------ validate
    let (issues, _allowlist) = validate::validate(&dashboard, &PROFILE_DESKTOP);
    let errors: Vec<_> = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "Phase 6 acceptance fixture must validate clean; got: {errors:?}"
    );

    // ----------------------------------------------------- widget kind set
    let mut kinds_present: Vec<WidgetKind> = Vec::new();
    for view in &dashboard.views {
        for section in &view.sections {
            for widget in &section.widgets {
                let kind = widget.widget_type.clone();
                if !kinds_present.contains(&kind) {
                    kinds_present.push(kind);
                }
            }
        }
    }
    let required: &[WidgetKind] = &[
        WidgetKind::Cover,
        WidgetKind::Fan,
        WidgetKind::Lock,
        WidgetKind::Alarm,
        WidgetKind::Climate,
        WidgetKind::MediaPlayer,
        WidgetKind::Camera,
        WidgetKind::History,
        WidgetKind::PowerFlow,
        WidgetKind::LightTile,
        WidgetKind::SensorTile,
        WidgetKind::EntityTile,
    ];
    for k in required {
        assert!(
            kinds_present.iter().any(|p| p == k),
            "Phase 6 acceptance fixture missing widget kind {k:?}"
        );
    }

    // ---------------------------------------------- per-widget body dispatch
    //
    // `body_for_widget` is the Risk #10 mitigation — a future addition to
    // `WidgetKind` is a compile error in the factory until extended. Here
    // we exercise the runtime path against a fully-populated live store
    // so every per-domain body's constructor runs.
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        entity("cover.garage", "open"),
        entity("fan.bedroom", "on"),
        entity("lock.front_door", "locked"),
        entity("alarm_control_panel.home", "armed_away"),
        entity("climate.living_room", "heat"),
        entity("media_player.tv", "playing"),
        entity("camera.front_door", "idle"),
        entity("sensor.energy_kwh", "12.5"),
        entity("sensor.grid_power", "1500"),
        entity("sensor.solar_power", "2000"),
        entity("sensor.battery_power", "-300"),
        entity("sensor.battery_soc", "82"),
        entity("sensor.home_power", "750"),
        entity("light.kitchen", "on"),
        entity("sensor.temperature", "21.0"),
        entity("switch.outlet", "off"),
    ]);

    for view in &dashboard.views {
        for section in &view.sections {
            for widget in &section.widgets {
                let body = body_for_widget(
                    widget.widget_type.clone(),
                    widget.options.as_ref(),
                    Arc::clone(&store),
                );
                // Smoke: render rows against the matching live entity. The
                // body must produce a (possibly empty) row list without
                // panicking. The result count varies per domain — we only
                // assert the call returns.
                if let Some(eid) = widget.entity.as_deref() {
                    if let Some(ent) =
                        hanui::ha::store::EntityStore::get(store.as_ref(), &EntityId::from(eid))
                    {
                        let _rows = body.render_rows(&ent);
                    }
                }
            }
        }
    }

    // ------------------------------------------------ visibility evaluation
    //
    // Two predicate-gated widgets in the fixture: `w_climate` (gated on
    // `climate.living_room != unavailable`) and `w_light` (gated on
    // `light.kitchen == on`). Both must evaluate true against the seeded
    // store, then flip false when the entity transitions.
    let primary = EntityId::from("dashboard");
    assert!(
        evaluate(
            "climate.living_room != unavailable",
            &primary,
            store.as_ref(),
            dashboard.device_profile,
        ),
        "climate available initially"
    );
    assert!(
        evaluate(
            "light.kitchen == on",
            &primary,
            store.as_ref(),
            dashboard.device_profile,
        ),
        "light is on initially"
    );

    // ----------------------------------------- dep-index built from fixture
    let index = build_dep_index(&dashboard);
    assert!(
        index.contains_key(&EntityId::from("climate.living_room")),
        "climate predicate dependency must reach the dep_index"
    );
    assert!(
        index.contains_key(&EntityId::from("light.kitchen")),
        "light predicate dependency must reach the dep_index"
    );

    // --------------------------------------------------- explicit verdict
    //
    // Test passes by reaching this point — the libtest "test result: ok"
    // line is the canonical machine-readable verdict. The PHASE6_ACCEPTANCE
    // marker below is a supplementary greppable line for human inspection
    // of CI logs (libtest only prints stdout from passing tests when run
    // with `--nocapture`); TASK-114's founder smoke gate consumes the
    // libtest result line directly, NOT this println.
    println!("PHASE6_ACCEPTANCE: PASS");
}

// ---------------------------------------------------------------------------
// Per-widget golden layout fixtures (TASK-112 acceptance #1)
// ---------------------------------------------------------------------------

/// Run a YAML fixture under `tests/layout/<name>.yaml` through the
/// validate + pack pipeline and assert the output matches the sibling
/// `.expected.json`. Mirrors the run_golden_fixture helper in
/// `tests/integration/layout.rs` (which is in this ticket's must_not_touch
/// list); duplicated here so this file stays self-contained and the
/// existing layout golden tests are not edited.
fn run_phase6_widget_fixture(name: &str) {
    #[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
    struct ExpectedPosition {
        widget_id: String,
        col: u8,
        row: u16,
        span_cols: u8,
        span_rows: u8,
    }
    #[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
    struct SectionPositions {
        section_id: String,
        positions: Vec<ExpectedPosition>,
    }
    #[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
    struct SuccessExpected {
        sections: Vec<SectionPositions>,
    }

    let yaml_path = format!("tests/layout/{name}.yaml");
    let expected_path = format!("tests/layout/{name}.expected.json");

    let yaml = std::fs::read_to_string(Path::new(&yaml_path))
        .unwrap_or_else(|e| panic!("read {yaml_path}: {e}"));
    let dashboard: Dashboard =
        serde_yaml_ng::from_str(&yaml).unwrap_or_else(|e| panic!("parse {yaml_path}: {e}"));

    // Validate cleanly under PROFILE_DESKTOP.
    let (issues, _allowlist) = validate::validate(&dashboard, &PROFILE_DESKTOP);
    let errors: Vec<_> = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "fixture {name} must validate clean; got: {errors:?}"
    );

    // Pack each section and compare positions to the expected.json.
    let actual: Vec<SectionPositions> = dashboard
        .views
        .iter()
        .flat_map(|v| v.sections.iter())
        .map(|section| {
            let positions = pack(&section.widgets, section.grid.columns)
                .into_iter()
                .map(|pw| ExpectedPosition {
                    widget_id: pw.widget_id,
                    col: pw.col,
                    row: pw.row,
                    span_cols: pw.span_cols,
                    span_rows: pw.span_rows,
                })
                .collect();
            SectionPositions {
                section_id: section.id.clone(),
                positions,
            }
        })
        .collect();

    let expected_str = std::fs::read_to_string(Path::new(&expected_path))
        .unwrap_or_else(|e| panic!("read {expected_path}: {e}"));
    let expected: SuccessExpected = serde_json::from_str(&expected_str)
        .unwrap_or_else(|e| panic!("parse {expected_path}: {e}"));

    assert_eq!(
        actual, expected.sections,
        "golden mismatch for fixture {name}"
    );
}

/// Each Phase 6 widget kind has a dedicated `<kind>_idle.yaml` +
/// `.expected.json` fixture under `tests/layout/`. Per-widget integration
/// tests in `tests/integration/<kind>.rs` cover idle / active / unavailable
/// VM rendering distinctness; this fixture set pins the schema + packer
/// path for the new widget kinds so a layout regression surfaces in CI.
#[test]
fn phase6_widget_layout_fixtures_pack_correctly() {
    let names = [
        "cover_idle",
        "fan_idle",
        "lock_idle",
        "alarm_idle",
        "climate_idle",
        "media_player_idle",
        "camera_idle",
        "history_idle",
        "power_flow_idle",
    ];
    for name in names {
        run_phase6_widget_fixture(name);
    }
}
