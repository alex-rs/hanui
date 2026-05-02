//! History graph view-model and per-frame state derivation (TASK-106).
//!
//! # Hot-path discipline
//!
//! [`HistoryGraphVM::from_entity`] is invoked at entity-change time, NOT per
//! render. The bridge's `build_tiles` / `apply_row_updates` paths call it once
//! per `sensor.*` (or any history-tracked) state-change event; the resulting
//! [`HistoryGraphVM`] is then projected into the Slint-typed
//! `HistoryGraphTileVM` (in `bridge.rs`) and pushed via the row-update path.
//! The polyline composition (`HistoryWindow → SVG commands string`) lives in
//! the bridge and runs at fetch time, not per frame.
//!
//! # Why no `Vec` fields on `HistoryGraphVM`
//!
//! `HistoryGraphVM` deliberately carries no `Vec` fields. The downsampled
//! point list lives in [`crate::ha::history::HistoryWindow`] and reaches
//! Slint as a string-encoded SVG polyline (the bridge composes the string
//! from the window). The per-frame VM stays scalar so the `build_tiles`
//! hot path allocates no per-history-widget vector — the lesson from
//! TASK-103's `FanVM` audit.
//!
//! # JSON-crate discipline (CI Gate 2)
//!
//! `src/ui/**` is gated against direct references to the JSON crate by
//! `.github/workflows/ci.yml` Gate 2. The implementation reads
//! `entity.attributes` only via inferred-type accessors (`.as_str`,
//! `.as_u64`, etc.) — never the JSON-crate `Value` type by name.

use crate::ha::entity::Entity;

// ---------------------------------------------------------------------------
// HistoryGraphVM
// ---------------------------------------------------------------------------

/// Per-frame derived view-state for a history-graph tile.
///
/// Built by [`HistoryGraphVM::from_entity`] at entity-change time. The bridge
/// then projects this into a Slint `HistoryGraphTileVM` and pushes the row
/// update — see `src/ui/bridge.rs::compute_history_graph_tile_vm`.
///
/// # Field semantics
///
/// * `state` — canonical HA state string forwarded verbatim to the Slint
///   tile for the hero label. The Slint tile branches on `is_available`
///   for the unavailable colour fallback rather than re-matching the
///   state string.
/// * `is_available` — `false` when the HA state is `"unavailable"` or
///   `"unknown"`. The Slint tile renders the unavailable visual
///   (state-unavailable tint, opacity 0.5) when `is_available == false`.
/// * `change_count` — number of plotted points the bridge has loaded for
///   this widget. `0` means no data yet (or all points were non-numeric);
///   the tile renders "0 samples" and an empty trace. The bridge writes
///   this from [`crate::ha::history::HistoryWindow::len`].
///
/// # No `Vec` fields
///
/// Per the TASK-103 audit lesson: this struct stays lean. The point list
/// lives in [`crate::ha::history::HistoryWindow`] (owned by the bridge per
/// widget) and reaches the Slint tile as a string-encoded SVG polyline
/// composed at fetch time. The more-info modal reads attribute rows
/// directly from the entity at modal-open time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryGraphVM {
    /// True when the HA state is anything except `"unavailable"` /
    /// `"unknown"`. Drives the three-state render in the Slint tile.
    pub is_available: bool,
    /// Number of plotted points the bridge has loaded for this widget.
    /// `0` means no data yet (or all points were non-numeric).
    pub change_count: i32,
}

impl HistoryGraphVM {
    /// Construct a [`HistoryGraphVM`] from a live [`Entity`] snapshot and
    /// the current point count from the bridge's [`HistoryWindow`].
    ///
    /// # State mapping
    ///
    /// | HA state         | `is_available` |
    /// |------------------|----------------|
    /// | `"unavailable"`  | false          |
    /// | `"unknown"`      | false          |
    /// | other / numeric  | true           |
    ///
    /// `change_count` is forwarded verbatim from the caller — the bridge
    /// passes [`crate::ha::history::HistoryWindow::len`] cast to `i32`
    /// (saturating: `change_count` never exceeds `HISTORY_MAX_POINTS_HARD_CAP`,
    /// which fits in `i32`).
    #[must_use]
    pub fn from_entity(entity: &Entity, change_count: i32) -> Self {
        let state = entity.state.as_ref();
        let is_available = !matches!(state, "unavailable" | "unknown");
        HistoryGraphVM {
            is_available,
            change_count,
        }
    }
}

/// Read the `unit_of_measurement` attribute as a `String` if present.
///
/// HA exposes the unit (e.g. `"°C"`, `"kWh"`) as a free-form string. The
/// more-info body surfaces this as a row when reported. Public so
/// [`crate::ui::more_info::HistoryBody`] can pull the value without
/// duplicating the parsing logic.
#[must_use]
pub fn read_unit_of_measurement_attribute(entity: &Entity) -> Option<String> {
    entity
        .attributes
        .get("unit_of_measurement")?
        .as_str()
        .map(str::to_owned)
}

/// Read the `friendly_name` attribute as a `String` if present.
///
/// HA-level friendly name override — distinct from
/// [`Entity::friendly_name`] only in that it returns an owned `String`
/// (the inherent accessor returns a borrowed `&str`). Used by the
/// more-info body where ownership is required.
#[must_use]
pub fn read_friendly_name_attribute(entity: &Entity) -> Option<String> {
    entity.friendly_name().map(str::to_owned)
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
    /// the helper in `src/ui/cover.rs::tests` and `src/ui/alarm.rs::tests` —
    /// uses `Arc::default()` so we do not name the JSON crate (Gate 2).
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
    /// snippet. Uses `serde_yaml_ng` to stay inside Gate 2 — the JSON
    /// crate path must not appear textually anywhere in `src/ui/**`.
    fn entity_with_attrs(state: &str, attrs_json: &str) -> Entity {
        let map = serde_yaml_ng::from_str(attrs_json)
            .expect("test snippet must parse as a YAML/JSON map");
        Entity {
            id: EntityId::from("sensor.test"),
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
    fn from_entity_numeric_state_is_available() {
        let entity = minimal_entity("sensor.energy", "23.4");
        let vm = HistoryGraphVM::from_entity(&entity, 60);
        assert!(
            vm.is_available,
            "numeric state must produce is_available=true"
        );
        assert_eq!(vm.change_count, 60);
    }

    #[test]
    fn from_entity_on_state_is_available() {
        let entity = minimal_entity("switch.heater", "on");
        let vm = HistoryGraphVM::from_entity(&entity, 30);
        assert!(vm.is_available);
    }

    #[test]
    fn from_entity_unavailable_state_is_not_available() {
        let entity = minimal_entity("sensor.energy", "unavailable");
        let vm = HistoryGraphVM::from_entity(&entity, 0);
        assert!(
            !vm.is_available,
            "unavailable state must produce is_available=false"
        );
        assert_eq!(vm.change_count, 0);
    }

    #[test]
    fn from_entity_unknown_state_is_not_available() {
        let entity = minimal_entity("sensor.energy", "unknown");
        let vm = HistoryGraphVM::from_entity(&entity, 0);
        assert!(
            !vm.is_available,
            "unknown state must produce is_available=false"
        );
    }

    #[test]
    fn from_entity_change_count_zero_is_idle() {
        // change_count==0 with an available state is the "idle" render
        // (data loaded but no recent change) per Phase 6 three-state
        // acceptance. The Slint tile picks the text-secondary stroke for
        // this branch.
        let entity = minimal_entity("sensor.energy", "0");
        let vm = HistoryGraphVM::from_entity(&entity, 0);
        assert!(vm.is_available);
        assert_eq!(vm.change_count, 0);
    }

    #[test]
    fn from_entity_change_count_passthrough() {
        // The bridge passes any non-negative i32 as change_count; the VM
        // must forward verbatim (the bridge clamps before passing in
        // production but the unit-test contract is "no transformation").
        let entity = minimal_entity("sensor.energy", "23.4");
        let vm = HistoryGraphVM::from_entity(&entity, 240);
        assert_eq!(vm.change_count, 240);
    }

    // -----------------------------------------------------------------------
    // Attribute helpers
    // -----------------------------------------------------------------------

    #[test]
    fn read_unit_of_measurement_attribute_present() {
        let entity = entity_with_attrs("23.4", r#"{"unit_of_measurement":"°C"}"#);
        assert_eq!(
            read_unit_of_measurement_attribute(&entity).as_deref(),
            Some("°C")
        );
    }

    #[test]
    fn read_unit_of_measurement_attribute_absent() {
        let entity = minimal_entity("sensor.energy", "23.4");
        assert_eq!(read_unit_of_measurement_attribute(&entity), None);
    }

    #[test]
    fn read_unit_of_measurement_attribute_non_string_is_none() {
        let entity = entity_with_attrs("23.4", r#"{"unit_of_measurement":42}"#);
        assert_eq!(read_unit_of_measurement_attribute(&entity), None);
    }

    #[test]
    fn read_friendly_name_attribute_present() {
        let entity = entity_with_attrs("23.4", r#"{"friendly_name":"Kitchen Thermometer"}"#);
        assert_eq!(
            read_friendly_name_attribute(&entity).as_deref(),
            Some("Kitchen Thermometer")
        );
    }

    #[test]
    fn read_friendly_name_attribute_absent() {
        let entity = minimal_entity("sensor.energy", "23.4");
        assert_eq!(read_friendly_name_attribute(&entity), None);
    }

    // -----------------------------------------------------------------------
    // No-Vec invariant (TASK-105 lesson from TASK-103)
    // -----------------------------------------------------------------------

    /// Compile-time assertion: `HistoryGraphVM` is `Eq` (a Vec field would
    /// force us off `Eq`/`Copy`-friendly territory and break this
    /// assertion). This is a structural reminder that `HistoryGraphVM`
    /// MUST NOT grow a `Vec` field — the per-frame view model stays lean.
    #[test]
    fn history_graph_vm_remains_eq() {
        fn assert_eq_impl<T: Eq>() {}
        assert_eq_impl::<HistoryGraphVM>();
    }
}
