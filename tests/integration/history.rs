//! Phase 6 acceptance integration tests for the history graph widget
//! (TASK-112).
//!
//! Exercises the HistoryGraphVM and the bridge's per-window path
//! composition (`history_path_commands`) end-to-end against a synthetic
//! history window. Per-VM unit tests live in `src/ui/history_graph.rs::tests`;
//! this file is the cross-component integration layer required by TASK-112
//! acceptance criterion #2.
//!
//! Mitigates Phase 6 Risk #1 for the history domain: idle (no data),
//! active (numeric trace), unavailable rendering plus a path-composition
//! sanity check that the SVG mini-language string the Slint Path consumes
//! is produced as documented.

use std::sync::Arc;

use jiff::Timestamp;
use serde_json::{json, Map};

use hanui::ha::entity::{Entity, EntityId};
use hanui::ha::history::{HistoryPoint, HistoryWindow};
use hanui::ha::live_store::LiveStore;
use hanui::ha::store::EntityStore;
use hanui::ui::bridge::{compute_history_graph_tile_vm, history_path_commands, TilePlacement};
use hanui::ui::history_graph::{
    read_friendly_name_attribute, read_unit_of_measurement_attribute, HistoryGraphVM,
};

// Build a `HistoryWindow` from a list of `(unix_seconds, value)` pairs by
// constructing `HistoryPoint`s (which carry the numeric parsing) and routing
// through `HistoryWindow::from_points` — the only public construction path.
fn window_from(points: &[(i64, f64)]) -> HistoryWindow {
    let raw: Vec<HistoryPoint> = points
        .iter()
        .map(|(ts, v)| {
            HistoryPoint::new(
                format!("{v}"),
                Timestamp::from_second(*ts).expect("test timestamp must be valid"),
            )
        })
        .collect();
    HistoryWindow::from_points(&raw, points.len().max(1))
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn sensor_entity(id: &str, state: &str, attrs: Map<String, serde_json::Value>) -> Entity {
    Entity {
        id: EntityId::from(id),
        state: Arc::from(state),
        attributes: Arc::new(attrs),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    }
}

fn placement() -> TilePlacement {
    TilePlacement {
        col: 0,
        row: 0,
        span_cols: 2,
        span_rows: 2,
    }
}

// ---------------------------------------------------------------------------
// Idle / active / unavailable rendering
// ---------------------------------------------------------------------------

#[test]
fn idle_active_unavailable_renders_distinct_vms() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        sensor_entity("sensor.idle", "0", Map::new()),
        sensor_entity("sensor.active", "23.5", Map::new()),
        sensor_entity("sensor.unavailable", "unavailable", Map::new()),
    ]);

    let idle_entity = store.get(&EntityId::from("sensor.idle")).unwrap();
    let active_entity = store.get(&EntityId::from("sensor.active")).unwrap();
    let unavail_entity = store.get(&EntityId::from("sensor.unavailable")).unwrap();

    // Idle: available state but no history points yet — change_count=0.
    let idle_vm = compute_history_graph_tile_vm(
        "Energy".to_owned(),
        "mdi:chart-line".to_owned(),
        2,
        2,
        placement(),
        &idle_entity,
        None,
    );
    assert!(idle_vm.is_available);
    assert_eq!(idle_vm.change_count, 0);
    assert!(idle_vm.path_commands.is_empty());

    // Active: numeric state, supplied window with 4 plotted points.
    let window = window_from(&[(0, 10.0), (60, 20.0), (120, 15.0), (180, 25.0)]);
    let active_vm = compute_history_graph_tile_vm(
        "Energy".to_owned(),
        "mdi:chart-line".to_owned(),
        2,
        2,
        placement(),
        &active_entity,
        Some(&window),
    );
    assert!(active_vm.is_available);
    assert_eq!(active_vm.change_count, 4);
    assert!(active_vm.path_commands.starts_with('M'));
    assert!(active_vm.path_commands.contains('L'));

    // Unavailable: is_available=false, change_count forwarded as 0.
    let unavail_vm = compute_history_graph_tile_vm(
        "Energy".to_owned(),
        "mdi:chart-line".to_owned(),
        2,
        2,
        placement(),
        &unavail_entity,
        None,
    );
    assert!(!unavail_vm.is_available);
    assert_eq!(unavail_vm.change_count, 0);
}

// ---------------------------------------------------------------------------
// History fetch + graph rendering — path composition contract
// ---------------------------------------------------------------------------

/// `history_path_commands` builds an `M x y L x y L x y ...` SVG mini-
/// language string per `locked_decisions.history_render_path`. The
/// coordinates are normalised into the unit square; the first point uses
/// `M`, every subsequent point uses `L`.
#[test]
fn history_path_commands_composes_unit_square_polyline() {
    let window = window_from(&[(0, 0.0), (50, 50.0), (100, 100.0)]);
    let path = history_path_commands(&window);
    // Begins with M, contains exactly two L commands (one per non-first
    // point), and contains exactly three coordinate pairs.
    assert!(path.starts_with("M "));
    let l_count = path.matches('L').count();
    assert_eq!(l_count, 2);

    // The first coordinate is always (0.0, *) — first point lands at x=0.
    assert!(path.starts_with("M 0.0000"));
    // The last coordinate is always (1.0, *) — last point lands at x=1.
    assert!(path.contains("1.0000"));
}

#[test]
fn history_path_commands_empty_window_returns_empty_string() {
    // A window built from zero raw points has no plottable data.
    let window = HistoryWindow::from_points(&[], 8);
    let path = history_path_commands(&window);
    assert!(
        path.is_empty(),
        "empty window must produce empty path commands; got {path:?}"
    );
}

#[test]
fn history_path_commands_single_point_collapses_to_zero_x() {
    let window = window_from(&[(42, 7.5)]);
    let path = history_path_commands(&window);
    // Single-point window: ts_span=0 → x=0; min==max → y=0.5.
    assert_eq!(path, "M 0.0000 0.5000");
}

// ---------------------------------------------------------------------------
// HistoryGraphVM — change_count threading
// ---------------------------------------------------------------------------

#[test]
fn history_graph_vm_reports_change_count() {
    let entity = sensor_entity("sensor.power", "1.5", Map::new());
    let vm = HistoryGraphVM::from_entity(&entity, 42);
    assert!(vm.is_available);
    assert_eq!(vm.change_count, 42);
}

// ---------------------------------------------------------------------------
// Attribute accessors used by HistoryBody
// ---------------------------------------------------------------------------

#[test]
fn history_attribute_accessors_round_trip() {
    let mut attrs = Map::new();
    attrs.insert("unit_of_measurement".to_owned(), json!("kWh"));
    attrs.insert("friendly_name".to_owned(), json!("House Energy"));
    let entity = sensor_entity("sensor.energy", "12.3", attrs);

    assert_eq!(
        read_unit_of_measurement_attribute(&entity).as_deref(),
        Some("kWh")
    );
    assert_eq!(
        read_friendly_name_attribute(&entity).as_deref(),
        Some("House Energy")
    );
}
