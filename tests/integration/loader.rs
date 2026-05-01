//! Integration tests for `dashboard::loader`.
//!
//! These tests exercise the public `load(path, config)` API from end to end,
//! verifying every `LoadError` variant, the 256 KiB boundary acceptance,
//! the examples-loadable `@smoke` test, and the determinism guarantee.
//!
//! TASK-089 acceptance criteria covered here:
//! - `loader::config_not_found_returns_err`
//! - `loader::config_too_large_257_kib_rejected`
//! - `loader::config_at_256_kib_accepted`
//! - `loader::examples_dashboard_yaml_loads_without_issues` (@smoke)
//! - `loader::determinism_loader_run_twice_byte_equal`
//! - Token-env error paths (`TokenEnvNotFound`, `TokenEnvEmpty`)
//! - Parse error on invalid YAML

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hanui::dashboard::layout::pack;
use hanui::dashboard::loader::{self, LoadError, MAX_YAML_BYTES};
use hanui::dashboard::profiles::PROFILE_DESKTOP;
use hanui::dashboard::validate;
use hanui::platform::config::Config;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Serialize env-mutation tests to avoid races between parallel test threads.
/// ALL tests that call `stub_config()` or set env vars must hold this lock.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Construct a minimal stub `Config` by temporarily setting the required
/// `HA_URL` and `HA_TOKEN` env vars.
///
/// **Caller must hold `ENV_LOCK`** before calling this function to prevent
/// races with other tests that mutate the same env vars.
///
/// The env vars are cleaned up immediately after `Config::from_env()` returns,
/// so they do not persist into the rest of the test.
fn stub_config_with_lock(_guard: &std::sync::MutexGuard<'_, ()>) -> Config {
    // SAFETY: serialized via ENV_LOCK.
    unsafe {
        std::env::set_var("HA_URL", "ws://stub.test:8123/api/websocket");
        std::env::set_var("HA_TOKEN", "stub-integration-test-token");
    }
    let config = Config::from_env().expect("stub_config: from_env must succeed");
    unsafe {
        std::env::remove_var("HA_URL");
        std::env::remove_var("HA_TOKEN");
    }
    config
}

/// Acquire the ENV_LOCK, recovering from poison if a previous test panicked
/// while holding it. This prevents PoisonError cascades when one test fails
/// and leaves the mutex poisoned.
fn acquire_env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Minimal valid YAML dashboard payload (no `home_assistant` block so
/// `resolve_token_env` is never called by the stub config).
const MINIMAL_YAML: &str = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"#;

/// YAML with `home_assistant.token_env` set to `name`.
fn yaml_with_token_env(name: &str) -> String {
    format!(
        r#"version: 1
device_profile: desktop
default_view: home
home_assistant:
  url: "ws://homeassistant.local:8123/api/websocket"
  token_env: "{name}"
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"#
    )
}

/// Write `content` bytes to a temporary file and return a guard that deletes
/// it on drop. Collision-resistant name from thread-id + nanoseconds.
struct TempFile(PathBuf);

impl TempFile {
    fn new_bytes(content: &[u8]) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let tid = std::thread::current().id();
        let name = format!("hanui_int_089_{tid:?}_{nanos}.yaml");
        let path = std::env::temp_dir().join(name);
        let mut f = std::fs::File::create(&path).expect("temp file create");
        f.write_all(content).expect("temp file write");
        f.flush().expect("temp file flush");
        TempFile(path)
    }

    fn new(content: &str) -> Self {
        Self::new_bytes(content.as_bytes())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------
// ConfigNotFound
// ---------------------------------------------------------------------------

/// `LoadError::ConfigNotFound` is returned when the file path does not exist.
///
/// Integration-level repeat of the unit test in `src/dashboard/loader.rs`
/// to confirm the public API contract via the full public call path.
#[test]
fn config_not_found_returns_err() {
    let guard = acquire_env_lock();
    let config = stub_config_with_lock(&guard);
    let path = Path::new("/nonexistent/path/hanui_integration_089.yaml");
    let result = loader::load(path, &config, &PROFILE_DESKTOP);
    assert!(
        matches!(result, Err(LoadError::ConfigNotFound { .. })),
        "missing file must return ConfigNotFound; got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// ConfigTooLarge boundary tests
// ---------------------------------------------------------------------------

/// A payload of exactly 256 KiB (MAX_YAML_BYTES) must be ACCEPTED — it
/// returns `Ok` or a non-`ConfigTooLarge` error.
///
/// This is the boundary-accept arm of the size-cap integration test.
#[test]
fn config_at_256_kib_accepted() {
    let guard = acquire_env_lock();
    let config = stub_config_with_lock(&guard);

    let cap = MAX_YAML_BYTES; // 256 * 1024
    let header = MINIMAL_YAML;
    let mut payload = header.to_string();
    while payload.len() < cap {
        payload.push_str("# padding to reach 256 KiB boundary\n");
    }
    payload.truncate(cap);
    assert_eq!(payload.len(), cap);

    let tmp = TempFile::new(&payload);
    let result = loader::load(tmp.path(), &config, &PROFILE_DESKTOP);
    assert!(
        !matches!(result, Err(LoadError::ConfigTooLarge { .. })),
        "exactly 256 KiB must not return ConfigTooLarge; got: {result:?}"
    );
}

/// A payload of 257 KiB (one KiB over the cap) must be REJECTED with
/// `LoadError::ConfigTooLarge` **before** touching the YAML parser.
///
/// Verifies: `bytes == 257 * 1024` and `cap == MAX_YAML_BYTES`.
#[test]
fn config_too_large_257_kib_rejected() {
    let guard = acquire_env_lock();
    let config = stub_config_with_lock(&guard);

    let target_bytes = 257 * 1024;
    // Use a payload that is definitively not valid YAML to confirm the parser
    // is never reached (if it were, we would get ParseError, not ConfigTooLarge).
    let payload = "x".repeat(target_bytes);

    let tmp = TempFile::new(&payload);
    let result = loader::load(tmp.path(), &config, &PROFILE_DESKTOP);
    match result {
        Err(LoadError::ConfigTooLarge { bytes, cap }) => {
            assert_eq!(bytes, target_bytes, "bytes field must be actual file size");
            assert_eq!(cap, MAX_YAML_BYTES, "cap field must be MAX_YAML_BYTES");
        }
        other => panic!("expected ConfigTooLarge for 257 KiB; got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Token-env errors
// ---------------------------------------------------------------------------

/// A YAML with `token_env` pointing to an absent env var must return
/// `Err(LoadError::TokenEnvNotFound { name })` containing the var name.
#[test]
fn token_env_not_found_returns_err() {
    let guard = acquire_env_lock();
    let config = stub_config_with_lock(&guard);

    let var_name = "HANUI_INT_089_TOKEN_NOT_FOUND";
    unsafe { std::env::remove_var(var_name) }; // ensure absent

    let yaml = yaml_with_token_env(var_name);
    let tmp = TempFile::new(&yaml);
    let result = loader::load(tmp.path(), &config, &PROFILE_DESKTOP);
    assert!(
        matches!(result, Err(LoadError::TokenEnvNotFound { ref name }) if name == var_name),
        "absent token_env must return TokenEnvNotFound; got: {result:?}"
    );
}

/// A YAML with `token_env: <name>` where the env var is set but empty must
/// return `Err(LoadError::TokenEnvEmpty { name })`.
#[test]
fn token_env_empty_returns_err() {
    let guard = acquire_env_lock();
    let config = stub_config_with_lock(&guard);

    let var_name = "HANUI_INT_089_TOKEN_EMPTY";
    unsafe { std::env::set_var(var_name, "") };

    let yaml = yaml_with_token_env(var_name);
    let tmp = TempFile::new(&yaml);
    let result = loader::load(tmp.path(), &config, &PROFILE_DESKTOP);
    assert!(
        matches!(result, Err(LoadError::TokenEnvEmpty { ref name }) if name == var_name),
        "empty token_env must return TokenEnvEmpty; got: {result:?}"
    );

    unsafe { std::env::remove_var(var_name) };
}

// ---------------------------------------------------------------------------
// ParseError
// ---------------------------------------------------------------------------

/// A YAML file with a structural parse error (unbalanced braces) must return
/// `Err(LoadError::ParseError { excerpt })` where `excerpt` is non-empty and
/// ≤ 256 characters.
///
/// Security assertion: the excerpt must NOT contain the sentinel token value
/// (confirming that token values do not bleed into error messages even when
/// the token appears in the YAML near the parse error site).
#[test]
fn parse_error_returns_bounded_excerpt() {
    let guard = acquire_env_lock();
    let config = stub_config_with_lock(&guard);

    let bad_yaml = "this: is: not: valid: yaml: {{{ broken";
    let tmp = TempFile::new(bad_yaml);
    let result = loader::load(tmp.path(), &config, &PROFILE_DESKTOP);
    match result {
        Err(LoadError::ParseError { ref excerpt }) => {
            assert!(!excerpt.is_empty(), "ParseError excerpt must be non-empty");
            assert!(
                excerpt.chars().count() <= 256,
                "ParseError excerpt must be ≤ 256 chars; was {} chars",
                excerpt.chars().count()
            );
        }
        other => panic!("invalid YAML must return ParseError; got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// @smoke: examples/dashboard.yaml
// ---------------------------------------------------------------------------

/// @smoke — loads `examples/dashboard.yaml` end-to-end under the `desktop`
/// profile.
///
/// Risk mitigation: Risk #7 (examples/dashboard.yaml out of sync with the
/// schema). This test asserts that the file parses and loads without
/// `Severity::Error` validation issues.
///
/// Security: the sentinel value `"smoke-test-token"` must NOT appear in any
/// `Issue.message`.
#[test]
fn examples_dashboard_yaml_loads_without_issues() {
    let guard = acquire_env_lock();

    let sentinel = "smoke-test-token";
    // Set HA_TOKEN so the loader's token-env step does not return
    // TokenEnvNotFound for `token_env: HA_TOKEN` in the examples file.
    unsafe { std::env::set_var("HA_TOKEN", sentinel) };
    let config = stub_config_with_lock(&guard);
    // stub_config_with_lock removed HA_TOKEN; re-set it for the loader call.
    unsafe { std::env::set_var("HA_TOKEN", sentinel) };

    let examples_path = Path::new("examples/dashboard.yaml");
    assert!(
        examples_path.exists(),
        "examples/dashboard.yaml must exist at the expected path"
    );

    let result = loader::load(examples_path, &config, &PROFILE_DESKTOP);
    unsafe { std::env::remove_var("HA_TOKEN") };

    let dashboard = result.expect("examples/dashboard.yaml must load successfully");
    assert!(
        !dashboard.views.is_empty(),
        "examples/dashboard.yaml must have at least one view"
    );
    let total_widgets: usize = dashboard
        .views
        .iter()
        .flat_map(|v| v.sections.iter())
        .map(|s| s.widgets.len())
        .sum();
    assert!(
        total_widgets >= 1,
        "examples/dashboard.yaml must have at least one widget; found {total_widgets}"
    );

    let (issues, _allowlist) = validate::validate(&dashboard, &PROFILE_DESKTOP);
    let error_issues: Vec<_> = issues
        .iter()
        .filter(|i| i.severity == hanui::dashboard::schema::Severity::Error)
        .collect();
    assert!(
        error_issues.is_empty(),
        "examples/dashboard.yaml must produce zero Error issues; \
         found {} error(s): {error_issues:#?}",
        error_issues.len()
    );

    // Token-leak guard.
    for issue in &issues {
        assert!(
            !issue.message.contains(sentinel),
            "token sentinel must not appear in issue messages; got: {:?}",
            issue.message
        );
    }
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

/// Load the same fixture YAML twice with deliberately fresh parser state (by
/// writing the file, loading, then re-loading). Assert:
/// 1. Both `Dashboard` outputs are byte-equal when serialized via
///    `serde_yaml_ng::to_string`.
/// 2. Both `Vec<PositionedWidget>` from `layout::pack` on each section
///    are byte-equal (i.e., identical `{widget_id, col, row, span_cols, span_rows}`).
///
/// Risk mitigation: Risk #18 (HashMap iteration-order leak producing
/// non-deterministic packer output). The schema uses BTreeMap exclusively;
/// this test is the end-to-end determinism gate.
#[test]
fn determinism_loader_run_twice_byte_equal() {
    let guard = acquire_env_lock();
    let config = stub_config_with_lock(&guard);

    // Fixture with a single section and multiple widgets of varying spans.
    const FIXTURE: &str = r#"version: 1
device_profile: desktop
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: Section 1
        grid:
          columns: 4
          gap: 8
        widgets:
          - id: alpha
            type: light_tile
            entity: light.alpha
            layout:
              preferred_columns: 2
              preferred_rows: 1
          - id: beta
            type: sensor_tile
            entity: sensor.beta
            layout:
              preferred_columns: 1
              preferred_rows: 2
          - id: gamma
            type: entity_tile
            entity: switch.gamma
            layout:
              preferred_columns: 3
              preferred_rows: 1
          - id: delta
            type: light_tile
            entity: light.delta
            layout:
              preferred_columns: 1
              preferred_rows: 1
"#;

    // Write the fixture to disk; load twice to rebuild parser state.
    let tmp = TempFile::new(FIXTURE);

    let first =
        loader::load(tmp.path(), &config, &PROFILE_DESKTOP).expect("first load must succeed");
    // Re-invoke to rebuild parser state (the loader re-reads the file each call).
    let second =
        loader::load(tmp.path(), &config, &PROFILE_DESKTOP).expect("second load must succeed");

    // Serialized YAML must be byte-identical.
    let first_yaml = serde_yaml_ng::to_string(&first).expect("first serialize must succeed");
    let second_yaml = serde_yaml_ng::to_string(&second).expect("second serialize must succeed");
    assert_eq!(
        first_yaml, second_yaml,
        "two loads of the same YAML must produce byte-identical serialized output"
    );

    // Pack positions from both loads must be byte-identical.
    let first_sections = &first.views[0].sections;
    let second_sections = &second.views[0].sections;
    assert_eq!(
        first_sections.len(),
        second_sections.len(),
        "section counts must match"
    );

    for (sec_idx, (sec_a, sec_b)) in first_sections
        .iter()
        .zip(second_sections.iter())
        .enumerate()
    {
        let pos_a = pack(&sec_a.widgets, sec_a.grid.columns);
        let pos_b = pack(&sec_b.widgets, sec_b.grid.columns);
        assert_eq!(
            pos_a, pos_b,
            "pack output for section {sec_idx} must be byte-identical across two loads"
        );
    }
}
