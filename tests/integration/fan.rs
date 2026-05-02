//! Phase 6 acceptance integration tests for the fan widget (TASK-112).
//!
//! Exercises the FanVM → bridge projection pipeline against a live store
//! seeded with synthetic fan entities. Per-VM unit tests live in
//! `src/ui/fan.rs::tests`; this file is the cross-component layer required
//! by TASK-112 acceptance criterion #2.
//!
//! Mitigates Phase 6 Risk #1 for the fan domain: idle / active / unavailable
//! coverage plus a state-changed event flow that re-derives the `FanVM`
//! including `speed_pct` and `current_speed` (preset_mode) on each tick.

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
use hanui::ui::bridge::{compute_fan_tile_vm, TilePlacement};
use hanui::ui::fan::{read_direction_attribute, read_oscillating_attribute, FanVM};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn fan_entity(id: &str, state: &str, attrs: Map<String, serde_json::Value>) -> Entity {
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
// Idle / active / unavailable state coverage
// ---------------------------------------------------------------------------

#[test]
fn idle_active_unavailable_renders_distinct_vms() {
    let mut active_attrs = Map::new();
    active_attrs.insert("percentage".to_owned(), json!(75));
    active_attrs.insert("preset_mode".to_owned(), json!("High"));

    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        fan_entity("fan.idle", "off", Map::new()),
        fan_entity("fan.active", "on", active_attrs),
        fan_entity("fan.unavailable", "unavailable", Map::new()),
    ]);

    let idle = store.get(&EntityId::from("fan.idle")).unwrap();
    let active = store.get(&EntityId::from("fan.active")).unwrap();
    let unavailable = store.get(&EntityId::from("fan.unavailable")).unwrap();

    let idle_vm = compute_fan_tile_vm(
        "Bedroom".to_owned(),
        "mdi:fan".to_owned(),
        2,
        1,
        placement(),
        &idle,
    );
    let active_vm = compute_fan_tile_vm(
        "Bedroom".to_owned(),
        "mdi:fan".to_owned(),
        2,
        1,
        placement(),
        &active,
    );
    let unavailable_vm = compute_fan_tile_vm(
        "Bedroom".to_owned(),
        "mdi:fan".to_owned(),
        2,
        1,
        placement(),
        &unavailable,
    );

    // Idle: off, no speed, no preset.
    assert_eq!(idle_vm.state, "off");
    assert!(!idle_vm.is_on);
    assert!(!idle_vm.has_speed_pct);
    assert!(!idle_vm.has_current_speed);

    // Active: on, speed 75, preset "High".
    assert_eq!(active_vm.state, "on");
    assert!(active_vm.is_on);
    assert!(active_vm.has_speed_pct);
    assert_eq!(active_vm.speed_pct, 75);
    assert!(active_vm.has_current_speed);
    assert_eq!(active_vm.current_speed, "High");

    // Unavailable: state forwarded; is_on=false.
    assert_eq!(unavailable_vm.state, "unavailable");
    assert!(!unavailable_vm.is_on);

    // Distinctness sanity.
    assert_ne!(idle_vm.is_on, active_vm.is_on);
    assert_ne!(active_vm.state, unavailable_vm.state);
}

// ---------------------------------------------------------------------------
// State-changed event flow
// ---------------------------------------------------------------------------

#[test]
fn state_changed_events_re_derive_fan_vm_on_each_tick() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![fan_entity("fan.kitchen", "off", Map::new())]);

    let initial = store.get(&EntityId::from("fan.kitchen")).unwrap();
    assert!(!FanVM::from_entity(&initial).is_on);

    // Tick 1: fan turns on.
    store.apply_event(make_update("fan.kitchen", "on"));
    let on_state = store.get(&EntityId::from("fan.kitchen")).unwrap();
    assert!(FanVM::from_entity(&on_state).is_on);

    // Tick 2: integration reports auto mode (also on).
    store.apply_event(make_update("fan.kitchen", "auto"));
    let auto_state = store.get(&EntityId::from("fan.kitchen")).unwrap();
    let auto_vm = FanVM::from_entity(&auto_state);
    assert!(auto_vm.is_on);
    assert_eq!(auto_vm.state, "auto");

    // Tick 3: fan goes unavailable.
    store.apply_event(make_update("fan.kitchen", "unavailable"));
    let unavail = store.get(&EntityId::from("fan.kitchen")).unwrap();
    assert!(!FanVM::from_entity(&unavail).is_on);
}

// ---------------------------------------------------------------------------
// speed_pct attribute flow through the bridge projection
// ---------------------------------------------------------------------------

#[test]
fn speed_pct_flows_to_tile_vm() {
    let mut attrs = Map::new();
    attrs.insert("percentage".to_owned(), json!(50));
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![fan_entity("fan.bath", "on", attrs)]);

    let entity = store.get(&EntityId::from("fan.bath")).unwrap();
    let vm = compute_fan_tile_vm(
        "Bath".to_owned(),
        "mdi:fan".to_owned(),
        2,
        1,
        placement(),
        &entity,
    );
    assert_eq!(vm.speed_pct, 50);
    assert!(vm.has_speed_pct);
    assert!(vm.is_on);
}

// ---------------------------------------------------------------------------
// preset_mode attribute flow
// ---------------------------------------------------------------------------

#[test]
fn preset_mode_flows_to_tile_vm() {
    let mut attrs = Map::new();
    attrs.insert("preset_mode".to_owned(), json!("Low"));
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![fan_entity("fan.attic", "on", attrs)]);

    let entity = store.get(&EntityId::from("fan.attic")).unwrap();
    let vm = compute_fan_tile_vm(
        "Attic".to_owned(),
        "mdi:fan".to_owned(),
        2,
        1,
        placement(),
        &entity,
    );
    assert!(vm.has_current_speed);
    assert_eq!(vm.current_speed, "Low");
    assert!(
        !vm.has_speed_pct,
        "no `percentage` attribute → no speed_pct"
    );
}

// ---------------------------------------------------------------------------
// Oscillating + direction accessors used by FanBody
// ---------------------------------------------------------------------------

#[test]
fn oscillating_and_direction_accessors_round_trip() {
    let mut attrs = Map::new();
    attrs.insert("oscillating".to_owned(), json!(true));
    attrs.insert("direction".to_owned(), json!("forward"));
    let entity = fan_entity("fan.full", "on", attrs);
    assert_eq!(read_oscillating_attribute(&entity), Some(true));
    assert_eq!(
        read_direction_attribute(&entity).as_deref(),
        Some("forward")
    );

    let bare = fan_entity("fan.bare", "on", Map::new());
    assert_eq!(read_oscillating_attribute(&bare), None);
    assert_eq!(read_direction_attribute(&bare), None);
}
