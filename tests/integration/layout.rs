//! Golden-fixture test runner for `dashboard::layout::pack`.
//!
//! Iterates over every `tests/layout/*.yaml` fixture, runs the full pipeline
//! (YAML parse → validate → pack), and asserts `==` against the paired
//! `*.expected.json`.
//!
//! # Expected JSON formats
//!
//! ## Success fixtures (01, 02, 03, 05, 06)
//!
//! ```json
//! {
//!   "sections": [
//!     {
//!       "section_id": "<id>",
//!       "positions": [
//!         {"widget_id": "w1", "col": 0, "row": 0, "span_cols": 1, "span_rows": 1}
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! ## Validation-error fixtures (04)
//!
//! ```json
//! {
//!   "issues": [
//!     {"rule": "SpanOverflow", "severity": "Error"}
//!   ]
//! }
//! ```
//!
//! The runner detects which format to expect by checking whether the parsed
//! `Dashboard` produces any `Severity::Error` issues during validation. If it
//! does, it serializes the error issue list and asserts against `issues[]`.
//! Otherwise it serializes the packer output per section and asserts against
//! `sections[]`.
//!
//! TASK-089 acceptance criterion: "iterates over `tests/layout/*.yaml`, loads
//! each, runs the validator + packer, serializes the output to JSON, compares
//! against `*.expected.json` byte-by-byte. Diff failure prints both files."

use std::path::Path;

use hanui::dashboard::layout::pack;
use hanui::dashboard::profiles::PROFILE_DESKTOP;
use hanui::dashboard::schema::{Dashboard, Severity, ValidationRule};
use hanui::dashboard::validate;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Output types (serialized to compare against expected.json)
// ---------------------------------------------------------------------------

/// Position of one widget, as produced by `layout::pack`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExpectedPosition {
    widget_id: String,
    col: u8,
    row: u16,
    span_cols: u8,
    span_rows: u8,
}

/// Packer output for one section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SectionPositions {
    section_id: String,
    positions: Vec<ExpectedPosition>,
}

/// The success-case expected output: one entry per section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SuccessExpected {
    sections: Vec<SectionPositions>,
}

/// One issue entry in the error-case expected output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExpectedIssue {
    rule: String,
    severity: String,
}

/// The error-case expected output: one entry per Error-severity issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ErrorExpected {
    issues: Vec<ExpectedIssue>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse the YAML fixture at `yaml_path` into a `Dashboard`.
///
/// Uses `serde_yaml_ng::from_str` directly (bypassing the loader's file I/O
/// and token-env steps, which are irrelevant for layout fixtures that have no
/// `home_assistant` block).
fn parse_fixture(yaml_path: &Path) -> Dashboard {
    let yaml = std::fs::read_to_string(yaml_path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", yaml_path.display()));
    serde_yaml_ng::from_str(&yaml)
        .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e}", yaml_path.display()))
}

/// Convert a `ValidationRule` to its stable string identifier.
fn rule_to_str(rule: ValidationRule) -> &'static str {
    match rule {
        ValidationRule::SpanOverflow => "SpanOverflow",
        ValidationRule::UnknownWidgetType => "UnknownWidgetType",
        ValidationRule::UnknownVisibilityPredicate => "UnknownVisibilityPredicate",
        ValidationRule::NonAllowlistedCallService => "NonAllowlistedCallService",
        ValidationRule::MaxWidgetsPerViewExceeded => "MaxWidgetsPerViewExceeded",
        ValidationRule::CameraIntervalBelowMin => "CameraIntervalBelowMin",
        ValidationRule::HistoryWindowAboveMax => "HistoryWindowAboveMax",
        // Phase 6: PinPolicyInvalidCodeFormat replaced by PinPolicyRequiredOnDisarmOnLock
        ValidationRule::PinPolicyRequiredOnDisarmOnLock => "PinPolicyRequiredOnDisarmOnLock",
        ValidationRule::CoverPositionOutOfBounds => "CoverPositionOutOfBounds",
        ValidationRule::ClimateMinMaxTempInvalid => "ClimateMinMaxTempInvalid",
        ValidationRule::MediaTransportNotAllowed => "MediaTransportNotAllowed",
        ValidationRule::HistoryMaxPointsExceeded => "HistoryMaxPointsExceeded",
        ValidationRule::ImageOptionExceedsMaxPx => "ImageOptionExceedsMaxPx",
        ValidationRule::CameraIntervalBelowDefault => "CameraIntervalBelowDefault",
        ValidationRule::PowerFlowBatteryWithoutSoC => "PowerFlowBatteryWithoutSoC",
    }
}

/// Convert a `Severity` to its canonical string.
fn severity_to_str(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "Error",
        Severity::Warning => "Warning",
    }
}

/// Run the golden-fixture comparison for one (yaml, expected.json) pair.
///
/// Pipeline:
/// 1. Parse the YAML to `Dashboard`.
/// 2. Run `validate::validate` under `PROFILE_DESKTOP`.
/// 3. If Error-severity issues exist: serialize the issue list and compare
///    against `expected.json`'s `issues[]` array.
///    If no Error issues: run `layout::pack` per section and compare against
///    `expected.json`'s `sections[]` array.
fn run_golden_fixture(yaml_path: &Path, expected_path: &Path) {
    let fixture_label = yaml_path.file_name().unwrap().to_string_lossy();

    // Step 1: parse YAML.
    let dashboard = parse_fixture(yaml_path);

    // Step 2: validate under desktop profile.
    let (issues, _allowlist) = validate::validate(&dashboard, &PROFILE_DESKTOP);
    let errors: Vec<_> = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .collect();

    // Read expected JSON.
    let expected_bytes = std::fs::read(expected_path)
        .unwrap_or_else(|e| panic!("failed to read expected JSON for {fixture_label}: {e}"));
    let expected_str = String::from_utf8(expected_bytes).expect("expected.json must be UTF-8");

    if !errors.is_empty() {
        // ---- Error path: serialize issue list and compare --------------------
        let actual_issues: Vec<ExpectedIssue> = errors
            .iter()
            .map(|i| ExpectedIssue {
                rule: rule_to_str(i.rule).to_string(),
                severity: severity_to_str(i.severity).to_string(),
            })
            .collect();

        let expected_error: ErrorExpected =
            serde_json::from_str(&expected_str).unwrap_or_else(|e| {
                panic!(
                    "expected.json for {fixture_label} must be a valid ErrorExpected JSON: {e}\n\
                     content: {expected_str}"
                )
            });

        assert_eq!(
            actual_issues, expected_error.issues,
            "golden mismatch for {fixture_label} (error path)\n\
             actual:   {actual_issues:?}\n\
             expected: {:?}",
            expected_error.issues
        );
    } else {
        // ---- Success path: pack per section and compare ---------------------
        let actual_sections: Vec<SectionPositions> = dashboard
            .views
            .iter()
            .flat_map(|v| v.sections.iter())
            .map(|section| {
                let positions = pack(&section.widgets, section.grid.columns)
                    .into_iter()
                    .map(|pw| ExpectedPosition {
                        widget_id: pw.widget_id,
                        col: pw.col,
                        row: pw.row,
                        span_cols: pw.span_cols,
                        span_rows: pw.span_rows,
                    })
                    .collect();
                SectionPositions {
                    section_id: section.id.clone(),
                    positions,
                }
            })
            .collect();

        let expected_success: SuccessExpected =
            serde_json::from_str(&expected_str).unwrap_or_else(|e| {
                panic!(
                    "expected.json for {fixture_label} must be a valid SuccessExpected JSON: {e}\n\
                     content: {expected_str}"
                )
            });

        assert_eq!(
            actual_sections, expected_success.sections,
            "golden mismatch for {fixture_label} (success path)\n\
             actual:   {actual_sections:?}\n\
             expected: {:?}",
            expected_success.sections
        );
    }
}

// ---------------------------------------------------------------------------
// Golden fixture tests (one per fixture file)
// ---------------------------------------------------------------------------

/// `01_single_widget`: 1 widget in a 4-col grid → placed at (col=0, row=0).
#[test]
fn golden_01_single_widget() {
    run_golden_fixture(
        Path::new("tests/layout/01_single_widget.yaml"),
        Path::new("tests/layout/01_single_widget.expected.json"),
    );
}

/// `02_span_honored`: 1 widget `preferred_columns: 3` in a 4-col grid →
/// placed at (col=0, row=0) with span_cols=3.
#[test]
fn golden_02_span_honored() {
    run_golden_fixture(
        Path::new("tests/layout/02_span_honored.yaml"),
        Path::new("tests/layout/02_span_honored.expected.json"),
    );
}

/// `03_wrap`: three `preferred_columns: 3` widgets in a 4-col grid. After
/// the first widget takes cols 0-2, only 1 col remains — not enough for the
/// second widget (span=3). Second and third widgets each wrap to a new row.
#[test]
fn golden_03_wrap() {
    run_golden_fixture(
        Path::new("tests/layout/03_wrap.yaml"),
        Path::new("tests/layout/03_wrap.expected.json"),
    );
}

/// `04_span_overflow`: 1 widget `preferred_columns: 5` in a 4-col grid.
/// Validator emits `SpanOverflow` Error; packer is never reached.
/// Expected JSON contains the issue list, not positions.
#[test]
fn golden_04_span_overflow() {
    run_golden_fixture(
        Path::new("tests/layout/04_span_overflow.yaml"),
        Path::new("tests/layout/04_span_overflow.expected.json"),
    );
}

/// `05_mixed_kinds`: light + sensor + entity widgets with varying spans in a
/// 4-col grid. Verifies correct first-fit placement across widget kinds.
#[test]
fn golden_05_mixed_kinds() {
    run_golden_fixture(
        Path::new("tests/layout/05_mixed_kinds.yaml"),
        Path::new("tests/layout/05_mixed_kinds.expected.json"),
    );
}

/// `06_multi_section`: two sections with different column counts, each packed
/// independently. Verifies that section-level packing is isolated.
#[test]
fn golden_06_multi_section() {
    run_golden_fixture(
        Path::new("tests/layout/06_multi_section.yaml"),
        Path::new("tests/layout/06_multi_section.expected.json"),
    );
}
