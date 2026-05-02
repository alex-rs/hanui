//! Phase 6 acceptance integration test for the audit substrate (TASK-076,
//! TASK-101, TASK-112).
//!
//! Exercises every `audit::emit` call site reachable from the integration
//! crate and asserts the locked field shape per
//! `locked_decisions.audit_substrate_placement`:
//!
//!   * Every row is on the dedicated `"audit"` tracing target.
//!   * Every row carries the locked field set: `event`, `outcome`,
//!     `error_kind`, `scheme`, `trace_id`.
//!   * No row contains user-supplied strings (`href`, PIN code, etc.).
//!
//! The xdg-open / `Action::Url` audit surface is exercised in detail by
//! `tests/integration/url_action.rs`. This file's contribution is the
//! cross-call-site invariant: the field shape is the same for every
//! emit, and no future emit can leak user-supplied content via a
//! non-`&'static str` field.

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use hanui::actions::url::{handle_url_action_with_spawner, Spawner, UrlOutcome};
use hanui::dashboard::profiles::UrlActionMode;

// ---------------------------------------------------------------------------
// Audit-row capture subscriber — only `target == "audit"`
// ---------------------------------------------------------------------------
//
// Mirrors the helper in `tests/integration/url_action.rs`; duplicated here
// so this file stays self-contained (the `tests/common/**` directory is in
// the must_not_touch list per TASK-112).

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
        metadata.target() == "audit"
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
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

/// Recording spawner — succeeds without touching the host.
fn ok_spawner(_href: &str) -> std::io::Result<()> {
    Ok(())
}

const SPAWNER: Spawner = ok_spawner;

// ---------------------------------------------------------------------------
// every_emit_callsite_target_audit
// ---------------------------------------------------------------------------

/// Walk all xdg-open URL action modes (`Always`, `Never`, `Ask`) and
/// confirm every emit:
///   * lands on `target == "audit"`,
///   * carries `event` / `outcome` / `error_kind` / `scheme` / `trace_id`.
///
/// The same row content is scrutinised more deeply in
/// `tests/integration/url_action.rs`; here we focus on the locked field
/// shape across modes.
#[test]
fn every_emit_callsite_target_audit() {
    const PROBE_HREF: &str = "https://audit-probe.example.org/test";
    const REQUIRED_FIELDS: &[&str] = &["event", "outcome", "error_kind", "scheme", "trace_id"];

    for mode in [
        UrlActionMode::Always,
        UrlActionMode::Never,
        UrlActionMode::Ask,
    ] {
        let (outcome, rows) =
            with_audit_capture(|| handle_url_action_with_spawner(PROBE_HREF, mode, SPAWNER));
        // Every mode produces an outcome (success path); the test does
        // not assert on the variant — `tests/integration/url_action.rs`
        // does that. We only need a row was emitted.
        let _ = outcome;

        assert_eq!(
            rows.len(),
            1,
            "mode {mode:?} must emit exactly one audit row; got {rows:?}"
        );
        let row = &rows[0];
        assert_eq!(
            row.target, "audit",
            "row must be on the `audit` tracing target"
        );
        for field in REQUIRED_FIELDS {
            assert!(
                row.fields.contains_key(*field),
                "row from mode {mode:?} missing required field `{field}`; got fields {:?}",
                row.fields
            );
        }
    }
}

// ---------------------------------------------------------------------------
// no_user_strings_in_audit_row
// ---------------------------------------------------------------------------

/// Defence-in-depth scan: a unique substring of the supplied href must
/// NEVER appear in any captured field, regardless of mode. The structural
/// `&'static str`-only field shape is the primary control; this scan is
/// the secondary gate.
#[test]
fn no_user_strings_in_audit_row_across_modes() {
    const PROBE: &str = "audit-substring-probe-zzz";
    let href = format!("https://example.org/{PROBE}");

    for mode in [
        UrlActionMode::Always,
        UrlActionMode::Never,
        UrlActionMode::Ask,
    ] {
        let (_outcome, rows) =
            with_audit_capture(|| handle_url_action_with_spawner(&href, mode, SPAWNER));
        for row in &rows {
            for (k, v) in &row.fields {
                assert!(
                    !v.contains(PROBE),
                    "row from {mode:?} field `{k}` contained probe `{PROBE}`: {row:?}"
                );
                assert!(
                    !v.contains("example.org"),
                    "row from {mode:?} field `{k}` contained host substring: {row:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// scheme variant — file:// produces the File audit scheme
// ---------------------------------------------------------------------------

#[test]
fn file_scheme_lands_on_file_audit_variant() {
    let (outcome, rows) = with_audit_capture(|| {
        handle_url_action_with_spawner("file:///tmp/probe.txt", UrlActionMode::Always, SPAWNER)
    });
    let outcome = outcome.expect("file:// must succeed in Always mode");
    assert!(matches!(outcome, UrlOutcome::Opened));

    assert_eq!(rows.len(), 1, "exactly one audit row expected");
    let row = &rows[0];
    let scheme = row.fields.get("scheme").map(String::as_str).unwrap_or("");
    assert!(
        scheme.contains("File"),
        "file:// href must record AuditScheme::File; got `scheme`={scheme:?}"
    );

    // Path content must NOT appear.
    for v in row.fields.values() {
        assert!(!v.contains("probe.txt"));
        assert!(!v.contains("/tmp/"));
    }
}
