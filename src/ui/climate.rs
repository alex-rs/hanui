//! Climate widget view-model and per-frame state derivation (TASK-108).
//!
//! # Hot-path discipline
//!
//! [`ClimateVM::from_entity`] is invoked at entity-change time, NOT per
//! render. The bridge's `build_tiles` / `apply_row_updates` paths call it
//! once per `climate.*` state-change event; the resulting [`ClimateVM`] is
//! then projected into the Slint-typed `ClimateTileVM` (in `bridge.rs`) and
//! pushed via the row-update path. No allocation occurs in any per-frame
//! Slint callback.
//!
//! # State vocabulary (Home Assistant `climate.*` entity)
//!
//! Home Assistant exposes the following canonical HVAC modes for `climate.*`
//! entities. Per `locked_decisions.hvac_mode_vocabulary`, these are the
//! standard values; the schema treats `mode` as a free `String` because
//! integrations may emit custom modes.
//!
//!   * `"off"`        — HVAC system idle.
//!   * `"heat"`       — actively heating (or scheduled to heat).
//!   * `"cool"`       — actively cooling.
//!   * `"auto"`       — controller chooses between heat/cool to hold setpoint.
//!   * `"heat_cool"`  — dual-setpoint mode (some integrations).
//!   * `"dry"`        — dehumidify.
//!   * `"fan_only"`   — fan running, compressor idle.
//!   * `"unavailable"` / `"unknown"` — not reachable.
//!
//! `ClimateVM` encodes only the **derived view-state** the tile needs:
//!   * `state` — canonical HVAC-mode string forwarded verbatim.
//!   * `current_temperature` — read from HA's `current_temperature` attribute
//!     when present.
//!   * `target_temperature` — read from HA's `temperature` attribute (HA's
//!     setpoint attribute name; `target_temperature` is NOT the HA wire
//!     attribute name).
//!   * `is_active` — `true` when the HVAC system is doing real work
//!     (any non-`off`, non-`unavailable`, non-`unknown` mode).
//!
//! # Why no `Vec` fields (lesson from TASK-103 / TASK-105 / TASK-107)
//!
//! `ClimateVM` deliberately carries no `Vec` fields. The mode-picker list
//! lives on `WidgetOptions::Climate.hvac_modes` (read at modal-open time
//! by [`crate::ui::more_info::ClimateBody`] / the dispatcher), NOT on the
//! per-frame tile VM. Allocating a `Vec` per state-change event for a list
//! that the tile renderer never reads would be wasted work; the lesson
//! learned in TASK-103's `FanVM` and reinforced by every per-domain VM
//! since is to keep the per-frame VM lean and scalar.
//!
//! # JSON-crate discipline (CI Gate 2)
//!
//! `src/ui/**` is gated against direct references to the JSON crate by
//! `.github/workflows/ci.yml` Gate 2. The implementation reads
//! `entity.attributes` only via inferred-type accessors (`.as_str`,
//! `.as_f64`, `.as_i64`, `.as_u64`, `.as_bool`) — never the JSON-crate
//! `Value` type by name.

use crate::ha::entity::Entity;

// ---------------------------------------------------------------------------
// ClimateVM
// ---------------------------------------------------------------------------

/// Per-frame derived view-state for a `climate.*` entity.
///
/// Built by [`ClimateVM::from_entity`] at entity-change time. The bridge then
/// projects this into a Slint `ClimateTileVM` and pushes the row update —
/// see `src/ui/bridge.rs::compute_climate_tile_vm`.
///
/// # Field semantics
///
/// * `state` — canonical HVAC-mode string forwarded verbatim to the Slint
///   tile for the hero label.
/// * `current_temperature` — present if HA reported `current_temperature`
///   on the entity attributes; absent otherwise. Forwarded as the read-out
///   "currently …°" label on the tile.
/// * `target_temperature` — present if HA reported the `temperature`
///   attribute (HA's setpoint attribute is named `temperature`, NOT
///   `target_temperature`). Absent for entities in modes without a
///   single-target setpoint (e.g. `heat_cool` reports `target_temp_low`
///   / `target_temp_high` separately and no `temperature`).
/// * `is_active` — `true` for any state other than `"off"`, `"unavailable"`,
///   or `"unknown"`. Drives the "active" visual variant on the tile.
#[derive(Debug, Clone, PartialEq)]
pub struct ClimateVM {
    /// Canonical HVAC-mode string for the tile hero label.
    pub state: String,
    /// Currently-measured temperature reported by the entity (HA's
    /// `current_temperature` attribute), if present.
    pub current_temperature: Option<f32>,
    /// Setpoint temperature (HA's `temperature` attribute), if present.
    pub target_temperature: Option<f32>,
    /// True when the HVAC system is in any non-idle, available mode.
    pub is_active: bool,
}

impl ClimateVM {
    /// Construct a [`ClimateVM`] from a live [`Entity`] snapshot.
    ///
    /// # State mapping
    ///
    /// | HA state         | `is_active` |
    /// |------------------|-------------|
    /// | `"off"`          | false       |
    /// | `"heat"`         | true        |
    /// | `"cool"`         | true        |
    /// | `"auto"`         | true        |
    /// | `"heat_cool"`    | true        |
    /// | `"dry"`          | true        |
    /// | `"fan_only"`     | true        |
    /// | `"unavailable"`  | false       |
    /// | `"unknown"`      | false       |
    /// | other / vendor   | true        |
    ///
    /// `state` is forwarded verbatim — the Slint tile renders the value
    /// as the hero label. The boolean `is_active` is precomputed here so
    /// the Slint side does not branch on the HVAC-mode vocabulary.
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Self {
        let state = entity.state.as_ref();
        let is_active = !matches!(state, "off" | "unavailable" | "unknown");

        let current_temperature = read_current_temperature_attribute(entity);
        let target_temperature = read_target_temperature_attribute(entity);

        ClimateVM {
            state: state.to_owned(),
            current_temperature,
            target_temperature,
            is_active,
        }
    }
}

// ---------------------------------------------------------------------------
// Attribute accessors (read by the bridge and `more_info::ClimateBody`)
// ---------------------------------------------------------------------------

/// Read the `current_temperature` attribute as `f32` if present.
///
/// HA's climate integration exposes the currently-measured temperature
/// under the attribute name `current_temperature`. The bridge surfaces
/// this on the tile; the more-info modal also forwards it as a row.
///
/// Returns `None` when the attribute is absent or the JSON value is not
/// numeric. We accept any of `as_f64` / `as_i64` / `as_u64` because HA
/// integrations vary (some emit integers, some emit floats with trailing
/// zeros).
#[must_use]
pub fn read_current_temperature_attribute(entity: &Entity) -> Option<f32> {
    read_temperature_like_attribute(entity, "current_temperature")
}

/// Read the `temperature` attribute as `f32` if present.
///
/// HA's climate integration exposes the **setpoint** target temperature
/// under the attribute name `temperature` (the field name `target_temperature`
/// is NOT used on the wire). Returns `None` when the attribute is absent
/// or the JSON value is not numeric — `heat_cool` mode entities typically
/// emit `target_temp_low` / `target_temp_high` instead and no top-level
/// `temperature`.
#[must_use]
pub fn read_target_temperature_attribute(entity: &Entity) -> Option<f32> {
    read_temperature_like_attribute(entity, "temperature")
}

/// Read the `humidity` attribute as `f32` if present.
///
/// HA's climate integration exposes the currently-measured humidity (when
/// supported by the device) under `humidity`. Used by
/// [`crate::ui::more_info::ClimateBody`].
#[must_use]
pub fn read_humidity_attribute(entity: &Entity) -> Option<f32> {
    read_temperature_like_attribute(entity, "humidity")
}

/// Read the `fan_mode` attribute as a `String` if present.
///
/// HA's climate integration exposes the active fan mode (e.g. `"auto"`,
/// `"low"`, `"high"`) under `fan_mode`. Used by
/// [`crate::ui::more_info::ClimateBody`].
#[must_use]
pub fn read_fan_mode_attribute(entity: &Entity) -> Option<String> {
    entity
        .attributes
        .get("fan_mode")?
        .as_str()
        .map(str::to_owned)
}

/// Read the `swing_mode` attribute as a `String` if present.
///
/// HA's climate integration exposes the active swing mode (e.g. `"on"`,
/// `"off"`, `"vertical"`) under `swing_mode`. Used by
/// [`crate::ui::more_info::ClimateBody`].
#[must_use]
pub fn read_swing_mode_attribute(entity: &Entity) -> Option<String> {
    entity
        .attributes
        .get("swing_mode")?
        .as_str()
        .map(str::to_owned)
}

/// Internal: read a numeric temperature-like attribute as `f32`, accepting
/// any of `f64`, `i64`, `u64` JSON shapes.
///
/// We narrow `f64 -> f32` deliberately; HA reports temperatures with
/// at most one decimal place, well inside the f32 mantissa. Out-of-range
/// values (e.g. NaN, infinity) are treated as absent.
fn read_temperature_like_attribute(entity: &Entity, key: &str) -> Option<f32> {
    let value = entity.attributes.get(key)?;
    if let Some(f) = value.as_f64() {
        if f.is_finite() {
            return Some(f as f32);
        }
        return None;
    }
    if let Some(i) = value.as_i64() {
        return Some(i as f32);
    }
    if let Some(u) = value.as_u64() {
        return Some(u as f32);
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::entity::EntityId;
    use std::sync::Arc;

    /// Construct a minimal [`Entity`] with an empty attribute map. Mirrors
    /// the helper in `src/ui/cover.rs::tests` / `src/ui/alarm.rs::tests` /
    /// `src/ui/camera.rs::tests` — uses `Arc::default()` so we do not name
    /// the JSON crate (Gate 2).
    fn minimal_entity(id: &str, state: &str) -> Entity {
        Entity {
            id: EntityId::from(id),
            state: Arc::from(state),
            attributes: Arc::default(),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    /// Construct an [`Entity`] carrying attributes parsed from a YAML/JSON
    /// snippet. Uses `serde_yaml_ng` to stay inside Gate 2 — the JSON crate
    /// path must not appear textually anywhere in `src/ui/**`.
    fn entity_with_attrs(state: &str, attrs_json: &str) -> Entity {
        let map = serde_yaml_ng::from_str(attrs_json)
            .expect("test snippet must parse as a YAML/JSON map");
        Entity {
            id: EntityId::from("climate.test"),
            state: Arc::from(state),
            attributes: Arc::new(map),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    // -----------------------------------------------------------------------
    // State mapping (TASK-108 acceptance: every HVAC mode → is_active)
    // -----------------------------------------------------------------------

    /// A `climate.*` entity in `"heat"` mode produces `is_active=true` and
    /// forwards the state string verbatim — the canonical TASK-108
    /// "from_entity for state==\"heat\" produces hvac_mode: \"heat\"" check.
    #[test]
    fn from_entity_heat_mode() {
        let entity = minimal_entity("climate.living_room", "heat");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.state, "heat");
        assert!(vm.is_active, "heat mode must produce is_active=true");
    }

    #[test]
    fn from_entity_cool_mode() {
        let entity = minimal_entity("climate.living_room", "cool");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.state, "cool");
        assert!(vm.is_active, "cool mode must produce is_active=true");
    }

    #[test]
    fn from_entity_auto_mode() {
        let entity = minimal_entity("climate.living_room", "auto");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.state, "auto");
        assert!(vm.is_active, "auto mode must produce is_active=true");
    }

    #[test]
    fn from_entity_heat_cool_mode() {
        let entity = minimal_entity("climate.living_room", "heat_cool");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.state, "heat_cool");
        assert!(vm.is_active, "heat_cool mode must produce is_active=true");
    }

    #[test]
    fn from_entity_dry_mode() {
        let entity = minimal_entity("climate.living_room", "dry");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.state, "dry");
        assert!(vm.is_active, "dry mode must produce is_active=true");
    }

    #[test]
    fn from_entity_fan_only_mode() {
        let entity = minimal_entity("climate.living_room", "fan_only");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.state, "fan_only");
        assert!(vm.is_active, "fan_only mode must produce is_active=true");
    }

    #[test]
    fn from_entity_off_state_is_idle() {
        let entity = minimal_entity("climate.living_room", "off");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.state, "off");
        assert!(!vm.is_active, "off must produce is_active=false");
    }

    #[test]
    fn from_entity_unavailable_state() {
        let entity = minimal_entity("climate.living_room", "unavailable");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.state, "unavailable");
        assert!(
            !vm.is_active,
            "unavailable must produce is_active=false (not running)"
        );
    }

    #[test]
    fn from_entity_unknown_state() {
        let entity = minimal_entity("climate.living_room", "unknown");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.state, "unknown");
        assert!(!vm.is_active, "unknown must produce is_active=false");
    }

    /// Vendor-specific HVAC modes (HA allows custom modes per
    /// `locked_decisions.hvac_mode_vocabulary`) must be treated as active
    /// rather than silently dropped to idle. The picker shows whatever
    /// `WidgetOptions::Climate.hvac_modes` lists; the tile must render the
    /// active visual for any operator-configured non-off mode.
    #[test]
    fn from_entity_vendor_specific_state_is_active() {
        let entity = minimal_entity("climate.living_room", "boost");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.state, "boost");
        assert!(vm.is_active, "vendor mode must produce is_active=true");
    }

    // -----------------------------------------------------------------------
    // Temperature attribute reads (TASK-108 acceptance:
    //   target_temperature reads HA's `temperature` attribute,
    //   current_temperature reads HA's `current_temperature` attribute)
    // -----------------------------------------------------------------------

    /// `from_entity` reads HA's `current_temperature` attribute into the
    /// `current_temperature` field, and HA's `temperature` attribute into
    /// `target_temperature`. This is the canonical TASK-108 acceptance
    /// check that target reads `temperature` (NOT `target_temperature`).
    #[test]
    fn target_and_current_temperature_attributes() {
        let entity =
            entity_with_attrs("heat", r#"{"current_temperature":21.5,"temperature":23.0}"#);
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(
            vm.current_temperature,
            Some(21.5),
            "current_temperature must read HA's current_temperature attribute"
        );
        assert_eq!(
            vm.target_temperature,
            Some(23.0),
            "target_temperature must read HA's `temperature` attribute (not `target_temperature`)"
        );
    }

    #[test]
    fn current_temperature_absent_when_attribute_missing() {
        let entity = minimal_entity("climate.living_room", "heat");
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.current_temperature, None);
        assert_eq!(vm.target_temperature, None);
    }

    /// HA integrations sometimes emit integer-valued temperatures
    /// (e.g. `21` rather than `21.0`); the reader must accept these as
    /// `f32` rather than returning `None`.
    #[test]
    fn temperature_attributes_accept_integer_values() {
        let entity = entity_with_attrs("heat", r#"{"current_temperature":22,"temperature":24}"#);
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.current_temperature, Some(22.0));
        assert_eq!(vm.target_temperature, Some(24.0));
    }

    /// Non-numeric temperature attributes resolve to `None` (we do NOT
    /// silently render `0.0` for a malformed value).
    #[test]
    fn temperature_attribute_string_value_is_none() {
        let entity = entity_with_attrs("heat", r#"{"temperature":"hot"}"#);
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.target_temperature, None);
    }

    /// Boolean values must NOT be silently coerced to numeric temperatures
    /// (a malformed integration emitting `temperature: true` should not
    /// surface as `1.0°` on the tile label).
    #[test]
    fn temperature_attribute_bool_value_is_none() {
        let entity = entity_with_attrs("heat", r#"{"temperature":true}"#);
        let vm = ClimateVM::from_entity(&entity);
        assert_eq!(vm.target_temperature, None);
    }

    // -----------------------------------------------------------------------
    // Attribute helpers (used by ClimateBody)
    // -----------------------------------------------------------------------

    #[test]
    fn read_humidity_attribute_present() {
        let entity = entity_with_attrs("cool", r#"{"humidity":42.5}"#);
        assert_eq!(read_humidity_attribute(&entity), Some(42.5));
    }

    #[test]
    fn read_humidity_attribute_absent() {
        let entity = minimal_entity("climate.living_room", "heat");
        assert_eq!(read_humidity_attribute(&entity), None);
    }

    #[test]
    fn read_fan_mode_attribute_present() {
        let entity = entity_with_attrs("heat", r#"{"fan_mode":"auto"}"#);
        assert_eq!(read_fan_mode_attribute(&entity).as_deref(), Some("auto"));
    }

    #[test]
    fn read_fan_mode_attribute_absent() {
        let entity = minimal_entity("climate.living_room", "heat");
        assert_eq!(read_fan_mode_attribute(&entity), None);
    }

    #[test]
    fn read_swing_mode_attribute_present() {
        let entity = entity_with_attrs("heat", r#"{"swing_mode":"vertical"}"#);
        assert_eq!(
            read_swing_mode_attribute(&entity).as_deref(),
            Some("vertical")
        );
    }

    #[test]
    fn read_swing_mode_attribute_absent() {
        let entity = minimal_entity("climate.living_room", "heat");
        assert_eq!(read_swing_mode_attribute(&entity), None);
    }

    #[test]
    fn read_fan_mode_attribute_non_string_is_none() {
        let entity = entity_with_attrs("heat", r#"{"fan_mode":42}"#);
        assert_eq!(read_fan_mode_attribute(&entity), None);
    }

    // -----------------------------------------------------------------------
    // No-Vec invariant (TASK-103 / TASK-105 / TASK-107 lesson)
    // -----------------------------------------------------------------------

    /// Compile-time assertion: `ClimateVM` does NOT carry a `Vec` field.
    /// We assert this structurally by requiring `Clone` (cheap on the
    /// scalar shape) and by counting the public field surface — any future
    /// edit that adds a `Vec<…>` must explicitly delete this test, which
    /// is a much louder review signal than an accidental allocation.
    ///
    /// Because `f32: !Eq` we cannot reuse the alarm/camera `Eq` trick;
    /// instead we assert via `mem::size_of` that the struct shape matches
    /// the expected scalar layout. `String` carries one heap pointer, two
    /// `Option<f32>` carry their discriminants, and `bool` is a single
    /// byte — well under any plausible `Vec` field size on a 64-bit
    /// target. This is a sentinel guard, not a strict layout test.
    #[test]
    fn climate_vm_remains_lean() {
        // Allowed shape on 64-bit Linux: 24 (String) + 8 (Option<f32>) + 8
        // (Option<f32>) + 1 (bool) + padding to 8 = 48 bytes. A `Vec`
        // field would push this to >= 56.
        assert!(
            std::mem::size_of::<ClimateVM>() <= 56,
            "ClimateVM has grown past the lean-shape budget; \
             did someone add a Vec field?"
        );
    }
}
