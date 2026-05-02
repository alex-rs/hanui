//! Camera widget view-model and per-frame state derivation (TASK-107).
//!
//! # Hot-path discipline
//!
//! [`CameraVM::from_entity`] is invoked at entity-change time, NOT per
//! render. The bridge's `build_tiles` / `apply_row_updates` paths call it
//! once per `camera.*` state-change event; the resulting [`CameraVM`] is
//! then projected into the Slint-typed `CameraTileVM` (in `bridge.rs`) and
//! pushed via the row-update path. No allocation occurs in any per-frame
//! Slint callback.
//!
//! The struct keeps only **scalar** fields needed by the tile renderer.
//! Per the TASK-103 audit lesson: anything that is only consumed by the
//! more-info modal lives in [`crate::ui::more_info::CameraBody`] which
//! reads attributes directly from the entity at modal-open time. There is
//! no `Vec`-allocating field on this struct (lesson from TASK-103/105).
//!
//! # State vocabulary (Home Assistant `camera.*` entity)
//!
//! Home Assistant exposes the following canonical states for `camera.*`
//! entities:
//!   * `"idle"`         — camera is reachable, not actively recording or
//!     streaming.
//!   * `"recording"`    — camera is actively recording (the typical NVR
//!     "armed" state).
//!   * `"streaming"`    — camera is delivering a live MJPEG/H.264 stream.
//!   * `"unavailable"` / `"unknown"` — not reachable.
//!
//! Vendor-specific states (some integrations emit free-form labels) are
//! forwarded verbatim — the Slint tile renders the raw string with the
//! `idle` colour fallback.
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
// CameraVM
// ---------------------------------------------------------------------------

/// Per-frame derived view-state for a `camera.*` entity.
///
/// Built by [`CameraVM::from_entity`] at entity-change time. The bridge then
/// projects this into a Slint `CameraTileVM` and pushes the row update — see
/// `src/ui/bridge.rs::compute_camera_tile_vm`.
///
/// # Field semantics
///
/// * `state` — the canonical HA state string forwarded verbatim to the
///   Slint tile for the placeholder label. The Slint tile branches on
///   `is_available` for the unavailable-colour fallback.
/// * `is_recording` — `true` only for HA state `"recording"`. Drives the
///   "active" (accent-tinted) icon variant.
/// * `is_streaming` — `true` only for HA state `"streaming"`. Drives the
///   "active" (state-on-tinted) icon variant.
/// * `is_available` — `false` when state is `"unavailable"` or `"unknown"`;
///   drives the dim-opacity / unavailable-colour branch.
///
/// # No `Vec` fields (lesson from TASK-103 / TASK-105)
///
/// The tile VM stays lean. The decoder pool's image bytes live in
/// [`crate::ha::camera::CameraPool`] and reach the Slint side later via the
/// `Image` property wiring — they do NOT travel through `CameraVM`.
/// Allocating a `Vec` here that the tile never reads would be wasted work
/// on every state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraVM {
    /// True only for HA state `"recording"`.
    pub is_recording: bool,
    /// True only for HA state `"streaming"`.
    pub is_streaming: bool,
    /// True for any state except `"unavailable"` / `"unknown"`.
    pub is_available: bool,
}

impl CameraVM {
    /// Construct a [`CameraVM`] from a live [`Entity`] snapshot.
    ///
    /// # State mapping
    ///
    /// | HA state         | `is_recording` | `is_streaming` | `is_available` |
    /// |------------------|----------------|----------------|----------------|
    /// | `"idle"`         | false          | false          | true           |
    /// | `"recording"`    | true           | false          | true           |
    /// | `"streaming"`    | false          | true           | true           |
    /// | `"unavailable"`  | false          | false          | false          |
    /// | `"unknown"`      | false          | false          | false          |
    /// | other / vendor   | false          | false          | true           |
    ///
    /// `state` is read off the entity by the bridge (it owns the string
    /// allocation). `CameraVM` itself stays fully `Copy` so per-state-change
    /// rebuilds allocate nothing.
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Self {
        let state = entity.state.as_ref();
        let is_recording = state == "recording";
        let is_streaming = state == "streaming";
        let is_available = !matches!(state, "unavailable" | "unknown");
        CameraVM {
            is_recording,
            is_streaming,
            is_available,
        }
    }
}

// ---------------------------------------------------------------------------
// Attribute accessors (read by `more_info::CameraBody`)
// ---------------------------------------------------------------------------

/// Read the `entity_picture` attribute as a `String` if present.
///
/// HA's camera integration exposes the snapshot URL via `entity_picture`
/// (a relative path like `/api/camera_proxy/camera.front_door?token=…`).
/// The more-info body surfaces a "snapshot URL is set" indicator WITHOUT
/// logging the URL itself: per `CLAUDE.md` security rules, the URL may
/// embed a short-lived access token and must NOT appear in tracing output.
/// This accessor returns the raw string so [`crate::ui::more_info::CameraBody`]
/// can decide what to surface; logging it is the caller's responsibility
/// to avoid.
#[must_use]
pub fn read_entity_picture_attribute(entity: &Entity) -> Option<String> {
    entity
        .attributes
        .get("entity_picture")?
        .as_str()
        .map(str::to_owned)
}

/// Read the `last_motion` attribute as a `String` if present.
///
/// HA's camera integrations (e.g. `generic`, `unifi_protect`) expose the
/// timestamp of the last detected motion as `last_motion`. The more-info
/// body surfaces this verbatim so users can see when the camera last saw
/// activity. Public so [`crate::ui::more_info::CameraBody`] can pull the
/// value without duplicating the parsing logic.
#[must_use]
pub fn read_last_motion_attribute(entity: &Entity) -> Option<String> {
    entity
        .attributes
        .get("last_motion")?
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

/// Read the `brand` attribute as a `String` if present.
///
/// Some HA camera integrations (e.g. `generic`, `nest`) expose the
/// camera's brand/manufacturer as `brand`. The more-info body surfaces
/// this verbatim. Public so [`crate::ui::more_info::CameraBody`] can
/// pull the value without duplicating the parsing logic.
#[must_use]
pub fn read_brand_attribute(entity: &Entity) -> Option<String> {
    entity.attributes.get("brand")?.as_str().map(str::to_owned)
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
    /// the helper in `src/ui/cover.rs::tests` and `src/ui/alarm.rs::tests`
    /// — uses `Arc::default()` so we do not name the JSON crate (Gate 2).
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
            id: EntityId::from("camera.test"),
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
    fn from_entity_idle_state_is_available_not_active() {
        let entity = minimal_entity("camera.front_door", "idle");
        let vm = CameraVM::from_entity(&entity);
        assert!(vm.is_available, "idle state must produce is_available=true");
        assert!(
            !vm.is_recording,
            "idle state must produce is_recording=false"
        );
        assert!(
            !vm.is_streaming,
            "idle state must produce is_streaming=false"
        );
    }

    #[test]
    fn from_entity_recording_state() {
        let entity = minimal_entity("camera.front_door", "recording");
        let vm = CameraVM::from_entity(&entity);
        assert!(vm.is_available);
        assert!(
            vm.is_recording,
            "recording state must produce is_recording=true"
        );
        assert!(
            !vm.is_streaming,
            "recording state must NOT also produce is_streaming=true"
        );
    }

    #[test]
    fn from_entity_streaming_state() {
        let entity = minimal_entity("camera.front_door", "streaming");
        let vm = CameraVM::from_entity(&entity);
        assert!(vm.is_available);
        assert!(!vm.is_recording);
        assert!(
            vm.is_streaming,
            "streaming state must produce is_streaming=true"
        );
    }

    #[test]
    fn from_entity_unavailable_state() {
        let entity = minimal_entity("camera.front_door", "unavailable");
        let vm = CameraVM::from_entity(&entity);
        assert!(
            !vm.is_available,
            "unavailable state must produce is_available=false"
        );
        assert!(!vm.is_recording);
        assert!(!vm.is_streaming);
    }

    #[test]
    fn from_entity_unknown_state() {
        let entity = minimal_entity("camera.front_door", "unknown");
        let vm = CameraVM::from_entity(&entity);
        assert!(
            !vm.is_available,
            "unknown state must produce is_available=false"
        );
    }

    #[test]
    fn from_entity_vendor_specific_state_is_available() {
        // Some integrations emit non-canonical states like "online" or
        // "armed_home". They are forwarded as available, idle.
        let entity = minimal_entity("camera.front_door", "online");
        let vm = CameraVM::from_entity(&entity);
        assert!(vm.is_available);
        assert!(!vm.is_recording);
        assert!(!vm.is_streaming);
    }

    // -----------------------------------------------------------------------
    // Attribute accessors
    // -----------------------------------------------------------------------

    #[test]
    fn read_entity_picture_attribute_present() {
        let entity = entity_with_attrs(
            "idle",
            r#"{"entity_picture":"/api/camera_proxy/camera.front_door"}"#,
        );
        assert_eq!(
            read_entity_picture_attribute(&entity).as_deref(),
            Some("/api/camera_proxy/camera.front_door")
        );
    }

    #[test]
    fn read_entity_picture_attribute_absent() {
        let entity = minimal_entity("camera.front_door", "idle");
        assert_eq!(read_entity_picture_attribute(&entity), None);
    }

    #[test]
    fn read_entity_picture_attribute_non_string_is_none() {
        let entity = entity_with_attrs("idle", r#"{"entity_picture":42}"#);
        assert_eq!(read_entity_picture_attribute(&entity), None);
    }

    #[test]
    fn read_last_motion_attribute_present() {
        let entity = entity_with_attrs("idle", r#"{"last_motion":"2026-04-30T12:00:00Z"}"#);
        assert_eq!(
            read_last_motion_attribute(&entity).as_deref(),
            Some("2026-04-30T12:00:00Z")
        );
    }

    #[test]
    fn read_last_motion_attribute_absent() {
        let entity = minimal_entity("camera.front_door", "idle");
        assert_eq!(read_last_motion_attribute(&entity), None);
    }

    #[test]
    fn read_friendly_name_attribute_present() {
        let entity = entity_with_attrs("idle", r#"{"friendly_name":"Front Door Camera"}"#);
        assert_eq!(
            read_friendly_name_attribute(&entity).as_deref(),
            Some("Front Door Camera")
        );
    }

    #[test]
    fn read_friendly_name_attribute_absent() {
        let entity = minimal_entity("camera.front_door", "idle");
        assert_eq!(read_friendly_name_attribute(&entity), None);
    }

    #[test]
    fn read_brand_attribute_present() {
        let entity = entity_with_attrs("idle", r#"{"brand":"Reolink"}"#);
        assert_eq!(read_brand_attribute(&entity).as_deref(), Some("Reolink"));
    }

    #[test]
    fn read_brand_attribute_absent() {
        let entity = minimal_entity("camera.front_door", "idle");
        assert_eq!(read_brand_attribute(&entity), None);
    }

    // -----------------------------------------------------------------------
    // No-Vec invariant (TASK-103 / TASK-105 lesson)
    // -----------------------------------------------------------------------

    /// Compile-time assertion: `CameraVM` is `Copy` (a `Vec` field would
    /// force us off `Copy` territory and break this assertion). This is a
    /// structural reminder that `CameraVM` MUST NOT grow a `Vec` field —
    /// the per-frame view model stays lean.
    #[test]
    fn camera_vm_remains_copy() {
        fn assert_copy_impl<T: Copy>() {}
        assert_copy_impl::<CameraVM>();
    }

    /// Compile-time assertion: `CameraVM` is `Eq` (preserved for hashable /
    /// dedup paths the bridge may add later). A Vec field would not have
    /// `Eq` for `f64` neighbours, so this guards against that drift too.
    #[test]
    fn camera_vm_remains_eq() {
        fn assert_eq_impl<T: Eq>() {}
        assert_eq_impl::<CameraVM>();
    }
}
