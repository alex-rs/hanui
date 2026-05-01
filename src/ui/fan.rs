//! Fan widget view-model and per-frame state derivation (TASK-103).
//!
//! # Hot-path discipline
//!
//! [`FanVM::from_entity`] is invoked at entity-change time, NOT per render.
//! The bridge's `build_tiles` / `apply_row_updates` paths call it once per
//! `fan.*` state-change event; the resulting [`FanVM`] is then projected
//! into the Slint-typed `FanTileVM` (in `bridge.rs`) and pushed via the
//! row-update path. No allocation occurs in any per-frame Slint callback.
//!
//! # State vocabulary (Home Assistant fan entity)
//!
//! Home Assistant exposes the following canonical states for `fan.*`
//! entities:
//!   * `"on"`           — fan is running (at any speed/preset).
//!   * `"off"`          — fan is stopped.
//!   * `"unavailable"` / `"unknown"` — not reachable.
//!
//! Some integrations also surface `"auto"` as a state when the fan is in a
//! controller-driven mode; we render this as a third state distinct from
//! plain on/off so the UI can hint "the fan is running, controlled by the
//! integration, not by you" — matching the TASK-103 acceptance for an
//! `is_on` boolean plus a `state` string the tile renders verbatim.
//!
//! `FanVM` encodes only the **derived view-state** the UI needs:
//!   * `state` — the canonical HA state string for the tile's hero label
//!     ("on", "off", "auto", "unavailable").
//!   * `is_on` — `true` for `"on"` / `"auto"`, `false` otherwise.
//!   * `speed_pct` — `Some(0..=100)` when the entity exposes the
//!     `percentage` attribute; `None` when the fan reports preset modes
//!     only or the attribute is absent. Speeds outside 0..=100 are
//!     dropped (the integration is misbehaving; we render no number).
//!   * `current_speed` — the active preset mode name, when the entity
//!     exposes the `preset_mode` attribute. `None` when the fan reports
//!     percentage only.
//!
//!   Preset mode list (`preset_modes`) is NOT stored on `FanVM` — it is
//!   only needed by `FanBody::render_rows` and the action dispatcher,
//!   neither of which runs at flush frequency.
//!
//! # JSON-crate discipline (CI Gate 2)
//!
//! `src/ui/**` is gated against direct references to the JSON crate by
//! `.github/workflows/ci.yml` Gate 2. The implementation reads
//! `entity.attributes` only via inferred-type accessors (`.as_str`,
//! `.as_u64`, `.as_f64`, `.as_i64`, `.as_array`) — never the JSON-crate
//! `Value` type by name.

use crate::ha::entity::Entity;

// ---------------------------------------------------------------------------
// FanVM
// ---------------------------------------------------------------------------

/// Per-frame derived view-state for a `fan.*` entity.
///
/// Built by [`FanVM::from_entity`] at entity-change time. The bridge then
/// projects this into a Slint `FanTileVM` and pushes the row update — see
/// `src/ui/bridge.rs::compute_fan_tile_vm`.
///
/// # Field semantics
///
/// * `state` — the canonical HA state string ("on" / "off" / "auto" /
///   "unavailable"), forwarded verbatim to the Slint tile for the hero
///   label. The Slint tile branches on this string to drive the
///   three-state colour render.
/// * `is_on` — `true` for `"on"` / `"auto"`, `false` for `"off"` /
///   `"unavailable"` / unknown. Drives the on/off toggle visual on the
///   tile and the more-info modal toggle.
/// * `speed_pct` — `Some(0..=100)` when `percentage` is present.
/// * `current_speed` — `Some(preset_name)` when `preset_mode` is present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanVM {
    /// Canonical HA state string for the tile hero label.
    pub state: String,
    /// True for `"on"` / `"auto"`.
    pub is_on: bool,
    /// Current speed percentage 0..=100 when reported.
    pub speed_pct: Option<u8>,
    /// Current preset mode name when reported.
    pub current_speed: Option<String>,
}

impl FanVM {
    /// Construct a [`FanVM`] from a live [`Entity`] snapshot.
    ///
    /// # State mapping
    ///
    /// | HA state         | `is_on` |
    /// |------------------|---------|
    /// | `"on"`           | true    |
    /// | `"auto"`         | true    |
    /// | `"off"`          | false   |
    /// | `"unavailable"`  | false   |
    /// | other / unknown  | false   |
    ///
    /// `speed_pct` is read from the `percentage` attribute (HA emits an
    /// integer 0..=100). When absent, out-of-range, or non-numeric, the
    /// field is `None` — the tile's percentage label is hidden rather
    /// than rendering an out-of-spec number.
    ///
    /// `current_speed` is read from the `preset_mode` attribute (HA
    /// string). When absent or non-string, the field is `None`.
    ///
    /// Preset mode list (`preset_modes`) is NOT read here — it is only
    /// needed by `FanBody::render_rows` (more-info modal) and the
    /// action dispatcher, neither of which runs at flush frequency.
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Self {
        let state = entity.state.as_ref();
        let is_on = matches!(state, "on" | "auto");

        let speed_pct = read_percentage_attribute(entity);
        let current_speed = read_preset_mode_attribute(entity);

        FanVM {
            state: state.to_owned(),
            is_on,
            speed_pct,
            current_speed,
        }
    }
}

/// Read the `percentage` attribute as a `u8` in 0..=100.
///
/// Returns `None` when:
///   * The attribute is absent.
///   * The attribute is not numeric.
///   * The numeric value is outside the 0..=100 range.
///
/// Reads the attribute through inferred-type accessors only (no JSON-crate
/// path reference) so this module remains compliant with the `src/ui/**`
/// Gate-2 grep.
fn read_percentage_attribute(entity: &Entity) -> Option<u8> {
    let value = entity.attributes.get("percentage")?;
    if let Some(u) = value.as_u64() {
        return u8::try_from(u).ok().filter(|&p| p <= 100);
    }
    if let Some(i) = value.as_i64() {
        if !(0..=100).contains(&i) {
            return None;
        }
        return u8::try_from(i).ok();
    }
    if let Some(f) = value.as_f64() {
        if !(0.0..=100.0).contains(&f) {
            return None;
        }
        // Round to nearest u8 — HA emits integer percentages but defensive
        // against integration variants (some emit f64).
        return Some(f.round() as u8);
    }
    None
}

/// Read the `preset_mode` attribute as a `String`.
///
/// Returns `None` when the attribute is absent or non-string.
fn read_preset_mode_attribute(entity: &Entity) -> Option<String> {
    let value = entity.attributes.get("preset_mode")?;
    value.as_str().map(str::to_owned)
}

/// Read the `oscillating` attribute as a `bool` if present.
///
/// HA fan integrations expose oscillation state as a boolean. The
/// more-info body surfaces this as a row when reported. Public so the
/// modal body can pull the value without duplicating the parsing logic.
#[must_use]
pub fn read_oscillating_attribute(entity: &Entity) -> Option<bool> {
    entity.attributes.get("oscillating")?.as_bool()
}

/// Read the `direction` attribute as a `String` if present.
///
/// HA fan integrations expose direction as a string ("forward" /
/// "reverse"). The more-info body surfaces this as a row when reported.
#[must_use]
pub fn read_direction_attribute(entity: &Entity) -> Option<String> {
    entity
        .attributes
        .get("direction")?
        .as_str()
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::entity::EntityId;
    use std::sync::Arc;

    /// Test-only helper: read the `preset_modes` attribute as a Vec<String>.
    /// Not on the hot path — only used in more-info modal and dispatcher
    /// contexts, so it lives here rather than in production code.
    fn read_preset_modes_attribute(entity: &Entity) -> Vec<String> {
        let Some(value) = entity.attributes.get("preset_modes") else {
            return Vec::new();
        };
        let Some(arr) = value.as_array() else {
            return Vec::new();
        };
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    }

    /// Construct a minimal [`Entity`] with an empty attribute map.
    /// Mirrors the helper in `src/ui/cover.rs::tests` — uses
    /// `Arc::default()` so we do not name the JSON crate (Gate 2).
    fn minimal_entity(id: &str, state: &str) -> Entity {
        Entity {
            id: EntityId::from(id),
            state: Arc::from(state),
            attributes: Arc::default(),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    /// Construct an [`Entity`] carrying attributes parsed from a
    /// YAML/JSON snippet. JSON is a strict subset of YAML 1.2; we use
    /// `serde_yaml_ng` (workspace dep) to stay inside Gate 2 — the
    /// JSON crate path must not appear textually anywhere in
    /// `src/ui/**`.
    fn entity_with_attrs(state: &str, attrs_json: &str) -> Entity {
        let map = serde_yaml_ng::from_str(attrs_json)
            .expect("test snippet must parse as a YAML/JSON map");
        Entity {
            id: EntityId::from("fan.test"),
            state: Arc::from(state),
            attributes: Arc::new(map),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    // -----------------------------------------------------------------------
    // State mapping
    // -----------------------------------------------------------------------

    #[test]
    fn from_entity_on_off_states() {
        let on = minimal_entity("fan.bedroom", "on");
        let vm = FanVM::from_entity(&on);
        assert!(vm.is_on, "on state must produce is_on=true");
        assert_eq!(vm.state, "on");

        let off = minimal_entity("fan.bedroom", "off");
        let vm = FanVM::from_entity(&off);
        assert!(!vm.is_on, "off state must produce is_on=false");
        assert_eq!(vm.state, "off");
    }

    #[test]
    fn from_entity_auto_state_is_on() {
        let entity = minimal_entity("fan.bedroom", "auto");
        let vm = FanVM::from_entity(&entity);
        assert!(vm.is_on, "auto state must produce is_on=true");
        assert_eq!(vm.state, "auto");
    }

    #[test]
    fn from_entity_unavailable_state() {
        let entity = minimal_entity("fan.bedroom", "unavailable");
        let vm = FanVM::from_entity(&entity);
        assert!(!vm.is_on, "unavailable falls back to is_on=false");
        assert_eq!(vm.state, "unavailable");
    }

    #[test]
    fn from_entity_unknown_state_falls_back_safely() {
        let entity = minimal_entity("fan.bedroom", "garbage_state_name");
        let vm = FanVM::from_entity(&entity);
        assert!(!vm.is_on);
        assert_eq!(vm.state, "garbage_state_name");
        assert!(vm.speed_pct.is_none());
        assert!(vm.current_speed.is_none());
    }

    // -----------------------------------------------------------------------
    // percentage attribute parsing
    // -----------------------------------------------------------------------

    #[test]
    fn from_entity_reads_percentage_u64() {
        let entity = entity_with_attrs("on", r#"{"percentage":42}"#);
        let vm = FanVM::from_entity(&entity);
        assert_eq!(vm.speed_pct, Some(42));
    }

    #[test]
    fn from_entity_reads_percentage_zero() {
        let entity = entity_with_attrs("off", r#"{"percentage":0}"#);
        let vm = FanVM::from_entity(&entity);
        assert_eq!(vm.speed_pct, Some(0));
    }

    #[test]
    fn from_entity_reads_percentage_hundred() {
        let entity = entity_with_attrs("on", r#"{"percentage":100}"#);
        let vm = FanVM::from_entity(&entity);
        assert_eq!(vm.speed_pct, Some(100));
    }

    #[test]
    fn from_entity_percentage_out_of_range_is_none() {
        // 150 is out of 0..=100 — drop, do not clamp.
        let entity = entity_with_attrs("on", r#"{"percentage":150}"#);
        let vm = FanVM::from_entity(&entity);
        assert_eq!(vm.speed_pct, None);
    }

    #[test]
    fn from_entity_percentage_negative_is_none() {
        let entity = entity_with_attrs("on", r#"{"percentage":-5}"#);
        let vm = FanVM::from_entity(&entity);
        assert_eq!(vm.speed_pct, None);
    }

    #[test]
    fn from_entity_percentage_float_rounds_to_nearest() {
        let entity = entity_with_attrs("on", r#"{"percentage":33.7}"#);
        let vm = FanVM::from_entity(&entity);
        assert_eq!(vm.speed_pct, Some(34));
    }

    #[test]
    fn from_entity_percentage_absent_is_none() {
        let entity = minimal_entity("fan.bedroom", "on");
        let vm = FanVM::from_entity(&entity);
        assert_eq!(vm.speed_pct, None);
    }

    // -----------------------------------------------------------------------
    // preset_mode / preset_modes attribute parsing
    // -----------------------------------------------------------------------

    #[test]
    fn current_speed_reads_preset_mode_attribute() {
        let entity = entity_with_attrs("on", r#"{"preset_mode":"High"}"#);
        let vm = FanVM::from_entity(&entity);
        assert_eq!(vm.current_speed.as_deref(), Some("High"));
    }

    #[test]
    fn current_speed_absent_is_none() {
        let entity = minimal_entity("fan.bedroom", "on");
        let vm = FanVM::from_entity(&entity);
        assert!(vm.current_speed.is_none());
    }

    #[test]
    fn read_preset_modes_reads_array() {
        let entity = entity_with_attrs("on", r#"{"preset_modes":["Low","Medium","High"]}"#);
        assert_eq!(
            read_preset_modes_attribute(&entity),
            vec!["Low".to_owned(), "Medium".to_owned(), "High".to_owned()]
        );
    }

    #[test]
    fn read_preset_modes_absent_is_empty_vec() {
        let entity = minimal_entity("fan.bedroom", "on");
        assert!(read_preset_modes_attribute(&entity).is_empty());
    }

    #[test]
    fn read_preset_modes_drops_non_string_entries() {
        let entity = entity_with_attrs("on", r#"{"preset_modes":["Low",2,null,"High"]}"#);
        assert_eq!(
            read_preset_modes_attribute(&entity),
            vec!["Low".to_owned(), "High".to_owned()]
        );
    }

    // -----------------------------------------------------------------------
    // oscillating + direction helpers (used by FanBody)
    // -----------------------------------------------------------------------

    #[test]
    fn read_oscillating_attribute_present_true() {
        let entity = entity_with_attrs("on", r#"{"oscillating":true}"#);
        assert_eq!(read_oscillating_attribute(&entity), Some(true));
    }

    #[test]
    fn read_oscillating_attribute_present_false() {
        let entity = entity_with_attrs("on", r#"{"oscillating":false}"#);
        assert_eq!(read_oscillating_attribute(&entity), Some(false));
    }

    #[test]
    fn read_oscillating_attribute_absent() {
        let entity = minimal_entity("fan.bedroom", "on");
        assert_eq!(read_oscillating_attribute(&entity), None);
    }

    #[test]
    fn read_direction_attribute_present() {
        let entity = entity_with_attrs("on", r#"{"direction":"forward"}"#);
        assert_eq!(
            read_direction_attribute(&entity).as_deref(),
            Some("forward")
        );
    }

    #[test]
    fn read_direction_attribute_absent() {
        let entity = minimal_entity("fan.bedroom", "on");
        assert_eq!(read_direction_attribute(&entity), None);
    }
}
