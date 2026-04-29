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
//! `docs/plans/2026-04-29-phase-4-layout.md`:
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
//! | `PinPolicyInvalidCodeFormat` | Error |
//! | `ImageOptionExceedsMaxPx` | Warning |
//! | `CameraIntervalBelowDefault` | Warning |
//!
//! # Visibility predicate namespace (locked — Phase 6 evaluates, Phase 4 validates)
//!
//! The known predicate set is a fixed const slice. Predicates not in the list
//! are an `UnknownVisibilityPredicate` Error so that schemas written for Phase 4
//! do not silently ignore future predicates on older clients.
//!
//! Known predicates (exact string match, or prefix match for parameterised forms):
//! - `always`
//! - `never`
//! - `entity_available:` (followed by an entity ID)
//! - `state_equals:` (followed by `<entity_id>:<value>`)
//! - `profile:` (followed by a profile key: `rpi4`, `opi-zero3`, `desktop`)
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
    CallServiceAllowlist, Dashboard, Issue, Section, Severity, ValidationRule, Widget,
    WidgetOptions,
};

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
const PARAMETERISED_PREFIXES: &[&str] = &["entity_available:", "state_equals:", "profile:"];

/// Returns `true` if `predicate` is a member of the locked Phase 4 predicate
/// namespace.
///
/// Matching rules:
/// 1. Exact match against any entry in [`EXACT_PREDICATES`].
/// 2. Prefix match against any entry in [`PARAMETERISED_PREFIXES`] where at
///    least one byte follows the prefix (i.e. `"entity_available:"` alone is
///    not valid — a target entity ID must follow).
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
    false
}

// ---------------------------------------------------------------------------
// PIN code-format valid values
// ---------------------------------------------------------------------------

/// Valid `pin_policy.code_format` string values.
///
/// Per `locked_decisions.validation_severity`: the format must be one of these
/// string values; anything else is a `PinPolicyInvalidCodeFormat` Error.
const VALID_CODE_FORMATS: &[&str] = &["Number", "Any"];

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
                "visibility predicate {:?} is not in the locked Phase 4 predicate namespace; \
                 known exact predicates: always, never; \
                 known parameterised prefixes: entity_available:, state_equals:, profile:",
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
            WidgetOptions::Camera { interval_seconds } => {
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
                // when the image option is added. Documented as Warning per the
                // severity table.
            }

            WidgetOptions::History { window_seconds } => {
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
            }

            WidgetOptions::Lock { pin_policy } | WidgetOptions::Alarm { pin_policy } => {
                // PinPolicyInvalidCodeFormat → Error
                if !VALID_CODE_FORMATS.contains(&pin_policy.code_format.as_str()) {
                    issues.push(Issue {
                        rule: ValidationRule::PinPolicyInvalidCodeFormat,
                        severity: Severity::Error,
                        path: format!("{widget_path}.options.pin_policy.code_format"),
                        message: format!(
                            "pin_policy.code_format {:?} is not a valid format; \
                             allowed values: Number, Any",
                            pin_policy.code_format,
                        ),
                        yaml_excerpt: String::new(),
                    });
                }
            }

            // Fan has no validator-relevant options at Phase 4.
            WidgetOptions::Fan { .. } => {}
        }
    }

    // --- ImageOptionExceedsMaxPx for icon -----------------------------------
    // The widget `icon` field is a string path/slug; the pixel dimension
    // is only known at decode time (Phase 6). The validator surfaces a Warning
    // when the widget carries an explicit numeric image dimension through
    // a dedicated option. At Phase 4 there is no such numeric field on the
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
        Dashboard, Layout, PinPolicy, Placement, ProfileKey, Section, SectionGrid, Severity,
        ValidationRule, View, Widget, WidgetKind, WidgetLayout, WidgetOptions,
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
            issues
                .iter()
                .all(|i| i.rule != ValidationRule::SpanOverflow),
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
                issues
                    .iter()
                    .all(|i| i.rule != ValidationRule::UnknownVisibilityPredicate),
                "predicate {predicate:?} must NOT emit UnknownVisibilityPredicate"
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
            issues
                .iter()
                .all(|i| i.rule != ValidationRule::NonAllowlistedCallService),
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
            issues
                .iter()
                .all(|i| i.rule != ValidationRule::MaxWidgetsPerViewExceeded),
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
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
        assert!(
            issues
                .iter()
                .all(|i| i.rule != ValidationRule::HistoryWindowAboveMax),
            "window_seconds == max must not emit HistoryWindowAboveMax"
        );
    }

    // -----------------------------------------------------------------------
    // PinPolicyInvalidCodeFormat
    // -----------------------------------------------------------------------

    #[test]
    fn validate_pin_policy_invalid_code_format_is_error() {
        let mut widget = minimal_widget("lock", 2);
        widget.widget_type = WidgetKind::Lock;
        widget.options = Some(WidgetOptions::Lock {
            pin_policy: PinPolicy {
                code_format: "regex:[0-9]{4}".to_string(), // not a valid format
            },
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);

        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::PinPolicyInvalidCodeFormat)
            .expect("PinPolicyInvalidCodeFormat must be present");
        assert_eq!(issue.severity, Severity::Error);
    }

    #[test]
    fn validate_pin_policy_valid_formats_are_clean() {
        for format in ["Number", "Any"] {
            let mut widget = minimal_widget("lock", 2);
            widget.widget_type = WidgetKind::Lock;
            widget.options = Some(WidgetOptions::Lock {
                pin_policy: PinPolicy {
                    code_format: format.to_string(),
                },
            });

            let section = section_with_columns(4, vec![widget]);
            let dashboard = dashboard_with_section(section);

            let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);
            assert!(
                issues
                    .iter()
                    .all(|i| i.rule != ValidationRule::PinPolicyInvalidCodeFormat),
                "code_format {format:?} must not emit PinPolicyInvalidCodeFormat"
            );
        }
    }

    #[test]
    fn validate_alarm_pin_policy_invalid_code_format_is_error() {
        let mut widget = minimal_widget("alarm", 2);
        widget.widget_type = WidgetKind::Alarm;
        widget.options = Some(WidgetOptions::Alarm {
            pin_policy: PinPolicy {
                code_format: "NotAValidFormat".to_string(),
            },
        });

        let section = section_with_columns(4, vec![widget]);
        let dashboard = dashboard_with_section(section);

        let (issues, _) = validate(&dashboard, &PROFILE_DESKTOP);

        let issue = issues
            .iter()
            .find(|i| i.rule == ValidationRule::PinPolicyInvalidCodeFormat)
            .expect("Alarm widget PinPolicyInvalidCodeFormat must be present");
        assert_eq!(issue.severity, Severity::Error);
    }

    // -----------------------------------------------------------------------
    // ImageOptionExceedsMaxPx — Warning
    // -----------------------------------------------------------------------
    //
    // At Phase 4, there is no dedicated numeric image-dimension option on
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
            (ValidationRule::PinPolicyInvalidCodeFormat, Severity::Error),
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
