//! TASK-063 / TASK-076 / TASK-101 integration tests — `Url` action handler.
//!
//! TASK-063 locked the per-mode behaviour:
//!
//! * `Always` → spawner is invoked once, outcome is `Opened`.
//! * `Never` → spawner is NOT invoked, outcome carries the
//!   "URL actions are disabled" toast text verbatim.
//! * `Ask` → spawner is NOT invoked, outcome carries the "Phase 6" toast
//!   text verbatim.
//!
//! TASK-101 / TASK-076 extend this surface with audit-row assertions: every
//! `xdg-open`-related call MUST emit an `audit::emit(AuditEvent { ... })`
//! row on the dedicated `"audit"` tracing target with the locked field
//! shape (`event` / `outcome` / `error_kind` / `scheme` — never `href`).
//! The capture path is a custom in-process tracing subscriber registered
//! via `tracing::subscriber::with_default` so the integration test does
//! not need a global subscriber install (and therefore does not collide
//! with parallel libtest threads).
//!
//! `security-engineer` review enforces:
//! * `href` is sourced from the `Action::Url` variant — never from live entity
//!   state. The match-and-extract pattern below is the structural proof.
//! * Shell metacharacters in a hostile `href` are rejected by validation
//!   regardless of mode.
//! * No audit row contains the `href` value (asserted as a substring scan
//!   per TASK-076 acceptance — the structural `&'static str`-only field
//!   shape is the primary control; this scan is defence in depth).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use hanui::actions::url::{
    handle_url_action_with_spawner, Spawner, UrlError, UrlOutcome, TOAST_ASK_PHASE_6,
    TOAST_BLOCKED_BY_PROFILE,
};
use hanui::actions::Action;
use hanui::dashboard::profiles::UrlActionMode;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

// ---------------------------------------------------------------------------
// Recording spawner — function-pointer compatible.
//
// `Spawner` is `fn(&str) -> io::Result<()>`, which cannot capture state. We
// communicate spawn count and the forced-failure flag through static atomics.
// Tests serialise access via `TEST_SERIAL` so concurrent libtest runs do not
// clobber each other's counters.
// ---------------------------------------------------------------------------

static SPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);
static SPAWN_FORCE_FAIL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn recording_spawner(_href: &str) -> std::io::Result<()> {
    SPAWN_COUNT.fetch_add(1, Ordering::SeqCst);
    if SPAWN_FORCE_FAIL.load(Ordering::SeqCst) {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "test forced spawn failure",
        ))
    } else {
        Ok(())
    }
}

fn reset_recorder(force_fail: bool) {
    SPAWN_COUNT.store(0, Ordering::SeqCst);
    SPAWN_FORCE_FAIL.store(force_fail, Ordering::SeqCst);
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
// Audit-row capture subscriber — TASK-101
//
// A minimal `tracing::Subscriber` that records every event whose target
// equals `"audit"` into an in-process `Vec<CapturedRow>`. Used via
// `tracing::subscriber::with_default(...)` so the subscriber is scoped
// to the closure under test — no global install, no cross-test bleed.
//
// The subscriber records each field's `Debug` rendering as a String. We
// rely on the `Debug` rendering for assertion purposes only; the
// production AuditEvent fields remain structurally `&'static str` /
// closed-enum values. The capture stringification happens inside the
// test harness, never in production code.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct CapturedRow {
    target: String,
    fields: std::collections::BTreeMap<String, String>,
}

#[derive(Default)]
struct CapturedField {
    fields: std::collections::BTreeMap<String, String>,
}

impl Visit for CapturedField {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }
}

struct AuditCaptureSubscriber {
    rows: Arc<Mutex<Vec<CapturedRow>>>,
}

impl Subscriber for AuditCaptureSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        // Only enable events on the `"audit"` target. Everything else is
        // dropped at the filter step, keeping captured rows uncluttered.
        metadata.target() == "audit"
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        // No span tracking in this subscriber — events fire at the root
        // level and we never enter a span. Return a fixed sentinel id;
        // the value is unused because we never call `enter`.
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = CapturedField::default();
        event.record(&mut visitor);
        let row = CapturedRow {
            target: event.metadata().target().to_owned(),
            fields: visitor.fields,
        };
        if let Ok(mut guard) = self.rows.lock() {
            guard.push(row);
        }
    }
}

/// Run `body` under the audit-capture subscriber and return every captured
/// `"audit"` target row.
fn with_audit_capture<F, R>(body: F) -> (R, Vec<CapturedRow>)
where
    F: FnOnce() -> R,
{
    let rows: Arc<Mutex<Vec<CapturedRow>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = AuditCaptureSubscriber { rows: rows.clone() };
    let result = tracing::subscriber::with_default(subscriber, body);
    let captured = rows.lock().expect("audit capture mutex poisoned").clone();
    (result, captured)
}

/// Assert that the supplied `href` substring does not appear in any field
/// of any captured row. The audit invariant is structural (the
/// `&'static str` field types prevent owned strings entirely), but a
/// substring scan here is defence in depth — it catches a future
/// regression that, for example, accidentally formats a `href` into
/// the `event` literal.
fn assert_no_href_in_rows(rows: &[CapturedRow], href: &str) {
    // Use a distinctive substring of the href. Pick a substring that is
    // unique to the href and not a generic token; for the integration
    // tests below we author hrefs that contain an ASCII path or query
    // marker we can probe (e.g. "/dashboard", "/blocked", "/ask",
    // "/spawn-fail-probe"). Caller passes a marker string.
    for row in rows {
        for (k, v) in &row.fields {
            assert!(
                !v.contains(href),
                "audit row field `{k}` contained the href substring `{href}`: {row:?}"
            );
        }
    }
}

/// Find the single audit row in `rows`. Panics if zero or more than one
/// row was captured — every test below emits exactly one row, so this
/// guard catches both "row missing" and "row duplicated" regressions.
fn single_row(rows: &[CapturedRow]) -> &CapturedRow {
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one audit row, got {}: {rows:?}",
        rows.len()
    );
    let row = &rows[0];
    assert_eq!(
        row.target, "audit",
        "captured row must be on the `audit` target"
    );
    row
}

// ---------------------------------------------------------------------------
// Always mode — spawner invoked once, outcome is Opened, audit row recorded
// ---------------------------------------------------------------------------

#[test]
fn always_mode_invokes_spawner_once_and_returns_opened() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_recorder(false);

    let action = Action::Url {
        href: "https://example.org/dashboard".to_owned(),
    };
    let href = href_from_url_action(action);

    let (outcome, rows) = with_audit_capture(|| {
        handle_url_action_with_spawner(&href, UrlActionMode::Always, SPAWNER)
            .expect("Always mode with a valid https href must succeed")
    });

    assert!(matches!(outcome, UrlOutcome::Opened));
    assert_eq!(
        spawn_count(),
        1,
        "Always mode must invoke the spawner exactly once for a valid href"
    );

    // TASK-076 audit-row assertions: spawned event with https scheme, no
    // error_kind, no href.
    let row = single_row(&rows);
    let f = &row.fields;
    assert_eq!(
        f.get("event").map(String::as_str),
        Some("url.xdg_open.spawn"),
        "Always-success row must have event=`url.xdg_open.spawn`"
    );
    assert_eq!(
        f.get("outcome").map(String::as_str),
        Some("spawned"),
        "Always-success row must have outcome=`spawned`"
    );
    // `error_kind` is `Option<&'static str>`; on success it is `None` and
    // the captured rendering is the literal string `None`.
    assert_eq!(
        f.get("error_kind").map(String::as_str),
        Some("None"),
        "Always-success row must have error_kind=`None`"
    );
    assert!(
        f.get("scheme")
            .map(|s| s.contains("Https"))
            .unwrap_or(false),
        "Always-success row must record AuditScheme::Https; got `scheme`={:?}",
        f.get("scheme")
    );
    assert!(
        f.contains_key("trace_id"),
        "every audit row must carry a trace_id field; got fields {f:?}"
    );

    // TASK-076 security gate: href must NEVER appear in any audit field.
    // Probe with the most distinctive part of the href.
    assert_no_href_in_rows(&rows, "/dashboard");
    assert_no_href_in_rows(&rows, "example.org");
}

// ---------------------------------------------------------------------------
// Always mode — spawn failure: audit row records spawn_failed + error_kind
// ---------------------------------------------------------------------------

#[test]
fn always_mode_spawn_failure_emits_spawn_failed_audit_row() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_recorder(true);

    let action = Action::Url {
        href: "https://example.org/spawn-fail-probe".to_owned(),
    };
    let href = href_from_url_action(action);

    let (result, rows) = with_audit_capture(|| {
        handle_url_action_with_spawner(&href, UrlActionMode::Always, SPAWNER)
    });

    let err = result.expect_err("forced spawn failure must surface as UrlError::Spawn");
    assert!(matches!(err, UrlError::Spawn(_)));
    assert_eq!(spawn_count(), 1);

    let row = single_row(&rows);
    let f = &row.fields;
    assert_eq!(
        f.get("event").map(String::as_str),
        Some("url.xdg_open.spawn"),
        "spawn-failure row must keep event=`url.xdg_open.spawn`"
    );
    assert_eq!(
        f.get("outcome").map(String::as_str),
        Some("spawn_failed"),
        "spawn-failure row must have outcome=`spawn_failed`"
    );
    assert!(
        f.get("error_kind")
            .map(|s| s.contains("NotFound"))
            .unwrap_or(false),
        "spawn-failure row must record the io::ErrorKind in error_kind; got {:?}",
        f.get("error_kind")
    );
    assert!(
        f.get("scheme")
            .map(|s| s.contains("Https"))
            .unwrap_or(false),
        "spawn-failure row must still record the validated scheme"
    );
    assert!(f.contains_key("trace_id"));

    // href is NEVER recorded — even on the failure path.
    assert_no_href_in_rows(&rows, "/spawn-fail-probe");
    assert_no_href_in_rows(&rows, "example.org");
}

// ---------------------------------------------------------------------------
// Never mode — no spawn, BlockedShowToast, audit row with blocked_by_profile
// ---------------------------------------------------------------------------

#[test]
fn never_mode_does_not_invoke_spawner_and_returns_blocked_toast() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_recorder(false);

    let action = Action::Url {
        href: "https://example.org/blocked".to_owned(),
    };
    let href = href_from_url_action(action);

    let (outcome, rows) = with_audit_capture(|| {
        handle_url_action_with_spawner(&href, UrlActionMode::Never, SPAWNER)
            .expect("Never mode does not error on a valid href; it returns a toast outcome")
    });

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

    let row = single_row(&rows);
    let f = &row.fields;
    assert_eq!(
        f.get("event").map(String::as_str),
        Some("url.blocked_by_profile"),
        "Never row must have event=`url.blocked_by_profile`"
    );
    assert_eq!(
        f.get("outcome").map(String::as_str),
        Some("blocked_by_profile"),
        "Never row must have outcome=`blocked_by_profile`"
    );
    assert_eq!(
        f.get("error_kind").map(String::as_str),
        Some("None"),
        "Never row carries no error_kind"
    );
    assert!(
        f.get("scheme")
            .map(|s| s.contains("Https"))
            .unwrap_or(false),
        "Never row must still record the validated scheme"
    );
    assert!(f.contains_key("trace_id"));

    assert_no_href_in_rows(&rows, "/blocked");
    assert_no_href_in_rows(&rows, "example.org");
}

// ---------------------------------------------------------------------------
// Ask mode — no spawn, AskShowToast, audit row with deferred_ask
// ---------------------------------------------------------------------------

#[test]
fn ask_mode_does_not_invoke_spawner_and_returns_phase_6_toast() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_recorder(false);

    let action = Action::Url {
        href: "https://example.org/ask".to_owned(),
    };
    let href = href_from_url_action(action);

    let (outcome, rows) = with_audit_capture(|| {
        handle_url_action_with_spawner(&href, UrlActionMode::Ask, SPAWNER)
            .expect("Ask mode does not error on a valid href; it returns a toast outcome")
    });

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

    let row = single_row(&rows);
    let f = &row.fields;
    assert_eq!(
        f.get("event").map(String::as_str),
        Some("url.deferred_ask"),
        "Ask row must have event=`url.deferred_ask`"
    );
    assert_eq!(
        f.get("outcome").map(String::as_str),
        Some("deferred_ask"),
        "Ask row must have outcome=`deferred_ask`"
    );
    assert_eq!(f.get("error_kind").map(String::as_str), Some("None"));
    assert!(
        f.get("scheme")
            .map(|s| s.contains("Https"))
            .unwrap_or(false),
        "Ask row must still record the validated scheme"
    );
    assert!(f.contains_key("trace_id"));

    assert_no_href_in_rows(&rows, "/ask");
    assert_no_href_in_rows(&rows, "example.org");
}

// ---------------------------------------------------------------------------
// File scheme audit row — covers the AuditScheme::File variant
// ---------------------------------------------------------------------------

#[test]
fn always_mode_file_scheme_emits_file_audit_scheme() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_recorder(false);

    let action = Action::Url {
        href: "file:///tmp/audit-probe.txt".to_owned(),
    };
    let href = href_from_url_action(action);

    let (outcome, rows) = with_audit_capture(|| {
        handle_url_action_with_spawner(&href, UrlActionMode::Always, SPAWNER)
            .expect("file:// is on the allowlist and must succeed")
    });
    assert!(matches!(outcome, UrlOutcome::Opened));

    let row = single_row(&rows);
    assert!(
        row.fields
            .get("scheme")
            .map(|s| s.contains("File"))
            .unwrap_or(false),
        "file:// href must record AuditScheme::File; got {:?}",
        row.fields.get("scheme")
    );

    // Defence in depth — the path must not appear anywhere.
    assert_no_href_in_rows(&rows, "audit-probe.txt");
    assert_no_href_in_rows(&rows, "/tmp/");
}

// ---------------------------------------------------------------------------
// Hostile href is rejected even in Always mode (security-engineer surface)
// ---------------------------------------------------------------------------

/// The classic shell-injection payload: a `;` plus a `rm -rf /`. The handler
/// MUST reject this before any spawn attempt. This test is the security
/// regression gate for the validator. The audit row must NOT fire on
/// validation rejection — the row records authorisation decisions and
/// shell-out outcomes, not validator rejections (which are caller errors,
/// not auditable boundary events under TASK-076's spec).
#[test]
fn hostile_href_with_semicolon_rejected_no_spawn_in_always_mode() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_recorder(false);

    let action = Action::Url {
        href: r#"https://example.org/"; rm -rf /"#.to_owned(),
    };
    let href = href_from_url_action(action);

    let (result, rows) = with_audit_capture(|| {
        handle_url_action_with_spawner(&href, UrlActionMode::Always, SPAWNER)
    });
    let err = result.expect_err("hostile href must be rejected before spawn");

    assert!(matches!(err, UrlError::InvalidHref { .. }));
    assert_eq!(
        spawn_count(),
        0,
        "hostile href must NOT reach the spawner; rejection is at validation time"
    );

    // No audit row on validator rejection — the rejection happens before
    // mode dispatch reaches the audit emit call sites.
    assert_eq!(
        rows.len(),
        0,
        "validator rejection must not fire an audit row; got {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Hostile href is rejected even in Never / Ask (defence in depth)
// ---------------------------------------------------------------------------

#[test]
fn hostile_href_with_pipe_rejected_in_never_mode() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_recorder(false);

    let action = Action::Url {
        href: "https://example.org/|cat".to_owned(),
    };
    let href = href_from_url_action(action);

    // Even though Never mode would not spawn, validation runs first so the
    // rejection happens before mode dispatch. This guards against a future
    // refactor that flips Never → Always silently.
    let (result, rows) =
        with_audit_capture(|| handle_url_action_with_spawner(&href, UrlActionMode::Never, SPAWNER));
    let err = result.expect_err("pipe in href must be rejected even in Never mode");
    assert!(matches!(err, UrlError::InvalidHref { .. }));
    assert_eq!(spawn_count(), 0);
    assert_eq!(
        rows.len(),
        0,
        "validator rejection must not fire an audit row"
    );
}

#[test]
fn file_traversal_href_rejected_in_always_mode() {
    let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    reset_recorder(false);

    let action = Action::Url {
        // Percent-encoded `..` to defeat the parser's normalisation. The
        // raw-string scan must catch this even after the parser would have
        // hidden the intent.
        href: "file:///opt/app/%2e%2e/etc/passwd".to_owned(),
    };
    let href = href_from_url_action(action);

    let (result, rows) = with_audit_capture(|| {
        handle_url_action_with_spawner(&href, UrlActionMode::Always, SPAWNER)
    });
    let err = result.expect_err("file:// path traversal must be rejected");
    assert!(matches!(err, UrlError::InvalidHref { .. }));
    assert_eq!(spawn_count(), 0);
    assert_eq!(
        rows.len(),
        0,
        "validator rejection must not fire an audit row"
    );
}

// ---------------------------------------------------------------------------
// TASK-076 / TASK-101 security cross-cut: href is NEVER in audit rows
//
// Aggregate scan across all three success-path modes with a single, distinctive
// href substring. If any future refactor accidentally formats `href` into an
// AuditEvent field via something like `event: leaked_href` (which would itself
// fail to compile thanks to the &'static str gate, but we belt-and-brace),
// the substring scan here surfaces it.
// ---------------------------------------------------------------------------

#[test]
fn href_not_in_audit_row_across_all_modes() {
    // A unique URL substring that no other test in this file uses, so any
    // contamination is unambiguously this test's fault.
    const PROBE: &str = "secret-probe-12345";
    let href = format!("https://example.org/{PROBE}");

    for mode in [
        UrlActionMode::Always,
        UrlActionMode::Never,
        UrlActionMode::Ask,
    ] {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_recorder(false);

        let (_outcome, rows) = with_audit_capture(|| {
            handle_url_action_with_spawner(&href, mode, SPAWNER)
                .expect("valid https href must succeed in all three modes")
        });

        assert_no_href_in_rows(&rows, PROBE);
        assert_no_href_in_rows(&rows, "example.org");
        // The URL parser normalises some hosts; also probe the full href.
        assert_no_href_in_rows(&rows, &href);
    }
}
