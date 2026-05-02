//! Phase 6 acceptance integration test for the Lovelace importer (TASK-111,
//! TASK-112).
//!
//! Cross-crate validation: the main `hanui` crate cannot depend on the
//! `lovelace-import` workspace member (the latter depends on this crate, so
//! pulling it as a dev-dep would form a cycle). Instead, this test reads
//! the importer's frozen `*.expected.hanui.yaml` fixtures from the
//! sibling workspace member and re-runs them through
//! `hanui::dashboard::validate::validate` under `PROFILE_DESKTOP`.
//!
//! The contract being asserted is the Risk #15 mitigation in the Phase 6
//! plan: every YAML the importer emits must pass the runtime validator
//! cleanly. The importer itself runs `validate()` before returning
//! `Conversion`, so this cross-crate check is a defence-in-depth gate that
//! catches a future drift between the importer's bundled `hanui = { path = "../.." }`
//! version and the source-of-truth schema in the main crate. If a future
//! merge bumps the schema without re-baking the fixtures, this test fails
//! with a clear validation-error list.
//!
//! # Coverage limits
//!
//! The validate-only gate catches Severity::Error rule violations against
//! the current schema. It does NOT catch semantic drift across rule
//! boundaries (e.g. a previously-permissive field that has been tightened
//! since the importer fixture was last re-baked is caught only if the
//! tightening produces an Error; rule loosening followed by re-tightening
//! across multiple Phase 6.x amendments could mask a regression). The
//! importer's own e2e suite in `tools/lovelace-import/tests/e2e.rs`
//! covers the byte-equal output contract; this file owns the cross-crate
//! validate gate only.
//!
//! # Fixture discovery
//!
//! The fixtures directory layout is documented in
//! `tools/lovelace-import/MAPPINGS.md`. We walk every `*.expected.hanui.yaml`
//! file under that directory and run validate on each.

use std::fs;
use std::path::{Path, PathBuf};

use hanui::dashboard::profiles::PROFILE_DESKTOP;
use hanui::dashboard::schema::{Dashboard, Severity};
use hanui::dashboard::validate;

/// Path to the importer fixtures directory, relative to the main crate's
/// `CARGO_MANIFEST_DIR`. The lovelace-import crate lives at
/// `tools/lovelace-import/`; its fixtures live under `fixtures/`.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tools")
        .join("lovelace-import")
        .join("fixtures")
}

/// Discover every `*.expected.hanui.yaml` file under `fixtures_dir`.
fn discover_fixtures() -> Vec<PathBuf> {
    let dir = fixtures_dir();
    let read = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("could not read fixtures dir {}: {e}", dir.display()));
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with(".expected.hanui.yaml"))
        {
            out.push(path);
        }
    }
    out.sort();
    assert!(
        !out.is_empty(),
        "expected at least one fixture under {}; got none",
        dir.display()
    );
    out
}

/// Strip the importer's `# UNMAPPED:` comment block so the YAML payload
/// alone reaches the parser. The block, when present, follows the
/// dashboard YAML and is purely a hand-edit hint for the user. Lines that
/// start with `# UNMAPPED:` are dropped; everything else passes through.
fn strip_unmapped_comments(yaml: &str) -> String {
    yaml.lines()
        .filter(|line| !line.trim_start().starts_with("# UNMAPPED"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_dashboard(path: &Path) -> Dashboard {
    let raw = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
    let stripped = strip_unmapped_comments(&raw);
    serde_yaml_ng::from_str(&stripped)
        .unwrap_or_else(|e| panic!("could not parse {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// cross_crate_validator_passes
// ---------------------------------------------------------------------------

/// Every importer fixture must pass `hanui::dashboard::validate::validate`
/// against `PROFILE_DESKTOP` with zero `Severity::Error` issues. Warnings
/// are allowed — the importer emits soft hints (e.g. preferred-column
/// caps) the user is expected to tune by hand.
#[test]
fn cross_crate_validator_passes_for_every_importer_fixture() {
    let fixtures = discover_fixtures();
    for path in &fixtures {
        let dashboard = parse_dashboard(path);
        let (issues, _allowlist) = validate::validate(&dashboard, &PROFILE_DESKTOP);
        let errors: Vec<_> = issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .collect();
        assert!(
            errors.is_empty(),
            "importer fixture `{}` must pass validate(); got Error issues: {errors:?}",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// every_fixture_round_trips
// ---------------------------------------------------------------------------

/// Every fixture round-trips through `serde_yaml_ng::to_string` →
/// `from_str` and produces a structurally-equal `Dashboard`. This pins the
/// importer's emission against schema drift: a serialiser change that
/// accidentally drops a field would fail this test, surfacing the
/// regression in the main crate's CI rather than only in the importer's.
#[test]
fn every_importer_fixture_round_trips_through_schema() {
    let fixtures = discover_fixtures();
    for path in &fixtures {
        let dashboard = parse_dashboard(path);
        let reserialised = serde_yaml_ng::to_string(&dashboard)
            .unwrap_or_else(|e| panic!("re-serialise {} failed: {e}", path.display()));
        let reparsed: Dashboard = serde_yaml_ng::from_str(&reserialised)
            .unwrap_or_else(|e| panic!("re-parse {} failed: {e}", path.display()));
        assert_eq!(
            dashboard,
            reparsed,
            "round-trip mismatch for fixture `{}`",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// importer_fixture_count_sanity
// ---------------------------------------------------------------------------

/// Sanity gate: the importer's `lovelace_minimum_card_set` locked decision
/// pins eight Lovelace card types. Each card type has a corresponding
/// `<name>.expected.hanui.yaml` fixture (plus the `unmapped_button` and
/// `entities` / `glance` / stack variants per `MAPPINGS.md`). A drop in
/// fixture count signals a fixture deletion that should have been caught
/// at importer-PR review time.
#[test]
fn importer_fixture_count_sanity() {
    let fixtures = discover_fixtures();
    // Per MAPPINGS.md as of TASK-111: the table currently ships nine
    // expected.hanui.yaml fixtures. We assert "≥ 8" rather than equality
    // so the importer can add new fixtures (e.g. new card mappings) in
    // future Phase 6.x ammendments without re-baking this gate.
    assert!(
        fixtures.len() >= 8,
        "expected at least 8 importer fixtures; got {} ({fixtures:?})",
        fixtures.len()
    );
}
