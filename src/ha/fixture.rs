//! Fixture loader: parses `examples/ha-states.json` into typed [`Entity`]
//! records and populates a [`MemoryStore`].
//!
//! # Hot-path discipline
//!
//! `serde_json::Value` is allowed **only** inside [`RawFixtureEntry`] —
//! the deserialization boundary.  Once [`load`] converts each
//! `RawFixtureEntry` to an [`Entity`], all downstream code works with
//! typed structs.  No `Value` traversal happens after the parse step.
//!
//! # JSON format
//!
//! The fixture file is a JSON array (`Vec<RawFixtureEntry>`) mirroring the
//! shape returned by the HA REST `/api/states` endpoint:
//!
//! ```json
//! [
//!   {
//!     "entity_id": "light.kitchen",
//!     "state": "on",
//!     "attributes": { "brightness": 180 },
//!     "last_changed": "2026-04-24T10:00:00Z",
//!     "last_updated": "2026-04-24T10:00:00Z"
//!   }
//! ]
//! ```

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::ha::entity::{Entity, EntityId};
use crate::ha::store::{MemoryStore, MemoryStoreError};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur while loading a fixture file.
#[derive(Debug, Error)]
pub enum FixtureError {
    /// The fixture file could not be read from disk.
    #[error("IO error reading fixture: {0}")]
    Io(#[from] std::io::Error),

    /// The fixture JSON could not be deserialized into `Vec<RawFixtureEntry>`.
    #[error("JSON parse error in fixture: {0}")]
    Json(#[from] serde_json::Error),

    /// A `last_changed` or `last_updated` timestamp string is not valid ISO 8601.
    #[error("invalid timestamp '{value}' in entity '{entity_id}': {source}")]
    Timestamp {
        entity_id: String,
        value: String,
        source: jiff::Error,
    },

    /// The fixture contains more entities than `DEFAULT_PROFILE.max_entities`.
    #[error("{0}")]
    Capacity(#[from] MemoryStoreError),
}

// ---------------------------------------------------------------------------
// RawFixtureEntry — the ONLY place serde_json::Value is permitted
// ---------------------------------------------------------------------------

/// Deserialization struct mirroring the HA REST `/api/states` element shape.
///
/// This is the **only** place in the codebase where `serde_json::Value` is
/// stored.  After [`load`] converts each entry to [`Entity`], `Value` never
/// appears downstream.
#[derive(Debug, Deserialize)]
pub struct RawFixtureEntry {
    pub entity_id: String,
    pub state: String,
    /// Arbitrary JSON attributes as received from HA.  The map is moved
    /// directly into `Entity::attributes` without further traversal.
    pub attributes: Map<String, Value>,
    pub last_changed: String,
    pub last_updated: String,
}

impl RawFixtureEntry {
    /// Convert this raw entry to a typed [`Entity`], parsing timestamps.
    ///
    /// Returns [`FixtureError::Timestamp`] if either timestamp string is not
    /// valid ISO 8601.
    fn into_entity(self) -> Result<Entity, FixtureError> {
        let last_changed =
            Timestamp::from_str(&self.last_changed).map_err(|e| FixtureError::Timestamp {
                entity_id: self.entity_id.clone(),
                value: self.last_changed.clone(),
                source: e,
            })?;

        let last_updated =
            Timestamp::from_str(&self.last_updated).map_err(|e| FixtureError::Timestamp {
                entity_id: self.entity_id.clone(),
                value: self.last_updated.clone(),
                source: e,
            })?;

        Ok(Entity {
            id: EntityId::from(self.entity_id.as_str()),
            state: Arc::from(self.state.as_str()),
            attributes: Arc::new(self.attributes),
            last_changed,
            last_updated,
        })
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a fixture file at `path` and populate a [`MemoryStore`].
///
/// The fixture must be a JSON array of objects whose shape matches
/// [`RawFixtureEntry`] (i.e. the HA REST `/api/states` response).
///
/// # Errors
///
/// - [`FixtureError::Io`] if the file cannot be read.
/// - [`FixtureError::Json`] if the file is not valid JSON or the schema
///   does not match `Vec<RawFixtureEntry>`.
/// - [`FixtureError::Timestamp`] if any `last_changed`/`last_updated` value
///   is not a valid ISO 8601 timestamp.
/// - [`FixtureError::Capacity`] if the fixture exceeds
///   `DEFAULT_PROFILE.max_entities` (currently {DEFAULT_PROFILE.max_entities}).
pub fn load(path: impl AsRef<Path>) -> Result<MemoryStore, FixtureError> {
    let bytes = std::fs::read(path)?;
    let raw: Vec<RawFixtureEntry> = serde_json::from_slice(&bytes)?;

    let entities: Vec<Entity> = raw
        .into_iter()
        .map(RawFixtureEntry::into_entity)
        .collect::<Result<_, _>>()?;

    let store = MemoryStore::load(entities)?;
    Ok(store)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use jiff::Timestamp;
    use serde_json::Map;

    use super::*;
    use crate::dashboard::profiles::DEFAULT_PROFILE;
    use crate::ha::store::EntityStore;

    /// Path to the canonical Phase 1 fixture relative to the crate root.
    ///
    /// `cargo test` runs with the crate root as cwd, so
    /// `examples/ha-states.json` resolves correctly.
    const FIXTURE_PATH: &str = "examples/ha-states.json";

    // -----------------------------------------------------------------------
    // Happy path: load the canonical Phase 1 fixture
    // -----------------------------------------------------------------------

    #[test]
    fn load_fixture_returns_store_with_expected_entities() {
        let store = load(FIXTURE_PATH).expect("fixture must load successfully");

        // Each of the four Phase 1 required entities must be present.
        let light = store.get(&EntityId::from("light.kitchen"));
        assert!(light.is_some(), "light.kitchen must be in fixture");
        assert_eq!(&*light.unwrap().state, "on");

        let sensor = store.get(&EntityId::from("sensor.hallway_temperature"));
        assert!(
            sensor.is_some(),
            "sensor.hallway_temperature must be in fixture"
        );
        assert_eq!(&*sensor.unwrap().state, "21.3");

        let generic = store.get(&EntityId::from("switch.outlet_1"));
        assert!(generic.is_some(), "switch.outlet_1 must be in fixture");

        let empty_attrs = store.get(&EntityId::from("binary_sensor.foo"));
        assert!(
            empty_attrs.is_some(),
            "binary_sensor.foo must be in fixture"
        );
        let empty_entity = empty_attrs.unwrap();
        assert_eq!(&*empty_entity.state, "off");
        assert!(
            empty_entity.attributes.is_empty(),
            "binary_sensor.foo must have empty attributes"
        );
    }

    #[test]
    fn load_fixture_parses_timestamps() {
        let store = load(FIXTURE_PATH).expect("fixture must load successfully");
        let sensor = store
            .get(&EntityId::from("sensor.hallway_temperature"))
            .expect("sensor must be present");
        // Timestamps must not be UNIX_EPOCH (i.e. they were actually parsed).
        assert_ne!(
            sensor.last_changed,
            Timestamp::UNIX_EPOCH,
            "last_changed must be parsed from ISO 8601, not left as epoch"
        );
        assert_ne!(
            sensor.last_updated,
            Timestamp::UNIX_EPOCH,
            "last_updated must be parsed from ISO 8601, not left as epoch"
        );
    }

    #[test]
    fn load_fixture_light_has_brightness_attribute() {
        let store = load(FIXTURE_PATH).expect("fixture must load successfully");
        let light = store
            .get(&EntityId::from("light.kitchen"))
            .expect("light must be present");
        assert!(
            light.attributes.contains_key("brightness"),
            "light.kitchen must have brightness attribute"
        );
    }

    #[test]
    fn load_fixture_sensor_has_unit_of_measurement() {
        let store = load(FIXTURE_PATH).expect("fixture must load successfully");
        let sensor = store
            .get(&EntityId::from("sensor.hallway_temperature"))
            .expect("sensor must be present");
        assert!(
            sensor.attributes.contains_key("unit_of_measurement"),
            "sensor must have unit_of_measurement attribute"
        );
    }

    // -----------------------------------------------------------------------
    // Capacity enforcement: exceeding DEFAULT_PROFILE.max_entities → Err
    // -----------------------------------------------------------------------

    #[test]
    fn load_fails_when_fixture_exceeds_max_entities() {
        let cap = DEFAULT_PROFILE.max_entities;
        // Build cap + 1 raw entries as a JSON array and write to a temp file.
        let entries: Vec<serde_json::Value> = (0..=cap)
            .map(|i| {
                serde_json::json!({
                    "entity_id": format!("light.e{i}"),
                    "state": "on",
                    "attributes": {},
                    "last_changed": "2026-04-24T10:00:00Z",
                    "last_updated": "2026-04-24T10:00:00Z"
                })
            })
            .collect();

        let tmp = tempfile_with_json(&entries);
        let result = load(tmp.path());
        assert!(
            result.is_err(),
            "load must fail when entity count ({}) exceeds cap ({})",
            cap + 1,
            cap
        );
        match result.unwrap_err() {
            FixtureError::Capacity(MemoryStoreError::CapExceeded { count, cap: c }) => {
                assert_eq!(count, cap + 1);
                assert_eq!(c, cap);
            }
            other => panic!("expected FixtureError::Capacity(CapExceeded), got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Error cases
    // -----------------------------------------------------------------------

    #[test]
    fn load_returns_io_error_for_missing_file() {
        let result = load("/nonexistent/path/ha-states.json");
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), FixtureError::Io(_)),
            "missing file must produce FixtureError::Io"
        );
    }

    #[test]
    fn load_returns_json_error_for_invalid_json() {
        let tmp = tempfile_with_bytes(b"not valid json at all {{{");
        let result = load(tmp.path());
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), FixtureError::Json(_)),
            "invalid JSON must produce FixtureError::Json"
        );
    }

    #[test]
    fn load_returns_timestamp_error_for_bad_timestamp() {
        let entries = vec![serde_json::json!({
            "entity_id": "light.bad_ts",
            "state": "on",
            "attributes": {},
            "last_changed": "not-a-timestamp",
            "last_updated": "2026-04-24T10:00:00Z"
        })];
        let tmp = tempfile_with_json(&entries);
        let result = load(tmp.path());
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), FixtureError::Timestamp { .. }),
            "malformed timestamp must produce FixtureError::Timestamp"
        );
    }

    #[test]
    fn load_accepts_exactly_max_entities() {
        let cap = DEFAULT_PROFILE.max_entities;
        let entries: Vec<serde_json::Value> = (0..cap)
            .map(|i| {
                serde_json::json!({
                    "entity_id": format!("light.e{i}"),
                    "state": "on",
                    "attributes": {},
                    "last_changed": "2026-04-24T10:00:00Z",
                    "last_updated": "2026-04-24T10:00:00Z"
                })
            })
            .collect();

        let tmp = tempfile_with_json(&entries);
        let result = load(tmp.path());
        assert!(
            result.is_ok(),
            "load must succeed at exactly the cap ({cap})"
        );
    }

    // -----------------------------------------------------------------------
    // RawFixtureEntry unit test
    // -----------------------------------------------------------------------

    #[test]
    fn raw_fixture_entry_into_entity_happy_path() {
        let raw = RawFixtureEntry {
            entity_id: "sensor.temp".to_owned(),
            state: "22.0".to_owned(),
            attributes: {
                let mut m = Map::new();
                m.insert(
                    "unit_of_measurement".to_owned(),
                    serde_json::Value::String("°C".to_owned()),
                );
                m
            },
            last_changed: "2026-04-24T10:00:00Z".to_owned(),
            last_updated: "2026-04-24T11:00:00Z".to_owned(),
        };

        let entity = raw.into_entity().expect("conversion must succeed");
        assert_eq!(entity.id.as_str(), "sensor.temp");
        assert_eq!(&*entity.state, "22.0");
        assert!(entity.attributes.contains_key("unit_of_measurement"));
        assert_ne!(entity.last_changed, Timestamp::UNIX_EPOCH);
        assert_ne!(entity.last_updated, Timestamp::UNIX_EPOCH);
    }

    #[test]
    fn raw_fixture_entry_into_entity_bad_last_changed() {
        let raw = RawFixtureEntry {
            entity_id: "sensor.bad".to_owned(),
            state: "1".to_owned(),
            attributes: Map::new(),
            last_changed: "INVALID".to_owned(),
            last_updated: "2026-04-24T10:00:00Z".to_owned(),
        };
        let result = raw.into_entity();
        assert!(
            matches!(result, Err(FixtureError::Timestamp { .. })),
            "bad last_changed must be FixtureError::Timestamp"
        );
    }

    // -----------------------------------------------------------------------
    // Arc field sharing after load
    // -----------------------------------------------------------------------

    #[test]
    fn loaded_entity_attributes_are_arc_wrapped() {
        let store = load(FIXTURE_PATH).expect("fixture must load");
        let a = store
            .get(&EntityId::from("light.kitchen"))
            .expect("must exist");
        let b = store
            .get(&EntityId::from("light.kitchen"))
            .expect("must exist");
        // Two gets of the same entity share the same Arc allocation.
        assert!(Arc::ptr_eq(&a.attributes, &b.attributes));
    }

    // -----------------------------------------------------------------------
    // Helper: build a temp file with JSON content
    // -----------------------------------------------------------------------

    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn tempfile_with_json(value: &[serde_json::Value]) -> TempFile {
        let bytes = serde_json::to_vec(value).expect("json serialisation must not fail");
        tempfile_with_bytes(&bytes)
    }

    fn tempfile_with_bytes(bytes: &[u8]) -> TempFile {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "hanui-fixture-test-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let mut f = std::fs::File::create(&path).expect("temp file creation must succeed");
        f.write_all(bytes).expect("write must succeed");
        TempFile(path)
    }
}
