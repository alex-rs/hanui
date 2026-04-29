//! TASK-063 integration tests — `Url` action handler under all three
//! `UrlActionMode` branches.
//!
//! These tests exercise the public seam: a typed
//! [`hanui::actions::Action::Url`] action paired with a
//! [`hanui::dashboard::profiles::UrlActionMode`] is fed through
//! [`hanui::actions::handle_url_action`], asserting the per-mode behaviour
//! locked by `docs/plans/2026-04-28-phase-3-actions.md`
//! `locked_decisions.url_action_gating`:
//!
//! * `Always` → spawner is invoked once, outcome is `Opened`.
//! * `Never` → spawner is NOT invoked, outcome carries the
//!   "URL actions are disabled" toast text verbatim.
//! * `Ask` → spawner is NOT invoked, outcome carries the "Phase 6" toast
//!   text verbatim.
//!
//! `security-engineer` review of TASK-063 enforces:
//! * `href` is sourced from the `Action::Url` variant — never from live entity
//!   state. The match-and-extract pattern below is the structural proof.
//! * Shell metacharacters in a hostile `href` are rejected by validation
//!   regardless of mode.

use std::sync::atomic::{AtomicUsize, Ordering};

use hanui::actions::url::{
    handle_url_action_with_spawner, Spawner, UrlError, UrlOutcome, TOAST_ASK_PHASE_6,
    TOAST_BLOCKED_BY_PROFILE,
};
use hanui::actions::Action;
use hanui::dashboard::profiles::UrlActionMode;

// ---------------------------------------------------------------------------
// Recording spawner — function-pointer compatible.
//
// `Spawner` is `fn(&str) -> io::Result<()>`, which cannot capture state. We
// communicate spawn count through a static `AtomicUsize`. Tests serialise
// access via `TEST_SERIAL` so concurrent libtest runs do not clobber each
// other's counters.
// ---------------------------------------------------------------------------

static SPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);
static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn recording_spawner(_href: &str) -> std::io::Result<()> {
    SPAWN_COUNT.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

fn reset_counter() {
    SPAWN_COUNT.store(0, Ordering::SeqCst);
}

fn spawn_count() -> usize {
    SPAWN_COUNT.load(Ordering::SeqCst)
}

const SPAWNER: Spawner = recording_spawner;

/// Pull the `href` out of an `Action::Url` variant.
///
/// Documents the structural invariant: the `href` is supplied by the action
/// spec (in-code Phase 3, YAML Phase 4) — there is no `LiveStore` access in
/// this path. If a future refactor changes `Action::Url` to derive `href`
/// from entity state, the destructure will fail to compile and this test
/// breaks deterministically.
fn href_from_url_action(action: Action) -> String {
    match action {
        Action::Url { href } => href,
        _ => panic!("expected Action::Url"),
    }
}

// ---------------------------------------------------------------------------
// Always mode — spawner invoked once, outcome is Opened
// ---------------------------------------------------------------------------

#[test]
fn always_mode_invokes_spawner_once_and_returns_opened() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_counter();

    let action = Action::Url {
        href: "https://example.org/dashboard".to_owned(),
    };
    let href = href_from_url_action(action);

    let outcome = handle_url_action_with_spawner(&href, UrlActionMode::Always, SPAWNER)
        .expect("Always mode with a valid https href must succeed");

    assert!(matches!(outcome, UrlOutcome::Opened));
    assert_eq!(
        spawn_count(),
        1,
        "Always mode must invoke the spawner exactly once for a valid href"
    );
}

// ---------------------------------------------------------------------------
// Never mode — no spawner invocation, BlockedShowToast text verbatim
// ---------------------------------------------------------------------------

#[test]
fn never_mode_does_not_invoke_spawner_and_returns_blocked_toast() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_counter();

    let action = Action::Url {
        href: "https://example.org/blocked".to_owned(),
    };
    let href = href_from_url_action(action);

    let outcome = handle_url_action_with_spawner(&href, UrlActionMode::Never, SPAWNER)
        .expect("Never mode does not error on a valid href; it returns a toast outcome");

    match outcome {
        UrlOutcome::BlockedShowToast(text) => {
            assert_eq!(
                text, TOAST_BLOCKED_BY_PROFILE,
                "Never mode toast text must be the canonical constant"
            );
            assert_eq!(text, "URL actions are disabled on this device profile");
        }
        other => panic!("expected BlockedShowToast, got {other:?}"),
    }

    assert_eq!(
        spawn_count(),
        0,
        "Never mode must NOT invoke the spawner under any condition"
    );
}

// ---------------------------------------------------------------------------
// Ask mode — no spawner invocation, AskShowToast Phase-6 text verbatim
// ---------------------------------------------------------------------------

#[test]
fn ask_mode_does_not_invoke_spawner_and_returns_phase_6_toast() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_counter();

    let action = Action::Url {
        href: "https://example.org/ask".to_owned(),
    };
    let href = href_from_url_action(action);

    let outcome = handle_url_action_with_spawner(&href, UrlActionMode::Ask, SPAWNER)
        .expect("Ask mode does not error on a valid href; it returns a toast outcome");

    match outcome {
        UrlOutcome::AskShowToast(text) => {
            assert_eq!(
                text, TOAST_ASK_PHASE_6,
                "Ask mode toast text must be the canonical constant"
            );
            assert_eq!(text, "Confirmation dialog comes in Phase 6");
        }
        other => panic!("expected AskShowToast, got {other:?}"),
    }

    assert_eq!(
        spawn_count(),
        0,
        "Ask mode must NOT invoke the spawner; Phase 6 swaps the handler in for a real confirmation"
    );
}

// ---------------------------------------------------------------------------
// Hostile href is rejected even in Always mode (security-engineer surface)
// ---------------------------------------------------------------------------

/// The classic shell-injection payload: a `;` plus a `rm -rf /`. The handler
/// MUST reject this before any spawn attempt. This test is the security
/// regression gate for the validator.
#[test]
fn hostile_href_with_semicolon_rejected_no_spawn_in_always_mode() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_counter();

    // Use the structural Action::Url path again — the malicious href is
    // simulated as if it had landed in the schema (e.g. via a future YAML
    // typo or a confused config). The handler must still reject.
    let action = Action::Url {
        href: r#"https://example.org/"; rm -rf /"#.to_owned(),
    };
    let href = href_from_url_action(action);

    let err = handle_url_action_with_spawner(&href, UrlActionMode::Always, SPAWNER)
        .expect_err("hostile href must be rejected before spawn");

    assert!(matches!(err, UrlError::InvalidHref { .. }));
    assert_eq!(
        spawn_count(),
        0,
        "hostile href must NOT reach the spawner; rejection is at validation time"
    );
}

// ---------------------------------------------------------------------------
// Hostile href is rejected even in Never / Ask (defence in depth)
// ---------------------------------------------------------------------------

#[test]
fn hostile_href_with_pipe_rejected_in_never_mode() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_counter();

    let action = Action::Url {
        href: "https://example.org/|cat".to_owned(),
    };
    let href = href_from_url_action(action);

    // Even though Never mode would not spawn, validation runs first so the
    // rejection happens before mode dispatch. This guards against a future
    // refactor that flips Never → Always silently.
    let err = handle_url_action_with_spawner(&href, UrlActionMode::Never, SPAWNER)
        .expect_err("pipe in href must be rejected even in Never mode");
    assert!(matches!(err, UrlError::InvalidHref { .. }));
    assert_eq!(spawn_count(), 0);
}

#[test]
fn file_traversal_href_rejected_in_always_mode() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_counter();

    let action = Action::Url {
        // Percent-encoded `..` to defeat the parser's normalisation. The
        // raw-string scan must catch this even after the parser would have
        // hidden the intent.
        href: "file:///opt/app/%2e%2e/etc/passwd".to_owned(),
    };
    let href = href_from_url_action(action);

    let err = handle_url_action_with_spawner(&href, UrlActionMode::Always, SPAWNER)
        .expect_err("file:// path traversal must be rejected");
    assert!(matches!(err, UrlError::InvalidHref { .. }));
    assert_eq!(spawn_count(), 0);
}
