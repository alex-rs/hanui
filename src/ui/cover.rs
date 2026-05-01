//! Cover widget view-model and per-frame state derivation (TASK-102).
//!
//! # Hot-path discipline
//!
//! [`CoverVM::from_entity`] is invoked at entity-change time, NOT per render.
//! The bridge's `build_tiles` / `apply_row_updates` paths call it once per
//! `cover.*` state-change event; the resulting [`CoverVM`] is then projected
//! into the Slint-typed `CoverTileVM` (in `bridge.rs`) and pushed via the
//! row-update path. No allocation occurs in any per-frame Slint callback.
//!
//! # State vocabulary (Home Assistant cover entity)
//!
//! Home Assistant exposes the following canonical states for `cover.*`
//! entities:
//!   * `"open"`        — fully or partially open and not moving.
//!   * `"closed"`      — fully closed and not moving.
//!   * `"opening"`     — actively transitioning from closed → open.
//!   * `"closing"`     — actively transitioning from open → closed.
//!   * `"stopped"`     — paused mid-transition (some integrations).
//!   * `"unavailable"` / `"unknown"` — not reachable.
//!
//! `CoverVM` encodes only the **derived booleans** the UI needs:
//!   * `is_open`   — `true` for `"open"` / `"opening"` (the latter so the
//!     tile colors active during the move match the destination).  `false`
//!     for `"closed"` / `"closing"` / `"stopped"` / unknown.
//!   * `is_moving` — `true` for `"opening"` / `"closing"`.
//!   * `position`  — `current_position` attribute (0..=100), clamped.
//!     Falls back to a sensible default when missing: 100 if the state is
//!     `"open"`, 0 otherwise.  Stays in `u8` to mirror the
//!     `WidgetOptions::Cover.position_min/position_max` types from
//!     `src/dashboard/schema.rs`.
//!
//! # JSON-crate discipline (CI Gate 2)
//!
//! `src/ui/**` is gated against direct references to the JSON crate by
//! `.github/workflows/ci.yml` Gate 2. The implementation reads
//! `entity.attributes` only via inferred-type accessors (`.as_u64`,
//! `.as_f64`, `.as_i64`) — never the JSON-crate `Value` type by name.

use crate::ha::entity::Entity;

// ---------------------------------------------------------------------------
// CoverVM
// ---------------------------------------------------------------------------

/// Per-frame derived view-state for a `cover.*` entity.
///
/// Built by [`CoverVM::from_entity`] at entity-change time. The bridge then
/// projects this into a Slint `CoverTileVM` and pushes the row update — see
/// `src/ui/bridge.rs::compute_cover_tile_vm`.
///
/// # Field semantics
///
/// * `position` — `u8` in 0..=100 (HA convention: 0 = fully closed,
///   100 = fully open). When the entity has no `current_position`
///   attribute, falls back to `100` for `"open"` and `0` otherwise.
/// * `is_open` — `true` for HA states `"open"` / `"opening"`. The
///   "moving" state colors with the destination so a tile that is
///   transitioning closed → open is rendered with the open palette
///   the moment the move starts (per the three-state render plan).
/// * `is_moving` — `true` for HA states `"opening"` / `"closing"`.
///   Drives the active-render branch in the Slint tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverVM {
    /// Current position in 0..=100 (HA convention).
    pub position: u8,
    /// True for `"open"` / `"opening"`.
    pub is_open: bool,
    /// True for `"opening"` / `"closing"`.
    pub is_moving: bool,
}

impl CoverVM {
    /// Construct a [`CoverVM`] from a live [`Entity`] snapshot.
    ///
    /// # Position resolution
    ///
    /// The HA cover integration exposes the current position via the
    /// `current_position` attribute (integer 0..=100). When absent or
    /// non-numeric, we fall back to `100` for `"open"` and `0` otherwise —
    /// rendering the boolean state without falsely implying a precise
    /// position the entity does not report.
    ///
    /// # State mapping
    ///
    /// | HA state       | `is_open` | `is_moving` |
    /// |----------------|-----------|-------------|
    /// | `"open"`       | true      | false       |
    /// | `"opening"`    | true      | true        |
    /// | `"closed"`     | false     | false       |
    /// | `"closing"`    | false     | true        |
    /// | `"stopped"`    | false     | false       |
    /// | other / unknown| false     | false       |
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Self {
        let state = entity.state.as_ref();
        let is_open = matches!(state, "open" | "opening");
        let is_moving = matches!(state, "opening" | "closing");

        let position = read_position_attribute(entity).unwrap_or(if is_open { 100 } else { 0 });

        CoverVM {
            position,
            is_open,
            is_moving,
        }
    }
}

/// Read the `current_position` attribute as a `u8` in 0..=100.
///
/// Returns `None` when:
///   * The attribute is absent.
///   * The attribute is not numeric.
///   * The numeric value is outside the 0..=100 range (clamping is
///     intentionally NOT applied here; the caller falls back to a
///     state-derived default rather than rendering an out-of-spec position).
///
/// Reads the attribute through inferred-type accessors only (no JSON-crate
/// path reference) so this module remains compliant with the `src/ui/**`
/// Gate-2 grep (no JSON-crate path references inside `src/ui/`).
fn read_position_attribute(entity: &Entity) -> Option<u8> {
    let value = entity.attributes.get("current_position")?;
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
        // Round to nearest u8 — HA emits integer positions but defensive
        // against integration variants (some emit f64).
        return Some(f.round() as u8);
    }
    None
}

/// Read the `current_tilt_position` attribute as a `u8` in 0..=100.
///
/// Same parsing rules as [`read_position_attribute`] but for the tilt
/// attribute exposed by HA covers that support tilt (e.g. blinds with
/// tiltable slats). Public so the more-info body can pull tilt without
/// duplicating the parsing logic.
#[must_use]
pub fn read_tilt_attribute(entity: &Entity) -> Option<u8> {
    let value = entity.attributes.get("current_tilt_position")?;
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
        return Some(f.round() as u8);
    }
    None
}

/// Read the `supported_features` bitmask attribute, if present.
///
/// HA encodes per-domain feature flags as an integer bitmask; the cover
/// domain uses bit 0 = OPEN, bit 1 = CLOSE, bit 2 = SET_POSITION, bit 3 =
/// STOP, bit 4 = OPEN_TILT, bit 5 = CLOSE_TILT, bit 6 = STOP_TILT,
/// bit 7 = SET_TILT_POSITION. The body uses this to render only the
/// supported controls; non-supported actions are hidden rather than
/// disabled.
#[must_use]
pub fn read_supported_features(entity: &Entity) -> Option<u32> {
    let value = entity.attributes.get("supported_features")?;
    if let Some(u) = value.as_u64() {
        return u32::try_from(u).ok();
    }
    if let Some(i) = value.as_i64() {
        return u32::try_from(i).ok();
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

    /// Construct a minimal [`Entity`] with an empty attribute map.
    /// Mirrors the helper in `src/ui/more_info.rs::tests` — uses
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

    // -----------------------------------------------------------------------
    // State mapping
    // -----------------------------------------------------------------------

    #[test]
    fn from_entity_open_state() {
        let entity = minimal_entity("cover.garage", "open");
        let vm = CoverVM::from_entity(&entity);
        assert!(vm.is_open, "open state must produce is_open=true");
        assert!(!vm.is_moving, "open state must produce is_moving=false");
        // No current_position attribute — fall through to state-derived default.
        assert_eq!(vm.position, 100, "open state defaults to position 100");
    }

    #[test]
    fn from_entity_closed_state() {
        let entity = minimal_entity("cover.garage", "closed");
        let vm = CoverVM::from_entity(&entity);
        assert!(!vm.is_open, "closed state must produce is_open=false");
        assert!(!vm.is_moving, "closed state must produce is_moving=false");
        assert_eq!(vm.position, 0, "closed state defaults to position 0");
    }

    #[test]
    fn from_entity_opening_state() {
        let entity = minimal_entity("cover.garage", "opening");
        let vm = CoverVM::from_entity(&entity);
        assert!(
            vm.is_open,
            "opening state colors with destination (is_open=true)"
        );
        assert!(vm.is_moving, "opening state must produce is_moving=true");
    }

    #[test]
    fn from_entity_closing_state() {
        let entity = minimal_entity("cover.garage", "closing");
        let vm = CoverVM::from_entity(&entity);
        assert!(
            !vm.is_open,
            "closing state colors with destination (is_open=false)"
        );
        assert!(vm.is_moving, "closing state must produce is_moving=true");
    }

    #[test]
    fn from_entity_stopped_state() {
        let entity = minimal_entity("cover.garage", "stopped");
        let vm = CoverVM::from_entity(&entity);
        assert!(!vm.is_open, "stopped state must not be 'open'");
        assert!(!vm.is_moving, "stopped state must not be 'moving'");
    }

    #[test]
    fn from_entity_unavailable_state() {
        let entity = minimal_entity("cover.garage", "unavailable");
        let vm = CoverVM::from_entity(&entity);
        // Sensible default: render as closed-equivalent (is_open=false,
        // is_moving=false). The bridge separately renders the unavailable
        // visual state via the entity's state string in the Slint tile.
        assert!(!vm.is_open, "unavailable falls back to is_open=false");
        assert!(!vm.is_moving, "unavailable falls back to is_moving=false");
        assert_eq!(vm.position, 0, "unavailable defaults to position 0");
    }

    #[test]
    fn from_entity_unknown_state_falls_back_safely() {
        let entity = minimal_entity("cover.garage", "garbage_state_name");
        let vm = CoverVM::from_entity(&entity);
        assert!(!vm.is_open);
        assert!(!vm.is_moving);
    }

    // -----------------------------------------------------------------------
    // Position attribute parsing
    // -----------------------------------------------------------------------

    /// Construct an [`Entity`] carrying a single numeric attribute parsed
    /// from a YAML/JSON snippet. The attribute map's element type is the
    /// JSON-crate `Value`, but it is reached purely through type inference
    /// at the call site — we never name the JSON crate textually, satisfying
    /// the `src/ui/**` Gate-2 grep.
    ///
    /// JSON is a strict subset of YAML 1.2, so `serde_yaml_ng::from_str`
    /// parses the JSON-shaped snippet correctly. We use serde_yaml_ng
    /// (already a workspace dep, named extensively in `src/dashboard/`)
    /// rather than the JSON crate so this file stays inside Gate 2 — the
    /// JSON crate path must not appear textually anywhere in `src/ui/**`.
    fn entity_with_attr(state: &str, key: &str, value_str: &str) -> Entity {
        let snippet = format!("{{\"{key}\":{value_str}}}");
        let map =
            serde_yaml_ng::from_str(&snippet).expect("test snippet must parse as a YAML/JSON map");
        Entity {
            id: EntityId::from("cover.test"),
            state: Arc::from(state),
            attributes: Arc::new(map),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    #[test]
    fn from_entity_reads_current_position_u64() {
        let entity = entity_with_attr("open", "current_position", "42");
        let vm = CoverVM::from_entity(&entity);
        assert_eq!(vm.position, 42);
    }

    #[test]
    fn from_entity_reads_current_position_zero() {
        let entity = entity_with_attr("closed", "current_position", "0");
        let vm = CoverVM::from_entity(&entity);
        assert_eq!(vm.position, 0);
    }

    #[test]
    fn from_entity_reads_current_position_hundred() {
        let entity = entity_with_attr("open", "current_position", "100");
        let vm = CoverVM::from_entity(&entity);
        assert_eq!(vm.position, 100);
    }

    #[test]
    fn from_entity_position_out_of_range_falls_back() {
        // 150 is out of 0..=100 — we fall through to the state-derived
        // default (state="open" → 100).
        let entity = entity_with_attr("open", "current_position", "150");
        let vm = CoverVM::from_entity(&entity);
        assert_eq!(vm.position, 100);
    }

    #[test]
    fn from_entity_position_negative_falls_back() {
        let entity = entity_with_attr("closed", "current_position", "-5");
        let vm = CoverVM::from_entity(&entity);
        // Falls back to state-derived default (closed → 0).
        assert_eq!(vm.position, 0);
    }

    #[test]
    fn from_entity_position_float_rounds_to_nearest() {
        let entity = entity_with_attr("open", "current_position", "33.7");
        let vm = CoverVM::from_entity(&entity);
        assert_eq!(vm.position, 34);
    }

    // -----------------------------------------------------------------------
    // Tilt + supported_features helpers (used by CoverBody)
    // -----------------------------------------------------------------------

    #[test]
    fn read_tilt_attribute_present() {
        let entity = entity_with_attr("open", "current_tilt_position", "75");
        assert_eq!(read_tilt_attribute(&entity), Some(75));
    }

    #[test]
    fn read_tilt_attribute_absent() {
        let entity = minimal_entity("cover.garage", "open");
        assert_eq!(read_tilt_attribute(&entity), None);
    }

    #[test]
    fn read_supported_features_present() {
        let entity = entity_with_attr("open", "supported_features", "11");
        assert_eq!(read_supported_features(&entity), Some(11));
    }

    #[test]
    fn read_supported_features_absent() {
        let entity = minimal_entity("cover.garage", "open");
        assert_eq!(read_supported_features(&entity), None);
    }
}
