//! Canonical [`Action`] schema for Phase 3 + Phase 6 (write/command path).
//!
//! This module is the **single source of truth** for the typed user-interaction
//! action surface. It supersedes the placeholder enum that previously lived in
//! `src/dashboard/view_spec.rs`, which is deleted in the same commit per
//! `docs/plans/2026-04-28-phase-3-actions.md` `locked_decisions.phase4_forward_compat`.
//!
//! # Wire shape (YAML / JSON)
//!
//! The enum is **internally tagged** by the discriminator field `action`,
//! matching the `docs/DASHBOARD_SCHEMA.md` YAML shape:
//!
//! ```yaml
//! tap_action:
//!   action: toggle
//! hold_action:
//!   action: more-info
//! double_tap_action:
//!   action: call-service
//!   domain: light
//!   service: turn_on
//!   target: light.kitchen
//! ```
//!
//! Variant tags are explicit kebab-case names matching the YAML literals; per
//! `locked_decisions.phase4_forward_compat` these renames are required from
//! day one so Phase 4 can wire the YAML loader without touching the dispatcher.
//! Each variant's struct fields use `#[serde(rename_all = "kebab-case")]` so
//! `view_id` → `view-id`, `domain` → `domain` (already kebab-safe), etc.
//!
//! # Idempotency
//!
//! Each variant exposes an [`Idempotency`] marker accessible via
//! [`Action::idempotency`]. The offline action queue (TASK-065) reads this
//! marker at runtime to reject non-idempotent actions (`Toggle`, `Url`) and
//! enqueue idempotent ones for reconnect-flush. Per
//! `locked_decisions.idempotency_marker`, `CallService` is **context-dependent**
//! and is shipped here as `Idempotent` placeholder; the runtime allowlist
//! (`turn_on`, `turn_off`, `set_*`) check is TASK-065's responsibility.
//!
//! # Phase 6 variants (TASK-099)
//!
//! Phase 6 adds typed setpoint/position/transport/lock/alarm variants. Each
//! has an explicit idempotency marker per
//! `locked_decisions.idempotency_marker_phase6_variants`. The dispatcher wiring
//! for these variants lives in TASK-102..TASK-105, TASK-108, TASK-109; this
//! module lands the types and the service map (`service_map.rs`) lands the
//! `(domain, service, body)` mappings.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Idempotency marker
// ---------------------------------------------------------------------------

/// Whether an [`Action`] variant is safe to retry / queue offline.
///
/// `Idempotent`: the operation has the same observable effect after one or
/// many invocations. The offline action queue may enqueue and replay these
/// on reconnect.
///
/// `NonIdempotent`: invoking twice produces two distinct observable effects
/// (state flip, external side-effect, navigation). The offline queue refuses
/// to enqueue these and surfaces an error toast immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idempotency {
    Idempotent,
    NonIdempotent,
}

// ---------------------------------------------------------------------------
// MediaTransport enum
// ---------------------------------------------------------------------------

/// Media transport operation for [`Action::MediaTransport`].
///
/// `Play`, `Pause`, `Stop` are **idempotent** — calling play on an already-
/// playing player is a no-op in HA.
///
/// `Next` and `Prev` are **non-idempotent** — each invocation advances or
/// reverses the track index; replaying on reconnect would skip a track.
///
/// Wire values are lowercase per `locked_decisions.idempotency_marker_phase6_variants`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaTransportOp {
    Play,
    Pause,
    Stop,
    Next,
    Prev,
}

// ---------------------------------------------------------------------------
// ActionError
// ---------------------------------------------------------------------------

/// Errors produced at action-dispatch time for Phase 6 service variants.
///
/// These are returned by `service_map::action_to_service_call` when the
/// caller supplies an unknown mode/speed string. The dispatcher (TASK-103,
/// TASK-104, TASK-105, TASK-108, TASK-109) surfaces these as toast events.
///
/// `UnknownAlarmArmMode` and `UnknownFanSpeed` are landed here in TASK-099
/// so the type is available for wiring in the consumer tickets without
/// requiring a separate type-only PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    /// `AlarmArm.mode` was not one of the HA-standard arm modes
    /// (`home`, `away`, `night`, `vacation`, `custom_bypass`).
    ///
    /// The invalid mode string is carried for diagnostic surfacing in a toast.
    /// Per `locked_decisions.alarm_arm_service_vocabulary`, the set of valid
    /// modes is the HA vocabulary; unknown values are rejected rather than
    /// forwarded to avoid silently mis-arming.
    UnknownAlarmArmMode(String),

    /// `SetFanSpeed.speed` was not found in the available preset/speed
    /// vocabulary. The dispatcher reads `FanOptions.preset_modes` at dispatch
    /// time (TASK-108) to determine the data field; an unrecognised speed
    /// surfaces this error as a toast per
    /// `locked_decisions.fan_speed_set_vocabulary`.
    UnknownFanSpeed(String),
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionError::UnknownAlarmArmMode(mode) => {
                write!(
                    f,
                    "unknown alarm arm mode `{mode}`; expected one of: \
                     home, away, night, vacation, custom_bypass"
                )
            }
            ActionError::UnknownFanSpeed(speed) => {
                write!(
                    f,
                    "unknown fan speed `{speed}`; not found in preset_modes vocabulary"
                )
            }
        }
    }
}

impl std::error::Error for ActionError {}

// ---------------------------------------------------------------------------
// Action enum
// ---------------------------------------------------------------------------

/// A typed user-interaction action.
///
/// See module-level docs for the wire shape and the
/// `locked_decisions.phase4_forward_compat` discussion of why every variant
/// carries an explicit `#[serde(rename = "...")]`.
///
/// Phase 6 variants (TASK-099) add typed setpoint/position/transport/lock/
/// alarm actions. Each has an explicit idempotency marker and an HA service
/// mapping in `service_map.rs`. Dispatcher wiring is deferred to
/// TASK-102..TASK-105, TASK-108, TASK-109.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum Action {
    /// Toggle the entity's primary state.
    ///
    /// Non-idempotent: a second invocation flips the state back. Never queued.
    #[serde(rename = "toggle")]
    Toggle,

    /// Invoke a Home Assistant service explicitly.
    ///
    /// Field shape mirrors the HA `call_service` WS frame body. `target` is the
    /// optional entity target; `data` carries arbitrary service-specific
    /// parameters (passed through to HA verbatim, validated by HA itself).
    ///
    /// Idempotency is **context-dependent** and resolved at runtime in TASK-065
    /// against an allowlist (`turn_on`, `turn_off`, `set_*`). The const marker
    /// here ships as `Idempotent` as a placeholder per
    /// `locked_decisions.idempotency_marker`; the runtime allowlist check is
    /// the actual gate.
    #[serde(rename = "call-service")]
    #[serde(rename_all = "kebab-case")]
    CallService {
        domain: String,
        service: String,
        /// Optional entity target. `None` means the service supplies its own
        /// target (area, label, or service-default).
        target: Option<String>,
        /// Free-form service data passed through to HA verbatim.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },

    /// Open the entity's more-info modal.
    ///
    /// Idempotent: opening twice shows the same modal.
    #[serde(rename = "more-info")]
    MoreInfo,

    /// Navigate to a named view.
    ///
    /// Idempotent: navigating to the current view is a no-op (TASK-068).
    /// Field rename: `view_id` → `view-id` on the wire.
    #[serde(rename = "navigate")]
    #[serde(rename_all = "kebab-case")]
    Navigate { view_id: String },

    /// Open an external URL via `xdg-open`.
    ///
    /// Non-idempotent: every invocation spawns a new external process. Gated
    /// by `DeviceProfile.url_action_mode` per `locked_decisions.url_action_gating`
    /// (TASK-063).
    #[serde(rename = "url")]
    Url { href: String },

    /// Suppress the default interaction.
    ///
    /// Idempotent: no side-effects at all.
    #[serde(rename = "none")]
    None,

    // -----------------------------------------------------------------------
    // Phase 6 variants (TASK-099) — typed setpoint / position / transport /
    // lock / alarm actions with explicit idempotency markers and HA service
    // mappings in `service_map.rs`. Dispatcher wiring is TASK-102..TASK-109.
    // -----------------------------------------------------------------------
    /// Set a climate entity's target temperature.
    ///
    /// Maps to `climate.set_temperature`. Idempotent: setting the same
    /// temperature twice leaves the entity in the same state.
    ///
    /// Wire: `{"action":"set-temperature","entity-id":"...","temperature":21.5}`
    #[serde(rename = "set-temperature")]
    #[serde(rename_all = "kebab-case")]
    SetTemperature {
        entity_id: String,
        /// Target temperature in degrees Celsius.
        temperature: f32,
    },

    /// Set a climate entity's HVAC operating mode.
    ///
    /// Maps to `climate.set_hvac_mode`. Idempotent: setting the same mode
    /// twice is a no-op. `mode` is a free `String` per
    /// `locked_decisions.hvac_mode_vocabulary`; standard HA values are
    /// documented in `service_map.rs::STANDARD_HVAC_MODES`.
    ///
    /// Wire: `{"action":"set-hvac-mode","entity-id":"...","mode":"heat"}`
    #[serde(rename = "set-hvac-mode")]
    #[serde(rename_all = "kebab-case")]
    SetHvacMode {
        entity_id: String,
        /// HVAC mode string (free, HA-validated). See `STANDARD_HVAC_MODES`
        /// in `service_map.rs` for the standard vocabulary.
        mode: String,
    },

    /// Set a media player's volume level.
    ///
    /// Maps to `media_player.volume_set`. Idempotent: setting the same volume
    /// twice leaves the entity at the same level.
    ///
    /// Wire: `{"action":"set-media-volume","entity-id":"...","volume-level":0.5}`
    #[serde(rename = "set-media-volume")]
    #[serde(rename_all = "kebab-case")]
    SetMediaVolume {
        entity_id: String,
        /// Volume level in range [0.0, 1.0].
        volume_level: f32,
    },

    /// Issue a media transport command (play/pause/stop/next/prev).
    ///
    /// Service mapping is variant-specific (see `service_map.rs`):
    /// - `Play` → `media_player.media_play`
    /// - `Pause` → `media_player.media_pause`
    /// - `Stop` → `media_player.media_stop`
    /// - `Next` → `media_player.media_next_track`
    /// - `Prev` → `media_player.media_previous_track`
    ///
    /// Idempotency per `locked_decisions.idempotency_marker_phase6_variants`:
    /// `Play`, `Pause`, `Stop` are idempotent; `Next` and `Prev` are
    /// non-idempotent (each invocation advances/reverses the track).
    ///
    /// Wire: `{"action":"media-transport","entity-id":"...","transport":"play"}`
    #[serde(rename = "media-transport")]
    #[serde(rename_all = "kebab-case")]
    MediaTransport {
        entity_id: String,
        transport: MediaTransportOp,
    },

    /// Set a cover entity's position.
    ///
    /// Maps to `cover.set_cover_position`. Idempotent: setting the same
    /// position twice is a no-op. `position` is bounded 0..=100 per
    /// `locked_decisions.cover_position_bounds`; the slider UI enforces
    /// this at render time — HA rejects out-of-range values via service
    /// error without dispatcher pre-validation.
    ///
    /// Wire: `{"action":"set-cover-position","entity-id":"...","position":50}`
    #[serde(rename = "set-cover-position")]
    #[serde(rename_all = "kebab-case")]
    SetCoverPosition {
        entity_id: String,
        /// Cover position in range 0..=100 (0 = closed, 100 = open).
        position: u8,
    },

    /// Set a fan entity's speed/preset.
    ///
    /// Maps to `fan.turn_on` with either `preset_mode` or `speed_step` data
    /// per `locked_decisions.fan_speed_set_vocabulary`. `speed` is a free
    /// `String`; the dispatcher reads `FanOptions.preset_modes` at dispatch
    /// time (TASK-108) to choose the data field. Unknown speeds return
    /// [`ActionError::UnknownFanSpeed`].
    ///
    /// Wire: `{"action":"set-fan-speed","entity-id":"...","speed":"high"}`
    #[serde(rename = "set-fan-speed")]
    #[serde(rename_all = "kebab-case")]
    SetFanSpeed {
        entity_id: String,
        /// Fan speed or preset mode string (free, HA-validated at dispatch).
        speed: String,
    },

    /// Lock a lock entity.
    ///
    /// Maps to `lock.lock`. Idempotent: locking an already-locked entity is
    /// a no-op. Per `locked_decisions.confirmation_on_lock_unlock`, the
    /// confirmation flag lives on `WidgetOptions::Lock.require_confirmation_on_unlock`,
    /// NOT on this action variant — offline replay does not show a confirm
    /// modal.
    ///
    /// Wire: `{"action":"lock","entity-id":"..."}`
    #[serde(rename = "lock")]
    #[serde(rename_all = "kebab-case")]
    Lock { entity_id: String },

    /// Unlock a lock entity.
    ///
    /// Maps to `lock.unlock`. Idempotent: unlocking an already-unlocked
    /// entity is a no-op. Per `locked_decisions.confirmation_on_lock_unlock`,
    /// confirmation is a widget-options concern, not an action-wire concern.
    ///
    /// Wire: `{"action":"unlock","entity-id":"..."}`
    #[serde(rename = "unlock")]
    #[serde(rename_all = "kebab-case")]
    Unlock { entity_id: String },

    /// Arm an alarm control panel in the specified mode.
    ///
    /// Service varies per `mode` per `locked_decisions.alarm_arm_service_vocabulary`:
    /// `home` → `alarm_control_panel.alarm_arm_home`, etc. Unknown `mode`
    /// returns [`ActionError::UnknownAlarmArmMode`]. Idempotent: arming an
    /// already-armed panel in the same mode is a no-op.
    ///
    /// Wire: `{"action":"alarm-arm","entity-id":"...","mode":"home"}`
    #[serde(rename = "alarm-arm")]
    #[serde(rename_all = "kebab-case")]
    AlarmArm {
        entity_id: String,
        /// Arm mode: `home`, `away`, `night`, `vacation`, `custom_bypass`.
        mode: String,
    },

    /// Disarm an alarm control panel.
    ///
    /// Maps to `alarm_control_panel.alarm_disarm`. Idempotent: disarming an
    /// already-disarmed panel is a no-op. The disarm code (PIN) is supplied
    /// at dispatch time from the PIN entry widget (TASK-100), not on the
    /// action wire.
    ///
    /// Wire: `{"action":"alarm-disarm","entity-id":"..."}`
    #[serde(rename = "alarm-disarm")]
    #[serde(rename_all = "kebab-case")]
    AlarmDisarm { entity_id: String },
}

impl Action {
    /// Returns the idempotency marker for this variant.
    ///
    /// Used by the offline action queue (TASK-065) to refuse enqueueing
    /// non-idempotent actions. `CallService` returns `Idempotent` as a
    /// placeholder; TASK-065 layers a runtime allowlist (`turn_on`, `turn_off`,
    /// `set_*`) on top — the placeholder alone is **not** the security gate.
    ///
    /// Phase 6 markers per `locked_decisions.idempotency_marker_phase6_variants`:
    /// - `SetTemperature`, `SetHvacMode`, `SetMediaVolume`, `SetCoverPosition`,
    ///   `SetFanSpeed`, `Lock`, `Unlock`, `AlarmArm`, `AlarmDisarm` → Idempotent
    /// - `MediaTransport(Play|Pause|Stop)` → Idempotent
    /// - `MediaTransport(Next|Prev)` → NonIdempotent (each invocation
    ///   advances/reverses the track; replaying would skip again)
    #[must_use]
    pub const fn idempotency(&self) -> Idempotency {
        match self {
            // Toggle flips state; replaying it would un-toggle. Never queue.
            Action::Toggle => Idempotency::NonIdempotent,
            // CallService idempotency is context-dependent — runtime allowlist
            // check in TASK-065. Placeholder returns Idempotent so the schema
            // does not pre-empt that check.
            Action::CallService { .. } => Idempotency::Idempotent,
            // No side-effect: opening a modal is purely UI-local.
            Action::MoreInfo => Idempotency::Idempotent,
            // No side-effect: navigation target is deterministic.
            Action::Navigate { .. } => Idempotency::Idempotent,
            // External process spawn — re-running spawns a second process.
            Action::Url { .. } => Idempotency::NonIdempotent,
            // No side-effect.
            Action::None => Idempotency::Idempotent,

            // Phase 6 variants — all idempotent per
            // locked_decisions.idempotency_marker_phase6_variants, except
            // MediaTransport(Next|Prev) which advance the track.
            Action::SetTemperature { .. } => Idempotency::Idempotent,
            Action::SetHvacMode { .. } => Idempotency::Idempotent,
            Action::SetMediaVolume { .. } => Idempotency::Idempotent,
            Action::MediaTransport { transport, .. } => match transport {
                // Next/Prev each advance or reverse the track; replaying on
                // reconnect would skip the track a second time.
                MediaTransportOp::Next | MediaTransportOp::Prev => Idempotency::NonIdempotent,
                // Play/Pause/Stop are idempotent: toggling a playing player
                // to Play again is a no-op in HA.
                MediaTransportOp::Play | MediaTransportOp::Pause | MediaTransportOp::Stop => {
                    Idempotency::Idempotent
                }
            },
            Action::SetCoverPosition { .. } => Idempotency::Idempotent,
            Action::SetFanSpeed { .. } => Idempotency::Idempotent,
            // Locking/unlocking twice is a no-op. Confirmation lives on
            // WidgetOptions per locked_decisions.confirmation_on_lock_unlock.
            Action::Lock { .. } => Idempotency::Idempotent,
            Action::Unlock { .. } => Idempotency::Idempotent,
            Action::AlarmArm { .. } => Idempotency::Idempotent,
            Action::AlarmDisarm { .. } => Idempotency::Idempotent,
        }
    }
}

// ---------------------------------------------------------------------------
// ActionSpec
// ---------------------------------------------------------------------------

/// Phase-3 alias for the canonical [`Action`] enum.
///
/// Phase 4 may replace this alias with a struct wrapper carrying additional
/// YAML-source metadata (line/column for diagnostics) without changing the
/// dispatcher signature. Today it is a transparent alias so the dispatcher
/// can be written against a single type now and not need a rename later.
pub type ActionSpec = Action;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Idempotency markers
    // -----------------------------------------------------------------------

    #[test]
    fn toggle_is_non_idempotent() {
        assert_eq!(Action::Toggle.idempotency(), Idempotency::NonIdempotent);
    }

    #[test]
    fn url_is_non_idempotent() {
        let action = Action::Url {
            href: "https://example.org/".to_string(),
        };
        assert_eq!(action.idempotency(), Idempotency::NonIdempotent);
    }

    #[test]
    fn more_info_is_idempotent() {
        assert_eq!(Action::MoreInfo.idempotency(), Idempotency::Idempotent);
    }

    #[test]
    fn navigate_is_idempotent() {
        let action = Action::Navigate {
            view_id: "home".to_string(),
        };
        assert_eq!(action.idempotency(), Idempotency::Idempotent);
    }

    #[test]
    fn call_service_idempotent_placeholder() {
        // Per locked_decisions.idempotency_marker: the schema marker ships as
        // Idempotent as a placeholder; TASK-065 layers a runtime allowlist on
        // top. This test asserts the placeholder, not the runtime decision.
        let action = Action::CallService {
            domain: "light".to_string(),
            service: "turn_on".to_string(),
            target: Some("light.kitchen".to_string()),
            data: None,
        };
        assert_eq!(action.idempotency(), Idempotency::Idempotent);
    }

    #[test]
    fn none_is_idempotent() {
        assert_eq!(Action::None.idempotency(), Idempotency::Idempotent);
    }

    // -----------------------------------------------------------------------
    // Phase 6 idempotency markers (TASK-099)
    // -----------------------------------------------------------------------

    #[test]
    fn idempotency_markers_per_variant() {
        // All idempotent variants.
        assert_eq!(
            Action::SetTemperature {
                entity_id: "climate.living_room".to_string(),
                temperature: 21.0,
            }
            .idempotency(),
            Idempotency::Idempotent
        );
        assert_eq!(
            Action::SetHvacMode {
                entity_id: "climate.living_room".to_string(),
                mode: "heat".to_string(),
            }
            .idempotency(),
            Idempotency::Idempotent
        );
        assert_eq!(
            Action::SetMediaVolume {
                entity_id: "media_player.tv".to_string(),
                volume_level: 0.5,
            }
            .idempotency(),
            Idempotency::Idempotent
        );
        // MediaTransport(Play/Pause/Stop) → Idempotent.
        for op in [
            MediaTransportOp::Play,
            MediaTransportOp::Pause,
            MediaTransportOp::Stop,
        ] {
            assert_eq!(
                Action::MediaTransport {
                    entity_id: "media_player.tv".to_string(),
                    transport: op,
                }
                .idempotency(),
                Idempotency::Idempotent,
                "{op:?} must be Idempotent"
            );
        }
        // MediaTransport(Next/Prev) → NonIdempotent.
        for op in [MediaTransportOp::Next, MediaTransportOp::Prev] {
            assert_eq!(
                Action::MediaTransport {
                    entity_id: "media_player.tv".to_string(),
                    transport: op,
                }
                .idempotency(),
                Idempotency::NonIdempotent,
                "{op:?} must be NonIdempotent"
            );
        }
        assert_eq!(
            Action::SetCoverPosition {
                entity_id: "cover.garage".to_string(),
                position: 50,
            }
            .idempotency(),
            Idempotency::Idempotent
        );
        assert_eq!(
            Action::SetFanSpeed {
                entity_id: "fan.bedroom".to_string(),
                speed: "high".to_string(),
            }
            .idempotency(),
            Idempotency::Idempotent
        );
        assert_eq!(
            Action::Lock {
                entity_id: "lock.front_door".to_string(),
            }
            .idempotency(),
            Idempotency::Idempotent
        );
        assert_eq!(
            Action::Unlock {
                entity_id: "lock.front_door".to_string(),
            }
            .idempotency(),
            Idempotency::Idempotent
        );
        assert_eq!(
            Action::AlarmArm {
                entity_id: "alarm_control_panel.home".to_string(),
                mode: "home".to_string(),
            }
            .idempotency(),
            Idempotency::Idempotent
        );
        assert_eq!(
            Action::AlarmDisarm {
                entity_id: "alarm_control_panel.home".to_string(),
            }
            .idempotency(),
            Idempotency::Idempotent
        );
    }

    // -----------------------------------------------------------------------
    // Serde: kebab-case wire shape
    //
    // These tests defend `locked_decisions.phase4_forward_compat`: the YAML
    // names in `docs/DASHBOARD_SCHEMA.md` are kebab-case (`more-info`,
    // `call-service`); Rust enum serde defaults are PascalCase. Without the
    // explicit `#[serde(rename = "...")]` on every variant, Phase 4 YAML
    // deserialize would fail silently. Risk #15.
    //
    // serde_yaml is not yet a dependency; serde_json suffices because the
    // wire format is identical for the discriminator field and field names.
    // The kebab-case literals on the wire are what matter — JSON exercises
    // exactly the same serde codepaths as YAML.
    // -----------------------------------------------------------------------

    fn assert_round_trip(action: Action, expected_action_tag: &str) {
        let json = serde_json::to_string(&action).expect("serialize must succeed");
        assert!(
            json.contains(&format!("\"action\":\"{expected_action_tag}\"")),
            "serialized JSON `{json}` must contain `\"action\":\"{expected_action_tag}\"`"
        );
        let decoded: Action =
            serde_json::from_str(&json).expect("deserialize round-trip must succeed");
        assert_eq!(
            decoded, action,
            "round-trip must produce an Eq value (action: {expected_action_tag})"
        );
    }

    #[test]
    fn toggle_round_trips_as_kebab() {
        assert_round_trip(Action::Toggle, "toggle");
    }

    #[test]
    fn call_service_round_trips_as_kebab() {
        assert_round_trip(
            Action::CallService {
                domain: "light".to_string(),
                service: "turn_on".to_string(),
                target: Some("light.kitchen".to_string()),
                data: None,
            },
            "call-service",
        );
    }

    #[test]
    fn more_info_round_trips_as_kebab() {
        assert_round_trip(Action::MoreInfo, "more-info");
    }

    #[test]
    fn navigate_round_trips_as_kebab() {
        assert_round_trip(
            Action::Navigate {
                view_id: "home".to_string(),
            },
            "navigate",
        );
    }

    #[test]
    fn url_round_trips_as_kebab() {
        assert_round_trip(
            Action::Url {
                href: "https://example.org/".to_string(),
            },
            "url",
        );
    }

    #[test]
    fn none_round_trips_as_kebab() {
        assert_round_trip(Action::None, "none");
    }

    // -----------------------------------------------------------------------
    // Phase 6 round-trip tests (TASK-099)
    // -----------------------------------------------------------------------

    #[test]
    fn set_temperature_round_trip() {
        let action = Action::SetTemperature {
            entity_id: "climate.living_room".to_string(),
            temperature: 21.5,
        };
        assert_round_trip(action.clone(), "set-temperature");
        // Field rename: entity_id → entity-id on wire.
        let json = serde_json::to_string(&action).unwrap();
        assert!(
            json.contains("\"entity-id\":"),
            "entity_id must serialize as kebab `entity-id`, got: {json}"
        );
        assert!(
            json.contains("\"temperature\":"),
            "temperature field must be present, got: {json}"
        );
    }

    #[test]
    fn set_hvac_mode_round_trip() {
        assert_round_trip(
            Action::SetHvacMode {
                entity_id: "climate.living_room".to_string(),
                mode: "heat".to_string(),
            },
            "set-hvac-mode",
        );
    }

    #[test]
    fn set_media_volume_round_trip() {
        assert_round_trip(
            Action::SetMediaVolume {
                entity_id: "media_player.tv".to_string(),
                volume_level: 0.5,
            },
            "set-media-volume",
        );
    }

    #[test]
    fn media_transport_round_trip() {
        // All five operations must round-trip.
        for op in [
            MediaTransportOp::Play,
            MediaTransportOp::Pause,
            MediaTransportOp::Stop,
            MediaTransportOp::Next,
            MediaTransportOp::Prev,
        ] {
            let action = Action::MediaTransport {
                entity_id: "media_player.tv".to_string(),
                transport: op,
            };
            assert_round_trip(action.clone(), "media-transport");
        }
        // Verify wire field name for transport.
        let play = Action::MediaTransport {
            entity_id: "media_player.tv".to_string(),
            transport: MediaTransportOp::Play,
        };
        let json = serde_json::to_string(&play).unwrap();
        assert!(
            json.contains("\"transport\":\"play\""),
            "transport must serialize as lowercase `play`, got: {json}"
        );
        let next = Action::MediaTransport {
            entity_id: "media_player.tv".to_string(),
            transport: MediaTransportOp::Next,
        };
        let json = serde_json::to_string(&next).unwrap();
        assert!(
            json.contains("\"transport\":\"next\""),
            "transport must serialize as lowercase `next`, got: {json}"
        );
    }

    #[test]
    fn set_cover_position_round_trip() {
        assert_round_trip(
            Action::SetCoverPosition {
                entity_id: "cover.garage".to_string(),
                position: 50,
            },
            "set-cover-position",
        );
    }

    #[test]
    fn set_fan_speed_round_trip() {
        assert_round_trip(
            Action::SetFanSpeed {
                entity_id: "fan.bedroom".to_string(),
                speed: "high".to_string(),
            },
            "set-fan-speed",
        );
    }

    #[test]
    fn lock_unlock_no_confirmation_field() {
        // Per locked_decisions.confirmation_on_lock_unlock: confirmation lives
        // on WidgetOptions::Lock.require_confirmation_on_unlock, NOT on the
        // action wire. The JSON must not contain "confirmation".
        let lock = Action::Lock {
            entity_id: "lock.front_door".to_string(),
        };
        let json = serde_json::to_string(&lock).unwrap();
        assert!(
            !json.contains("confirmation"),
            "Lock action wire must not contain `confirmation` field, got: {json}"
        );
        assert_round_trip(lock, "lock");

        let unlock = Action::Unlock {
            entity_id: "lock.front_door".to_string(),
        };
        let json = serde_json::to_string(&unlock).unwrap();
        assert!(
            !json.contains("confirmation"),
            "Unlock action wire must not contain `confirmation` field, got: {json}"
        );
        assert_round_trip(unlock, "unlock");
    }

    #[test]
    fn alarm_arm_round_trip() {
        assert_round_trip(
            Action::AlarmArm {
                entity_id: "alarm_control_panel.home".to_string(),
                mode: "home".to_string(),
            },
            "alarm-arm",
        );
    }

    #[test]
    fn alarm_disarm_round_trip() {
        assert_round_trip(
            Action::AlarmDisarm {
                entity_id: "alarm_control_panel.home".to_string(),
            },
            "alarm-disarm",
        );
    }

    // -----------------------------------------------------------------------
    // Field renames
    // -----------------------------------------------------------------------

    #[test]
    fn navigate_view_id_field_is_kebab_case_on_wire() {
        let action = Action::Navigate {
            view_id: "home".to_string(),
        };
        let json = serde_json::to_string(&action).expect("serialize");
        assert!(
            json.contains("\"view-id\":\"home\""),
            "Navigate field must serialize as kebab `view-id`, got: {json}"
        );
        // And the inverse: kebab field must deserialize.
        let decoded: Action = serde_json::from_str(r#"{"action":"navigate","view-id":"home"}"#)
            .expect("kebab `view-id` must deserialize");
        assert_eq!(decoded, action);
    }

    #[test]
    fn call_service_yaml_shape_matches_dashboard_schema() {
        // Mirrors `docs/DASHBOARD_SCHEMA.md` (and the Phase 4 YAML loader's
        // future input). Specifying via JSON is sufficient; serde_yaml uses
        // the same internal tag / rename pipeline.
        let raw = r#"{
            "action": "call-service",
            "domain": "light",
            "service": "turn_on",
            "target": "light.kitchen"
        }"#;
        let decoded: Action =
            serde_json::from_str(raw).expect("DASHBOARD_SCHEMA shape must deserialize");
        assert_eq!(
            decoded,
            Action::CallService {
                domain: "light".to_string(),
                service: "turn_on".to_string(),
                target: Some("light.kitchen".to_string()),
                data: None,
            }
        );
    }

    #[test]
    fn pascal_case_action_tag_is_rejected() {
        // Defensive: if someone removes the `#[serde(rename = "...")]` the
        // PascalCase literal would deserialize and we would silently regress.
        // This test asserts that the PascalCase tag is NOT accepted, locking
        // the kebab-case requirement in place.
        let raw = r#"{"action":"Toggle"}"#;
        let result: Result<Action, _> = serde_json::from_str(raw);
        assert!(
            result.is_err(),
            "PascalCase `Toggle` must NOT deserialize; only kebab-case `toggle` is accepted"
        );
    }

    // -----------------------------------------------------------------------
    // ActionSpec alias
    // -----------------------------------------------------------------------

    #[test]
    fn action_spec_is_action() {
        // Compile-time: ActionSpec is currently a type alias for Action.
        // This test is a behavioural assertion that the alias does not
        // silently diverge (e.g. into a wrapper struct) without an explicit
        // schema-migration commit.
        let spec: ActionSpec = Action::Toggle;
        let action: Action = spec;
        assert_eq!(action, Action::Toggle);
    }

    // -----------------------------------------------------------------------
    // ActionError
    // -----------------------------------------------------------------------

    #[test]
    fn action_error_display_unknown_alarm_arm_mode() {
        let err = ActionError::UnknownAlarmArmMode("silent".to_string());
        let msg = err.to_string();
        assert!(
            msg.contains("silent"),
            "Display must include the unknown mode, got: {msg}"
        );
        assert!(
            msg.contains("home"),
            "Display must include expected modes, got: {msg}"
        );
    }

    #[test]
    fn action_error_display_unknown_fan_speed() {
        let err = ActionError::UnknownFanSpeed("turbo".to_string());
        let msg = err.to_string();
        assert!(
            msg.contains("turbo"),
            "Display must include the unknown speed, got: {msg}"
        );
    }
}
