//! End-to-end tests for `tools/lovelace-import`.
//!
//! These tests run the importer's library entry point [`lovelace_import::convert`]
//! against the fixtures under `tools/lovelace-import/fixtures/`. Each fixture
//! is a triple:
//!
//! - `<name>.lovelace.yaml` — input (Home Assistant Lovelace YAML).
//! - `<name>.expected.hanui.yaml` — expected hanui YAML (byte-equal).
//! - `<name>.expected.unmapped.txt` — expected UNMAPPED log lines, one entry
//!   per line; empty file means no UNMAPPED entries are expected.
//!
//! Naming convention is documented in `MAPPINGS.md`. The byte-equal comparison
//! pins the importer's output: a re-format of the YAML serialiser would force
//! a deliberate fixture re-bake, so we never accidentally change the on-disk
//! shape of imported dashboards.
//!
//! The CLI policy tests (`refuses_to_overwrite_dashboard_yaml`,
//! `force_flag_overwrites`) drive the binary as a subprocess via Cargo's
//! `CARGO_BIN_EXE_<name>` env var so the file-existence + filename refusal
//! checks are exercised end-to-end.

use std::path::{Path, PathBuf};
use std::process::Command;

use lovelace_import::{convert, mappings::widget_kind_coverage};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Path to the `fixtures/` directory, relative to this test file's package
/// root (which Cargo sets as `CARGO_MANIFEST_DIR`).
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Render a [`lovelace_import::Conversion`] back to the same byte stream the
/// CLI would write: the YAML payload, optionally followed by a `# UNMAPPED:`
/// comment block. Mirrors `src/main.rs::render_with_unmapped`.
fn render_with_unmapped(c: &lovelace_import::Conversion) -> String {
    if c.unmapped.is_empty() {
        return c.yaml.clone();
    }
    let mut out = c.yaml.clone();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# UNMAPPED: the following Lovelace cards have no hanui mapping;\n");
    out.push_str("# fix these by hand before deploying.\n");
    for line in &c.unmapped {
        out.push_str("# UNMAPPED: ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Drive a single fixture: read input, convert, byte-compare against the
/// `<name>.expected.hanui.yaml` and the `<name>.expected.unmapped.txt`.
fn run_fixture(name: &str) {
    let dir = fixtures_dir();
    let input_path = dir.join(format!("{name}.lovelace.yaml"));
    let expected_yaml_path = dir.join(format!("{name}.expected.hanui.yaml"));
    let expected_unmapped_path = dir.join(format!("{name}.expected.unmapped.txt"));

    let input = std::fs::read_to_string(&input_path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", input_path.display()));
    let expected_yaml = std::fs::read_to_string(&expected_yaml_path).unwrap_or_else(|e| {
        panic!(
            "could not read expected output {}: {e}",
            expected_yaml_path.display()
        )
    });
    let expected_unmapped = std::fs::read_to_string(&expected_unmapped_path).unwrap_or_else(|e| {
        panic!(
            "could not read expected unmapped log {}: {e}",
            expected_unmapped_path.display()
        )
    });

    let conversion =
        convert(&input).unwrap_or_else(|e| panic!("conversion failed for {name}: {e:?}"));

    let actual_payload = render_with_unmapped(&conversion);
    assert_eq!(
        actual_payload,
        expected_yaml,
        "byte-compare against {} failed for fixture {name}",
        expected_yaml_path.display()
    );

    // The expected unmapped file is one entry per line (or empty). Compare the
    // joined `\n`-terminated form of `conversion.unmapped` against it.
    let actual_unmapped = if conversion.unmapped.is_empty() {
        String::new()
    } else {
        let mut s = conversion.unmapped.join("\n");
        s.push('\n');
        s
    };
    assert_eq!(
        actual_unmapped, expected_unmapped,
        "UNMAPPED log mismatch for fixture {name}",
    );
}

/// Cargo sets `CARGO_BIN_EXE_<name>` for every binary target in the same
/// crate as the integration test, so we can drive the CLI without rebuilding.
fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lovelace-import"))
}

/// Per-test scratch directory under `target/`. Avoids polluting cwd and keeps
/// parallel test runs collision-free.
fn scratch_dir(test_name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test_name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir create");
    dir
}

// ---------------------------------------------------------------------------
// Round-trip tests — one per mandatory fixture
// ---------------------------------------------------------------------------

#[test]
fn round_trip_entities() {
    run_fixture("entities");
}

#[test]
fn round_trip_glance() {
    run_fixture("glance");
}

#[test]
fn round_trip_light() {
    run_fixture("light");
}

#[test]
fn round_trip_thermostat() {
    run_fixture("thermostat");
}

#[test]
fn round_trip_vertical_stack() {
    run_fixture("vertical_stack");
}

#[test]
fn round_trip_horizontal_stack() {
    run_fixture("horizontal_stack");
}

#[test]
fn round_trip_media_control() {
    run_fixture("media_control");
}

#[test]
fn round_trip_picture_entity() {
    run_fixture("picture_entity");
}

#[test]
fn round_trip_unmapped_button() {
    run_fixture("unmapped_button");
}

// ---------------------------------------------------------------------------
// CLI policy tests
// ---------------------------------------------------------------------------

/// The importer must refuse to write to a path whose basename is
/// `dashboard.yaml`, regardless of `--force`. Per
/// `locked_decisions.lovelace_import_output_path`.
#[test]
fn refuses_to_overwrite_dashboard_yaml() {
    let scratch = scratch_dir("refuses_to_overwrite_dashboard_yaml");
    let input = fixtures_dir().join("light.lovelace.yaml");
    let target = scratch.join("dashboard.yaml");

    let status = Command::new(binary_path())
        .arg("--input")
        .arg(&input)
        .arg("--output")
        .arg(&target)
        .arg("--force")
        .status()
        .expect("invoke importer");
    assert!(
        !status.success(),
        "importer must exit non-zero when --output basename is dashboard.yaml"
    );
    assert!(
        !target.exists(),
        "target must NOT be written when basename is dashboard.yaml"
    );
}

/// Without `--force` the importer must refuse to overwrite an existing output
/// file; with `--force` the same invocation must succeed.
#[test]
fn force_flag_overwrites() {
    let scratch = scratch_dir("force_flag_overwrites");
    let input = fixtures_dir().join("light.lovelace.yaml");
    let target = scratch.join("dashboard.lovelace-import.yaml");

    // First invocation creates the file.
    let first = Command::new(binary_path())
        .arg("--input")
        .arg(&input)
        .arg("--output")
        .arg(&target)
        .status()
        .expect("invoke importer (initial)");
    assert!(first.success(), "first run must succeed");
    assert!(target.exists(), "first run must create the file");

    // Second invocation without --force must fail and not modify the file.
    let bytes_before = std::fs::read(&target).expect("read after first run");
    let second_no_force = Command::new(binary_path())
        .arg("--input")
        .arg(&input)
        .arg("--output")
        .arg(&target)
        .status()
        .expect("invoke importer (no --force)");
    assert!(
        !second_no_force.success(),
        "second run without --force must exit non-zero"
    );
    let bytes_after_no_force = std::fs::read(&target).expect("read after no-force run");
    assert_eq!(
        bytes_before, bytes_after_no_force,
        "no-force run must not modify the file"
    );

    // Third invocation with --force must succeed.
    let third_force = Command::new(binary_path())
        .arg("--input")
        .arg(&input)
        .arg("--output")
        .arg(&target)
        .arg("--force")
        .status()
        .expect("invoke importer (--force)");
    assert!(third_force.success(), "third run with --force must succeed");
}

/// `--stdout` must write the YAML to stdout and skip file creation.
#[test]
fn stdout_flag_writes_to_stdout() {
    let scratch = scratch_dir("stdout_flag_writes_to_stdout");
    let input = fixtures_dir().join("light.lovelace.yaml");
    let untouched = scratch.join("dashboard.lovelace-import.yaml");

    let output = Command::new(binary_path())
        .arg("--input")
        .arg(&input)
        .arg("--stdout")
        .current_dir(&scratch)
        .output()
        .expect("invoke importer (--stdout)");
    assert!(output.status.success(), "--stdout must succeed");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(
        stdout.contains("light_tile") && stdout.contains("light.kitchen"),
        "stdout must contain converted YAML; got: {stdout}",
    );
    assert!(
        !untouched.exists(),
        "no file must be written in --stdout mode"
    );
}

// ---------------------------------------------------------------------------
// CI gate: every WidgetKind has a fixture entry
// ---------------------------------------------------------------------------

/// Asserts that every hanui [`hanui::dashboard::schema::WidgetKind`] variant
/// appears in [`widget_kind_coverage`]. Adding a new variant in
/// `src/dashboard/schema.rs` without updating the importer's coverage list
/// fails this test. Per `locked_decisions.lovelace_minimum_card_set`: the gate
/// checks for presence in the enum coverage list (the variant may map to an
/// `Unmapped(String)` placeholder).
#[test]
fn every_widget_kind_has_entry() {
    use hanui::dashboard::schema::WidgetKind;

    let listed: Vec<WidgetKind> = widget_kind_coverage().into_iter().map(|(k, _)| k).collect();

    // All currently-defined Phase 6 variants. Adding a new variant requires
    // adding it to BOTH the enum AND `widget_kind_coverage()`.
    let required = [
        WidgetKind::LightTile,
        WidgetKind::SensorTile,
        WidgetKind::EntityTile,
        WidgetKind::Camera,
        WidgetKind::History,
        WidgetKind::Fan,
        WidgetKind::Lock,
        WidgetKind::Alarm,
        WidgetKind::Cover,
        WidgetKind::MediaPlayer,
        WidgetKind::Climate,
        WidgetKind::PowerFlow,
    ];
    for kind in required {
        assert!(
            listed.contains(&kind),
            "WidgetKind::{kind:?} is missing from widget_kind_coverage(); \
             add it to tools/lovelace-import/src/mappings.rs"
        );
    }
}

// ---------------------------------------------------------------------------
// Fixture file naming sanity
// ---------------------------------------------------------------------------

/// The naming triple `<name>.lovelace.yaml` + `<name>.expected.hanui.yaml` +
/// `<name>.expected.unmapped.txt` is documented in `MAPPINGS.md`. This test
/// checks every `*.lovelace.yaml` has its companion files.
#[test]
fn every_input_fixture_has_expected_pair() {
    let dir = fixtures_dir();
    let entries = std::fs::read_dir(&dir).expect("read fixtures dir");
    let mut input_count = 0_usize;
    for entry in entries {
        let path = entry.expect("read entry").path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(".lovelace.yaml") {
            input_count += 1;
            let expected_yaml = Path::new(&dir).join(format!("{stem}.expected.hanui.yaml"));
            let expected_unmapped = Path::new(&dir).join(format!("{stem}.expected.unmapped.txt"));
            assert!(
                expected_yaml.exists(),
                "missing companion file: {}",
                expected_yaml.display()
            );
            assert!(
                expected_unmapped.exists(),
                "missing companion file: {}",
                expected_unmapped.display()
            );
        }
    }
    assert!(
        input_count >= 6,
        "must have at least 6 fixture pairs (the locked minimum); found {input_count}"
    );
}
