//! Core Home Assistant entity types.
//!
//! `Entity` is designed for cheap cloning: heavy fields (`state`, `attributes`)
//! are `Arc`-wrapped so a clone copies only the pointer, not the heap data.
//! Timestamps use `jiff::Timestamp` (pinned in TASK-003).

use std::fmt;
use std::sync::Arc;

use jiff::Timestamp;
use serde_json::{Map, Value};
use smol_str::SmolStr;

// ---------------------------------------------------------------------------
// EntityId
// ---------------------------------------------------------------------------

/// Newtype wrapping [`SmolStr`] for Home Assistant entity identifiers.
///
/// Most entity IDs fit inside SmolStr's inline buffer (<=23 bytes) and avoid
/// a heap allocation entirely.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntityId(SmolStr);

impl EntityId {
    /// Returns the string slice of this entity ID.
    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl From<&str> for EntityId {
    fn from(s: &str) -> Self {
        EntityId(SmolStr::new(s))
    }
}

impl AsRef<str> for EntityId {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

// ---------------------------------------------------------------------------
// EntityKind
// ---------------------------------------------------------------------------

/// Domain class of a Home Assistant entity, derived from the prefix before
/// the first `.` in the entity ID (e.g. `light.kitchen` -> `Light`).
///
/// Unknown domains map to `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EntityKind {
    Light,
    Sensor,
    Switch,
    BinarySensor,
    Climate,
    MediaPlayer,
    Cover,
    Fan,
    Other,
}

impl EntityKind {
    fn from_prefix(prefix: &str) -> EntityKind {
        match prefix {
            "light" => EntityKind::Light,
            "sensor" => EntityKind::Sensor,
            "switch" => EntityKind::Switch,
            "binary_sensor" => EntityKind::BinarySensor,
            "climate" => EntityKind::Climate,
            "media_player" => EntityKind::MediaPlayer,
            "cover" => EntityKind::Cover,
            "fan" => EntityKind::Fan,
            _ => EntityKind::Other,
        }
    }
}

impl From<&EntityId> for EntityKind {
    fn from(id: &EntityId) -> Self {
        let prefix = id.as_str().split('.').next().unwrap_or("");
        EntityKind::from_prefix(prefix)
    }
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// A single Home Assistant entity snapshot.
///
/// Cloning is cheap: `state` and `attributes` are `Arc`-wrapped so only the
/// reference count is bumped on clone.
#[derive(Debug, Clone)]
pub struct Entity {
    pub id: EntityId,
    /// State string (e.g. `"on"`, `"off"`, `"unavailable"`). High duplication
    /// across snapshots makes `Arc<str>` the right storage choice.
    pub state: Arc<str>,
    /// Arbitrary JSON attributes as received from HA. Wrapped in `Arc` so
    /// snapshots share the map without deep-copying on clone.
    pub attributes: Arc<Map<String, Value>>,
    pub last_changed: Timestamp,
    pub last_updated: Timestamp,
}

impl Entity {
    /// Returns the entity's `friendly_name` attribute, if present and a string.
    ///
    /// This is the canonical accessor for the HA `friendly_name` attribute.
    /// It hides the raw `serde_json::Value` machinery behind a typed boundary
    /// so callers in `src/ui/` never need to import or spell `serde_json`.
    pub fn friendly_name(&self) -> Option<&str> {
        self.attributes
            .get("friendly_name")
            .and_then(|v| v.as_str())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    #[test]
    fn entity_kind_light() {
        let id = EntityId::from("light.kitchen");
        assert_eq!(EntityKind::from(&id), EntityKind::Light);
    }

    #[test]
    fn entity_kind_sensor() {
        let id = EntityId::from("sensor.hallway_temperature");
        assert_eq!(EntityKind::from(&id), EntityKind::Sensor);
    }

    #[test]
    fn entity_kind_binary_sensor() {
        let id = EntityId::from("binary_sensor.front_door");
        assert_eq!(EntityKind::from(&id), EntityKind::BinarySensor);
    }

    #[test]
    fn entity_kind_fallback_other() {
        let id = EntityId::from("unknown_thing.foo");
        assert_eq!(EntityKind::from(&id), EntityKind::Other);
    }

    #[test]
    fn entity_kind_no_dot_is_other() {
        let id = EntityId::from("nodotentityid");
        assert_eq!(EntityKind::from(&id), EntityKind::Other);
    }

    #[test]
    fn entity_id_small_string_not_heap_allocated() {
        // Most HA entity IDs are <=23 bytes and must not allocate on the heap.
        // SmolStr 0.3 exposes `is_heap_allocated()` for this assertion.
        let raw = "light.kitchen"; // 13 bytes -- well within SmolStr inline limit
        let smol = SmolStr::new(raw);
        assert!(
            !smol.is_heap_allocated(),
            "expected inline storage for short entity ID"
        );
        let id = EntityId::from(raw);
        assert_eq!(id.as_str(), raw);
    }

    #[test]
    fn entity_id_display() {
        let id = EntityId::from("sensor.temp");
        assert_eq!(id.to_string(), "sensor.temp");
    }

    #[test]
    fn entity_id_as_ref_str() {
        let id = EntityId::from("switch.outlet");
        let s: &str = id.as_ref();
        assert_eq!(s, "switch.outlet");
    }

    #[test]
    fn entity_clone_shares_arc_state_and_attributes() {
        let attrs: Map<String, Value> = Map::new();
        let entity = Entity {
            id: EntityId::from("light.kitchen"),
            state: Arc::from("on"),
            attributes: Arc::new(attrs),
            last_changed: Timestamp::UNIX_EPOCH,
            last_updated: Timestamp::UNIX_EPOCH,
        };
        let cloned = entity.clone();
        // Both clones must point to the same allocation -- pointer equality holds.
        assert!(Arc::ptr_eq(&entity.attributes, &cloned.attributes));
        assert!(Arc::ptr_eq(
            &entity.state as &Arc<str>,
            &cloned.state as &Arc<str>
        ));
    }

    // -----------------------------------------------------------------------
    // Entity::friendly_name accessor
    // -----------------------------------------------------------------------

    fn make_entity_with_attrs(attrs: Map<String, Value>) -> Entity {
        Entity {
            id: EntityId::from("light.test"),
            state: Arc::from("on"),
            attributes: Arc::new(attrs),
            last_changed: Timestamp::UNIX_EPOCH,
            last_updated: Timestamp::UNIX_EPOCH,
        }
    }

    #[test]
    fn friendly_name_present_and_string_returns_some() {
        let mut attrs = Map::new();
        attrs.insert(
            "friendly_name".to_string(),
            Value::String("Kitchen Light".to_string()),
        );
        let entity = make_entity_with_attrs(attrs);
        assert_eq!(entity.friendly_name(), Some("Kitchen Light"));
    }

    #[test]
    fn friendly_name_missing_returns_none() {
        let attrs = Map::new();
        let entity = make_entity_with_attrs(attrs);
        assert_eq!(entity.friendly_name(), None);
    }

    #[test]
    fn friendly_name_non_string_value_returns_none() {
        let mut attrs = Map::new();
        attrs.insert("friendly_name".to_string(), Value::Number(42.into()));
        let entity = make_entity_with_attrs(attrs);
        assert_eq!(entity.friendly_name(), None);
    }
}
