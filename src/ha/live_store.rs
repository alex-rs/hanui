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
//! # Phase 3 command channel
//!
//! The `command_tx` field is `None` throughout Phase 2.  Phase 3's dispatcher
//! will populate it via a setter so `LiveStore` can forward commands to the WS
//! client without bridging through `src/main.rs`.
//!
//! [`EntityStore`]: super::store::EntityStore

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::{broadcast, mpsc};

use crate::ha::entity::{Entity, EntityId};
use crate::ha::protocol::OutboundMsg;
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

    /// Phase 3 command channel.
    ///
    /// `None` throughout Phase 2.  Phase 3's service-call dispatcher populates
    /// this field so `LiveStore` can forward `OutboundMsg` frames to the WS
    /// client task.  Using `Option` avoids dead-code while reserving the field
    /// — Phase 3 will not need a struct reshape.
    pub command_tx: Option<mpsc::Sender<OutboundMsg>>,
}

impl LiveStore {
    /// Construct a new, empty `LiveStore`.
    ///
    /// The snapshot starts empty; call `apply_snapshot` after the initial
    /// `get_states` reply arrives.
    pub fn new() -> Self {
        LiveStore {
            snapshot: RwLock::new(Arc::new(HashMap::new())),
            senders: RwLock::new(HashMap::new()),
            command_tx: None,
        }
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
