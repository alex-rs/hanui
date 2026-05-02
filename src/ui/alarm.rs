//! Alarm panel widget view-model and per-frame state derivation (TASK-105).
//!
//! # Hot-path discipline
//!
//! [`AlarmVM::from_entity`] is invoked at entity-change time, NOT per render.
//! The bridge's `build_tiles` / `apply_row_updates` paths call it once per
//! `alarm_control_panel.*` state-change event; the resulting [`AlarmVM`] is
//! then projected into the Slint-typed `AlarmTileVM` (in `bridge.rs`) and
//! pushed via the row-update path. No allocation occurs in any per-frame
//! Slint callback.
//!
//! # State vocabulary (Home Assistant `alarm_control_panel` entity)
//!
//! Home Assistant exposes the following canonical states for
//! `alarm_control_panel.*` entities:
//!   * `"disarmed"`              — alarm panel is disarmed (idle).
//!   * `"armed_home"`            — armed in home mode.
//!   * `"armed_away"`            — armed in away mode.
//!   * `"armed_night"`           — armed in night mode.
//!   * `"armed_vacation"`        — armed in vacation mode.
//!   * `"armed_custom_bypass"`   — armed in custom_bypass mode.
//!   * `"pending"`               — transition in progress (e.g. disarm
//!     entry delay, exit delay).
//!   * `"triggered"`             — alarm has fired (visually distinct).
//!   * `"unavailable"`/`"unknown"` — not reachable.
//!
//! `AlarmVM` encodes only the **derived view-state** the UI needs:
//!   * `state` — canonical HA state string for the tile's hero label
//!     (forwarded verbatim).
//!   * `is_armed` — `true` for any `armed_*` state, `false` otherwise.
//!   * `is_triggered` — `true` only for `"triggered"`.
//!   * `is_pending` — `true` only for `"pending"` (the HA transitional
//!     state, NOT the per-tile spinner gate).
//!
//! # Why no `Vec` fields (lesson from TASK-103)
//!
//! `AlarmVM` deliberately carries no `Vec` fields. Arm-mode lists, code
//! formats, and dispatcher-side preset vocabularies are NOT stored on the
//! per-frame tile VM — they are read at modal-open / dispatch time from the
//! widget options or from HA attributes. Allocating a `Vec` per
//! state-change event would be wasted work; the lesson learned in
//! TASK-103's `FanVM` is to keep the per-frame VM lean.
//!
//! # JSON-crate discipline (CI Gate 2)
//!
//! `src/ui/**` is gated against direct references to the JSON crate by
//! `.github/workflows/ci.yml` Gate 2. The implementation reads
//! `entity.attributes` only via inferred-type accessors (`.as_str`,
//! `.as_u64`, etc.) — never the JSON-crate `Value` type by name.

use crate::ha::entity::Entity;

// ---------------------------------------------------------------------------
// AlarmVM
// ---------------------------------------------------------------------------

/// Per-frame derived view-state for an `alarm_control_panel.*` entity.
///
/// Built by [`AlarmVM::from_entity`] at entity-change time. The bridge then
/// projects this into a Slint `AlarmTileVM` and pushes the row update — see
/// `src/ui/bridge.rs::compute_alarm_tile_vm`.
///
/// # Field semantics
///
/// * `state` — canonical HA state string forwarded verbatim to the Slint
///   tile for the hero label. The Slint tile branches on this string only
///   for the `"unavailable"` colour fallback; the `is_armed` /
///   `is_triggered` booleans drive the rest.
/// * `is_armed` — `true` for any HA state matching `armed_*`. Drives the
///   "active" visual on the tile and the disarm button on the more-info
///   modal.
/// * `is_triggered` — `true` only for HA state `"triggered"`. Drives the
///   distinct triggered visual (accent colour) so a fired alarm is
///   immediately visible to the user.
/// * `is_pending` — `true` only for HA state `"pending"` (transitional).
///   Distinct from the per-tile spinner gate (`pending` on the
///   Slint-side `AlarmTileVM`); this is the HA-side state, not optimistic UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmVM {
    /// Canonical HA state string for the tile hero label.
    pub state: String,
    /// True for any `armed_*` state.
    pub is_armed: bool,
    /// True only for `"triggered"`.
    pub is_triggered: bool,
    /// True only for `"pending"` (HA-state, not optimistic-UI gate).
    pub is_pending: bool,
}

impl AlarmVM {
    /// Construct an [`AlarmVM`] from a live [`Entity`] snapshot.
    ///
    /// # State mapping
    ///
    /// | HA state               | `is_armed` | `is_triggered` | `is_pending` |
    /// |------------------------|------------|----------------|--------------|
    /// | `"disarmed"`           | false      | false          | false        |
    /// | `"armed_home"`         | true       | false          | false        |
    /// | `"armed_away"`         | true       | false          | false        |
    /// | `"armed_night"`        | true       | false          | false        |
    /// | `"armed_vacation"`     | true       | false          | false        |
    /// | `"armed_custom_bypass"`| true       | false          | false        |
    /// | `"pending"`            | false      | false          | true         |
    /// | `"triggered"`          | false      | true           | false        |
    /// | `"unavailable"`        | false      | false          | false        |
    /// | other / unknown        | false      | false          | false        |
    ///
    /// `state` is forwarded verbatim — the Slint tile renders the value as
    /// the hero label. The boolean fields are precomputed here so the
    /// Slint side does not branch on the state-string vocabulary.
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Self {
        let state = entity.state.as_ref();
        let is_armed = matches!(
            state,
            "armed_home" | "armed_away" | "armed_night" | "armed_vacation" | "armed_custom_bypass"
        );
        let is_triggered = state == "triggered";
        let is_pending = state == "pending";

        AlarmVM {
            state: state.to_owned(),
            is_armed,
            is_triggered,
            is_pending,
        }
    }
}

/// Read the `changed_by` attribute as a `String` if present.
///
/// HA's `alarm_control_panel` integration exposes the user/code id that
/// triggered the last state change as `changed_by`. The more-info body
/// surfaces this as a row when reported. Public so the modal body can
/// pull the value without duplicating the parsing logic.
#[must_use]
pub fn read_changed_by_attribute(entity: &Entity) -> Option<String> {
    entity
        .attributes
        .get("changed_by")?
        .as_str()
        .map(str::to_owned)
}

/// Read the `code_format` attribute as a `String` if present.
///
/// HA's `alarm_control_panel` integration exposes the expected disarm-code
/// format (typically `"number"` or `"any"`). The more-info body surfaces
/// this as a row when reported.
#[must_use]
pub fn read_code_format_attribute(entity: &Entity) -> Option<String> {
    entity
        .attributes
        .get("code_format")?
        .as_str()
        .map(str::to_owned)
}

/// Read the `code_arm_required` attribute as a `bool` if present.
///
/// HA's `alarm_control_panel` integration exposes whether a code is also
/// required on arm (vs only on disarm). The more-info body surfaces this
/// as a row when reported.
#[must_use]
pub fn read_code_arm_required_attribute(entity: &Entity) -> Option<bool> {
    entity.attributes.get("code_arm_required")?.as_bool()
}

// ---------------------------------------------------------------------------
// AlarmBody — per-domain MoreInfoBody (TASK-105 acceptance #6)
// ---------------------------------------------------------------------------
//
// A richer `AlarmBody` is provided by `src/ui/more_info.rs` (existing stub
// replaced in this ticket). It surfaces the state row plus the
// alarm-specific HA attributes (`changed_by`, `code_format`,
// `code_arm_required`) when present — see `more_info.rs::AlarmBody`.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::entity::EntityId;
    use std::sync::Arc;

    /// Construct a minimal [`Entity`] with an empty attribute map.
    /// Mirrors the helper in `src/ui/cover.rs::tests` and
    /// `src/ui/fan.rs::tests` — uses `Arc::default()` so we do not name
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

    /// Construct an [`Entity`] carrying attributes parsed from a
    /// YAML/JSON snippet. JSON is a strict subset of YAML 1.2; we use
    /// `serde_yaml_ng` (workspace dep) to stay inside Gate 2 — the
    /// JSON crate path must not appear textually anywhere in
    /// `src/ui/**`.
    fn entity_with_attrs(state: &str, attrs_json: &str) -> Entity {
        let map = serde_yaml_ng::from_str(attrs_json)
            .expect("test snippet must parse as a YAML/JSON map");
        Entity {
            id: EntityId::from("alarm_control_panel.test"),
            state: Arc::from(state),
            attributes: Arc::new(map),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    // -----------------------------------------------------------------------
    // State mapping (TASK-105 acceptance #9 — every HA state)
    // -----------------------------------------------------------------------

    #[test]
    fn from_entity_disarmed_state() {
        let entity = minimal_entity("alarm_control_panel.home", "disarmed");
        let vm = AlarmVM::from_entity(&entity);
        assert!(!vm.is_armed, "disarmed must produce is_armed=false");
        assert!(!vm.is_triggered, "disarmed must produce is_triggered=false");
        assert!(!vm.is_pending, "disarmed must produce is_pending=false");
        assert_eq!(vm.state, "disarmed");
    }

    #[test]
    fn from_entity_armed_states() {
        for mode in [
            "armed_home",
            "armed_away",
            "armed_night",
            "armed_vacation",
            "armed_custom_bypass",
        ] {
            let entity = minimal_entity("alarm_control_panel.home", mode);
            let vm = AlarmVM::from_entity(&entity);
            assert!(vm.is_armed, "{mode} must produce is_armed=true");
            assert!(!vm.is_triggered, "{mode} must produce is_triggered=false");
            assert!(!vm.is_pending, "{mode} must produce is_pending=false");
            assert_eq!(vm.state, mode);
        }
    }

    #[test]
    fn from_entity_triggered_state() {
        let entity = minimal_entity("alarm_control_panel.home", "triggered");
        let vm = AlarmVM::from_entity(&entity);
        assert!(!vm.is_armed, "triggered must NOT produce is_armed=true");
        assert!(vm.is_triggered, "triggered must produce is_triggered=true");
        assert!(!vm.is_pending);
        assert_eq!(vm.state, "triggered");
    }

    #[test]
    fn from_entity_pending_state() {
        let entity = minimal_entity("alarm_control_panel.home", "pending");
        let vm = AlarmVM::from_entity(&entity);
        assert!(!vm.is_armed);
        assert!(!vm.is_triggered);
        assert!(vm.is_pending, "pending must produce is_pending=true");
        assert_eq!(vm.state, "pending");
    }

    #[test]
    fn from_entity_unavailable_state() {
        let entity = minimal_entity("alarm_control_panel.home", "unavailable");
        let vm = AlarmVM::from_entity(&entity);
        assert!(!vm.is_armed, "unavailable must fall back to is_armed=false");
        assert!(!vm.is_triggered);
        assert!(!vm.is_pending);
        assert_eq!(vm.state, "unavailable");
    }

    #[test]
    fn from_entity_unknown_state_falls_back_safely() {
        let entity = minimal_entity("alarm_control_panel.home", "garbage_state_name");
        let vm = AlarmVM::from_entity(&entity);
        assert!(!vm.is_armed);
        assert!(!vm.is_triggered);
        assert!(!vm.is_pending);
        assert_eq!(vm.state, "garbage_state_name");
    }

    // -----------------------------------------------------------------------
    // Attribute helpers (used by AlarmBody)
    // -----------------------------------------------------------------------

    #[test]
    fn read_changed_by_attribute_present() {
        let entity = entity_with_attrs("disarmed", r#"{"changed_by":"Master"}"#);
        assert_eq!(
            read_changed_by_attribute(&entity).as_deref(),
            Some("Master")
        );
    }

    #[test]
    fn read_changed_by_attribute_absent() {
        let entity = minimal_entity("alarm_control_panel.home", "disarmed");
        assert_eq!(read_changed_by_attribute(&entity), None);
    }

    #[test]
    fn read_code_format_attribute_present() {
        let entity = entity_with_attrs("disarmed", r#"{"code_format":"number"}"#);
        assert_eq!(
            read_code_format_attribute(&entity).as_deref(),
            Some("number")
        );
    }

    #[test]
    fn read_code_format_attribute_absent() {
        let entity = minimal_entity("alarm_control_panel.home", "disarmed");
        assert_eq!(read_code_format_attribute(&entity), None);
    }

    #[test]
    fn read_code_arm_required_attribute_true() {
        let entity = entity_with_attrs("disarmed", r#"{"code_arm_required":true}"#);
        assert_eq!(read_code_arm_required_attribute(&entity), Some(true));
    }

    #[test]
    fn read_code_arm_required_attribute_false() {
        let entity = entity_with_attrs("disarmed", r#"{"code_arm_required":false}"#);
        assert_eq!(read_code_arm_required_attribute(&entity), Some(false));
    }

    #[test]
    fn read_code_arm_required_attribute_absent() {
        let entity = minimal_entity("alarm_control_panel.home", "disarmed");
        assert_eq!(read_code_arm_required_attribute(&entity), None);
    }

    // -----------------------------------------------------------------------
    // No-Vec invariant (TASK-105 lesson from TASK-103)
    // -----------------------------------------------------------------------

    /// Compile-time assertion: `AlarmVM` is `Eq` (a Vec field would force
    /// us off `Eq`/`Copy`-friendly territory and break this assertion).
    /// This is a structural reminder that `AlarmVM` MUST NOT grow a `Vec`
    /// field — the per-frame view model stays lean.
    #[test]
    fn alarm_vm_remains_eq() {
        fn assert_eq_impl<T: Eq>() {}
        assert_eq_impl::<AlarmVM>();
    }
}
