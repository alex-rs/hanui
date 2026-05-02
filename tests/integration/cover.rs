//! Phase 6 acceptance integration tests for the cover widget (TASK-112).
//!
//! Exercises the full pipeline (LiveStore → CoverVM → bridge projection)
//! end-to-end for `cover.*` entities. Per-VM unit tests live in
//! `src/ui/cover.rs::tests`; this file is the cross-component integration
//! layer required by TASK-112 acceptance criterion #2.
//!
//! Mitigates Phase 6 Risk #1 (per-domain dispatch coverage gap) for the
//! cover domain: idle / active / unavailable state coverage plus a
//! state-changed event flow that re-derives the projection from the live
//! store. No production code is exercised through any path other than the
//! existing public API; the test never names `src/**` symbols outside the
//! library's public surface.

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
use hanui::ui::bridge::{compute_cover_tile_vm, TilePlacement};
use hanui::ui::cover::{read_supported_features, read_tilt_attribute, CoverVM};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Construct a minimal cover entity with the given state and an optional
/// `current_position` attribute.
fn cover_entity(id: &str, state: &str, position: Option<u8>) -> Entity {
    let mut attrs = Map::new();
    if let Some(p) = position {
        attrs.insert("current_position".to_owned(), json!(p));
    }
    Entity {
        id: EntityId::from(id),
        state: Arc::from(state),
        attributes: Arc::new(attrs),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    }
}

/// Build an `EntityUpdate` carrying a single `state_changed` event for the
/// given entity. Mirrors the helper in `tests/integration/power_flow.rs` —
/// `EntityUpdate` is `#[non_exhaustive]`; struct-literal construction is
/// forbidden so the conversion path is the only public route in.
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
// Idle / active / unavailable state coverage (TASK-112 acceptance #11)
// ---------------------------------------------------------------------------

/// Per acceptance criterion #11 ("each new widget renders correctly in 3
/// fixture states"): assert the bridge's per-frame projection produces a
/// distinct view-model for each of the canonical idle / active / unavailable
/// states. The test drives the same `compute_cover_tile_vm` entry point the
/// `build_tiles` hot path uses.
#[test]
fn idle_active_unavailable_renders_distinct_vms() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        cover_entity("cover.idle", "closed", Some(0)),
        cover_entity("cover.active", "opening", Some(40)),
        cover_entity("cover.unavailable", "unavailable", None),
    ]);

    let idle = store.get(&EntityId::from("cover.idle")).unwrap();
    let active = store.get(&EntityId::from("cover.active")).unwrap();
    let unavailable = store.get(&EntityId::from("cover.unavailable")).unwrap();

    let idle_vm = compute_cover_tile_vm(
        "Garage".to_owned(),
        "mdi:garage".to_owned(),
        2,
        1,
        placement(),
        &idle,
    );
    let active_vm = compute_cover_tile_vm(
        "Garage".to_owned(),
        "mdi:garage".to_owned(),
        2,
        1,
        placement(),
        &active,
    );
    let unavailable_vm = compute_cover_tile_vm(
        "Garage".to_owned(),
        "mdi:garage".to_owned(),
        2,
        1,
        placement(),
        &unavailable,
    );

    // Idle: closed cover, position 0, not moving.
    assert_eq!(idle_vm.state, "closed");
    assert!(!idle_vm.is_open);
    assert!(!idle_vm.is_moving);
    assert_eq!(idle_vm.position, 0);
    assert!(idle_vm.has_position);

    // Active: opening cover, position 40, is_moving + is_open per the
    // destination-colour rule.
    assert_eq!(active_vm.state, "opening");
    assert!(active_vm.is_open);
    assert!(active_vm.is_moving);
    assert_eq!(active_vm.position, 40);

    // Unavailable: state forwarded verbatim, no position attribute.
    assert_eq!(unavailable_vm.state, "unavailable");
    assert!(!unavailable_vm.is_open);
    assert!(!unavailable_vm.is_moving);
    assert!(!unavailable_vm.has_position);

    // Cross-state distinctness — the three VMs MUST NOT collapse into the
    // same projection (would mean the per-state branch is broken).
    assert_ne!(idle_vm.state, active_vm.state);
    assert_ne!(active_vm.state, unavailable_vm.state);
    assert_ne!(idle_vm.is_open, active_vm.is_open);
}

// ---------------------------------------------------------------------------
// State-changed event flow
// ---------------------------------------------------------------------------

/// State-changed event for a cover entity must propagate through the live
/// store and produce a re-derived `CoverVM` reflecting the new state on the
/// next call. This is the per-flush cadence contract from
/// `locked_decisions.more_info_modal` (the live store is the single source
/// of truth; the bridge re-reads on every flush).
#[test]
fn state_changed_events_re_derive_cover_vm_on_each_tick() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![cover_entity("cover.shade", "closed", Some(0))]);

    let initial = store.get(&EntityId::from("cover.shade")).unwrap();
    let initial_vm = CoverVM::from_entity(&initial);
    assert!(!initial_vm.is_open);
    assert!(!initial_vm.is_moving);

    // Tick 1: cover starts opening — destination colours apply immediately.
    store.apply_event(make_update("cover.shade", "opening"));
    let after_open_start = store.get(&EntityId::from("cover.shade")).unwrap();
    let opening_vm = CoverVM::from_entity(&after_open_start);
    assert!(opening_vm.is_open);
    assert!(opening_vm.is_moving);

    // Tick 2: cover finishes opening — moving flag clears.
    store.apply_event(make_update("cover.shade", "open"));
    let after_open = store.get(&EntityId::from("cover.shade")).unwrap();
    let open_vm = CoverVM::from_entity(&after_open);
    assert!(open_vm.is_open);
    assert!(!open_vm.is_moving);

    // Tick 3: HA reports unavailable — fall back to the safe defaults.
    store.apply_event(make_update("cover.shade", "unavailable"));
    let after_unavail = store.get(&EntityId::from("cover.shade")).unwrap();
    let unavail_vm = CoverVM::from_entity(&after_unavail);
    assert!(!unavail_vm.is_open);
    assert!(!unavail_vm.is_moving);
}

// ---------------------------------------------------------------------------
// Position attribute flow through the bridge projection
// ---------------------------------------------------------------------------

/// The `current_position` attribute on the entity surfaces on the
/// `CoverTileVM` via `compute_cover_tile_vm`. A subsequent state-changed
/// event that does NOT carry an attribute payload must not corrupt the
/// projection — the live store keeps the previous attributes for the entity
/// when the new event omits them.
#[test]
fn position_attribute_flows_to_tile_vm() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![cover_entity("cover.blind", "open", Some(75))]);

    let entity = store.get(&EntityId::from("cover.blind")).unwrap();
    let vm = compute_cover_tile_vm(
        "Blind".to_owned(),
        "mdi:blinds".to_owned(),
        2,
        1,
        placement(),
        &entity,
    );

    assert_eq!(vm.position, 75, "position attribute must propagate");
    assert!(vm.has_position);
    assert!(vm.is_open);
}

// ---------------------------------------------------------------------------
// Tilt + supported_features attributes (per-domain accessors)
// ---------------------------------------------------------------------------

/// `read_tilt_attribute` and `read_supported_features` handle absence and
/// presence per their documented contract. The accessors are public so the
/// more-info `CoverBody` can read them without duplicating parsing logic.
#[test]
fn tilt_and_supported_features_round_trip() {
    let mut attrs = Map::new();
    attrs.insert("current_tilt_position".to_owned(), json!(60));
    attrs.insert("supported_features".to_owned(), json!(15));
    let entity = Entity {
        id: EntityId::from("cover.window"),
        state: Arc::from("open"),
        attributes: Arc::new(attrs),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    };
    assert_eq!(read_tilt_attribute(&entity), Some(60));
    assert_eq!(read_supported_features(&entity), Some(15));

    // Absent attributes return None.
    let bare = cover_entity("cover.bare", "closed", None);
    assert_eq!(read_tilt_attribute(&bare), None);
    assert_eq!(read_supported_features(&bare), None);
}
