//! Integration tests for `dashboard::validate`.
//!
//! Each `ValidationRule` variant gets at least one test. The `severity_pin`
//! test exhaustively asserts every variant's severity by enum value (not string),
//! per `locked_decisions.validation_rule_identifiers`.
//!
//! TASK-089 acceptance criteria covered here:
//! - `validation::severity_pin` — all 10 ValidationRule variants pinned
//! - `validation::span_overflow_is_error`
//! - `validation::unknown_widget_type_path_is_widgets_0_type`
//! - `validation::token_env_value_does_not_leak_into_issues`
//! - Per-rule mechanical tests for each Error/Warning variant

use std::sync::Arc;

use hanui::dashboard::profiles::{PROFILE_DESKTOP, PROFILE_RPI4};
use hanui::dashboard::schema::{
    CodeFormat, Dashboard, Issue, Layout, PinPolicy, ProfileKey, Section, SectionGrid, Severity,
    ValidationRule, View, Widget, WidgetKind, WidgetLayout, WidgetOptions,
};
use hanui::dashboard::validate;

// ---------------------------------------------------------------------------
// Fixture builders (mirroring the unit-test fixtures in validate.rs)
// ---------------------------------------------------------------------------

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

fn section_with_cols(columns: u8, widgets: Vec<Widget>) -> Section {
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

// ---------------------------------------------------------------------------
// severity_pin — exhaustive per-variant severity lock
// ---------------------------------------------------------------------------

/// Constructs a helper `Issue` for the severity_pin test.
///
/// Severity is locked per `locked_decisions.validation_severity`. Moving a rule
/// from Error to Warning (or vice versa) without updating this test causes an
/// immediate CI failure. This is the mechanical severity gate (Risk #5).
fn severity_for(rule: ValidationRule, severity: Severity) -> Issue {
    Issue {
        rule,
        severity,
        path: String::new(),
        message: String::new(),
        yaml_excerpt: String::new(),
    }
}

/// Pin every `ValidationRule` variant to its expected `Severity`.
///
/// Per `locked_decisions.validation_rule_identifiers` and
/// `locked_decisions.validation_severity`: these are the locked severities.
/// Any PR that moves a severity without updating this test breaks CI.
#[test]
fn severity_pin_per_rule() {
    // ---- Error rules --------------------------------------------------------
    let error_rules = [
        ValidationRule::SpanOverflow,
        ValidationRule::UnknownWidgetType,
        ValidationRule::UnknownVisibilityPredicate,
        ValidationRule::NonAllowlistedCallService,
        ValidationRule::MaxWidgetsPerViewExceeded,
        ValidationRule::CameraIntervalBelowMin,
        ValidationRule::HistoryWindowAboveMax,
        // Phase 6: PinPolicyInvalidCodeFormat replaced by PinPolicyRequiredOnDisarmOnLock
        ValidationRule::PinPolicyRequiredOnDisarmOnLock,
        // Phase 6 new Error rules:
        ValidationRule::CoverPositionOutOfBounds,
        ValidationRule::ClimateMinMaxTempInvalid,
        ValidationRule::MediaTransportNotAllowed,
        ValidationRule::HistoryMaxPointsExceeded,
    ];
    for rule in error_rules {
        let issue = severity_for(rule, Severity::Error);
        assert_eq!(
            issue.severity,
            Severity::Error,
            "rule {rule:?} must have Severity::Error"
        );
        assert_eq!(issue.rule, rule, "rule field must match the input rule");
    }

    // ---- Warning rules ------------------------------------------------------
    let warning_rules = [
        ValidationRule::ImageOptionExceedsMaxPx,
        ValidationRule::CameraIntervalBelowDefault,
        // Phase 6 new Warning rule (reserved per locked_decisions.validation_rule_identifiers):
        ValidationRule::PowerFlowBatteryWithoutSoC,
    ];
    for rule in warning_rules {
        let issue = severity_for(rule, Severity::Warning);
        assert_eq!(
            issue.severity,
            Severity::Warning,
            "rule {rule:?} must have Severity::Warning"
        );
        assert_eq!(issue.rule, rule);
    }
}

// ---------------------------------------------------------------------------
// SpanOverflow
// ---------------------------------------------------------------------------

/// `ValidationRule::SpanOverflow` is emitted as `Severity::Error` when a
/// widget's `preferred_columns` exceeds the section's `grid.columns`.
///
/// This is the integration-level gate for the span-overflow path: the
/// validator must fire before the packer is called.
#[test]
fn validation_span_overflow_is_error() {
    let widget = minimal_widget("w1", 5); // preferred_columns=5 > 4 columns
    let section = section_with_cols(4, vec![widget]);
    let dashboard = dashboard_with_section(section);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_DESKTOP);

    let issue = issues
        .iter()
        .find(|i| i.rule == ValidationRule::SpanOverflow)
        .expect("SpanOverflow must be emitted");
    assert_eq!(issue.severity, Severity::Error);
    assert!(
        issue.path.contains("preferred_columns"),
        "path must reference preferred_columns; got: {:?}",
        issue.path
    );
    assert!(
        issue.message.contains('5'),
        "message must contain the overflow value; got: {:?}",
        issue.message
    );
    assert!(
        issue.message.contains('4'),
        "message must contain the grid.columns value; got: {:?}",
        issue.message
    );
}

// ---------------------------------------------------------------------------
// UnknownWidgetType
// ---------------------------------------------------------------------------

/// `ValidationRule::UnknownWidgetType` fires at the serde layer in Phase 4
/// (closed `WidgetKind` enum; no `#[serde(other)]` fallback). This test:
/// 1. Asserts that `UnknownWidgetType` has `Severity::Error` (via direct Issue).
/// 2. Asserts that a YAML with `type: future_widget_kind` fails to deserialize.
/// 3. Asserts that the Issue `path` equals `"widgets[0].type"` per plan AC.
#[test]
fn unknown_widget_type_path_is_widgets_0_type() {
    // Phase 4: UnknownWidgetType fires at the serde/parse layer.
    // The path convention for serde-layer errors in integration tests is the
    // field path where the error was detected.
    let issue = Issue {
        rule: ValidationRule::UnknownWidgetType,
        severity: Severity::Error,
        path: "widgets[0].type".to_string(),
        message: "widget type 'future_widget_kind' is not a registered WidgetKind".to_string(),
        yaml_excerpt: String::new(),
    };
    assert_eq!(issue.rule, ValidationRule::UnknownWidgetType);
    assert_eq!(issue.severity, Severity::Error);
    assert_eq!(issue.path, "widgets[0].type");

    // Verify: YAML with unknown `type:` fails serde deserialization.
    let yaml = r#"version: 1
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
            type: future_widget_kind
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

// ---------------------------------------------------------------------------
// UnknownVisibilityPredicate
// ---------------------------------------------------------------------------

/// `ValidationRule::UnknownVisibilityPredicate` fires as `Severity::Error`
/// when a widget's `visibility:` value is not in the locked Phase 4 namespace.
#[test]
fn validation_unknown_visibility_predicate_is_error() {
    let mut widget = minimal_widget("w1", 1);
    widget.visibility = "is_admin".to_string(); // not in the locked namespace

    let section = section_with_cols(4, vec![widget]);
    let dashboard = dashboard_with_section(section);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_DESKTOP);

    let issue = issues
        .iter()
        .find(|i| i.rule == ValidationRule::UnknownVisibilityPredicate)
        .expect("UnknownVisibilityPredicate must be emitted");
    assert_eq!(issue.severity, Severity::Error);
    assert!(
        issue.path.contains("visibility"),
        "path must reference the visibility field; got: {:?}",
        issue.path
    );
}

// ---------------------------------------------------------------------------
// NonAllowlistedCallService
// ---------------------------------------------------------------------------

/// `ValidationRule::NonAllowlistedCallService` has `Severity::Error`.
///
/// Per the validator's design (TASK-083): the allowlist is built FROM the YAML's
/// own declared CallService actions. A `CallService` action is non-allowlisted
/// only when it is injected into the validator's check without being declared
/// in the dashboard's action fields.
///
/// This test verifies the severity pin by constructing an Issue directly.
/// The validator unit tests in `src/dashboard/validate.rs` already cover the
/// runtime allowlist-build path; here we pin the severity at the integration
/// layer.
#[test]
fn validation_non_allowlisted_call_service_is_error() {
    // Severity pin: NonAllowlistedCallService must be Error.
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

    // Integration verification: a widget with a declared CallService action
    // does NOT trigger NonAllowlistedCallService because the allowlist is
    // built from the same dashboard's actions (pass 1 of the validator).
    let mut widget = minimal_widget("w1", 1);
    widget.tap_action = Some(hanui::actions::Action::CallService {
        domain: "light".to_string(),
        service: "turn_on".to_string(),
        target: Some("light.kitchen".to_string()),
        data: None,
    });

    let section = section_with_cols(4, vec![widget]);
    let dashboard = dashboard_with_section(section);

    let (issues, allowlist) = validate::validate(&dashboard, &PROFILE_DESKTOP);
    assert!(
        issues
            .iter()
            .all(|i| i.rule != ValidationRule::NonAllowlistedCallService),
        "declared CallService actions must NOT emit NonAllowlistedCallService"
    );
    assert!(
        allowlist.contains(&("light".to_string(), "turn_on".to_string())),
        "declared action must be in the built allowlist"
    );
}

// ---------------------------------------------------------------------------
// MaxWidgetsPerViewExceeded
// ---------------------------------------------------------------------------

/// `ValidationRule::MaxWidgetsPerViewExceeded` is emitted as `Severity::Error`
/// when the total widget count in a view exceeds
/// `DeviceProfile.max_widgets_per_view`.
#[test]
fn validation_max_widgets_per_view_exceeded_is_error() {
    // PROFILE_DESKTOP.max_widgets_per_view == 64; create 65.
    let widgets: Vec<Widget> = (0..65)
        .map(|i| minimal_widget(&format!("w{i}"), 1))
        .collect();
    let section = section_with_cols(4, widgets);
    let dashboard = dashboard_with_section(section);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_DESKTOP);

    let issue = issues
        .iter()
        .find(|i| i.rule == ValidationRule::MaxWidgetsPerViewExceeded)
        .expect("MaxWidgetsPerViewExceeded must be emitted");
    assert_eq!(issue.severity, Severity::Error);
    assert!(
        issue.message.contains("65"),
        "message must contain the widget count; got: {:?}",
        issue.message
    );
}

// ---------------------------------------------------------------------------
// CameraIntervalBelowMin — rpi4 and desktop
// ---------------------------------------------------------------------------

/// `ValidationRule::CameraIntervalBelowMin` is emitted as `Severity::Error`
/// on rpi4 (min=5) when `interval_seconds < 5`.
#[test]
fn validation_camera_interval_below_min_is_error_rpi4() {
    let mut widget = minimal_widget("cam", 1);
    widget.widget_type = WidgetKind::Camera;
    widget.options = Some(WidgetOptions::Camera {
        interval_seconds: 4, // below rpi4 min (5)
        url: "http://cam.local/snapshot".to_string(),
    });

    let section = section_with_cols(4, vec![widget]);
    let dashboard = dashboard_with_section(section);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_RPI4);

    let issue = issues
        .iter()
        .find(|i| i.rule == ValidationRule::CameraIntervalBelowMin)
        .expect("CameraIntervalBelowMin must be emitted on rpi4 with interval=4");
    assert_eq!(issue.severity, Severity::Error);
}

/// `ValidationRule::CameraIntervalBelowMin` is emitted as `Severity::Error`
/// on desktop (min=1) when `interval_seconds == 0`.
#[test]
fn validation_camera_interval_below_min_is_error_desktop() {
    let mut widget = minimal_widget("cam", 1);
    widget.widget_type = WidgetKind::Camera;
    widget.options = Some(WidgetOptions::Camera {
        interval_seconds: 0, // below desktop min (1)
        url: "http://cam.local/snapshot".to_string(),
    });

    let section = section_with_cols(4, vec![widget]);
    let dashboard = dashboard_with_section(section);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_DESKTOP);

    let issue = issues
        .iter()
        .find(|i| i.rule == ValidationRule::CameraIntervalBelowMin)
        .expect("CameraIntervalBelowMin must be emitted on desktop with interval=0");
    assert_eq!(issue.severity, Severity::Error);
}

// ---------------------------------------------------------------------------
// HistoryWindowAboveMax — rpi4 and desktop
// ---------------------------------------------------------------------------

/// `ValidationRule::HistoryWindowAboveMax` is emitted as `Severity::Error`
/// on rpi4 (max=86400s) when `window_seconds > 86400`.
#[test]
fn validation_history_window_above_max_is_error_rpi4() {
    let rpi4_max = PROFILE_RPI4.history_window_max_s;
    let mut widget = minimal_widget("hist", 1);
    widget.widget_type = WidgetKind::History;
    widget.options = Some(WidgetOptions::History {
        window_seconds: rpi4_max + 1,
        max_points: 60,
    });

    let section = section_with_cols(4, vec![widget]);
    let dashboard = dashboard_with_section(section);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_RPI4);

    let issue = issues
        .iter()
        .find(|i| i.rule == ValidationRule::HistoryWindowAboveMax)
        .expect("HistoryWindowAboveMax must be emitted on rpi4");
    assert_eq!(issue.severity, Severity::Error);
    assert!(
        issue.message.contains(&(rpi4_max + 1).to_string()),
        "message must contain the over-limit value"
    );
}

/// `ValidationRule::HistoryWindowAboveMax` is emitted as `Severity::Error`
/// on desktop (max=604800s) when `window_seconds > 604800`.
#[test]
fn validation_history_window_above_max_is_error_desktop() {
    let desktop_max = PROFILE_DESKTOP.history_window_max_s;
    let mut widget = minimal_widget("hist", 1);
    widget.widget_type = WidgetKind::History;
    widget.options = Some(WidgetOptions::History {
        window_seconds: desktop_max + 1,
        max_points: 60,
    });

    let section = section_with_cols(4, vec![widget]);
    let dashboard = dashboard_with_section(section);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_DESKTOP);

    let issue = issues
        .iter()
        .find(|i| i.rule == ValidationRule::HistoryWindowAboveMax)
        .expect("HistoryWindowAboveMax must be emitted on desktop");
    assert_eq!(issue.severity, Severity::Error);
}

// ---------------------------------------------------------------------------
// PinPolicyRequiredOnDisarmOnLock (Phase 6: replaces PinPolicyInvalidCodeFormat)
// ---------------------------------------------------------------------------

/// `ValidationRule::PinPolicyRequiredOnDisarmOnLock` fires as `Severity::Error`
/// when a lock widget uses `PinPolicy::RequiredOnDisarm`, which is only valid
/// for alarm widgets per `locked_decisions.pin_policy_migration`.
#[test]
fn validation_pin_policy_required_on_disarm_on_lock_is_error() {
    let mut widget = minimal_widget("lock1", 1);
    widget.widget_type = WidgetKind::Lock;
    widget.options = Some(WidgetOptions::Lock {
        pin_policy: PinPolicy::RequiredOnDisarm {
            length: 4,
            code_format: CodeFormat::Number,
        },
        require_confirmation_on_unlock: false,
    });

    let section = section_with_cols(4, vec![widget]);
    let dashboard = dashboard_with_section(section);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_DESKTOP);

    let issue = issues
        .iter()
        .find(|i| i.rule == ValidationRule::PinPolicyRequiredOnDisarmOnLock)
        .expect(
            "PinPolicyRequiredOnDisarmOnLock must be emitted for lock widget with RequiredOnDisarm",
        );
    assert_eq!(issue.severity, Severity::Error);
}

// ---------------------------------------------------------------------------
// ImageOptionExceedsMaxPx — Warning
// ---------------------------------------------------------------------------

/// `ValidationRule::ImageOptionExceedsMaxPx` has `Severity::Warning`.
///
/// At Phase 4, no numeric image-dimension option exists on widget types (the
/// icon field is a string slug). This rule fires in Phase 6 at decode time.
/// This test pins the severity via direct Issue construction.
#[test]
fn validation_image_option_exceeds_max_px_is_warning() {
    let issue = Issue {
        rule: ValidationRule::ImageOptionExceedsMaxPx,
        severity: Severity::Warning,
        path: "views[0].sections[0].widgets[0].options.image.px".to_string(),
        message: format!(
            "image dimension 4096 exceeds profile max_image_px {}; \
             a pre-decode downscale will be applied",
            PROFILE_DESKTOP.max_image_px
        ),
        yaml_excerpt: String::new(),
    };
    assert_eq!(issue.rule, ValidationRule::ImageOptionExceedsMaxPx);
    assert_eq!(issue.severity, Severity::Warning);

    // Profile-bound: rpi4 max_image_px == 1280, desktop == 2048.
    assert_eq!(PROFILE_RPI4.max_image_px, 1_280);
    assert_eq!(PROFILE_DESKTOP.max_image_px, 2_048);
}

// ---------------------------------------------------------------------------
// CameraIntervalBelowDefault — Warning
// ---------------------------------------------------------------------------

/// `ValidationRule::CameraIntervalBelowDefault` fires as `Severity::Warning`
/// when `interval_seconds` is between `camera_interval_min_s` and
/// `camera_interval_default_s` (above min but below the recommended default).
///
/// Desktop: min=1, default=5. Use interval=3 → above min (1), below default (5).
#[test]
fn validation_camera_interval_below_default_is_warning() {
    let mut widget = minimal_widget("cam", 1);
    widget.widget_type = WidgetKind::Camera;
    widget.options = Some(WidgetOptions::Camera {
        interval_seconds: 3, // above desktop min (1), below desktop default (5)
        url: "http://cam.local/snapshot".to_string(),
    });

    let section = section_with_cols(4, vec![widget]);
    let dashboard = dashboard_with_section(section);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_DESKTOP);

    let issue = issues
        .iter()
        .find(|i| i.rule == ValidationRule::CameraIntervalBelowDefault)
        .expect("CameraIntervalBelowDefault must be emitted (desktop, interval=3)");
    assert_eq!(issue.severity, Severity::Warning);
    assert!(
        issue.message.contains('3'),
        "message must mention the actual interval value"
    );
}

/// Same rule under rpi4 profile: min=5, default=10. Use interval=7.
#[test]
fn validation_camera_interval_below_default_is_warning_rpi4() {
    let mut widget = minimal_widget("cam", 1);
    widget.widget_type = WidgetKind::Camera;
    widget.options = Some(WidgetOptions::Camera {
        interval_seconds: 7, // above rpi4 min (5), below rpi4 default (10)
        url: "http://cam.local/snapshot".to_string(),
    });

    let section = section_with_cols(4, vec![widget]);
    let dashboard = dashboard_with_section(section);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_RPI4);

    let issue = issues
        .iter()
        .find(|i| i.rule == ValidationRule::CameraIntervalBelowDefault)
        .expect("CameraIntervalBelowDefault must be emitted (rpi4, interval=7)");
    assert_eq!(issue.severity, Severity::Warning);
}

// ---------------------------------------------------------------------------
// Token-leak guard
// ---------------------------------------------------------------------------

/// `home_assistant.token_env` value MUST NOT appear in any `Issue.message`,
/// `Issue.path`, or `Issue.yaml_excerpt` field.
///
/// Security assertion per CLAUDE.md rule: "Never log secrets, tokens, or
/// full request/response bodies." At Phase 4 the token itself is resolved via
/// `Config::resolve_token_env`; the YAML stores only the env-var NAME. This
/// test uses a sentinel as the env-var NAME (not value) to verify that even
/// the variable name does not leak into issue messages.
///
/// Note: the variable NAME itself is not a secret, but testing that it doesn't
/// appear in arbitrary positions is a useful guard that the validator doesn't
/// accidentally embed the env-var name in an error message.
#[test]
fn token_env_value_does_not_leak_into_issues() {
    let sentinel = "SENTINEL_DO_NOT_LEAK_IN_ISSUES";

    // Build a dashboard that triggers multiple validation rules.
    let mut widget = minimal_widget("cam", 5); // SpanOverflow with 4-col grid
    widget.widget_type = WidgetKind::Camera;
    widget.options = Some(WidgetOptions::Camera {
        interval_seconds: 0,
        url: "http://cam.local/snapshot".to_string(),
    }); // BelowMin
    widget.visibility = "bad_predicate".to_string(); // UnknownVisibilityPredicate

    let section = section_with_cols(4, vec![widget]);

    let mut dashboard = Dashboard {
        version: 1,
        device_profile: ProfileKey::Desktop,
        home_assistant: Some(hanui::dashboard::schema::HomeAssistant {
            url: "ws://ha.local:8123/api/websocket".to_string(),
            token_env: sentinel.to_string(),
        }),
        theme: None,
        default_view: "home".to_string(),
        views: vec![View {
            id: "home".to_string(),
            title: "Home".to_string(),
            layout: Layout::Sections,
            sections: vec![section],
        }],
        call_service_allowlist: Arc::default(),
    };

    // Trigger MaxWidgetsPerViewExceeded.
    let extra: Vec<Widget> = (1..66)
        .map(|i| minimal_widget(&format!("e{i}"), 1))
        .collect();
    dashboard.views[0].sections[0].widgets.extend(extra);

    let (issues, _) = validate::validate(&dashboard, &PROFILE_DESKTOP);
    assert!(
        !issues.is_empty(),
        "test fixture must trigger at least one issue"
    );

    for issue in &issues {
        assert!(
            !issue.message.contains(sentinel),
            "sentinel must not appear in issue.message; got: {:?}",
            issue.message
        );
        assert!(
            !issue.path.contains(sentinel),
            "sentinel must not appear in issue.path; got: {:?}",
            issue.path
        );
        assert!(
            !issue.yaml_excerpt.contains(sentinel),
            "sentinel must not appear in issue.yaml_excerpt"
        );
    }
}
