//! Offline action FIFO queue with runtime idempotency gating (TASK-065).
//!
//! When the WebSocket connection is not [`ConnectionState::Live`], the
//! dispatcher routes `CallService` actions through this queue instead of
//! pushing them onto the WS command channel. On reconnect, the queue is
//! drained in FIFO order back through the same `command_tx` the live path
//! uses.
//!
//! # Security: runtime idempotency gate (Risk #6)
//!
//! `docs/plans/2026-04-28-phase-3-actions.md` §
//! `locked_decisions.idempotency_marker` is the load-bearing contract:
//!
//! * [`Action::Toggle`] — non-idempotent. Replaying after reconnect could
//!   un-toggle the entity (the user wanted ON; HA may have flipped it; the
//!   queued Toggle would flip it OFF). **Never queued.**
//! * [`Action::Url`] — non-idempotent. Each replay spawns a new external
//!   process. **Never queued.**
//! * [`Action::CallService`] — context-dependent. Idempotent in HA semantics
//!   only when the `(domain, service)` pair is on the per-config
//!   [`CallServiceAllowlist`](crate::dashboard::schema::CallServiceAllowlist)
//!   produced by the YAML validator (TASK-083). When no YAML is loaded
//!   (`--fixture` mode), the Phase 3 prefix rule (`turn_on`, `turn_off`,
//!   `set_*`) is the fallback gate, with a once-per-process
//!   `tracing::warn!` flagging that the loose gate is active. Other
//!   services (`delete_*`, `restart`, etc.) are rejected at enqueue time.
//!   **Allowlisted only.**
//!
//! The schema's `Action::idempotency()` const marker is the first gate; the
//! `CallService` allowlist is the second. Both fire at enqueue time so that
//! [`OfflineQueue::flush`] never has to repeat the check (Risk #6:
//! "logic error here would let a non-idempotent action enqueue and fire
//! twice on reconnect").
//!
//! # Allowlist source (TASK-090)
//!
//! Per `locked_decisions.call_service_allowlist_runtime_access` in
//! `docs/plans/2026-04-29-phase-4-layout.md`, the queue is constructed at
//! startup with an `Option<Arc<CallServiceAllowlist>>`:
//! - `Some(arc)` — production / YAML path. The validator-derived per-config
//!   set is the strict gate; `(domain, service)` pairs not in the set are
//!   rejected even if they would have satisfied the prefix rule.
//! - `None` — `--fixture` mode (no YAML loaded). The Phase 3 prefix rule is
//!   the fallback. A once-per-process `tracing::warn!` (gated by an atomic
//!   latch — semantically a `OnceLock<()>`) fires the first time this path
//!   is exercised so operators see the loose gate is active. Production
//!   should always load a Dashboard via `dashboard::loader::load()`.
//!
//! # Capacity & age-out (`locked_decisions.action_timing`)
//!
//! * `capacity` — drop-oldest on overflow (preserve the *recent* user intent
//!   over the historical one when both cannot fit).
//! * `max_age_ms` — defaults to 60 000 ms per
//!   `locked_decisions.action_timing.queue_max_age_ms`. An entry older than
//!   this is dropped at flush time without being dispatched. The age-out
//!   timer is wall-clock, not session-relative — disconnect at T0, reconnect
//!   at T0+90s, every entry from before T0+30s is silently aged out.
//!
//! # Threat model
//!
//! The queue is the **runtime control** that prevents non-idempotent actions
//! from being replayed in unexpected ways. The schema's const marker is a
//! convenience; the queue's enqueue gate is the actual security boundary
//! that cannot be bypassed by a logic error elsewhere in the dispatcher.
//!
//! Test coverage:
//! * Three explicit `non_idempotent_*` tests (`toggle_offline_returns_err_and_queue_remains_empty`,
//!   `url_offline_returns_err_and_queue_remains_empty`, plus the
//!   `call_service_not_allowlisted_*` family) assert zero entries land in
//!   the queue when a non-idempotent action is offered.
//! * Allowlist coverage: `turn_on`, `turn_off`, every `set_*` prefix, and
//!   one explicit deny case (`delete_user`).
//! * Reconnect-flush FIFO ordering, age-out, capacity overflow.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::actions::schema::{Action, Idempotency};
use crate::dashboard::schema::CallServiceAllowlist;
use crate::ha::client::{AckResult, OutboundCommand, OutboundFrame};
use crate::ha::entity::EntityId;

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Default offline-queue capacity (Phase 3 — `DeviceProfile.offline_queue_cap`
/// is a Phase 4 addition; until then this constant is the source of truth).
///
/// Rationale: 32 entries comfortably covers 30 seconds of single-tap user
/// interaction at human cadence. Above this the user is likely tapping into
/// the void and the drop-oldest policy is correct: more recent intent wins.
pub const DEFAULT_OFFLINE_QUEUE_CAPACITY: usize = 32;

/// Default age-out window per `locked_decisions.action_timing.queue_max_age_ms`.
///
/// Mirrors [`crate::actions::timing::ActionTiming`] — reproduced here so the
/// queue can be constructed with a single sensible default without dragging
/// in the timing struct (and so the constant participates in the locked-
/// decisions grep audit).
pub const DEFAULT_QUEUE_MAX_AGE_MS: u64 = 60_000;

// ---------------------------------------------------------------------------
// QueueEntry
// ---------------------------------------------------------------------------

/// One queued offline action awaiting reconnect-flush.
///
/// All entries are idempotent at enqueue time — the runtime gate in
/// [`OfflineQueue::enqueue`] rejects non-idempotent actions before this
/// struct is constructed. Flushing therefore never has to re-check the
/// idempotency marker (Risk #6 — keeping the gate at exactly one site
/// reduces surface for a logic-error regression).
#[derive(Debug, Clone)]
pub struct QueueEntry {
    /// The action variant. Always one of the queue-eligible variants
    /// ([`Action::CallService`] today; future Phase 4 idempotent variants
    /// would join the same allowlist).
    pub action: Action,
    /// Optional target entity supplied by the dispatcher (typically the
    /// `WidgetActionEntry.entity_id`). May differ from the `target` field
    /// inside the action itself when the dispatcher resolves a different
    /// authoritative target — preserving both is intentional so flush-time
    /// can reproduce the same wire frame.
    pub target: Option<EntityId>,
    /// Free-form service data passed through to HA verbatim at flush time.
    pub data: Option<serde_json::Value>,
    /// Wall-clock enqueue timestamp; compared against the configured
    /// `max_age_ms` at flush time.
    pub enqueued_at: Timestamp,
}

// ---------------------------------------------------------------------------
// QueueError
// ---------------------------------------------------------------------------

/// Why an [`OfflineQueue::enqueue`] call refused to accept an action.
///
/// Each variant maps onto a user-visible toast at the dispatcher boundary —
/// the queue itself does not surface UI; the dispatcher (the only caller in
/// production) translates these to [`crate::actions::dispatcher::DispatchError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    /// The action is non-idempotent ([`Action::Toggle`] or [`Action::Url`])
    /// per [`Action::idempotency`]. Per
    /// `locked_decisions.idempotency_marker` these MUST surface a loud error
    /// to the user offline — replaying them on reconnect could double-fire
    /// or un-toggle a user-driven state.
    NonIdempotentRejected,
    /// The action was an [`Action::CallService`] but the `service` name did
    /// not satisfy the allowlist (`turn_on`, `turn_off`, `set_*`). The
    /// runtime allowlist is the supplementary control on top of the schema
    /// marker — without it, services like `delete_user` would queue and
    /// replay on reconnect.
    ServiceNotAllowlisted {
        /// HA domain (preserved for diagnostic toast).
        domain: String,
        /// HA service name that failed the allowlist check.
        service: String,
    },
    /// Variant given to [`OfflineQueue::enqueue`] is not one of the WS-bound
    /// idempotent shapes. UI-local variants ([`Action::MoreInfo`],
    /// [`Action::Navigate`], [`Action::None`]) are handled directly by the
    /// dispatcher even when offline; if the dispatcher mistakenly forwards
    /// one of them to the queue, this is the surfaced error rather than a
    /// silent "queued anyway".
    UnsupportedVariant,
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::NonIdempotentRejected => f.write_str(
                "non-idempotent action refused: cannot be queued offline (would re-fire on reconnect)",
            ),
            QueueError::ServiceNotAllowlisted { domain, service } => write!(
                f,
                "service `{domain}.{service}` is not on the offline-queue allowlist (turn_on / turn_off / set_*)"
            ),
            QueueError::UnsupportedVariant => f.write_str(
                "action variant cannot be queued offline (UI-local variants are dispatched directly)",
            ),
        }
    }
}

impl std::error::Error for QueueError {}

// ---------------------------------------------------------------------------
// FlushOutcome
// ---------------------------------------------------------------------------

/// Tally of what happened when [`OfflineQueue::flush`] drained the queue.
///
/// `dispatched + aged_out + send_failed == initial entry count`. Returned so
/// the caller (the reconnect FSM in production; tests in this module) can
/// log a single structured line per flush instead of one-per-entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlushOutcome {
    /// Entries successfully forwarded onto `command_tx`.
    pub dispatched: usize,
    /// Entries dropped because their age exceeded `max_age_ms`.
    pub aged_out: usize,
    /// Entries the queue tried to forward but the channel was closed / full.
    /// These are surfaced as a count rather than re-enqueued — a closed
    /// channel means the WS task has gone away again, and replaying on the
    /// next reconnect would mean the entry could sit in the queue past
    /// `max_age_ms` while it is in the dispatch attempt path.
    pub send_failed: usize,
}

// ---------------------------------------------------------------------------
// OfflineQueue
// ---------------------------------------------------------------------------

/// FIFO offline-action queue.
///
/// Construction is `OfflineQueue::with_capacity(cap, max_age_ms)` (no
/// allowlist — fallback prefix rule applies) or
/// [`OfflineQueue::with_allowlist`] (validator-derived per-config allowlist
/// is the primary gate). Tests inject a clock via
/// [`OfflineQueue::enqueue_at`] / [`OfflineQueue::flush_at`]; production
/// paths use [`OfflineQueue::enqueue`] and [`OfflineQueue::flush`] which read
/// `Timestamp::now()`.
///
/// The queue is `Send` and is intended to be wrapped in
/// `Arc<Mutex<OfflineQueue>>` in production so the dispatcher and the
/// reconnect FSM can share it without lifetime gymnastics.
#[derive(Debug)]
pub struct OfflineQueue {
    entries: VecDeque<QueueEntry>,
    max_age_ms: u64,
    capacity: usize,
    /// Per-config `(domain, service)` allowlist produced by the YAML
    /// validator (TASK-083). `None` means no YAML was loaded (`--fixture`
    /// mode); enqueue falls back to the Phase 3 prefix rule with a
    /// once-per-process `tracing::warn!`. See module-level
    /// `# Allowlist source (TASK-090)` for the threat-model rationale.
    allowlist: Option<Arc<CallServiceAllowlist>>,
}

impl OfflineQueue {
    /// Construct a new queue with explicit capacity + age-out window. The
    /// queue runs without a validator-derived allowlist — the Phase 3
    /// prefix rule (`turn_on`, `turn_off`, `set_*`) is the fallback gate.
    /// A once-per-process `tracing::warn!` fires the first time this path
    /// is exercised so operators see the loose gate is active.
    ///
    /// Use [`Self::with_allowlist`] in production when a Dashboard is
    /// loaded.
    #[must_use]
    pub fn with_capacity(capacity: usize, max_age_ms: u64) -> Self {
        // VecDeque::with_capacity is a hint; the queue's hard cap is
        // `self.capacity` enforced inside `enqueue_inner`.
        OfflineQueue {
            entries: VecDeque::with_capacity(capacity),
            max_age_ms,
            capacity,
            allowlist: None,
        }
    }

    /// Construct a queue with the Phase 3 defaults
    /// (`DEFAULT_OFFLINE_QUEUE_CAPACITY` entries, `DEFAULT_QUEUE_MAX_AGE_MS`
    /// age-out) and no allowlist (fallback prefix rule).
    ///
    /// Production should prefer [`Self::with_allowlist`] when a Dashboard
    /// has been loaded — the validator-derived per-config set is the strict
    /// gate per `locked_decisions.call_service_allowlist_runtime_access`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_OFFLINE_QUEUE_CAPACITY, DEFAULT_QUEUE_MAX_AGE_MS)
    }

    /// Construct a queue with an explicit per-config
    /// [`CallServiceAllowlist`] sourced from a loaded
    /// [`Dashboard`](crate::dashboard::schema::Dashboard).
    ///
    /// Capacity and age-out window default to
    /// [`DEFAULT_OFFLINE_QUEUE_CAPACITY`] /
    /// [`DEFAULT_QUEUE_MAX_AGE_MS`] respectively.
    ///
    /// `allowlist` is `Some(arc)` for the YAML path (strict per-domain
    /// gate); `None` falls back to the Phase 3 prefix rule. The arc is
    /// shared with `Dashboard.call_service_allowlist` per
    /// `locked_decisions.call_service_allowlist_runtime_access`.
    #[must_use]
    pub fn with_allowlist(allowlist: Option<Arc<CallServiceAllowlist>>) -> Self {
        Self::with_capacity_and_allowlist(
            DEFAULT_OFFLINE_QUEUE_CAPACITY,
            DEFAULT_QUEUE_MAX_AGE_MS,
            allowlist,
        )
    }

    /// Construct a queue with explicit capacity + age-out window AND an
    /// optional validator-derived allowlist.
    ///
    /// This is the most general constructor; the simpler
    /// [`Self::with_capacity`] / [`Self::with_allowlist`] / [`Self::new`]
    /// forms are convenience wrappers around it.
    #[must_use]
    pub fn with_capacity_and_allowlist(
        capacity: usize,
        max_age_ms: u64,
        allowlist: Option<Arc<CallServiceAllowlist>>,
    ) -> Self {
        OfflineQueue {
            entries: VecDeque::with_capacity(capacity),
            max_age_ms,
            capacity,
            allowlist,
        }
    }

    /// Number of entries currently in the queue.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Configured capacity (drop-oldest threshold).
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Configured `max_age_ms`.
    #[must_use]
    pub fn max_age_ms(&self) -> u64 {
        self.max_age_ms
    }

    /// Enqueue an action for replay on reconnect.
    ///
    /// Wall-clock time is read from [`Timestamp::now`]. Tests should prefer
    /// [`Self::enqueue_at`] for deterministic ordering.
    ///
    /// # Errors
    ///
    /// See [`QueueError`].
    pub fn enqueue(
        &mut self,
        action: Action,
        target: Option<EntityId>,
        data: Option<serde_json::Value>,
    ) -> Result<(), QueueError> {
        self.enqueue_at(action, target, data, Timestamp::now())
    }

    /// [`Self::enqueue`] with an injected `now` for deterministic tests.
    ///
    /// All idempotency / allowlist checks fire here; this is the **single
    /// security gate** for the offline queue per the module-level threat
    /// model.
    pub fn enqueue_at(
        &mut self,
        action: Action,
        target: Option<EntityId>,
        data: Option<serde_json::Value>,
        now: Timestamp,
    ) -> Result<(), QueueError> {
        // Gate 1: schema-level idempotency marker. Toggle / Url are
        // immediate `Err` — they are NEVER queued. This is the
        // load-bearing rejection path tested by
        // `toggle_offline_returns_err_and_queue_remains_empty` and
        // `url_offline_returns_err_and_queue_remains_empty`.
        if action.idempotency() == Idempotency::NonIdempotent {
            warn!(
                ?action,
                "offline queue: rejecting non-idempotent action (Toggle/Url cannot be replayed)"
            );
            return Err(QueueError::NonIdempotentRejected);
        }

        // Gate 2: shape validation + service allowlist for CallService.
        // Other idempotent variants (MoreInfo / Navigate / None) are
        // UI-local and the dispatcher should not have forwarded them
        // here. The dispatcher's offline-routing branch only forwards
        // CallService (per its design); this defensive `UnsupportedVariant`
        // surfaces a programming error rather than queueing UI-local noise.
        match &action {
            Action::CallService {
                domain, service, ..
            } => {
                if !is_service_allowlisted(self.allowlist.as_ref(), domain, service) {
                    warn!(
                        domain = %domain,
                        service = %service,
                        allowlist_source = if self.allowlist.is_some() { "yaml" } else { "fallback-prefix" },
                        "offline queue: rejecting non-allowlisted CallService"
                    );
                    return Err(QueueError::ServiceNotAllowlisted {
                        domain: domain.clone(),
                        service: service.clone(),
                    });
                }
            }
            // Idempotency::NonIdempotent already returned above; the
            // remaining idempotent variants land here.
            Action::MoreInfo | Action::Navigate { .. } | Action::None => {
                return Err(QueueError::UnsupportedVariant);
            }
            // Phase 6 typed variants (TASK-099): dispatcher wiring is deferred
            // to TASK-102..TASK-105, TASK-108, TASK-109. Until those tickets
            // wire the per-variant dispatch paths, the offline queue treats
            // them as UnsupportedVariant — the dispatcher returns
            // NotImplementedYet before routing them here, so this branch is
            // unreachable in practice. Kept exhaustive so a future variant
            // addition remains a compile error.
            Action::SetTemperature { .. }
            | Action::SetHvacMode { .. }
            | Action::SetMediaVolume { .. }
            | Action::MediaTransport { .. }
            | Action::SetCoverPosition { .. }
            | Action::SetFanSpeed { .. }
            | Action::Lock { .. }
            | Action::Unlock { .. }
            | Action::AlarmArm { .. }
            | Action::AlarmDisarm { .. } => {
                return Err(QueueError::UnsupportedVariant);
            }
            // Toggle / Url already rejected by gate 1 — match exhaustively
            // so a future Action variant addition is a compile error
            // surfacing this exact decision.
            Action::Toggle | Action::Url { .. } => {
                debug_assert!(false, "non-idempotent variants must be rejected by gate 1");
                return Err(QueueError::NonIdempotentRejected);
            }
        }

        // Both gates passed → enqueue.
        let entry = QueueEntry {
            action,
            target,
            data,
            enqueued_at: now,
        };

        // Capacity enforcement: drop-oldest. We do this BEFORE pushing the
        // new entry so the queue is never momentarily over-cap (a writer
        // observing `len()` mid-enqueue would see at most `capacity`).
        while self.entries.len() >= self.capacity {
            if let Some(dropped) = self.entries.pop_front() {
                info!(
                    ?dropped.action,
                    enqueued_at = %dropped.enqueued_at,
                    "offline queue: drop-oldest on capacity overflow"
                );
            }
        }
        self.entries.push_back(entry);
        Ok(())
    }

    /// Drop entries older than `max_age_ms` from the head of the queue.
    ///
    /// Wall-clock time is read from [`Timestamp::now`]. Returns the number of
    /// entries dropped. This is invoked implicitly at the start of
    /// [`Self::flush`] / [`Self::flush_at`]; expose as a separate method
    /// so the (Phase 3 hypothetical) periodic-cleanup task can call it
    /// without doing a full flush.
    pub fn age_out(&mut self) -> usize {
        self.age_out_at(Timestamp::now())
    }

    /// [`Self::age_out`] with an injected `now`.
    pub fn age_out_at(&mut self, now: Timestamp) -> usize {
        let max_age = SignedDuration::from_millis(self.max_age_ms as i64);
        let mut dropped = 0usize;
        while let Some(front) = self.entries.front() {
            // `now - front.enqueued_at` may be negative if `now` precedes
            // the entry's timestamp (clock skew / test injection); jiff's
            // `Timestamp::duration_since` returns a `SignedDuration` which
            // can be negative — we only age out when the result is
            // strictly greater than `max_age`, so negative ages are
            // implicitly treated as fresh.
            let age = now.duration_since(front.enqueued_at);
            if age > max_age {
                let removed = self
                    .entries
                    .pop_front()
                    .expect("front existed under the same lock");
                debug!(
                    ?removed.action,
                    age_ms = age.as_millis(),
                    "offline queue: dropping aged-out entry"
                );
                dropped += 1;
            } else {
                break;
            }
        }
        dropped
    }

    /// Drain the queue in FIFO order, dispatching each entry through
    /// `command_tx`.
    ///
    /// Aged-out entries are dropped without dispatch. Each surviving entry
    /// is rebuilt into an [`OutboundCommand`] with a fresh oneshot. When
    /// `ack_observer` is supplied (typically by integration tests asserting
    /// FIFO order), the dispatcher-side `oneshot::Receiver<AckResult>` is
    /// pushed into it; production wiring passes `None` and lets the ack
    /// channel be dropped by the queue (the offline-queue path is
    /// fire-and-forget on reconnect — the live optimistic path resumes once
    /// the queue is empty).
    ///
    /// `flush` is **synchronous** so the dispatcher and the reconnect FSM
    /// can hold a `std::sync::Mutex<OfflineQueue>` across the whole
    /// operation without exposing a `.await`.
    pub fn flush(
        &mut self,
        command_tx: &mpsc::Sender<OutboundCommand>,
        ack_observer: Option<&mut Vec<oneshot::Receiver<AckResult>>>,
    ) -> FlushOutcome {
        self.flush_at(command_tx, ack_observer, Timestamp::now())
    }

    /// [`Self::flush`] with an injected `now` for deterministic age-out
    /// tests.
    pub fn flush_at(
        &mut self,
        command_tx: &mpsc::Sender<OutboundCommand>,
        mut ack_observer: Option<&mut Vec<oneshot::Receiver<AckResult>>>,
        now: Timestamp,
    ) -> FlushOutcome {
        let aged_out = self.age_out_at(now);
        let mut outcome = FlushOutcome {
            dispatched: 0,
            aged_out,
            send_failed: 0,
        };

        // FIFO: pop_front in a loop. Popping eagerly means a partial flush
        // (e.g. send failure mid-loop) leaves the queue in a well-defined
        // state — entries already popped are gone whether dispatched or
        // dropped, and any survivors keep their original FIFO order.
        while let Some(entry) = self.entries.pop_front() {
            let frame = build_frame(&entry);
            let (ack_tx, ack_rx) = oneshot::channel::<AckResult>();
            let cmd = OutboundCommand { frame, ack_tx };

            // try_send avoids ever blocking the reconnect FSM. A full
            // channel under a reconnect is itself a bug worth surfacing —
            // we count it as send_failed and continue with the next entry
            // so a single stuck entry does not strand the rest.
            match command_tx.try_send(cmd) {
                Ok(()) => {
                    outcome.dispatched += 1;
                    if let Some(observer) = ack_observer.as_deref_mut() {
                        observer.push(ack_rx);
                    }
                }
                Err(send_err) => {
                    outcome.send_failed += 1;
                    warn!(
                        ?send_err,
                        "offline queue: command_tx send failed during flush; dropping entry"
                    );
                }
            }
        }

        outcome
    }
}

impl Default for OfflineQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Runtime `CallService` allowlist gate (TASK-090).
///
/// Returns `true` if the `(domain, service)` pair is permitted to enqueue
/// for offline replay.
///
/// # Source-of-truth ordering
///
/// 1. If `allowlist` is `Some(arc)` — the validator-derived per-config set
///    (TASK-083) is the strict gate. Only pairs that appear verbatim in the
///    set are permitted. The Phase 3 prefix rule does NOT apply.
/// 2. If `allowlist` is `None` — `--fixture` mode (no YAML loaded). The
///    Phase 3 prefix fallback (`turn_on`, `turn_off`, `set_*`) is the gate.
///    A once-per-process `tracing::warn!` ([`warn_once_about_fallback`])
///    fires the first time this branch is hit.
///
/// # Why two sources
///
/// The validator-derived allowlist is per-config explicit (no prefix
/// heuristic — verb-named services like `cover.open_cover` are routable
/// when they appear in the YAML, while `light.set_brightness` is rejected
/// when the YAML only declares `light.turn_on`). Phase 4 production runs
/// always load a YAML; the prefix fallback is a development/test
/// convenience and emits the warn so an operator who sees it in CI logs
/// knows the loose gate is active.
///
/// # Why a prefix fallback at all
///
/// `set_*` matches the conventional HA set-attribute-shape services that
/// HA itself documents as idempotent (set_temperature replays harmlessly).
/// Using a denylist would mean every new HA service requires a security
/// review of whether it should land in the queue. The fallback exists so
/// `--fixture` mode (and pre-Phase-4 paths) still rejects destructive
/// services like `delete_user` / `restart` while permitting common `set_*`
/// flows.
#[must_use]
pub fn is_service_allowlisted(
    allowlist: Option<&Arc<CallServiceAllowlist>>,
    domain: &str,
    service: &str,
) -> bool {
    match allowlist {
        Some(set) => {
            // The set is keyed by owned `String` tuples. Avoid two
            // allocations per call by constructing the lookup tuple
            // explicitly — `BTreeSet::contains` requires `Borrow<K>` which
            // for `(String, String)` does not extend to `(&str, &str)`.
            // This branch is only on the YAML path; the allocation cost is
            // acceptable per `locked_decisions.call_service_allowlist_runtime_access`.
            set.contains(&(domain.to_string(), service.to_string()))
        }
        None => {
            warn_once_about_fallback();
            service == "turn_on" || service == "turn_off" || service.starts_with("set_")
        }
    }
}

// ---------------------------------------------------------------------------
// Once-per-process fallback warn (TASK-090)
// ---------------------------------------------------------------------------

/// Latch for the once-per-process fallback warn. Implemented as
/// `AtomicBool` rather than `std::sync::Once` / `OnceLock<()>` so tests
/// can deterministically reset it via [`reset_fallback_warned_for_test`]
/// without depending on cargo test ordering — a single-process test run
/// otherwise lets only the first test that exercises the fallback path
/// observe the warn output. Production semantics ("warn fires exactly once
/// per process") are preserved: `swap(true)` returns `false` exactly once
/// for the lifetime of the latch and only that caller emits the warn.
///
/// Per `locked_decisions.call_service_allowlist_runtime_access` the
/// "OnceLock<()> or Once" guidance is honoured semantically — `AtomicBool`
/// with a `swap(true)` is the same one-shot latch pattern.
static FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

/// Emit the fallback-prefix-allowlist warn the first time per process.
///
/// Invoked from [`is_service_allowlisted`] whenever the YAML allowlist is
/// `None` (no Dashboard loaded, e.g. `--fixture` mode). Subsequent calls
/// after the first are silent — the latch ensures the warn never spams the
/// log on every offline enqueue.
fn warn_once_about_fallback() {
    if !FALLBACK_WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "offline queue using Phase-3 prefix-allowlist fallback (no YAML loaded; \
             --fixture mode). Production should load a Dashboard via \
             `dashboard::loader::load()` so the validator-derived per-config \
             allowlist is the primary gate. This warn fires once per process."
        );
    }
}

/// Reset the once-per-process fallback-warn latch.
///
/// Test-only: production code never resets the latch (the warn is
/// intentionally one-shot). Tests that exercise the fallback path call
/// this at the top of `#[traced_test]` bodies so cargo's randomised test
/// ordering does not cause an earlier test to consume the latch.
#[cfg(test)]
pub(crate) fn reset_fallback_warned_for_test() {
    FALLBACK_WARNED.store(false, Ordering::Relaxed);
}

/// Reconstruct the [`OutboundFrame`] from a queued entry at flush time.
///
/// The entry's `target` (entity_id supplied by the dispatcher) is preferred
/// over the action's optional inline target so that the same widget-bound
/// resolution rule the live path uses applies on flush — a Phase 4 YAML
/// override that points at a different entity than the action's inline
/// payload remains the authoritative target.
fn build_frame(entry: &QueueEntry) -> OutboundFrame {
    match &entry.action {
        Action::CallService {
            domain,
            service,
            target: action_target,
            data: action_data,
        } => {
            let target_value = entry
                .target
                .as_ref()
                .map(|e| serde_json::json!({ "entity_id": e.as_str() }))
                .or_else(|| {
                    action_target
                        .as_ref()
                        .map(|t| serde_json::json!({ "entity_id": t }))
                });
            // entry.data takes precedence over the action's inline data —
            // the dispatcher passes the resolved data through as the
            // queue's third arg.
            let data = entry.data.clone().or_else(|| action_data.clone());
            OutboundFrame {
                domain: domain.clone(),
                service: service.clone(),
                target: target_value,
                data,
            }
        }
        // The enqueue-side gate guarantees only CallService lands here.
        // This branch is unreachable at runtime; we surface a debug_assert
        // so a future regression that lets a non-CallService variant past
        // the gate trips loudly in tests.
        other => {
            debug_assert!(
                false,
                "build_frame called with non-CallService variant: {other:?}"
            );
            OutboundFrame {
                domain: String::new(),
                service: String::new(),
                target: None,
                data: None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use jiff::SignedDuration;
    use serde_json::json;
    use tokio::sync::mpsc;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn call_service(service: &str) -> Action {
        Action::CallService {
            domain: "light".to_owned(),
            service: service.to_owned(),
            target: Some("light.kitchen".to_owned()),
            data: None,
        }
    }

    fn target_kitchen() -> Option<EntityId> {
        Some(EntityId::from("light.kitchen"))
    }

    fn make_recorder() -> (
        mpsc::Sender<OutboundCommand>,
        mpsc::Receiver<OutboundCommand>,
    ) {
        mpsc::channel::<OutboundCommand>(64)
    }

    // -----------------------------------------------------------------------
    // Allowlist helper — fallback prefix rule (None allowlist, --fixture mode)
    //
    // Per TASK-090: the prefix rule is retained as the FALLBACK only when no
    // YAML is loaded. These tests assert the fallback semantics are unchanged
    // from Phase 3.
    // -----------------------------------------------------------------------

    #[test]
    fn fallback_allowlist_accepts_turn_on_turn_off_and_set_prefix() {
        // Reset the once-per-process latch so this test deterministically
        // observes the fallback path regardless of test ordering.
        reset_fallback_warned_for_test();

        assert!(is_service_allowlisted(None, "light", "turn_on"));
        assert!(is_service_allowlisted(None, "switch", "turn_off"));
        assert!(is_service_allowlisted(None, "climate", "set_temperature"));
        assert!(is_service_allowlisted(None, "light", "set_brightness"));
        assert!(is_service_allowlisted(None, "any_domain", "set_"));
    }

    #[test]
    fn fallback_allowlist_rejects_destructive_and_neutral_services() {
        reset_fallback_warned_for_test();

        assert!(!is_service_allowlisted(None, "user", "delete_user"));
        assert!(!is_service_allowlisted(None, "homeassistant", "restart"));
        assert!(!is_service_allowlisted(None, "homeassistant", "reload"));
        // case-sensitive: only lower-case kebab/snake matches the allowlist
        assert!(!is_service_allowlisted(None, "light", "Turn_On"));
        assert!(!is_service_allowlisted(None, "light", ""));
        // Pre-set-prefix substrings must NOT slip through.
        assert!(!is_service_allowlisted(None, "fake", "xset_value"));
    }

    // -----------------------------------------------------------------------
    // Allowlist helper — YAML-derived strict gate (Some allowlist)
    //
    // Per TASK-090: the validator-derived allowlist supersedes the prefix
    // rule. A `(domain, service)` pair is allowed iff it appears verbatim in
    // the set; otherwise it is rejected even when the prefix rule WOULD have
    // allowed it.
    // -----------------------------------------------------------------------

    fn allowlist_of(pairs: &[(&str, &str)]) -> Arc<CallServiceAllowlist> {
        let set: CallServiceAllowlist = pairs
            .iter()
            .map(|(d, s)| ((*d).to_string(), (*s).to_string()))
            .collect();
        Arc::new(set)
    }

    #[test]
    fn yaml_allowlist_permits_only_listed_pairs() {
        let allowlist = allowlist_of(&[("light", "turn_on"), ("light", "turn_off")]);
        assert!(is_service_allowlisted(Some(&allowlist), "light", "turn_on"));
        assert!(is_service_allowlisted(
            Some(&allowlist),
            "light",
            "turn_off"
        ));
        // set_brightness WOULD pass the prefix fallback but the YAML
        // allowlist only declares turn_on / turn_off — strict gate REJECTS.
        assert!(!is_service_allowlisted(
            Some(&allowlist),
            "light",
            "set_brightness"
        ));
        // domain mismatch — same service, different domain.
        assert!(!is_service_allowlisted(
            Some(&allowlist),
            "switch",
            "turn_on"
        ));
    }

    #[test]
    fn yaml_allowlist_permits_verb_named_services_unreachable_by_prefix() {
        // The Phase 3 prefix rule rejects open_cover / close_cover (neither
        // turn_on nor turn_off nor set_*). The YAML-derived allowlist is per-
        // config explicit — declaring these services in the YAML routes them.
        let allowlist = allowlist_of(&[("cover", "open_cover"), ("cover", "close_cover")]);
        assert!(is_service_allowlisted(
            Some(&allowlist),
            "cover",
            "open_cover"
        ));
        assert!(is_service_allowlisted(
            Some(&allowlist),
            "cover",
            "close_cover"
        ));
        // A service not declared in the YAML is rejected even if it matches
        // the prefix rule — strict gate.
        assert!(!is_service_allowlisted(
            Some(&allowlist),
            "cover",
            "set_position"
        ));
    }

    // -----------------------------------------------------------------------
    // Non-idempotent rejection — load-bearing acceptance
    //
    // Per ticket TASK-065 acceptance: "Toggle offline → Err + queue empty:
    // load-bearing acceptance — non-idempotent rejection. Queue must contain
    // ZERO entries after Toggle rejection."
    //
    // Two tests fire here, one per non-idempotent variant. Both assert the
    // queue's `len() == 0` post-rejection.
    // -----------------------------------------------------------------------

    #[test]
    fn toggle_offline_returns_err_and_queue_remains_empty() {
        let mut queue = OfflineQueue::new();
        let result = queue.enqueue(Action::Toggle, target_kitchen(), None);
        assert_eq!(result, Err(QueueError::NonIdempotentRejected));
        assert_eq!(
            queue.len(),
            0,
            "Toggle rejection MUST leave the queue empty (Risk #6)"
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn url_offline_returns_err_and_queue_remains_empty() {
        let mut queue = OfflineQueue::new();
        let result = queue.enqueue(
            Action::Url {
                href: "https://example.org".to_owned(),
            },
            None,
            None,
        );
        assert_eq!(result, Err(QueueError::NonIdempotentRejected));
        assert_eq!(
            queue.len(),
            0,
            "Url rejection MUST leave the queue empty (Risk #6)"
        );
    }

    #[test]
    fn ui_local_variants_are_unsupported_not_silently_accepted() {
        // MoreInfo / Navigate / None are idempotent in the schema sense
        // (no side-effect / deterministic). Forwarding them to the queue
        // is a programming error in the dispatcher; the queue refuses
        // them rather than queueing UI-local noise.
        let mut queue = OfflineQueue::new();
        assert_eq!(
            queue.enqueue(Action::MoreInfo, target_kitchen(), None),
            Err(QueueError::UnsupportedVariant)
        );
        assert_eq!(
            queue.enqueue(
                Action::Navigate {
                    view_id: "default".to_owned()
                },
                None,
                None
            ),
            Err(QueueError::UnsupportedVariant)
        );
        assert_eq!(
            queue.enqueue(Action::None, None, None),
            Err(QueueError::UnsupportedVariant)
        );
        assert_eq!(queue.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Allowlisted enqueue
    // -----------------------------------------------------------------------

    #[test]
    fn call_service_turn_on_is_enqueued() {
        let mut queue = OfflineQueue::new();
        queue
            .enqueue(call_service("turn_on"), target_kitchen(), None)
            .expect("turn_on must be allowlisted");
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn call_service_turn_off_is_enqueued() {
        let mut queue = OfflineQueue::new();
        queue
            .enqueue(call_service("turn_off"), target_kitchen(), None)
            .expect("turn_off must be allowlisted");
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn call_service_set_prefix_is_enqueued() {
        let mut queue = OfflineQueue::new();
        queue
            .enqueue(
                call_service("set_temperature"),
                target_kitchen(),
                Some(json!({ "temperature": 21 })),
            )
            .expect("set_* must be allowlisted");
        assert_eq!(queue.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Non-allowlisted CallService — runtime allowlist gate
    // -----------------------------------------------------------------------

    #[test]
    fn call_service_not_allowlisted_returns_err_and_queue_empty() {
        let mut queue = OfflineQueue::new();
        let action = Action::CallService {
            domain: "user".to_owned(),
            service: "delete_user".to_owned(),
            target: Some("user.someone".to_owned()),
            data: None,
        };
        let result = queue.enqueue(action, None, None);
        match result {
            Err(QueueError::ServiceNotAllowlisted { domain, service }) => {
                assert_eq!(domain, "user");
                assert_eq!(service, "delete_user");
            }
            other => panic!("expected ServiceNotAllowlisted, got {other:?}"),
        }
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn call_service_restart_returns_err_and_queue_empty() {
        let mut queue = OfflineQueue::new();
        let action = Action::CallService {
            domain: "homeassistant".to_owned(),
            service: "restart".to_owned(),
            target: None,
            data: None,
        };
        assert!(matches!(
            queue.enqueue(action, None, None),
            Err(QueueError::ServiceNotAllowlisted { .. })
        ));
        assert_eq!(queue.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Reconnect-flush FIFO order (load-bearing)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reconnect_flush_preserves_fifo_order() {
        let mut queue = OfflineQueue::new();

        // Enqueue 5 distinct actions in a known order. Use injected
        // monotonically-advancing timestamps so age-out never fires.
        let base = Timestamp::now();
        for i in 0..5 {
            let action = Action::CallService {
                domain: "light".to_owned(),
                service: "turn_on".to_owned(),
                target: Some(format!("light.entity_{i}")),
                data: Some(json!({ "marker": i })),
            };
            queue
                .enqueue_at(
                    action,
                    Some(EntityId::from(format!("light.entity_{i}").as_str())),
                    Some(json!({ "marker": i })),
                    base.checked_add(SignedDuration::from_millis(i as i64))
                        .unwrap(),
                )
                .expect("turn_on must be allowlisted");
        }
        assert_eq!(queue.len(), 5);

        let (tx, mut rx) = make_recorder();
        let outcome = queue.flush_at(&tx, None, base);
        assert_eq!(outcome.dispatched, 5);
        assert_eq!(outcome.aged_out, 0);
        assert_eq!(outcome.send_failed, 0);
        assert!(queue.is_empty(), "flush must drain the queue completely");

        // Drain the recorder and assert FIFO order via the marker payload.
        for expected_marker in 0..5 {
            let cmd = rx.try_recv().expect("recorder must have received");
            assert_eq!(cmd.frame.domain, "light");
            assert_eq!(cmd.frame.service, "turn_on");
            assert_eq!(
                cmd.frame.data,
                Some(json!({ "marker": expected_marker })),
                "FIFO order broken at index {expected_marker}"
            );
            assert_eq!(
                cmd.frame.target,
                Some(json!({ "entity_id": format!("light.entity_{expected_marker}") }))
            );
        }
        assert!(rx.try_recv().is_err(), "no extra commands beyond the 5");
    }

    // -----------------------------------------------------------------------
    // Age-out
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn age_out_drops_entries_older_than_max_age_ms() {
        // max_age_ms=1000 — entries enqueued at T=0 must be dropped at
        // T=1500 (already past the window).
        let mut queue = OfflineQueue::with_capacity(8, 1000);
        let t0 = Timestamp::now();
        queue
            .enqueue_at(call_service("turn_on"), target_kitchen(), None, t0)
            .expect("enqueue at t=0");
        queue
            .enqueue_at(
                call_service("turn_off"),
                target_kitchen(),
                None,
                t0.checked_add(SignedDuration::from_millis(100)).unwrap(),
            )
            .expect("enqueue at t=100");
        assert_eq!(queue.len(), 2);

        // Advance clock past the age-out window for both.
        let t_late = t0.checked_add(SignedDuration::from_millis(1500)).unwrap();
        let dropped = queue.age_out_at(t_late);
        assert_eq!(dropped, 2);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn flush_drops_aged_out_entries_without_dispatch() {
        // Enqueue at T=0; flush at T=2*max_age — the entry is aged-out at
        // flush time and the recorder must not see any frame.
        let mut queue = OfflineQueue::with_capacity(8, 1000);
        let t0 = Timestamp::now();
        queue
            .enqueue_at(call_service("turn_on"), target_kitchen(), None, t0)
            .expect("enqueue must succeed");

        let (tx, mut rx) = make_recorder();
        let t_flush = t0.checked_add(SignedDuration::from_millis(2000)).unwrap();
        let outcome = queue.flush_at(&tx, None, t_flush);

        assert_eq!(outcome.dispatched, 0);
        assert_eq!(outcome.aged_out, 1);
        assert_eq!(outcome.send_failed, 0);
        assert!(rx.try_recv().is_err(), "aged-out entry must not be sent");
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn age_out_only_strips_head_entries_within_window() {
        // First entry old, second entry fresh — only the head should
        // age out. (FIFO ordering inside the queue keeps the assertion
        // straightforward: pop_front while front.is_aged.)
        let mut queue = OfflineQueue::with_capacity(8, 1000);
        let t0 = Timestamp::now();
        queue
            .enqueue_at(call_service("turn_on"), target_kitchen(), None, t0)
            .expect("first enqueue");
        let t_recent = t0.checked_add(SignedDuration::from_millis(900)).unwrap();
        queue
            .enqueue_at(call_service("turn_off"), target_kitchen(), None, t_recent)
            .expect("second enqueue");

        // At t=1500, only the first entry is past 1000ms.
        let t_check = t0.checked_add(SignedDuration::from_millis(1500)).unwrap();
        let dropped = queue.age_out_at(t_check);
        assert_eq!(dropped, 1);
        assert_eq!(queue.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Capacity overflow — drop-oldest
    // -----------------------------------------------------------------------

    #[test]
    fn capacity_overflow_drops_oldest_to_make_room() {
        // Capacity 3 — enqueue 4 distinct entries; the FIRST must be
        // evicted, the latter three remain in order.
        let mut queue = OfflineQueue::with_capacity(3, 60_000);
        let t0 = Timestamp::now();
        for i in 0..4i64 {
            let action = Action::CallService {
                domain: "light".to_owned(),
                service: "turn_on".to_owned(),
                target: Some(format!("light.entity_{i}")),
                data: Some(json!({ "marker": i })),
            };
            queue
                .enqueue_at(
                    action,
                    Some(EntityId::from(format!("light.entity_{i}").as_str())),
                    Some(json!({ "marker": i })),
                    t0.checked_add(SignedDuration::from_millis(i)).unwrap(),
                )
                .expect("enqueue must succeed");
        }
        assert_eq!(
            queue.len(),
            3,
            "capacity overflow must keep len at exactly capacity"
        );

        // The remaining entries are markers 1, 2, 3 — entry 0 was evicted.
        let markers: Vec<i64> = queue
            .entries
            .iter()
            .map(|e| match &e.action {
                Action::CallService { data: Some(d), .. } => {
                    d.get("marker").and_then(|v| v.as_i64()).unwrap_or(-1)
                }
                _ => -1,
            })
            .collect();
        assert_eq!(markers, vec![1, 2, 3]);
    }

    // -----------------------------------------------------------------------
    // Frame reconstruction at flush time
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn flush_uses_dispatcher_supplied_target_over_action_inline_target() {
        // The dispatcher passes the WidgetActionEntry's entity_id as the
        // queue's `target` arg; the resolved entity_id is preferred over
        // the action's inline target so flush reproduces the same wire
        // frame the live path would have built.
        let mut queue = OfflineQueue::new();
        let action = Action::CallService {
            domain: "light".to_owned(),
            service: "turn_on".to_owned(),
            // inline-target intentionally differs from dispatcher target.
            target: Some("light.inline_only".to_owned()),
            data: None,
        };
        queue
            .enqueue(
                action,
                Some(EntityId::from("light.dispatcher_target")),
                None,
            )
            .expect("enqueue");

        let (tx, mut rx) = make_recorder();
        let _ = queue.flush(&tx, None);
        let cmd = rx.try_recv().expect("flush dispatched the frame");
        assert_eq!(
            cmd.frame.target,
            Some(json!({ "entity_id": "light.dispatcher_target" })),
            "dispatcher-supplied target must win over action inline target"
        );
    }

    #[tokio::test]
    async fn flush_falls_back_to_action_inline_target_when_dispatcher_target_absent() {
        let mut queue = OfflineQueue::new();
        let action = Action::CallService {
            domain: "light".to_owned(),
            service: "turn_on".to_owned(),
            target: Some("light.inline_only".to_owned()),
            data: None,
        };
        queue.enqueue(action, None, None).expect("enqueue");

        let (tx, mut rx) = make_recorder();
        let _ = queue.flush(&tx, None);
        let cmd = rx.try_recv().expect("flush dispatched the frame");
        assert_eq!(
            cmd.frame.target,
            Some(json!({ "entity_id": "light.inline_only" })),
            "fall back to inline target when dispatcher omits one"
        );
    }

    // -----------------------------------------------------------------------
    // Default & defaults round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn default_constants_match_locked_decisions() {
        // locked_decisions.action_timing.queue_max_age_ms = 60000
        assert_eq!(DEFAULT_QUEUE_MAX_AGE_MS, 60_000);
        assert_eq!(DEFAULT_OFFLINE_QUEUE_CAPACITY, 32);
        let q = OfflineQueue::default();
        assert_eq!(q.capacity(), DEFAULT_OFFLINE_QUEUE_CAPACITY);
        assert_eq!(q.max_age_ms(), DEFAULT_QUEUE_MAX_AGE_MS);
        assert!(q.is_empty());
    }

    // -----------------------------------------------------------------------
    // TASK-090 — validator-derived per-config allowlist (queue-level)
    //
    // The four tests below mirror the ticket's `tests_added` block:
    //
    //   queue::tests::yaml_allowlist_rejects_set_brightness_when_only_turn_on_off_listed
    //   queue::tests::yaml_allowlist_permits_verb_named_open_cover
    //   queue::tests::fixture_mode_fallback_warn_fires_once
    //   queue::tests::fixture_mode_fallback_enqueues_set_temperature
    //
    // The pure helper-level tests (above) cover `is_service_allowlisted` in
    // isolation; these exercise the same gates through the full `enqueue`
    // path so the wiring between `OfflineQueue.allowlist` and the Gate-2
    // call site is regression-protected.
    // -----------------------------------------------------------------------

    #[test]
    fn yaml_allowlist_rejects_set_brightness_when_only_turn_on_off_listed() {
        // YAML declares `light: [turn_on, turn_off]` only. An attempt to
        // enqueue `light.set_brightness` MUST be rejected even though the
        // Phase 3 prefix rule WOULD have allowed it. This is the closing
        // promise of TASK-077 / TASK-090: the validator-derived allowlist
        // is strict, the prefix rule is fallback-only.
        let allowlist = Some(allowlist_of(&[("light", "turn_on"), ("light", "turn_off")]));
        let mut queue = OfflineQueue::with_allowlist(allowlist);

        let action = Action::CallService {
            domain: "light".to_owned(),
            service: "set_brightness".to_owned(),
            target: Some("light.kitchen".to_owned()),
            data: Some(json!({ "brightness": 128 })),
        };
        match queue.enqueue(action, target_kitchen(), None) {
            Err(QueueError::ServiceNotAllowlisted { domain, service }) => {
                assert_eq!(domain, "light");
                assert_eq!(service, "set_brightness");
            }
            other => {
                panic!("expected ServiceNotAllowlisted for light.set_brightness, got {other:?}")
            }
        }
        assert_eq!(queue.len(), 0, "rejected action must not enter the queue");

        // Sanity: the listed pairs ARE permitted by the same allowlist.
        queue
            .enqueue(call_service("turn_on"), target_kitchen(), None)
            .expect("light.turn_on is in the YAML allowlist");
        queue
            .enqueue(call_service("turn_off"), target_kitchen(), None)
            .expect("light.turn_off is in the YAML allowlist");
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn yaml_allowlist_permits_verb_named_open_cover() {
        // YAML declares `cover: [open_cover, close_cover]`. The prefix rule
        // would REJECT `cover.open_cover` (neither turn_on / turn_off /
        // set_*). The YAML allowlist permits it — verb-named idempotent
        // services become routable by per-config declaration.
        let allowlist = Some(allowlist_of(&[
            ("cover", "open_cover"),
            ("cover", "close_cover"),
        ]));
        let mut queue = OfflineQueue::with_allowlist(allowlist);

        let open = Action::CallService {
            domain: "cover".to_owned(),
            service: "open_cover".to_owned(),
            target: Some("cover.living_room".to_owned()),
            data: None,
        };
        queue
            .enqueue(open, Some(EntityId::from("cover.living_room")), None)
            .expect("cover.open_cover is declared in the YAML allowlist");

        let close = Action::CallService {
            domain: "cover".to_owned(),
            service: "close_cover".to_owned(),
            target: Some("cover.living_room".to_owned()),
            data: None,
        };
        queue
            .enqueue(close, Some(EntityId::from("cover.living_room")), None)
            .expect("cover.close_cover is declared in the YAML allowlist");

        assert_eq!(queue.len(), 2);

        // A non-listed pair is still rejected.
        let set_pos = Action::CallService {
            domain: "cover".to_owned(),
            service: "set_cover_position".to_owned(),
            target: Some("cover.living_room".to_owned()),
            data: Some(json!({ "position": 50 })),
        };
        assert!(matches!(
            queue.enqueue(set_pos, None, None),
            Err(QueueError::ServiceNotAllowlisted { .. })
        ));
        assert_eq!(queue.len(), 2, "non-listed pair must not enter the queue");
    }

    #[tracing_test::traced_test]
    #[test]
    fn fixture_mode_fallback_warn_fires_once() {
        // 100 enqueues in --fixture mode (allowlist=None). The fallback
        // prefix rule allows `set_temperature` on every call, but the
        // tracing::warn! must fire EXACTLY ONCE for the entire batch — not
        // 100 times. Per locked_decisions.call_service_allowlist_runtime_access
        // Test 2.
        reset_fallback_warned_for_test();

        // capacity=128 so all 100 entries fit without drop-oldest churn
        // muddying the assertion. The constructor still uses the
        // None-allowlist (fallback prefix rule active).
        let mut queue = OfflineQueue::with_capacity_and_allowlist(128, 60_000, None);
        for i in 0..100 {
            let action = Action::CallService {
                domain: "climate".to_owned(),
                service: "set_temperature".to_owned(),
                target: Some(format!("climate.room_{i}")),
                data: Some(json!({ "temperature": 21 })),
            };
            queue
                .enqueue(action, None, None)
                .expect("set_temperature is allowlisted by the fallback prefix rule");
        }
        assert_eq!(queue.len(), 100, "100 enqueues must all succeed");

        // Assert the warn message appears exactly ONCE in this test's
        // captured tracing output. The substring is matched against the
        // fixture-warn message body emitted by warn_once_about_fallback.
        logs_assert(|lines: &[&str]| {
            let needle = "offline queue using Phase-3 prefix-allowlist fallback";
            let count = lines.iter().filter(|l| l.contains(needle)).count();
            if count == 1 {
                Ok(())
            } else {
                Err(format!(
                    "expected fallback warn exactly once across 100 enqueues, observed {count}; \
                     captured lines: {lines:?}"
                ))
            }
        });
    }

    #[test]
    fn fixture_mode_fallback_enqueues_set_temperature() {
        // No YAML loaded (allowlist=None) → the fallback prefix rule lets
        // `climate.set_temperature` enter the queue. The warn fires (once
        // per process; reset for determinism), but enqueue succeeds.
        reset_fallback_warned_for_test();

        let mut queue = OfflineQueue::with_allowlist(None);
        let action = Action::CallService {
            domain: "climate".to_owned(),
            service: "set_temperature".to_owned(),
            target: Some("climate.living_room".to_owned()),
            data: Some(json!({ "temperature": 22 })),
        };
        queue
            .enqueue(action, Some(EntityId::from("climate.living_room")), None)
            .expect("set_temperature must enqueue under the fallback prefix rule");
        assert_eq!(queue.len(), 1);

        // The fallback warn latch is now set — a second fixture-mode enqueue
        // must not re-fire the warn. We verify the latch state directly: a
        // second swap(true) MUST observe `true` (the previous enqueue set
        // it). Direct latch state is checked rather than re-grepping logs
        // because `traced_test` is not in scope on this `#[test]`-only path.
        assert!(
            FALLBACK_WARNED.load(Ordering::Relaxed),
            "fallback warn latch must be set after the first fixture-mode enqueue"
        );
    }

    #[test]
    fn yaml_allowlist_constructor_threads_arc_to_enqueue_gate() {
        // The Arc passed to `with_allowlist` is the same one consulted at
        // enqueue time — proving the constructor is not a no-op. Without
        // this assertion a regression that swallowed the Arc (e.g. by
        // shadowing the field) would compile and silently fall back to the
        // prefix rule.
        let allowlist = allowlist_of(&[("light", "turn_on")]);
        let mut queue = OfflineQueue::with_allowlist(Some(allowlist.clone()));

        // Enqueue a service NOT on the allowlist but allowed by the prefix
        // rule. The strict gate must reject it — proving the Arc is the
        // gate, not the prefix rule.
        let set_brightness = Action::CallService {
            domain: "light".to_owned(),
            service: "set_brightness".to_owned(),
            target: Some("light.kitchen".to_owned()),
            data: None,
        };
        assert!(matches!(
            queue.enqueue(set_brightness, None, None),
            Err(QueueError::ServiceNotAllowlisted { .. })
        ));
    }
}
