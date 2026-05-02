//! Integration smoke test for TASK-016.
//!
//! Verifies that the fixture loads cleanly and that each Phase 1 tile kind
//! (light, sensor, entity) is represented by at least one entity in the store.
//! `MainWindow` is intentionally NOT instantiated here — constructing a Slint
//! window requires a live X display / graphics backend, which is unavailable
//! in headless CI runners. The data-side path (fixture → MemoryStore → get)
//! is what this test exercises.
//!
//! # Coverage scope
//!
//! - `ha::fixture::load` returns `Ok` for the canonical fixture.
//! - `MemoryStore::get` returns `Some` for at least one entity per Phase 1 tile
//!   kind (`light.*`, `sensor.*`, switch/binary_sensor as entity-tile proxies).
//! - A fixture entity with empty attributes (`binary_sensor.foo`) does not panic.

use hanui::ha::entity::EntityId;
use hanui::ha::fixture;
use hanui::ha::store::EntityStore;

/// Path to the canonical Phase 1 fixture relative to the crate root.
///
/// Cargo integration tests run with `cwd` set to the workspace root, so this
/// path resolves identically to how `fixture::load` is called in `lib.rs`.
const FIXTURE_PATH: &str = "examples/ha-states.json";

// ---------------------------------------------------------------------------
// Fixture loads without error
// ---------------------------------------------------------------------------

#[test]
fn fixture_loads_successfully() {
    fixture::load(FIXTURE_PATH).expect("canonical fixture must load without error");
}

// ---------------------------------------------------------------------------
// At least one entity per Phase 1 tile kind
// ---------------------------------------------------------------------------

#[test]
fn fixture_contains_at_least_one_light_entity() {
    let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
    let light = store.get(&EntityId::from("light.kitchen"));
    assert!(
        light.is_some(),
        "fixture must contain at least one light entity (light.kitchen)"
    );
}

#[test]
fn fixture_contains_at_least_one_sensor_entity() {
    let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
    let sensor = store.get(&EntityId::from("sensor.hallway_temperature"));
    assert!(
        sensor.is_some(),
        "fixture must contain at least one sensor entity (sensor.hallway_temperature)"
    );
}

#[test]
fn fixture_contains_at_least_one_entity_tile_proxy() {
    // Phase 1's entity_tile covers non-light/sensor domains.
    // switch.outlet_1 serves as the canonical entity-tile proxy in the fixture.
    let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
    let entity = store.get(&EntityId::from("switch.outlet_1"));
    assert!(
        entity.is_some(),
        "fixture must contain at least one entity-tile proxy (switch.outlet_1)"
    );
}

// ---------------------------------------------------------------------------
// Empty-attributes entity does not panic (acceptance criterion #4)
// ---------------------------------------------------------------------------

#[test]
fn empty_attributes_entity_does_not_panic() {
    let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
    // binary_sensor.foo has an empty attributes map; accessing it must not panic.
    let entity = store
        .get(&EntityId::from("binary_sensor.foo"))
        .expect("binary_sensor.foo must be present in fixture");
    assert!(
        entity.attributes.is_empty(),
        "binary_sensor.foo must have empty attributes"
    );
    // Accessing absent friendly_name via the bridge's unwrap_or path must not panic.
    let friendly_name = entity
        .attributes
        .get("friendly_name")
        .and_then(|v| v.as_str());
    assert_eq!(
        friendly_name, None,
        "friendly_name must be absent for binary_sensor.foo"
    );
    // Fallback to entity ID is the expected name — verify the chain does not panic.
    let display_name = friendly_name.unwrap_or(entity.id.as_ref());
    assert_eq!(
        display_name, "binary_sensor.foo",
        "entity ID must be the fallback display name"
    );
}

// ---------------------------------------------------------------------------
// Store visitor covers all fixture entities
// ---------------------------------------------------------------------------

#[test]
fn for_each_visits_all_fixture_entities() {
    let store = fixture::load(FIXTURE_PATH).expect("fixture must load");
    let mut count = 0usize;
    store.for_each(&mut |_id, _entity| {
        count += 1;
    });
    // The canonical fixture covers every Phase 6 widget kind and carries
    // 18 entities. Bump this constant when the fixture grows again.
    assert_eq!(
        count, 18,
        "canonical fixture must contain exactly 18 entities"
    );
}
