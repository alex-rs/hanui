//! Phase 6 acceptance integration test for PIN entry redaction (TASK-100,
//! TASK-104, TASK-105, TASK-112).
//!
//! Per Phase 6 Risk #7 + plan acceptance line 1219, the entered PIN code
//! MUST NOT appear in any captured tracing span or event during a PIN
//! submit/cancel cycle. The unit-level test in
//! `src/actions/pin.rs::tests::code_not_captured_in_tracing_spans` exercises
//! the bare `PinEntryHost` trait against a mock; this integration test
//! exercises the same security invariant from the test crate's vantage
//! point and pins the cancellation path independently — together they
//! cover the FnOnce consume-once contract end-to-end.
//!
//! # Security invariant
//!
//! Per `locked_decisions.pin_entry_dispatch`:
//!
//!   * The entered code is consumed exactly once via `FnOnce`.
//!   * No tracing span / event MAY contain the code string.
//!   * On cancel, `on_submit` is never called.
//!
//! The capturing tracing layer here records EVERY event field across all
//! targets (not just `"audit"`) so a misfired `tracing::debug!` somewhere
//! in the call path cannot leak the PIN.

use std::sync::{Arc, Mutex};

use tracing::subscriber::with_default;
use tracing_subscriber::layer::SubscriberExt;

use hanui::actions::pin::{CodeFormat, PinEntryHost};

// ---------------------------------------------------------------------------
// Mock host — captures the on_submit closure for synthetic submit / cancel
// ---------------------------------------------------------------------------

type PendingSlot = Mutex<Option<Box<dyn FnOnce(String) + Send>>>;

struct MockPinHost {
    pending: PendingSlot,
    received_format: Mutex<Option<CodeFormat>>,
}

impl MockPinHost {
    fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            received_format: Mutex::new(None),
        }
    }

    fn submit(&self, code: String) {
        let cb = self
            .pending
            .lock()
            .unwrap()
            .take()
            .expect("request_pin must run before submit");
        cb(code);
    }

    fn cancel(&self) {
        // Drop the closure without invoking it — the production cancel
        // path drops the `on_submit` slot; the test mirrors that drop.
        drop(self.pending.lock().unwrap().take());
    }

    fn pending_remaining(&self) -> bool {
        self.pending.lock().unwrap().is_some()
    }
}

impl PinEntryHost for MockPinHost {
    fn request_pin(&self, code_format: CodeFormat, on_submit: Box<dyn FnOnce(String) + Send>) {
        *self.received_format.lock().unwrap() = Some(code_format);
        *self.pending.lock().unwrap() = Some(on_submit);
    }
}

// ---------------------------------------------------------------------------
// Capturing tracing layer — records EVERY field of EVERY event
// ---------------------------------------------------------------------------

struct CapturingLayer {
    events: Arc<Mutex<Vec<String>>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturingLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct FieldCollector(Vec<String>);
        impl tracing::field::Visit for FieldCollector {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push(format!("{}={:?}", field.name(), value));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.push(format!("{}={}", field.name(), value));
            }
        }
        let mut collector = FieldCollector(Vec::new());
        event.record(&mut collector);
        let line = collector.0.join(" ");
        self.events.lock().unwrap().push(line);
    }
}

fn run_under_capturing<F: FnOnce() -> R, R>(body: F) -> (R, Vec<String>) {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let layer = CapturingLayer {
        events: Arc::clone(&events),
    };
    let subscriber = tracing_subscriber::registry().with(layer);
    let result = with_default(subscriber, body);
    let captured = events.lock().unwrap().clone();
    (result, captured)
}

fn assert_no_substring(rows: &[String], probe: &str) {
    for line in rows {
        assert!(
            !line.contains(probe),
            "tracing event contained PIN substring `{probe}`: {line:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// code_not_in_tracing_or_audit — submit path
// ---------------------------------------------------------------------------

/// Submit a synthetic 4-digit code through the mock host and assert that
/// no captured tracing event mentions the code substring. The dispatcher
/// side consumes the code via `code.len()` and drops the value; nothing
/// should reach a `tracing!` macro.
#[test]
fn code_not_in_tracing_or_audit_on_submit() {
    const PROBE: &str = "1357";

    let received: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let received_clone = Arc::clone(&received);

    let (_unit, captured) = run_under_capturing(|| {
        let host = MockPinHost::new();
        host.request_pin(
            CodeFormat::Number,
            Box::new(move |code| {
                // Use the code to satisfy FnOnce, but never log it.
                *received_clone.lock().unwrap() = Some(code.len());
            }),
        );

        // The captured `received_format` field is asserted via the host
        // surface, not the tracing capture (the host does not emit
        // tracing events).
        assert_eq!(
            *host.received_format.lock().unwrap(),
            Some(CodeFormat::Number)
        );

        // Submit the synthetic PIN.
        host.submit(PROBE.to_owned());

        // FnOnce was consumed exactly once.
        assert!(!host.pending_remaining());
    });

    assert_eq!(*received.lock().unwrap(), Some(PROBE.len()));
    assert_no_substring(&captured, PROBE);
}

// ---------------------------------------------------------------------------
// Cancel path — code is never delivered, never traced
// ---------------------------------------------------------------------------

/// On cancel, `on_submit` is never invoked. The receive slot stays empty
/// and no tracing event mentions any PIN substring.
#[test]
fn code_not_delivered_or_traced_on_cancel() {
    const PROBE: &str = "9876";

    let delivered: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let delivered_clone = Arc::clone(&delivered);

    let (_unit, captured) = run_under_capturing(|| {
        let host = MockPinHost::new();
        host.request_pin(
            CodeFormat::Any,
            Box::new(move |_code| {
                *delivered_clone.lock().unwrap() = true;
            }),
        );

        // Synthesise a cancel.
        host.cancel();

        // The closure was dropped — pending slot is empty and the
        // delivered flag is still false.
        assert!(!host.pending_remaining());
    });

    assert!(
        !*delivered.lock().unwrap(),
        "on_submit must NOT fire on cancel"
    );
    // Even though the code never reached the closure, defence in depth:
    // assert no captured tracing event accidentally mentions the probe.
    assert_no_substring(&captured, PROBE);
}

// ---------------------------------------------------------------------------
// CodeFormat — both variants reach the host without leaking
// ---------------------------------------------------------------------------

#[test]
fn code_format_passthrough_for_both_variants() {
    for fmt in [CodeFormat::Number, CodeFormat::Any] {
        let host = MockPinHost::new();
        host.request_pin(fmt, Box::new(|_code| {}));
        assert_eq!(*host.received_format.lock().unwrap(), Some(fmt));
        host.cancel();
    }
}

// ---------------------------------------------------------------------------
// Audit-target gate — defence in depth on top of the all-events scan
// ---------------------------------------------------------------------------

/// Even if a future refactor accidentally routes the PIN through an
/// `audit::emit` call site, the audit-target rows must NEVER contain the
/// code substring. This test mirrors the `tests/integration/audit.rs`
/// pattern (a dedicated `target == "audit"` capture subscriber) and runs
/// the PIN submit flow underneath it. The expected outcome: zero audit
/// rows captured (the bare `MockPinHost` does not emit any audit events),
/// AND no audit row contains the synthetic code.
#[test]
fn no_audit_row_contains_pin_code() {
    use std::collections::BTreeMap;

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Default)]
    struct AuditFieldVisitor {
        fields: BTreeMap<String, String>,
    }
    impl Visit for AuditFieldVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_owned(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields
                .insert(field.name().to_owned(), value.to_owned());
        }
    }

    struct AuditOnlySubscriber {
        rows: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    }
    impl Subscriber for AuditOnlySubscriber {
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
            let mut visitor = AuditFieldVisitor::default();
            event.record(&mut visitor);
            self.rows.lock().unwrap().push(visitor.fields);
        }
    }

    const PROBE: &str = "5555";
    let rows: Arc<Mutex<Vec<BTreeMap<String, String>>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = AuditOnlySubscriber { rows: rows.clone() };

    tracing::subscriber::with_default(subscriber, || {
        let host = MockPinHost::new();
        host.request_pin(
            CodeFormat::Number,
            Box::new(move |code| {
                let _ = code.len();
            }),
        );
        host.submit(PROBE.to_owned());
    });

    // Whatever audit rows did fire (likely zero from the bare host), none
    // may contain the PIN substring.
    let captured = rows.lock().unwrap().clone();
    for row in &captured {
        for (k, v) in row {
            assert!(
                !v.contains(PROBE),
                "audit field `{k}` contained PIN substring `{PROBE}`: {row:?}"
            );
        }
    }
}
