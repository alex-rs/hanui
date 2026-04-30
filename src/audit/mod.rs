//! Audit substrate (Phase 6 6.0 prerequisite — TASK-101).
//!
//! Top-level peer of [`crate::ha`], [`crate::actions`], [`crate::dashboard`].
//! Deliberately NOT placed under `src/platform/` per the locked decision in
//! `docs/plans/2026-04-30-phase-6-advanced-widgets.md`
//! (`locked_decisions.audit_substrate_placement`): the audit surface is not
//! platform-coupled — it touches no DPMS, device-tree, or systemd code path.
//! `src/platform/` is reserved for genuinely platform-specific concerns
//! (CLI, health socket, blanking, device-profile autodetect).
//!
//! # The single security invariant
//!
//! **No audit field may carry a user-supplied string.** Every field on
//! [`AuditEvent`] is one of:
//!
//! * `&'static str` (compile-time literal — cannot reflect user input);
//! * `Option<&'static str>` (same, optional);
//! * [`AuditScheme`] (closed three-variant enum — `Http`, `Https`, `File`);
//! * [`TraceId`] (`u64` newtype, assigned internally by [`emit`]).
//!
//! There is **no** `String`, no `&str` (open lifetime), and no
//! open-vocabulary type. A future PR that adds a `String` field to
//! `AuditEvent` will fail to compile — see the structural-invariant gate
//! `field_gate::ASSERT_AUDIT_EVENT_FIELDS_ARE_AUDIT_FIELDS` and the
//! [`AuditField`] sealed trait.
//!
//! This invariant exists because audit rows are written via the same
//! `tracing` substrate the rest of the app uses for debug logging. A loose
//! `String` field here would be the path by which a hostile URL, token,
//! HA entity payload, or otherwise sensitive value leaks into log storage —
//! exactly the class of bug `CLAUDE.md` § "Security rules" forbids.
//!
//! # Why the closed [`AuditScheme`] enum
//!
//! `Option<&'static str>` for the URL scheme would technically satisfy the
//! invariant above, but it leaves the door open for a future caller to
//! construct `AuditEvent { scheme: Some("javascript"), .. }` from a string
//! literal. The URL validator in `src/actions/url.rs` already rejects
//! non-`http`/`https`/`file` URLs before reaching the audit emit call site,
//! but the type system now provides a second enforcement layer: the closed
//! enum makes adding a fourth scheme a compile-forced explicit decision
//! (no `#[non_exhaustive]` — adding a variant is a deliberate review step,
//! not a silent extension).
//!
//! # Why a local atomic counter for [`TraceId`]
//!
//! Per `locked_decisions.trace_id_source`: `tracing::span::Id` is only
//! meaningful inside an active span context. Many audit emit call sites
//! (e.g., `xdg-open` in `src/actions/url.rs`) are reached from contexts
//! without an active span (gesture-callback thread). A local atomic
//! counter gives every emit a unique, monotonically-increasing ID
//! regardless of span context, with no allocation and no contention beyond
//! the single `fetch_add`.
//!
//! Callers do **not** supply a `TraceId`; [`emit`] calls [`TraceId::next`]
//! internally before writing the row. This removes one whole class of
//! caller error (forgetting to advance the counter, sharing IDs across
//! threads, etc.).
//!
//! # Pre-subscriber silent-drop behaviour
//!
//! Calling [`emit`] before `tracing_subscriber::fmt::init()` runs is a
//! **silent drop** — no panic, no error. Rationale: `tracing::event!`
//! with no subscriber is a documented no-op of the `tracing` crate;
//! panicking here would make every component test that constructs an
//! action-handler before wiring up a subscriber fail, which is a
//! widespread pattern in this codebase (see `src/actions/url.rs` test
//! module for one example among many). The silent-drop behaviour is
//! asserted by [`tests::emit_with_no_subscriber_does_not_panic`] so a
//! future change of intent is caught at PR time.
//!
//! # Tracing transport
//!
//! Every [`emit`] call resolves to:
//! ```ignore
//! tracing::event!(
//!     target: "audit",
//!     tracing::Level::INFO,
//!     event = event.event,
//!     outcome = event.outcome,
//!     error_kind = ?event.error_kind,
//!     scheme = ?event.scheme,
//!     trace_id = %trace_id,
//! );
//! ```
//! The `target: "audit"` literal is greppable
//! (`git grep -F 'target: "audit"' -- src/audit/mod.rs`) — operators
//! filter via `RUST_LOG="audit=info"` or a tracing-subscriber filter on
//! the target string. When Phase 5 lands JSON-formatted tracing for
//! systemd-journal, the audit rows will appear in that JSON stream
//! automatically; no I/O path or appender is added here.
//!
//! # Known limitation: durability is subscriber-defined
//!
//! `tracing::event!` is fire-and-forget: durability, ordering vs. other
//! log streams, and survival across process exit are all properties of
//! the subscriber backend (currently `tracing-subscriber` with the
//! default writer). On unclean shutdown, in-flight rows can be lost; on
//! heavy load, rows can be reordered relative to non-audit logs from
//! other subsystems. This is acceptable for Phase 6 6.0's TASK-076
//! scope (the `xdg-open` shell-out boundary): every audit row's
//! information is also derivable from the `Spawner`'s `io::Result`
//! return path, so a lost row is recoverable from the dispatcher's
//! own state. **Reliable durability + total ordering** is on the
//! Phase 5 work list (`docs/PHASES.md` Phase 5 — JSON-formatted
//! tracing for systemd-journal, with `journald`'s native persistence
//! contract). Higher-trust audit consumers (e.g., authentication
//! decisions, secret unwrap) MUST wait for that Phase 5 substrate
//! before adoption.
//!
//! # Public API
//!
//! ```ignore
//! use hanui::audit::{self, AuditEvent, AuditScheme};
//!
//! audit::emit(AuditEvent {
//!     event: "url.xdg_open.spawn",
//!     outcome: "spawned",
//!     error_kind: None,
//!     scheme: Some(AuditScheme::Https),
//! });
//! ```
//!
//! No builder, no trait, no macro in Phase 6. The struct-literal call site
//! is intentionally simple — every field is named so the reviewer at the
//! call site reads exactly what gets written.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

// ---------------------------------------------------------------------------
// AuditScheme — closed three-variant enum (Http / Https / File)
// ---------------------------------------------------------------------------

/// URL scheme attached to a URL-related audit row.
///
/// **Closed** by intent — no `#[non_exhaustive]`. Adding a fourth variant
/// must be a deliberate code change reviewed under the audit-substrate's
/// `security-engineer` ownership; the compile-forced shape is the control
/// that prevents a drive-by extension to (e.g.) `javascript:` or `data:`
/// schemes via a string literal.
///
/// The variants intentionally do not carry user data; the [`Display`] impl
/// emits exactly the lower-case scheme literal `"http"`, `"https"`, or
/// `"file"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditScheme {
    /// `http://` — plaintext HTTP.
    Http,
    /// `https://` — TLS-protected HTTP.
    Https,
    /// `file://` — local-filesystem URL.
    File,
}

impl std::fmt::Display for AuditScheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AuditScheme::Http => "http",
            AuditScheme::Https => "https",
            AuditScheme::File => "file",
        })
    }
}

// ---------------------------------------------------------------------------
// TraceId — opaque u64 newtype assigned by audit::emit
// ---------------------------------------------------------------------------

/// Local audit-event sequence counter.
///
/// Initialised to 1 (so `TraceId(0)` is reserved as a sentinel for
/// uninitialised data, should any future caller need one). `Ordering::Relaxed`
/// is sufficient — there is no other shared state we need to publish in step
/// with the counter, only "every emit gets a unique value".
static AUDIT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Opaque audit-row trace identifier.
///
/// Newtype around `u64`; constructed only by [`TraceId::next`], which is
/// `pub(crate)` so callers cannot mint their own (every emit's id is
/// assigned by [`emit`]). `Display` renders the underlying `u64` in
/// decimal — that is the form written to tracing output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TraceId(u64);

impl TraceId {
    /// Allocate the next unique trace id.
    ///
    /// Crate-private: the only call site is [`emit`]. External callers must
    /// not synthesise their own trace ids — that would defeat the
    /// monotonic-uniqueness guarantee callers rely on for audit-row
    /// correlation.
    pub(crate) fn next() -> Self {
        TraceId(AUDIT_SEQ.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// AuditEvent — caller-supplied input to emit()
// ---------------------------------------------------------------------------

/// One audit row's caller-supplied payload.
///
/// **Every field's type is locked.** See module documentation for the
/// security invariant; structural enforcement lives in the
/// [`AuditField`] sealed trait and the static gate
/// `field_gate::ASSERT_AUDIT_EVENT_FIELDS_ARE_AUDIT_FIELDS` — a
/// non-conforming field added in a future PR fails to compile.
///
/// `trace_id` is **not** a field here. [`emit`] assigns one internally per
/// `locked_decisions.trace_id_source` so callers cannot accidentally share
/// or omit ids.
#[derive(Debug, Clone, Copy)]
pub struct AuditEvent {
    /// Static event kind, e.g. `"url.xdg_open.spawn"`. Greppable.
    pub event: &'static str,
    /// Static outcome label, e.g. `"spawned"`, `"spawn_failed"`,
    /// `"blocked_by_profile"`, `"deferred_ask"`.
    pub outcome: &'static str,
    /// On failure outcomes: a static string naming the error kind
    /// (e.g. `<io::ErrorKind as &'static str>` from
    /// [`std::io::ErrorKind`]'s `Debug`-like view). `None` on success.
    pub error_kind: Option<&'static str>,
    /// URL scheme for URL-related rows (`None` for non-URL events).
    pub scheme: Option<AuditScheme>,
}

// ---------------------------------------------------------------------------
// Sealed trait — structural enforcement of the security invariant
// ---------------------------------------------------------------------------

/// Sealed trait: only the four whitelisted audit-field types implement it.
///
/// A future PR that adds a field to [`AuditEvent`] of any other type will
/// fail the static gate
/// `field_gate::ASSERT_AUDIT_EVENT_FIELDS_ARE_AUDIT_FIELDS` at compile
/// time, because the field cannot satisfy [`AuditField`].
///
/// The trait lives in a private module so external crates (and future code
/// in this crate) cannot extend the whitelist without editing this file —
/// which is exactly the review-trail surface `security-engineer` audits.
///
/// **Always-on** (not `cfg(test)`): the sealed trait + the `field_gate`
/// static together form the structural seal that prevents a future
/// contributor from adding `message: String` (or any other non-whitelisted
/// type) to `AuditEvent`. Gating only on `cfg(test)` would mean the seal
/// is enforced only when CI runs `cargo test`; that is too thin a control.
/// The release-build anchor lives at the bottom of [`emit`], which calls
/// the gate static once per audit row. The gate body is no-op at runtime
/// (every `assert_is_audit_field` call has an empty body) and the
/// optimiser inlines the call away in release builds — the runtime cost
/// is zero.
mod sealed {
    use super::{AuditScheme, TraceId};

    /// Sealed marker. The trait is `pub` to the parent module only.
    pub trait AuditField {}

    impl AuditField for &'static str {}
    impl AuditField for Option<&'static str> {}
    impl AuditField for AuditScheme {}
    impl AuditField for Option<AuditScheme> {}
    impl AuditField for TraceId {}
}

use sealed::AuditField;

// The structural-invariant gate (the `ASSERT_AUDIT_EVENT_FIELDS_*` static +
// `assert_is_audit_field`) lives inside `mod field_gate` below. The gate is
// **always-on** (not `cfg(test)`) and is anchored as "used" by a one-liner
// call inside `emit`. The closure body is no-op at runtime (every
// `assert_is_audit_field` call has an empty body) — the optimiser inlines
// the call away in release builds, so the runtime cost is zero while the
// trait + sealed-impls remain visible in every build.
//
// Why always-on rather than `cfg(test)`: the structural seal is a
// security-engineer-owned control. A `cfg(test)` gate would mean the seal
// is enforced only when CI runs `cargo test`; out-of-CI builds (e.g. a
// developer's `cargo build` ahead of running tests, or `cargo run` in a
// development VM) would still compile a hostile `AuditEvent` extension.
// The always-on gate forces the seal to fire in **every** build, which is
// what the security review actually wants.
//
// We could have used the dead-code-allow attribute on a free function instead, but
// that attribute would need a waiver entry per the project's
// forbidden-token list. The always-on module + `emit`-call anchor is the
// cleaner pattern.
mod field_gate {
    use super::{AuditEvent, AuditField, AuditScheme};

    /// Trait-bound assertion helper: monomorphises only when `T: AuditField`.
    /// Invoked only by the closure-static below; the body is empty so the
    /// optimiser inlines the call away in release builds.
    pub(super) fn assert_is_audit_field<T: AuditField>(_: &T) {}

    /// Compile-time + always-on gate: every field of `AuditEvent` satisfies
    /// `AuditField`.
    ///
    /// `pub(super) static` so [`crate::audit::emit`] can reference it, which
    /// makes the helper + trait visible as "used" to rustc's dead-code lint
    /// in every build (test and release). The closure body destructures
    /// `AuditEvent` exhaustively (the `..` rest pattern is **deliberately
    /// not** used) — adding a new field that the destructure does not name
    /// is a separate compile error caught here.
    pub(super) static ASSERT_AUDIT_EVENT_FIELDS_ARE_AUDIT_FIELDS: fn(AuditEvent) =
        |e: AuditEvent| {
            let AuditEvent {
                event,
                outcome,
                error_kind,
                scheme,
            } = e;
            assert_is_audit_field::<&'static str>(&event);
            assert_is_audit_field::<&'static str>(&outcome);
            assert_is_audit_field::<Option<&'static str>>(&error_kind);
            assert_is_audit_field::<Option<AuditScheme>>(&scheme);
        };
}

// ---------------------------------------------------------------------------
// emit — sole public API
// ---------------------------------------------------------------------------

/// Emit one audit row.
///
/// Allocates a fresh [`TraceId`] and writes a `tracing::event!` at
/// `INFO` level on the dedicated `"audit"` target. The `tracing` crate's
/// documented contract makes this a no-op when no subscriber is
/// installed — see module docs for the silent-drop rationale.
///
/// This is the **only** public API of `crate::audit`. There is no
/// builder, no trait, no macro. Callers construct an [`AuditEvent`]
/// literal and pass it by value.
pub fn emit(event: AuditEvent) {
    // Anchor the structural-invariant gate as "used" so the sealed
    // `AuditField` trait is enforced in every build (not only `cargo
    // test`). The closure body is no-op at runtime and the optimiser
    // inlines this call away in release; the security value is in the
    // compile-time bound on `assert_is_audit_field`. `AuditEvent` is
    // `Copy`, so passing it here costs nothing.
    field_gate::ASSERT_AUDIT_EVENT_FIELDS_ARE_AUDIT_FIELDS(event);

    let trace_id = TraceId::next();
    tracing::event!(
        target: "audit",
        tracing::Level::INFO,
        event = event.event,
        outcome = event.outcome,
        error_kind = ?event.error_kind,
        scheme = ?event.scheme,
        trace_id = %trace_id,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `AuditScheme` must have exactly three variants. A drive-by addition
    /// of a fourth (e.g. `Ssh`, `Mailto`) trips the exhaustive `match`
    /// below — exactly the compile-forced explicit-decision behaviour
    /// `locked_decisions.audit_substrate_placement` requires (no
    /// `#[non_exhaustive]`).
    #[test]
    fn audit_scheme_has_exactly_three_variants() {
        // Iterate every constructed variant; the `match` below must be
        // exhaustive without a wildcard arm. Adding a fourth variant
        // without updating this test fails to compile.
        for v in [AuditScheme::Http, AuditScheme::Https, AuditScheme::File] {
            // `Display` round-trip: prove the implementation is wired and
            // emits the locked lower-case string for each variant.
            let s = format!("{v}");
            match v {
                AuditScheme::Http => assert_eq!(s, "http"),
                AuditScheme::Https => assert_eq!(s, "https"),
                AuditScheme::File => assert_eq!(s, "file"),
            }
        }
    }

    /// Serde JSON round-trip for `AuditScheme` — confirms the `Serialize`
    /// impl emits the locked lower-case string. Phase 5 may consume this
    /// path for JSON-formatted audit rows.
    #[test]
    fn audit_scheme_serializes_as_lowercase_string() {
        assert_eq!(
            serde_json::to_string(&AuditScheme::Http).unwrap(),
            "\"http\""
        );
        assert_eq!(
            serde_json::to_string(&AuditScheme::Https).unwrap(),
            "\"https\""
        );
        assert_eq!(
            serde_json::to_string(&AuditScheme::File).unwrap(),
            "\"file\""
        );
    }

    /// Every [`AuditEvent`] field type must satisfy [`AuditField`]. The
    /// gate is the compile-time `static`
    /// `field_gate::ASSERT_AUDIT_EVENT_FIELDS_ARE_AUDIT_FIELDS`; a
    /// runtime call here invokes the closure stored in that `static` and
    /// verifies the gate is wired (proving the destructure is valid for
    /// the current `AuditEvent` shape). A future addition of `String`
    /// (or any non-whitelisted type) to `AuditEvent` fails to compile
    /// at the gate, and this test fails as a knock-on signal.
    #[test]
    fn audit_event_fields_are_static_str_or_traceid() {
        let e = AuditEvent {
            event: "test.event",
            outcome: "ok",
            error_kind: None,
            scheme: None,
        };
        (super::field_gate::ASSERT_AUDIT_EVENT_FIELDS_ARE_AUDIT_FIELDS)(e);
    }

    /// `emit` with no subscriber installed must silently drop, not panic.
    /// `tracing::event!` is a documented no-op when no subscriber is
    /// active; this test pins the behaviour as a regression gate per
    /// `locked_decisions.audit_substrate_placement`.
    ///
    /// We do NOT install any subscriber here — neither
    /// `tracing_test::traced_test` (which would defeat the test's
    /// purpose) nor `with_default` (same reason). We rely on the libtest
    /// harness: in-process tests that have not installed a subscriber
    /// observe the global no-op default that `tracing` ships with.
    ///
    /// Note: any *other* test in the same binary that calls
    /// `tracing_subscriber::fmt::init()` (without `try_init`) would
    /// install a global subscriber and shadow this assertion. None of
    /// the `src/audit/` unit tests do so; integration tests run in
    /// separate binaries.
    #[test]
    fn emit_with_no_subscriber_does_not_panic() {
        emit(AuditEvent {
            event: "audit.test.no_subscriber",
            outcome: "ok",
            error_kind: None,
            scheme: None,
        });
        // Reaching this line without panicking is the assertion.
    }

    /// `TraceId::next` returns strictly-increasing values across
    /// successive calls. Relaxed ordering is enough for monotonicity on a
    /// single counter (the AtomicU64 itself is total-ordered).
    #[test]
    fn trace_id_monotonically_increases() {
        let a = TraceId::next();
        let b = TraceId::next();
        let c = TraceId::next();
        assert!(
            a.0 < b.0 && b.0 < c.0,
            "trace ids must strictly increase: got {} {} {}",
            a.0,
            b.0,
            c.0
        );
    }

    /// `TraceId::Display` renders the underlying `u64` in decimal.
    #[test]
    fn trace_id_display_is_decimal_u64() {
        let id = TraceId(42);
        assert_eq!(format!("{id}"), "42");
    }

    // -----------------------------------------------------------------
    // Custom in-process subscriber for emit() field-shape coverage.
    //
    // We deliberately do NOT use `tracing_test::traced_test` here:
    // tracing-test installs a default subscriber whose `EnvFilter` is set
    // to the crate name (`hanui=trace`). `EnvFilter`'s directive matches
    // an event's *target*, and `audit::emit` deliberately uses a custom
    // target literal `"audit"` (not `"hanui::audit"`) so operators can
    // filter via `RUST_LOG="audit=info"` without coupling to module
    // paths. The two filters are mutually exclusive — tracing-test would
    // drop every audit row before assertion.
    //
    // The capture subscriber below is the same shape as the one in
    // `tests/integration/url_action.rs`; we keep one local copy here so
    // the unit-test binary stays self-contained (no shared test helper
    // crate needed).
    // -----------------------------------------------------------------

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Default)]
    struct CapturedFields(BTreeMap<String, String>);

    impl Visit for CapturedFields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
    }

    struct AuditCapture {
        rows: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    }

    impl Subscriber for AuditCapture {
        fn enabled(&self, m: &Metadata<'_>) -> bool {
            m.target() == "audit"
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut v = CapturedFields::default();
            event.record(&mut v);
            if let Ok(mut g) = self.rows.lock() {
                g.push(v.0);
            }
        }
    }

    fn capture<F: FnOnce()>(body: F) -> Vec<BTreeMap<String, String>> {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let sub = AuditCapture { rows: rows.clone() };
        tracing::subscriber::with_default(sub, body);
        let guard = rows.lock().expect("audit-capture mutex poisoned");
        guard.clone()
    }

    /// `emit` writes a row at `INFO` level on the `"audit"` target with
    /// exactly the fields supplied. Includes the `trace_id` field
    /// (proving `emit` allocated one) and the `event`/`outcome`/`scheme`
    /// literals round-trip.
    ///
    /// This is the production-path coverage for `emit` itself; the
    /// `xdg-open` consumer's behaviour is asserted in
    /// `tests/integration/url_action.rs`.
    #[test]
    fn emit_writes_audit_target_row_with_supplied_fields() {
        let rows = capture(|| {
            emit(AuditEvent {
                event: "url.xdg_open.spawn",
                outcome: "spawned",
                error_kind: None,
                scheme: Some(AuditScheme::Https),
            });
        });

        assert_eq!(rows.len(), 1, "exactly one audit row expected: {rows:?}");
        let f = &rows[0];
        assert_eq!(
            f.get("event").map(String::as_str),
            Some("url.xdg_open.spawn"),
            "event literal must round-trip"
        );
        assert_eq!(
            f.get("outcome").map(String::as_str),
            Some("spawned"),
            "outcome literal must round-trip"
        );
        // `error_kind` is `Option<&'static str>`; on success it is `None`
        // and the captured Debug rendering is the literal string `None`.
        assert_eq!(
            f.get("error_kind").map(String::as_str),
            Some("None"),
            "error_kind on success must render as `None`"
        );
        assert!(
            f.get("scheme")
                .map(|s| s.contains("Https"))
                .unwrap_or(false),
            "scheme must record AuditScheme::Https; got {:?}",
            f.get("scheme")
        );
        assert!(
            f.contains_key("trace_id"),
            "every audit row must carry a trace_id field; got {f:?}"
        );
    }

    /// Negative coverage: when an `error_kind` is supplied, the captured
    /// row contains the error-kind literal and the `File` scheme variant.
    /// Pairs with the success-row test above to lock both branches of the
    /// `Option<&'static str>` field and a second `AuditScheme` variant.
    #[test]
    fn emit_writes_error_kind_when_supplied() {
        let rows = capture(|| {
            emit(AuditEvent {
                event: "url.xdg_open.spawn",
                outcome: "spawn_failed",
                error_kind: Some("NotFound"),
                scheme: Some(AuditScheme::File),
            });
        });

        assert_eq!(rows.len(), 1);
        let f = &rows[0];
        assert_eq!(f.get("outcome").map(String::as_str), Some("spawn_failed"));
        assert!(
            f.get("error_kind")
                .map(|s| s.contains("NotFound"))
                .unwrap_or(false),
            "error_kind must record NotFound; got {:?}",
            f.get("error_kind")
        );
        assert!(
            f.get("scheme").map(|s| s.contains("File")).unwrap_or(false),
            "scheme must record AuditScheme::File; got {:?}",
            f.get("scheme")
        );
    }
}
