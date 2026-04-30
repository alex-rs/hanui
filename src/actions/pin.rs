//! [`PinEntryHost`] trait and [`CodeFormat`] enum for PIN entry dispatch.
//!
//! # Module placement (per `locked_decisions.pin_entry_dispatch`)
//!
//! This trait lives in `src/actions/pin.rs` (not `src/ui/`) because:
//!
//! 1. The dispatcher (`src/actions/dispatcher.rs`) holds a reference to the
//!    trait object and calls `request_pin` before building the
//!    `call_service` frame for `Unlock` / `AlarmDisarm` actions.
//! 2. The `src/ui/ → src/actions/` import direction is already established in
//!    the codebase: `src/actions/dispatcher.rs:130` imports
//!    `crate::ui::view_router::ViewRouter`. Phase 6 follows the same pattern.
//! 3. Placing the trait in `src/actions/` avoids circular imports:
//!    `src/ui/bridge.rs` imports this trait (`crate::actions::pin::PinEntryHost`)
//!    to implement it — this is a `ui → actions` direction which is legal.
//!
//! # Security invariant (per `locked_decisions.pin_entry_dispatch`)
//!
//! The entered code is consumed **exactly once** via `FnOnce`:
//!
//! - The bridge implementation shows the Slint modal, waits for the user to
//!   submit, and calls `on_submit(code)` exactly once.
//! - The bridge MUST clear its local copy of the code immediately after
//!   calling `on_submit`.
//! - The code MUST NOT be stored in any struct field, logged, serialized, or
//!   passed through any channel beyond the `on_submit` callback.
//! - The `tracing-redact` layer provides a runtime safety net, but the primary
//!   control is the structural `FnOnce` consumption.
//!
//! # `CodeFormat` re-export
//!
//! `CodeFormat` is defined in `src/dashboard/schema.rs` (TASK-096). This
//! module re-exports it under `crate::actions::pin::CodeFormat` so callers
//! that only depend on `src/actions/` need not import from `src/dashboard/`.
//! The re-export is the canonical path used by TASK-104 / TASK-105.

pub use crate::dashboard::schema::CodeFormat;

// ---------------------------------------------------------------------------
// PinEntryHost
// ---------------------------------------------------------------------------

/// Capability trait: the dispatcher calls this to show a PIN entry prompt and
/// receive the entered code asynchronously via `on_submit`.
///
/// # Contract
///
/// - `request_pin` returns immediately; it is **not** a blocking call.
/// - The entered code is delivered asynchronously via `on_submit`, which is
///   called exactly once when the user presses OK on the modal.
/// - If the user cancels the modal, `on_submit` is **not** called. The
///   caller (dispatcher) must treat a missing `on_submit` invocation as a
///   cancelled operation and produce no service-call frame.
/// - The implementation MUST NOT log, store, or pass the code string beyond
///   the `on_submit` invocation.
/// - `Send + Sync` bounds allow the dispatcher to hold the trait object across
///   Tokio task boundaries and behind `Arc<dyn PinEntryHost>`.
///
/// # Example (dispatcher call site, TASK-104/105)
///
/// ```rust,ignore
/// let host: &dyn PinEntryHost = self.pin_host.as_ref();
/// host.request_pin(CodeFormat::Number, Box::new(move |code| {
///     // code is the entered PIN — used once, dropped at end of this closure.
///     dispatch_alarm_disarm(entity_id, code);
/// }));
/// ```
pub trait PinEntryHost: Send + Sync {
    /// Show a PIN entry modal.
    ///
    /// The entered code is delivered asynchronously via `on_submit` exactly
    /// once when the user confirms. On cancel, `on_submit` is never called.
    ///
    /// `code_format` constrains the keypad presentation:
    ///   - [`CodeFormat::Number`]: digits 0-9 only.
    ///   - [`CodeFormat::Any`]: full alphanumeric keypad (Phase 6 uses
    ///     numeric-only for all current tiles; `Any` is reserved for future
    ///     alarm panel variants that allow letter codes).
    ///
    /// # Security
    ///
    /// The implementation MUST clear its local copy of the code immediately
    /// after calling `on_submit`. The `FnOnce` bound structurally ensures the
    /// closure is consumed exactly once.
    fn request_pin(&self, code_format: CodeFormat, on_submit: Box<dyn FnOnce(String) + Send>);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // ── Type alias for the FnOnce closure slot ───────────────────────────────

    /// Boxed one-shot callback slot used by the mock.
    type PendingSlot = Mutex<Option<Box<dyn FnOnce(String) + Send>>>;

    // ── Mock implementation ──────────────────────────────────────────────────

    /// A mock `PinEntryHost` that captures the `on_submit` closure and lets
    /// the test invoke it with a synthetic code.
    struct MockPinEntryHost {
        /// Stores the pending `on_submit` closure after `request_pin` is called.
        pending: PendingSlot,
        /// Stores the `code_format` passed to `request_pin` for assertion.
        received_format: Mutex<Option<CodeFormat>>,
    }

    impl MockPinEntryHost {
        fn new() -> Self {
            MockPinEntryHost {
                pending: Mutex::new(None),
                received_format: Mutex::new(None),
            }
        }

        /// Invoke the pending `on_submit` with the given code. Panics if
        /// no `request_pin` has been called yet.
        fn submit(&self, code: String) {
            let cb = self
                .pending
                .lock()
                .unwrap()
                .take()
                .expect("request_pin must be called before submit");
            cb(code);
        }
    }

    impl PinEntryHost for MockPinEntryHost {
        fn request_pin(&self, code_format: CodeFormat, on_submit: Box<dyn FnOnce(String) + Send>) {
            *self.received_format.lock().unwrap() = Some(code_format);
            *self.pending.lock().unwrap() = Some(on_submit);
        }
    }

    /// `on_submit` is consumed exactly once: the mock captures the closure, the
    /// test invokes it with a synthetic code, and asserts the dispatcher-side
    /// `received_code` captures the value via the closure path.
    ///
    /// No code value escapes to any other code path (the `received_code` cell is
    /// the only holder; once the closure fires and writes into it, the closure
    /// itself is consumed and dropped).
    #[test]
    fn on_submit_consumes_code_once() {
        let host = MockPinEntryHost::new();

        // Synthetic code that the test "user" would enter.
        let synthetic_code = "1234".to_string();

        // Shared slot for the dispatcher-side receiver.
        let received_code: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let received_code_clone = Arc::clone(&received_code);

        host.request_pin(
            CodeFormat::Number,
            Box::new(move |code| {
                // This is the dispatcher-side consumer. It places the code
                // into the shared slot (simulating injection into a service frame)
                // and then drops `code` at end of scope.
                *received_code_clone.lock().unwrap() = Some(code);
                // `code` is dropped here — structural FnOnce consumption.
            }),
        );

        // Assert format was captured correctly.
        assert_eq!(
            *host.received_format.lock().unwrap(),
            Some(CodeFormat::Number)
        );

        // Assert no code has arrived yet (on_submit not fired).
        assert!(received_code.lock().unwrap().is_none());

        // Simulate the user entering the PIN and pressing OK.
        host.submit(synthetic_code.clone());

        // Assert the code arrived via the closure path exactly.
        let arrived = received_code
            .lock()
            .unwrap()
            .take()
            .expect("code should have arrived via on_submit");
        assert_eq!(arrived, synthetic_code);

        // Assert no pending closure remains (FnOnce was consumed exactly once).
        assert!(
            host.pending.lock().unwrap().is_none(),
            "on_submit closure must be consumed after one call"
        );
    }

    /// The entered code MUST NOT appear in any tracing span or event during
    /// a PIN entry cycle.
    ///
    /// This test installs a capturing tracing subscriber, runs a mock PIN
    /// entry that passes `on_submit("9876")`, and asserts the code "9876"
    /// does not appear in any captured event's formatted output.
    ///
    /// The test is intentionally conservative: it checks the formatted output
    /// of EVERY captured event, not just those with a specific target. This
    /// ensures that even a misfired debug!/info!/error! somewhere in the call
    /// path cannot leak the PIN.
    ///
    /// Per CLAUDE.md § Security rules: "Never log secrets, tokens, or full
    /// request/response bodies."
    /// Per `locked_decisions.pin_entry_dispatch` security invariant: code
    /// MUST NOT be logged.
    #[test]
    fn code_not_captured_in_tracing_spans() {
        use std::sync::Arc;
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;

        // CapturingLayer records every formatted event string.
        struct CapturingLayer {
            events: Arc<Mutex<Vec<String>>>,
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturingLayer {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                // Collect the formatted representation of every field.
                struct FieldCollector(Vec<String>);
                impl tracing::field::Visit for FieldCollector {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
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

        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let layer = CapturingLayer {
            events: Arc::clone(&events),
        };
        let subscriber = tracing_subscriber::registry().with(layer);

        let synthetic_code = "9876";

        with_default(subscriber, || {
            let host = MockPinEntryHost::new();

            host.request_pin(
                CodeFormat::Number,
                Box::new(move |code| {
                    // Simulate the dispatcher side: use the code, then drop it.
                    // The dispatcher would build a service frame here; in this
                    // test we just consume the string to satisfy FnOnce.
                    let _ = code.len(); // use the value without logging it
                                        // `code` is dropped at end of closure — no logging, no storage.
                }),
            );

            // Simulate the modal submission path.
            host.submit(synthetic_code.to_string());
        });

        // Check that the synthetic code does not appear in ANY captured event.
        let captured = events.lock().unwrap();
        for event_line in captured.iter() {
            assert!(
                !event_line.contains(synthetic_code),
                "PIN code '{}' must not appear in tracing event: {:?}",
                synthetic_code,
                event_line
            );
        }
    }

    /// Cancellation path: when `on_submit` is never called (user pressed
    /// Cancel), the code string is never seen by the dispatcher side.
    #[test]
    fn cancel_does_not_deliver_code() {
        let host = MockPinEntryHost::new();
        let received: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let received_clone = Arc::clone(&received);

        host.request_pin(
            CodeFormat::Any,
            Box::new(move |_code| {
                *received_clone.lock().unwrap() = true;
            }),
        );

        // Simulate cancel: do NOT call host.submit().
        // In production the bridge calls on-cancel() and drops the closure.
        drop(host.pending.lock().unwrap().take());

        // The dispatcher never received a code.
        assert!(
            !*received.lock().unwrap(),
            "on_submit must not fire on cancel"
        );
    }
}
