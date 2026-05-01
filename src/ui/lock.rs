//! Lock widget view-model and per-frame state derivation (TASK-104).
//!
//! # Hot-path discipline
//!
//! [`LockVM::from_entity`] is invoked at entity-change time, NOT per render.
//! The bridge's `build_tiles` / `apply_row_updates` paths call it once per
//! `lock.*` state-change event; the resulting [`LockVM`] is then projected
//! into the Slint-typed `LockTileVM` (in `bridge.rs`) and pushed via the
//! row-update path. No allocation occurs in any per-frame Slint callback.
//!
//! The struct keeps only **scalar** fields needed by the tile renderer.
//! Per the TASK-103 audit lesson: anything that is only consumed by the
//! more-info modal lives in [`crate::ui::more_info::LockBody`] which
//! reads attributes directly from the entity at modal-open time. There is
//! no `Vec`-allocating field on this struct.
//!
//! # State vocabulary (Home Assistant lock entity)
//!
//! Home Assistant exposes the following canonical states for `lock.*`
//! entities:
//!   * `"locked"`      — bolt is engaged.
//!   * `"unlocked"`    — bolt is retracted.
//!   * `"locking"`     — actively transitioning unlocked → locked.
//!   * `"unlocking"`   — actively transitioning locked → unlocked.
//!   * `"jammed"`      — mechanical jam reported by the integration.
//!   * `"unavailable"` / `"unknown"` — not reachable.
//!
//! `LockVM` encodes only the **derived booleans** the UI needs:
//!   * `state`     — the canonical HA state string for the tile's hero
//!     label, forwarded verbatim. The Slint tile branches on this string
//!     for the jammed / unavailable cases.
//!   * `is_locked` — `true` for `"locked"` / `"locking"` (the latter so
//!     the tile colours during the move match the destination — same
//!     pattern `CoverVM` uses for `"opening"` / `"closing"`).
//!
//! # JSON-crate discipline (CI Gate 2)
//!
//! `src/ui/**` is gated against direct references to the JSON crate by
//! `.github/workflows/ci.yml` Gate 2. The implementation reads
//! `entity.attributes` only via inferred-type accessors (`.as_str`,
//! `.as_u64`, `.as_f64`, `.as_i64`, `.as_bool`) — never the JSON-crate
//! `Value` type by name.

use crate::ha::entity::Entity;

// ---------------------------------------------------------------------------
// LockVM
// ---------------------------------------------------------------------------

/// Per-frame derived view-state for a `lock.*` entity.
///
/// Built by [`LockVM::from_entity`] at entity-change time. The bridge then
/// projects this into a Slint `LockTileVM` and pushes the row update — see
/// `src/ui/bridge.rs::compute_lock_tile_vm`.
///
/// # Field semantics
///
/// * `state` — the canonical HA state string (`"locked"` / `"unlocked"` /
///   `"locking"` / `"unlocking"` / `"jammed"` / `"unavailable"`).
///   Forwarded verbatim to the Slint tile for the hero label and for the
///   jammed / unavailable colour branches.
/// * `is_locked` — `true` for HA states `"locked"` / `"locking"`. Drives
///   the lock-icon visual on the tile without re-matching the state
///   string.
///
/// # No `Vec` fields
///
/// Per the TASK-103 audit lesson: the tile VM stays lean. Anything only
/// needed by the more-info modal — battery level, code length, integration
/// metadata — is read in `LockBody::render_rows` from the entity at
/// modal-open time. Allocating a `Vec` here that the tile never reads
/// would be wasted work on every state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockVM {
    /// True for `"locked"` / `"locking"`.
    pub is_locked: bool,
}

impl LockVM {
    /// Construct a [`LockVM`] from a live [`Entity`] snapshot.
    ///
    /// # State mapping
    ///
    /// | HA state         | `is_locked` |
    /// |------------------|-------------|
    /// | `"locked"`       | true        |
    /// | `"locking"`      | true        |
    /// | `"unlocked"`     | false       |
    /// | `"unlocking"`    | false       |
    /// | `"jammed"`       | false       |
    /// | `"unavailable"`  | false       |
    /// | other / unknown  | false       |
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Self {
        let state = entity.state.as_ref();
        let is_locked = matches!(state, "locked" | "locking");

        LockVM { is_locked }
    }
}

/// Read the `battery_level` attribute as a `u8` in 0..=100.
///
/// HA lock integrations expose battery as an integer percentage. The
/// more-info body surfaces this as a row when reported. Public so
/// [`crate::ui::more_info::LockBody`] can pull the value without
/// duplicating the parsing logic.
#[must_use]
pub fn read_battery_level_attribute(entity: &Entity) -> Option<u8> {
    let value = entity.attributes.get("battery_level")?;
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

/// Read the `code_format` attribute as a `String` if present.
///
/// HA lock integrations expose the code-format hint as a free-form
/// string (e.g. `"number"`, `"text"`). The more-info body surfaces this
/// verbatim so the user can confirm whether the entity expects digits or
/// arbitrary text. Public so [`crate::ui::more_info::LockBody`] can
/// pull the value without duplicating the parsing logic.
#[must_use]
pub fn read_code_format_attribute(entity: &Entity) -> Option<String> {
    entity
        .attributes
        .get("code_format")?
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
    /// YAML/JSON snippet. Uses `serde_yaml_ng` to stay inside Gate 2 —
    /// the JSON crate path must not appear textually anywhere in
    /// `src/ui/**`.
    fn entity_with_attrs(state: &str, attrs_json: &str) -> Entity {
        let map = serde_yaml_ng::from_str(attrs_json)
            .expect("test snippet must parse as a YAML/JSON map");
        Entity {
            id: EntityId::from("lock.test"),
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
    fn from_entity_locked_state() {
        let entity = minimal_entity("lock.front_door", "locked");
        let vm = LockVM::from_entity(&entity);
        assert!(vm.is_locked, "locked state must produce is_locked=true");
    }

    #[test]
    fn from_entity_unlocked_state() {
        let entity = minimal_entity("lock.front_door", "unlocked");
        let vm = LockVM::from_entity(&entity);
        assert!(!vm.is_locked, "unlocked state must produce is_locked=false");
    }

    #[test]
    fn from_entity_locking_state_colors_locked() {
        // "locking" is the active transition state to "locked"; the tile
        // colours with the destination so the user sees the locked palette
        // the moment the move starts.
        let entity = minimal_entity("lock.front_door", "locking");
        let vm = LockVM::from_entity(&entity);
        assert!(
            vm.is_locked,
            "locking state must colour with destination (is_locked=true)"
        );
    }

    #[test]
    fn from_entity_unlocking_state_colors_unlocked() {
        let entity = minimal_entity("lock.front_door", "unlocking");
        let vm = LockVM::from_entity(&entity);
        assert!(
            !vm.is_locked,
            "unlocking state must colour with destination (is_locked=false)"
        );
    }

    #[test]
    fn from_entity_jammed_state() {
        let entity = minimal_entity("lock.front_door", "jammed");
        let vm = LockVM::from_entity(&entity);
        // Jammed is not "locked" — the bolt position is indeterminate;
        // the tile renders with the unavailable colour but full opacity
        // so the warning is not dimmed.
        assert!(!vm.is_locked, "jammed state must produce is_locked=false");
    }

    #[test]
    fn from_entity_unavailable_state() {
        let entity = minimal_entity("lock.front_door", "unavailable");
        let vm = LockVM::from_entity(&entity);
        assert!(!vm.is_locked, "unavailable falls back to is_locked=false");
    }

    #[test]
    fn from_entity_unknown_state_falls_back_safely() {
        let entity = minimal_entity("lock.front_door", "garbage_state_name");
        let vm = LockVM::from_entity(&entity);
        assert!(!vm.is_locked);
    }

    // -----------------------------------------------------------------------
    // battery_level attribute parsing
    // -----------------------------------------------------------------------

    #[test]
    fn read_battery_level_attribute_present_u64() {
        let entity = entity_with_attrs("locked", r#"{"battery_level":85}"#);
        assert_eq!(read_battery_level_attribute(&entity), Some(85));
    }

    #[test]
    fn read_battery_level_attribute_zero() {
        let entity = entity_with_attrs("locked", r#"{"battery_level":0}"#);
        assert_eq!(read_battery_level_attribute(&entity), Some(0));
    }

    #[test]
    fn read_battery_level_attribute_hundred() {
        let entity = entity_with_attrs("locked", r#"{"battery_level":100}"#);
        assert_eq!(read_battery_level_attribute(&entity), Some(100));
    }

    #[test]
    fn read_battery_level_attribute_out_of_range() {
        let entity = entity_with_attrs("locked", r#"{"battery_level":150}"#);
        assert_eq!(read_battery_level_attribute(&entity), None);
    }

    #[test]
    fn read_battery_level_attribute_negative() {
        let entity = entity_with_attrs("locked", r#"{"battery_level":-5}"#);
        assert_eq!(read_battery_level_attribute(&entity), None);
    }

    #[test]
    fn read_battery_level_attribute_float_rounds() {
        let entity = entity_with_attrs("locked", r#"{"battery_level":42.7}"#);
        assert_eq!(read_battery_level_attribute(&entity), Some(43));
    }

    #[test]
    fn read_battery_level_attribute_absent() {
        let entity = minimal_entity("lock.front_door", "locked");
        assert_eq!(read_battery_level_attribute(&entity), None);
    }

    // -----------------------------------------------------------------------
    // code_format attribute parsing
    // -----------------------------------------------------------------------

    #[test]
    fn read_code_format_attribute_present() {
        let entity = entity_with_attrs("locked", r#"{"code_format":"number"}"#);
        assert_eq!(
            read_code_format_attribute(&entity).as_deref(),
            Some("number")
        );
    }

    #[test]
    fn read_code_format_attribute_absent() {
        let entity = minimal_entity("lock.front_door", "locked");
        assert_eq!(read_code_format_attribute(&entity), None);
    }

    #[test]
    fn read_code_format_attribute_non_string_is_none() {
        // Numeric values for code_format aren't string-typed; the typed
        // accessor returns None and the more-info body shows nothing
        // rather than rendering an out-of-spec value.
        let entity = entity_with_attrs("locked", r#"{"code_format":42}"#);
        assert_eq!(read_code_format_attribute(&entity), None);
    }
}
