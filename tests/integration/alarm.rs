//! Phase 6 acceptance integration tests for the alarm panel widget (TASK-112).
//!
//! Exercises the AlarmVM → bridge projection pipeline against a live store
//! seeded with synthetic `alarm_control_panel.*` entities. Per-VM unit tests
//! live in `src/ui/alarm.rs::tests`; this file is the cross-component layer
//! required by TASK-112 acceptance criterion #2.
//!
//! # RequiredOnDisarm flow
//!
//! Per Phase 6 acceptance criterion (PIN-policy entry path), an alarm panel
//! whose `WidgetOptions::Alarm.pin_policy = PinPolicy::RequiredOnDisarm`
//! triggers PIN entry on disarm transitions. The dispatcher-side wiring
//! lives in `src/actions/dispatcher.rs`; this integration test verifies the
//! schema flow — the validator accepts the policy on Alarm widgets and the
//! per-frame VM correctly mirrors the `armed_*` → `disarmed` transition.

use std::sync::Arc;

use jiff::Timestamp;
use serde_json::{json, Map};

use hanui::dashboard::profiles::PROFILE_DESKTOP;
use hanui::dashboard::schema::{
    CodeFormat, Dashboard, Layout, PinPolicy, ProfileKey, Section, SectionGrid, Severity, View,
    Widget, WidgetKind, WidgetLayout, WidgetOptions,
};
use hanui::dashboard::validate;
use hanui::ha::client::event_to_entity_update;
use hanui::ha::entity::{Entity, EntityId};
use hanui::ha::live_store::LiveStore;
use hanui::ha::protocol::{
    EventPayload, EventVariant, RawEntityState, StateChangedData, StateChangedEvent,
};
use hanui::ha::store::EntityStore;
use hanui::ui::alarm::{
    read_changed_by_attribute, read_code_arm_required_attribute, read_code_format_attribute,
    AlarmVM,
};
use hanui::ui::bridge::{compute_alarm_tile_vm, TilePlacement};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn alarm_entity(id: &str, state: &str, attrs: Map<String, serde_json::Value>) -> Entity {
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
// Idle / active / unavailable coverage (3 fixture states)
// ---------------------------------------------------------------------------

#[test]
fn idle_active_unavailable_renders_distinct_vms() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        alarm_entity("alarm_control_panel.idle", "disarmed", Map::new()),
        alarm_entity("alarm_control_panel.active", "armed_away", Map::new()),
        alarm_entity("alarm_control_panel.unavailable", "unavailable", Map::new()),
    ]);

    let idle = store
        .get(&EntityId::from("alarm_control_panel.idle"))
        .unwrap();
    let active = store
        .get(&EntityId::from("alarm_control_panel.active"))
        .unwrap();
    let unavailable = store
        .get(&EntityId::from("alarm_control_panel.unavailable"))
        .unwrap();

    let idle_vm = compute_alarm_tile_vm(
        "Home Alarm".to_owned(),
        "mdi:shield".to_owned(),
        2,
        1,
        placement(),
        &idle,
    );
    let active_vm = compute_alarm_tile_vm(
        "Home Alarm".to_owned(),
        "mdi:shield".to_owned(),
        2,
        1,
        placement(),
        &active,
    );
    let unavail_vm = compute_alarm_tile_vm(
        "Home Alarm".to_owned(),
        "mdi:shield".to_owned(),
        2,
        1,
        placement(),
        &unavailable,
    );

    assert_eq!(idle_vm.state, "disarmed");
    assert!(!idle_vm.is_armed);
    assert!(!idle_vm.is_triggered);
    assert!(!idle_vm.is_pending);

    assert_eq!(active_vm.state, "armed_away");
    assert!(active_vm.is_armed);
    assert!(!active_vm.is_triggered);

    assert_eq!(unavail_vm.state, "unavailable");
    assert!(!unavail_vm.is_armed);
}

// ---------------------------------------------------------------------------
// Triggered + pending — distinct from `armed_*` and `disarmed`
// ---------------------------------------------------------------------------

#[test]
fn triggered_and_pending_are_distinct_from_armed() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![
        alarm_entity("alarm_control_panel.x", "triggered", Map::new()),
        alarm_entity("alarm_control_panel.y", "pending", Map::new()),
    ]);

    let triggered =
        AlarmVM::from_entity(&store.get(&EntityId::from("alarm_control_panel.x")).unwrap());
    let pending =
        AlarmVM::from_entity(&store.get(&EntityId::from("alarm_control_panel.y")).unwrap());

    assert!(triggered.is_triggered);
    assert!(!triggered.is_armed);
    assert!(!triggered.is_pending);

    assert!(pending.is_pending);
    assert!(!pending.is_armed);
    assert!(!pending.is_triggered);
}

// ---------------------------------------------------------------------------
// State transition flow
// ---------------------------------------------------------------------------

#[test]
fn state_changed_events_drive_arm_disarm_transitions() {
    let store = Arc::new(LiveStore::new());
    store.apply_snapshot(vec![alarm_entity(
        "alarm_control_panel.home",
        "disarmed",
        Map::new(),
    )]);

    // Disarmed initial.
    let initial = store
        .get(&EntityId::from("alarm_control_panel.home"))
        .unwrap();
    assert!(!AlarmVM::from_entity(&initial).is_armed);

    // Arm: armed_away.
    store.apply_event(make_update("alarm_control_panel.home", "armed_away"));
    let armed = store
        .get(&EntityId::from("alarm_control_panel.home"))
        .unwrap();
    let armed_vm = AlarmVM::from_entity(&armed);
    assert!(armed_vm.is_armed);
    assert_eq!(armed_vm.state, "armed_away");

    // Triggered.
    store.apply_event(make_update("alarm_control_panel.home", "triggered"));
    let triggered = store
        .get(&EntityId::from("alarm_control_panel.home"))
        .unwrap();
    let triggered_vm = AlarmVM::from_entity(&triggered);
    assert!(!triggered_vm.is_armed, "triggered is NOT armed");
    assert!(triggered_vm.is_triggered);

    // Disarm transition begins (HA emits `pending`).
    store.apply_event(make_update("alarm_control_panel.home", "pending"));
    let pending = store
        .get(&EntityId::from("alarm_control_panel.home"))
        .unwrap();
    let pending_vm = AlarmVM::from_entity(&pending);
    assert!(pending_vm.is_pending);
    assert!(!pending_vm.is_armed);
    assert!(!pending_vm.is_triggered);

    // Disarmed.
    store.apply_event(make_update("alarm_control_panel.home", "disarmed"));
    let disarmed = store
        .get(&EntityId::from("alarm_control_panel.home"))
        .unwrap();
    assert!(!AlarmVM::from_entity(&disarmed).is_armed);
}

// ---------------------------------------------------------------------------
// RequiredOnDisarm policy — schema-side validation flow
// ---------------------------------------------------------------------------

/// `PinPolicy::RequiredOnDisarm` is valid on `WidgetOptions::Alarm` and
/// passes validation cleanly. The same policy on `WidgetOptions::Lock` is
/// asserted as Error elsewhere (`tests/integration/validation.rs`); here we
/// pin the positive case for Alarm.
#[test]
fn required_on_disarm_policy_validates_on_alarm_widget() {
    let dashboard = Dashboard {
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
                widgets: vec![Widget {
                    id: "alarm1".to_owned(),
                    widget_type: WidgetKind::Alarm,
                    entity: Some("alarm_control_panel.home".to_owned()),
                    entities: vec![],
                    name: None,
                    icon: None,
                    visibility: "always".to_owned(),
                    tap_action: None,
                    hold_action: None,
                    double_tap_action: None,
                    layout: WidgetLayout {
                        preferred_columns: 2,
                        preferred_rows: 1,
                    },
                    options: Some(WidgetOptions::Alarm {
                        pin_policy: PinPolicy::RequiredOnDisarm {
                            length: 4,
                            code_format: CodeFormat::Number,
                        },
                    }),
                    placement: None,
                }],
            }],
        }],
        call_service_allowlist: Arc::default(),
        dep_index: Arc::default(),
    };

    let (issues, _allowlist) = validate::validate(&dashboard, &PROFILE_DESKTOP);
    let errors: Vec<_> = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "PinPolicy::RequiredOnDisarm on Alarm must validate clean; got: {errors:?}"
    );
}

// ---------------------------------------------------------------------------
// Attribute accessors used by AlarmBody
// ---------------------------------------------------------------------------

#[test]
fn alarm_attribute_accessors_round_trip() {
    let mut attrs = Map::new();
    attrs.insert("changed_by".to_owned(), json!("Master"));
    attrs.insert("code_format".to_owned(), json!("number"));
    attrs.insert("code_arm_required".to_owned(), json!(true));
    let entity = alarm_entity("alarm_control_panel.full", "disarmed", attrs);

    assert_eq!(
        read_changed_by_attribute(&entity).as_deref(),
        Some("Master")
    );
    assert_eq!(
        read_code_format_attribute(&entity).as_deref(),
        Some("number")
    );
    assert_eq!(read_code_arm_required_attribute(&entity), Some(true));

    let bare = alarm_entity("alarm_control_panel.bare", "disarmed", Map::new());
    assert_eq!(read_changed_by_attribute(&bare), None);
    assert_eq!(read_code_format_attribute(&bare), None);
    assert_eq!(read_code_arm_required_attribute(&bare), None);
}
