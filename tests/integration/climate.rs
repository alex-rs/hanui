//! Phase 6 acceptance integration tests for the climate widget (TASK-112).
//!
//! Exercises the ClimateVM → bridge projection pipeline against a live
//! store seeded with synthetic `climate.*` entities. Per-VM unit tests live
//! in `src/ui/climate.rs::tests`; this file is the cross-component layer
//! required by TASK-112 acceptance criterion #2.
//!
//! # HVAC mode + setpoint flow
//!
//! `ClimateVM::from_entity` reads the `current_temperature` and HA's
//! setpoint attribute (`temperature`, NOT `target_temperature` — the wire
//! attribute is named `temperature`). The bridge projects these into the
//! per-frame `ClimateTileVM`. State changes drive `is_active` (any non-off,
//! non-unavailable mode); the typed VM stays scalar per the no-Vec
//! invariant.

use std::sync::Arc;

use jiff::Timestamp;
use serde_json::{json, Map};

use hanui::ha::client::event_to_entity_update;
use hanui::ha::entity::{Entity, EntityId};
use hanui::ha::live_store::LiveStore;
use hanui::ha::protocol::{
    EventPayload, EventVariant, RawEntityState, StateChangedData, StateChangedEvent,
};
use hanui::ha::store::EntityStore;
use hanui::ui::bridge::{compute_climate_tile_vm, TilePlacement};
use hanui::ui::climate::{
    read_current_temperature_attribute, read_fan_mode_attribute, read_humidity_attribute,
    read_swing_mode_attribute, read_target_temperature_attribute, ClimateVM,
};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn climate_entity(id: &str, state: &str, attrs: Map<String, serde_json::Value>) -> Entity {
    Entity {
        id: EntityId::from(id),
        state: Arc::from(state),
        attributes: Arc::new(attrs),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    }
}

fn make_update(entity_id: &str, new_state: &str) -> hanui::ha::store::EntityUpdate {
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

fn placement() -> TilePlacement {
    TilePlacement {
        col: 0,
        row: 0,
        span_cols: 2,
        span_rows: 1,
    }
}

// ---------------------------------------------------------------------------
// Idle (off) / active (heat) / unavailable rendering
// ---------------------------------------------------------------------------

#[test]
fn idle_active_unavailable_renders_distinct_vms() {
    let mut active_attrs = Map::new();
    active_attrs.insert("current_temperature".to_owned(), json!(20.5));
    active_attrs.insert("temperature".to_owned(), json!(22.0));

    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        climate_entity("climate.idle", "off", Map::new()),
        climate_entity("climate.active", "heat", active_attrs),
        climate_entity("climate.unavailable", "unavailable", Map::new()),
    ]);

    let idle = store.get(&EntityId::from("climate.idle")).unwrap();
    let active = store.get(&EntityId::from("climate.active")).unwrap();
    let unavail = store.get(&EntityId::from("climate.unavailable")).unwrap();

    let idle_vm = compute_climate_tile_vm(
        "Living".to_owned(),
        "mdi:thermostat".to_owned(),
        2,
        1,
        placement(),
        &idle,
    );
    let active_vm = compute_climate_tile_vm(
        "Living".to_owned(),
        "mdi:thermostat".to_owned(),
        2,
        1,
        placement(),
        &active,
    );
    let unavail_vm = compute_climate_tile_vm(
        "Living".to_owned(),
        "mdi:thermostat".to_owned(),
        2,
        1,
        placement(),
        &unavail,
    );

    // Idle (off): not active, no temperatures.
    assert!(!idle_vm.is_active);
    assert_eq!(idle_vm.state, "off");
    assert!(idle_vm.current_temperature.is_none());
    assert!(idle_vm.target_temperature.is_none());

    // Active (heat): is_active, both temperatures populated.
    assert!(active_vm.is_active);
    assert_eq!(active_vm.state, "heat");
    assert!(active_vm.current_temperature.is_some());
    assert!(active_vm.target_temperature.is_some());
    let curr = active_vm.current_temperature.unwrap();
    let target = active_vm.target_temperature.unwrap();
    assert!((curr - 20.5).abs() < 0.01);
    assert!((target - 22.0).abs() < 0.01);

    // Unavailable: not active, state forwarded.
    assert!(!unavail_vm.is_active);
    assert_eq!(unavail_vm.state, "unavailable");
}

// ---------------------------------------------------------------------------
// HVAC mode flow
// ---------------------------------------------------------------------------

#[test]
fn hvac_mode_state_changes_drive_is_active() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![climate_entity("climate.bedroom", "off", Map::new())]);

    let off_vm = ClimateVM::from_entity(&store.get(&EntityId::from("climate.bedroom")).unwrap());
    assert!(!off_vm.is_active);

    for mode in ["heat", "cool", "auto", "heat_cool", "dry", "fan_only"] {
        store.apply_event(make_update("climate.bedroom", mode));
        let entity = store.get(&EntityId::from("climate.bedroom")).unwrap();
        let vm = ClimateVM::from_entity(&entity);
        assert!(vm.is_active, "mode `{mode}` must produce is_active=true");
        assert_eq!(vm.state, mode);
    }

    // Back to off → not active.
    store.apply_event(make_update("climate.bedroom", "off"));
    let final_vm = ClimateVM::from_entity(&store.get(&EntityId::from("climate.bedroom")).unwrap());
    assert!(!final_vm.is_active);
}

// ---------------------------------------------------------------------------
// Setpoint attribute flow
// ---------------------------------------------------------------------------

#[test]
fn current_and_target_temperature_attributes_flow() {
    let mut attrs = Map::new();
    attrs.insert("current_temperature".to_owned(), json!(18.5));
    attrs.insert("temperature".to_owned(), json!(21.0));
    let entity = climate_entity("climate.kitchen", "heat", attrs);

    assert_eq!(read_current_temperature_attribute(&entity), Some(18.5));
    assert_eq!(read_target_temperature_attribute(&entity), Some(21.0));
}

// ---------------------------------------------------------------------------
// Heat-cool dual-setpoint mode — `temperature` may be absent
// ---------------------------------------------------------------------------

/// In `heat_cool` mode, HA emits `target_temp_low` / `target_temp_high`
/// instead of a single `temperature` setpoint. The `temperature` accessor
/// returns `None` and the VM still renders the active state without falsely
/// implying a single-target setpoint.
#[test]
fn heat_cool_mode_without_temperature_attribute() {
    let mut attrs = Map::new();
    attrs.insert("current_temperature".to_owned(), json!(20.0));
    // No `temperature` attribute — heat_cool mode reports target_temp_low/high.
    attrs.insert("target_temp_low".to_owned(), json!(18.0));
    attrs.insert("target_temp_high".to_owned(), json!(24.0));
    let entity = climate_entity("climate.dual", "heat_cool", attrs);

    let vm = ClimateVM::from_entity(&entity);
    assert!(vm.is_active);
    assert_eq!(vm.current_temperature, Some(20.0));
    assert!(
        vm.target_temperature.is_none(),
        "heat_cool mode without `temperature` attribute → target_temperature=None"
    );
}

// ---------------------------------------------------------------------------
// Humidity / fan_mode / swing_mode accessors used by ClimateBody
// ---------------------------------------------------------------------------

#[test]
fn climate_body_attribute_accessors_round_trip() {
    let mut attrs = Map::new();
    attrs.insert("humidity".to_owned(), json!(45));
    attrs.insert("fan_mode".to_owned(), json!("auto"));
    attrs.insert("swing_mode".to_owned(), json!("vertical"));
    let entity = climate_entity("climate.full", "cool", attrs);

    assert_eq!(read_humidity_attribute(&entity), Some(45.0));
    assert_eq!(read_fan_mode_attribute(&entity).as_deref(), Some("auto"));
    assert_eq!(
        read_swing_mode_attribute(&entity).as_deref(),
        Some("vertical")
    );

    let bare = climate_entity("climate.bare", "off", Map::new());
    assert_eq!(read_humidity_attribute(&bare), None);
    assert_eq!(read_fan_mode_attribute(&bare), None);
    assert_eq!(read_swing_mode_attribute(&bare), None);
}
