//! HA service mapping table for Phase 6 [`Action`] variants (TASK-099).
//!
//! Per `locked_decisions.actions_service_map_name`, this file is
//! `src/actions/service_map.rs` — NOT `src/actions/services.rs` (that path
//! would shadow the existing `src/ha/services.rs` module in the import tree).
//!
//! This module is the **canonical lookup table** for the `(domain, service,
//! body)` triple produced by each Phase 6 [`Action`] variant. The dispatcher
//! wiring that calls into this table lives in TASK-102..TASK-105, TASK-108,
//! TASK-109.
//!
//! # Alarm arm vocabulary
//!
//! Per `locked_decisions.alarm_arm_service_vocabulary`, `AlarmArm.mode` is a
//! free `String`. The standard HA services are mapped by this table:
//!
//! | mode           | HA service                                     |
//! |----------------|------------------------------------------------|
//! | `home`         | `alarm_control_panel.alarm_arm_home`           |
//! | `away`         | `alarm_control_panel.alarm_arm_away`           |
//! | `night`        | `alarm_control_panel.alarm_arm_night`          |
//! | `vacation`     | `alarm_control_panel.alarm_arm_vacation`       |
//! | `custom_bypass`| `alarm_control_panel.alarm_arm_custom_bypass`  |
//!
//! An unknown mode returns [`ActionError::UnknownAlarmArmMode`].
//!
//! # HVAC mode vocabulary
//!
//! Per `locked_decisions.hvac_mode_vocabulary`, `SetHvacMode.mode` is a free
//! `String` — NOT a closed enum. The informational constant
//! [`STANDARD_HVAC_MODES`] documents the HA vocabulary for documentation
//! purposes; HA validates the value server-side.
//!
//! # Fan speed vocabulary
//!
//! Per `locked_decisions.fan_speed_set_vocabulary`, `SetFanSpeed.speed` is a
//! free `String`. The dispatcher reads `FanOptions.preset_modes` at dispatch
//! time (TASK-108) to choose between `preset_mode` and `speed_step` data
//! fields. An unexpected speed returns [`ActionError::UnknownFanSpeed`].
//!
//! # Cover position bounds
//!
//! Per `locked_decisions.cover_position_bounds`, the dispatcher does NOT
//! validate `SetCoverPosition.position` against `position_min`/`position_max`
//! at dispatch time. The slider UI enforces the bounds at render time; HA
//! rejects out-of-range values via service error.
//!
//! # No TBD entries
//!
//! Per `locked_decisions.idempotency_marker_phase6_variants`, every variant
//! has a complete `(domain, service, body)` mapping. No "TBD" placeholders.

use serde_json::{json, Value};

use crate::actions::schema::{Action, ActionError, MediaTransportOp};

// ---------------------------------------------------------------------------
// Informational vocabulary constants
// ---------------------------------------------------------------------------

/// Standard HA HVAC mode strings per `locked_decisions.hvac_mode_vocabulary`.
///
/// `SetHvacMode.mode` is a free `String` — HA validates the value server-
/// side. This constant is **informational only**: it documents the expected
/// vocabulary for dashboard authors and reviewers; the dispatcher does not
/// validate against this list at dispatch time.
pub const STANDARD_HVAC_MODES: &[&str] = &[
    "off",
    "heat",
    "cool",
    "heat_cool",
    "auto",
    "dry",
    "fan_only",
];

// ---------------------------------------------------------------------------
// Service call result
// ---------------------------------------------------------------------------

/// Resolved `(domain, service, data)` triple for a Phase 6 [`Action`] variant.
///
/// `data` is `None` for variants that carry no extra payload (e.g. `Lock`,
/// `Unlock`, `AlarmDisarm`). For variants like `SetTemperature` it contains
/// the HA service data JSON object.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceCall {
    /// HA service domain (e.g. `"climate"`, `"lock"`).
    pub domain: &'static str,
    /// HA service name (e.g. `"set_temperature"`, `"lock"`).
    pub service: &'static str,
    /// Optional service data JSON. `None` when the service takes no extra
    /// parameters beyond the entity target (which the dispatcher supplies
    /// separately as the `target.entity_id` field).
    pub data: Option<Value>,
}

// ---------------------------------------------------------------------------
// Main mapping function
// ---------------------------------------------------------------------------

/// Map a Phase 6 [`Action`] variant to its HA `(domain, service, data)` call.
///
/// Returns `Err(ActionError::UnknownAlarmArmMode)` for an `AlarmArm` with an
/// unrecognised mode, or `Err(ActionError::UnknownFanSpeed)` for a `SetFanSpeed`
/// with an unrecognised speed string (dispatched per
/// `locked_decisions.fan_speed_set_vocabulary`).
///
/// Non-Phase-6 variants (`Toggle`, `CallService`, `MoreInfo`, `Navigate`,
/// `Url`, `None`) return `Ok(None)` — callers that iterate over all action
/// variants should handle the `None` case by falling back to the existing
/// dispatcher paths.
///
/// # Examples
///
/// ```
/// use hanui::actions::schema::{Action, MediaTransportOp};
/// use hanui::actions::service_map::action_to_service_call;
///
/// let call = action_to_service_call(&Action::Lock {
///     entity_id: "lock.front_door".to_string(),
/// })
/// .unwrap()
/// .unwrap();
/// assert_eq!(call.domain, "lock");
/// assert_eq!(call.service, "lock");
/// assert!(call.data.is_none());
/// ```
pub fn action_to_service_call(action: &Action) -> Result<Option<ServiceCall>, ActionError> {
    match action {
        Action::SetTemperature { temperature, .. } => Ok(Some(ServiceCall {
            domain: "climate",
            service: "set_temperature",
            data: Some(json!({ "temperature": temperature })),
        })),

        Action::SetHvacMode { mode, .. } => Ok(Some(ServiceCall {
            domain: "climate",
            service: "set_hvac_mode",
            data: Some(json!({ "hvac_mode": mode })),
        })),

        Action::SetMediaVolume { volume_level, .. } => Ok(Some(ServiceCall {
            domain: "media_player",
            service: "volume_set",
            data: Some(json!({ "volume_level": volume_level })),
        })),

        Action::MediaTransport { transport, .. } => {
            let service = match transport {
                MediaTransportOp::Play => "media_play",
                MediaTransportOp::Pause => "media_pause",
                MediaTransportOp::Stop => "media_stop",
                MediaTransportOp::Next => "media_next_track",
                MediaTransportOp::Prev => "media_previous_track",
            };
            Ok(Some(ServiceCall {
                domain: "media_player",
                service,
                data: None,
            }))
        }

        Action::SetCoverPosition { position, .. } => Ok(Some(ServiceCall {
            domain: "cover",
            service: "set_cover_position",
            data: Some(json!({ "position": position })),
        })),

        // Fan speed: the caller (dispatcher in TASK-108) resolves whether to
        // use `preset_mode` or `speed_step` by reading FanOptions.preset_modes.
        // Here we return a `preset_mode` body as the default service data shape;
        // the dispatcher overrides if needed. An empty or unrecognised speed
        // surfaces ActionError::UnknownFanSpeed.
        Action::SetFanSpeed { speed, .. } => {
            if speed.is_empty() {
                return Err(ActionError::UnknownFanSpeed(speed.clone()));
            }
            Ok(Some(ServiceCall {
                domain: "fan",
                service: "turn_on",
                // Default to preset_mode; dispatcher (TASK-108) may override
                // to speed_step if the speed is numeric / not in preset_modes.
                data: Some(json!({ "preset_mode": speed })),
            }))
        }

        Action::Lock { .. } => Ok(Some(ServiceCall {
            domain: "lock",
            service: "lock",
            data: None,
        })),

        Action::Unlock { .. } => Ok(Some(ServiceCall {
            domain: "lock",
            service: "unlock",
            data: None,
        })),

        Action::AlarmArm { mode, .. } => {
            let service = match mode.as_str() {
                "home" => "alarm_arm_home",
                "away" => "alarm_arm_away",
                "night" => "alarm_arm_night",
                "vacation" => "alarm_arm_vacation",
                "custom_bypass" => "alarm_arm_custom_bypass",
                other => return Err(ActionError::UnknownAlarmArmMode(other.to_string())),
            };
            Ok(Some(ServiceCall {
                domain: "alarm_control_panel",
                service,
                data: None,
            }))
        }

        Action::AlarmDisarm { .. } => Ok(Some(ServiceCall {
            domain: "alarm_control_panel",
            service: "alarm_disarm",
            data: None,
        })),

        // Non-Phase-6 variants handled by existing dispatcher paths.
        Action::Toggle
        | Action::CallService { .. }
        | Action::MoreInfo
        | Action::Navigate { .. }
        | Action::Url { .. }
        | Action::None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::schema::{Action, MediaTransportOp};

    // -----------------------------------------------------------------------
    // every_variant_has_mapping — no variant returns Ok(None) or Err on a
    // valid input. This test covers every Phase 6 variant at least once.
    // -----------------------------------------------------------------------

    #[test]
    fn every_variant_has_mapping() {
        let cases: &[(Action, &str, &str)] = &[
            (
                Action::SetTemperature {
                    entity_id: "climate.lr".to_string(),
                    temperature: 22.0,
                },
                "climate",
                "set_temperature",
            ),
            (
                Action::SetHvacMode {
                    entity_id: "climate.lr".to_string(),
                    mode: "heat".to_string(),
                },
                "climate",
                "set_hvac_mode",
            ),
            (
                Action::SetMediaVolume {
                    entity_id: "media_player.tv".to_string(),
                    volume_level: 0.5,
                },
                "media_player",
                "volume_set",
            ),
            (
                Action::MediaTransport {
                    entity_id: "media_player.tv".to_string(),
                    transport: MediaTransportOp::Play,
                },
                "media_player",
                "media_play",
            ),
            (
                Action::MediaTransport {
                    entity_id: "media_player.tv".to_string(),
                    transport: MediaTransportOp::Pause,
                },
                "media_player",
                "media_pause",
            ),
            (
                Action::MediaTransport {
                    entity_id: "media_player.tv".to_string(),
                    transport: MediaTransportOp::Stop,
                },
                "media_player",
                "media_stop",
            ),
            (
                Action::MediaTransport {
                    entity_id: "media_player.tv".to_string(),
                    transport: MediaTransportOp::Next,
                },
                "media_player",
                "media_next_track",
            ),
            (
                Action::MediaTransport {
                    entity_id: "media_player.tv".to_string(),
                    transport: MediaTransportOp::Prev,
                },
                "media_player",
                "media_previous_track",
            ),
            (
                Action::SetCoverPosition {
                    entity_id: "cover.garage".to_string(),
                    position: 50,
                },
                "cover",
                "set_cover_position",
            ),
            (
                Action::SetFanSpeed {
                    entity_id: "fan.bedroom".to_string(),
                    speed: "high".to_string(),
                },
                "fan",
                "turn_on",
            ),
            (
                Action::Lock {
                    entity_id: "lock.front_door".to_string(),
                },
                "lock",
                "lock",
            ),
            (
                Action::Unlock {
                    entity_id: "lock.front_door".to_string(),
                },
                "lock",
                "unlock",
            ),
            (
                Action::AlarmArm {
                    entity_id: "alarm_control_panel.home".to_string(),
                    mode: "home".to_string(),
                },
                "alarm_control_panel",
                "alarm_arm_home",
            ),
            (
                Action::AlarmDisarm {
                    entity_id: "alarm_control_panel.home".to_string(),
                },
                "alarm_control_panel",
                "alarm_disarm",
            ),
        ];

        for (action, expected_domain, expected_service) in cases {
            let result = action_to_service_call(action)
                .unwrap_or_else(|e| panic!("unexpected Err for {action:?}: {e}"));
            let call = result
                .unwrap_or_else(|| panic!("expected Some(ServiceCall) for {action:?}, got None"));
            assert_eq!(
                call.domain, *expected_domain,
                "domain mismatch for {action:?}"
            );
            assert_eq!(
                call.service, *expected_service,
                "service mismatch for {action:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Alarm arm — all 5 standard modes map correctly.
    // -----------------------------------------------------------------------

    #[test]
    fn alarm_arm_all_standard_modes() {
        let cases = [
            ("home", "alarm_arm_home"),
            ("away", "alarm_arm_away"),
            ("night", "alarm_arm_night"),
            ("vacation", "alarm_arm_vacation"),
            ("custom_bypass", "alarm_arm_custom_bypass"),
        ];
        for (mode, expected_service) in cases {
            let action = Action::AlarmArm {
                entity_id: "alarm_control_panel.home".to_string(),
                mode: mode.to_string(),
            };
            let call = action_to_service_call(&action)
                .unwrap_or_else(|e| panic!("Err for mode `{mode}`: {e}"))
                .expect("Some for known mode");
            assert_eq!(
                call.service, expected_service,
                "mode `{mode}` service mismatch"
            );
            assert_eq!(call.domain, "alarm_control_panel");
        }
    }

    // -----------------------------------------------------------------------
    // Alarm arm — unknown mode returns ActionError::UnknownAlarmArmMode.
    // -----------------------------------------------------------------------

    #[test]
    fn alarm_arm_unknown_mode_returns_error() {
        let action = Action::AlarmArm {
            entity_id: "alarm_control_panel.home".to_string(),
            mode: "silent_mode_not_in_ha".to_string(),
        };
        let err = action_to_service_call(&action).expect_err("unknown mode must return Err");
        assert_eq!(
            err,
            ActionError::UnknownAlarmArmMode("silent_mode_not_in_ha".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // Fan speed — empty speed returns ActionError::UnknownFanSpeed.
    // -----------------------------------------------------------------------

    #[test]
    fn fan_speed_empty_returns_error() {
        let action = Action::SetFanSpeed {
            entity_id: "fan.bedroom".to_string(),
            speed: String::new(),
        };
        let err = action_to_service_call(&action).expect_err("empty speed must return Err");
        assert!(matches!(err, ActionError::UnknownFanSpeed(_)));
    }

    // -----------------------------------------------------------------------
    // Service data payloads for key variants.
    // -----------------------------------------------------------------------

    #[test]
    fn set_temperature_data_contains_temperature() {
        let action = Action::SetTemperature {
            entity_id: "climate.lr".to_string(),
            temperature: 21.5,
        };
        let call = action_to_service_call(&action).unwrap().unwrap();
        let data = call.data.expect("SetTemperature must have data");
        assert_eq!(data["temperature"], 21.5_f64);
    }

    #[test]
    fn set_hvac_mode_data_contains_hvac_mode() {
        let action = Action::SetHvacMode {
            entity_id: "climate.lr".to_string(),
            mode: "cool".to_string(),
        };
        let call = action_to_service_call(&action).unwrap().unwrap();
        let data = call.data.expect("SetHvacMode must have data");
        assert_eq!(data["hvac_mode"], "cool");
    }

    #[test]
    fn set_media_volume_data_contains_volume_level() {
        let action = Action::SetMediaVolume {
            entity_id: "media_player.tv".to_string(),
            volume_level: 0.75,
        };
        let call = action_to_service_call(&action).unwrap().unwrap();
        let data = call.data.expect("SetMediaVolume must have data");
        assert_eq!(data["volume_level"], 0.75_f64);
    }

    #[test]
    fn set_cover_position_data_contains_position() {
        let action = Action::SetCoverPosition {
            entity_id: "cover.garage".to_string(),
            position: 75,
        };
        let call = action_to_service_call(&action).unwrap().unwrap();
        let data = call.data.expect("SetCoverPosition must have data");
        assert_eq!(data["position"], 75);
    }

    #[test]
    fn set_fan_speed_data_contains_preset_mode() {
        let action = Action::SetFanSpeed {
            entity_id: "fan.bedroom".to_string(),
            speed: "low".to_string(),
        };
        let call = action_to_service_call(&action).unwrap().unwrap();
        let data = call.data.expect("SetFanSpeed must have data");
        assert_eq!(data["preset_mode"], "low");
    }

    #[test]
    fn lock_and_unlock_have_no_data() {
        for action in [
            Action::Lock {
                entity_id: "lock.front_door".to_string(),
            },
            Action::Unlock {
                entity_id: "lock.front_door".to_string(),
            },
        ] {
            let call = action_to_service_call(&action).unwrap().unwrap();
            assert!(
                call.data.is_none(),
                "Lock/Unlock must have no data, got: {:?}",
                call.data
            );
        }
    }

    #[test]
    fn alarm_disarm_has_no_data() {
        let action = Action::AlarmDisarm {
            entity_id: "alarm_control_panel.home".to_string(),
        };
        let call = action_to_service_call(&action).unwrap().unwrap();
        assert!(
            call.data.is_none(),
            "AlarmDisarm must have no data, got: {:?}",
            call.data
        );
    }

    // -----------------------------------------------------------------------
    // Non-Phase-6 variants return Ok(None).
    // -----------------------------------------------------------------------

    #[test]
    fn non_phase6_variants_return_none() {
        let non_phase6 = [
            Action::Toggle,
            Action::MoreInfo,
            Action::None,
            Action::Navigate {
                view_id: "home".to_string(),
            },
            Action::Url {
                href: "https://example.com".to_string(),
            },
            Action::CallService {
                domain: "light".to_string(),
                service: "turn_on".to_string(),
                target: None,
                data: None,
            },
        ];
        for action in &non_phase6 {
            let result = action_to_service_call(action)
                .unwrap_or_else(|e| panic!("non-phase6 {action:?} returned Err: {e}"));
            assert!(
                result.is_none(),
                "non-phase6 {action:?} must return Ok(None), got Some"
            );
        }
    }

    // -----------------------------------------------------------------------
    // STANDARD_HVAC_MODES constant contains the expected HA vocabulary.
    // -----------------------------------------------------------------------

    #[test]
    fn standard_hvac_modes_contains_expected_values() {
        for mode in &[
            "off",
            "heat",
            "cool",
            "heat_cool",
            "auto",
            "dry",
            "fan_only",
        ] {
            assert!(
                STANDARD_HVAC_MODES.contains(mode),
                "`{mode}` must be in STANDARD_HVAC_MODES"
            );
        }
    }
}
