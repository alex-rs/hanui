//! Phase 6 acceptance integration tests for the lock widget (TASK-112).
//!
//! Exercises the LockVM → bridge projection pipeline against a live store
//! seeded with synthetic lock entities. Per-VM unit tests live in
//! `src/ui/lock.rs::tests`; this file is the cross-component layer required
//! by TASK-112 acceptance criterion #2.
//!
//! Mitigates Phase 6 Risk #1 for the lock domain: idle (locked) / active
//! (unlocked) / jammed / unavailable coverage plus a state-changed event
//! flow.

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
use hanui::ui::bridge::{compute_lock_tile_vm, TilePlacement};
use hanui::ui::lock::{read_battery_level_attribute, read_code_format_attribute, LockVM};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn lock_entity(id: &str, state: &str, attrs: Map<String, serde_json::Value>) -> Entity {
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
// Idle / active / jammed / unavailable coverage
// ---------------------------------------------------------------------------

#[test]
fn idle_active_jammed_unavailable_renders_distinct_vms() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        lock_entity("lock.idle", "locked", Map::new()),
        lock_entity("lock.active", "unlocked", Map::new()),
        lock_entity("lock.jammed", "jammed", Map::new()),
        lock_entity("lock.unavailable", "unavailable", Map::new()),
    ]);

    let cases: &[(&str, bool, &str)] = &[
        ("lock.idle", true, "locked"),
        ("lock.active", false, "unlocked"),
        ("lock.jammed", false, "jammed"),
        ("lock.unavailable", false, "unavailable"),
    ];

    for (id, expected_locked, expected_state) in cases {
        let entity = store.get(&EntityId::from(*id)).unwrap();
        let vm = compute_lock_tile_vm(
            "Front Door".to_owned(),
            "mdi:lock".to_owned(),
            2,
            1,
            placement(),
            &entity,
        );
        assert_eq!(
            vm.is_locked, *expected_locked,
            "is_locked for `{id}` must be {expected_locked}"
        );
        assert_eq!(
            vm.state, *expected_state,
            "state for `{id}` must forward verbatim"
        );
    }
}

// ---------------------------------------------------------------------------
// Jammed handling — distinct VM, not collapsed onto unavailable or unlocked
// ---------------------------------------------------------------------------

#[test]
fn jammed_state_is_distinct_from_unlocked_and_unavailable() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        lock_entity("lock.j", "jammed", Map::new()),
        lock_entity("lock.u", "unlocked", Map::new()),
        lock_entity("lock.x", "unavailable", Map::new()),
    ]);

    let j = LockVM::from_entity(&store.get(&EntityId::from("lock.j")).unwrap());
    let u = LockVM::from_entity(&store.get(&EntityId::from("lock.u")).unwrap());
    let x = LockVM::from_entity(&store.get(&EntityId::from("lock.x")).unwrap());

    assert!(!j.is_locked);
    assert!(!u.is_locked);
    assert!(!x.is_locked);

    let j_vm = compute_lock_tile_vm(
        "x".to_owned(),
        "i".to_owned(),
        1,
        1,
        placement(),
        &store.get(&EntityId::from("lock.j")).unwrap(),
    );
    let x_vm = compute_lock_tile_vm(
        "x".to_owned(),
        "i".to_owned(),
        1,
        1,
        placement(),
        &store.get(&EntityId::from("lock.x")).unwrap(),
    );
    // The state strings differ — the Slint tile uses these for the
    // jammed-versus-unavailable colour branch.
    assert_eq!(j_vm.state, "jammed");
    assert_eq!(x_vm.state, "unavailable");
    assert_ne!(j_vm.state, x_vm.state);
}

// ---------------------------------------------------------------------------
// State-changed event flow
// ---------------------------------------------------------------------------

#[test]
fn state_changed_events_re_derive_lock_vm() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![lock_entity("lock.front", "locked", Map::new())]);

    assert!(LockVM::from_entity(&store.get(&EntityId::from("lock.front")).unwrap()).is_locked);

    // Unlocking transition — destination colours apply (is_locked=false for
    // "unlocking" per the documented mapping).
    store.apply_event(make_update("lock.front", "unlocking"));
    let unlocking = store.get(&EntityId::from("lock.front")).unwrap();
    assert!(!LockVM::from_entity(&unlocking).is_locked);

    // Settled unlocked.
    store.apply_event(make_update("lock.front", "unlocked"));
    let unlocked = store.get(&EntityId::from("lock.front")).unwrap();
    assert!(!LockVM::from_entity(&unlocked).is_locked);

    // Locking back up — destination colours apply (is_locked=true).
    store.apply_event(make_update("lock.front", "locking"));
    let locking = store.get(&EntityId::from("lock.front")).unwrap();
    assert!(LockVM::from_entity(&locking).is_locked);
}

// ---------------------------------------------------------------------------
// Battery + code_format accessors used by LockBody
// ---------------------------------------------------------------------------

#[test]
fn battery_and_code_format_accessors_round_trip() {
    let mut attrs = Map::new();
    attrs.insert("battery_level".to_owned(), json!(72));
    attrs.insert("code_format".to_owned(), json!("number"));
    let entity = lock_entity("lock.smart", "locked", attrs);

    assert_eq!(read_battery_level_attribute(&entity), Some(72));
    assert_eq!(
        read_code_format_attribute(&entity).as_deref(),
        Some("number")
    );

    let bare = lock_entity("lock.bare", "locked", Map::new());
    assert_eq!(read_battery_level_attribute(&bare), None);
    assert_eq!(read_code_format_attribute(&bare), None);
}
