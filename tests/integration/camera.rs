//! Phase 6 acceptance integration tests for the camera widget (TASK-112).
//!
//! Exercises the CameraVM → bridge projection pipeline against a live store
//! seeded with synthetic camera entities. The HTTP fetch path is exercised
//! by `tests/integration/camera_pool.rs` (existing); this file focuses on
//! the per-frame VM and the per-domain attribute accessors used by the
//! more-info `CameraBody`.
//!
//! Mitigates Phase 6 Risk #1 for the camera domain: idle / recording /
//! streaming / unavailable rendering plus the snapshot-URL accessor's
//! security contract (URL is reachable as a string but the more-info body
//! is responsible for not logging it — the accessor itself returns the raw
//! value as documented).

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
use hanui::ui::bridge::{compute_camera_tile_vm, TilePlacement};
use hanui::ui::camera::{
    read_brand_attribute, read_entity_picture_attribute, read_friendly_name_attribute,
    read_last_motion_attribute, CameraVM,
};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn camera_entity(id: &str, state: &str, attrs: Map<String, serde_json::Value>) -> Entity {
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
        span_rows: 2,
    }
}

// ---------------------------------------------------------------------------
// Idle / recording / streaming / unavailable rendering
// ---------------------------------------------------------------------------

#[test]
fn idle_recording_streaming_unavailable_renders_distinct_vms() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        camera_entity("camera.idle", "idle", Map::new()),
        camera_entity("camera.recording", "recording", Map::new()),
        camera_entity("camera.streaming", "streaming", Map::new()),
        camera_entity("camera.unavailable", "unavailable", Map::new()),
    ]);

    let cases: &[(&str, bool, bool, bool)] = &[
        ("camera.idle", false, false, true),
        ("camera.recording", true, false, true),
        ("camera.streaming", false, true, true),
        ("camera.unavailable", false, false, false),
    ];

    for (id, expect_recording, expect_streaming, expect_available) in cases {
        let entity = store.get(&EntityId::from(*id)).unwrap();
        let vm = compute_camera_tile_vm(
            "Front Door".to_owned(),
            "mdi:cctv".to_owned(),
            2,
            2,
            placement(),
            &entity,
        );
        assert_eq!(
            vm.is_recording, *expect_recording,
            "is_recording for `{id}` must be {expect_recording}"
        );
        assert_eq!(
            vm.is_streaming, *expect_streaming,
            "is_streaming for `{id}` must be {expect_streaming}"
        );
        assert_eq!(
            vm.is_available, *expect_available,
            "is_available for `{id}` must be {expect_available}"
        );
    }
}

// ---------------------------------------------------------------------------
// State-changed event flow
// ---------------------------------------------------------------------------

#[test]
fn state_changed_events_re_derive_camera_vm() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![camera_entity("camera.front", "idle", Map::new())]);

    // idle → streaming → recording → unavailable
    store.apply_event(make_update("camera.front", "streaming"));
    let streaming = store.get(&EntityId::from("camera.front")).unwrap();
    let s_vm = CameraVM::from_entity(&streaming);
    assert!(s_vm.is_streaming);
    assert!(!s_vm.is_recording);

    store.apply_event(make_update("camera.front", "recording"));
    let recording = store.get(&EntityId::from("camera.front")).unwrap();
    let r_vm = CameraVM::from_entity(&recording);
    assert!(!r_vm.is_streaming);
    assert!(r_vm.is_recording);

    store.apply_event(make_update("camera.front", "unavailable"));
    let unavail = store.get(&EntityId::from("camera.front")).unwrap();
    assert!(!CameraVM::from_entity(&unavail).is_available);
}

// ---------------------------------------------------------------------------
// Snapshot-URL attribute — accessor returns raw URL, caller must not log
// ---------------------------------------------------------------------------

/// Per `src/ui/camera.rs::read_entity_picture_attribute` doc-comment, the
/// snapshot URL may embed a short-lived access token and must NOT appear in
/// tracing output. The accessor returns the raw string so the caller can
/// decide what to surface; the security gate is on the caller side. We
/// pin the round-trip here to confirm the accessor reads the attribute.
#[test]
fn snapshot_url_accessor_returns_raw_value() {
    let mut attrs = Map::new();
    attrs.insert(
        "entity_picture".to_owned(),
        json!("/api/camera_proxy/camera.front_door?token=abcd1234"),
    );
    let entity = camera_entity("camera.front", "idle", attrs);
    let url = read_entity_picture_attribute(&entity).expect("url must be present");
    assert!(url.starts_with("/api/camera_proxy/"));
    assert!(url.contains("token="));
}

// ---------------------------------------------------------------------------
// CameraBody attribute accessors
// ---------------------------------------------------------------------------

#[test]
fn camera_attribute_accessors_round_trip() {
    let mut attrs = Map::new();
    attrs.insert("last_motion".to_owned(), json!("2024-01-01T12:00:00Z"));
    attrs.insert("brand".to_owned(), json!("Nest"));
    attrs.insert("friendly_name".to_owned(), json!("Front Door Cam"));
    let entity = camera_entity("camera.front", "idle", attrs);

    assert_eq!(
        read_last_motion_attribute(&entity).as_deref(),
        Some("2024-01-01T12:00:00Z")
    );
    assert_eq!(read_brand_attribute(&entity).as_deref(), Some("Nest"));
    assert_eq!(
        read_friendly_name_attribute(&entity).as_deref(),
        Some("Front Door Cam")
    );

    let bare = camera_entity("camera.bare", "idle", Map::new());
    assert_eq!(read_last_motion_attribute(&bare), None);
    assert_eq!(read_brand_attribute(&bare), None);
    assert_eq!(read_friendly_name_attribute(&bare), None);
}
