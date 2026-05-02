//! YAML configuration loader for the dashboard.
//!
//! [`load`] is the single public entry point. It reads a `dashboard.yaml` file,
//! enforces the 256 KiB pre-parse byte cap, parses the YAML into a typed
//! [`Dashboard`], delegates `token_env` resolution to
//! [`Config::resolve_token_env`], runs the schema validator, and returns the
//! populated `Dashboard` on success.
//!
//! # No-direct-env-var contract
//!
//! This module MUST NOT call the standard library's environment-variable lookup
//! function directly. All environment-variable lookups go through
//! [`Config::resolve_token_env`]. A unit test
//! (`loader::tests::no_env_var_call_in_loader_source`) reads this file's source
//! at test time and asserts the constraint has not regressed.
//!
//! # Error taxonomy
//!
//! | Variant | Cause |
//! |---|---|
//! | `ConfigNotFound` | File path does not exist on disk. |
//! | `ConfigTooLarge` | File exceeds `MAX_YAML_BYTES` before parsing. |
//! | `ParseError` | `serde_yaml_ng` returns a parse error. |
//! | `TokenEnvNotFound` | `token_env` env var is absent. |
//! | `TokenEnvEmpty` | `token_env` env var is set but empty. |
//! | `Validation` | Validator produced Error-severity `Issue` entries. |
//!
//! # Path resolution
//!
//! `path` is the caller-supplied argument (from `--config <file>` or the
//! XDG default `$XDG_CONFIG_HOME/hanui/dashboard.yaml`). No silent fallback to
//! `examples/` is performed — the caller must resolve the path before calling.
//!
//! # Parent plan
//!
//! `docs/plans/2026-04-29-phase-4-layout.md` — relevant decisions:
//! `yaml_loader_size_cap`, `token_env_failure_mode`, `platform_config_naming`,
//! `view_spec_disposition`.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::dashboard::profiles::DeviceProfile;
use crate::dashboard::schema::{Dashboard, Issue, Severity};
use crate::platform::config::{Config, ConfigError};

/// Hard byte cap on dashboard YAML files (256 KiB).
///
/// Enforced BEFORE handing bytes to the YAML parser. Protects against:
/// 1. Legitimate-but-bloated configs allocating multi-MB on small SBCs.
/// 2. YAML-bomb / billion-laughs alias-expansion — even before the parser
///    sees aliases, the byte cap shuts down deep nesting payloads.
///
/// Realistic `dashboard.yaml` configs are 1–20 KiB (the in-tree
/// `examples/dashboard.yaml` is under 1 KiB); 256 KiB is generous.
pub const MAX_YAML_BYTES: usize = 256 * 1024;

/// Errors returned by [`load`].
#[derive(Debug, Error)]
pub enum LoadError {
    /// The config file path does not exist.
    #[error("dashboard config not found: {}", path.display())]
    ConfigNotFound {
        /// The path that was tried.
        path: PathBuf,
    },
    /// The config file exceeds the pre-parse byte cap.
    #[error("dashboard config too large: {bytes} bytes exceeds cap of {cap} bytes (256 KiB)")]
    ConfigTooLarge {
        /// Actual file size in bytes.
        bytes: usize,
        /// The cap in bytes.
        cap: usize,
    },
    /// The YAML failed to parse.
    #[error("dashboard config parse error: {excerpt}")]
    ParseError {
        /// The offending YAML line(s), bounded to ≤256 chars. No token
        /// material is captured here — the excerpt is from the structural
        /// YAML error location, never from a token value.
        excerpt: String,
    },
    /// The `home_assistant.token_env` env var does not exist.
    #[error("Home Assistant token env var '{name}' is not set; set it before starting hanui")]
    TokenEnvNotFound {
        /// Name of the missing environment variable.
        name: String,
    },
    /// The `home_assistant.token_env` env var exists but is empty.
    #[error("Home Assistant token env var '{name}' is empty; set it before starting hanui")]
    TokenEnvEmpty {
        /// Name of the empty environment variable.
        name: String,
    },
    /// The dashboard passed parsing but failed schema validation.
    ///
    /// Only Error-severity issues are reported here; Warning-severity issues
    /// are attached to the returned `Dashboard` but do not prevent loading.
    #[error("dashboard validation failed with {} error(s)", issues.len())]
    Validation {
        /// All Error-severity validation issues.
        issues: Vec<Issue>,
    },
}

/// Read and parse `path` into a [`Dashboard`] without resolving the
/// `home_assistant.token_env` env var or running the Phase 4 validator.
///
/// This is the **F4-bootstrap pre-flight** entry point used by
/// `src/lib.rs::run` (TASK-120a) to read the `device_profile` field BEFORE
/// the Tokio runtime is constructed. The full [`load`] function is still
/// invoked later by `run_with_live_store`; that second call is the one that
/// performs token resolution and (eventually) full schema validation.
///
/// # Why a separate entry point?
///
/// During bootstrap we have not yet decided which Tokio worker count to use,
/// and the only thing we need from the YAML is the typed `device_profile`
/// enum. Performing token resolution here would mean validating env-var
/// presence twice on every startup (once for the profile pre-flight, once
/// for the real load), and would expand the surface where a missing
/// `HA_TOKEN`-style env var could change the runtime's thread count. By
/// limiting this entry point to parse-only behaviour we keep the rule
/// "Tokio runtime size is determined solely by the YAML `device_profile`
/// field" structurally enforced.
///
/// # Errors
///
/// Same first three error variants as [`load`]:
/// * [`LoadError::ConfigNotFound`]
/// * [`LoadError::ConfigTooLarge`]
/// * [`LoadError::ParseError`]
///
/// Token-env and validation errors are NOT returned here — they are surfaced
/// later by the call to [`load`] inside `run_with_live_store`.
pub fn load_dashboard_only(path: &Path) -> Result<Dashboard, LoadError> {
    if !path.exists() {
        return Err(LoadError::ConfigNotFound {
            path: path.to_owned(),
        });
    }

    let bytes = std::fs::read(path).map_err(|_| LoadError::ConfigNotFound {
        path: path.to_owned(),
    })?;

    if bytes.len() > MAX_YAML_BYTES {
        return Err(LoadError::ConfigTooLarge {
            bytes: bytes.len(),
            cap: MAX_YAML_BYTES,
        });
    }

    serde_yaml_ng::from_slice::<Dashboard>(&bytes).map_err(|e| {
        let msg = e.to_string();
        let excerpt = msg.chars().take(256).collect::<String>();
        LoadError::ParseError { excerpt }
    })
}

/// Load, parse, and validate a `dashboard.yaml` file.
///
/// # Arguments
///
/// * `path` — path to the YAML config file. The caller is responsible for
///   resolving `--config <file>` or `$XDG_CONFIG_HOME/hanui/dashboard.yaml`
///   before calling this function. No silent fallback to `examples/` is
///   performed.
/// * `config` — the platform config used to resolve the `token_env` name.
///   [`Config::resolve_token_env`] is called for the `home_assistant.token_env`
///   field; the loader itself NEVER calls the env-var lookup directly.
/// * `profile` — the static [`DeviceProfile`] selected by the F4-bootstrap
///   pre-flight in `src/lib.rs::run` (TASK-120a). Forwarded into
///   [`crate::dashboard::validate::validate`] so per-profile caps
///   (`max_widgets_per_view`, `camera_interval_min_s`, `history_window_max_s`,
///   …) are enforced at load time. The reference is `'static` to match
///   TASK-120b's threading of `&'static DeviceProfile` through every live-path
///   entry point — every shipped profile lives in `static` storage
///   (`PROFILE_DESKTOP`, `PROFILE_OPI_ZERO3`, `PROFILE_RPI4`).
///
/// # Errors
///
/// See [`LoadError`] for the full taxonomy.
pub fn load(
    path: &Path,
    config: &Config,
    profile: &'static DeviceProfile,
) -> Result<Dashboard, LoadError> {
    // Step 1: check existence.
    if !path.exists() {
        return Err(LoadError::ConfigNotFound {
            path: path.to_owned(),
        });
    }

    // Step 2: read bytes.
    let bytes = std::fs::read(path).map_err(|_| LoadError::ConfigNotFound {
        path: path.to_owned(),
    })?;

    // Step 3: enforce byte cap BEFORE parsing (mitigates YAML bomb / RSS spike).
    if bytes.len() > MAX_YAML_BYTES {
        return Err(LoadError::ConfigTooLarge {
            bytes: bytes.len(),
            cap: MAX_YAML_BYTES,
        });
    }

    // Step 4: parse YAML into typed Dashboard.
    let mut dashboard: Dashboard = serde_yaml_ng::from_slice(&bytes).map_err(|e| {
        // Extract a bounded excerpt from the error message; never return the
        // full YAML (which might contain token-adjacent context).
        let msg = e.to_string();
        let excerpt = msg.chars().take(256).collect::<String>();
        LoadError::ParseError { excerpt }
    })?;

    // Step 5: resolve token_env if home_assistant is present.
    // The loader delegates to config.resolve_token_env — never calls the env lookup directly.
    if let Some(ha) = &dashboard.home_assistant {
        config
            .resolve_token_env(&ha.token_env)
            .map_err(|config_err| match config_err {
                ConfigError::TokenEnvNotFound { name } => LoadError::TokenEnvNotFound { name },
                ConfigError::TokenEnvEmpty { name } => LoadError::TokenEnvEmpty { name },
                // Missing / Empty refer to the static HA_URL / HA_TOKEN vars —
                // those come from Config::from_env, not resolve_token_env.
                // resolve_token_env only returns TokenEnvNotFound / TokenEnvEmpty.
                ConfigError::Missing { var } => LoadError::TokenEnvNotFound {
                    name: var.to_owned(),
                },
                ConfigError::Empty { var } => LoadError::TokenEnvEmpty {
                    name: var.to_owned(),
                },
            })?;
        // Token value is not stored — the loader only validates it exists and
        // is non-empty at load time. The WS client re-reads it via
        // Config::expose_token at connection time.
    }

    // Step 6: validate against the selected hardware profile.
    //
    // TASK-121 (F5) wires the real Phase 4/6 validator (`validate.rs`) into the
    // loader. The validator returns:
    //   * `Vec<Issue>` — every rule finding, both Error and Warning severities.
    //     Only Error-severity issues abort the load; Warnings are surfaced
    //     elsewhere (the dashboard still loads).
    //   * `CallServiceAllowlist` — the per-config allowlist of (domain, service)
    //     pairs found in widget action fields. Per
    //     `locked_decisions.call_service_allowlist_runtime_access`, this is the
    //     future consumer for the runtime actions queue (TASK-090). The
    //     loader does not store the allowlist here — that wiring belongs to
    //     a follow-up task and is intentionally out of scope for TASK-121.
    let (issues, _allowlist) = crate::dashboard::validate::validate(&dashboard, profile);
    let errors: Vec<Issue> = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .cloned()
        .collect();
    if !errors.is_empty() {
        return Err(LoadError::Validation { issues: errors });
    }

    // Step 7: build the entity → widget dependency index for the
    // visibility evaluator (TASK-110). The bridge holds an `Arc<Dashboard>`
    // and reads `dashboard.dep_index` on each `state_changed` event to
    // resolve, in O(1), which widgets need a visibility re-evaluation.
    //
    // Per `locked_decisions.dep_index_partial_eq`, the field has
    // `#[serde(default, skip)]`; we populate it AFTER serde parsing.
    let dep_index = crate::dashboard::visibility::build_dep_index(&dashboard);
    dashboard.dep_index = std::sync::Arc::new(dep_index);

    Ok(dashboard)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::profiles::{PROFILE_DESKTOP, PROFILE_OPI_ZERO3, PROFILE_RPI4};
    use crate::platform::config::Config;

    /// Construct a minimal stub `Config` that satisfies the loader's
    /// `config: &Config` parameter without touching real env vars.
    fn stub_config() -> Config {
        Config::new_for_testing("ws://stub".to_string())
    }

    /// Default static profile used by tests that do not exercise per-profile
    /// validation rules. Most tests pin `device_profile: rpi4` in their YAML
    /// fixtures (the `MINIMAL_YAML` constant); the validator does not consult
    /// the YAML's `device_profile` field — it consults the profile passed in
    /// here. Using `&PROFILE_DESKTOP` is intentional: its caps are the
    /// loosest of the three presets, so existing tests that aim to exercise
    /// non-validation behaviour (size cap, ParseError, token resolution)
    /// remain green even when a YAML payload happens to contain widgets.
    fn stub_profile() -> &'static DeviceProfile {
        &PROFILE_DESKTOP
    }

    /// Minimal valid YAML dashboard payload.
    const MINIMAL_YAML: &str = r#"version: 1
device_profile: rpi4
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"#;

    /// Write `content` to a temporary file and return a `TempFile` guard.
    ///
    /// The file is deleted when the guard goes out of scope. Uses
    /// `std::env::temp_dir()` so no external `tempfile` crate is required.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(content: &str) -> Self {
            use std::io::Write as _;
            // Use a unique name derived from a thread ID + nanosecond timestamp
            // to avoid collisions in parallel test runs.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            let tid = std::thread::current().id();
            let name = format!("hanui_test_{tid:?}_{nanos}.yaml");
            let path = std::env::temp_dir().join(name);
            let mut f = std::fs::File::create(&path).expect("temp file create");
            f.write_all(content.as_bytes()).expect("temp file write");
            f.flush().expect("temp file flush");
            TempFile(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // -----------------------------------------------------------------------
    // Size cap — boundary tests
    // -----------------------------------------------------------------------

    /// A 256 KiB payload should be accepted (Ok or Validation only, not
    /// ConfigTooLarge).
    ///
    /// We synthesize a deliberately pathological one-key-per-line YAML payload
    /// to maximise AST node count while staying within the byte cap.
    #[test]
    fn config_accepted_at_256_kib() {
        let cap = MAX_YAML_BYTES; // 256 * 1024

        // Build a valid-YAML-but-large string: start with valid headers, then
        // pad with comment lines (# ...) to reach exactly `cap` bytes.
        // The final content is parseable YAML but the `Dashboard` type won't
        // deserialise from the comment-padded content — we only care that it
        // does NOT return ConfigTooLarge.
        let header = MINIMAL_YAML;
        let mut payload = header.to_string();
        while payload.len() < cap {
            payload.push_str("# padding line to reach 256KiB cap boundary\n");
        }
        // Trim to exactly `cap` bytes (may cut a partial line).
        payload.truncate(cap);
        assert_eq!(payload.len(), cap);

        let tmp = TempFile::new(&payload);
        let result = load(tmp.path(), &stub_config(), stub_profile());
        // Must NOT be ConfigTooLarge.
        assert!(
            !matches!(result, Err(LoadError::ConfigTooLarge { .. })),
            "256 KiB payload must not trigger ConfigTooLarge; got: {result:?}"
        );
    }

    /// A 257 KiB payload must return `Err(LoadError::ConfigTooLarge)` BEFORE
    /// parsing (i.e., without touching the YAML parser at all).
    #[test]
    fn config_too_large_at_257_kib() {
        // We use 257 * 1024 to match the acceptance criterion's exact wording.
        let target = 257 * 1024;
        let payload = "x".repeat(target);

        let tmp = TempFile::new(&payload);
        let result = load(tmp.path(), &stub_config(), stub_profile());
        match result {
            Err(LoadError::ConfigTooLarge { bytes, cap }) => {
                assert_eq!(bytes, target, "bytes field must reflect actual file size");
                assert_eq!(cap, MAX_YAML_BYTES, "cap field must be MAX_YAML_BYTES");
            }
            other => panic!("expected ConfigTooLarge for 257 KiB payload, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // ConfigNotFound
    // -----------------------------------------------------------------------

    #[test]
    fn config_not_found_returns_error() {
        let path = std::path::Path::new("/tmp/hanui_nonexistent_dashboard_xyz_082.yaml");
        let result = load(path, &stub_config(), stub_profile());
        assert!(
            matches!(result, Err(LoadError::ConfigNotFound { .. })),
            "missing file must return ConfigNotFound; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // ParseError
    // -----------------------------------------------------------------------

    #[test]
    fn parse_error_on_invalid_yaml_returns_error() {
        let bad_yaml = "this: is: not: valid: yaml: {{{";
        let tmp = TempFile::new(bad_yaml);
        let result = load(tmp.path(), &stub_config(), stub_profile());
        assert!(
            matches!(result, Err(LoadError::ParseError { .. })),
            "invalid YAML must return ParseError; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // token_env resolution
    // -----------------------------------------------------------------------

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// YAML with `home_assistant.token_env` pointing to an absent env var.
    fn yaml_with_token_env(token_env_name: &str) -> String {
        format!(
            r#"version: 1
device_profile: rpi4
default_view: home
home_assistant:
  url: "ws://homeassistant.local:8123/api/websocket"
  token_env: "{token_env_name}"
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"#
        )
    }

    #[test]
    fn token_env_not_found_returns_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        let var_name = "HANUI_TEST_TOKEN_NOT_FOUND_082";
        // Ensure the var is absent.
        unsafe { std::env::remove_var(var_name) };

        let yaml = yaml_with_token_env(var_name);
        let tmp = TempFile::new(&yaml);
        let result = load(tmp.path(), &stub_config(), stub_profile());
        assert!(
            matches!(result, Err(LoadError::TokenEnvNotFound { ref name }) if name == var_name),
            "absent token_env var must return TokenEnvNotFound; got: {result:?}"
        );
    }

    #[test]
    fn token_env_empty_returns_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        let var_name = "HANUI_TEST_TOKEN_EMPTY_082";
        unsafe { std::env::set_var(var_name, "") };

        let yaml = yaml_with_token_env(var_name);
        let tmp = TempFile::new(&yaml);
        let result = load(tmp.path(), &stub_config(), stub_profile());
        assert!(
            matches!(result, Err(LoadError::TokenEnvEmpty { ref name }) if name == var_name),
            "empty token_env var must return TokenEnvEmpty; got: {result:?}"
        );
        unsafe { std::env::remove_var(var_name) };
    }

    // -----------------------------------------------------------------------
    // Mechanical no-direct-env-lookup gate
    // -----------------------------------------------------------------------

    /// Regression guard: assert that `loader.rs` contains no direct call to the
    /// standard library's environment-variable lookup function.
    ///
    /// The loader must always delegate to `Config::resolve_token_env`. This
    /// test scans non-comment, non-string-literal lines for the call pattern.
    ///
    /// This test reads the source file at test time (relative to the crate
    /// root, which is cargo's cwd during `cargo test`).
    #[test]
    fn no_env_var_call_in_loader_source() {
        let source =
            std::fs::read_to_string("src/dashboard/loader.rs").expect("loader.rs must be readable");
        // Build the forbidden call-site pattern by concatenation so this test
        // file itself does not contain the literal string being checked for.
        // This prevents the test from falsely matching its own assertion text.
        let forbidden = format!("{}::{}", "env", "var(");
        let call_pattern_std = format!("std::{}", forbidden);

        let violation_lines: Vec<(usize, &str)> = source
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let trimmed = line.trim_start();
                // Skip pure comment lines.
                if trimmed.starts_with("//") {
                    return false;
                }
                // Skip lines that contain the pattern only inside a string literal
                // (heuristic: if the pattern appears after a '"' before a '"').
                // We check for the pattern AND require it is not inside a test
                // assertion's message string.
                line.contains(&call_pattern_std) || {
                    // Also catch bare `env::var(` that does not start with `std::`.
                    // Strip out string-delimited content first (simple heuristic).
                    let without_strings = strip_string_literals(line);
                    without_strings.contains(&forbidden)
                }
            })
            .collect();

        assert!(
            violation_lines.is_empty(),
            "loader.rs must not call env-var lookup directly; delegate to Config::resolve_token_env. \
             Offending lines: {:?}",
            violation_lines.iter().map(|(n, l)| format!("line {}: {}", n + 1, l)).collect::<Vec<_>>()
        );
    }

    /// Very simple string-literal stripper: replaces content between `"..."` with
    /// spaces so that patterns in string literals don't match code checks.
    fn strip_string_literals(line: &str) -> String {
        let mut result = String::with_capacity(line.len());
        let mut in_string = false;
        let mut prev_char = '\0';
        for ch in line.chars() {
            if ch == '"' && prev_char != '\\' {
                in_string = !in_string;
                result.push(' ');
            } else if in_string {
                result.push(' ');
            } else {
                result.push(ch);
            }
            prev_char = ch;
        }
        result
    }

    // -----------------------------------------------------------------------
    // Happy path — successful load
    // -----------------------------------------------------------------------

    #[test]
    fn happy_path_loads_minimal_yaml() {
        let tmp = TempFile::new(MINIMAL_YAML);
        let result = load(tmp.path(), &stub_config(), stub_profile());
        assert!(
            result.is_ok(),
            "minimal valid YAML must load successfully; got: {result:?}"
        );
        let dashboard = result.unwrap();
        assert_eq!(dashboard.version, 1);
        assert_eq!(
            dashboard.device_profile,
            crate::dashboard::schema::ProfileKey::Rpi4
        );
    }

    #[test]
    fn happy_path_with_token_env_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let var_name = "HANUI_TEST_TOKEN_HAPPY_082";
        unsafe { std::env::set_var(var_name, "my-ha-token") };

        let yaml = yaml_with_token_env(var_name);
        let tmp = TempFile::new(&yaml);
        let result = load(tmp.path(), &stub_config(), stub_profile());
        assert!(
            result.is_ok(),
            "YAML with valid token_env must load successfully; got: {result:?}"
        );
        unsafe { std::env::remove_var(var_name) };
    }

    // -----------------------------------------------------------------------
    // load_dashboard_only — TASK-120a F4-bootstrap pre-flight
    // -----------------------------------------------------------------------

    /// `load_dashboard_only` happy path: a minimal valid YAML returns a
    /// `Dashboard` whose `device_profile` matches the YAML field, WITHOUT
    /// touching `home_assistant.token_env` resolution. No env-var setup is
    /// required because this entry point skips token resolution entirely.
    #[test]
    fn load_dashboard_only_returns_parsed_dashboard_without_token_resolution() {
        // Build a YAML payload that DOES include `home_assistant.token_env`,
        // but with a token env name we deliberately leave UNSET. If the
        // bootstrap pre-flight accidentally invoked `Config::resolve_token_env`
        // it would error out; load_dashboard_only must succeed regardless.
        let var_name = "HANUI_TEST_BOOTSTRAP_NEVER_RESOLVED_120A";
        unsafe { std::env::remove_var(var_name) };

        let yaml = format!(
            r#"version: 1
device_profile: opi-zero3
default_view: home
home_assistant:
  url: "ws://homeassistant.local:8123/api/websocket"
  token_env: "{var_name}"
views:
  - id: home
    title: Home
    layout: sections
    sections: []
"#
        );
        let tmp = TempFile::new(&yaml);
        let result = load_dashboard_only(tmp.path());
        assert!(
            result.is_ok(),
            "bootstrap pre-flight must succeed without env-var setup; got: {result:?}"
        );
        let dashboard = result.expect("ok above");
        assert_eq!(
            dashboard.device_profile,
            crate::dashboard::schema::ProfileKey::OpiZero3,
            "device_profile must round-trip through the parser"
        );
    }

    /// `load_dashboard_only` propagates `ConfigNotFound` for a missing path —
    /// matches the behaviour of the full `load` for the same condition.
    #[test]
    fn load_dashboard_only_config_not_found_for_missing_path() {
        let path = std::path::Path::new("/tmp/hanui_nonexistent_bootstrap_120a.yaml");
        let result = load_dashboard_only(path);
        assert!(
            matches!(result, Err(LoadError::ConfigNotFound { .. })),
            "missing path must return ConfigNotFound; got: {result:?}"
        );
    }

    /// `load_dashboard_only` enforces the same 256 KiB byte cap as `load`;
    /// the F4-bootstrap pre-flight must not be a back door around the
    /// YAML-bomb / RSS-spike mitigation.
    #[test]
    fn load_dashboard_only_config_too_large_above_cap() {
        let payload = "x".repeat(MAX_YAML_BYTES + 1);
        let tmp = TempFile::new(&payload);
        let result = load_dashboard_only(tmp.path());
        assert!(
            matches!(result, Err(LoadError::ConfigTooLarge { .. })),
            "byte-cap overflow must return ConfigTooLarge; got: {result:?}"
        );
    }

    /// `load_dashboard_only` returns `ParseError` for malformed YAML.
    #[test]
    fn load_dashboard_only_parse_error_on_invalid_yaml() {
        let bad_yaml = "this: is: not: valid: yaml: {{{";
        let tmp = TempFile::new(bad_yaml);
        let result = load_dashboard_only(tmp.path());
        assert!(
            matches!(result, Err(LoadError::ParseError { .. })),
            "malformed YAML must return ParseError; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Parse-time RSS test (Risk #17)
    // -----------------------------------------------------------------------

    /// Assert that parsing a worst-case 256 KiB YAML config does not push
    /// RSS above `PROFILE_OPI_ZERO3.idle_rss_mb_cap` (60 MB).
    ///
    /// The payload uses a deliberately pathological one-key-per-line structure
    /// to maximise YAML AST node count. If this test fails, the byte cap
    /// should be tightened (per the acceptance criterion: "If this test fails,
    /// the byte cap is tightened").
    ///
    /// NOTE: This test measures the delta in process RSS by reading
    /// `/proc/self/status` (VmRSS). It is Linux-only. On non-Linux platforms
    /// the test passes unconditionally (no measurement available).
    #[test]
    fn parse_time_rss_under_opi_zero3_budget() {
        use crate::dashboard::profiles::PROFILE_OPI_ZERO3;

        // Build a pathological 256 KiB YAML payload: one mapping key per line.
        // The content is syntactically valid YAML but won't deserialize as a
        // `Dashboard` (ParseError expected); we only care about RSS, not parse
        // correctness.
        let cap = MAX_YAML_BYTES;
        let header = "# pathological YAML payload for RSS budget test\n";
        let mut payload = header.to_string();
        let mut i: u64 = 0;
        while payload.len() < cap {
            payload.push_str(&format!("key_{i}: value_{i}\n"));
            i += 1;
        }
        payload.truncate(cap);

        let rss_before_kb = read_proc_rss_kb();

        let tmp = TempFile::new(&payload);
        // The load may succeed or fail — we only care about RSS.
        let _result = load(tmp.path(), &stub_config(), stub_profile());

        let rss_after_kb = read_proc_rss_kb();

        if let (Some(before), Some(after)) = (rss_before_kb, rss_after_kb) {
            let delta_mb = (after.saturating_sub(before)) / 1024;
            let budget_mb = PROFILE_OPI_ZERO3.idle_rss_mb_cap as u64;
            assert!(
                delta_mb <= budget_mb,
                "parse-time RSS delta {delta_mb} MiB exceeds opi_zero3 budget \
                 {budget_mb} MiB for a 256 KiB YAML payload — tighten MAX_YAML_BYTES"
            );
        }
        // If /proc/self/status is unavailable, the test passes (non-Linux CI).
    }

    // -----------------------------------------------------------------------
    // Validator integration (TASK-121 F5)
    //
    // These tests assert the loader threads the chosen `&'static DeviceProfile`
    // into `validate::validate()` and that profile-bound caps abort the load
    // with `LoadError::Validation`. Per-profile cap values (asserted in
    // `profiles::tests::preset_values_match_phases_md_budgets_table`) are
    // duplicated in the YAML fixture sizing here as load-bearing test inputs:
    //   * PROFILE_RPI4.max_widgets_per_view = 32
    //   * PROFILE_OPI_ZERO3.max_widgets_per_view = 20
    // -----------------------------------------------------------------------

    /// Build a YAML payload with `count` widgets in a single section. Each
    /// widget renders as `light_tile` with `preferred_columns: 1` so it fits
    /// inside a 1-column grid (no SpanOverflow noise — only the widget-count
    /// rule fires).
    fn yaml_with_n_widgets(device_profile_kebab: &str, count: usize) -> String {
        let mut widgets = String::with_capacity(count * 96);
        for i in 0..count {
            widgets.push_str(&format!(
                "          - id: w{i}\n            \
                 type: light_tile\n            \
                 entity: light.w{i}\n            \
                 visibility: always\n            \
                 layout:\n              \
                 preferred_columns: 1\n              \
                 preferred_rows: 1\n",
            ));
        }
        format!(
            r#"version: 1
device_profile: {device_profile_kebab}
default_view: home
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: s1
        title: S1
        grid:
          columns: 1
          gap: 8
        widgets:
{widgets}"#,
        )
    }

    /// rpi4 caps `max_widgets_per_view` at 32. A YAML fixture with 33 widgets
    /// in a single view must abort the load with `LoadError::Validation`,
    /// surfacing the `MaxWidgetsPerViewExceeded` issue (Error severity).
    ///
    /// This test is the load-time mirror of
    /// `validate_max_widgets_per_view_exceeded_is_error` in `validate.rs`; it
    /// guards the loader-validator wiring (TASK-121 F5) — moving the wiring
    /// without updating the test is caught immediately.
    #[test]
    fn rpi4_dashboard_over_widget_cap_fails_at_load() {
        let cap = PROFILE_RPI4.max_widgets_per_view;
        let yaml = yaml_with_n_widgets("rpi4", cap + 1);
        let tmp = TempFile::new(&yaml);

        let result = load(tmp.path(), &stub_config(), &PROFILE_RPI4);

        match result {
            Err(LoadError::Validation { issues }) => {
                assert!(
                    !issues.is_empty(),
                    "validation must surface at least one Error-severity issue"
                );
                assert!(
                    issues.iter().all(|i| i.severity == Severity::Error),
                    "LoadError::Validation must contain only Error-severity issues; got {issues:?}"
                );
                assert!(
                    issues.iter().any(|i| matches!(
                        i.rule,
                        crate::dashboard::schema::ValidationRule::MaxWidgetsPerViewExceeded
                    )),
                    "must surface MaxWidgetsPerViewExceeded for {} widgets in a {}-cap view; \
                     got: {issues:?}",
                    cap + 1,
                    cap,
                );
            }
            other => panic!(
                "{} widgets in a {}-cap rpi4 view must return LoadError::Validation; got: {other:?}",
                cap + 1,
                cap,
            ),
        }
    }

    /// opi_zero3 caps `max_widgets_per_view` at 20 — a tighter cap than rpi4's
    /// 32. A YAML fixture with 21 widgets must abort the load with
    /// `LoadError::Validation`. Test name uses "entity_cap" per the TASK-121
    /// brief: the validator's per-view widget cap is the principal entity-count
    /// gate enforced at load time (the `DeviceProfile.max_entities` field is
    /// enforced separately by `MemoryStore::load`, downstream of the loader).
    #[test]
    fn opi_zero3_dashboard_over_entity_cap_fails_at_load() {
        let cap = PROFILE_OPI_ZERO3.max_widgets_per_view;
        let yaml = yaml_with_n_widgets("opi-zero3", cap + 1);
        let tmp = TempFile::new(&yaml);

        let result = load(tmp.path(), &stub_config(), &PROFILE_OPI_ZERO3);

        match result {
            Err(LoadError::Validation { issues }) => {
                assert!(
                    !issues.is_empty(),
                    "validation must surface at least one Error-severity issue"
                );
                assert!(
                    issues.iter().all(|i| i.severity == Severity::Error),
                    "LoadError::Validation must contain only Error-severity issues; got {issues:?}"
                );
                assert!(
                    issues.iter().any(|i| matches!(
                        i.rule,
                        crate::dashboard::schema::ValidationRule::MaxWidgetsPerViewExceeded
                    )),
                    "must surface MaxWidgetsPerViewExceeded for {} widgets in a {}-cap view; \
                     got: {issues:?}",
                    cap + 1,
                    cap,
                );
            }
            other => panic!(
                "{} widgets in a {}-cap opi_zero3 view must return LoadError::Validation; \
                 got: {other:?}",
                cap + 1,
                cap,
            ),
        }
    }

    /// Positive control: at exactly the cap, the load succeeds. Without this
    /// pin, a future off-by-one in the validator (say, `>=` instead of `>`)
    /// would silently break loads for caps-edge configs while the negative
    /// tests above continue to pass.
    #[test]
    fn rpi4_dashboard_at_widget_cap_loads_successfully() {
        let cap = PROFILE_RPI4.max_widgets_per_view;
        let yaml = yaml_with_n_widgets("rpi4", cap);
        let tmp = TempFile::new(&yaml);

        let result = load(tmp.path(), &stub_config(), &PROFILE_RPI4);
        assert!(
            result.is_ok(),
            "{cap} widgets in a {cap}-cap view must NOT trigger validation; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Read the current process's resident set size from `/proc/self/status`
    /// (Linux only). Returns `None` on non-Linux platforms or if the read fails.
    fn read_proc_rss_kb() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest.trim().trim_end_matches(" kB").parse().ok()?;
                return Some(kb);
            }
        }
        None
    }
}
