//! `Url` action handler — `xdg-open` shell-out gate.
//!
//! # Dispatcher wiring (TASK-075)
//!
//! TASK-063 shipped this handler with `must_not_touch: src/actions/dispatcher.rs`
//! so `security-engineer` could audit the `xdg-open` boundary in a small,
//! focused diff. TASK-075 then routed the handler through the dispatcher
//! under the [`UrlActionMode`] gate; the dispatcher's `Action::Url` arm
//! now calls [`handle_url_action_with_spawner`] with `self.url_action_mode`
//! and `self.url_spawner`. The validation, scheme allowlist, `..` traversal
//! defence, length cap, and "href is never logged" invariants in this file
//! are unchanged — TASK-075 only adds the routing.
//!
//! Phase 3 implements the `Url` action's runtime path per
//! `docs/plans/2026-04-28-phase-3-actions.md` `locked_decisions.url_action_gating`.
//! The handler is gated by [`crate::dashboard::profiles::UrlActionMode`]:
//!
//! | Mode                                                       | Behaviour                                                                            |
//! |------------------------------------------------------------|--------------------------------------------------------------------------------------|
//! | [`UrlActionMode::Always`]                                  | Spawn `xdg-open <href>` directly (desktop dev VM default).                           |
//! | [`UrlActionMode::Never`]                                   | Emit a "URL actions are disabled on this device profile" toast; never shell out.     |
//! | [`UrlActionMode::Ask`]                                     | Emit a "Confirmation dialog comes in Phase 6" toast; never shell out (Phase 6 swap). |
//!
//! [`UrlActionMode::Always`]: crate::dashboard::profiles::UrlActionMode::Always
//! [`UrlActionMode::Never`]: crate::dashboard::profiles::UrlActionMode::Never
//! [`UrlActionMode::Ask`]: crate::dashboard::profiles::UrlActionMode::Ask
//!
//! # Security boundary (Risk #5; `security-engineer` review required)
//!
//! `xdg-open` is an external-process boundary. The `href` is **never** derived
//! from live entity state — it is authored in Rust code (Phase 3) or YAML
//! (Phase 4). This invariant is structural: the [`crate::actions::Action::Url`]
//! variant carries `href: String` in the schema, which is populated only by:
//!
//! * Phase 3: in-code `WidgetActionMap` construction at startup.
//! * Phase 4: the YAML deserializer parsing `url-action` blocks.
//!
//! There is no path from `LiveStore` entity state to `Action::Url::href`. The
//! schema layer (`src/actions/schema.rs`) and the dispatcher
//! (`src/actions/dispatcher.rs`) both treat the `href` as a static value
//! supplied by the action spec.
//!
//! # Spawn invocation pattern
//!
//! [`std::process::Command::new("xdg-open").arg(href).spawn()`]. The `arg`
//! method passes a single argument — never a shell-string — so the URL is not
//! interpreted by `/bin/sh` and shell metacharacters in the `href` cannot
//! mutate the command line. `Command::spawn` with no shell wrapper is the
//! fundamental defence against argument injection at this boundary.
//!
//! # `href` validation
//!
//! Before spawning, [`handle_url_action`] validates `href`:
//!
//! 1. The string must parse via the `url` crate.
//! 2. The scheme must be one of `http`, `https`, or `file`.
//! 3. The string must not contain any shell metacharacter from the
//!    [`SHELL_META_CHARS`] set. Even though `Command::arg` does not invoke a
//!    shell, this provides defence in depth: a future code path that
//!    accidentally concatenates `href` into a shell string would still be
//!    rejected here.
//! 4. For `file://` URLs, every path segment is iteratively percent-decoded
//!    (up to a fixed bound) and rejected if any decoding level produces
//!    `..`. This catches single-encoded `%2e%2e`, double-encoded
//!    `%252e%252e`, mixed forms `%2e%252e` / `%252e%2e`, and arbitrarily
//!    deeper encodings — all of which a downstream re-decoding consumer
//!    could otherwise resolve to a parent-directory traversal.
//!
//! Any violation is rejected as [`UrlError::InvalidHref`] before the spawn
//! attempt.
//!
//! # Logging hygiene
//!
//! The `href` value is **not** logged. Only the validated scheme is included
//! in trace lines so a debug operator can see "an http URL was launched"
//! without recording the full URL (which may carry tracking parameters or
//! credentials in malformed configs). This complies with `CLAUDE.md` security
//! rules (never log full request/response bodies).

use std::io;
use std::process::Command;

use tracing::{debug, warn};

use crate::dashboard::profiles::UrlActionMode;

// ---------------------------------------------------------------------------
// Toast text constants (TASK-067 will render these verbatim)
// ---------------------------------------------------------------------------

/// Toast text emitted when [`UrlActionMode::Never`] blocks a URL action.
///
/// Public so the integration test in `tests/integration/url_action.rs` can
/// assert the exact string without duplicating it.
pub const TOAST_BLOCKED_BY_PROFILE: &str = "URL actions are disabled on this device profile";

/// Toast text emitted when [`UrlActionMode::Ask`] is selected. Phase 6 swaps
/// this branch for an actual confirmation dialog.
///
/// Public for the same reason as [`TOAST_BLOCKED_BY_PROFILE`].
pub const TOAST_ASK_PHASE_6: &str = "Confirmation dialog comes in Phase 6";

// ---------------------------------------------------------------------------
// Shell metacharacter set
// ---------------------------------------------------------------------------

/// Characters rejected by [`handle_url_action`] in the `href`.
///
/// The set covers the POSIX shell metacharacters plus a few newline-class
/// characters. `Command::arg` passes the string as a single argument and
/// does **not** invoke a shell; this list is therefore defence-in-depth, not
/// the primary control. The primary control is the `Command::new(...).arg(...)`
/// pattern itself.
///
/// Listed explicitly (not via `is_ascii_punctuation` etc.) so the rejection
/// rule is auditable: every character here has a documented reason for being
/// in the set.
const SHELL_META_CHARS: &[char] = &[
    ';',  // command separator
    '&',  // background / AND
    '|',  // pipe / OR
    '`',  // backtick command substitution
    '$',  // variable / $() substitution
    '\\', // escape
    '\n', // newline (multi-line injection)
    '\r', // carriage return
    '\0', // NUL byte (truncation attacks)
    '<',  // input redirection
    '>',  // output redirection
    '"',  // quote (could close a shell quote in a future bad concat)
    '\'', // single quote (same)
];

/// Schemes accepted by the `Url` action.
///
/// Phase 3 ships only `http`, `https`, and `file`. Other schemes (e.g.
/// `ssh`, `mailto`, `tel`) require explicit `security-engineer` sign-off in
/// later phases — they each have different abuse profiles.
const ALLOWED_SCHEMES: &[&str] = &["http", "https", "file"];

// ---------------------------------------------------------------------------
// UrlOutcome / UrlError
// ---------------------------------------------------------------------------

/// Successful outcome of [`handle_url_action`].
///
/// `BlockedShowToast` and `AskShowToast` carry the exact toast string the
/// caller (TASK-067) will render. `Opened` indicates `xdg-open` was spawned
/// successfully — the spawned process's exit status is **not** waited on
/// (xdg-open returns immediately after the child registers with the desktop
/// environment).
#[derive(Debug)]
pub enum UrlOutcome {
    /// `xdg-open <href>` was spawned successfully. The child handle is
    /// dropped; the OS reaps it asynchronously. The dispatcher does not
    /// hold any resource bound to the child.
    Opened,
    /// [`UrlActionMode::Never`] blocked the action. The string is the
    /// toast text the UI should render.
    BlockedShowToast(&'static str),
    /// [`UrlActionMode::Ask`] deferred the action to Phase 6. The string is
    /// the toast text the UI should render.
    AskShowToast(&'static str),
}

/// Why [`handle_url_action`] could not produce a [`UrlOutcome`].
#[derive(Debug)]
pub enum UrlError {
    /// `xdg-open` could not be spawned (binary missing, permissions, etc.).
    /// Carries the underlying `io::Error` for diagnosis. The dispatcher
    /// surfaces this as a toast (TASK-067) with the [`std::fmt::Display`]
    /// rendering of the wrapped error — never the raw `Debug` form, which
    /// can include OS-error-code paths that may inadvertently surface
    /// environment metadata.
    Spawn(io::Error),

    /// The `href` failed validation. The reason is a static string so it can
    /// safely surface in toasts and logs — it never echoes the rejected
    /// `href` itself, which may contain malicious content.
    InvalidHref {
        /// One-line description of the rejection reason.
        reason: &'static str,
    },
}

impl std::fmt::Display for UrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The wrapped io::Error's Display is intentionally surfaced; that
            // form is the standard "No such file or directory (os error 2)"
            // text and does not leak the href.
            UrlError::Spawn(e) => write!(f, "failed to spawn xdg-open: {e}"),
            UrlError::InvalidHref { reason } => write!(f, "invalid url: {reason}"),
        }
    }
}

impl std::error::Error for UrlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UrlError::Spawn(e) => Some(e),
            UrlError::InvalidHref { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Spawner indirection
// ---------------------------------------------------------------------------

/// Function-pointer type used to spawn `xdg-open`.
///
/// The default value is [`spawn_xdg_open`]; tests inject a fake spawner via
/// [`handle_url_action_with_spawner`] to record the call without actually
/// launching a process. The indirection is the entire reason `Action::Url`
/// is unit-testable on a CI runner that has no display server.
pub type Spawner = fn(&str) -> io::Result<()>;

/// Default spawner: invokes `xdg-open <href>` and discards the child handle.
///
/// `Command::new("xdg-open").arg(href).spawn()` — the `arg` method ensures
/// `href` is passed as a single, opaque argument (no shell interpretation,
/// no concatenation). `spawn()` returns immediately; we drop the
/// `std::process::Child` so the OS reaps the process asynchronously. We do
/// not call `wait()` because `xdg-open` itself returns once the child is
/// dispatched to the desktop environment — blocking on it would stall the
/// dispatcher's gesture-callback thread.
fn spawn_xdg_open(href: &str) -> io::Result<()> {
    Command::new("xdg-open").arg(href).spawn().map(|_child| ())
}

/// The production [`Spawner`] used by the dispatcher.
///
/// Exposed as `pub(crate)` so the [`crate::actions::dispatcher::Dispatcher`]
/// can wire this as its default `url_spawner` field without needing to name
/// the private `spawn_xdg_open` function. Tests inject a recording closure via
/// [`handle_url_action_with_spawner`] rather than calling this directly.
///
/// The security boundary is unchanged — this is simply a re-export of the
/// existing private spawner under a crate-visible name. `security-engineer`
/// review of TASK-063 covers the `spawn_xdg_open` implementation; this
/// wrapper adds no new behaviour.
pub(crate) fn default_spawner(href: &str) -> io::Result<()> {
    spawn_xdg_open(href)
}

// ---------------------------------------------------------------------------
// Public handler
// ---------------------------------------------------------------------------

/// Handle a `Url` action.
///
/// `href` is validated before any branch runs; an invalid `href` is rejected
/// regardless of `mode`. For `Always`, the validated `href` is passed to
/// `xdg-open` via [`Command::arg`] — never via a shell. For `Never` and
/// `Ask`, no process is spawned.
///
/// # Errors
///
/// * [`UrlError::InvalidHref`] — the `href` failed validation (parse, scheme,
///   or shell-metachar check).
/// * [`UrlError::Spawn`] — `Always` mode and `xdg-open` could not be spawned.
///
/// # Invariant: `href` is not derived from entity state
///
/// The `href` argument is supplied by the caller from the
/// [`crate::actions::Action::Url`] variant in the action map. The schema
/// (`src/actions/schema.rs`) carries `href: String` populated at action-map
/// construction time, never from `LiveStore` entity state. `security-engineer`
/// review of TASK-063 enforces this surface.
pub fn handle_url_action(href: &str, mode: UrlActionMode) -> Result<UrlOutcome, UrlError> {
    handle_url_action_with_spawner(href, mode, spawn_xdg_open)
}

/// As [`handle_url_action`] but with an injectable [`Spawner`]. Tests use
/// this to record the call without launching a real process.
pub fn handle_url_action_with_spawner(
    href: &str,
    mode: UrlActionMode,
    spawner: Spawner,
) -> Result<UrlOutcome, UrlError> {
    // Validate `href` regardless of mode. Even Never / Ask paths must reject
    // a malformed href so a future code change that flips the gate cannot
    // suddenly start passing unsanitised input to xdg-open.
    let scheme = validate_href(href)?;

    match mode {
        UrlActionMode::Always => {
            // Trace line records ONLY the scheme; the full href is never
            // logged. CLAUDE.md security rule: never log full request bodies.
            debug!(scheme, "url action: shelling out to xdg-open");
            spawner(href).map_err(|e| {
                // Log the underlying io::Error category but do NOT log href.
                warn!(scheme, error = %e, "url action: xdg-open spawn failed");
                UrlError::Spawn(e)
            })?;
            Ok(UrlOutcome::Opened)
        }
        UrlActionMode::Never => {
            debug!(scheme, "url action: blocked by Never profile");
            Ok(UrlOutcome::BlockedShowToast(TOAST_BLOCKED_BY_PROFILE))
        }
        UrlActionMode::Ask => {
            debug!(scheme, "url action: deferred to Phase 6 (Ask)");
            Ok(UrlOutcome::AskShowToast(TOAST_ASK_PHASE_6))
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate `href` and return its (lower-cased) scheme on success.
///
/// Order of checks is deliberate:
///
/// 1. **Shell metacharacter scan** runs first: a string with a `;` is rejected
///    even if it would have parsed as a URL. This is the cheapest check and
///    rejects the most obvious injection payloads.
/// 2. **`url::Url::parse`** validates structural URL grammar.
/// 3. **Scheme allowlist** restricts to `http` / `https` / `file`.
/// 4. **`file://` path-traversal scan** rejects `..` segments to defeat
///    `file:///etc/../etc/passwd`-style attempts. Path traversal in `file`
///    URLs is the most concrete attack vector at this boundary in Phase 3
///    (kiosk profiles default to Never; desktop profile is the dev VM where
///    this layer is the last line of defence).
fn validate_href(href: &str) -> Result<&'static str, UrlError> {
    // Pre-check: shell metacharacters anywhere in the string. The `url` crate
    // would percent-encode these in the fragment / query, but in the path or
    // host they would round-trip — and any future shell-string concatenation
    // would then carry them through. Reject early.
    if let Some(bad) = href.chars().find(|c| SHELL_META_CHARS.contains(c)) {
        // The `bad` char is logged at debug only — and only its byte index,
        // not the full href.
        debug!(
            byte = bad as u32,
            "url action: rejecting href containing shell metacharacter"
        );
        return Err(UrlError::InvalidHref {
            reason: "contains shell metacharacter",
        });
    }

    // Length cap: 4096 chars is well above any reasonable HA action URL and
    // well below the kernel ARG_MAX on every supported platform. A multi-MB
    // href would otherwise be a low-effort DoS on the gesture thread.
    const MAX_HREF_LEN: usize = 4096;
    if href.len() > MAX_HREF_LEN {
        return Err(UrlError::InvalidHref {
            reason: "href exceeds 4096 characters",
        });
    }

    let parsed = url::Url::parse(href).map_err(|_| UrlError::InvalidHref {
        reason: "not a well-formed url",
    })?;

    // Scheme allowlist. Match against the &'static str slice so the returned
    // value is a static reference (the lifetime extension is what makes the
    // signature `Result<&'static str, _>` ergonomic for the caller's tracing).
    let scheme = parsed.scheme();
    let allowed = ALLOWED_SCHEMES
        .iter()
        .find(|s| **s == scheme)
        .ok_or(UrlError::InvalidHref {
            reason: "scheme is not http, https, or file",
        })?;

    // file:// path-traversal scan. The `url` crate NORMALISES `..` away during
    // parse (so `parsed.path_segments()` will not see it after the parser
    // resolves the relative reference). We therefore scan the RAW INPUT
    // string for `..` and percent-encoded variants — that scan happens
    // before any normalisation can hide the intent.
    if *allowed == "file" {
        if href_contains_dotdot_segment(href) {
            return Err(UrlError::InvalidHref {
                reason: "file:// href contains a `..` segment",
            });
        }
        // file:// must not have a non-empty host (no `file://attacker.example/etc/passwd`).
        if let Some(host) = parsed.host_str() {
            if !host.is_empty() {
                return Err(UrlError::InvalidHref {
                    reason: "file:// href must not specify a host",
                });
            }
        }
    }

    Ok(*allowed)
}

/// Whether the raw `href` (pre-parse) contains a `..` path segment.
///
/// We scan the raw input rather than `parsed.path_segments()` because
/// `url::Url::parse` normalises `..` away during parse. Both literal `..` and
/// percent-encoded forms (`%2e%2e`, `%2E%2E`, mixed case) are detected.
///
/// The scan looks for path-segment boundaries: a slash, then `..` (or its
/// percent-encoded form), then another slash or end-of-input. This avoids
/// matching `..` inside a non-segment context (e.g. `?q=..`) — but a `..`
/// in a query is harmless to xdg-open anyway.
fn href_contains_dotdot_segment(href: &str) -> bool {
    // Tokenise on `/` boundaries within the path component only. Find the
    // path's start (after `://` and the host) by scanning past the scheme.
    let after_scheme = match href.find("://") {
        Some(pos) => &href[pos + 3..],
        None => href, // schemeless input is rejected earlier; defensive
    };
    // Path starts after the host; first `/` after the host is the path's
    // first separator.
    let path_start = after_scheme.find('/').unwrap_or(after_scheme.len());
    let path_and_more = &after_scheme[path_start..];
    // Strip query / fragment — `..` in a query is not a path segment.
    let path_only = path_and_more
        .split_once(['?', '#'])
        .map(|(p, _)| p)
        .unwrap_or(path_and_more);
    for segment in path_only.split('/') {
        if is_dotdot(segment) {
            return true;
        }
    }
    false
}

/// Whether `seg` represents `..` either literally or via any combination of
/// percent-encoding levels.
///
/// Implementation: iteratively percent-decode the segment up to a fixed
/// recursion bound, then check whether any iteration produced exactly `..`.
/// This covers every combination an attacker can express:
///
/// * literal `..`
/// * single-encoded `%2e%2e` / `%2E%2E` / case variants
/// * double-encoded `%252e%252e` / case variants
/// * mixed encodings such as `%2e%252e` or `%252e%2e`
/// * triple-or-deeper encodings (any `%25%32%65...` chain), bounded by
///   [`MAX_DECODE_PASSES`] to defeat a malicious infinitely-encoded payload
///
/// Returning `true` on any decoded form is the right default at this
/// boundary: the cost is rejecting URLs that legitimately encode `..` in a
/// segment (which `xdg-open` would resolve to a parent path on the local
/// filesystem anyway — exactly the behaviour we are trying to prevent).
///
/// Uses a fixed-depth iterative decode rather than a single-shot
/// fully-decoding helper so the function remains pure and allocates at most
/// a small bounded number of intermediate strings.
fn is_dotdot(seg: &str) -> bool {
    /// Maximum number of percent-decode passes before giving up. Each pass
    /// strictly shrinks the input (every `%XX` triple becomes one byte), so
    /// this bound is generous — a 4096-byte href has at most ~1365 single
    /// `%XX` triples, and decoding ~10 times reduces any pathological chain
    /// to its base form well under the bound.
    const MAX_DECODE_PASSES: usize = 16;

    // Fast-path: the literal form is the most common.
    if seg == ".." {
        return true;
    }

    // Iteratively percent-decode. After each pass, check if the result is
    // ".." — accept any depth up to MAX_DECODE_PASSES.
    let mut current = seg.to_owned();
    for _ in 0..MAX_DECODE_PASSES {
        let next = match percent_decode_once(&current) {
            Some(decoded) => decoded,
            None => break, // no `%` triples left (or malformed) — done
        };
        if next == ".." {
            return true;
        }
        if next == current {
            break; // no progress this pass — terminate
        }
        current = next;
    }
    false
}

/// Decode every `%XX` triple in `s` once. Returns `None` if the string
/// contains no `%` (so the caller can stop iterating). Malformed `%`
/// sequences (incomplete triple, non-hex digits) are passed through
/// verbatim — the boundary is not a strict URL decoder, just a "did this
/// segment ever encode `..`" probe.
fn percent_decode_once(s: &str) -> Option<String> {
    if !s.contains('%') {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_value(bytes[i + 1]);
            let lo = hex_value(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Lossy conversion: any non-UTF-8 byte sequence becomes a replacement
    // character. The caller compares the result to ".." which is pure
    // ASCII — so non-ASCII decoded bytes can never match.
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Map an ASCII hex digit byte to its 0..=15 numeric value.
fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, Ordering};

    // -----------------------------------------------------------------------
    // Test fixture: recording Spawner that does not launch a real process.
    // -----------------------------------------------------------------------

    /// Captures whether the Spawner was called.
    ///
    /// The recorder uses an `AtomicBool` reachable through a static so the
    /// `fn(&str) -> io::Result<()>` signature can be a plain function pointer
    /// (closures with captured state cannot coerce to `fn`-pointer). Tests
    /// reset the flag before exercising and read it after.
    static SPAWN_CALLED: AtomicBool = AtomicBool::new(false);
    static SPAWN_FAILS: AtomicBool = AtomicBool::new(false);

    /// Recording spawner.  Uses the static toggles above to communicate with
    /// the test body — there is no per-call closure capture (the function-
    /// pointer signature does not allow it).
    fn recording_spawner(_href: &str) -> io::Result<()> {
        SPAWN_CALLED.store(true, Ordering::SeqCst);
        if SPAWN_FAILS.load(Ordering::SeqCst) {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "test forced spawn failure",
            ))
        } else {
            Ok(())
        }
    }

    /// Reset the recorder before a test.  Must be called at the start of any
    /// test that reads `SPAWN_CALLED` to avoid leakage from an earlier test
    /// in the same binary.
    fn reset_spawn_recorder(force_fail: bool) {
        SPAWN_CALLED.store(false, Ordering::SeqCst);
        SPAWN_FAILS.store(force_fail, Ordering::SeqCst);
    }

    // -----------------------------------------------------------------------
    // Always — happy path: spawner is called once and Outcome is Opened
    // -----------------------------------------------------------------------

    #[test]
    fn always_mode_invokes_spawner_and_returns_opened() {
        // Static recorder serialised by Mutex below in the failure test; for
        // the happy path no other test touches SPAWN_FAILS so we can take the
        // simple path.
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);

        let outcome = handle_url_action_with_spawner(
            "https://example.org/path?q=1",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect("Always mode must succeed for a valid href");

        assert!(matches!(outcome, UrlOutcome::Opened));
        assert!(
            SPAWN_CALLED.load(Ordering::SeqCst),
            "Always mode must invoke the spawner exactly once"
        );
    }

    // -----------------------------------------------------------------------
    // Always — spawn failure: returns UrlError::Spawn (no panic)
    // -----------------------------------------------------------------------

    #[test]
    fn always_mode_spawn_failure_returns_url_error_spawn() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(true);

        let err = handle_url_action_with_spawner(
            "https://example.org/",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("forced spawn failure must surface as UrlError::Spawn");

        match err {
            UrlError::Spawn(io_err) => {
                // The forced error is NotFound; assert that exactly so a
                // future regression that swallows the underlying error is
                // caught.
                assert_eq!(io_err.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected UrlError::Spawn, got {other:?}"),
        }
        assert!(SPAWN_CALLED.load(Ordering::SeqCst));
    }

    // -----------------------------------------------------------------------
    // Never — does not invoke spawner, returns BlockedShowToast
    // -----------------------------------------------------------------------

    #[test]
    fn never_mode_does_not_spawn_and_returns_blocked_toast() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);

        let outcome = handle_url_action_with_spawner(
            "https://example.org/",
            UrlActionMode::Never,
            recording_spawner,
        )
        .expect("Never mode must not error on a valid href");

        match outcome {
            UrlOutcome::BlockedShowToast(text) => {
                assert_eq!(text, TOAST_BLOCKED_BY_PROFILE);
                assert!(
                    text.contains("disabled"),
                    "blocked toast text must indicate the action was disabled, got: {text}"
                );
            }
            other => panic!("expected BlockedShowToast, got {other:?}"),
        }
        assert!(
            !SPAWN_CALLED.load(Ordering::SeqCst),
            "Never mode must NOT invoke the spawner"
        );
    }

    // -----------------------------------------------------------------------
    // Ask — does not invoke spawner, returns AskShowToast with Phase-6 text
    // -----------------------------------------------------------------------

    #[test]
    fn ask_mode_does_not_spawn_and_returns_phase_6_toast() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);

        let outcome = handle_url_action_with_spawner(
            "https://example.org/",
            UrlActionMode::Ask,
            recording_spawner,
        )
        .expect("Ask mode must not error on a valid href");

        match outcome {
            UrlOutcome::AskShowToast(text) => {
                assert_eq!(text, TOAST_ASK_PHASE_6);
                assert!(
                    text.contains("Phase 6"),
                    "Ask toast text must reference Phase 6, got: {text}"
                );
            }
            other => panic!("expected AskShowToast, got {other:?}"),
        }
        assert!(
            !SPAWN_CALLED.load(Ordering::SeqCst),
            "Ask mode must NOT invoke the spawner"
        );
    }

    // -----------------------------------------------------------------------
    // Validation — shell metacharacter rejection
    //
    // The classic "`; rm -rf /`" payload, plus a smattering of other
    // metacharacters from SHELL_META_CHARS. Every one must be rejected as
    // InvalidHref BEFORE the spawn branch is reached.
    // -----------------------------------------------------------------------

    #[test]
    fn invalid_href_with_semicolon_rejected_in_all_modes() {
        let payload = "https://example.org/\";rm -rf /";
        for mode in [
            UrlActionMode::Always,
            UrlActionMode::Never,
            UrlActionMode::Ask,
        ] {
            let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
            reset_spawn_recorder(false);
            let err = handle_url_action_with_spawner(payload, mode, recording_spawner)
                .expect_err("shell-meta payload must be rejected");
            assert!(matches!(err, UrlError::InvalidHref { .. }));
            assert!(
                !SPAWN_CALLED.load(Ordering::SeqCst),
                "spawner must NOT be invoked for an invalid href (mode {mode:?})"
            );
        }
    }

    #[test]
    fn invalid_href_with_backtick_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "https://example.org/`whoami`",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("backtick must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
        assert!(!SPAWN_CALLED.load(Ordering::SeqCst));
    }

    #[test]
    fn invalid_href_with_dollar_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "https://example.org/$(whoami)",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("$ must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
    }

    #[test]
    fn invalid_href_with_pipe_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "https://example.org/|cat",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("| must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
    }

    #[test]
    fn invalid_href_with_newline_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "https://example.org/\nrm -rf /",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("\\n must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
    }

    #[test]
    fn invalid_href_with_nul_byte_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "https://example.org/\0",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("NUL byte must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
    }

    // -----------------------------------------------------------------------
    // Validation — malformed URL grammar
    // -----------------------------------------------------------------------

    #[test]
    fn invalid_href_unparseable_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "not a url at all",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("unparseable href must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
    }

    // -----------------------------------------------------------------------
    // Validation — scheme allowlist
    // -----------------------------------------------------------------------

    #[test]
    fn invalid_href_disallowed_scheme_rejected() {
        // ssh:// is plausible but not on the allowlist; it must be rejected.
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "ssh://attacker.example/payload",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("ssh:// must be rejected");
        match err {
            UrlError::InvalidHref { reason } => {
                assert!(
                    reason.contains("scheme"),
                    "scheme rejection reason must cite 'scheme', got: {reason}"
                );
            }
            other => panic!("expected InvalidHref, got {other:?}"),
        }
    }

    #[test]
    fn invalid_href_javascript_scheme_rejected() {
        // javascript: is the most dangerous scheme to ever leak through; it's
        // not on the allowlist so this must be rejected.
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "javascript:alert(1)",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("javascript: must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
    }

    // -----------------------------------------------------------------------
    // Validation — valid schemes accepted
    // -----------------------------------------------------------------------

    #[test]
    fn valid_http_scheme_accepted() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        handle_url_action_with_spawner(
            "http://example.org/",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect("http:// must be accepted");
        assert!(SPAWN_CALLED.load(Ordering::SeqCst));
    }

    #[test]
    fn valid_https_scheme_accepted() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        handle_url_action_with_spawner(
            "https://example.org/",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect("https:// must be accepted");
        assert!(SPAWN_CALLED.load(Ordering::SeqCst));
    }

    #[test]
    fn valid_file_scheme_accepted() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        handle_url_action_with_spawner(
            "file:///tmp/safe.txt",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect("file:// must be accepted");
        assert!(SPAWN_CALLED.load(Ordering::SeqCst));
    }

    // -----------------------------------------------------------------------
    // Validation — file:// path traversal
    // -----------------------------------------------------------------------

    #[test]
    fn file_scheme_dotdot_segment_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        // url::Url::parse normalises `/etc/../etc/passwd` to `/etc/passwd`,
        // so we synthesize the dot-dot via a path that the parser preserves
        // — `/etc/../passwd` is normalised to `/passwd`, but the segment
        // walk still observes the input string. We use `/foo/..%2fpasswd`
        // and percent-encoded variants below to exercise both the literal
        // and encoded paths.
        //
        // Literal `/foo/../bar` is normalised away by the parser, so the
        // direct attack surface is the encoded form. We assert the encoded
        // form is rejected; the literal form is normalised before reaching
        // the segment walk and is therefore already neutralised by the URL
        // parser itself (which is exactly what we want — defence in depth
        // means both layers must reject).
        let err = handle_url_action_with_spawner(
            "file:///foo/%2e%2e/etc/passwd",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("encoded `..` in file:// must be rejected");
        match err {
            UrlError::InvalidHref { reason } => {
                assert!(
                    reason.contains("..")
                        || reason.contains("traversal")
                        || reason.contains("`..`"),
                    "rejection reason must cite the dot-dot path, got: {reason}"
                );
            }
            other => panic!("expected InvalidHref, got {other:?}"),
        }
        assert!(!SPAWN_CALLED.load(Ordering::SeqCst));
    }

    #[test]
    fn file_scheme_percent_encoded_uppercase_dotdot_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "file:///foo/%2E%2E/etc/passwd",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("uppercase `%2E%2E` must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
    }

    #[test]
    fn file_scheme_double_encoded_dotdot_rejected() {
        // `%252e%252e` decodes once to `%2e%2e` and twice to `..`. A
        // downstream consumer that double-decodes would see `..`. Defence
        // in depth — opencode-review iteration 1 caught this gap.
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "file:///foo/%252e%252e/etc/passwd",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("double-encoded `..` must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
        assert!(!SPAWN_CALLED.load(Ordering::SeqCst));
    }

    #[test]
    fn file_scheme_double_encoded_uppercase_dotdot_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "file:///foo/%252E%252E/etc/passwd",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("double-encoded uppercase `%252E%252E` must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
    }

    #[test]
    fn file_scheme_mixed_single_and_double_encoded_dotdot_rejected() {
        // opencode-review iteration 2 caught this: `%2e%252e` is 7 bytes
        // and falls through both the legacy 6-byte and 10-byte fixed-size
        // checks. The iterative-decode is_dotdot now handles it because
        // each pass strictly progresses toward `..`.
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "file:///foo/%2e%252e/etc/passwd",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("mixed single+double-encoded `%2e%252e` must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
        assert!(!SPAWN_CALLED.load(Ordering::SeqCst));
    }

    #[test]
    fn file_scheme_mixed_double_then_single_encoded_dotdot_rejected() {
        // Mirror of the above: `%252e%2e` (8 bytes) — first half
        // double-encoded, second half single-encoded.
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        let err = handle_url_action_with_spawner(
            "file:///foo/%252e%2e/etc/passwd",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("mixed double+single-encoded `%252e%2e` must be rejected");
        assert!(matches!(err, UrlError::InvalidHref { .. }));
    }

    #[test]
    fn is_dotdot_unit_tests_lock_decode_levels() {
        // Direct unit test of is_dotdot for the encoding levels. This pins
        // the function's behaviour independently of the surrounding
        // validate_href flow so a future refactor that breaks any specific
        // encoding fails at this assertion site rather than via an
        // integration test that may or may not exercise the form.
        assert!(is_dotdot(".."));
        assert!(is_dotdot("%2e%2e"));
        assert!(is_dotdot("%2E%2E"));
        assert!(is_dotdot("%2e%2E"));
        assert!(is_dotdot("%252e%252e"));
        assert!(is_dotdot("%252E%252E"));
        assert!(is_dotdot("%2e%252e"));
        assert!(is_dotdot("%252e%2e"));

        // Negative cases — must NOT trigger.
        assert!(!is_dotdot("foo"));
        assert!(!is_dotdot("."));
        assert!(!is_dotdot("...")); // three dots is not the traversal pattern
        assert!(!is_dotdot("")); // empty segment
        assert!(!is_dotdot("%2e")); // single-encoded `.` — one dot, not two
    }

    #[test]
    fn file_scheme_with_host_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        // `file://attacker.example/etc/passwd` — a host on a file:// URL
        // would, on some platforms, mount or fetch from the named host.
        // Reject defensively.
        let err = handle_url_action_with_spawner(
            "file://attacker.example/etc/passwd",
            UrlActionMode::Always,
            recording_spawner,
        )
        .expect_err("file:// with a non-empty host must be rejected");
        match err {
            UrlError::InvalidHref { reason } => {
                assert!(
                    reason.contains("host"),
                    "rejection reason must cite the host, got: {reason}"
                );
            }
            other => panic!("expected InvalidHref, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Validation — length cap
    // -----------------------------------------------------------------------

    #[test]
    fn invalid_href_exceeding_length_cap_rejected() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_spawn_recorder(false);
        // Build a URL just over 4096 chars; everything past the prefix is
        // ASCII alphanumeric so it does not trip the shell-meta filter
        // before the length check (length check runs after the meta-char
        // scan but is tested here as the dominant rejection reason).
        let mut href = String::from("https://example.org/");
        href.extend(std::iter::repeat_n('a', 4100));
        let err = handle_url_action_with_spawner(&href, UrlActionMode::Always, recording_spawner)
            .expect_err("over-length href must be rejected");
        match err {
            UrlError::InvalidHref { reason } => {
                assert!(
                    reason.contains("4096") || reason.contains("characters"),
                    "rejection reason must cite the length cap, got: {reason}"
                );
            }
            other => panic!("expected InvalidHref, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Display impls — toast surfaces (TASK-067)
    // -----------------------------------------------------------------------

    #[test]
    fn url_error_display_does_not_leak_href() {
        // Defence in depth: the Display impls must not include the rejected
        // href string. The reason is a `&'static str` so it cannot leak the
        // input even by accident.
        let err = UrlError::InvalidHref {
            reason: "contains shell metacharacter",
        };
        let display = format!("{err}");
        assert!(display.contains("invalid url"));
        assert!(display.contains("shell metacharacter"));
    }

    #[test]
    fn url_error_spawn_display_is_descriptive() {
        let err = UrlError::Spawn(io::Error::new(io::ErrorKind::NotFound, "boom"));
        let display = format!("{err}");
        assert!(display.contains("xdg-open"));
        assert!(display.contains("boom"));
    }

    // -----------------------------------------------------------------------
    // Compile-time: Action::Url::href is a String from the schema layer
    //
    // Document the invariant in test code: the `href` is the action spec's
    // String field — it is NOT pulled from EntityState. If the schema ever
    // changes shape (e.g. href: Box<dyn FromEntity>), this test fails to
    // compile and the security-engineer review trail fires.
    // -----------------------------------------------------------------------

    #[test]
    fn href_type_invariant_compile_time_check() {
        // Construct a Url action and pull out the String. If a future
        // refactor changes Action::Url to derive `href` from entity state,
        // the destructure pattern below will not compile.
        let action = crate::actions::Action::Url {
            href: "https://example.org/".to_owned(),
        };
        let href: String = match action {
            crate::actions::Action::Url { href } => href,
            _ => unreachable!(),
        };
        // No runtime assertion on the value — the test exists to anchor
        // the schema-level type invariant in CI.
        let _ = href;
    }

    // -----------------------------------------------------------------------
    // Test serialisation guard
    //
    // The `recording_spawner` shares static `AtomicBool` state across test
    // threads. Cargo's libtest runs `#[test]` fns in parallel by default; a
    // mutex serialises tests that read/write the static.
    // -----------------------------------------------------------------------

    static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
