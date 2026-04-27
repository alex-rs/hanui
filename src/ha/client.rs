//! WebSocket client for the Home Assistant real-time API.
//!
//! # FSM
//!
//! ```text
//! Connecting → Authenticating → Subscribing → Snapshotting → Services → Live
//!                                                                          │
//!                                            ←── Reconnecting ←───────────┘
//!                                                     │
//!                                                 (retry)
//!
//! auth_invalid or overflow circuit-breaker → Failed (no reconnect)
//! ```
//!
//! # Subscribe-before-snapshot sequencing gate
//!
//! The FSM does NOT issue `get_states` until it has received the `result` ACK
//! for `subscribe_events`.  This closes the race window where a state change
//! could arrive between snapshot delivery and subscription activation.
//!
//! # Snapshot buffer
//!
//! State-changed events that arrive while `get_states` is in flight are
//! buffered in a bounded ring of capacity
//! `DEFAULT_PROFILE.snapshot_buffer_events` (10 000).  After the snapshot
//! reply arrives the buffered events are replayed before the FSM transitions
//! to `Live`.  On ring overflow the connection is dropped; 3 overflows within
//! 60 s trip the circuit-breaker and transition to `Failed`.
//!
//! # Reconnect exponential backoff
//!
//! When a transport error occurs the FSM transitions to `Reconnecting` and
//! sleeps for a jittered backoff window before attempting to reconnect.
//! Backoff window doubles on each attempt: `min(prev_window * 2, MAX_BACKOFF)`.
//! Full jitter is applied: `actual_delay = rand::uniform(0, current_window)`.
//!
//! **Stable-Live invariant (Codex Important I3):**  The reconnect attempt
//! counter and backoff window reset ONLY after a stable transition to `Live`,
//! defined as: the `get_states` snapshot has been applied AND broadcast events
//! are flowing for at least `STABLE_LIVE_DURATION` (5 s) with NO transport
//! error in that window.  A resync that fails mid-`Snapshotting` does NOT
//! reset the counter.
//!
//! # Reconnect snapshot diff
//!
//! On reconnect, a new `get_states` snapshot is compared against the previous
//! snapshot via [`SnapshotApplier`].  Only entities whose `last_updated`
//! timestamp changed produce broadcast `EntityUpdate` events.  The Arc swap is
//! atomic — no per-entity churn occurs during the diff.
//!
//! # Security: token handling
//!
//! The HA access token is exposed from `Config::expose_token()` exactly once,
//! at `auth` frame serialization time.  The resulting JSON bytes are written
//! directly to the WS write half.  No intermediate `String` is bound, logged,
//! or debug-printed.
//!
//! # Payload cap
//!
//! `tokio-tungstenite` is configured with `max_message_size` and
//! `max_frame_size` from `DEFAULT_PROFILE.ws_payload_cap`.  A message exceeding
//! the cap causes the transport to return an error; the FSM treats this as a
//! transport error and drops the connection for a full resync.
//!
//! # Phase 4
//!
//! `MIN_BACKOFF`, `MAX_BACKOFF`, and `STABLE_LIVE_DURATION` are module-level
//! constants here.  Phase 4 should surface them through `DeviceProfile` so that
//! SBC (single-board computer) profiles can use different values if the HA
//! instance runs on constrained hardware.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use jiff::Timestamp;
use rand::Rng;
use tokio::sync::{oneshot, watch};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::dashboard::profiles::DEFAULT_PROFILE;
use crate::ha::entity::{Entity, EntityId};
use crate::ha::live_store::LiveStore;
use crate::ha::protocol::{
    AuthPayload, EventVariant, GetServicesPayload, GetStatesPayload, InboundMsg, OutboundMsg,
    RawEntityState, SubscribeEventsPayload,
};
use crate::ha::services::{ServiceRegistry, ServiceRegistryHandle};
use crate::ha::store::EntityUpdate;
use crate::platform::config::Config;
use crate::platform::status::ConnectionState;

// Compile-time assertion: ServiceRegistryHandle is Send + Sync + Clone.
// Arc<RwLock<_>> satisfies all three by construction; this is here to make the
// import visibly used before the struct definition below and to surface any
// regression immediately if ServiceRegistry ever gains a non-Send/Sync field.
const _: fn() = || {
    fn _assert<T: Send + Sync + Clone>() {}
    _assert::<ServiceRegistryHandle>();
};

// ---------------------------------------------------------------------------
// Backoff constants
// ---------------------------------------------------------------------------

/// Minimum reconnect backoff window.
///
/// Phase 4: surface through `DeviceProfile` for SBC profiles.
pub const MIN_BACKOFF: Duration = Duration::from_secs(1);

/// Maximum reconnect backoff window.
///
/// Phase 4: surface through `DeviceProfile` for SBC profiles.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Duration a `Live` connection must remain stable (events flowing, no
/// transport errors) before the reconnect attempt counter is reset.
///
/// Phase 4: surface through `DeviceProfile` for SBC profiles.
pub const STABLE_LIVE_DURATION: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Public error type
// ---------------------------------------------------------------------------

/// Errors produced by the WebSocket client FSM.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The HA access token was rejected.
    ///
    /// Token plaintext is NEVER included in this error.  The `reason` field
    /// contains the human-readable message from HA with no token values.
    #[error("authentication failed: {reason}")]
    AuthInvalid { reason: String },

    /// The snapshot buffer overflowed 3 times within 60 s.
    #[error("HA instance too large for current profile")]
    OverflowCircuitBreaker,

    /// The WebSocket transport reported an error.
    #[error("WebSocket transport error: {0}")]
    Transport(#[from] tokio_tungstenite::tungstenite::Error),

    /// A `result` reply arrived with an unexpected id (correlation mismatch).
    #[error("result id {received} has no pending request")]
    IdMismatch { received: u32 },
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        ClientError::Transport(tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        ))
    }
}

// ---------------------------------------------------------------------------
// Backoff state
// ---------------------------------------------------------------------------

/// Exponential-backoff state for the reconnect loop.
///
/// The counter and window reset ONLY after a stable `Live` transition (see
/// module-level doc on the stable-Live invariant).
///
/// `pub` so TASK-034's reconnect loop and external observers can read the
/// state.  Fields are also `pub` for the same reason.
#[derive(Debug, Clone)]
pub struct BackoffState {
    /// Number of reconnect attempts since the last successful stable `Live`.
    pub attempts: u32,
    /// Current backoff window (doubles each attempt up to `MAX_BACKOFF`).
    pub current_window: Duration,
}

impl BackoffState {
    fn new() -> Self {
        BackoffState {
            attempts: 0,
            current_window: MIN_BACKOFF,
        }
    }

    /// Record one more failed attempt and advance the window.
    ///
    /// Returns the new window (before jitter is applied).
    pub fn advance(&mut self) -> Duration {
        // Window doubles each attempt, capped at MAX_BACKOFF.
        if self.attempts > 0 {
            self.current_window = Duration::min(self.current_window.saturating_mul(2), MAX_BACKOFF);
        }
        self.attempts = self.attempts.saturating_add(1);
        self.current_window
    }

    /// Reset the counter and window after a stable `Live` transition.
    pub fn reset(&mut self) {
        self.attempts = 0;
        self.current_window = MIN_BACKOFF;
    }
}

/// Apply full jitter to a backoff window: returns a random duration in
/// `[0, window]` using the provided RNG.
///
/// Full jitter is used (not equal jitter) per AWS exponential-backoff guidance,
/// as it distributes reconnect storms most evenly under simultaneous-failure
/// scenarios.
pub fn full_jitter<R: Rng>(window: Duration, rng: &mut R) -> Duration {
    let millis = window.as_millis() as u64;
    if millis == 0 {
        return Duration::ZERO;
    }
    let jittered = rng.gen_range(0..=millis);
    Duration::from_millis(jittered)
}

// ---------------------------------------------------------------------------
// SnapshotApplier trait
// ---------------------------------------------------------------------------

/// Minimal store interface used by [`WsClient`] during reconnect snapshot diff.
///
/// Only entities whose `last_updated` timestamp changed produce broadcast
/// `EntityUpdate` events.  The Arc swap is atomic — no per-entity churn.
///
/// `pub` so TASK-034's reconnect loop can wire `LiveStore` into this trait
/// without re-exporting it.  The blanket impl for `LiveStore` is below.
pub trait SnapshotApplier: Send + Sync {
    /// Return the current snapshot as an `Arc<HashMap>`.
    fn current_snapshot(&self) -> Arc<HashMap<EntityId, Entity>>;

    /// Replace the snapshot atomically with a new entity list.
    ///
    /// Does NOT broadcast any `EntityUpdate` events — the diff step is
    /// performed by the caller via `apply_changed_event`.
    fn apply_full_snapshot(&self, entities: Vec<Entity>);

    /// Broadcast an incremental update for a single entity.
    ///
    /// Called by the diff step for each entity whose `last_updated` changed
    /// across the reconnect snapshot.
    fn apply_changed_event(&self, update: EntityUpdate);
}

/// Blanket impl of [`SnapshotApplier`] for [`LiveStore`].
///
/// This wires the production `LiveStore` into the reconnect diff path without
/// modifying `live_store.rs`.
impl SnapshotApplier for LiveStore {
    fn current_snapshot(&self) -> Arc<HashMap<EntityId, Entity>> {
        self.snapshot()
    }

    fn apply_full_snapshot(&self, entities: Vec<Entity>) {
        self.apply_snapshot(entities);
    }

    fn apply_changed_event(&self, update: EntityUpdate) {
        self.apply_event(update);
    }
}

// ---------------------------------------------------------------------------
// Protocol → Entity conversion
// ---------------------------------------------------------------------------

/// Convert a [`RawEntityState`] (from `get_states` / `state_changed` events)
/// into a typed [`Entity`].
///
/// On timestamp parse failure the field falls back to `Timestamp::UNIX_EPOCH`
/// and emits a `tracing::warn` row.  This matches the resilience profile of
/// the rest of the protocol layer (better to render an entity with a stale
/// timestamp than to drop it entirely on a single malformed field).
pub fn raw_entity_to_entity(raw: &RawEntityState) -> Entity {
    let last_changed = Timestamp::from_str(&raw.last_changed).unwrap_or_else(|_| {
        tracing::warn!(
            entity_id = %raw.entity_id,
            "invalid last_changed timestamp; falling back to UNIX_EPOCH"
        );
        Timestamp::UNIX_EPOCH
    });
    let last_updated = Timestamp::from_str(&raw.last_updated).unwrap_or_else(|_| {
        tracing::warn!(
            entity_id = %raw.entity_id,
            "invalid last_updated timestamp; falling back to UNIX_EPOCH"
        );
        Timestamp::UNIX_EPOCH
    });

    let attrs_map = match raw.attributes.as_object() {
        Some(m) => m.clone(),
        None => serde_json::Map::new(),
    };

    Entity {
        id: EntityId::from(raw.entity_id.as_str()),
        state: Arc::from(raw.state.as_str()),
        attributes: Arc::new(attrs_map),
        last_changed,
        last_updated,
    }
}

/// Convert a `state_changed` event payload into an [`EntityUpdate`].
///
/// Returns `Some(update)` when the event is a `state_changed`; returns `None`
/// when the event variant is anything else (which the FSM ignores at the
/// caller).  The resulting `update.entity` is `None` when `new_state` is
/// `null` (HA's signal that the entity was removed).
pub fn event_to_entity_update(event: &crate::ha::protocol::EventPayload) -> Option<EntityUpdate> {
    let EventVariant::StateChanged(sc) = &event.event else {
        return None;
    };
    let id = EntityId::from(sc.data.entity_id.as_str());
    let entity = sc.data.new_state.as_ref().map(raw_entity_to_entity);
    Some(EntityUpdate { id, entity })
}

/// Apply a single `service_registered` / `service_removed` event to the
/// shared [`ServiceRegistryHandle`].
///
/// Returns `true` if the event matched a service-lifecycle variant and the
/// registry was mutated (or the no-op remove path exercised); `false` if the
/// event was anything else (the caller can fall through to other dispatch
/// arms).  The handle's write-lock is acquired only for the duration of this
/// single mutation — readers (`LiveStore::services_lookup`, Phase 3
/// dispatchers) see at most a single-pair gap.
///
/// `service_registered` events apply with a default-empty
/// [`crate::ha::services::ServiceMeta`] because HA's event payload does NOT
/// carry the field schema (verified against the documented event shape at
/// <https://www.home-assistant.io/docs/configuration/events/>).  The
/// next successful `get_services` round refills the metadata; until then,
/// `lookup` returns `Some(default_meta)` for the new pair, which is the
/// correct "exists but schema unknown" signal for Phase 3 dispatchers — they
/// can still resolve the `(domain, service)` and issue a `call_service`
/// frame, just without parameter introspection.
///
/// `service_removed` events call [`crate::ha::services::ServiceRegistry::remove_service`],
/// which is a documented no-op for unknown pairs (so a duplicate or
/// out-of-order event can never panic).
fn apply_service_lifecycle_event(
    services: &ServiceRegistryHandle,
    event: &crate::ha::protocol::EventPayload,
) -> bool {
    use crate::ha::services::ServiceMeta;
    match &event.event {
        EventVariant::ServiceRegistered(sl) => {
            // `expect` (not `unwrap`) so a poisoned lock surfaces a clear
            // panic message rather than the generic "called Result::unwrap
            // on Err".  Same convention as the get_services apply site.
            services
                .write()
                .expect("ServiceRegistry RwLock poisoned")
                .add_service(&sl.data.domain, &sl.data.service, ServiceMeta::default());
            tracing::info!(
                domain = %sl.data.domain,
                service = %sl.data.service,
                "service_registered event applied to ServiceRegistry (default meta until next get_services)"
            );
            true
        }
        EventVariant::ServiceRemoved(sl) => {
            services
                .write()
                .expect("ServiceRegistry RwLock poisoned")
                .remove_service(&sl.data.domain, &sl.data.service);
            tracing::info!(
                domain = %sl.data.domain,
                service = %sl.data.service,
                "service_removed event applied to ServiceRegistry"
            );
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Internal FSM phase
// ---------------------------------------------------------------------------

/// Internal FSM phases (mirrors `ConnectionState` but with richer payload).
///
/// `pub(crate)` so that in-crate tests can construct and inspect phases
/// without exposing the type in the public API.
#[derive(Debug)]
pub(crate) enum Phase {
    /// Sending the `auth` frame; waiting for `auth_ok` or `auth_invalid`.
    Authenticating,
    /// `auth_ok` received; `subscribe_events` sent; waiting for result ACK.
    Subscribing { subscribe_id: u32 },
    /// Subscription ACK received; `get_states` in flight.
    Snapshotting {
        get_states_id: u32,
        /// Ring buffer of state-changed events arriving during snapshot fetch.
        /// Capacity: `DEFAULT_PROFILE.snapshot_buffer_events`.
        event_buffer: Vec<InboundMsg>,
    },
    /// Snapshot applied; `get_services` in flight.
    Services { get_services_id: u32 },
    /// Fully operational.
    Live,
}

// ---------------------------------------------------------------------------
// Pending-request map
// ---------------------------------------------------------------------------

/// One-shot sender waiting for a `result` reply.
type PendingSender = oneshot::Sender<Result<serde_json::Value, String>>;

// ---------------------------------------------------------------------------
// Overflow circuit-breaker
// ---------------------------------------------------------------------------

/// Tracks consecutive snapshot-buffer overflows for the circuit-breaker.
pub struct OverflowBreaker {
    /// Timestamps of recent overflow events (within 60 s window).
    pub recent: Vec<Instant>,
}

impl OverflowBreaker {
    fn new() -> Self {
        OverflowBreaker { recent: Vec::new() }
    }

    /// Record an overflow.  Returns `true` if the circuit-breaker has tripped
    /// (3 or more overflows within 60 s).
    pub fn record_overflow(&mut self) -> bool {
        let now = Instant::now();
        self.recent
            .retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        self.recent.push(now);
        self.recent.len() >= 3
    }
}

// ---------------------------------------------------------------------------
// ID counter
// ---------------------------------------------------------------------------

struct IdCounter(Arc<AtomicU32>);

impl IdCounter {
    fn new() -> Self {
        IdCounter(Arc::new(AtomicU32::new(1)))
    }

    fn next(&self) -> u32 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// WsClient
// ---------------------------------------------------------------------------

/// Drives the HA WebSocket FSM.
///
/// Construct via [`WsClient::new`] and run via [`WsClient::run`].
///
/// # Routing snapshots and events into a store
///
/// By default `WsClient` parses the protocol but does not persist anything —
/// useful for protocol-only tests.  Call [`WsClient::with_store`] to attach an
/// [`Arc<dyn SnapshotApplier>`][SnapshotApplier] (typically a
/// [`LiveStore`][crate::ha::live_store::LiveStore]); the FSM will then route
/// `get_states` snapshots and `state_changed` events into the store via the
/// trait methods.
pub struct WsClient {
    config: Config,
    state_tx: watch::Sender<ConnectionState>,
    id_counter: IdCounter,
    /// Map from request id to the oneshot sender awaiting the result.
    pending: HashMap<u32, PendingSender>,
    /// Circuit-breaker for snapshot-buffer overflows.
    pub overflow_breaker: OverflowBreaker,
    /// Exponential-backoff state for the reconnect loop.
    pub(crate) backoff: BackoffState,
    /// When the FSM entered the `Live` state on the current connection.
    ///
    /// `None` if not currently `Live` or not yet reached `Live`.
    /// Reset to `None` on disconnect; set to `Some(Instant::now())` on `Live`
    /// entry.  The stability window begins from this timestamp.
    pub(crate) stable_live_since: Option<Instant>,
    /// Number of `Live` events received since `stable_live_since` was set.
    ///
    /// At least one event is required during the stability window for the
    /// stable-Live invariant to be satisfied (events must be "flowing").
    pub(crate) live_event_count: u64,
    /// How long the connection must remain `Live` with events flowing before
    /// the backoff counter resets.
    ///
    /// Defaults to `STABLE_LIVE_DURATION` (5 s).  Tests inject a shorter
    /// duration to avoid real sleeps.
    pub(crate) stable_live_threshold: Duration,
    /// Optional sink for `get_states` snapshots and live `state_changed`
    /// events.
    ///
    /// When `None`, parsed events are dropped after the FSM consumes them
    /// (protocol-only mode).  When `Some(store)`, the FSM:
    ///
    /// - applies the `get_states` snapshot via `store.apply_full_snapshot`,
    /// - replays the buffered events through `store.apply_changed_event`,
    /// - routes every subsequent `state_changed` event into the store via
    ///   `store.apply_changed_event`.
    pub(crate) store: Option<Arc<dyn SnapshotApplier>>,
    /// Shared, thread-safe handle to the service-definition registry.
    ///
    /// BLOCKER 3 fix (TASK-044) populated this on the `Services → Live`
    /// transition.  TASK-048 reshaped the field from an owned
    /// `ServiceRegistry` into a `ServiceRegistryHandle`
    /// (`Arc<RwLock<ServiceRegistry>>`) so the populated registry is
    /// reachable from a task OTHER than the WS reconnect-loop task.
    ///
    /// Constructed in `src/lib.rs::run_with_live_store` and shared via `Arc`
    /// clone with the [`LiveStore`] passed to [`WsClient::with_store`].  The
    /// FSM's `Phase::Services` arm acquires the write-lock and bulk-replaces
    /// the inner registry; Phase 3 dispatchers acquire the read-lock through
    /// [`LiveStore::services_lookup`] (or directly via the handle returned
    /// by [`WsClient::services_handle`]) to validate `(domain, service)`
    /// pairs before issuing a `call_service` frame.
    ///
    /// Defaults to a fresh, empty registry; remains empty if `get_services`
    /// fails (the FSM still proceeds to `Live`, matching the pre-existing
    /// tolerance contract — Phase 2 must not refuse to render just because
    /// the service catalogue is unavailable).
    ///
    /// [`LiveStore`]: crate::ha::live_store::LiveStore
    /// [`LiveStore::services_lookup`]: crate::ha::live_store::LiveStore::services_lookup
    pub(crate) services: ServiceRegistryHandle,
}

impl WsClient {
    /// Create a new client.
    ///
    /// The `state_tx` watch sender is updated on every FSM transition so
    /// external observers (e.g. the status banner) can react.
    pub fn new(config: Config, state_tx: watch::Sender<ConnectionState>) -> Self {
        WsClient {
            config,
            state_tx,
            id_counter: IdCounter::new(),
            pending: HashMap::new(),
            overflow_breaker: OverflowBreaker::new(),
            backoff: BackoffState::new(),
            stable_live_since: None,
            live_event_count: 0,
            stable_live_threshold: STABLE_LIVE_DURATION,
            store: None,
            services: ServiceRegistry::new_handle(),
        }
    }

    /// Return a clone of the shared [`ServiceRegistryHandle`].
    ///
    /// The returned handle is an `Arc` clone — cheap and `Send + Sync`.  After
    /// the FSM has completed `Phase::Services → Live`, callers can acquire a
    /// read-lock and look up `(domain, service)` pairs:
    ///
    /// ```ignore
    /// let handle = client.services_handle();
    /// let guard = handle.read().unwrap();
    /// let meta = guard.lookup("light", "turn_on");
    /// ```
    ///
    /// Phase 3's command dispatcher should prefer
    /// [`LiveStore::services_lookup`] (which already encapsulates the
    /// read-lock) over reaching through `WsClient`; this accessor exists so
    /// integration tests can assert the shared-Arc invariant via
    /// `Arc::ptr_eq` between this handle and the one held by `LiveStore`.
    ///
    /// [`LiveStore::services_lookup`]: crate::ha::live_store::LiveStore::services_lookup
    pub fn services_handle(&self) -> ServiceRegistryHandle {
        Arc::clone(&self.services)
    }

    /// Attach a [`SnapshotApplier`] sink so the FSM persists `get_states`
    /// snapshots and routes live events to the store.
    ///
    /// Without this call, parsed snapshot/event payloads are dropped after the
    /// FSM consumes them.  Production code in `src/lib.rs::run_with_live_store`
    /// (TASK-034) wires a `LiveStore` here; protocol-only tests omit it.
    pub fn with_store(mut self, store: Arc<dyn SnapshotApplier>) -> Self {
        self.store = Some(store);
        self
    }

    /// Attach a shared [`ServiceRegistryHandle`] so the FSM populates the
    /// same registry the [`LiveStore`] (and Phase 3 dispatchers) read from.
    ///
    /// Without this call the client owns a private handle constructed in
    /// `WsClient::new`; the registry is still populated by the
    /// `Services → Live` transition but no other task can observe the result.
    /// Production wiring in `src/lib.rs::run_with_live_store` constructs the
    /// handle once, clones it into both the [`LiveStore`] and the
    /// [`WsClient`] via this builder, so a single `Arc<RwLock<_>>` backs both
    /// endpoints.
    ///
    /// [`LiveStore`]: crate::ha::live_store::LiveStore
    pub fn with_registry(mut self, services: ServiceRegistryHandle) -> Self {
        self.services = services;
        self
    }

    /// Transition the FSM to a new `ConnectionState` and publish via watch.
    pub fn set_state(&self, state: ConnectionState) {
        tracing::info!(state = ?state, "FSM transition");
        let _ = self.state_tx.send(state);
    }

    /// Allocate a fresh message ID.
    fn next_id(&self) -> u32 {
        self.id_counter.next()
    }

    /// Register a pending request and return the receiver for its result.
    pub fn register_pending(
        &mut self,
        id: u32,
    ) -> oneshot::Receiver<Result<serde_json::Value, String>> {
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);
        rx
    }

    /// Resolve a pending request by id.  Returns `IdMismatch` if no pending
    /// request is registered for the id.
    // ClientError::Transport is large; this function only ever returns
    // ClientError::IdMismatch which is small, but clippy sees the enum.
    #[allow(clippy::result_large_err)]
    pub fn resolve_pending(
        &mut self,
        id: u32,
        value: Result<serde_json::Value, String>,
    ) -> Result<(), ClientError> {
        match self.pending.remove(&id) {
            Some(tx) => {
                let _ = tx.send(value);
                Ok(())
            }
            None => Err(ClientError::IdMismatch { received: id }),
        }
    }

    /// Record that the FSM has entered the `Live` state.
    ///
    /// Resets the stability window.  Call this whenever the FSM transitions
    /// to `Phase::Live`.
    pub(crate) fn on_live_entered(&mut self) {
        self.stable_live_since = Some(Instant::now());
        self.live_event_count = 0;
    }

    /// Record one `Live` event arrival.
    ///
    /// Also checks whether the stable-Live invariant is now satisfied and, if
    /// so, resets the backoff counter.  Returns `true` if the counter was just
    /// reset (i.e. the stability window was satisfied by this event).
    pub(crate) fn on_live_event(&mut self) -> bool {
        self.live_event_count = self.live_event_count.saturating_add(1);
        self.check_and_maybe_reset_backoff()
    }

    /// Check the stable-Live invariant and reset backoff if satisfied.
    ///
    /// Returns `true` if the backoff was reset on this call.  Idempotent —
    /// after the first reset, subsequent calls return `false` (the counter is
    /// already 0 and window is already `MIN_BACKOFF`).
    pub(crate) fn check_and_maybe_reset_backoff(&mut self) -> bool {
        // Already at baseline — nothing to reset.
        if self.backoff.attempts == 0 && self.backoff.current_window == MIN_BACKOFF {
            return false;
        }

        let Some(since) = self.stable_live_since else {
            return false;
        };

        // Stable if: elapsed >= threshold AND at least one event arrived.
        if since.elapsed() >= self.stable_live_threshold && self.live_event_count > 0 {
            tracing::info!(
                attempts_before_reset = self.backoff.attempts,
                "stable-Live invariant satisfied; resetting backoff counter"
            );
            self.backoff.reset();
            return true;
        }

        false
    }

    /// Reset reconnect-related state on disconnect.
    ///
    /// Must be called before each reconnect attempt so that stale stability
    /// state from the previous connection does not interfere.
    pub(crate) fn on_disconnect(&mut self) {
        self.stable_live_since = None;
        self.live_event_count = 0;
    }

    /// Diff two entity snapshots and broadcast `EntityUpdate` for changed entities.
    ///
    /// "Changed" means the entity's `last_updated` timestamp differs between
    /// `old_snap` and the new snapshot.  New entities (not in `old_snap`) and
    /// removed entities (in `old_snap` but not in new snapshot) are always
    /// broadcast.
    ///
    /// This is called after `apply_full_snapshot` has already swapped the new
    /// snapshot into the store.  The diff runs against `old_snap` (captured
    /// before the swap) and the new snapshot returned by `store.current_snapshot()`.
    pub fn diff_and_broadcast<S: SnapshotApplier + ?Sized>(
        old_snap: &Arc<HashMap<EntityId, Entity>>,
        store: &S,
    ) {
        let new_snap = store.current_snapshot();

        // Entities in new snapshot — broadcast if last_updated changed or new.
        for (id, new_entity) in new_snap.iter() {
            let changed = old_snap
                .get(id)
                .map(|old| old.last_updated != new_entity.last_updated)
                .unwrap_or(true); // new entity → always broadcast

            if changed {
                store.apply_changed_event(EntityUpdate {
                    id: id.clone(),
                    entity: Some(new_entity.clone()),
                });
            }
        }

        // Entities removed (in old but not in new) → broadcast removal.
        for id in old_snap.keys() {
            if !new_snap.contains_key(id) {
                store.apply_changed_event(EntityUpdate {
                    id: id.clone(),
                    entity: None,
                });
            }
        }
    }

    /// Connect to HA and run the FSM until `Failed` or a transport error.
    ///
    /// Returns `Err(ClientError::AuthInvalid)` on auth failure (no reconnect).
    /// Returns `Err(ClientError::OverflowCircuitBreaker)` when the circuit
    /// breaker trips.  Returns transport errors as `Err(ClientError::Transport)`.
    #[allow(clippy::result_large_err)]
    pub async fn run(&mut self) -> Result<(), ClientError> {
        self.set_state(ConnectionState::Connecting);
        tracing::info!(url = %self.config.url, "connecting to HA");

        let ws_config = WebSocketConfig {
            max_message_size: Some(DEFAULT_PROFILE.ws_payload_cap),
            max_frame_size: Some(DEFAULT_PROFILE.ws_payload_cap),
            ..Default::default()
        };

        // Pass the URL as a &str — tungstenite's IntoClientRequest is implemented
        // for &str and String but not url::Url.
        let (ws_stream, _response) = tokio_tungstenite::connect_async_with_config(
            self.config.url.as_str(),
            Some(ws_config),
            false,
        )
        .await
        .map_err(ClientError::Transport)?;

        tracing::info!("WebSocket connected");

        let (mut write, mut read) = ws_stream.split();

        let mut phase = Phase::Authenticating;
        self.set_state(ConnectionState::Authenticating);

        loop {
            let msg = match read.next().await {
                Some(Ok(m)) => m,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "WebSocket transport error");
                    return Err(ClientError::Transport(e));
                }
                None => {
                    tracing::warn!("WebSocket stream closed by server");
                    return Err(ClientError::Transport(
                        tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                    ));
                }
            };

            let bytes = match msg {
                Message::Text(text) => text.into_bytes(),
                Message::Binary(b) => b,
                Message::Close(_) => {
                    tracing::info!("received WS Close frame");
                    return Err(ClientError::Transport(
                        tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                    ));
                }
                _ => continue,
            };

            let inbound = match serde_json::from_slice::<InboundMsg>(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse inbound message; skipping");
                    continue;
                }
            };

            phase = self.handle_message(inbound, phase, &mut write).await?;
        }
    }

    /// Handle a single inbound message, drive the FSM, return the new phase.
    ///
    /// `pub(crate)` so tests can drive the FSM directly without a live
    /// WebSocket connection.  `Phase` is also `pub(crate)`.
    ///
    /// Takes ownership of `phase` to avoid borrow conflicts when destructuring
    /// enum variants, and returns the (possibly new) phase.
    #[allow(clippy::result_large_err)]
    pub(crate) async fn handle_message<S>(
        &mut self,
        msg: InboundMsg,
        phase: Phase,
        write: &mut S,
    ) -> Result<Phase, ClientError>
    where
        S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        match (phase, msg) {
            // ── Authenticating ───────────────────────────────────────────────

            // HA sends auth_required immediately on connection.
            // SECURITY: token is exposed via expose_token() once, serialized
            // into JSON in a single expression, and written directly to the
            // sink.  The resulting String is never bound to a named variable
            // or passed to any tracing macro.
            (Phase::Authenticating, InboundMsg::AuthRequired(_)) => {
                write
                    .send(Message::Text(serde_json::to_string(&OutboundMsg::Auth(
                        AuthPayload {
                            access_token: self.config.expose_token().to_owned(),
                        },
                    ))?))
                    .await
                    .map_err(ClientError::Transport)?;
                tracing::info!("auth frame sent");
                Ok(Phase::Authenticating)
            }

            // auth_ok → send subscribe_events (subscribe-all), advance to
            // Subscribing.
            //
            // TASK-049: `event_type` is `None`, which serializes the field
            // OUT of the JSON.  HA's WS API documents this as "subscribe to
            // all events on the bus".  Internal dispatch by `EventVariant`
            // (in `Phase::Live` and during snapshot-buffer replay) routes
            // `state_changed` to the entity store, `service_registered` /
            // `service_removed` to the `ServiceRegistry`, and everything else
            // is ignored.  This keeps the single-ACK gate and avoids an FSM
            // phase explosion that three separate filtered subscriptions
            // would have required.
            (Phase::Authenticating, InboundMsg::AuthOk(_)) => {
                self.set_state(ConnectionState::Subscribing);
                let subscribe_id = self.next_id();
                write
                    .send(Message::Text(serde_json::to_string(
                        &OutboundMsg::SubscribeEvents(SubscribeEventsPayload {
                            id: subscribe_id,
                            event_type: None,
                        }),
                    )?))
                    .await
                    .map_err(ClientError::Transport)?;
                tracing::info!(id = subscribe_id, "subscribe_events sent (all events)");
                Ok(Phase::Subscribing { subscribe_id })
            }

            // auth_invalid → Failed (no reconnect; token plaintext not logged).
            (Phase::Authenticating, InboundMsg::AuthInvalid(p)) => {
                tracing::error!("auth_invalid received; transitioning to Failed");
                // Clear any stability tracking before terminating the connection
                // so that a subsequent client reuse (new run()) cannot inherit
                // stale `stable_live_since` from a prior session.
                self.on_disconnect();
                self.set_state(ConnectionState::Failed);
                Err(ClientError::AuthInvalid { reason: p.message })
            }

            // ── Subscribing ──────────────────────────────────────────────────

            // Await the result ACK for subscribe_events.
            (Phase::Subscribing { subscribe_id }, InboundMsg::Result(result))
                if result.id == subscribe_id =>
            {
                if !result.success {
                    let reason = result
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "subscribe_events failed".to_owned());
                    tracing::error!(%reason, "subscribe_events result: failure");
                    self.on_disconnect();
                    self.set_state(ConnectionState::Failed);
                    return Err(ClientError::AuthInvalid { reason });
                }
                tracing::info!(id = subscribe_id, "subscribe_events ACK received");
                self.set_state(ConnectionState::Snapshotting);

                // Gate: ACK received — NOW issue get_states.
                let get_states_id = self.next_id();
                write
                    .send(Message::Text(serde_json::to_string(
                        &OutboundMsg::GetStates(GetStatesPayload { id: get_states_id }),
                    )?))
                    .await
                    .map_err(ClientError::Transport)?;
                tracing::info!(id = get_states_id, "get_states sent");
                Ok(Phase::Snapshotting {
                    get_states_id,
                    event_buffer: Vec::new(),
                })
            }

            // ── Snapshotting: buffer state_changed events ────────────────────
            (
                Phase::Snapshotting {
                    get_states_id,
                    mut event_buffer,
                },
                InboundMsg::Event(event_payload),
            ) => {
                if event_buffer.len() >= DEFAULT_PROFILE.snapshot_buffer_events {
                    tracing::warn!(
                        cap = DEFAULT_PROFILE.snapshot_buffer_events,
                        "snapshot event buffer overflow; dropping connection"
                    );
                    let tripped = self.overflow_breaker.record_overflow();
                    if tripped {
                        tracing::error!("overflow circuit-breaker tripped (3 overflows in 60 s)");
                        self.on_disconnect();
                        self.set_state(ConnectionState::Failed);
                        return Err(ClientError::OverflowCircuitBreaker);
                    }
                    return Err(ClientError::Transport(
                        tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                    ));
                }
                event_buffer.push(InboundMsg::Event(event_payload));
                Ok(Phase::Snapshotting {
                    get_states_id,
                    event_buffer,
                })
            }

            // Snapshot result received → replay buffered events, issue get_services.
            (
                Phase::Snapshotting {
                    get_states_id,
                    event_buffer,
                },
                InboundMsg::Result(result),
            ) if result.id == get_states_id => {
                if !result.success {
                    let reason = result
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "get_states failed".to_owned());
                    tracing::error!(%reason, "get_states result: failure");
                    // Mid-Snapshotting failure must NOT advance to Live, so the
                    // backoff counter remains as-is — `on_disconnect` clears
                    // only the stability window, never the backoff state.
                    self.on_disconnect();
                    self.set_state(ConnectionState::Failed);
                    return Err(ClientError::AuthInvalid { reason });
                }
                tracing::info!(
                    id = get_states_id,
                    buffered_events = event_buffer.len(),
                    "get_states snapshot received; replaying buffered events"
                );

                // Apply the snapshot to the attached store (if any).  On
                // reconnect the store may already hold the previous snapshot —
                // we capture it now so we can diff after the swap and only
                // broadcast entities whose `last_updated` changed.
                if let Some(store) = self.store.clone() {
                    let old_snap = store.current_snapshot();
                    let raw_states: Vec<RawEntityState> = match result.result {
                        Some(v) => match serde_json::from_value(v) {
                            Ok(states) => states,
                            Err(e) => {
                                tracing::warn!(error = %e, "get_states result did not deserialize as Vec<RawEntityState>; treating as empty");
                                Vec::new()
                            }
                        },
                        None => Vec::new(),
                    };
                    let entities: Vec<Entity> =
                        raw_states.iter().map(raw_entity_to_entity).collect();
                    store.apply_full_snapshot(entities);

                    // Diff the new snapshot against the previous one (if any)
                    // and broadcast only changed entities.  On a fresh connect
                    // the previous snapshot is empty, so every entity counts
                    // as "new" and is broadcast.
                    Self::diff_and_broadcast(&old_snap, store.as_ref());
                }

                // Replay buffered events into the store (if any).  Buffered
                // events arrived during snapshot fetch; replaying them after
                // the snapshot is applied keeps the store consistent with HA.
                //
                // TASK-049: with the subscribe-all subscription, the buffer
                // can also hold `service_registered` / `service_removed`
                // events.  Apply them best-effort here — note that the
                // `Phase::Services → Live` transition below will overwrite
                // the registry with the authoritative `get_services` reply
                // anyway, so service events arriving DURING the snapshot
                // window are mostly redundant.  We still apply them for
                // completeness so a `service_removed` arriving exactly
                // before the get_services reply is not silently lost.
                for buffered in event_buffer {
                    if let InboundMsg::Event(ev) = buffered {
                        if let Some(update) = event_to_entity_update(&ev) {
                            if let Some(store) = self.store.as_ref() {
                                store.apply_changed_event(update);
                            }
                        } else {
                            // Service-lifecycle or unknown — best-effort
                            // dispatch.  Ignored if the variant is `Other`.
                            apply_service_lifecycle_event(&self.services, &ev);
                        }
                    }
                }

                self.set_state(ConnectionState::Services);
                let get_services_id = self.next_id();
                write
                    .send(Message::Text(serde_json::to_string(
                        &OutboundMsg::GetServices(GetServicesPayload {
                            id: get_services_id,
                        }),
                    )?))
                    .await
                    .map_err(ClientError::Transport)?;
                tracing::info!(id = get_services_id, "get_services sent");
                Ok(Phase::Services { get_services_id })
            }

            // ── Services ────────────────────────────────────────────────────
            (Phase::Services { get_services_id }, InboundMsg::Result(result))
                if result.id == get_services_id =>
            {
                if result.success {
                    // BLOCKER 3 fix (TASK-044): parse the result map into a
                    // ServiceRegistry instead of just logging the reply.  The
                    // pre-fix code dropped the payload on the floor, leaving
                    // Phase 3's dispatcher with an empty registry.  Parse
                    // failures (malformed shape) are surfaced as a warn-log
                    // and the registry is left empty — we still advance to
                    // `Live` so a quirky HA build can't keep the UI offline.
                    match result.result {
                        Some(ref value) => match ServiceRegistry::from_get_services_result(value) {
                            Ok(registry) => {
                                tracing::info!(
                                    id = get_services_id,
                                    "get_services reply received; ServiceRegistry populated"
                                );
                                // Acquire the registry write-lock and bulk-replace
                                // the inner `ServiceRegistry`.  The lock is held
                                // only for the duration of this single assignment
                                // — readers in other tasks (e.g. the bridge / a
                                // Phase 3 dispatcher) may briefly observe an empty
                                // registry between handshake start and this point,
                                // but never a torn registry.
                                //
                                // `expect` (not `unwrap`) so a poisoned lock
                                // surfaces a clear panic message rather than the
                                // generic "called Result::unwrap on Err".
                                *self
                                    .services
                                    .write()
                                    .expect("ServiceRegistry RwLock poisoned") = registry;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    id = get_services_id,
                                    error = %e,
                                    "get_services result did not parse as ServiceRegistry; \
                                     proceeding to Live with empty registry"
                                );
                            }
                        },
                        None => {
                            tracing::warn!(
                                id = get_services_id,
                                "get_services reply had no `result` field; \
                                 proceeding to Live with empty registry"
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        id = get_services_id,
                        "get_services failed; proceeding to Live anyway"
                    );
                }
                self.set_state(ConnectionState::Live);
                tracing::info!("FSM reached Live");
                self.on_live_entered();
                Ok(Phase::Live)
            }

            // ── Live ────────────────────────────────────────────────────────

            // Mid-session auth_required → treat as transport disconnect.
            (Phase::Live, InboundMsg::AuthRequired(_)) => {
                tracing::warn!(
                    "auth_required received in Live state; \
                     treating as transport disconnect → Reconnecting"
                );
                self.on_disconnect();
                self.set_state(ConnectionState::Reconnecting);
                Err(ClientError::Transport(
                    tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                ))
            }

            // Live events: route to the attached store (state_changed) or to
            // the shared `ServiceRegistry` (service_registered /
            // service_removed), and update the stable-Live tracker so the
            // backoff counter can reset.
            //
            // TASK-049: dispatch by `EventVariant` — `state_changed` flows to
            // the entity store as before; service-lifecycle events flow into
            // the shared `services_handle` so the LiveStore-side
            // `services_lookup` accessor returns fresh state mid-session
            // without waiting for a reconnect.  Unknown event types
            // (`EventVariant::Other`) are silently ignored — the
            // subscribe-all strategy means HA may emit events the client
            // doesn't care about, and the FSM should not crash on them.
            (Phase::Live, InboundMsg::Event(event)) => {
                if let Some(update) = event_to_entity_update(&event) {
                    if let Some(store) = self.store.as_ref() {
                        store.apply_changed_event(update);
                    }
                } else {
                    // Not a state_changed — try service-lifecycle dispatch.
                    // Ignored if the variant is `Other` or any future type.
                    apply_service_lifecycle_event(&self.services, &event);
                }
                self.on_live_event();
                Ok(Phase::Live)
            }

            // ── Mid-session auth_invalid (any non-Authenticating phase) ─────
            //
            // BLOCKER 2 fix (TASK-044): codex's post-shipment audit flagged that
            // `auth_invalid` was only handled in `Phase::Authenticating`.  In any
            // other phase the message would fall through to the catch-all skip
            // arm at the bottom of this match and be silently ignored.  HA can
            // emit `auth_invalid` mid-session if the access token is revoked
            // while the connection is open; treating that as "no-op" left the
            // FSM stuck in `Live` with a server that has already cut auth.
            //
            // Semantics: identical to the `Authenticating` branch — clear stable
            // tracking, transition to `Failed`, return `ClientError::AuthInvalid`
            // (which the reconnect loop in `lib.rs::run_ws_client` interprets as
            // "do NOT reconnect").  The token plaintext is never logged; only
            // the human-readable message returned by HA is surfaced.
            //
            // Distinct from mid-session `auth_required` (handled above for
            // `Phase::Live`), which TASK-029 correctly treats as a transport
            // disconnect → `Reconnecting`.
            (_phase, InboundMsg::AuthInvalid(p)) => {
                tracing::error!(
                    "auth_invalid received mid-session; transitioning to Failed (no reconnect)"
                );
                self.on_disconnect();
                self.set_state(ConnectionState::Failed);
                Err(ClientError::AuthInvalid { reason: p.message })
            }

            // ── Result correlation (any phase) ───────────────────────────────
            (phase, InboundMsg::Result(result)) => {
                let id = result.id;
                let value = if result.success {
                    Ok(result.result.unwrap_or(serde_json::Value::Null))
                } else {
                    Err(result
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "unknown error".to_owned()))
                };
                if let Err(e) = self.resolve_pending(id, value) {
                    tracing::warn!(error = %e, "unmatched result id");
                }
                Ok(phase)
            }

            // Unknown messages are silently ignored.
            (phase, InboundMsg::Unknown { type_str, .. }) => {
                tracing::debug!(type_str = %type_str, "ignoring unknown message type");
                Ok(phase)
            }

            // Any other combination: skip.
            (phase, _other) => Ok(phase),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::status;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use tracing_test::traced_test;

    // -----------------------------------------------------------------------
    // In-process mock sink
    // -----------------------------------------------------------------------

    struct MockSink {
        sent: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    impl MockSink {
        fn new() -> (Self, Arc<tokio::sync::Mutex<Vec<String>>>) {
            let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            (MockSink { sent: sent.clone() }, sent)
        }
    }

    impl futures::Sink<Message> for MockSink {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            if let Message::Text(text) = item {
                // Use block_on since start_send is a synchronous trait method.
                futures::executor::block_on(async {
                    self.sent.lock().await.push(text);
                });
            }
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    // -----------------------------------------------------------------------
    // Mock SnapshotApplier for diff tests
    // -----------------------------------------------------------------------

    /// Test double for `SnapshotApplier` that records which entity IDs had
    /// `apply_changed_event` called, and holds an in-memory snapshot.
    struct MockStore {
        snapshot: Mutex<Arc<HashMap<EntityId, Entity>>>,
        events: Mutex<Vec<EntityUpdate>>,
    }

    impl MockStore {
        fn new() -> Self {
            MockStore {
                snapshot: Mutex::new(Arc::new(HashMap::new())),
                events: Mutex::new(Vec::new()),
            }
        }

        fn recorded_events(&self) -> Vec<EntityUpdate> {
            self.events.lock().unwrap().clone()
        }
    }

    impl SnapshotApplier for MockStore {
        fn current_snapshot(&self) -> Arc<HashMap<EntityId, Entity>> {
            Arc::clone(&self.snapshot.lock().unwrap())
        }

        fn apply_full_snapshot(&self, entities: Vec<Entity>) {
            let new_map: HashMap<EntityId, Entity> =
                entities.into_iter().map(|e| (e.id.clone(), e)).collect();
            *self.snapshot.lock().unwrap() = Arc::new(new_map);
        }

        fn apply_changed_event(&self, update: EntityUpdate) {
            self.events.lock().unwrap().push(update);
        }
    }

    // -----------------------------------------------------------------------
    // Env serialization mutex
    // -----------------------------------------------------------------------

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // -----------------------------------------------------------------------
    // Helper: build a WsClient from env
    //
    // The caller MUST acquire `ENV_LOCK` before calling and drop it BEFORE
    // any subsequent `.await` to avoid `clippy::await_holding_lock`.
    // Pattern:
    //   let (mut client, rx) = {
    //       let _g = ENV_LOCK.lock().unwrap();
    //       make_client("tok")
    //   }; // guard dropped here
    // -----------------------------------------------------------------------

    fn make_client(token: &str) -> (WsClient, watch::Receiver<ConnectionState>) {
        unsafe {
            std::env::set_var("HA_URL", "ws://ha.local:8123/api/websocket");
            std::env::set_var("HA_TOKEN", token);
        }
        let config = Config::from_env().expect("test config");
        let (tx, rx) = status::channel();
        let client = WsClient::new(config, tx);
        (client, rx)
    }

    fn inbound(json: &str) -> InboundMsg {
        serde_json::from_str(json).unwrap_or_else(|e| panic!("bad test JSON: {e}: {json}"))
    }

    /// Build a minimal entity with the given `last_updated` timestamp string.
    fn make_entity_ts(id: &str, state: &str, last_updated: &str) -> crate::ha::entity::Entity {
        use jiff::Timestamp;
        use serde_json::Map;
        use std::sync::Arc;
        crate::ha::entity::Entity {
            id: EntityId::from(id),
            state: Arc::from(state),
            attributes: Arc::new(Map::new()),
            last_changed: Timestamp::UNIX_EPOCH,
            last_updated: last_updated
                .parse::<Timestamp>()
                .unwrap_or(Timestamp::UNIX_EPOCH),
        }
    }

    // -----------------------------------------------------------------------
    // Test: happy path auth_ok → Live
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_ok_happy_path_reaches_live() {
        // Guard dropped before first .await — avoids clippy::await_holding_lock.
        let (mut client, state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-happy")
        };
        let (mut sink, sent) = MockSink::new();

        let messages = vec![
            inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
            inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
            // subscribe_events ACK (id=1)
            inbound(r#"{"type":"result","id":1,"success":true,"result":null}"#),
            // get_states result (id=2)
            inbound(r#"{"type":"result","id":2,"success":true,"result":[]}"#),
            // get_services result (id=3)
            inbound(r#"{"type":"result","id":3,"success":true,"result":{}}"#),
        ];

        let mut phase = Phase::Authenticating;
        for msg in messages {
            phase = client
                .handle_message(msg, phase, &mut sink)
                .await
                .expect("message should succeed");
        }

        assert!(
            matches!(phase, Phase::Live),
            "expected Live phase, got: {phase:?}"
        );
        assert_eq!(*state_rx.borrow(), ConnectionState::Live);

        let frames = sent.lock().await;
        assert_eq!(
            frames.len(),
            4,
            "expected 4 outbound frames: auth, subscribe_events, get_states, get_services"
        );

        let auth_f: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(auth_f["type"], "auth");

        let sub_f: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(sub_f["type"], "subscribe_events");
        // TASK-049: subscribe-all — the `event_type` field MUST be absent
        // from the serialized frame (HA treats absence, not `null`, as
        // "all events").  This pins the wire-level guarantee.
        assert!(
            sub_f.get("event_type").is_none(),
            "subscribe_events frame must omit `event_type` (subscribe-all); got: {sub_f}"
        );

        let gs_f: serde_json::Value = serde_json::from_str(&frames[2]).unwrap();
        assert_eq!(gs_f["type"], "get_states");

        let gsvc_f: serde_json::Value = serde_json::from_str(&frames[3]).unwrap();
        assert_eq!(gsvc_f["type"], "get_services");
    }

    // -----------------------------------------------------------------------
    // Test: auth_invalid → Failed, no reconnect
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_invalid_transitions_to_failed_no_reconnect() {
        let (mut client, state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("invalid-token")
        };
        let (mut sink, _sent) = MockSink::new();

        // auth_required → sends auth frame, stays Authenticating.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();

        // auth_invalid → AuthInvalid error.
        let result = client
            .handle_message(
                inbound(r#"{"type":"auth_invalid","message":"Invalid access token"}"#),
                phase,
                &mut sink,
            )
            .await;

        assert!(
            matches!(result, Err(ClientError::AuthInvalid { .. })),
            "expected AuthInvalid; got: {result:?}"
        );
        assert_eq!(
            *state_rx.borrow(),
            ConnectionState::Failed,
            "state must be Failed after auth_invalid"
        );

        // Token must NOT appear in the error reason.
        if let Err(ClientError::AuthInvalid { reason }) = result {
            assert!(
                !reason.contains("invalid-token"),
                "reason must not contain token; got: {reason}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test: mid-session auth_required → Reconnecting
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_mid_session_auth_required_triggers_reconnect() {
        let (mut client, state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-reconnect")
        };
        let (mut sink, _sent) = MockSink::new();

        client.set_state(ConnectionState::Live);

        let result = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Live,
                &mut sink,
            )
            .await;

        assert!(
            matches!(
                result,
                Err(ClientError::Transport(
                    tokio_tungstenite::tungstenite::Error::ConnectionClosed
                ))
            ),
            "expected ConnectionClosed for mid-session auth_required; got: {result:?}"
        );
        assert_eq!(
            *state_rx.borrow(),
            ConnectionState::Reconnecting,
            "state must be Reconnecting after mid-session auth_required"
        );
    }

    // -----------------------------------------------------------------------
    // Test: subscribe-ACK gate — get_states NOT sent until ACK received
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_states_not_sent_before_subscribe_ack() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-gate")
        };
        let (mut sink, sent) = MockSink::new();

        // Drive auth_required + auth_ok → Subscribing.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();

        // After auth_ok: 2 frames sent (auth + subscribe_events), no get_states yet.
        {
            let frames = sent.lock().await;
            assert_eq!(frames.len(), 2);
            let sub_f: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
            assert_eq!(sub_f["type"], "subscribe_events");
            let has_get_states = frames.iter().any(|f| {
                serde_json::from_str::<serde_json::Value>(f)
                    .map(|v| v["type"] == "get_states")
                    .unwrap_or(false)
            });
            assert!(
                !has_get_states,
                "get_states must NOT be sent before subscribe_events ACK"
            );
        }

        let subscribe_id = match &phase {
            Phase::Subscribing { subscribe_id } => *subscribe_id,
            other => panic!("expected Subscribing phase; got: {other:?}"),
        };

        // Now send the subscribe ACK.
        let _phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();

        // After ACK: get_states must now be present.
        {
            let frames = sent.lock().await;
            let has_get_states = frames.iter().any(|f| {
                serde_json::from_str::<serde_json::Value>(f)
                    .map(|v| v["type"] == "get_states")
                    .unwrap_or(false)
            });
            assert!(
                has_get_states,
                "get_states must be sent after subscribe_events ACK"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test: ring overflow → drop + counter increment (first overflow, no trip)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ring_overflow_triggers_drop_and_increments_counter() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-overflow")
        };
        let (mut sink, _sent) = MockSink::new();

        // Pre-fill buffer to exactly the capacity limit.
        let event_buffer: Vec<InboundMsg> = (0..DEFAULT_PROFILE.snapshot_buffer_events)
            .map(|_| {
                inbound(r#"{"type":"event","id":1,"event":{"event_type":"state_changed","data":{"entity_id":"light.x","new_state":null,"old_state":null},"origin":"LOCAL","time_fired":"2024-01-01T00:00:00+00:00"}}"#)
            })
            .collect();

        let phase = Phase::Snapshotting {
            get_states_id: 42,
            event_buffer,
        };

        let extra_event = inbound(
            r#"{"type":"event","id":1,"event":{"event_type":"state_changed","data":{"entity_id":"light.extra","new_state":null,"old_state":null},"origin":"LOCAL","time_fired":"2024-01-01T00:00:01+00:00"}}"#,
        );

        let result = client.handle_message(extra_event, phase, &mut sink).await;

        assert!(
            matches!(
                result,
                Err(ClientError::Transport(
                    tokio_tungstenite::tungstenite::Error::ConnectionClosed
                ))
            ),
            "first overflow must trigger a connection-drop; got: {result:?}"
        );
        assert_eq!(
            client.overflow_breaker.recent.len(),
            1,
            "overflow counter must be 1 after first overflow"
        );
    }

    // -----------------------------------------------------------------------
    // Test: 3 overflows within 60s → circuit-breaker trips → Failed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_three_overflows_trip_circuit_breaker() {
        let (mut client, state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-cb")
        };
        let (mut sink, _sent) = MockSink::new();

        // Pre-record 2 overflows so the next one trips the breaker.
        client.overflow_breaker.record_overflow();
        client.overflow_breaker.record_overflow();

        // Trigger a 3rd overflow via handle_message.
        let event_buffer: Vec<InboundMsg> = (0..DEFAULT_PROFILE.snapshot_buffer_events)
            .map(|_| {
                inbound(r#"{"type":"event","id":1,"event":{"event_type":"state_changed","data":{"entity_id":"light.x","new_state":null,"old_state":null},"origin":"LOCAL","time_fired":"2024-01-01T00:00:00+00:00"}}"#)
            })
            .collect();
        let phase = Phase::Snapshotting {
            get_states_id: 10,
            event_buffer,
        };
        let extra_event = inbound(
            r#"{"type":"event","id":1,"event":{"event_type":"state_changed","data":{"entity_id":"light.extra","new_state":null,"old_state":null},"origin":"LOCAL","time_fired":"2024-01-01T00:00:01+00:00"}}"#,
        );

        let result = client.handle_message(extra_event, phase, &mut sink).await;

        assert!(
            matches!(result, Err(ClientError::OverflowCircuitBreaker)),
            "3rd overflow must trip circuit-breaker; got: {result:?}"
        );
        assert_eq!(
            *state_rx.borrow(),
            ConnectionState::Failed,
            "state must be Failed after circuit-breaker"
        );
    }

    // -----------------------------------------------------------------------
    // Test: ID correlation — 3 concurrent requests, out-of-order replies
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_id_correlation_out_of_order_resolves_correctly() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-id-correlation")
        };

        let rx1 = client.register_pending(10);
        let rx2 = client.register_pending(20);
        let rx3 = client.register_pending(30);

        // Resolve out of order: 30, 10, 20.
        client
            .resolve_pending(30, Ok(serde_json::json!("reply-30")))
            .unwrap();
        client
            .resolve_pending(10, Ok(serde_json::json!("reply-10")))
            .unwrap();
        client
            .resolve_pending(20, Ok(serde_json::json!("reply-20")))
            .unwrap();

        assert_eq!(rx1.await.unwrap().unwrap(), serde_json::json!("reply-10"));
        assert_eq!(rx2.await.unwrap().unwrap(), serde_json::json!("reply-20"));
        assert_eq!(rx3.await.unwrap().unwrap(), serde_json::json!("reply-30"));
    }

    // -----------------------------------------------------------------------
    // Test: ID mismatch returns IdMismatch error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_id_mismatch_returns_error() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-mismatch")
        };

        let result = client.resolve_pending(999, Ok(serde_json::Value::Null));
        assert!(
            matches!(result, Err(ClientError::IdMismatch { received: 999 })),
            "unmatched id must produce IdMismatch; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Test: token not leaked to trace after full auth handshake
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[traced_test]
    async fn test_token_not_leaked_to_trace_after_auth_handshake() {
        // Guard dropped before first .await — avoids clippy::await_holding_lock.
        let (mut client, _sink_sent) = {
            let _g = ENV_LOCK.lock().unwrap();
            let plaintext_token = "UNIQUE_PLAINTEXT_TOKEN_XYZ987ABC";
            unsafe {
                std::env::set_var("HA_URL", "ws://ha.local:8123/api/websocket");
                std::env::set_var("HA_TOKEN", plaintext_token);
            }
            let config = Config::from_env().unwrap();
            let (tx, _rx) = status::channel();
            (WsClient::new(config, tx), MockSink::new())
        };
        let plaintext_token = "UNIQUE_PLAINTEXT_TOKEN_XYZ987ABC";
        let (mut sink, _sent) = _sink_sent;

        // Drive auth_required → auth_ok to exercise the token exposure path.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let _phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();

        // No trace event must contain the plaintext token.
        assert!(
            !logs_contain(plaintext_token),
            "plaintext token must not appear in any trace event"
        );
    }

    // -----------------------------------------------------------------------
    // Test: OverflowBreaker evicts stale events and resets
    // -----------------------------------------------------------------------

    #[test]
    fn test_overflow_breaker_evicts_old_events() {
        let mut breaker = OverflowBreaker::new();
        // Inject one stale event (>60s ago).
        breaker
            .recent
            .push(Instant::now() - Duration::from_secs(61));
        // Two more fresh overflows: after stale eviction we start from 1.
        assert!(!breaker.record_overflow()); // evicts stale; now 1
        assert!(!breaker.record_overflow()); // 2
        assert!(breaker.record_overflow()); // 3 → trip
    }

    #[test]
    fn test_overflow_breaker_does_not_trip_if_overflows_stale() {
        let mut breaker = OverflowBreaker::new();
        breaker
            .recent
            .push(Instant::now() - Duration::from_secs(62));
        breaker
            .recent
            .push(Instant::now() - Duration::from_secs(61));
        // One fresh overflow: stale events are evicted, only 1 remains → no trip.
        let tripped = breaker.record_overflow();
        assert!(!tripped);
        assert_eq!(breaker.recent.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Tests: BackoffState — unit coverage for the counter and window logic
    // -----------------------------------------------------------------------

    #[test]
    fn backoff_state_initial_window_is_min() {
        let bs = BackoffState::new();
        assert_eq!(bs.attempts, 0);
        assert_eq!(bs.current_window, MIN_BACKOFF);
    }

    #[test]
    fn backoff_state_first_advance_returns_min() {
        // First advance: attempts was 0, window stays at MIN_BACKOFF.
        let mut bs = BackoffState::new();
        let w = bs.advance();
        assert_eq!(w, MIN_BACKOFF, "first advance must return MIN_BACKOFF");
        assert_eq!(bs.attempts, 1);
    }

    #[test]
    fn backoff_state_second_advance_doubles_window() {
        let mut bs = BackoffState::new();
        bs.advance(); // 1s, attempts=1
        let w = bs.advance(); // 2s, attempts=2
        assert_eq!(w, MIN_BACKOFF * 2, "second advance must double window");
        assert_eq!(bs.attempts, 2);
    }

    #[test]
    fn backoff_state_saturates_at_max() {
        let mut bs = BackoffState::new();
        // Advance enough times to saturate at MAX_BACKOFF.
        for _ in 0..10 {
            bs.advance();
        }
        assert_eq!(
            bs.current_window, MAX_BACKOFF,
            "window must cap at MAX_BACKOFF"
        );
    }

    #[test]
    fn backoff_state_saturated_window_stays_at_max() {
        let mut bs = BackoffState::new();
        for _ in 0..10 {
            bs.advance();
        }
        let w1 = bs.advance();
        let w2 = bs.advance();
        assert_eq!(w1, MAX_BACKOFF);
        assert_eq!(
            w2, MAX_BACKOFF,
            "window must not exceed MAX_BACKOFF after saturation"
        );
    }

    #[test]
    fn backoff_state_reset_restores_initial() {
        let mut bs = BackoffState::new();
        bs.advance();
        bs.advance();
        bs.advance();
        bs.reset();
        assert_eq!(bs.attempts, 0);
        assert_eq!(bs.current_window, MIN_BACKOFF);
    }

    // -----------------------------------------------------------------------
    // Test (a): three successive Snapshotting failures → monotonically
    // increasing backoff window; counter does NOT reset on connect/auth steps.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_a_snapshotting_failures_produce_monotonically_increasing_backoff() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-backoff-a")
        };
        let (mut sink, _sent) = MockSink::new();

        // Helper: drive through auth + subscribe + receive snapshot failure.
        // This simulates the server closing connection right after sending
        // get_states reply with success=false (or a transport close mid-snapshot).
        //
        // We simulate "fails during Snapshotting" by driving to Snapshotting
        // and then returning a transport error — which advances the backoff.
        //
        // The key invariant being tested: the backoff counter increments each
        // time we fail in Snapshotting, and the window grows monotonically.
        // The counter does NOT reset just because we reached Subscribing or
        // Snapshotting again — it only resets after stable Live (5s + events).

        let mut windows: Vec<Duration> = Vec::new();

        for _ in 0..3 {
            // Advance backoff BEFORE the reconnect attempt (simulating what the
            // reconnect loop does on each failed attempt), then record the
            // resulting window (the delay used for THIS attempt).
            let window = client.backoff.advance();

            // Drive auth/subscribe portion (does NOT reset backoff).
            let phase = client
                .handle_message(
                    inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                    Phase::Authenticating,
                    &mut sink,
                )
                .await
                .unwrap();
            let phase = client
                .handle_message(
                    inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                    phase,
                    &mut sink,
                )
                .await
                .unwrap();

            // Get subscribe_id so we can ACK it.
            let subscribe_id = match &phase {
                Phase::Subscribing { subscribe_id } => *subscribe_id,
                other => panic!("expected Subscribing; got: {other:?}"),
            };

            let phase = client
                .handle_message(
                    inbound(&format!(
                        r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                    )),
                    phase,
                    &mut sink,
                )
                .await
                .unwrap();

            // Verify we're in Snapshotting.
            assert!(
                matches!(phase, Phase::Snapshotting { .. }),
                "expected Snapshotting; got: {phase:?}"
            );

            // Simulate server closing connection mid-Snapshotting (before
            // get_states reply) — we expect no backoff reset.
            let _ = phase;

            // Simulate disconnect: on_disconnect() clears stable_live_since.
            client.on_disconnect();

            windows.push(window);
        }

        // The advance() sequence: attempts 0→1 (returns MIN_BACKOFF=1s),
        //                         attempts 1→2 (returns 2s),
        //                         attempts 2→3 (returns 4s).
        // Windows must be monotonically increasing: 1s < 2s < 4s.
        assert_eq!(
            windows[0], MIN_BACKOFF,
            "first window must be MIN_BACKOFF (1s)"
        );
        assert!(
            windows[1] > windows[0],
            "second window must be larger than first; got {:?} vs {:?}",
            windows[1],
            windows[0]
        );
        assert!(
            windows[2] > windows[1],
            "third window must be larger than second; got {:?} vs {:?}",
            windows[2],
            windows[1]
        );
        assert_eq!(
            windows[1],
            Duration::from_secs(2),
            "second window must be 2s"
        );
        assert_eq!(
            windows[2],
            Duration::from_secs(4),
            "third window must be 4s"
        );

        // Backoff counter must be 3, not reset.
        assert_eq!(
            client.backoff.attempts, 3,
            "backoff counter must be 3 after 3 Snapshotting failures"
        );
    }

    // -----------------------------------------------------------------------
    // Test (b): stable Live for ≥ threshold → backoff resets; next reconnect
    // starts from MIN_BACKOFF.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_b_stable_live_resets_backoff_counter() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-backoff-b")
        };
        let (mut sink, _sent) = MockSink::new();

        // Simulate 3 failed attempts so backoff is elevated.
        client.backoff.advance();
        client.backoff.advance();
        client.backoff.advance();
        assert_eq!(client.backoff.attempts, 3);
        assert!(client.backoff.current_window > MIN_BACKOFF);

        // Drive FSM to Live.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let subscribe_id = match &phase {
            Phase::Subscribing { subscribe_id } => *subscribe_id,
            other => panic!("expected Subscribing; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_states_id = match &phase {
            Phase::Snapshotting { get_states_id, .. } => *get_states_id,
            other => panic!("expected Snapshotting; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_states_id},"success":true,"result":[]}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_services_id = match &phase {
            Phase::Services { get_services_id } => *get_services_id,
            other => panic!("expected Services; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_services_id},"success":true,"result":{{}}}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();

        assert!(
            matches!(phase, Phase::Live),
            "expected Live; got: {phase:?}"
        );
        assert!(
            client.stable_live_since.is_some(),
            "stable_live_since must be set on Live entry"
        );

        // Inject a short threshold so we don't wait 5 real seconds.
        client.stable_live_threshold = Duration::from_millis(1);

        // Advance clock past threshold via a tiny sleep.
        tokio::time::sleep(Duration::from_millis(5)).await;

        // Deliver a Live event — this triggers the stability check.
        let _phase = client
            .handle_message(
                inbound(r#"{"type":"event","id":1,"event":{"event_type":"state_changed","data":{"entity_id":"light.x","new_state":null,"old_state":null},"origin":"LOCAL","time_fired":"2024-01-01T00:00:00+00:00"}}"#),
                Phase::Live,
                &mut sink,
            )
            .await
            .unwrap();

        // Backoff must now be reset.
        assert_eq!(
            client.backoff.attempts, 0,
            "backoff.attempts must be 0 after stable-Live reset"
        );
        assert_eq!(
            client.backoff.current_window, MIN_BACKOFF,
            "backoff.current_window must return to MIN_BACKOFF after stable-Live reset"
        );
    }

    // -----------------------------------------------------------------------
    // Test (b1): elapsed < threshold + events flowing → counter does NOT reset.
    //
    // Codex finding 8 (TASK-046): the existing happy-path test proves that
    // backoff resets after both `elapsed >= threshold` AND `live_event_count > 0`
    // are satisfied, but does not lock down the AND-conjunction.  This sub-test
    // closes the (b1) hole: when events flow but the stability window has not
    // yet elapsed, the backoff state must remain untouched.
    //
    // Strategy: set `stable_live_threshold` to a very long value (1 hour) so
    // the test's real wall-clock time cannot satisfy the elapsed-since arm.
    // Drive the FSM to Live, deliver one Live state_changed event (which
    // increments `live_event_count` and triggers `check_and_maybe_reset_backoff`),
    // then assert the backoff state is byte-for-byte unchanged.  Forcing a
    // disconnect at the end mirrors the spec's "force disconnect, assert
    // backoff window did NOT reset" formulation; the disconnect is observable
    // via `on_disconnect` not touching the backoff counter (only the stability
    // window is cleared — see `WsClient::on_disconnect`).
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_b1_stable_live_does_not_reset_before_threshold() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-backoff-b1")
        };
        let (mut sink, _sent) = MockSink::new();

        // Simulate 3 failed attempts so the backoff counter is elevated.
        client.backoff.advance();
        client.backoff.advance();
        client.backoff.advance();
        let attempts_before = client.backoff.attempts;
        let window_before = client.backoff.current_window;
        assert_eq!(attempts_before, 3);
        assert!(window_before > MIN_BACKOFF);

        // Drive FSM to Live (mirrors test_b setup).
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let subscribe_id = match &phase {
            Phase::Subscribing { subscribe_id } => *subscribe_id,
            other => panic!("expected Subscribing; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_states_id = match &phase {
            Phase::Snapshotting { get_states_id, .. } => *get_states_id,
            other => panic!("expected Snapshotting; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_states_id},"success":true,"result":[]}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_services_id = match &phase {
            Phase::Services { get_services_id } => *get_services_id,
            other => panic!("expected Services; got: {other:?}"),
        };
        let _phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_services_id},"success":true,"result":{{}}}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        assert!(
            client.stable_live_since.is_some(),
            "stable_live_since must be set on Live entry"
        );

        // Inject a 1-hour threshold — the rest of the test takes well under a
        // second, so the elapsed-since arm of the AND can never be satisfied.
        client.stable_live_threshold = Duration::from_secs(3600);

        // Deliver a Live event — this increments live_event_count and runs
        // `check_and_maybe_reset_backoff`.  With elapsed << threshold, the
        // function must short-circuit without resetting.
        let _phase = client
            .handle_message(
                inbound(r#"{"type":"event","id":1,"event":{"event_type":"state_changed","data":{"entity_id":"light.x","new_state":null,"old_state":null},"origin":"LOCAL","time_fired":"2024-01-01T00:00:00+00:00"}}"#),
                Phase::Live,
                &mut sink,
            )
            .await
            .unwrap();
        assert!(
            client.live_event_count >= 1,
            "live_event_count must reflect the event delivery; got: {}",
            client.live_event_count
        );

        // Backoff state must be unchanged BEFORE the disconnect step — this
        // is the load-bearing assertion (closes the codex-review observation
        // that b1 could pass vacuously if `on_disconnect` ever started
        // resetting backoff itself).  The Live event handler invokes
        // `check_and_maybe_reset_backoff`; with elapsed << threshold, that
        // function must return false without mutating backoff state.
        assert_eq!(
            client.backoff.attempts, attempts_before,
            "backoff.attempts must NOT change after a Live event when elapsed < threshold"
        );
        assert_eq!(
            client.backoff.current_window, window_before,
            "backoff.current_window must NOT change after a Live event when elapsed < threshold"
        );

        // Mirror the spec's "force disconnect" step.  on_disconnect clears
        // the stability window but MUST NOT touch the backoff counter; the
        // pre-disconnect assertion above already proved the event-handler
        // didn't reset, so any post-disconnect change here would be
        // attributable to on_disconnect itself.
        client.on_disconnect();
        assert_eq!(
            client.backoff.attempts, attempts_before,
            "backoff.attempts must NOT change after on_disconnect either"
        );
        assert_eq!(
            client.backoff.current_window, window_before,
            "backoff.current_window must NOT change after on_disconnect either"
        );
    }

    // -----------------------------------------------------------------------
    // Test (b2): elapsed >= threshold + NO events → counter does NOT reset.
    //
    // Codex finding 8 (TASK-046) sibling: closes the second AND-conjunction
    // hole.  Even when the elapsed-since-Live window has comfortably exceeded
    // the threshold, the absence of any Live event must keep the counter
    // intact.  Phase 2 design intent (see `WsClient::check_and_maybe_reset_backoff`):
    // the stability invariant is "this connection produced at least one
    // payload" — a connection that goes idle for 5 s without ever delivering
    // a state_changed event has not earned a reset.
    //
    // Strategy: set `stable_live_threshold` to 1 ms so the elapsed arm is
    // trivially satisfied.  Enter Live, sleep past the threshold, deliver
    // ZERO events, then call `check_and_maybe_reset_backoff` directly to
    // exercise the stability check without an event.  Force a disconnect to
    // mirror the spec wording, then assert the backoff state is unchanged.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_b2_stable_live_does_not_reset_without_events() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-backoff-b2")
        };
        let (mut sink, _sent) = MockSink::new();

        // Simulate 3 failed attempts so the backoff counter is elevated.
        client.backoff.advance();
        client.backoff.advance();
        client.backoff.advance();
        let attempts_before = client.backoff.attempts;
        let window_before = client.backoff.current_window;
        assert_eq!(attempts_before, 3);
        assert!(window_before > MIN_BACKOFF);

        // Drive FSM to Live (mirrors test_b setup).
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let subscribe_id = match &phase {
            Phase::Subscribing { subscribe_id } => *subscribe_id,
            other => panic!("expected Subscribing; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_states_id = match &phase {
            Phase::Snapshotting { get_states_id, .. } => *get_states_id,
            other => panic!("expected Snapshotting; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_states_id},"success":true,"result":[]}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_services_id = match &phase {
            Phase::Services { get_services_id } => *get_services_id,
            other => panic!("expected Services; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_services_id},"success":true,"result":{{}}}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        assert!(
            matches!(phase, Phase::Live),
            "expected Live; got: {phase:?}"
        );
        assert!(
            client.stable_live_since.is_some(),
            "stable_live_since must be set on Live entry"
        );
        assert_eq!(
            client.live_event_count, 0,
            "live_event_count must be 0 immediately after Live entry"
        );

        // Threshold of 1 ms; sleep 5 ms to be well past it.
        client.stable_live_threshold = Duration::from_millis(1);
        tokio::time::sleep(Duration::from_millis(5)).await;

        // Deliver NO events.  Call `check_and_maybe_reset_backoff` directly
        // (it is `pub(crate)`) so the stability-AND is exercised in the
        // "elapsed satisfied, events not satisfied" configuration without
        // accidentally triggering the events arm.  Returns `false` when the
        // invariant is not satisfied.
        let did_reset = client.check_and_maybe_reset_backoff();
        assert!(
            !did_reset,
            "check_and_maybe_reset_backoff must NOT report a reset when no events have arrived"
        );

        // Pre-disconnect: backoff must be unchanged after the
        // check_and_maybe_reset_backoff call (this is the load-bearing
        // assertion; closes the codex-review observation that b2 could pass
        // vacuously if `on_disconnect` ever began resetting backoff itself).
        assert_eq!(
            client.backoff.attempts, attempts_before,
            "backoff.attempts must NOT change after a no-event stability check when elapsed >= threshold"
        );
        assert_eq!(
            client.backoff.current_window, window_before,
            "backoff.current_window must NOT change after a no-event stability check when elapsed >= threshold"
        );

        // Mirror the spec's "force disconnect" step.
        client.on_disconnect();
        assert_eq!(
            client.backoff.attempts, attempts_before,
            "backoff.attempts must NOT change after on_disconnect either"
        );
        assert_eq!(
            client.backoff.current_window, window_before,
            "backoff.current_window must NOT change after on_disconnect either"
        );
    }

    // -----------------------------------------------------------------------
    // Test (b) complement: failed Snapshotting does NOT reset backoff.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_b_snapshotting_failure_does_not_reset_backoff() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-backoff-no-reset")
        };
        let (mut sink, _sent) = MockSink::new();

        // Simulate 2 prior failures.
        client.backoff.advance();
        client.backoff.advance();
        let window_before = client.backoff.current_window;
        let attempts_before = client.backoff.attempts;

        // Drive to Snapshotting.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let subscribe_id = match &phase {
            Phase::Subscribing { subscribe_id } => *subscribe_id,
            other => panic!("expected Subscribing; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();

        // Arrived in Snapshotting — simulate failure by calling on_disconnect
        // without ever reaching Live.
        assert!(matches!(phase, Phase::Snapshotting { .. }));
        client.on_disconnect();

        // Backoff state must be unchanged (no reset happened).
        assert_eq!(
            client.backoff.attempts, attempts_before,
            "backoff attempts must NOT change when Snapshotting fails"
        );
        assert_eq!(
            client.backoff.current_window, window_before,
            "backoff window must NOT change when Snapshotting fails"
        );
    }

    // -----------------------------------------------------------------------
    // Test (b) negative path through real handle_message error:
    // a get_states result with success=false in Snapshotting must NOT reset
    // the backoff counter (exercises the Phase::Snapshotting failure branch
    // exactly as a real HA error would).
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_b_snapshotting_get_states_failure_via_handle_message_does_not_reset_backoff() {
        let (mut client, state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-snapshot-fail")
        };
        let (mut sink, _sent) = MockSink::new();

        // Prime the backoff: 2 prior failures → attempts=2, window > MIN.
        client.backoff.advance();
        client.backoff.advance();
        let attempts_before = client.backoff.attempts;
        let window_before = client.backoff.current_window;
        assert!(window_before > MIN_BACKOFF);

        // Drive auth → subscribe ACK → Snapshotting.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let subscribe_id = match &phase {
            Phase::Subscribing { subscribe_id } => *subscribe_id,
            other => panic!("expected Subscribing; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_states_id = match &phase {
            Phase::Snapshotting { get_states_id, .. } => *get_states_id,
            other => panic!("expected Snapshotting; got: {other:?}"),
        };

        // Real failure path: HA returns success=false on get_states.
        // This exercises the same code path a server-side rejection produces.
        let result = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_states_id},"success":false,"error":{{"code":"db_error","message":"db lock"}}}}"#
                )),
                phase,
                &mut sink,
            )
            .await;

        assert!(
            matches!(result, Err(ClientError::AuthInvalid { .. })),
            "get_states success=false must produce a ClientError; got: {result:?}"
        );
        assert_eq!(
            *state_rx.borrow(),
            ConnectionState::Failed,
            "state must be Failed after get_states failure"
        );

        // CRITICAL invariant (Codex I3): a resync that fails mid-Snapshotting
        // does NOT reset the backoff counter. The window must remain elevated.
        assert_eq!(
            client.backoff.attempts, attempts_before,
            "backoff.attempts must NOT reset on mid-Snapshotting failure (real handle_message path)"
        );
        assert_eq!(
            client.backoff.current_window, window_before,
            "backoff.current_window must NOT reset on mid-Snapshotting failure (real handle_message path)"
        );
        // stable_live_since must be cleared so a future Live transition starts fresh.
        assert!(
            client.stable_live_since.is_none(),
            "stable_live_since must be cleared by on_disconnect on Failed transition"
        );
    }

    // -----------------------------------------------------------------------
    // Test: auth_invalid path also clears stable_live_since defensively.
    // Prevents a stale Live timestamp from a prior session leaking into
    // a subsequent run() call's stability tracking.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_invalid_clears_stable_live_since() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-auth-invalid-clear")
        };
        let (mut sink, _sent) = MockSink::new();

        // Inject a stale stable_live_since (as if a prior session reached Live).
        client.stable_live_since = Some(Instant::now());
        client.live_event_count = 7;

        let _ = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let _ = client
            .handle_message(
                inbound(r#"{"type":"auth_invalid","message":"bad token"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await;

        assert!(
            client.stable_live_since.is_none(),
            "auth_invalid path must clear stable_live_since"
        );
        assert_eq!(
            client.live_event_count, 0,
            "auth_invalid path must reset live_event_count"
        );
    }

    // -----------------------------------------------------------------------
    // Test (c): window saturates at MAX_BACKOFF; jitter stays in [0, MAX].
    // -----------------------------------------------------------------------

    #[test]
    fn test_c_backoff_window_saturates_at_max_and_jitter_bounded() {
        let mut bs = BackoffState::new();
        // Drive many failures to saturate.
        for _ in 0..20 {
            bs.advance();
        }
        assert_eq!(
            bs.current_window, MAX_BACKOFF,
            "window must saturate at MAX_BACKOFF (30s)"
        );

        // Verify jitter is bounded in [0, MAX_BACKOFF] using a seeded RNG
        // for determinism (required by "Determinism: seeded RNG" testing rule).
        let mut rng = SmallRng::seed_from_u64(0xDEAD_BEEF_CAFE_1234);
        for _ in 0..1000 {
            let j = full_jitter(bs.current_window, &mut rng);
            assert!(
                j <= MAX_BACKOFF,
                "jitter must not exceed MAX_BACKOFF; got: {j:?}"
            );
        }
    }

    #[test]
    fn test_c_consecutive_saturation_does_not_exceed_max() {
        let mut bs = BackoffState::new();
        for _ in 0..30 {
            let w = bs.advance();
            assert!(
                w <= MAX_BACKOFF,
                "window must never exceed MAX_BACKOFF; got: {w:?} at attempt {}",
                bs.attempts
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test: full_jitter is always in [0, window].
    // -----------------------------------------------------------------------

    #[test]
    fn full_jitter_is_bounded_within_window() {
        let mut rng = SmallRng::seed_from_u64(42);
        let window = Duration::from_secs(10);
        for _ in 0..1000 {
            let j = full_jitter(window, &mut rng);
            assert!(j <= window, "jitter {j:?} exceeds window {window:?}");
        }
    }

    #[test]
    fn full_jitter_zero_window_returns_zero() {
        let mut rng = SmallRng::seed_from_u64(1);
        let j = full_jitter(Duration::ZERO, &mut rng);
        assert_eq!(j, Duration::ZERO);
    }

    // -----------------------------------------------------------------------
    // Test: diff_and_broadcast — only changed last_updated produces events.
    // -----------------------------------------------------------------------

    #[test]
    fn test_diff_broadcast_only_changed_last_updated() {
        // Build an old snapshot with 3 entities.
        // light.a: unchanged (same last_updated)
        // light.b: changed last_updated
        // light.c: removed in new snapshot
        // light.d: new in new snapshot

        let ts_old = "2024-01-01T00:00:00Z";
        let ts_new = "2024-01-01T01:00:00Z";

        let old_entities = vec![
            make_entity_ts("light.a", "on", ts_old),
            make_entity_ts("light.b", "off", ts_old),
            make_entity_ts("light.c", "on", ts_old),
        ];
        let old_snap: Arc<HashMap<EntityId, Entity>> = Arc::new(
            old_entities
                .into_iter()
                .map(|e| (e.id.clone(), e))
                .collect(),
        );

        // New snapshot: light.a unchanged, light.b updated, light.c removed, light.d new.
        let new_entities = vec![
            make_entity_ts("light.a", "on", ts_old), // same ts → no event
            make_entity_ts("light.b", "on", ts_new), // updated ts → event
            make_entity_ts("light.d", "off", ts_new), // new → event
        ];

        let store = MockStore::new();
        store.apply_full_snapshot(new_entities);

        WsClient::diff_and_broadcast(&old_snap, &store);

        let events = store.recorded_events();

        // Collect entity IDs of events.
        let mut changed_ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
        changed_ids.sort();

        assert_eq!(
            changed_ids,
            vec!["light.b", "light.c", "light.d"],
            "only changed/new/removed entities must produce events"
        );

        // light.c must be a removal event (entity = None).
        let c_event = events.iter().find(|e| e.id.as_str() == "light.c").unwrap();
        assert!(
            c_event.entity.is_none(),
            "removed entity must produce EntityUpdate {{ entity: None }}"
        );

        // light.b must be an update event (entity = Some).
        let b_event = events.iter().find(|e| e.id.as_str() == "light.b").unwrap();
        assert!(
            b_event.entity.is_some(),
            "updated entity must produce EntityUpdate {{ entity: Some }}"
        );

        // light.d must be a new-entity event (entity = Some).
        let d_event = events.iter().find(|e| e.id.as_str() == "light.d").unwrap();
        assert!(
            d_event.entity.is_some(),
            "new entity must produce EntityUpdate {{ entity: Some }}"
        );

        // light.a must NOT appear — its last_updated didn't change.
        assert!(
            !changed_ids.contains(&"light.a"),
            "unchanged entity must NOT produce an event"
        );
    }

    #[test]
    fn test_diff_broadcast_empty_snapshots_produce_no_events() {
        let old_snap: Arc<HashMap<EntityId, Entity>> = Arc::new(HashMap::new());
        let store = MockStore::new();
        // New snapshot is also empty.
        store.apply_full_snapshot(vec![]);

        WsClient::diff_and_broadcast(&old_snap, &store);

        assert!(
            store.recorded_events().is_empty(),
            "empty-to-empty diff must produce no events"
        );
    }

    // -----------------------------------------------------------------------
    // Test: FSM transitions don't include token plaintext in trace.
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[traced_test]
    async fn test_fsm_transitions_do_not_log_token_plaintext() {
        let fixture_token = "SECRET_FSM_TOKEN_TRACE_CHECK_7654";
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            unsafe {
                std::env::set_var("HA_URL", "ws://ha.local:8123/api/websocket");
                std::env::set_var("HA_TOKEN", fixture_token);
            }
            let config = Config::from_env().unwrap();
            let (tx, rx) = status::channel();
            (WsClient::new(config, tx), rx)
        };
        let (mut sink, _sent) = MockSink::new();

        // Drive through the full happy-path FSM.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let subscribe_id = match &phase {
            Phase::Subscribing { subscribe_id } => *subscribe_id,
            other => panic!("expected Subscribing; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_states_id = match &phase {
            Phase::Snapshotting { get_states_id, .. } => *get_states_id,
            other => panic!("expected Snapshotting; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_states_id},"success":true,"result":[]}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_services_id = match &phase {
            Phase::Services { get_services_id } => *get_services_id,
            other => panic!("expected Services; got: {other:?}"),
        };
        let _phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_services_id},"success":true,"result":{{}}}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();

        // No trace line must contain the plaintext token.
        assert!(
            !logs_contain(fixture_token),
            "plaintext token must not appear in FSM trace output"
        );
    }

    // -----------------------------------------------------------------------
    // BLOCKER 2 (TASK-044): auth_invalid received in Live → Failed, no reconnect.
    //
    // Codex's audit found that the FSM only handled `auth_invalid` in
    // `Phase::Authenticating`.  If HA revokes the token while the connection is
    // live, the FSM must transition to `Failed` with no reconnect attempt — same
    // semantics as auth_invalid during Authenticating.  The complementary
    // contract (auth_required mid-Live → Reconnecting) is asserted by the
    // existing `test_mid_session_auth_required_triggers_reconnect`.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_invalid_during_live_transitions_to_failed() {
        let (mut client, state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-mid-live-revoked")
        };
        let (mut sink, _sent) = MockSink::new();

        // Prime the backoff so we can verify it does NOT advance on auth_invalid
        // (auth_invalid is terminal — the reconnect loop bails on it).
        client.backoff.advance();
        client.backoff.advance();
        let attempts_before = client.backoff.attempts;
        let window_before = client.backoff.current_window;

        // Drive the FSM through the full handshake to Live so the test exercises
        // the production code path, not a hand-constructed phase.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let subscribe_id = match &phase {
            Phase::Subscribing { subscribe_id } => *subscribe_id,
            other => panic!("expected Subscribing; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_states_id = match &phase {
            Phase::Snapshotting { get_states_id, .. } => *get_states_id,
            other => panic!("expected Snapshotting; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_states_id},"success":true,"result":[]}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_services_id = match &phase {
            Phase::Services { get_services_id } => *get_services_id,
            other => panic!("expected Services; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_services_id},"success":true,"result":{{}}}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        assert!(
            matches!(phase, Phase::Live),
            "test must reach Live before injecting auth_invalid; got: {phase:?}"
        );
        assert_eq!(*state_rx.borrow(), ConnectionState::Live);

        // Inject auth_invalid in Live → Failed, no reconnect, plaintext message
        // surfaced (token NOT echoed).
        let result = client
            .handle_message(
                inbound(r#"{"type":"auth_invalid","message":"Token revoked"}"#),
                phase,
                &mut sink,
            )
            .await;

        assert!(
            matches!(result, Err(ClientError::AuthInvalid { .. })),
            "auth_invalid in Live must yield ClientError::AuthInvalid; got: {result:?}"
        );
        assert_eq!(
            *state_rx.borrow(),
            ConnectionState::Failed,
            "state must be Failed after mid-Live auth_invalid"
        );

        if let Err(ClientError::AuthInvalid { reason }) = result {
            assert_eq!(reason, "Token revoked");
            assert!(
                !reason.contains("test-token-mid-live-revoked"),
                "token plaintext must never appear in error reason; got: {reason}"
            );
        }

        // Backoff state untouched — the AuthInvalid contract is "no reconnect".
        // The reconnect loop in lib.rs bails out on this variant, so any
        // changes here would be invisible side-effects; assert they don't
        // happen so a future refactor can't quietly start advancing the
        // counter on a terminal failure.
        assert_eq!(
            client.backoff.attempts, attempts_before,
            "auth_invalid must not advance backoff (no reconnect)"
        );
        assert_eq!(
            client.backoff.current_window, window_before,
            "auth_invalid must not advance backoff window"
        );

        // stable_live_since must be cleared so a future client reuse does not
        // inherit a Live timestamp from this dead session.
        assert!(
            client.stable_live_since.is_none(),
            "auth_invalid must clear stable_live_since"
        );
    }

    // -----------------------------------------------------------------------
    // BLOCKER 3 (TASK-044): get_services result populates ServiceRegistry.
    //
    // Codex's audit found the FSM logged the reply and discarded it.  This test
    // drives the FSM through `Services` with a 2-domain × 2-service mock result
    // and asserts the `services()` accessor returns Some(_) for known
    // (domain, service) pairs and None for unknown ones.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_services_result_populates_registry() {
        let (mut client, _state_rx) = {
            let _g = ENV_LOCK.lock().unwrap();
            make_client("test-token-registry")
        };
        let (mut sink, _sent) = MockSink::new();

        // Pre-condition: registry starts empty.  Acquire the read-lock through
        // the shared handle to mirror the production read path.
        assert!(
            client
                .services
                .read()
                .expect("ServiceRegistry RwLock poisoned")
                .lookup("light", "turn_on")
                .is_none(),
            "registry must be empty before get_services reply"
        );

        // Drive FSM auth → subscribe ACK → snapshot → Services.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let subscribe_id = match &phase {
            Phase::Subscribing { subscribe_id } => *subscribe_id,
            other => panic!("expected Subscribing; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_states_id = match &phase {
            Phase::Snapshotting { get_states_id, .. } => *get_states_id,
            other => panic!("expected Snapshotting; got: {other:?}"),
        };
        let phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{get_states_id},"success":true,"result":[]}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();
        let get_services_id = match &phase {
            Phase::Services { get_services_id } => *get_services_id,
            other => panic!("expected Services; got: {other:?}"),
        };

        // Deliver a 2-domain × 2-service result map.  The shape matches
        // ServiceRegistry::from_get_services_result's contract (verified by
        // src/ha/services.rs unit tests).
        let services_payload = format!(
            r#"{{"type":"result","id":{get_services_id},"success":true,"result":{{
                "light":{{
                    "turn_on":{{"name":"Turn on","fields":{{}}}},
                    "turn_off":{{"name":"Turn off","fields":{{}}}}
                }},
                "switch":{{
                    "turn_on":{{"name":"Turn on","fields":{{}}}},
                    "toggle":{{"name":"Toggle","fields":{{}}}}
                }}
            }}}}"#
        );
        let phase = client
            .handle_message(inbound(&services_payload), phase, &mut sink)
            .await
            .unwrap();
        assert!(
            matches!(phase, Phase::Live),
            "FSM must reach Live after services reply; got: {phase:?}"
        );

        // Post-condition: every (domain, service) the payload listed is
        // present, and an unknown lookup returns None (registry is bounded by
        // the payload).  Hold a single read-lock for the batch so the test
        // exercises the same lock-acquisition shape a Phase 3 dispatcher
        // would use when checking multiple services in one frame.
        let guard = client
            .services
            .read()
            .expect("ServiceRegistry RwLock poisoned");
        assert!(
            guard.lookup("light", "turn_on").is_some(),
            "ServiceRegistry must contain light.turn_on after get_services parse"
        );
        assert!(
            guard.lookup("light", "turn_off").is_some(),
            "ServiceRegistry must contain light.turn_off"
        );
        assert!(
            guard.lookup("switch", "turn_on").is_some(),
            "ServiceRegistry must contain switch.turn_on"
        );
        assert!(
            guard.lookup("switch", "toggle").is_some(),
            "ServiceRegistry must contain switch.toggle"
        );
        assert!(
            guard.lookup("nonexistent", "x").is_none(),
            "ServiceRegistry must return None for unknown (domain, service) pairs"
        );
        assert!(
            guard.lookup("light", "unknown_service").is_none(),
            "ServiceRegistry must return None for unknown service in known domain"
        );
    }
}
