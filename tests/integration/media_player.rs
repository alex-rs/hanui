//! Phase 6 acceptance integration tests for the media player widget
//! (TASK-112).
//!
//! Exercises the MediaPlayerVM → bridge projection pipeline against a live
//! store seeded with synthetic `media_player.*` entities. Per-VM unit tests
//! live in `src/ui/media_player.rs::tests`; this file is the cross-component
//! layer required by TASK-112 acceptance criterion #2.
//!
//! # Playback state coverage
//!
//! `MediaPlayerVM::is_playing` is true only for HA state `"playing"`.
//! `volume_level` clamps out-of-range values (0..=1). `media_title` /
//! `artist` are surfaced verbatim.

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
use hanui::ui::bridge::{compute_media_player_tile_vm, TilePlacement};
use hanui::ui::media_player::{
    read_album_attribute, read_sound_mode_attribute, read_source_attribute,
    read_volume_level_attribute, MediaPlayerVM,
};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn mp_entity(id: &str, state: &str, attrs: Map<String, serde_json::Value>) -> Entity {
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
// Idle / playing / unavailable rendering
// ---------------------------------------------------------------------------

#[test]
fn idle_playing_unavailable_renders_distinct_vms() {
    let mut playing_attrs = Map::new();
    playing_attrs.insert("media_title".to_owned(), json!("Song A"));
    playing_attrs.insert("media_artist".to_owned(), json!("Artist X"));
    playing_attrs.insert("volume_level".to_owned(), json!(0.5));

    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        mp_entity("media_player.idle", "idle", Map::new()),
        mp_entity("media_player.playing", "playing", playing_attrs),
        mp_entity("media_player.unavailable", "unavailable", Map::new()),
    ]);

    let idle = store.get(&EntityId::from("media_player.idle")).unwrap();
    let playing = store.get(&EntityId::from("media_player.playing")).unwrap();
    let unavail = store
        .get(&EntityId::from("media_player.unavailable"))
        .unwrap();

    let idle_vm = compute_media_player_tile_vm(
        "Speaker".to_owned(),
        "mdi:speaker".to_owned(),
        2,
        2,
        placement(),
        &idle,
    );
    let playing_vm = compute_media_player_tile_vm(
        "Speaker".to_owned(),
        "mdi:speaker".to_owned(),
        2,
        2,
        placement(),
        &playing,
    );
    let unavail_vm = compute_media_player_tile_vm(
        "Speaker".to_owned(),
        "mdi:speaker".to_owned(),
        2,
        2,
        placement(),
        &unavail,
    );

    assert_eq!(idle_vm.state, "idle");
    assert!(!idle_vm.is_playing);
    assert!(idle_vm.media_title.is_none());

    assert_eq!(playing_vm.state, "playing");
    assert!(playing_vm.is_playing);
    assert_eq!(playing_vm.media_title.as_deref(), Some("Song A"));
    assert_eq!(playing_vm.artist.as_deref(), Some("Artist X"));
    assert!(playing_vm.volume_level.is_some());
    let v = playing_vm.volume_level.unwrap();
    assert!((v - 0.5).abs() < 0.01);

    assert_eq!(unavail_vm.state, "unavailable");
    assert!(!unavail_vm.is_playing);
}

// ---------------------------------------------------------------------------
// Playback state transitions
// ---------------------------------------------------------------------------

#[test]
fn state_changed_events_drive_is_playing_transitions() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![mp_entity("media_player.tv", "off", Map::new())]);

    // off → on (powered on, no playback).
    store.apply_event(make_update("media_player.tv", "on"));
    let on = store.get(&EntityId::from("media_player.tv")).unwrap();
    assert!(!MediaPlayerVM::from_entity(&on).is_playing);

    // → playing.
    store.apply_event(make_update("media_player.tv", "playing"));
    let playing = store.get(&EntityId::from("media_player.tv")).unwrap();
    let vm = MediaPlayerVM::from_entity(&playing);
    assert!(vm.is_playing);
    assert_eq!(vm.state, "playing");

    // → paused.
    store.apply_event(make_update("media_player.tv", "paused"));
    let paused = store.get(&EntityId::from("media_player.tv")).unwrap();
    let vm = MediaPlayerVM::from_entity(&paused);
    assert!(!vm.is_playing);
    assert_eq!(vm.state, "paused");

    // → buffering (still not "playing" — distinct loading state).
    store.apply_event(make_update("media_player.tv", "buffering"));
    let buf = store.get(&EntityId::from("media_player.tv")).unwrap();
    assert!(!MediaPlayerVM::from_entity(&buf).is_playing);
}

// ---------------------------------------------------------------------------
// Volume level — out-of-range values clamp to 0..=1
// ---------------------------------------------------------------------------

#[test]
fn volume_level_clamps_out_of_range_values() {
    let mut over = Map::new();
    over.insert("volume_level".to_owned(), json!(1.5));
    let entity_over = mp_entity("media_player.over", "playing", over);
    let v = read_volume_level_attribute(&entity_over).expect("must clamp, not drop");
    assert!(
        (v - 1.0).abs() < 0.001,
        "above-1.0 must clamp to 1.0; got {v}"
    );

    let mut under = Map::new();
    under.insert("volume_level".to_owned(), json!(-0.5));
    let entity_under = mp_entity("media_player.under", "playing", under);
    let v = read_volume_level_attribute(&entity_under).expect("must clamp, not drop");
    assert!(v.abs() < 0.001, "below-0.0 must clamp to 0.0; got {v}");
}

// ---------------------------------------------------------------------------
// MediaPlayerBody attribute accessors
// ---------------------------------------------------------------------------

#[test]
fn media_player_body_attribute_accessors_round_trip() {
    let mut attrs = Map::new();
    attrs.insert("source".to_owned(), json!("HDMI 1"));
    attrs.insert("sound_mode".to_owned(), json!("Movie"));
    attrs.insert("media_album_name".to_owned(), json!("Greatest Hits"));
    let entity = mp_entity("media_player.full", "playing", attrs);

    assert_eq!(read_source_attribute(&entity).as_deref(), Some("HDMI 1"));
    assert_eq!(read_sound_mode_attribute(&entity).as_deref(), Some("Movie"));
    assert_eq!(
        read_album_attribute(&entity).as_deref(),
        Some("Greatest Hits")
    );

    let bare = mp_entity("media_player.bare", "idle", Map::new());
    assert_eq!(read_source_attribute(&bare), None);
    assert_eq!(read_sound_mode_attribute(&bare), None);
    assert_eq!(read_album_attribute(&bare), None);
}
