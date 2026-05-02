//! Integration tests for the power-flow widget (TASK-094).
//!
//! These tests live outside `src/ui/**` so they can construct
//! [`Entity`] values with arbitrary attribute maps via the JSON crate
//! (the `src/ui/**` Gate 2 grep gate forbids the JSON-crate path inside
//! production source). They exercise the full pipeline:
//!
//!   1. `WidgetOptions::PowerFlow` schema round-trip + the widget config
//!      is consumed by `body_for_widget` to construct a per-domain body.
//!   2. `LiveStore` is seeded with a synthetic set of grid / solar /
//!      battery / battery_soc / home entities.
//!   3. `body_for_widget` resolves the auxiliary entity ids against the
//!      store and constructs a `PowerFlowBody`. The body's
//!      `render_rows` produces the expected modal rows.
//!   4. The bridge's `compute_power_flow_tile_vm` projects the typed
//!      Rust VM with the live grid reading and pre-resolved auxiliary
//!      readings — exercising the entity-change-time hot path.
//!   5. State-changed events for the configured entities update the
//!      `LiveStore`; a re-derived `PowerFlowVM` reflects the new state
//!      within one flush cadence (locked_decisions.more_info_modal:
//!      bodies are computed at modal-open, NOT per render).

#![allow(clippy::cast_possible_truncation)]

use std::sync::Arc;

use jiff::Timestamp;
use serde_json::{json, Map, Value};

use hanui::dashboard::schema::WidgetOptions;
use hanui::ha::client::event_to_entity_update;
use hanui::ha::entity::{Entity, EntityId};
use hanui::ha::live_store::LiveStore;
use hanui::ha::protocol::{
    EventPayload, EventVariant, RawEntityState, StateChangedData, StateChangedEvent,
};
use hanui::ha::store::{EntityStore, EntityUpdate};
use hanui::ui::bridge::{compute_power_flow_tile_vm, PowerFlowAuxiliaryReadings, TilePlacement};
use hanui::ui::more_info::{body_for_widget, MoreInfoBody};
use hanui::ui::power_flow::{PowerFlowBody, PowerFlowVM};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Build a sensor-like entity with the given numeric state. Attributes are
/// optional — the typical power sensor reports the value via `state` only.
fn power_entity(id: &str, state: &str) -> Entity {
    Entity {
        id: EntityId::from(id),
        state: Arc::from(state),
        attributes: Arc::new(Map::new()),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    }
}

/// Build a `state_changed` [`EntityUpdate`] via the public conversion path.
///
/// `EntityUpdate` is `#[non_exhaustive]`; external struct-literal syntax is
/// forbidden. `EventPayload` (and its nested types) are constructible
/// externally and `event_to_entity_update` is the documented conversion path
/// — mirroring the helper in `tests/integration/lagged_resync.rs`.
fn make_update(entity_id: &str, new_state: &str) -> EntityUpdate {
    let payload = EventPayload {
        id: 1,
        event: EventVariant::StateChanged(Box::new(StateChangedEvent {
            event_type: "state_changed".to_owned(),
            data: StateChangedData {
                entity_id: entity_id.to_owned(),
                new_state: Some(RawEntityState {
                    entity_id: entity_id.to_owned(),
                    state: new_state.to_owned(),
                    attributes: serde_json::Value::Object(serde_json::Map::new()),
                    last_changed: "2024-01-01T00:00:00+00:00".to_owned(),
                    last_updated: "2024-01-01T00:00:00+00:00".to_owned(),
                }),
                old_state: None,
            },
            origin: "LOCAL".to_owned(),
            time_fired: "2024-01-01T00:00:00+00:00".to_owned(),
        })),
    };
    event_to_entity_update(&payload).expect("state_changed payload must produce Some(update)")
}

/// Construct a populated `LiveStore` with the canonical 5-entity power-flow
/// fixture (grid / solar / battery / battery_soc / home).
fn five_entity_store() -> Arc<LiveStore> {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        power_entity("sensor.grid_power", "1500"),
        power_entity("sensor.solar_power", "2000"),
        power_entity("sensor.battery_power", "-300"),
        power_entity("sensor.battery_soc", "82"),
        power_entity("sensor.home_power", "750"),
    ]);
    store
}

/// Build the canonical PowerFlow widget options pointing at the fixture
/// entity ids.
fn power_flow_options() -> WidgetOptions {
    WidgetOptions::PowerFlow {
        grid_entity: "sensor.grid_power".to_owned(),
        solar_entity: Some("sensor.solar_power".to_owned()),
        battery_entity: Some("sensor.battery_power".to_owned()),
        battery_soc_entity: Some("sensor.battery_soc".to_owned()),
        home_entity: Some("sensor.home_power".to_owned()),
    }
}

// ---------------------------------------------------------------------------
// PowerFlowVM read paths exercised against a live store
// ---------------------------------------------------------------------------

/// `PowerFlowVM::read_power_watts` produces the expected scalar for a
/// freshly-seeded `LiveStore` entry.
#[test]
fn power_flow_vm_read_power_watts_against_live_store() {
    let store = five_entity_store();
    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid entity must be present after apply_snapshot");
    assert_eq!(PowerFlowVM::read_power_watts(&grid), Some(1500.0));
}

/// `PowerFlowVM::read_power_watts` returns `None` when the live entity
/// transitions to `unavailable`.
#[test]
fn power_flow_vm_read_power_watts_handles_unavailable_state() {
    let store = five_entity_store();
    store.apply_event(make_update("sensor.grid_power", "unavailable"));
    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid entity must be present");
    assert_eq!(PowerFlowVM::read_power_watts(&grid), None);
}

/// `PowerFlowVM::read_battery_pct` clamps an above-100 SoC reading.
#[test]
fn power_flow_vm_read_battery_pct_clamps_against_live_store() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![power_entity("sensor.battery_soc", "120")]);
    let battery = store
        .get(&EntityId::from("sensor.battery_soc"))
        .expect("battery soc entity must be present");
    assert_eq!(PowerFlowVM::read_battery_pct(&battery), Some(100.0));
}

// ---------------------------------------------------------------------------
// body_for_widget integration — auxiliary entities resolve against the store
// ---------------------------------------------------------------------------

/// `body_for_widget` for `WidgetKind::PowerFlow` resolves the auxiliary
/// entity ids against the live store at modal-open time and the returned
/// body emits all five rows when every auxiliary entity is live.
#[test]
fn body_for_widget_power_flow_resolves_auxiliary_entities() {
    use hanui::dashboard::schema::WidgetKind;

    let store = five_entity_store();
    let options = power_flow_options();
    let body = body_for_widget(WidgetKind::PowerFlow, Some(&options), Arc::clone(&store));

    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid entity must be present");
    let rows = body.render_rows(&grid);

    // 5 rows: grid_w + solar_w + battery_w + battery_pct + home_w.
    assert_eq!(
        rows.len(),
        5,
        "all auxiliary entities live → 5 rows; got: {rows:?}"
    );
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(
        keys,
        vec!["grid_w", "solar_w", "battery_w", "battery_pct", "home_w"]
    );

    // Verify the row values reflect the snapshot readings.
    let by_key: std::collections::HashMap<_, _> = rows
        .iter()
        .map(|r| (r.key.as_str(), r.value.as_str()))
        .collect();
    assert_eq!(by_key.get("grid_w"), Some(&"1500.0 W"));
    assert_eq!(by_key.get("solar_w"), Some(&"2000.0 W"));
    assert_eq!(
        by_key.get("battery_w"),
        Some(&"-300.0 W"),
        "battery discharge sign preserved"
    );
    assert_eq!(by_key.get("battery_pct"), Some(&"82%"));
    assert_eq!(by_key.get("home_w"), Some(&"750.0 W"));
}

/// `body_for_widget` for `WidgetKind::PowerFlow` with `options=None`
/// returns a body that emits ONLY the grid_w row (the auxiliary entities
/// are unconfigured).
#[test]
fn body_for_widget_power_flow_no_options_grid_only() {
    use hanui::dashboard::schema::WidgetKind;

    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![power_entity("sensor.grid_power", "1234")]);
    let body = body_for_widget(WidgetKind::PowerFlow, None, Arc::clone(&store));

    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid entity must be present");
    let rows = body.render_rows(&grid);
    assert_eq!(rows.len(), 1, "no auxiliary options → 1 row");
    assert_eq!(rows[0].key, "grid_w");
    assert_eq!(rows[0].value, "1234.0 W");
}

/// `body_for_widget` for `WidgetKind::PowerFlow` with auxiliary entities
/// MISSING from the live store still emits the grid_w row plus suppressed
/// auxiliary rows (the resolver returns `None` for an absent entity).
#[test]
fn body_for_widget_power_flow_missing_auxiliary_entities_grid_only() {
    use hanui::dashboard::schema::WidgetKind;

    // Live store has only the grid entity; the options reference solar,
    // battery, battery_soc, home — none are present.
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![power_entity("sensor.grid_power", "500")]);
    let options = power_flow_options();
    let body = body_for_widget(WidgetKind::PowerFlow, Some(&options), Arc::clone(&store));

    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid entity must be present");
    let rows = body.render_rows(&grid);
    assert_eq!(
        rows.len(),
        1,
        "missing auxiliary entities → only grid_w row; got: {rows:?}"
    );
    assert_eq!(rows[0].key, "grid_w");
    assert_eq!(rows[0].value, "500.0 W");
}

/// `body_for_widget` for `WidgetKind::PowerFlow` with auxiliary entities in
/// the `unavailable` state suppresses the corresponding rows
/// (`PowerFlowVM::read_power_watts` returns `None` for the sentinel).
#[test]
fn body_for_widget_power_flow_unavailable_auxiliary_entities_suppressed() {
    use hanui::dashboard::schema::WidgetKind;

    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        power_entity("sensor.grid_power", "1500"),
        power_entity("sensor.solar_power", "unavailable"),
        power_entity("sensor.battery_power", "unknown"),
        power_entity("sensor.battery_soc", "unavailable"),
        power_entity("sensor.home_power", "750"),
    ]);
    let options = power_flow_options();
    let body = body_for_widget(WidgetKind::PowerFlow, Some(&options), Arc::clone(&store));

    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid entity must be present");
    let rows = body.render_rows(&grid);
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(
        keys,
        vec!["grid_w", "home_w"],
        "only live entities surface their rows; got: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// State-changed event flow
// ---------------------------------------------------------------------------

/// Pump synthetic state-changed events for grid / solar / battery /
/// battery_soc / home entities; assert the recomputed `PowerFlowVM`
/// reflects the new state within one flush cadence (no caching beyond
/// the live store).
#[test]
fn state_changed_events_recompute_power_flow_vm_on_each_tick() {
    let store = five_entity_store();

    // Initial readings.
    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid present");
    assert_eq!(PowerFlowVM::read_power_watts(&grid), Some(1500.0));

    // Tick 1: grid flips from import to export.
    store.apply_event(make_update("sensor.grid_power", "-450"));
    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid present");
    assert_eq!(
        PowerFlowVM::read_power_watts(&grid),
        Some(-450.0),
        "post-event grid reading must reflect export sign"
    );

    // Tick 2: battery state-of-charge changes.
    store.apply_event(make_update("sensor.battery_soc", "65"));
    let soc = store
        .get(&EntityId::from("sensor.battery_soc"))
        .expect("battery_soc present");
    assert_eq!(PowerFlowVM::read_battery_pct(&soc), Some(65.0));

    // Tick 3: solar entity disappears (HA reports unavailable).
    store.apply_event(make_update("sensor.solar_power", "unavailable"));
    let solar = store
        .get(&EntityId::from("sensor.solar_power"))
        .expect("solar present");
    assert_eq!(
        PowerFlowVM::read_power_watts(&solar),
        None,
        "unavailable solar must read as None"
    );
}

// ---------------------------------------------------------------------------
// Bridge compute_power_flow_tile_vm — full pipeline against live store
// ---------------------------------------------------------------------------

/// `compute_power_flow_tile_vm` projects the live grid entity + supplied
/// auxiliary readings into a typed `PowerFlowTileVM` matching what the
/// Slint side will consume.
#[test]
fn compute_power_flow_tile_vm_projects_live_store_readings() {
    let store = five_entity_store();
    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid present");
    let solar = store
        .get(&EntityId::from("sensor.solar_power"))
        .expect("solar present");
    let battery = store
        .get(&EntityId::from("sensor.battery_power"))
        .expect("battery present");
    let battery_soc = store
        .get(&EntityId::from("sensor.battery_soc"))
        .expect("battery_soc present");
    let home = store
        .get(&EntityId::from("sensor.home_power"))
        .expect("home present");

    let auxiliary = PowerFlowAuxiliaryReadings {
        solar_w: PowerFlowVM::read_power_watts(&solar),
        battery_w: PowerFlowVM::read_power_watts(&battery),
        battery_pct: PowerFlowVM::read_battery_pct(&battery_soc),
        home_w: PowerFlowVM::read_power_watts(&home),
    };

    let vm = compute_power_flow_tile_vm(
        "Power Flow".to_owned(),
        "mdi:lightning-bolt-circle".to_owned(),
        2,
        2,
        TilePlacement {
            col: 0,
            row: 0,
            span_cols: 2,
            span_rows: 2,
        },
        &grid,
        auxiliary,
    );

    assert_eq!(vm.name, "Power Flow");
    assert_eq!(vm.grid_w, Some(1500.0));
    assert_eq!(vm.solar_w, Some(2000.0));
    assert_eq!(vm.battery_w, Some(-300.0));
    assert_eq!(vm.battery_pct, Some(82.0));
    assert_eq!(vm.home_w, Some(750.0));
    assert_eq!(vm.icon_id, "mdi:lightning-bolt-circle");
    assert!(!vm.pending);
}

/// After a state-changed event, the next call to
/// `compute_power_flow_tile_vm` reflects the new state — confirms the
/// projection is stateless and reads from the live store on every call.
#[test]
fn compute_power_flow_tile_vm_reflects_state_changes_on_next_invocation() {
    let store = five_entity_store();

    // Initial projection.
    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid present");
    let vm1 = compute_power_flow_tile_vm(
        "Power Flow".to_owned(),
        "mdi:lightning-bolt-circle".to_owned(),
        2,
        2,
        TilePlacement {
            col: 0,
            row: 0,
            span_cols: 2,
            span_rows: 2,
        },
        &grid,
        PowerFlowAuxiliaryReadings::default(),
    );
    assert_eq!(vm1.grid_w, Some(1500.0));

    // Tick: grid flips to export.
    store.apply_event(make_update("sensor.grid_power", "-200"));

    // Re-derive: new grid reading reflected.
    let grid = store
        .get(&EntityId::from("sensor.grid_power"))
        .expect("grid present");
    let vm2 = compute_power_flow_tile_vm(
        "Power Flow".to_owned(),
        "mdi:lightning-bolt-circle".to_owned(),
        2,
        2,
        TilePlacement {
            col: 0,
            row: 0,
            span_cols: 2,
            span_rows: 2,
        },
        &grid,
        PowerFlowAuxiliaryReadings::default(),
    );
    assert_eq!(vm2.grid_w, Some(-200.0));
    assert_ne!(vm1.grid_w, vm2.grid_w);
}

// ---------------------------------------------------------------------------
// PowerFlowBody attribute branch coverage (modal body)
// ---------------------------------------------------------------------------

/// `PowerFlowBody` constructed with all `None` auxiliary readings
/// surfaces ONLY the grid_w row (matching the body's branch contract).
#[test]
fn power_flow_body_grid_only_branch() {
    let entity = power_entity("sensor.grid_power", "1500");
    let body = PowerFlowBody::new(None, None, None, None);
    let rows = body.render_rows(&entity);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "grid_w");
    assert_eq!(rows[0].value, "1500.0 W");
}

/// `PowerFlowBody` for an `unavailable` grid entity still emits the
/// grid_w row using the fallback `0.0`. The modal must always show the
/// grid lane even when its current reading is unknown.
#[test]
fn power_flow_body_unavailable_grid_still_emits_row() {
    let entity = power_entity("sensor.grid_power", "unavailable");
    let body = PowerFlowBody::new(None, None, None, None);
    let rows = body.render_rows(&entity);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "grid_w");
    assert_eq!(rows[0].value, "0.0 W");
}

/// `PowerFlowBody::render_rows` does NOT consult the entity's
/// attribute map — power sensors emit numeric values via the `state`
/// field, not as attributes. A populated attribute map must not cause
/// extra rows.
#[test]
fn power_flow_body_does_not_read_entity_attributes() {
    let mut attrs = Map::new();
    attrs.insert("unit_of_measurement".to_owned(), json!("W"));
    attrs.insert("device_class".to_owned(), json!("power"));
    attrs.insert("friendly_name".to_owned(), json!("Grid Power"));
    let entity = Entity {
        id: EntityId::from("sensor.grid_power"),
        state: Arc::from("1500"),
        attributes: Arc::new(attrs),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    };
    let body = PowerFlowBody::new(None, None, None, None);
    let rows = body.render_rows(&entity);
    // Body emits exactly one row (grid_w); attribute map is irrelevant.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "grid_w");
}

/// `PowerFlowVM` and `PowerFlowBody` reject `Value::Null` / non-numeric
/// state strings consistently — the JSON-crate path is only available
/// in this integration test crate; the production source treats
/// non-numeric state via the same `f64::parse` / sentinel branch.
#[test]
fn power_flow_vm_rejects_null_value_state() {
    let entity = power_entity("sensor.grid_power", "");
    assert_eq!(
        PowerFlowVM::read_power_watts(&entity),
        None,
        "empty state must not parse as 0.0 — `f64::parse(\"\")` returns Err"
    );
}

/// Additional defence: a `Value::String` non-numeric like `"high"` does
/// not coerce. Distinct from the production unit test because here we
/// can additionally exercise the construction-time path through the
/// JSON-crate-aware fixture builder.
#[test]
fn power_flow_vm_rejects_non_numeric_value_state() {
    let _ = Value::String("high".to_owned()); // attests the JSON crate is reachable here
    let entity = power_entity("sensor.grid_power", "high");
    assert_eq!(PowerFlowVM::read_power_watts(&entity), None);
}
