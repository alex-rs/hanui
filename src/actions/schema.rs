//! Canonical [`Action`] schema for Phase 3 (write/command path).
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
// Action enum
// ---------------------------------------------------------------------------

/// A typed user-interaction action.
///
/// See module-level docs for the wire shape and the
/// `locked_decisions.phase4_forward_compat` discussion of why every variant
/// carries an explicit `#[serde(rename = "...")]`.
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
}

impl Action {
    /// Returns the idempotency marker for this variant.
    ///
    /// Used by the offline action queue (TASK-065) to refuse enqueueing
    /// non-idempotent actions. `CallService` returns `Idempotent` as a
    /// placeholder; TASK-065 layers a runtime allowlist (`turn_on`, `turn_off`,
    /// `set_*`) on top — the placeholder alone is **not** the security gate.
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
}
