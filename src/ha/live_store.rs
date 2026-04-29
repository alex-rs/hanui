//! Phase 2 live entity store, backed by a Home Assistant WebSocket connection.
//!
//! # Overview
//!
//! [`LiveStore`] is the drop-in replacement for [`MemoryStore`][super::store::MemoryStore]
//! introduced in Phase 2.  It implements [`EntityStore`] with the same trait
//! shape so the bridge in `src/ui/bridge.rs` requires no changes.
//!
//! # Snapshot model
//!
//! The entity map is stored as `Arc<HashMap<EntityId, Entity>>` wrapped in an
//! outer `RwLock`.  `apply_snapshot` performs an atomic Arc swap — no per-entity
//! churn during reconnect.  `snapshot()` returns an `Arc` clone in O(1) without
//! copying the map.
//!
//! # Per-entity broadcast channels
//!
//! Channels are lazy: created on first `subscribe` call for a given entity id.
//! The channel capacity is 1 (latest-only).  When the sender pushes a second
//! event before the receiver has consumed the first, the receiver gets
//! `RecvError::Lagged(_)`.  The bridge resync path: call `store.get(id)` for
//! each subscribed id to rebuild state, then re-subscribe.
//!
//! # `subscribe` single-id contract
//!
//! Phase 2 `subscribe` accepts a `&[EntityId]` slice for API symmetry with the
//! `EntityStore` trait, but it creates one combined receiver only for the
//! **first** id in the slice.  TASK-033's bridge subscribes per-id (one call per
//! entity), so this is the expected usage pattern.  Passing an empty slice
//! returns a receiver that will never yield an event.  Passing multiple ids in
//! one call returns a receiver tied to only the first id; callers that need
//! multi-id subscriptions should issue one `subscribe` call per id.
//!
//! # Phase 3 command channel (TASK-072)
//!
//! `command_tx` is the dispatcher → WS client write seam locked by
//! `docs/plans/2026-04-28-phase-3-actions.md`
//! § `locked_decisions.command_tx_wiring` and `locked_decisions.ws_command_ack_envelope`.
//!
//! The field is `None` throughout Phase 2 and is populated by `src/lib.rs`
//! at startup (after the WS client task launches) via
//! [`LiveStore::set_command_tx`]. A Phase 3 dispatcher (TASK-062) clones the
//! returned [`mpsc::Sender<OutboundCommand>`][OutboundCommand] and pushes
//! [`OutboundCommand`] envelopes onto it; the WS client task drains the matching
//! receiver, allocates the next monotonic id, registers the envelope's
//! `ack_tx` in its pending-ack map, and serializes the wire JSON.
//!
//! ## Reconnect repopulation (Risk #11)
//!
//! When the WS client task exits/restarts, the receiver end is dropped. The
//! reconnect loop in `src/lib.rs::run_ws_client` calls [`LiveStore::set_command_tx`]
//! again with a fresh sender as part of the next attempt; in the gap, dispatch
//! attempts return [`DispatchError::ChannelClosed`][crate::actions::dispatcher::DispatchError]
//! (the dispatcher's existing handling of a closed `mpsc::Sender`) and surface as
//! a toast — never panic.
//!
//! [`OutboundCommand`]: crate::ha::client::OutboundCommand
//! [`EntityStore`]: super::store::EntityStore

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::{broadcast, mpsc};

use crate::ha::client::OutboundCommand;
use crate::ha::entity::{Entity, EntityId};
use crate::ha::services::{ServiceMeta, ServiceRegistry, ServiceRegistryHandle};
use crate::ha::store::{EntityStore, EntityUpdate};

// ---------------------------------------------------------------------------
// LiveStore
// ---------------------------------------------------------------------------

/// Phase 2 live entity store.
///
/// See module-level documentation for the snapshot, broadcast, and
/// Phase 3 command-channel contracts.
pub struct LiveStore {
    /// Atomic-swap snapshot.
    ///
    /// The inner `Arc<HashMap>` is swapped atomically by `apply_snapshot` so
    /// that no per-entity churn occurs during reconnect.  `snapshot()` clones
    /// only the outer `Arc` — O(1) and lock-free after the read-guard is
    /// acquired.
    snapshot: RwLock<Arc<HashMap<EntityId, Entity>>>,

    /// Per-entity broadcast senders, created on first `subscribe` call.
    ///
    /// Capacity 1 per sender — matching the Phase 1 `MemoryStore` contract.
    /// When a sender has no active receivers, `send` returns an error; this is
    /// silently discarded (no receiver is a normal, non-error condition).
    senders: RwLock<HashMap<EntityId, broadcast::Sender<EntityUpdate>>>,

    /// Phase 3 command channel — dispatcher → WS client task seam (TASK-072).
    ///
    /// Wrapped in [`RwLock`] (not `Mutex`) because the read path is far hotter
    /// than the write path: every dispatch acquires a read-lock to clone the
    /// sender, while writes only happen at startup and on WS client task
    /// restart (the reconnect FSM repopulation per
    /// `locked_decisions.command_tx_wiring`). `RwLock` lets concurrent
    /// dispatchers proceed without contention.
    ///
    /// `None` until [`LiveStore::set_command_tx`] is called by the
    /// orchestrator in `src/lib.rs::run_ws_client`. While `None`, a dispatcher
    /// constructed from `LiveStore.command_tx()` will return
    /// [`DispatchError::ChannelNotWired`][crate::actions::dispatcher::DispatchError]
    /// for every WS-bound action; while `Some(closed)` (between WS task exit
    /// and the next [`set_command_tx`][LiveStore::set_command_tx] call), it
    /// returns [`DispatchError::ChannelClosed`][crate::actions::dispatcher::DispatchError]
    /// (Risk #11). Either way, the dispatcher never panics.
    command_tx: RwLock<Option<mpsc::Sender<OutboundCommand>>>,

    /// Shared handle to the populated service registry (TASK-048).
    ///
    /// Constructed in `src/lib.rs::build_ws_client_with_store` and shared via
    /// `Arc` clone with the [`WsClient`] that populates it during the
    /// `Phase::Services → Live` transition.  Phase 3 dispatchers that hold an
    /// `Arc<LiveStore>` call [`LiveStore::services_lookup`] to validate
    /// `(domain, service)` pairs without touching the `WsClient`.
    ///
    /// The `Arc` is constructed once; `LiveStore` owns a clone and `WsClient`
    /// owns a clone — the same backing `RwLock<ServiceRegistry>` is shared.
    ///
    /// [`WsClient`]: crate::ha::client::WsClient
    pub services_handle: ServiceRegistryHandle,
}

impl LiveStore {
    /// Construct a new, empty `LiveStore`.
    ///
    /// Initialises `services_handle` with a fresh `ServiceRegistryHandle` via
    /// [`ServiceRegistry::new_handle`].  When `LiveStore` is wired into
    /// `WsClient` via `src/lib.rs::run_with_live_store`, the shared handle
    /// from that callsite replaces this default via
    /// [`LiveStore::with_services_handle`] so both ends point at the same
    /// backing lock.
    ///
    /// The snapshot starts empty; call `apply_snapshot` after the initial
    /// `get_states` reply arrives.
    pub fn new() -> Self {
        LiveStore {
            snapshot: RwLock::new(Arc::new(HashMap::new())),
            senders: RwLock::new(HashMap::new()),
            command_tx: RwLock::new(None),
            services_handle: ServiceRegistry::new_handle(),
        }
    }

    /// Replace the default `services_handle` with a shared one.
    ///
    /// Builder companion to [`LiveStore::new`].  Production wiring in
    /// `src/lib.rs::run_with_live_store` constructs the handle once and
    /// clones it into both this `LiveStore` and the [`WsClient`] via
    /// [`WsClient::with_registry`], so a single `Arc<RwLock<_>>` backs both
    /// the WS-task writer and the read-side accessor.
    ///
    /// Returns `Self` so it composes with `Arc::new(...)` at the call site:
    ///
    /// ```ignore
    /// let registry = ServiceRegistry::new_handle();
    /// let store = Arc::new(LiveStore::new().with_services_handle(registry.clone()));
    /// let client = WsClient::new(config, state_tx)
    ///     .with_store(store.clone())
    ///     .with_registry(registry);
    /// ```
    ///
    /// [`WsClient`]: crate::ha::client::WsClient
    /// [`WsClient::with_registry`]: crate::ha::client::WsClient::with_registry
    pub fn with_services_handle(mut self, services_handle: ServiceRegistryHandle) -> Self {
        self.services_handle = services_handle;
        self
    }

    /// Return a clone of the shared `ServiceRegistryHandle`.
    ///
    /// The returned handle is an `Arc` clone — cheap and `Send + Sync`.
    /// Phase 3 code that holds `Arc<LiveStore>` uses this to share the same
    /// registry reference, or calls [`services_lookup`] directly.
    ///
    /// [`services_lookup`]: LiveStore::services_lookup
    pub fn services_handle(&self) -> ServiceRegistryHandle {
        Arc::clone(&self.services_handle)
    }

    /// Look up a `(domain, service)` pair in the shared registry.
    ///
    /// Acquires the registry read-lock, performs the lookup, clones the result
    /// if found, then releases the lock.  Returns `None` if the domain or
    /// service is not present, or if the registry has not yet been populated
    /// (i.e. the FSM has not completed `Phase::Services → Live`).
    ///
    /// Callers in Phase 3 command dispatchers use this to validate a tap-action
    /// target before issuing a `call_service` frame, without needing a handle to
    /// the `WsClient`.
    pub fn services_lookup(&self, domain: &str, service: &str) -> Option<ServiceMeta> {
        let guard = self
            .services_handle
            .read()
            .expect("ServiceRegistry RwLock poisoned");
        guard.lookup(domain, service).cloned()
    }

    // -----------------------------------------------------------------------
    // Phase 3 command channel (TASK-072)
    // -----------------------------------------------------------------------

    /// Install the dispatcher → WS client command sender.
    ///
    /// Called by `src/lib.rs::run_ws_client` once per WS attempt, **after**
    /// the WS client task has been spawned with the matching
    /// [`mpsc::Receiver<OutboundCommand>`]. Replaces any prior sender (the
    /// reconnect FSM repopulation case per
    /// `locked_decisions.command_tx_wiring`); the old sender — if still held
    /// — becomes inert when its receiver is dropped, so the next dispatch on
    /// a stale clone surfaces as
    /// [`DispatchError::ChannelClosed`][crate::actions::dispatcher::DispatchError].
    ///
    /// Takes `&self` (not `&mut self`) so the orchestrator can call this
    /// through an `Arc<LiveStore>` without exclusive access.
    ///
    /// [`OutboundCommand`]: crate::ha::client::OutboundCommand
    pub fn set_command_tx(&self, tx: mpsc::Sender<OutboundCommand>) {
        let mut guard = self
            .command_tx
            .write()
            .expect("LiveStore command_tx RwLock poisoned");
        *guard = Some(tx);
    }

    /// Return a clone of the current command sender, if any.
    ///
    /// `mpsc::Sender` is cheap to clone (an `Arc` bump). Phase 3 dispatchers
    /// hold their own clone — typically passed to
    /// [`Dispatcher::with_command_tx`][crate::actions::dispatcher::Dispatcher::with_command_tx]
    /// at construction. Returns `None` until [`LiveStore::set_command_tx`]
    /// has been called.
    pub fn command_tx(&self) -> Option<mpsc::Sender<OutboundCommand>> {
        let guard = self
            .command_tx
            .read()
            .expect("LiveStore command_tx RwLock poisoned");
        guard.as_ref().cloned()
    }

    /// Drop the currently-installed command sender, if any.
    ///
    /// Called by `src/lib.rs::run_ws_client` when the WS client task exits
    /// so that subsequent dispatches see `None` (and return
    /// [`DispatchError::ChannelNotWired`][crate::actions::dispatcher::DispatchError]
    /// rather than racing the next reconnect's
    /// [`set_command_tx`][LiveStore::set_command_tx] call against a still-Some
    /// stale sender). Idempotent: clearing an already-`None` field is a no-op.
    pub fn clear_command_tx(&self) {
        let mut guard = self
            .command_tx
            .write()
            .expect("LiveStore command_tx RwLock poisoned");
        *guard = None;
    }

    /// Replace the entire entity map atomically.
    ///
    /// Called after the initial `get_states` reply (and after each reconnect
    /// resync).  The new map is built from the provided `entities` slice and
    /// swapped into place under a write-lock.  No per-entity broadcast is fired
    /// — the bridge performs a full `for_each` resync after `apply_snapshot`.
    pub fn apply_snapshot(&self, entities: Vec<Entity>) {
        let new_map: HashMap<EntityId, Entity> =
            entities.into_iter().map(|e| (e.id.clone(), e)).collect();
        let mut guard = self
            .snapshot
            .write()
            .expect("LiveStore snapshot RwLock poisoned");
        *guard = Arc::new(new_map);
    }

    /// Apply a single incremental entity update.
    ///
    /// - `update.entity == Some(entity)` → insert or replace the entity in the
    ///   snapshot.
    /// - `update.entity == None` → remove the entity from the snapshot.
    ///
    /// After the snapshot is updated, a broadcast is sent to any active
    /// per-entity subscriber.  If no subscriber exists for this entity, the
    /// broadcast is silently discarded.
    pub fn apply_event(&self, update: EntityUpdate) {
        // Update the snapshot under a write-lock.
        {
            let mut guard = self
                .snapshot
                .write()
                .expect("LiveStore snapshot RwLock poisoned");

            // Clone the current map, apply the change, then Arc-swap.
            let mut new_map: HashMap<EntityId, Entity> = (**guard).clone();
            match &update.entity {
                Some(entity) => {
                    new_map.insert(update.id.clone(), entity.clone());
                }
                None => {
                    new_map.remove(&update.id);
                }
            }
            *guard = Arc::new(new_map);
        }

        // Broadcast to any active per-entity subscriber.  Holding the senders
        // read-lock while sending is safe because broadcast::Sender::send does
        // not block and does not re-acquire any internal lock on this store.
        let senders_guard = self
            .senders
            .read()
            .expect("LiveStore senders RwLock poisoned");
        if let Some(tx) = senders_guard.get(&update.id) {
            // Discard send errors: no receivers is expected when no subscriber
            // is currently watching this entity.
            let _ = tx.send(update);
        }
    }

    /// Return an O(1) clone of the current snapshot arc.
    ///
    /// The returned `Arc<HashMap>` is a stable snapshot at the instant of the
    /// call.  Subsequent `apply_event` calls do not mutate the returned map —
    /// they produce a new `Arc` and swap it in.
    pub fn snapshot(&self) -> Arc<HashMap<EntityId, Entity>> {
        let guard = self
            .snapshot
            .read()
            .expect("LiveStore snapshot RwLock poisoned");
        Arc::clone(&*guard)
    }
}

impl Default for LiveStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// EntityStore impl
// ---------------------------------------------------------------------------

impl EntityStore for LiveStore {
    /// Look up a single entity by id.
    ///
    /// Acquires a read-lock, clones the `Arc` snapshot, then looks up the id.
    /// The lock is released before the clone is returned so callers are never
    /// blocked on a write-lock.
    fn get(&self, id: &EntityId) -> Option<Entity> {
        let guard = self
            .snapshot
            .read()
            .expect("LiveStore snapshot RwLock poisoned");
        guard.get(id).cloned()
    }

    /// Visit every entity in the snapshot under a single read-lock acquisition.
    ///
    /// The entire walk runs while the read-lock is held.  Callers must not
    /// perform any action inside the visitor that would attempt to acquire a
    /// write-lock on this store (deadlock).
    fn for_each(&self, f: &mut dyn FnMut(&EntityId, &Entity)) {
        let guard = self
            .snapshot
            .read()
            .expect("LiveStore snapshot RwLock poisoned");
        for (id, entity) in guard.iter() {
            f(id, entity);
        }
        // Lock is released at end of scope — entire walk occurs under one
        // read-lock acquisition, consistent with the Phase 1 MemoryStore
        // contract.
    }

    /// Subscribe to updates for an entity.
    ///
    /// Creates a per-entity broadcast sender lazily (on first call for a given
    /// id).  Returns a receiver with capacity 1; lagging receivers get
    /// `RecvError::Lagged` and must resync via `store.get(id)`.
    ///
    /// Only the first element of `ids` is used; passing an empty slice returns
    /// a receiver that will never yield an event.  See module documentation
    /// for the single-id subscribe contract.
    fn subscribe(&self, ids: &[EntityId]) -> broadcast::Receiver<EntityUpdate> {
        let Some(id) = ids.first() else {
            // No id requested — return a receiver from a throw-away channel.
            let (tx, rx) = broadcast::channel(1);
            drop(tx);
            return rx;
        };

        // Fast path: check under read-lock first.
        {
            let guard = self
                .senders
                .read()
                .expect("LiveStore senders RwLock poisoned");
            if let Some(tx) = guard.get(id) {
                return tx.subscribe();
            }
        }

        // Slow path: create a new sender under write-lock.
        let mut guard = self
            .senders
            .write()
            .expect("LiveStore senders RwLock poisoned");
        // Re-check after acquiring write-lock (another thread may have inserted
        // between the read-lock release and this write-lock acquisition).
        let tx = guard
            .entry(id.clone())
            .or_insert_with(|| broadcast::channel(1).0);
        tx.subscribe()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use jiff::Timestamp;
    use serde_json::Map;
    use tokio::sync::broadcast::error::RecvError;

    use super::*;

    /// Local compile-time helper mirroring `store::_assert_store`.
    ///
    /// `store::_assert_store` is crate-private; we replicate the pattern here
    /// rather than widening its visibility (TASK-030 must not touch store.rs).
    fn assert_store_bound<S: EntityStore + Send + Sync>() {}

    fn make_entity(id: &str, state: &str) -> Entity {
        Entity {
            id: EntityId::from(id),
            state: Arc::from(state),
            attributes: Arc::new(Map::new()),
            last_changed: Timestamp::UNIX_EPOCH,
            last_updated: Timestamp::UNIX_EPOCH,
        }
    }

    // -----------------------------------------------------------------------
    // Compile-time bound proof
    // -----------------------------------------------------------------------

    #[test]
    fn live_store_satisfies_entity_store_bound() {
        // Compile-time: LiveStore must implement EntityStore + Send + Sync.
        assert_store_bound::<LiveStore>();
        // Also verify Arc<dyn EntityStore> coercion from Arc<LiveStore>.
        let _: Arc<dyn EntityStore> = Arc::new(LiveStore::new());
    }

    // -----------------------------------------------------------------------
    // apply_snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn apply_snapshot_populates_map() {
        let store = LiveStore::new();
        let entities = vec![
            make_entity("light.kitchen", "on"),
            make_entity("sensor.temp", "21.5"),
        ];
        store.apply_snapshot(entities);

        let e = store.get(&EntityId::from("light.kitchen")).unwrap();
        assert_eq!(&*e.state, "on");

        let e2 = store.get(&EntityId::from("sensor.temp")).unwrap();
        assert_eq!(&*e2.state, "21.5");
    }

    #[test]
    fn apply_snapshot_replaces_existing_map() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![make_entity("light.old", "on")]);
        store.apply_snapshot(vec![make_entity("light.new", "off")]);

        // Old entity is gone.
        assert!(store.get(&EntityId::from("light.old")).is_none());
        // New entity is present.
        let e = store.get(&EntityId::from("light.new")).unwrap();
        assert_eq!(&*e.state, "off");
    }

    #[test]
    fn snapshot_method_returns_arc_of_current_map() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![make_entity("light.a", "on")]);

        let snap = store.snapshot();
        assert!(snap.contains_key(&EntityId::from("light.a")));
        assert_eq!(snap.len(), 1);
    }

    // -----------------------------------------------------------------------
    // apply_event — Some(entity)
    // -----------------------------------------------------------------------

    #[test]
    fn apply_event_some_updates_existing_entity() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![make_entity("light.kitchen", "off")]);

        store.apply_event(EntityUpdate {
            id: EntityId::from("light.kitchen"),
            entity: Some(make_entity("light.kitchen", "on")),
        });

        let e = store.get(&EntityId::from("light.kitchen")).unwrap();
        assert_eq!(&*e.state, "on");
    }

    #[test]
    fn apply_event_some_inserts_new_entity() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![]);

        store.apply_event(EntityUpdate {
            id: EntityId::from("light.new"),
            entity: Some(make_entity("light.new", "on")),
        });

        let e = store.get(&EntityId::from("light.new")).unwrap();
        assert_eq!(&*e.state, "on");
    }

    // -----------------------------------------------------------------------
    // apply_event — None (removal)
    // -----------------------------------------------------------------------

    #[test]
    fn apply_event_none_removes_entity() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![make_entity("light.gone", "on")]);

        // Verify entity is present before removal.
        assert!(store.get(&EntityId::from("light.gone")).is_some());

        store.apply_event(EntityUpdate {
            id: EntityId::from("light.gone"),
            entity: None,
        });

        // Entity must be gone from the snapshot.
        assert!(
            store.get(&EntityId::from("light.gone")).is_none(),
            "get(id) must return None after EntityUpdate {{ entity: None }}"
        );
    }

    #[test]
    fn apply_event_none_for_nonexistent_entity_is_noop() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![make_entity("light.other", "on")]);

        // Remove an entity that doesn't exist — must not panic.
        store.apply_event(EntityUpdate {
            id: EntityId::from("light.missing"),
            entity: None,
        });

        // Other entity is unaffected.
        assert!(store.get(&EntityId::from("light.other")).is_some());
    }

    // -----------------------------------------------------------------------
    // subscribe + broadcast
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn subscribe_receives_broadcast_from_apply_event() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![make_entity("light.x", "off")]);

        let mut rx = store.subscribe(&[EntityId::from("light.x")]);

        store.apply_event(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "on")),
        });

        let update = rx.recv().await.expect("expected an update");
        assert_eq!(update.id.as_str(), "light.x");
        assert!(update.entity.is_some());
        assert_eq!(&*update.entity.unwrap().state, "on");
    }

    #[tokio::test]
    async fn subscribe_receives_removal_broadcast() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![make_entity("light.gone", "on")]);

        let mut rx = store.subscribe(&[EntityId::from("light.gone")]);

        store.apply_event(EntityUpdate {
            id: EntityId::from("light.gone"),
            entity: None,
        });

        let update = rx.recv().await.expect("expected removal update");
        assert_eq!(update.id.as_str(), "light.gone");
        assert!(
            update.entity.is_none(),
            "removal update must carry entity: None"
        );
    }

    // -----------------------------------------------------------------------
    // Lagged semantics (capacity-1 channel, Phase 1 contract)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn two_events_without_consume_causes_lagged() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![make_entity("light.x", "off")]);

        let mut rx = store.subscribe(&[EntityId::from("light.x")]);

        // Send two events without consuming the first.
        store.apply_event(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "on")),
        });
        store.apply_event(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "off")),
        });

        match rx.recv().await {
            Err(RecvError::Lagged(_)) => {}
            other => panic!("expected RecvError::Lagged, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn after_lagged_get_returns_latest_entity() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![make_entity("light.x", "off")]);

        let mut rx = store.subscribe(&[EntityId::from("light.x")]);

        // Force lag.
        store.apply_event(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "state1")),
        });
        store.apply_event(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "state2")),
        });

        // Confirm lag.
        assert!(matches!(rx.recv().await, Err(RecvError::Lagged(_))));

        // Resync via get — must return the latest applied state.
        let current = store.get(&EntityId::from("light.x")).unwrap();
        assert_eq!(
            &*current.state, "state2",
            "get(id) must return the latest entity after Lagged resync"
        );
    }

    // -----------------------------------------------------------------------
    // for_each
    // -----------------------------------------------------------------------

    #[test]
    fn for_each_visits_all_entities() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![
            make_entity("light.a", "on"),
            make_entity("light.b", "off"),
            make_entity("sensor.c", "21"),
        ]);

        let mut visited: Vec<String> = Vec::new();
        store.for_each(&mut |id, _entity| {
            visited.push(id.as_str().to_owned());
        });
        visited.sort();

        assert_eq!(visited, ["light.a", "light.b", "sensor.c"]);
    }

    #[test]
    fn for_each_on_empty_store_calls_visitor_zero_times() {
        let store = LiveStore::new();
        let mut count = 0usize;
        store.for_each(&mut |_id, _entity| {
            count += 1;
        });
        assert_eq!(count, 0);
    }

    // -----------------------------------------------------------------------
    // command_tx setter / getter / clear (TASK-072)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn command_tx_initial_value_is_none() {
        let store = LiveStore::new();
        assert!(
            store.command_tx().is_none(),
            "fresh LiveStore must have command_tx = None"
        );
    }

    #[tokio::test]
    async fn set_command_tx_round_trip_delivers_to_receiver() {
        // Acceptance: set_command_tx installs a sender that round-trips an
        // OutboundCommand to the receiver end.  The receiver here stands in
        // for the WS client's drain task.
        use crate::ha::client::{OutboundCommand, OutboundFrame};
        use tokio::sync::oneshot;

        let store = LiveStore::new();
        let (tx, mut rx) = mpsc::channel::<OutboundCommand>(4);
        store.set_command_tx(tx);

        let cloned = store
            .command_tx()
            .expect("command_tx must be Some after set_command_tx");

        let (ack_tx, _ack_rx) = oneshot::channel();
        cloned
            .send(OutboundCommand {
                frame: OutboundFrame {
                    domain: "light".to_owned(),
                    service: "turn_on".to_owned(),
                    target: None,
                    data: None,
                },
                ack_tx,
            })
            .await
            .expect("send through installed command_tx must succeed");

        let received = rx
            .recv()
            .await
            .expect("receiver must yield the dispatched OutboundCommand");
        assert_eq!(received.frame.domain, "light");
        assert_eq!(received.frame.service, "turn_on");
    }

    #[tokio::test]
    async fn set_command_tx_replaces_prior_sender() {
        // Reconnect-repopulation invariant per locked_decisions.command_tx_wiring:
        // a second set_command_tx call replaces the prior sender so the next
        // dispatch reaches the NEW receiver, not the stale one.
        use crate::ha::client::{OutboundCommand, OutboundFrame};
        use tokio::sync::oneshot;

        let store = LiveStore::new();

        // Install sender #1, then drop its receiver (simulating WS task exit).
        let (tx1, rx1) = mpsc::channel::<OutboundCommand>(4);
        store.set_command_tx(tx1);
        drop(rx1);

        // Install sender #2 — fresh receiver.
        let (tx2, mut rx2) = mpsc::channel::<OutboundCommand>(4);
        store.set_command_tx(tx2);

        // Cloning command_tx now must yield the NEW sender (talks to rx2).
        let cloned = store
            .command_tx()
            .expect("command_tx must be Some after second set_command_tx");
        let (ack_tx, _ack_rx) = oneshot::channel();
        cloned
            .send(OutboundCommand {
                frame: OutboundFrame {
                    domain: "switch".to_owned(),
                    service: "toggle".to_owned(),
                    target: None,
                    data: None,
                },
                ack_tx,
            })
            .await
            .expect("send through replacement command_tx must succeed");

        let received = rx2
            .recv()
            .await
            .expect("replacement receiver must yield the dispatch");
        assert_eq!(received.frame.domain, "switch");
        assert_eq!(received.frame.service, "toggle");
    }

    #[tokio::test]
    async fn clear_command_tx_unsets_sender() {
        // After clear_command_tx, command_tx() returns None — caller-visible
        // signal that no WS task is currently draining.
        let (tx, _rx) = mpsc::channel::<crate::ha::client::OutboundCommand>(1);
        let store = LiveStore::new();
        store.set_command_tx(tx);
        assert!(store.command_tx().is_some());
        store.clear_command_tx();
        assert!(
            store.command_tx().is_none(),
            "clear_command_tx must reset the field to None"
        );
    }

    #[tokio::test]
    async fn dropped_receiver_makes_clone_send_fail() {
        // Risk #11: when the WS task exits, the receiver is dropped; any
        // dispatch that still holds a clone of the old sender must observe a
        // closed-channel error rather than panic.  This is the boundary the
        // dispatcher's DispatchError::ChannelClosed path keys on.
        use crate::ha::client::{OutboundCommand, OutboundFrame};
        use tokio::sync::oneshot;

        let store = LiveStore::new();
        let (tx, rx) = mpsc::channel::<OutboundCommand>(1);
        store.set_command_tx(tx);
        let cloned = store
            .command_tx()
            .expect("command_tx must be Some after set_command_tx");

        // Simulate WS task exit: drop the receiver.
        drop(rx);

        let (ack_tx, _ack_rx) = oneshot::channel();
        let result = cloned
            .send(OutboundCommand {
                frame: OutboundFrame {
                    domain: "light".to_owned(),
                    service: "toggle".to_owned(),
                    target: None,
                    data: None,
                },
                ack_tx,
            })
            .await;
        assert!(
            result.is_err(),
            "send on a sender whose receiver was dropped must return Err"
        );
    }

    // -----------------------------------------------------------------------
    // subscribe with empty ids slice
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn subscribe_empty_ids_returns_inert_receiver() {
        let store = LiveStore::new();
        store.apply_snapshot(vec![make_entity("light.x", "on")]);

        // Subscribe with no ids — the returned receiver must never yield.
        let mut rx = store.subscribe(&[]);

        // Sending an event must NOT arrive on the inert receiver.
        store.apply_event(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "off")),
        });

        // try_recv should return an error (no messages or channel closed).
        match rx.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            Err(broadcast::error::TryRecvError::Closed) => {}
            Ok(msg) => panic!("expected no message on inert receiver, got: {msg:?}"),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                panic!("unexpected Lagged on inert receiver")
            }
        }
    }
}
