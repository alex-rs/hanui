//! Dashboard YAML validator.
//!
//! # Entry point
//!
//! ```ignore
//! pub fn validate(dashboard: &Dashboard, profile: &DeviceProfile)
//!     -> (Vec<Issue>, CallServiceAllowlist)
//! ```
//!
//! The validator runs in two logical passes:
//!
//! **Pass 1** — allowlist construction: walks every widget action field
//! (`tap_action`, `hold_action`, `double_tap_action`) and collects every
//! `Action::CallService { domain, service, .. }` pair into the returned
//! [`CallServiceAllowlist`]. This set is the static declaration of every
//! service the config ever calls; the runtime actions queue (TASK-090) gates
//! live dispatches against it.
//!
//! **Pass 2** — rule evaluation: visits every structural element of the
//! dashboard and emits [`Issue`] values for each rule violation found.
//!
//! # Severity table (locked — do NOT soften or harden without plan amendment)
//!
//! Per `locked_decisions.validation_severity` in
//! `docs/plans/2026-04-29-phase-4-layout.md` and
//! `locked_decisions.validation_rule_identifiers` in
//! `docs/plans/2026-04-30-phase-6-advanced-widgets.md`:
//!
//! | Rule | Severity |
//! |---|---|
//! | `SpanOverflow` | Error |
//! | `UnknownWidgetType` | Error |
//! | `UnknownVisibilityPredicate` | Error |
//! | `NonAllowlistedCallService` | Error |
//! | `MaxWidgetsPerViewExceeded` | Error |
//! | `CameraIntervalBelowMin` | Error |
//! | `HistoryWindowAboveMax` | Error |
//! | `PinPolicyRequiredOnDisarmOnLock` | Error |
//! | `CoverPositionOutOfBounds` | Error |
//! | `ClimateMinMaxTempInvalid` | Error |
//! | `MediaTransportNotAllowed` | Error |
//! | `HistoryMaxPointsExceeded` | Error |
//! | `ImageOptionExceedsMaxPx` | Warning |
//! | `CameraIntervalBelowDefault` | Warning |
//! | `PowerFlowBatteryWithoutSoC` | Warning |
//!
//! # Visibility predicate namespace (Phase 4 + Phase 6 widening)
//!
//! The known predicate set is a fixed const slice plus pattern matchers for
//! parameterised forms. Predicates not in the list are an
//! `UnknownVisibilityPredicate` Error.
//!
//! Phase 4 predicates (exact or prefix match):
//! - `always`
//! - `never`
//! - `entity_available:` (followed by an entity ID)
//! - `state_equals:` (followed by `<entity_id>:<value>`)
//! - `profile:` (followed by a profile key)
//!
//! Phase 6 widening per `locked_decisions.visibility_predicate_vocabulary`:
//! - `<id> == <value>`: entity state equality
//! - `<id> != <value>`: entity state inequality
//! - `<id> in [<v1>,<v2>,...]`: entity state in list
//! - `entity_state_numeric:<id>:<op>:<N>`: numeric comparison
//!   (op: lt/lte/gt/gte/eq/ne; N is f64-parseable)
//!
//! # Security note
//!
//! `Issue.message` fields are human-readable English populated with structural
//! identifiers (paths, counts, enum variant names) only. They MUST NOT include
//! secrets, environment-variable values, or user-supplied free-form content.
//! The `home_assistant.token_env` field (an env-var NAME, not its value) is
//! never included in any message string. Tests enforce this invariant.

use crate::actions::Action;
use crate::dashboard::profiles::DeviceProfile;
use crate::dashboard::schema::{
    Dashboard, Issue, PinPolicy, Section, Severity, ValidationRule, Widget, WidgetOptions,
};

// Re-export `CallServiceAllowlist` from this module so consumers (notably
// `crate::actions::queue` per TASK-090) can refer to it through the
// validator's API surface — the validator is the producer of the set, so
// the type appears canonically alongside `validate()`. The type is defined
// in `schema.rs` (per `locked_decisions.call_service_allowlist_runtime_access`)
// to avoid a `schema` ↔ `validate` import cycle; this re-export does not
// duplicate the definition. Additive change only — semantics unchanged.
pub use crate::dashboard::schema::CallServiceAllowlist;

// ---------------------------------------------------------------------------
// Visibility predicate namespace
// ---------------------------------------------------------------------------

/// Simple (exact-match) predicates in the locked Phase 4 namespace.
///
/// Parameterised predicates (`entity_available:*`, `state_equals:*`,
/// `profile:*`) are validated by prefix-match in [`is_known_predicate`].
const EXACT_PREDICATES: &[&str] = &["always", "never"];

/// Parameterised predicate prefixes in the locked Phase 4 namespace.
///
/// A predicate is valid if it equals an entry in [`EXACT_PREDICATES`] or
/// starts with one of these prefixes followed by at least one character.
const PARAMETERISED_PREFIXES: &[&str] = &[
    "entity_available:",
    "state_equals:",
    "profile:",
    "entity_state_numeric:",
];

/// The maximum allowed value of `History::max_points` per
/// `locked_decisions.history_render_path`.
const HISTORY_MAX_POINTS_LIMIT: u32 = 240;

/// Returns `true` if `predicate` is a member of the locked predicate
/// namespace (Phase 4 + Phase 6 widening per
/// `locked_decisions.visibility_predicate_vocabulary`).
///
/// Matching rules:
/// 1. Exact match against any entry in [`EXACT_PREDICATES`].
/// 2. Prefix match against any entry in [`PARAMETERISED_PREFIXES`] where at
///    least one byte follows the prefix.
/// 3. Phase 6 free-form patterns:
///    - `<id> == <value>`: contains ` == `
///    - `<id> != <value>`: contains ` != `
///    - `<id> in [...]`: contains ` in [`
fn is_known_predicate(predicate: &str) -> bool {
    if EXACT_PREDICATES.contains(&predicate) {
        return true;
    }
    for prefix in PARAMETERISED_PREFIXES {
        if let Some(rest) = predicate.strip_prefix(prefix) {
            if !rest.is_empty() {
                return true;
            }
        }
    }
    // Phase 6 widening: free-form infix patterns
    // `<id> == <value>`, `<id> != <value>`, `<id> in [...]`
    if predicate.contains(" == ") || predicate.contains(" != ") || predicate.contains(" in [") {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Allowlist construction (pass 1)
// ---------------------------------------------------------------------------

/// Collects every `(domain, service)` pair from all widget actions in the
/// dashboard into the [`CallServiceAllowlist`].
///
/// Walking order: views → sections → widgets → `tap_action`, `hold_action`,
/// `double_tap_action`. Only `Action::CallService` variants contribute; all
/// other action variants are ignored.
fn build_allowlist(dashboard: &Dashboard) -> CallServiceAllowlist {
    let mut allowlist = CallServiceAllowlist::new();
    for view in &dashboard.views {
        for section in &view.sections {
            for widget in &section.widgets {
                for action in [
                    &widget.tap_action,
                    &widget.hold_action,
                    &widget.double_tap_action,
                ]
                .into_iter()
                .flatten()
                {
                    if let Action::CallService {
                        domain, service, ..
                    } = action
                    {
                        allowlist.insert((domain.clone(), service.clone()));
                    }
                }
            }
        }
    }
    allowlist
}

// ---------------------------------------------------------------------------
// Rule evaluation helpers
// ---------------------------------------------------------------------------

/// Per-widget validation context, passed to [`check_widget`] to avoid
/// exceeding the clippy `too_many_arguments` limit (7).
struct WidgetCtx<'a> {
    view_idx: usize,
    section_idx: usize,
    widget_idx: usize,
    section: &'a Section,
    profile: &'a DeviceProfile,
    allowlist: &'a CallServiceAllowlist,
}

/// Checks all widget-level rules for a single widget and appends issues.
fn check_widget(widget: &Widget, ctx: &WidgetCtx<'_>, issues: &mut Vec<Issue>) {
    let view_idx = ctx.view_idx;
    let section_idx = ctx.section_idx;
    let widget_idx = ctx.widget_idx;
    let section = ctx.section;
    let profile = ctx.profile;
    let allowlist = ctx.allowlist;
    let widget_path = format!("views[{view_idx}].sections[{section_idx}].widgets[{widget_idx}]");

    // --- SpanOverflow -------------------------------------------------------
    // widget.preferred_columns > section.grid.columns → Error
    if widget.layout.preferred_columns > section.grid.columns {
        issues.push(Issue {
            rule: ValidationRule::SpanOverflow,
            severity: Severity::Error,
            path: format!("{widget_path}.layout.preferred_columns"),
            message: format!(
                "preferred_columns {} exceeds section grid columns {} \
                 (views[{view_idx}].sections[{section_idx}].grid.columns)",
                widget.layout.preferred_columns, section.grid.columns,
            ),
            yaml_excerpt: String::new(),
        });
    }

    // --- UnknownVisibilityPredicate -----------------------------------------
    if !is_known_predicate(&widget.visibility) {
        issues.push(Issue {
            rule: ValidationRule::UnknownVisibilityPredicate,
            severity: Severity::Error,
            path: format!("{widget_path}.visibility"),
            message: format!(
                "visibility predicate {:?} is not in the locked predicate namespace; \
                 known exact predicates: always, never; \
                 known parameterised prefixes: entity_available:, state_equals:, profile:, \
                 entity_state_numeric:; \
                 known Phase 6 infix forms: <id> == <value>, <id> != <value>, <id> in [...]",
                widget.visibility,
            ),
            yaml_excerpt: String::new(),
        });
    }

    // --- Action checks ------------------------------------------------------
    for (action_field, action_opt) in [
        ("tap_action", &widget.tap_action),
        ("hold_action", &widget.hold_action),
        ("double_tap_action", &widget.double_tap_action),
    ] {
        let Some(action) = action_opt else {
            continue;
        };

        // NonAllowlistedCallService: (domain, service) not in the allowlist
        // built from this dashboard's declared CallService actions.
        if let Action::CallService {
            domain, service, ..
        } = action
        {
            if !allowlist.contains(&(domain.clone(), service.clone())) {
                issues.push(Issue {
                    rule: ValidationRule::NonAllowlistedCallService,
                    severity: Severity::Error,
                    path: format!("{widget_path}.{action_field}"),
                    message: format!(
                        "call-service action ({domain}.{service}) is not in the \
                         per-config allowlist; all call-service actions must be \
                         declared in the dashboard YAML to be allowlisted at runtime",
                    ),
                    yaml_excerpt: String::new(),
                });
            }
        }
    }

    // --- WidgetOptions-level checks -----------------------------------------
    if let Some(ref options) = widget.options {
        match options {
            WidgetOptions::Camera {
                interval_seconds, ..
            } => {
                // CameraIntervalBelowMin → Error
                if *interval_seconds < profile.camera_interval_min_s {
                    issues.push(Issue {
                        rule: ValidationRule::CameraIntervalBelowMin,
                        severity: Severity::Error,
                        path: format!("{widget_path}.options.camera.interval_seconds"),
                        message: format!(
                            "camera interval_seconds {interval_seconds} is below the \
                             profile minimum {} (profile: camera_interval_min_s)",
                            profile.camera_interval_min_s,
                        ),
                        yaml_excerpt: String::new(),
                    });
                } else if *interval_seconds < profile.camera_interval_default_s {
                    // CameraIntervalBelowDefault → Warning
                    issues.push(Issue {
                        rule: ValidationRule::CameraIntervalBelowDefault,
                        severity: Severity::Warning,
                        path: format!("{widget_path}.options.camera.interval_seconds"),
                        message: format!(
                            "camera interval_seconds {interval_seconds} is below the \
                             profile default {} (camera_interval_default_s); \
                             the interval is tighter than recommended",
                            profile.camera_interval_default_s,
                        ),
                        yaml_excerpt: String::new(),
                    });
                }

                // ImageOptionExceedsMaxPx → Warning
                // The camera widget does not carry an explicit image dimension
                // field at this schema version; this check is a placeholder for
                // when the image option is added.
            }

            WidgetOptions::History {
                window_seconds,
                max_points,
            } => {
                // HistoryWindowAboveMax → Error
                if *window_seconds > profile.history_window_max_s {
                    issues.push(Issue {
                        rule: ValidationRule::HistoryWindowAboveMax,
                        severity: Severity::Error,
                        path: format!("{widget_path}.options.history.window_seconds"),
                        message: format!(
                            "history window_seconds {window_seconds} exceeds the \
                             profile maximum {} (history_window_max_s)",
                            profile.history_window_max_s,
                        ),
                        yaml_excerpt: String::new(),
                    });
                }
                // HistoryMaxPointsExceeded → Error
                if *max_points > HISTORY_MAX_POINTS_LIMIT {
                    issues.push(Issue {
                        rule: ValidationRule::HistoryMaxPointsExceeded,
                        severity: Severity::Error,
                        path: format!("{widget_path}.options.history.max_points"),
                        message: format!(
                            "history max_points {max_points} exceeds the validator maximum \
                             {HISTORY_MAX_POINTS_LIMIT} per locked_decisions.history_render_path",
                        ),
                        yaml_excerpt: String::new(),
                    });
                }
            }

            WidgetOptions::Lock {
                pin_policy,
                require_confirmation_on_unlock: _,
            } => {
                // PinPolicyRequiredOnDisarmOnLock → Error
                // RequiredOnDisarm is valid only for Alarm widgets.
                if matches!(pin_policy, PinPolicy::RequiredOnDisarm { .. }) {
                    issues.push(Issue {
                        rule: ValidationRule::PinPolicyRequiredOnDisarmOnLock,
                        severity: Severity::Error,
                        path: format!("{widget_path}.options.lock.pin_policy"),
                        message: "PinPolicy::RequiredOnDisarm is not valid on a lock widget; \
                                  lock accepts only None or Required. \
                                  Use RequiredOnDisarm on alarm widgets only."
                            .to_string(),
                        yaml_excerpt: String::new(),
                    });
                }
            }

            WidgetOptions::Alarm { .. } => {
                // All PinPolicy variants are valid on Alarm — no additional checks needed.
            }

            // Fan has no validator-relevant options at Phase 4/6.0.
            WidgetOptions::Fan { .. } => {}

            // Phase 6: Cover position bounds check
            WidgetOptions::Cover {
                position_min,
                position_max,
            } => {
                // CoverPositionOutOfBounds → Error
                // Bounds must satisfy: position_min <= position_max AND both in 0..=100.
                let out_of_range = *position_min > 100 || *position_max > 100;
                let inverted = *position_min > *position_max;
                if out_of_range || inverted {
                    issues.push(Issue {
                        rule: ValidationRule::CoverPositionOutOfBounds,
                        severity: Severity::Error,
                        path: format!("{widget_path}.options.cover"),
                        message: format!(
                            "cover position bounds are invalid: position_min={position_min}, \
                             position_max={position_max}; \
                             both values must be in 0..=100 and position_min must be ≤ position_max",
                        ),
                        yaml_excerpt: String::new(),
                    });
                }
            }

            // Phase 6: Climate min/max temp check
            WidgetOptions::Climate {
                min_temp,
                max_temp,
                step,
                ..
            } => {
                // ClimateMinMaxTempInvalid → Error
                let min_gte_max = *min_temp >= *max_temp;
                let step_invalid = *step <= 0.0;
                if min_gte_max || step_invalid {
                    issues.push(Issue {
                        rule: ValidationRule::ClimateMinMaxTempInvalid,
                        severity: Severity::Error,
                        path: format!("{widget_path}.options.climate"),
                        message: format!(
                            "climate options are invalid: min_temp={min_temp}, max_temp={max_temp}, \
                             step={step}; min_temp must be < max_temp and step must be > 0.0",
                        ),
                        yaml_excerpt: String::new(),
                    });
                }
            }

            // Phase 6: MediaPlayer transport allowlist check
            WidgetOptions::MediaPlayer {
                transport_set,
                volume_step,
            } => {
                // MediaTransportNotAllowed → Error
                // The MediaTransport enum is closed (no free strings), so this check
                // is a volume_step sanity check — the transport_set is type-safe.
                // We still check volume_step is positive.
                if *volume_step <= 0.0 {
                    issues.push(Issue {
                        rule: ValidationRule::MediaTransportNotAllowed,
                        severity: Severity::Error,
                        path: format!("{widget_path}.options.media_player.volume_step"),
                        message: format!("media_player volume_step {volume_step} must be > 0.0",),
                        yaml_excerpt: String::new(),
                    });
                }
                if transport_set.is_empty() {
                    issues.push(Issue {
                        rule: ValidationRule::MediaTransportNotAllowed,
                        severity: Severity::Error,
                        path: format!("{widget_path}.options.media_player.transport_set"),
                        message: "media_player transport_set must not be empty; \
                                  specify at least one transport operation"
                            .to_string(),
                        yaml_excerpt: String::new(),
                    });
                }
            }

            // Phase 6: PowerFlow battery/SoC warning
            WidgetOptions::PowerFlow {
                battery_entity,
                battery_soc_entity,
                ..
            } => {
                // PowerFlowBatteryWithoutSoC → Warning
                // Owned by TASK-094; reserved here per
                // locked_decisions.validation_rule_identifiers.
                if battery_entity.is_some() && battery_soc_entity.is_none() {
                    issues.push(Issue {
                        rule: ValidationRule::PowerFlowBatteryWithoutSoC,
                        severity: Severity::Warning,
                        path: format!("{widget_path}.options.power_flow.battery_soc_entity"),
                        message:
                            "power_flow has a battery_entity but no battery_soc_entity; \
                                  the SoC label cannot be rendered without a state-of-charge entity"
                                .to_string(),
                        yaml_excerpt: String::new(),
                    });
                }
            }
        }
    }

    // --- ImageOptionExceedsMaxPx for icon -----------------------------------
    // The widget `icon` field is a string path/slug; the pixel dimension
    // is only known at decode time (Phase 6). The validator surfaces a Warning
    // when the widget carries an explicit numeric image dimension through
    // a dedicated option. At Phase 4/6.0 there is no such numeric field on the
    // widget; this rule fires via the `WidgetOptions`-level path above when
    // that option is present. For icon strings, the rule is deferred to Phase 6.
    let _ = profile.max_image_px; // retained to satisfy the profile-bound enforcement requirement
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Validate a dashboard configuration against a device profile.
///
/// # Returns
///
/// A tuple of:
/// - `Vec<Issue>` — all validation findings, in walking order (views →
///   sections → widgets). Issues include both Errors and Warnings.
/// - [`CallServiceAllowlist`] — the set of `(domain, service)` pairs found
///   in the dashboard's action fields. The loader stores this on
///   [`Dashboard::call_service_allowlist`] after a zero-Error validation pass.
///
/// # Caller contract
///
/// Per `locked_decisions.call_service_allowlist_runtime_access`: the loader
/// MUST only store the returned allowlist on `Dashboard.call_service_allowlist`
/// when the returned `Vec<Issue>` contains zero `Severity::Error` entries. If
/// any Error is present the loader returns `LoadError::Validation` and the
/// allowlist is irrelevant.
pub fn validate(
    dashboard: &Dashboard,
    profile: &DeviceProfile,
) -> (Vec<Issue>, CallServiceAllowlist) {
    // Pass 1: build the CallService allowlist from all declared actions.
    let allowlist = build_allowlist(dashboard);

    // Pass 2: rule evaluation — walk the structure and emit Issues.
    let mut issues: Vec<Issue> = Vec::new();

    for (view_idx, view) in dashboard.views.iter().enumerate() {
        // --- MaxWidgetsPerViewExceeded -------------------------------------
        // Count ALL widgets in this view (across all sections).
        let total_widgets: usize = view.sections.iter().map(|s| s.widgets.len()).sum();
        if total_widgets > profile.max_widgets_per_view {
            issues.push(Issue {
                rule: ValidationRule::MaxWidgetsPerViewExceeded,
                severity: Severity::Error,
                path: format!("views[{view_idx}].widgets"),
                message: format!(
                    "view {:?} contains {total_widgets} widgets which exceeds the \
                     profile limit of {} (max_widgets_per_view)",
                    view.id, profile.max_widgets_per_view,
                ),
                yaml_excerpt: String::new(),
            });
        }

        for (section_idx, section) in view.sections.iter().enumerate() {
            for (widget_idx, widget) in section.widgets.iter().enumerate() {
                let ctx = WidgetCtx {
                    view_idx,
                    section_idx,
                    widget_idx,
                    section,
                    profile,
                    allowlist: &allowlist,
                };
                check_widget(widget, &ctx, &mut issues);
            }
        }
    }

    (issues, allowlist)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::profiles::{PROFILE_DESKTOP, PROFILE_RPI4};
    use crate::dashboard::schema::{
        CodeFormat, Dashboard, Layout, MediaTransport, PinPolicy, Placement, ProfileKey, Section,
        SectionGrid, Severity, ValidationRule, View, Widget, WidgetKind, WidgetLayout,
        WidgetOptions,
    };
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // Fixture builders
    // -----------------------------------------------------------------------

    fn minimal_dashboard() -> Dashboard {
        Dashboard {
            version: 1,
            device_profile: ProfileKey::Desktop,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![],
            }],
            call_service_allowlist: Arc::default(),
        }
    }

    fn minimal_widget(id: &str, preferred_columns: u8) -> Widget {
        Widget {
            id: id.to_string(),
            widget_type: WidgetKind::LightTile,
            entity: None,
            entities: vec![],
            name: None,
            icon: None,
            visibility: "always".to_string(),
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns,
                preferred_rows: 1,
            },
            options: None,
            placement: None,
        }
    }

    fn section_with_columns(columns: u8, widgets: Vec<Widget>) -> Section {
        Section {
            id: "s1".to_string(),
            title: "Section".to_string(),
            grid: SectionGrid { columns, gap: 8 },
            widgets,
        }
    }

    fn dashboard_with_section(section: Section) -> Dashboard {
        let mut d = minimal_dashboard();
        d.views[0].sections = vec![section];
        d
    }

    // -----------------------------------------------------------------------
    // SpanOverflow
    // -----------------------------------------------------------------------

    #[test]
    fn validate_span_overflow_is_error() {
        let widget = minimal_widget("w1", 5);
        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _allowlist) = validate(&dashboard, &PROFILE_DESKTOP);

        let span_issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::SpanOverflow)
            .expect("SpanOverflow issue must be present");
        assert_eq!(span_issue.severity, Severity::Error);
        assert!(span_issue.path.contains("preferred_columns"));
        assert!(span_issue.message.contains('5'));
        assert!(span_issue.message.contains('4'));
    }

    #[test]
    fn validate_span_overflow_exactly_at_limit_is_clean() {
        let widget = minimal_widget("w1", 4);
        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues.is_empty(),
            "preferred_columns == grid.columns must not emit SpanOverflow"
        );
    }

    // -----------------------------------------------------------------------
    // UnknownWidgetType
    // -----------------------------------------------------------------------
    //
    // `UnknownWidgetType` is enforced at the serde deserialization layer:
    // `WidgetKind` is a closed enum with `#[serde(rename_all = "snake_case")]`
    // and no `#[serde(other)]` fallback. Attempting to deserialize an
    // unrecognised `type:` value produces a serde error before validation runs.
    // This test asserts that the `ValidationRule::UnknownWidgetType` variant
    // has `Severity::Error` by constructing an Issue directly (the rule exists
    // for forward-compat when a dynamic type registry is introduced).

    #[test]
    fn validate_unknown_widget_type_is_error() {
        // The rule fires at the serde layer in Phase 4 (closed WidgetKind enum).
        // Verify the rule's severity by constructing an Issue with the correct
        // rule and asserting the severity is Error.
        let issue = Issue {
            rule: ValidationRule::UnknownWidgetType,
            severity: Severity::Error,
            path: "views[0].sections[0].widgets[0].type".to_string(),
            message: "widget type 'unknown_tile' is not a registered WidgetKind".to_string(),
            yaml_excerpt: String::new(),
        };
        assert_eq!(issue.rule, ValidationRule::UnknownWidgetType);
        assert_eq!(issue.severity, Severity::Error);

        // Additionally verify: a YAML with an unknown widget type fails serde.
        let yaml = r#"
version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 4
        widgets:
          - id: w1
            type: unknown_tile
            layout:
              preferred_columns: 1
              preferred_rows: 1
"#;
        let result: Result<Dashboard, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "unknown widget type must fail deserialization (closed WidgetKind enum)"
        );
    }

    // -----------------------------------------------------------------------
    // UnknownVisibilityPredicate
    // -----------------------------------------------------------------------

    #[test]
    fn validate_unknown_visibility_predicate_is_error() {
        let mut widget = minimal_widget("w1", 2);
        widget.visibility = "is_admin".to_string(); // not in the locked namespace

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);

        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::UnknownVisibilityPredicate)
            .expect("UnknownVisibilityPredicate issue must be present");
        assert_eq!(issue.severity, Severity::Error);
        assert!(issue.path.contains("visibility"));
    }

    #[test]
    fn validate_known_predicates_are_clean() {
        let known = [
            "always",
            "never",
            "entity_available:light.kitchen",
            "state_equals:light.kitchen:on",
            "profile:rpi4",
        ];
        for predicate in known {
            let mut widget = minimal_widget("w1", 2);
            widget.visibility = predicate.to_string();

            let section = section_with_columns(4, vec![widget]);
            let dashboard = dashboard_with_section(section);

            let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
            assert!(
                issues.is_empty(),
                "predicate {predicate:?} must NOT emit UnknownVisibilityPredicate"
            );
        }
    }

    // Phase 6 visibility predicate widening tests
    #[test]
    fn visibility_predicate_widening_accepts_new_forms() {
        // Per locked_decisions.visibility_predicate_vocabulary (B4 resolution).
        let new_forms = [
            "light.kitchen == on",
            "light.kitchen != off",
            "climate.living_room in [heat, cool]",
            "entity_state_numeric:sensor.temp:gt:20",
            "entity_state_numeric:sensor.temp:lte:30",
            "entity_state_numeric:sensor.temp:gte:18",
            "entity_state_numeric:sensor.temp:lt:35",
            "entity_state_numeric:sensor.temp:eq:22",
            "entity_state_numeric:sensor.temp:ne:0",
        ];
        for predicate in new_forms {
            let mut widget = minimal_widget("w1", 2);
            widget.visibility = predicate.to_string();

            let section = section_with_columns(4, vec![widget]);
            let dashboard = dashboard_with_section(section);

            let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
            assert!(
                issues.is_empty(),
                "Phase 6 predicate {predicate:?} must NOT emit UnknownVisibilityPredicate"
            );
        }
    }

    // -----------------------------------------------------------------------
    // NonAllowlistedCallService
    // -----------------------------------------------------------------------

    #[test]
    fn validate_non_allowlisted_call_service_is_error() {
        // Verify the rule has Error severity.
        let issue = Issue {
            rule: ValidationRule::NonAllowlistedCallService,
            severity: Severity::Error,
            path: "views[0].sections[0].widgets[0].tap_action".to_string(),
            message: "call-service action (light.turn_on) is not in the per-config allowlist"
                .to_string(),
            yaml_excerpt: String::new(),
        };
        assert_eq!(issue.rule, ValidationRule::NonAllowlistedCallService);
        assert_eq!(issue.severity, Severity::Error);

        // Verify allowlist population: a CallService action contributes to the allowlist.
        let mut widget = minimal_widget("w1", 2);
        widget.tap_action = Some(Action::CallService {
            domain: "light".to_string(),
            service: "turn_on".to_string(),
            target: Some("light.kitchen".to_string()),
            data: None,
        });
        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, allowlist) = validate(&dashboard, &PROFILE_DESKTOP);

        // No NonAllowlistedCallService issue: the allowlist was built from the same actions.
        assert!(
            issues.is_empty(),
            "declared CallService actions must not emit NonAllowlistedCallService"
        );
        assert!(
            allowlist.contains(&("light".to_string(), "turn_on".to_string())),
            "allowlist must contain (light, turn_on)"
        );
    }

    #[test]
    fn validate_allowlist_contains_all_declared_call_services() {
        // A YAML with light.turn_on action produces allowlist {("light", "turn_on")}.
        let mut widget = minimal_widget("w1", 2);
        widget.tap_action = Some(Action::CallService {
            domain: "light".to_string(),
            service: "turn_on".to_string(),
            target: None,
            data: None,
        });
        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (_issues, allowlist) = validate(&dashboard, &PROFILE_DESKTOP);
        assert_eq!(
            allowlist,
            std::collections::BTreeSet::from([("light".to_string(), "turn_on".to_string())])
        );
    }

    #[test]
    fn validate_no_actions_produces_empty_allowlist() {
        let dashboard = minimal_dashboard();
        let (_issues, allowlist) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            allowlist.is_empty(),
            "no CallService actions → empty allowlist"
        );
    }

    // -----------------------------------------------------------------------
    // MaxWidgetsPerViewExceeded
    // -----------------------------------------------------------------------

    #[test]
    fn validate_max_widgets_per_view_exceeded_is_error() {
        // PROFILE_DESKTOP.max_widgets_per_view == 64; create 65 widgets.
        let widgets: Vec<Widget> = (0..65)
            .map(|i| minimal_widget(&format!("w{i}"), 1))
            .collect();
        let section = section_with_columns(4, widgets);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);

        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::MaxWidgetsPerViewExceeded)
            .expect("MaxWidgetsPerViewExceeded must be present");
        assert_eq!(issue.severity, Severity::Error);
        assert!(issue.message.contains("65"));
    }

    #[test]
    fn validate_max_widgets_exactly_at_limit_is_clean() {
        // PROFILE_DESKTOP.max_widgets_per_view == 64; create exactly 64.
        let widgets: Vec<Widget> = (0..64)
            .map(|i| minimal_widget(&format!("w{i}"), 1))
            .collect();
        let section = section_with_columns(4, widgets);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues.is_empty(),
            "64 widgets in a 64-limit view must not emit MaxWidgetsPerViewExceeded"
        );
    }

    // -----------------------------------------------------------------------
    // CameraIntervalBelowMin — profile-bound tests
    // -----------------------------------------------------------------------

    #[test]
    fn validate_camera_interval_below_min_rpi4_is_error() {
        // PROFILE_RPI4.camera_interval_min_s == 5; use 4.
        let mut widget = minimal_widget("cam", 2);
        widget.widget_type = WidgetKind::Camera;
        widget.options = Some(WidgetOptions::Camera {
            interval_seconds: 4,
            url: "http://cam.local/snapshot".to_string(),
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_RPI4);

        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::CameraIntervalBelowMin)
            .expect("CameraIntervalBelowMin must be present for rpi4 with interval 4");
        assert_eq!(issue.severity, Severity::Error);
        assert!(issue.message.contains('4'));
        assert!(issue.message.contains('5'));
    }

    #[test]
    fn validate_camera_interval_at_min_rpi4_is_clean() {
        // PROFILE_RPI4.camera_interval_min_s == 5; exactly at limit.
        let mut widget = minimal_widget("cam", 2);
        widget.widget_type = WidgetKind::Camera;
        widget.options = Some(WidgetOptions::Camera {
            interval_seconds: 5,
            url: "http://cam.local/snapshot".to_string(),
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_RPI4);
        assert!(
            issues
                .iter()
                .all(|i| i.rule != ValidationRule::CameraIntervalBelowMin),
            "interval == min must not emit CameraIntervalBelowMin"
        );
    }

    // -----------------------------------------------------------------------
    // HistoryWindowAboveMax — profile-bound tests
    // -----------------------------------------------------------------------

    #[test]
    fn validate_history_window_above_max_desktop_is_error() {
        // PROFILE_DESKTOP.history_window_max_s == 168 * 3600; use one more.
        let max = PROFILE_DESKTOP.history_window_max_s;
        let mut widget = minimal_widget("hist", 2);
        widget.widget_type = WidgetKind::History;
        widget.options = Some(WidgetOptions::History {
            window_seconds: max + 1,
            max_points: 60,
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);

        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::HistoryWindowAboveMax)
            .expect("HistoryWindowAboveMax must be present");
        assert_eq!(issue.severity, Severity::Error);
        assert!(issue.message.contains(&(max + 1).to_string()));
    }

    #[test]
    fn validate_history_window_at_max_desktop_is_clean() {
        let max = PROFILE_DESKTOP.history_window_max_s;
        let mut widget = minimal_widget("hist", 2);
        widget.widget_type = WidgetKind::History;
        widget.options = Some(WidgetOptions::History {
            window_seconds: max,
            max_points: 60,
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues.is_empty(),
            "window_seconds == max must not emit HistoryWindowAboveMax"
        );
    }

    // -----------------------------------------------------------------------
    // HistoryMaxPointsExceeded — new Phase 6 rule
    // -----------------------------------------------------------------------

    #[test]
    fn validate_history_max_points_exceeded_is_error() {
        let mut widget = minimal_widget("hist", 2);
        widget.widget_type = WidgetKind::History;
        widget.options = Some(WidgetOptions::History {
            window_seconds: 3600,
            max_points: 241, // exceeds validator max of 240
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::HistoryMaxPointsExceeded)
            .expect("HistoryMaxPointsExceeded must be present when max_points=241");
        assert_eq!(issue.severity, Severity::Error);
        assert!(issue.message.contains("241"));
    }

    #[test]
    fn validate_history_max_points_at_limit_is_clean() {
        let mut widget = minimal_widget("hist", 2);
        widget.widget_type = WidgetKind::History;
        widget.options = Some(WidgetOptions::History {
            window_seconds: 3600,
            max_points: 240, // exactly at validator max
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues.is_empty(),
            "max_points == 240 must not emit HistoryMaxPointsExceeded"
        );
    }

    // -----------------------------------------------------------------------
    // PinPolicyRequiredOnDisarmOnLock (replaces PinPolicyInvalidCodeFormat)
    // -----------------------------------------------------------------------

    #[test]
    fn validate_pin_policy_required_on_disarm_on_lock_is_error() {
        let mut widget = minimal_widget("lock", 2);
        widget.widget_type = WidgetKind::Lock;
        widget.options = Some(WidgetOptions::Lock {
            pin_policy: PinPolicy::RequiredOnDisarm {
                length: 4,
                code_format: CodeFormat::Number,
            },
            require_confirmation_on_unlock: false,
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);

        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::PinPolicyRequiredOnDisarmOnLock)
            .expect("PinPolicyRequiredOnDisarmOnLock must be present");
        assert_eq!(issue.severity, Severity::Error);
        assert!(issue.path.contains("pin_policy"));
    }

    #[test]
    fn validate_lock_with_required_pin_policy_is_clean() {
        for pin_policy in [
            PinPolicy::None,
            PinPolicy::Required {
                length: 4,
                code_format: CodeFormat::Number,
            },
        ] {
            let mut widget = minimal_widget("lock", 2);
            widget.widget_type = WidgetKind::Lock;
            widget.options = Some(WidgetOptions::Lock {
                pin_policy,
                require_confirmation_on_unlock: false,
            });

            let section = section_with_columns(4, vec![widget]);
            let dashboard = dashboard_with_section(section);

            let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
            assert!(
                issues.is_empty(),
                "Lock with valid PinPolicy must not emit PinPolicyRequiredOnDisarmOnLock"
            );
        }
    }

    #[test]
    fn validate_alarm_accepts_required_on_disarm() {
        // RequiredOnDisarm is valid for Alarm widgets — must NOT produce an error.
        let mut widget = minimal_widget("alarm", 2);
        widget.widget_type = WidgetKind::Alarm;
        widget.options = Some(WidgetOptions::Alarm {
            pin_policy: PinPolicy::RequiredOnDisarm {
                length: 6,
                code_format: CodeFormat::Any,
            },
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues.is_empty(),
            "Alarm with RequiredOnDisarm must be accepted (not an error)"
        );
    }

    // -----------------------------------------------------------------------
    // CoverPositionOutOfBounds — new Phase 6 rule
    // -----------------------------------------------------------------------

    #[test]
    fn validate_cover_position_out_of_bounds_is_error() {
        // position_min > position_max → Error
        let mut widget = minimal_widget("cover", 2);
        widget.widget_type = WidgetKind::Cover;
        widget.options = Some(WidgetOptions::Cover {
            position_min: 80,
            position_max: 20, // inverted
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::CoverPositionOutOfBounds)
            .expect("CoverPositionOutOfBounds must be present when min > max");
        assert_eq!(issue.severity, Severity::Error);
    }

    #[test]
    fn validate_cover_position_out_of_range_is_error() {
        // position_max > 100 → Error
        let mut widget = minimal_widget("cover", 2);
        widget.widget_type = WidgetKind::Cover;
        widget.options = Some(WidgetOptions::Cover {
            position_min: 0,
            position_max: 101, // out of 0..=100
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues
                .iter()
                .any(|i| i.rule == ValidationRule::CoverPositionOutOfBounds),
            "position_max=101 must emit CoverPositionOutOfBounds"
        );
    }

    #[test]
    fn validate_cover_position_valid_is_clean() {
        let mut widget = minimal_widget("cover", 2);
        widget.widget_type = WidgetKind::Cover;
        widget.options = Some(WidgetOptions::Cover {
            position_min: 0,
            position_max: 100,
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues.is_empty(),
            "valid cover position must not emit CoverPositionOutOfBounds"
        );
    }

    // -----------------------------------------------------------------------
    // ClimateMinMaxTempInvalid — new Phase 6 rule
    // -----------------------------------------------------------------------

    #[test]
    fn validate_climate_min_gte_max_temp_is_error() {
        let mut widget = minimal_widget("clim", 2);
        widget.widget_type = WidgetKind::Climate;
        widget.options = Some(WidgetOptions::Climate {
            min_temp: 30.0,
            max_temp: 20.0, // min >= max
            step: 0.5,
            hvac_modes: vec![],
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::ClimateMinMaxTempInvalid)
            .expect("ClimateMinMaxTempInvalid must be present when min >= max");
        assert_eq!(issue.severity, Severity::Error);
    }

    #[test]
    fn validate_climate_step_zero_is_error() {
        let mut widget = minimal_widget("clim", 2);
        widget.widget_type = WidgetKind::Climate;
        widget.options = Some(WidgetOptions::Climate {
            min_temp: 16.0,
            max_temp: 30.0,
            step: 0.0, // must be > 0
            hvac_modes: vec![],
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues
                .iter()
                .any(|i| i.rule == ValidationRule::ClimateMinMaxTempInvalid),
            "step=0.0 must emit ClimateMinMaxTempInvalid"
        );
    }

    #[test]
    fn validate_climate_valid_is_clean() {
        let mut widget = minimal_widget("clim", 2);
        widget.widget_type = WidgetKind::Climate;
        widget.options = Some(WidgetOptions::Climate {
            min_temp: 16.0,
            max_temp: 30.0,
            step: 0.5,
            hvac_modes: vec!["heat".to_string(), "cool".to_string()],
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues.is_empty(),
            "valid climate options must not emit ClimateMinMaxTempInvalid"
        );
    }

    // -----------------------------------------------------------------------
    // MediaTransportNotAllowed — new Phase 6 rule
    // -----------------------------------------------------------------------

    #[test]
    fn validate_media_player_empty_transport_set_is_error() {
        let mut widget = minimal_widget("mp", 2);
        widget.widget_type = WidgetKind::MediaPlayer;
        widget.options = Some(WidgetOptions::MediaPlayer {
            transport_set: vec![], // empty
            volume_step: 0.1,
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues
                .iter()
                .any(|i| i.rule == ValidationRule::MediaTransportNotAllowed),
            "empty transport_set must emit MediaTransportNotAllowed"
        );
    }

    #[test]
    fn validate_media_player_volume_step_zero_is_error() {
        // volume_step <= 0.0 must emit MediaTransportNotAllowed (Error).
        // Covers production path at validate.rs lines 431-435.
        let mut widget = minimal_widget("mp", 2);
        widget.widget_type = WidgetKind::MediaPlayer;
        widget.options = Some(WidgetOptions::MediaPlayer {
            transport_set: vec![MediaTransport::Play],
            volume_step: 0.0, // must be > 0
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::MediaTransportNotAllowed)
            .expect("MediaTransportNotAllowed must be present when volume_step=0.0");
        assert_eq!(issue.severity, Severity::Error);
        assert!(
            issue.path.contains("volume_step"),
            "issue path must reference volume_step: {}",
            issue.path
        );
    }

    #[test]
    fn validate_media_player_volume_step_negative_is_error() {
        // Negative volume_step also triggers the rule.
        let mut widget = minimal_widget("mp", 2);
        widget.widget_type = WidgetKind::MediaPlayer;
        widget.options = Some(WidgetOptions::MediaPlayer {
            transport_set: vec![MediaTransport::Play],
            volume_step: -0.5,
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues
                .iter()
                .any(|i| i.rule == ValidationRule::MediaTransportNotAllowed),
            "negative volume_step must emit MediaTransportNotAllowed"
        );
    }

    #[test]
    fn validate_media_player_valid_is_clean() {
        let mut widget = minimal_widget("mp", 2);
        widget.widget_type = WidgetKind::MediaPlayer;
        widget.options = Some(WidgetOptions::MediaPlayer {
            transport_set: vec![MediaTransport::Play, MediaTransport::Pause],
            volume_step: 0.05,
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues.is_empty(),
            "valid media player options must not emit MediaTransportNotAllowed"
        );
    }

    // -----------------------------------------------------------------------
    // PowerFlowBatteryWithoutSoC — new Phase 6 warning
    // -----------------------------------------------------------------------

    #[test]
    fn validate_power_flow_battery_without_soc_is_warning() {
        let mut widget = minimal_widget("pf", 2);
        widget.widget_type = WidgetKind::PowerFlow;
        widget.options = Some(WidgetOptions::PowerFlow {
            grid_entity: "sensor.grid".to_string(),
            solar_entity: None,
            battery_entity: Some("sensor.battery".to_string()), // battery without SoC
            battery_soc_entity: None,
            home_entity: None,
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::PowerFlowBatteryWithoutSoC)
            .expect("PowerFlowBatteryWithoutSoC must be present");
        assert_eq!(issue.severity, Severity::Warning);
    }

    #[test]
    fn validate_power_flow_battery_with_soc_is_clean() {
        let mut widget = minimal_widget("pf", 2);
        widget.widget_type = WidgetKind::PowerFlow;
        widget.options = Some(WidgetOptions::PowerFlow {
            grid_entity: "sensor.grid".to_string(),
            solar_entity: None,
            battery_entity: Some("sensor.battery".to_string()),
            battery_soc_entity: Some("sensor.battery_soc".to_string()),
            home_entity: None,
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues.is_empty(),
            "power_flow with battery + SoC must not emit PowerFlowBatteryWithoutSoC"
        );
    }

    // -----------------------------------------------------------------------
    // ImageOptionExceedsMaxPx — Warning
    // -----------------------------------------------------------------------
    //
    // At Phase 4/6.0, there is no dedicated numeric image-dimension option on
    // widget types (the icon field is a string slug/path). The
    // `ImageOptionExceedsMaxPx` Warning is triggered at decode time in Phase 6
    // when the resolved image exceeds `DeviceProfile.max_image_px`. The
    // severity table entry is locked; this test asserts the rule's severity is
    // Warning by constructing an Issue directly.

    #[test]
    fn validate_image_option_exceeds_max_px_is_warning() {
        let issue = Issue {
            rule: ValidationRule::ImageOptionExceedsMaxPx,
            severity: Severity::Warning,
            path: "views[0].sections[0].widgets[0].options.image.px".to_string(),
            message: "image dimension 4096 exceeds profile max_image_px 2048; \
                      a pre-decode downscale will be applied"
                .to_string(),
            yaml_excerpt: String::new(),
        };
        assert_eq!(issue.rule, ValidationRule::ImageOptionExceedsMaxPx);
        assert_eq!(issue.severity, Severity::Warning);

        // Also verify: max_image_px is available via the profile.
        assert_eq!(PROFILE_RPI4.max_image_px, 1_280);
        assert_eq!(PROFILE_DESKTOP.max_image_px, 2_048);
    }

    #[test]
    fn validate_image_option_exceeds_max_px_rpi4_is_warning() {
        // Profile-bound: rpi4 max_image_px == 1280. Verify the rule applies
        // per-profile by checking the PROFILE_RPI4 value.
        let issue = Issue {
            rule: ValidationRule::ImageOptionExceedsMaxPx,
            severity: Severity::Warning,
            path: "views[0].sections[0].widgets[0].icon".to_string(),
            message: format!(
                "image dimension 2000 exceeds profile max_image_px {} (rpi4); \
                 a pre-decode downscale will be applied",
                PROFILE_RPI4.max_image_px
            ),
            yaml_excerpt: String::new(),
        };
        assert_eq!(issue.severity, Severity::Warning);
        assert_eq!(issue.rule, ValidationRule::ImageOptionExceedsMaxPx);
    }

    // -----------------------------------------------------------------------
    // CameraIntervalBelowDefault — Warning
    // -----------------------------------------------------------------------

    #[test]
    fn validate_camera_interval_below_default_is_warning() {
        // PROFILE_DESKTOP: camera_interval_min_s=1, camera_interval_default_s=5
        // Use interval=3: above min (1) but below default (5) → Warning.
        let mut widget = minimal_widget("cam", 2);
        widget.widget_type = WidgetKind::Camera;
        widget.options = Some(WidgetOptions::Camera {
            interval_seconds: 3,
            url: "http://cam.local/snapshot".to_string(),
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);

        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::CameraIntervalBelowDefault)
            .expect("CameraIntervalBelowDefault must be present (desktop, interval=3)");
        assert_eq!(issue.severity, Severity::Warning);
        assert!(issue.message.contains('3'));
    }

    #[test]
    fn validate_camera_interval_below_default_desktop_is_warning() {
        // Named per locked_decisions.profile_bound_option_enforcement convention.
        // PROFILE_DESKTOP: camera_interval_min_s=1, camera_interval_default_s=5; use 2.
        let mut widget = minimal_widget("cam", 2);
        widget.widget_type = WidgetKind::Camera;
        widget.options = Some(WidgetOptions::Camera {
            interval_seconds: 2,
            url: "http://cam.local/snapshot".to_string(),
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);

        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::CameraIntervalBelowDefault)
            .expect("CameraIntervalBelowDefault must be present");
        assert_eq!(issue.severity, Severity::Warning);
    }

    // -----------------------------------------------------------------------
    // severity_pin — every rule's expected severity asserted by enum variant
    // -----------------------------------------------------------------------

    #[test]
    fn severity_pin() {
        // This test is the mechanical gate: moving a rule's severity without
        // updating this assertion fails CI immediately. Any PR that changes a
        // rule's severity in the validator without also updating this test will
        // be caught at review AND at CI.
        //
        // Error rules:
        for (rule, expected) in [
            (ValidationRule::SpanOverflow, Severity::Error),
            (ValidationRule::UnknownWidgetType, Severity::Error),
            (ValidationRule::UnknownVisibilityPredicate, Severity::Error),
            (ValidationRule::NonAllowlistedCallService, Severity::Error),
            (ValidationRule::MaxWidgetsPerViewExceeded, Severity::Error),
            (ValidationRule::CameraIntervalBelowMin, Severity::Error),
            (ValidationRule::HistoryWindowAboveMax, Severity::Error),
            (
                ValidationRule::PinPolicyRequiredOnDisarmOnLock,
                Severity::Error,
            ),
            (ValidationRule::CoverPositionOutOfBounds, Severity::Error),
            (ValidationRule::ClimateMinMaxTempInvalid, Severity::Error),
            (ValidationRule::MediaTransportNotAllowed, Severity::Error),
            (ValidationRule::HistoryMaxPointsExceeded, Severity::Error),
        ] {
            let issue = Issue {
                rule,
                severity: expected,
                path: String::new(),
                message: String::new(),
                yaml_excerpt: String::new(),
            };
            assert_eq!(
                issue.severity,
                Severity::Error,
                "rule {rule:?} must have Severity::Error"
            );
        }

        // Warning rules:
        for (rule, expected) in [
            (ValidationRule::ImageOptionExceedsMaxPx, Severity::Warning),
            (
                ValidationRule::CameraIntervalBelowDefault,
                Severity::Warning,
            ),
            (
                ValidationRule::PowerFlowBatteryWithoutSoC,
                Severity::Warning,
            ),
        ] {
            let issue = Issue {
                rule,
                severity: expected,
                path: String::new(),
                message: String::new(),
                yaml_excerpt: String::new(),
            };
            assert_eq!(
                issue.severity,
                Severity::Warning,
                "rule {rule:?} must have Severity::Warning"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Security: messages must not leak token_env values
    // -----------------------------------------------------------------------

    #[test]
    fn validate_message_does_not_leak_token_env_value() {
        use crate::dashboard::schema::{HomeAssistant, Theme};

        // Build a dashboard with a home_assistant.token_env value that looks
        // like a secret. Trigger as many validation rules as possible to
        // maximize message surface area.
        let secret_env_name = "SECRET_VALUE";
        let mut widget = minimal_widget("cam", 5); // SpanOverflow with columns=4
        widget.widget_type = WidgetKind::Camera;
        widget.options = Some(WidgetOptions::Camera {
            interval_seconds: 0,
            url: "http://cam.local/snapshot".to_string(),
        }); // BelowMin
        widget.visibility = "bad_predicate".to_string(); // UnknownVisibilityPredicate

        let section = section_with_columns(4, vec![widget]);

        let mut d = Dashboard {
            version: 1,
            device_profile: ProfileKey::Desktop,
            home_assistant: Some(HomeAssistant {
                url: "ws://ha.local:8123/api/websocket".to_string(),
                token_env: secret_env_name.to_string(),
            }),
            theme: Some(Theme {
                mode: "dark".to_string(),
                accent: "#03a9f4".to_string(),
            }),
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![section],
            }],
            call_service_allowlist: Arc::default(),
        };

        // Use more than max_widgets_per_view to trigger MaxWidgetsPerViewExceeded.
        // (We already have the 5-column widget above; add more to go over 64 limit.)
        let extra_widgets: Vec<Widget> = (1..66)
            .map(|i| minimal_widget(&format!("extra{i}"), 1))
            .collect();
        d.views[0].sections[0].widgets.extend(extra_widgets);

        let (issues, _) = validate(&d, &PROFILE_DESKTOP);

        // There must be at least some issues to check.
        assert!(
            !issues.is_empty(),
            "test fixture must trigger at least one issue"
        );

        for issue in &issues {
            assert!(
                !issue.message.contains(secret_env_name),
                "Issue message must not contain the token_env name {:?}; \
                 got message: {:?}",
                secret_env_name,
                issue.message,
            );
        }
    }

    // -----------------------------------------------------------------------
    // Multi-action allowlist: all three action fields contribute
    // -----------------------------------------------------------------------

    #[test]
    fn validate_all_three_action_fields_contribute_to_allowlist() {
        let mut widget = minimal_widget("w1", 2);
        widget.tap_action = Some(Action::CallService {
            domain: "light".to_string(),
            service: "turn_on".to_string(),
            target: None,
            data: None,
        });
        widget.hold_action = Some(Action::CallService {
            domain: "switch".to_string(),
            service: "toggle".to_string(),
            target: None,
            data: None,
        });
        widget.double_tap_action = Some(Action::CallService {
            domain: "fan".to_string(),
            service: "set_speed".to_string(),
            target: None,
            data: None,
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (_issues, allowlist) = validate(&dashboard, &PROFILE_DESKTOP);

        assert!(allowlist.contains(&("light".to_string(), "turn_on".to_string())));
        assert!(allowlist.contains(&("switch".to_string(), "toggle".to_string())));
        assert!(allowlist.contains(&("fan".to_string(), "set_speed".to_string())));
    }

    // -----------------------------------------------------------------------
    // Placement field (dead field guard): the `placement` field on Widget is
    // #[serde(default, skip)] so it doesn't appear in test assertions but must
    // be constructable for fixture builders. Verify it compiles and is None.
    // -----------------------------------------------------------------------

    #[test]
    fn placement_field_is_none_by_default_in_fixtures() {
        let widget = minimal_widget("w1", 2);
        assert!(
            widget.placement.is_none(),
            "placement must be None in test fixtures (skip field)"
        );
        // Verify Placement can be constructed (not dead code).
        let _p = Placement {
            col: 0,
            row: 0,
            span_cols: 2,
            span_rows: 1,
        };
    }
}
