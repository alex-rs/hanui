//! Phase 6 acceptance integration tests for the visibility predicate
//! evaluator (TASK-110, TASK-112).
//!
//! Pumps synthetic state-changed events through a populated `LiveStore`
//! and asserts that `dashboard::visibility::evaluate` flips its result
//! deterministically as predicate-relevant entities change. The unit-level
//! coverage lives in `src/dashboard/visibility.rs::tests`; this file is the
//! cross-component integration layer required by TASK-112 acceptance
//! criterion #3 (visibility-flip end-to-end).
//!
//! # Predicate vocabulary covered
//!
//! Per `locked_decisions.visibility_predicate_vocabulary`:
//!   * `always` / `never`
//!   * `entity_available:<id>` (Phase 4 alias)
//!   * `state_equals:<id>:<v>` (Phase 4 alias)
//!   * `<id> == <v>` / `<id> != <v>` (Phase 6)
//!   * `<id> in [<v1>,<v2>,...]` (Phase 6)
//!   * `entity_state_numeric:<id>:<op>:<N>` (Phase 6)
//!   * `profile:<key>`
//!
//! # Dep-index round trip
//!
//! `build_dep_index` produces an `EntityId → Vec<WidgetId>` reverse map.
//! The test below builds a Dashboard with mixed predicate forms, asserts
//! the bucket assignments match the documented per-form dependency
//! extraction, and that always/never/profile predicates contribute no
//! entries.

use std::sync::Arc;

use hanui::actions::map::WidgetId;
use hanui::dashboard::schema::{
    Dashboard, Layout, ProfileKey, Section, SectionGrid, View, Widget, WidgetKind, WidgetLayout,
};
use hanui::dashboard::visibility::{build_dep_index, evaluate};
use hanui::ha::client::event_to_entity_update;
use hanui::ha::entity::{Entity, EntityId};
use hanui::ha::live_store::LiveStore;
use hanui::ha::protocol::{
    EventPayload, EventVariant, RawEntityState, StateChangedData, StateChangedEvent,
};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn entity(id: &str, state: &str) -> Entity {
    Entity {
        id: EntityId::from(id),
        state: Arc::from(state),
        attributes: Arc::new(serde_json::Map::new()),
        last_changed: jiff::Timestamp::UNIX_EPOCH,
        last_updated: jiff::Timestamp::UNIX_EPOCH,
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

fn widget(id: &str, visibility: &str, entity_id: Option<&str>) -> Widget {
    Widget {
        id: id.to_owned(),
        widget_type: WidgetKind::SensorTile,
        entity: entity_id.map(str::to_owned),
        entities: vec![],
        name: None,
        icon: None,
        visibility: visibility.to_owned(),
        tap_action: None,
        hold_action: None,
        double_tap_action: None,
        layout: WidgetLayout {
            preferred_columns: 1,
            preferred_rows: 1,
        },
        options: None,
        placement: None,
    }
}

fn dashboard_with_widgets(widgets: Vec<Widget>) -> Dashboard {
    Dashboard {
        version: 1,
        device_profile: ProfileKey::Desktop,
        home_assistant: None,
        theme: None,
        default_view: "home".to_owned(),
        views: vec![View {
            id: "home".to_owned(),
            title: "Home".to_owned(),
            layout: Layout::Sections,
            sections: vec![Section {
                id: "main".to_owned(),
                title: "Main".to_owned(),
                grid: SectionGrid::default(),
                widgets,
            }],
        }],
        call_service_allowlist: Arc::default(),
        dep_index: Arc::default(),
    }
}

// ---------------------------------------------------------------------------
// flip_no_flicker_end_to_end
// ---------------------------------------------------------------------------

/// Synthetic state-changed events drive a predicate's truth value
/// deterministically through the live store. The Phase 4 alias form
/// `state_equals:<id>:<v>` and the Phase 6 canonical `<id> == <v>` form
/// MUST agree on every transition — that is the alias contract documented
/// on `evaluate`.
#[test]
fn flip_no_flicker_end_to_end() {
    let store = LiveStore::new();
    store.apply_snapshot(vec![entity("binary_sensor.motion", "off")]);

    let primary = EntityId::from("binary_sensor.motion");
    let canonical = "binary_sensor.motion == on";
    let alias = "state_equals:binary_sensor.motion:on";

    // Initial: motion off → predicate false.
    assert!(!evaluate(canonical, &primary, &store, ProfileKey::Desktop));
    assert!(!evaluate(alias, &primary, &store, ProfileKey::Desktop));

    // Tick: motion → on. Predicate flips to true.
    store.apply_event(make_update("binary_sensor.motion", "on"));
    assert!(evaluate(canonical, &primary, &store, ProfileKey::Desktop));
    assert!(evaluate(alias, &primary, &store, ProfileKey::Desktop));

    // Tick: motion → off. Predicate flips back.
    store.apply_event(make_update("binary_sensor.motion", "off"));
    assert!(!evaluate(canonical, &primary, &store, ProfileKey::Desktop));
    assert!(!evaluate(alias, &primary, &store, ProfileKey::Desktop));

    // Tick: motion → unavailable. The `== on` predicate stays false; the
    // `entity_available:` alias drops to false.
    store.apply_event(make_update("binary_sensor.motion", "unavailable"));
    assert!(!evaluate(canonical, &primary, &store, ProfileKey::Desktop));
    assert!(!evaluate(
        "entity_available:binary_sensor.motion",
        &primary,
        &store,
        ProfileKey::Desktop,
    ));
}

// ---------------------------------------------------------------------------
// in-list predicate
// ---------------------------------------------------------------------------

#[test]
fn in_list_predicate_flips_with_state_changes() {
    let store = LiveStore::new();
    store.apply_snapshot(vec![entity("media_player.tv", "off")]);

    let primary = EntityId::from("media_player.tv");
    let pred = "media_player.tv in [playing,paused,buffering]";

    assert!(!evaluate(pred, &primary, &store, ProfileKey::Desktop));

    for state in ["playing", "paused", "buffering"] {
        store.apply_event(make_update("media_player.tv", state));
        assert!(
            evaluate(pred, &primary, &store, ProfileKey::Desktop),
            "state {state} must satisfy predicate {pred}"
        );
    }

    store.apply_event(make_update("media_player.tv", "off"));
    assert!(!evaluate(pred, &primary, &store, ProfileKey::Desktop));
}

// ---------------------------------------------------------------------------
// numeric predicate
// ---------------------------------------------------------------------------

#[test]
fn numeric_predicate_flips_at_threshold() {
    let store = LiveStore::new();
    store.apply_snapshot(vec![entity("sensor.power", "100")]);

    let primary = EntityId::from("sensor.power");
    let pred = "entity_state_numeric:sensor.power:gt:200";

    // Initial 100 → not > 200.
    assert!(!evaluate(pred, &primary, &store, ProfileKey::Desktop));

    // Cross threshold.
    store.apply_event(make_update("sensor.power", "300"));
    assert!(evaluate(pred, &primary, &store, ProfileKey::Desktop));

    // Drop to non-numeric (HA emits "unavailable") — predicate returns
    // false; the evaluator never panics.
    store.apply_event(make_update("sensor.power", "unavailable"));
    assert!(!evaluate(pred, &primary, &store, ProfileKey::Desktop));
}

// ---------------------------------------------------------------------------
// always / never / profile — entity-independent forms
// ---------------------------------------------------------------------------

#[test]
fn entity_independent_predicates() {
    let store = LiveStore::new();
    let primary = EntityId::from("anything");
    assert!(evaluate("always", &primary, &store, ProfileKey::Desktop));
    assert!(!evaluate("never", &primary, &store, ProfileKey::Desktop));

    assert!(evaluate(
        "profile:desktop",
        &primary,
        &store,
        ProfileKey::Desktop
    ));
    assert!(!evaluate(
        "profile:rpi4",
        &primary,
        &store,
        ProfileKey::Desktop
    ));
}

// ---------------------------------------------------------------------------
// Dependency index round-trip
// ---------------------------------------------------------------------------

#[test]
fn build_dep_index_collects_all_phase6_predicate_forms() {
    let widgets = vec![
        widget("w_canon_eq", "light.k == on", Some("light.k")),
        widget("w_canon_neq", "light.k != off", Some("light.k")),
        widget(
            "w_in_list",
            "media_player.tv in [playing,paused]",
            Some("media_player.tv"),
        ),
        widget(
            "w_numeric",
            "entity_state_numeric:sensor.power:gt:100",
            Some("sensor.power"),
        ),
        widget(
            "w_avail_alias",
            "entity_available:lock.front",
            Some("lock.front"),
        ),
        widget(
            "w_eq_alias",
            "state_equals:binary_sensor.motion:on",
            Some("binary_sensor.motion"),
        ),
        widget("w_always", "always", None),
        widget("w_never", "never", None),
        widget("w_profile", "profile:desktop", None),
    ];
    let dashboard = dashboard_with_widgets(widgets);
    let index = build_dep_index(&dashboard);

    // Per-entity buckets:
    let light_bucket = index
        .get(&EntityId::from("light.k"))
        .expect("light.k bucket");
    assert_eq!(light_bucket.len(), 2);
    assert!(light_bucket.contains(&WidgetId::from("w_canon_eq")));
    assert!(light_bucket.contains(&WidgetId::from("w_canon_neq")));

    assert_eq!(
        index
            .get(&EntityId::from("media_player.tv"))
            .map(Vec::len)
            .unwrap_or(0),
        1
    );
    assert_eq!(
        index
            .get(&EntityId::from("sensor.power"))
            .map(Vec::len)
            .unwrap_or(0),
        1
    );
    assert_eq!(
        index
            .get(&EntityId::from("lock.front"))
            .map(Vec::len)
            .unwrap_or(0),
        1
    );
    assert_eq!(
        index
            .get(&EntityId::from("binary_sensor.motion"))
            .map(Vec::len)
            .unwrap_or(0),
        1
    );

    // always / never / profile contribute no entries.
    assert_eq!(
        index.len(),
        5,
        "5 buckets (one per dependent entity); got: {:?}",
        index.keys().collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// visibility_flip.yaml fixture — deserialise + dep_index extraction
// ---------------------------------------------------------------------------

/// The TASK-106 fixture `tests/layout/visibility_flip.yaml` exercises a
/// minimal predicate-gated dashboard. This test confirms it parses
/// cleanly through the schema and that its predicate's dependency is
/// extracted into the dep_index.
#[test]
fn visibility_flip_fixture_parses_and_indexes() {
    let yaml = std::fs::read_to_string("tests/layout/visibility_flip.yaml")
        .expect("visibility_flip.yaml must exist");
    let dashboard: Dashboard =
        serde_yaml_ng::from_str(&yaml).expect("visibility_flip.yaml must parse");

    let index = build_dep_index(&dashboard);
    let bucket = index
        .get(&EntityId::from("binary_sensor.motion"))
        .expect("binary_sensor.motion bucket must be present");
    assert_eq!(bucket.len(), 1, "exactly one widget gates on this entity");
    assert!(bucket.contains(&WidgetId::from("gated")));
}
