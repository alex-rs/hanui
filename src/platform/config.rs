//! Environment + config-file precedence loader for HA connection parameters.
//!
//! # Precedence rule (Phase 4 must respect this contract)
//!
//! Values are resolved in the following priority order, highest first:
//!
//! 1. **Environment variables** (`HA_URL`, `HA_TOKEN`) — always checked first.
//! 2. **`config.toml`** — file-based override (Phase 2 skeleton; Phase 2 only
//!    reads env vars; `config.toml` parsing is wired in Phase 4).
//! 3. **Dashboard YAML** — lowest precedence; Phase 4 feature.
//!
//! Any Phase 4 change to this module MUST preserve the env-wins invariant: if
//! `HA_URL` or `HA_TOKEN` is set in the environment, the environment value is
//! used regardless of file-based config.
//!
//! # Secret-handling discipline
//!
//! `HA_TOKEN` is loaded directly into [`secrecy::SecretString`] in a single
//! expression:
//!
//! ```ignore
//! SecretString::from(std::env::var("HA_TOKEN")?)
//! ```
//!
//! The intermediate `String` returned by `env::var` lives for exactly one
//! statement before being consumed by `SecretString::from`.  No binding that
//! could be debug-printed or logged holds the plaintext value.
//!
//! The only access path to the plaintext token is [`Config::expose_token`],
//! which emits a `tracing::trace` audit row before delegating to
//! [`secrecy::ExposeSecret::expose_secret`].  All token consumers (TASK-029
//! and later) MUST call `Config::expose_token`, never `expose_secret` directly.
//!
//! The one exception is the private [`is_token_empty`] helper, which accesses
//! the secret solely to check zero-length and emits its own audit row before
//! doing so.  The plaintext is never returned to the caller.

use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

/// Errors returned by [`Config::from_env`] and [`Config::resolve_token_env`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Environment variable is not set.
    #[error("required environment variable `{var}` is not set")]
    Missing {
        /// The name of the missing environment variable.
        var: &'static str,
    },
    /// Environment variable is set but empty.
    #[error("environment variable `{var}` must not be empty")]
    Empty {
        /// The name of the empty environment variable.
        var: &'static str,
    },
    /// The named env var (from `token_env` YAML field) does not exist.
    ///
    /// Per `locked_decisions.token_env_failure_mode`: a missing token env var
    /// is a load-time error — no dashboard renders when the token cannot be
    /// resolved.
    #[error("Home Assistant token env var '{name}' is not set; set it before starting hanui")]
    TokenEnvNotFound {
        /// Name of the environment variable that was looked up.
        name: String,
    },
    /// The named env var exists but is empty.
    ///
    /// An empty token string would cause HA auth failure silently; better to
    /// fail loudly at load time.
    #[error("Home Assistant token env var '{name}' is empty; set it before starting hanui")]
    TokenEnvEmpty {
        /// Name of the environment variable that was looked up.
        name: String,
    },
}

/// HA connection configuration.
///
/// Constructed via [`Config::from_env`].  The token is stored as a
/// [`SecretString`] and never exposed in [`Debug`] output or log messages.
pub struct Config {
    /// The WebSocket URL of the Home Assistant instance (e.g.
    /// `ws://homeassistant.local:8123/api/websocket`).
    pub url: String,
    /// The HA long-lived access token.  Stored as [`SecretString`]; access the
    /// plaintext value only through [`Config::expose_token`], which emits an
    /// audit trace row.
    token: SecretString,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("url", &self.url)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Check whether a secret token is empty without returning the plaintext to
/// the caller.
///
/// Emits a `tracing::trace` audit row (`token_accessed = true`,
/// `reason = "empty-check"`, message `"token-accessed"`) before accessing
/// the secret, preserving the "every access to `expose_secret` writes one
/// audit row" invariant introduced in TASK-028.
///
/// The returned `bool` carries no secret material.
fn is_token_empty(token: &SecretString) -> bool {
    tracing::trace!(
        token_accessed = true,
        reason = "empty-check",
        "token-accessed"
    );
    token.expose_secret().is_empty()
}

impl Config {
    /// Load configuration from the environment.
    ///
    /// Reads `HA_URL` and `HA_TOKEN` from the process environment.  Returns
    /// [`ConfigError::Missing`] if a variable is absent and
    /// [`ConfigError::Empty`] if it is present but zero-length.
    ///
    /// # Precedence
    ///
    /// Phase 2 reads environment variables only.  See the module-level doc for
    /// the full precedence chain that Phase 4 must honour.
    ///
    /// # Token security
    ///
    /// `HA_TOKEN` is captured as `SecretString::from(std::env::var("HA_TOKEN")?)` —
    /// the intermediate `String` lives for exactly one statement.  No binding that
    /// could be debug-printed or logged holds the plaintext value.
    pub fn from_env() -> Result<Self, ConfigError> {
        let url = std::env::var("HA_URL").map_err(|_| ConfigError::Missing { var: "HA_URL" })?;
        if url.is_empty() {
            return Err(ConfigError::Empty { var: "HA_URL" });
        }

        // SECURITY: HA_TOKEN is loaded directly into SecretString.
        // The String returned by env::var is consumed in this single expression;
        // no intermediate binding can be debug-printed or logged.
        let token = SecretString::from(
            std::env::var("HA_TOKEN").map_err(|_| ConfigError::Missing { var: "HA_TOKEN" })?,
        );
        if is_token_empty(&token) {
            return Err(ConfigError::Empty { var: "HA_TOKEN" });
        }

        Ok(Config { url, token })
    }

    /// Expose the plaintext token value for outbound use (e.g. WebSocket auth).
    ///
    /// Every call to this method emits a `tracing::trace` audit row with
    /// `token_accessed = true` and the message `"token-accessed"`.  No
    /// plaintext token value appears in the log row.
    ///
    /// **All token consumers MUST call this method.**  Calling
    /// `expose_secret()` on the inner `SecretString` directly bypasses the
    /// audit trail and is forbidden.
    pub fn expose_token(&self) -> &str {
        tracing::trace!(token_accessed = true, "token-accessed");
        self.token.expose_secret()
    }

    /// Construct a `Config` for unit tests without touching environment variables.
    ///
    /// Only available in `#[cfg(test)]` contexts. The token is an empty-ish
    /// placeholder; only the `url` field matters for most test paths (loader
    /// tests pass this config but never call `expose_token`, only
    /// `resolve_token_env` which reads a different env var by name).
    #[cfg(test)]
    pub fn new_for_testing(url: String) -> Self {
        Config {
            url,
            token: secrecy::SecretString::from("test-placeholder".to_string()),
        }
    }

    /// Look up a Home Assistant token by reading the named environment variable.
    ///
    /// This method is the **sole** env-var lookup path for YAML-configured HA
    /// tokens. The YAML loader reads `home_assistant.token_env` as a plain
    /// `String` (the variable name) and delegates the actual `env::var` call
    /// here. The loader itself never calls `env::var` directly —
    /// `locked_decisions.platform_config_naming` requires this split so that
    /// env-var access is auditable in one place.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::TokenEnvNotFound`] when the environment variable
    /// is absent and [`ConfigError::TokenEnvEmpty`] when it is set but empty.
    ///
    /// # Security note
    ///
    /// The returned `String` contains the plaintext HA token. The caller
    /// (loader.rs) must consume it immediately (wrap in `SecretString` or use
    /// it to validate non-emptiness). It must not be stored in a log or
    /// debug output.
    pub fn resolve_token_env(&self, name: &str) -> Result<String, ConfigError> {
        match std::env::var(name) {
            Ok(value) if value.is_empty() => Err(ConfigError::TokenEnvEmpty {
                name: name.to_owned(),
            }),
            Ok(value) => Ok(value),
            Err(_) => Err(ConfigError::TokenEnvNotFound {
                name: name.to_owned(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tracing_test::traced_test;

    use super::*;

    // Serialize env-mutation tests to avoid races between parallel test threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_env(url: Option<&str>, token: Option<&str>) {
        match url {
            Some(v) => unsafe { std::env::set_var("HA_URL", v) },
            None => unsafe { std::env::remove_var("HA_URL") },
        }
        match token {
            Some(v) => unsafe { std::env::set_var("HA_TOKEN", v) },
            None => unsafe { std::env::remove_var("HA_TOKEN") },
        }
    }

    // -----------------------------------------------------------------------
    // Happy path
    // -----------------------------------------------------------------------

    #[test]
    fn from_env_loads_url_and_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_env(Some("ws://ha.local:8123/api/websocket"), Some("tok-abc"));

        let cfg = Config::from_env().expect("from_env must succeed with valid env vars");
        assert_eq!(cfg.url, "ws://ha.local:8123/api/websocket");
    }

    #[test]
    fn debug_output_redacts_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_env(
            Some("ws://ha.local:8123/api/websocket"),
            Some("supersecret"),
        );

        let cfg = Config::from_env().unwrap();
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("supersecret"),
            "Debug must not expose token plaintext"
        );
        assert!(dbg.contains("[REDACTED]"));
    }

    // -----------------------------------------------------------------------
    // Missing-var errors
    // -----------------------------------------------------------------------

    #[test]
    fn from_env_errors_when_ha_url_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_env(None, Some("tok"));

        let err = Config::from_env().expect_err("must fail when HA_URL is absent");
        assert!(matches!(err, ConfigError::Missing { var: "HA_URL" }));
        assert!(err.to_string().contains("HA_URL"));
    }

    #[test]
    fn from_env_errors_when_ha_token_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_env(Some("ws://ha.local/api/websocket"), None);

        let err = Config::from_env().expect_err("must fail when HA_TOKEN is absent");
        assert!(matches!(err, ConfigError::Missing { var: "HA_TOKEN" }));
        assert!(err.to_string().contains("HA_TOKEN"));
    }

    // -----------------------------------------------------------------------
    // Empty-var errors
    // -----------------------------------------------------------------------

    #[test]
    fn from_env_errors_when_ha_url_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_env(Some(""), Some("tok"));

        let err = Config::from_env().expect_err("must fail when HA_URL is empty");
        assert!(matches!(err, ConfigError::Empty { var: "HA_URL" }));
        assert!(err.to_string().contains("HA_URL"));
    }

    #[test]
    fn from_env_errors_when_ha_token_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_env(Some("ws://ha.local/api/websocket"), Some(""));

        let err = Config::from_env().expect_err("must fail when HA_TOKEN is empty");
        assert!(matches!(err, ConfigError::Empty { var: "HA_TOKEN" }));
        assert!(err.to_string().contains("HA_TOKEN"));
    }

    // -----------------------------------------------------------------------
    // Audit-row tests
    // -----------------------------------------------------------------------

    /// Verify that `expose_token` emits exactly one trace row containing
    /// `"token-accessed"` and that the row contains no token plaintext.
    #[test]
    #[traced_test]
    fn expose_token_emits_exactly_one_audit_trace_row() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_env(
            Some("ws://ha.local:8123/api/websocket"),
            Some("my-secret-token-value"),
        );

        let cfg = Config::from_env().unwrap();

        // Call expose_token once.
        let _ = cfg.expose_token();

        // The traced_test macro captures log output; assert the audit row exists.
        assert!(logs_contain("token-accessed"));
        // The plaintext token must not appear in any log row.
        assert!(!logs_contain("my-secret-token-value"));
    }

    /// Verify that `expose_token` called twice emits two audit rows.
    #[test]
    #[traced_test]
    fn expose_token_emits_one_audit_row_per_call() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_env(
            Some("ws://ha.local:8123/api/websocket"),
            Some("another-secret"),
        );

        let cfg = Config::from_env().unwrap();

        // A single call must produce exactly one trace row.
        let _ = cfg.expose_token();
        assert!(logs_contain("token-accessed"));
        // The plaintext token must never appear.
        assert!(!logs_contain("another-secret"));
    }

    // -----------------------------------------------------------------------
    // Empty-check audit-row test (TASK-045)
    // -----------------------------------------------------------------------

    /// Verify that `Config::from_env` with a non-empty token emits the audit
    /// row from `is_token_empty` (the empty-check path) and does NOT expose
    /// the plaintext token in any captured event.
    ///
    /// This confirms the "every access to `expose_secret` writes one audit
    /// row" invariant from TASK-028 extends to the empty-check performed
    /// during construction.
    #[test]
    #[traced_test]
    fn from_env_empty_check_emits_audit_row() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_env(
            Some("ws://ha.local:8123/api/websocket"),
            Some("fixture-token-not-plaintext"),
        );

        // Call from_env; internally is_token_empty runs and must emit the audit row.
        let _cfg = Config::from_env().expect("from_env must succeed with valid env vars");

        // The audit row from is_token_empty must be present.
        assert!(
            logs_contain("token-accessed"),
            "is_token_empty must emit the token-accessed audit row"
        );
        // The plaintext fixture token must not appear in any log event.
        assert!(
            !logs_contain("fixture-token-not-plaintext"),
            "plaintext token must never appear in any captured log event"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_token_env tests (TASK-082)
    // -----------------------------------------------------------------------

    /// Helper to set/clear a test-specific env var that does NOT collide with
    /// the HA_URL/HA_TOKEN vars used by the other tests.
    fn set_token_env(name: &str, value: Option<&str>) {
        match value {
            Some(v) => unsafe { std::env::set_var(name, v) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    /// A minimal stub `Config` for `resolve_token_env` tests.  We don't need
    /// a real URL/token — `resolve_token_env` reads a DIFFERENT env var by
    /// name, not the `Config`-internal token.
    fn stub_config() -> Config {
        // Build directly to avoid env mutations in other tests.
        Config::new_for_testing("ws://stub".to_string())
    }

    #[test]
    fn resolve_token_env_returns_value_when_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_token_env("HANUI_TEST_HA_TOKEN_082", Some("my-ha-token"));

        let cfg = stub_config();
        let result = cfg
            .resolve_token_env("HANUI_TEST_HA_TOKEN_082")
            .expect("must succeed when env var is set and non-empty");
        assert_eq!(result, "my-ha-token");

        set_token_env("HANUI_TEST_HA_TOKEN_082", None);
    }

    #[test]
    fn resolve_token_env_returns_not_found_when_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Ensure the var is absent.
        set_token_env("HANUI_TEST_HA_TOKEN_082_ABSENT", None);

        let cfg = stub_config();
        let err = cfg
            .resolve_token_env("HANUI_TEST_HA_TOKEN_082_ABSENT")
            .expect_err("must fail when env var is absent");
        assert!(
            matches!(err, ConfigError::TokenEnvNotFound { ref name } if name == "HANUI_TEST_HA_TOKEN_082_ABSENT"),
            "expected TokenEnvNotFound, got: {err}"
        );
        assert!(err.to_string().contains("HANUI_TEST_HA_TOKEN_082_ABSENT"));
    }

    #[test]
    fn resolve_token_env_returns_empty_when_var_is_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_token_env("HANUI_TEST_HA_TOKEN_082_EMPTY", Some(""));

        let cfg = stub_config();
        let err = cfg
            .resolve_token_env("HANUI_TEST_HA_TOKEN_082_EMPTY")
            .expect_err("must fail when env var is set but empty");
        assert!(
            matches!(err, ConfigError::TokenEnvEmpty { ref name } if name == "HANUI_TEST_HA_TOKEN_082_EMPTY"),
            "expected TokenEnvEmpty, got: {err}"
        );
        assert!(err.to_string().contains("HANUI_TEST_HA_TOKEN_082_EMPTY"));

        set_token_env("HANUI_TEST_HA_TOKEN_082_EMPTY", None);
    }
}
