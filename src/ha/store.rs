//! Phase 1/2 seam: [`EntityStore`] trait and [`MemoryStore`] implementation.
//!
//! # Phase 1/2 contract
//!
//! The `EntityStore` trait is the binding interface that Phase 2's `LiveStore`
//! must satisfy as a drop-in replacement.  Any change to the three method
//! signatures — including adding associated types or lifetime parameters —
//! breaks the Phase 2 contract and requires a cross-phase architecture review.
//!
//! ## `for_each` is a VISITOR, not an iterator
//!
//! `for_each` accepts a closure rather than returning an iterator.  This is
//! intentional: a visitor pattern lets the implementer hold an internal
//! read-lock for the entire duration of the walk, without exposing the lock
//! guard's lifetime through the public API.  Phase 2's `LiveStore` wraps its
//! map in a `RwLock`; returning an iterator would force either a lock-holding
//! iterator (unsound across await points) or an expensive snapshot on each
//! call.  Iterator-shaped alternative designs are explicitly rejected.
//!
//! ## `subscribe` capacity = 1 (latest-only)
//!
//! The broadcast channel is created with capacity 1.  When the sender pushes a
//! second event before the receiver has consumed the first, the receiver gets
//! `RecvError::Lagged(_)` on its next `.recv()` call.  The expected resync
//! pattern for the bridge layer:
//!
//! 1. Receiver calls `.recv()` → `Err(RecvError::Lagged(_))`.
//! 2. Bridge calls `store.get(id)` for each subscribed id to rebuild state.
//! 3. Bridge re-registers by calling `store.subscribe(&ids)` again.
//!
//! This keeps the fast-path (single event, no lag) zero-allocation and trades
//! stale-receiver recovery for simplicity.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use thiserror::Error;
use tokio::sync::broadcast;

use crate::dashboard::profiles::DEFAULT_PROFILE;

use super::entity::{Entity, EntityId};

// ---------------------------------------------------------------------------
// EntityUpdate
// ---------------------------------------------------------------------------

/// A change notification pushed through the broadcast channel.
///
/// `Some(entity)` signals a state change or new entity.
/// `None` signals that the entity was removed from HA.
///
/// `#[non_exhaustive]` allows Phase 2 to add fields (e.g. event sequence
/// numbers) without breaking existing match arms.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EntityUpdate {
    pub id: EntityId,
    /// `Some` = entity state changed or entity appeared; `None` = entity removed.
    pub entity: Option<Entity>,
}

// ---------------------------------------------------------------------------
// EntityStore trait
// ---------------------------------------------------------------------------

/// Read-only view of the entity state map, plus a change-notification channel.
///
/// Implementors: [`MemoryStore`] (Phase 1), `LiveStore` (Phase 2, not yet
/// defined).  Both must be `Send + Sync` so they can be shared across Tokio
/// tasks via `Arc<dyn EntityStore>`.
///
/// **Object-safety (PATH A — dyn-compat refactor, 2026-04-26):** `for_each`
/// accepts `&mut dyn FnMut` rather than a generic `<F: FnMut>` parameter.
/// This makes the trait object-safe: `Box<dyn EntityStore>` and
/// `Arc<dyn EntityStore>` both compile and can be used for Phase 2's runtime-
/// swappable `LiveStore` drop-in.  The visitor pattern is retained so the
/// implementer can hold an internal read-lock for the entire walk without
/// exposing a lock-guarded iterator across the API boundary.
///
/// # Method contract
///
/// - [`get`][EntityStore::get] — cheap clone (`Entity` is Arc-wrapped); `None`
///   if the id is not known.
/// - [`for_each`][EntityStore::for_each] — VISITOR pattern; closure receives
///   borrowed references and may not escape them.  The implementer may hold a
///   lock for the entire walk.
/// - [`subscribe`][EntityStore::subscribe] — returns a broadcast receiver with
///   capacity **1**; lagging receivers get `RecvError::Lagged` and must resync
///   via `get`.  The `ids` slice is accepted for API symmetry with Phase 2's
///   filtered subscription; Phase 1 broadcasts all updates regardless.
pub trait EntityStore: Send + Sync {
    fn get(&self, id: &EntityId) -> Option<Entity>;
    fn for_each(&self, f: &mut dyn FnMut(&EntityId, &Entity));
    fn subscribe(&self, ids: &[EntityId]) -> broadcast::Receiver<EntityUpdate>;
}

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

/// Error returned by [`MemoryStore::load`].
#[derive(Debug, Error)]
pub enum MemoryStoreError {
    /// The number of entities exceeds [`DEFAULT_PROFILE`]``.max_entities`.
    #[error("entity count {count} exceeds profile cap {cap} (DEFAULT_PROFILE.max_entities)")]
    CapExceeded { count: usize, cap: usize },
}

/// A read-mostly, in-memory implementation of [`EntityStore`].
///
/// Phase 1 loads a fixture once at startup; the map is never mutated after
/// construction.  The broadcast channel is wired so that Phase 2's mutation
/// path (bridge calling `MemoryStore`'s internal `publish` helper or replacing
/// this type with `LiveStore`) is a drop-in.
///
/// # Internal layout
///
/// - `map`: `Arc<RwLock<HashMap<EntityId, Entity>>>` — shared under a reader
///   lock so `for_each` can walk without copying the entire map.
/// - `tx`: `broadcast::Sender<EntityUpdate>` — capacity-1 sender retained so
///   callers can subscribe at any time.
#[derive(Debug)]
pub struct MemoryStore {
    map: Arc<RwLock<HashMap<EntityId, Entity>>>,
    tx: broadcast::Sender<EntityUpdate>,
}

impl MemoryStore {
    /// Construct a [`MemoryStore`] from a slice of entities.
    ///
    /// Returns [`MemoryStoreError::CapExceeded`] if `entities.len()` exceeds
    /// `DEFAULT_PROFILE.max_entities`.
    ///
    /// # TASK-008 handoff
    ///
    /// The fixture loader (TASK-008) calls this method after deserializing the
    /// JSON fixture file into `Vec<Entity>`.  The load path is:
    ///
    /// ```text
    /// fixture JSON  →  Vec<Entity>  →  MemoryStore::load  →  Arc<MemoryStore>
    /// ```
    ///
    /// The returned `Arc<MemoryStore>` is then threaded into the bridge layer
    /// as `Arc<dyn EntityStore>`.
    pub fn load(entities: Vec<Entity>) -> Result<Self, MemoryStoreError> {
        let cap = DEFAULT_PROFILE.max_entities;
        if entities.len() > cap {
            return Err(MemoryStoreError::CapExceeded {
                count: entities.len(),
                cap,
            });
        }

        let map: HashMap<EntityId, Entity> =
            entities.into_iter().map(|e| (e.id.clone(), e)).collect();

        // Capacity 1: latest-only semantics.  Receivers that fall behind get
        // `RecvError::Lagged` and must resync via `get`.
        let (tx, _) = broadcast::channel(1);

        Ok(MemoryStore {
            map: Arc::new(RwLock::new(map)),
            tx,
        })
    }

    /// Publish an [`EntityUpdate`] to all active subscribers.
    ///
    /// An `Err` return means no subscribers are currently listening; this is
    /// not an error condition in Phase 1 (the fixture is static).
    pub fn publish(&self, update: EntityUpdate) {
        // Discard the error: no receivers is fine.
        let _ = self.tx.send(update);
    }
}

impl EntityStore for MemoryStore {
    fn get(&self, id: &EntityId) -> Option<Entity> {
        self.map
            .read()
            .expect("MemoryStore RwLock poisoned")
            .get(id)
            .cloned()
    }

    fn for_each(&self, f: &mut dyn FnMut(&EntityId, &Entity)) {
        let guard = self.map.read().expect("MemoryStore RwLock poisoned");
        for (id, entity) in guard.iter() {
            f(id, entity);
        }
        // Lock is released here — the entire walk occurs under one read-lock
        // acquisition, consistent with the Phase 2 LiveStore contract.
    }

    fn subscribe(&self, _ids: &[EntityId]) -> broadcast::Receiver<EntityUpdate> {
        // Phase 1: subscribes to all updates regardless of the id filter.
        // Phase 2 will use the ids slice for server-side subscription filtering.
        self.tx.subscribe()
    }
}

// ---------------------------------------------------------------------------
// Compile-time bound test
// ---------------------------------------------------------------------------

/// Compile-time assertion that `S: EntityStore` implies `Send + Sync`.
///
/// Called from tests to exercise these lines under coverage instrumentation.
fn _assert_store<S: EntityStore>() {}

/// Concrete instantiation of the bound check against [`MemoryStore`].
///
/// Verifies at compile time that `MemoryStore: EntityStore + Send + Sync`.
/// Called from tests so that coverage instrumentation reaches these lines.
fn _assert_memory_store_satisfies_bound() {
    _assert_store::<MemoryStore>();
}

/// Compile-time proof that `EntityStore` is dyn-compatible.
///
/// Both `&dyn EntityStore` and `Arc<dyn EntityStore>` must be constructible
/// from a concrete `MemoryStore` reference.  Phase 2's `LiveStore` drop-in
/// depends on this: the Tokio task that receives entity updates will hold an
/// `Arc<dyn EntityStore>` so the bridge can be swapped without changing its
/// call sites.  This function is never called; it exists solely to verify the
/// invariant at build time.
fn _assert_dyn_entity_store_works() {
    fn _accepts_dyn(_: &dyn EntityStore) {}
    fn _accepts_arc_dyn(_: std::sync::Arc<dyn EntityStore>) {}
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
    use crate::ha::entity::Entity;

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
    // get
    // -----------------------------------------------------------------------

    #[test]
    fn get_returns_entity_for_known_id() {
        let entity = make_entity("light.kitchen", "on");
        let store = MemoryStore::load(vec![entity.clone()]).unwrap();

        let result = store.get(&EntityId::from("light.kitchen"));
        assert!(result.is_some());
        let got = result.unwrap();
        assert_eq!(got.id.as_str(), "light.kitchen");
        assert_eq!(&*got.state, "on");
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let store = MemoryStore::load(vec![]).unwrap();
        let result = store.get(&EntityId::from("light.unknown"));
        assert!(result.is_none());
    }

    #[test]
    fn get_clones_arc_fields_without_deep_copy() {
        let entity = make_entity("sensor.temp", "21.5");
        let store = MemoryStore::load(vec![entity]).unwrap();

        let a = store.get(&EntityId::from("sensor.temp")).unwrap();
        let b = store.get(&EntityId::from("sensor.temp")).unwrap();
        // Both clones share the same Arc allocation — pointer equality holds.
        assert!(Arc::ptr_eq(&a.attributes, &b.attributes));
        assert!(Arc::ptr_eq(&a.state as &Arc<str>, &b.state as &Arc<str>));
    }

    // -----------------------------------------------------------------------
    // for_each (visitor pattern)
    // -----------------------------------------------------------------------

    #[test]
    fn for_each_visits_all_entities() {
        let entities = vec![
            make_entity("light.a", "on"),
            make_entity("light.b", "off"),
            make_entity("sensor.c", "21"),
        ];
        let store = MemoryStore::load(entities).unwrap();

        let mut visited: Vec<String> = Vec::new();
        store.for_each(&mut |id, _entity| {
            visited.push(id.as_str().to_owned());
        });
        visited.sort();

        assert_eq!(visited, ["light.a", "light.b", "sensor.c"]);
    }

    #[test]
    fn for_each_on_empty_store_calls_visitor_zero_times() {
        let store = MemoryStore::load(vec![]).unwrap();
        let mut count = 0usize;
        store.for_each(&mut |_id, _entity| {
            count += 1;
        });
        assert_eq!(count, 0);
    }

    #[test]
    fn for_each_entity_references_are_valid() {
        let entity = make_entity("switch.outlet", "off");
        let store = MemoryStore::load(vec![entity]).unwrap();

        store.for_each(&mut |id, e| {
            assert_eq!(id.as_str(), "switch.outlet");
            assert_eq!(&*e.state, "off");
        });
    }

    // -----------------------------------------------------------------------
    // capacity-1 lag behavior
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn subscribe_returns_receiver_for_updates() {
        let store = MemoryStore::load(vec![make_entity("light.x", "on")]).unwrap();
        let mut rx = store.subscribe(&[EntityId::from("light.x")]);

        store.publish(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "off")),
        });

        let update = rx.recv().await.expect("expected an update");
        assert_eq!(update.id.as_str(), "light.x");
        assert!(update.entity.is_some());
        assert_eq!(&*update.entity.unwrap().state, "off");
    }

    #[tokio::test]
    async fn capacity_one_causes_lagged_error_on_second_event() {
        let store = MemoryStore::load(vec![make_entity("light.x", "on")]).unwrap();
        let mut rx = store.subscribe(&[EntityId::from("light.x")]);

        // Publish two events without consuming the first — the channel can only
        // buffer 1, so the first is overwritten and the receiver gets Lagged.
        store.publish(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "off")),
        });
        store.publish(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "on")),
        });

        // The receiver must get RecvError::Lagged, not the first message.
        match rx.recv().await {
            Err(RecvError::Lagged(_)) => {
                // Expected: bridge must now call store.get() to resync.
            }
            other => panic!("expected RecvError::Lagged, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn lagged_receiver_can_resync_via_get() {
        let store = Arc::new(MemoryStore::load(vec![make_entity("light.x", "on")]).unwrap());
        let mut rx = store.subscribe(&[EntityId::from("light.x")]);

        // Force lag.
        store.publish(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "off")),
        });
        store.publish(EntityUpdate {
            id: EntityId::from("light.x"),
            entity: Some(make_entity("light.x", "on")),
        });

        // Simulate bridge resync path after Lagged.
        let lagged = rx.recv().await;
        assert!(matches!(lagged, Err(RecvError::Lagged(_))));

        // Bridge calls get() to recover current state.
        let current = store.get(&EntityId::from("light.x"));
        assert!(current.is_some(), "get must return entity after lag");
        assert_eq!(&*current.unwrap().state, "on");
    }

    // -----------------------------------------------------------------------
    // cap enforcement
    // -----------------------------------------------------------------------

    #[test]
    fn load_rejects_fixture_exceeding_max_entities() {
        let cap = DEFAULT_PROFILE.max_entities;
        // Build cap + 1 entities.
        let entities: Vec<Entity> = (0..=cap)
            .map(|i| make_entity(&format!("light.e{i}"), "on"))
            .collect();

        let result = MemoryStore::load(entities);
        assert!(
            result.is_err(),
            "load must fail when entity count exceeds cap"
        );
        match result.unwrap_err() {
            MemoryStoreError::CapExceeded { count, cap: c } => {
                assert_eq!(count, cap + 1);
                assert_eq!(c, cap);
            }
        }
    }

    #[test]
    fn load_accepts_fixture_at_exactly_max_entities() {
        let cap = DEFAULT_PROFILE.max_entities;
        let entities: Vec<Entity> = (0..cap)
            .map(|i| make_entity(&format!("light.e{i}"), "on"))
            .collect();

        let result = MemoryStore::load(entities);
        assert!(result.is_ok(), "load must succeed at exactly the cap");
    }

    #[test]
    fn entity_update_none_signals_removal() {
        let update = EntityUpdate {
            id: EntityId::from("light.gone"),
            entity: None,
        };
        assert_eq!(update.id.as_str(), "light.gone");
        assert!(update.entity.is_none());
    }

    // -----------------------------------------------------------------------
    // Compile-time bound proof — coverage exercise
    // -----------------------------------------------------------------------

    #[test]
    fn entity_store_bound_proof_runs_clean() {
        // Calls _assert_memory_store_satisfies_bound so the compiler-proof
        // function body is reached under coverage instrumentation.
        // The call will not compile if MemoryStore stops implementing
        // EntityStore (or if EntityStore drops the Send + Sync requirement),
        // which is the intended regression guard.
        _assert_memory_store_satisfies_bound();
    }
}
