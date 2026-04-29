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

use jiff::Timestamp;
use tokio::sync::{broadcast, mpsc};

use crate::actions::map::{WidgetActionMap, WidgetId};
use crate::ha::client::OutboundCommand;
use crate::ha::entity::{Entity, EntityId};
use crate::ha::services::{ServiceMeta, ServiceRegistry, ServiceRegistryHandle};
use crate::ha::store::{EntityStore, EntityUpdate};

// ---------------------------------------------------------------------------
// Optimistic UI (TASK-064)
// ---------------------------------------------------------------------------

/// Default per-entity cap on concurrent optimistic entries
/// (`locked_decisions.backpressure`).
pub const DEFAULT_PER_ENTITY_OPTIMISTIC_CAP: usize = 4;

/// Default global cap on concurrent optimistic entries across all entities
/// (`locked_decisions.backpressure`).
pub const DEFAULT_GLOBAL_OPTIMISTIC_CAP: usize = 64;

/// One in-flight optimistic UI prediction for a dispatched action.
///
/// The dispatcher (TASK-064) creates an `OptimisticEntry` immediately after
/// pushing an `OutboundCommand` to the WS client. The Slint rendering layer
/// (TASK-067) consults [`LiveStore::pending_for_widget`] to drive the per-tile
/// pending spinner; the dispatcher's reconciliation task uses the entry's
/// fields to decide whether to drop, hold, or revert based on inbound HA
/// events and the service-call ack.
///
/// # Field semantics
///
/// * `entity_id` — the HA entity the action targets. Multiple entries may
///   live under the same `entity_id` when a burst of taps fires (subject to
///   [`DEFAULT_PER_ENTITY_OPTIMISTIC_CAP`]).
/// * `request_id` — a dispatcher-allocated, monotonic identity used to
///   correlate the entry with its reconciliation task. Per
///   `locked_decisions.ws_command_ack_envelope` the dispatcher does NOT see
///   the WS-client-allocated id, so this field is the dispatcher's local
///   identity (deterministic for tests, opaque to HA).
/// * `dispatched_at` — wall-clock timestamp captured at dispatch time. The
///   reconciliation key is `entity.last_changed > entry.dispatched_at`
///   (`locked_decisions.optimistic_reconciliation_key`); ANY HA `state_changed`
///   that updates `last_changed` past this point is treated as the
///   confirming truth and the entry is dropped (rule 1).
/// * `tentative_state` — what the optimistic update predicts. Rule 2
///   (ack-success without state_changed) compares this against the current
///   entity state at ack time to decide drop-vs-hold.
/// * `prior_state` — the entity state at dispatch time. Rule 4 (ack-error)
///   and rule 5 (timeout) revert to this value. Per
///   `locked_decisions.action_timing` `LastWriteWins`, when a second gesture
///   fires while the first is pending, the new entry's `prior_state` is the
///   cancelled entry's `prior_state` (chain-root preservation, NOT the
///   cancelled entry's `tentative_state`).
#[derive(Debug, Clone)]
pub struct OptimisticEntry {
    /// HA entity this action targets.
    pub entity_id: EntityId,
    /// Dispatcher-local monotonic identity (NOT the WS client's id).
    pub request_id: u32,
    /// Wall-clock dispatch timestamp; reconciliation compares against
    /// `entity.last_changed`.
    pub dispatched_at: Timestamp,
    /// What the optimistic update predicts (e.g. `"on"` after a Toggle on
    /// an off light).
    pub tentative_state: Arc<str>,
    /// Pre-dispatch state value; revert target on ack-error / timeout.
    pub prior_state: Arc<str>,
}

/// Outcome reported by [`LiveStore::insert_optimistic_entry`] when capacity
/// is saturated (`locked_decisions.backpressure`).
///
/// The dispatcher maps this onto a `BackpressureRejected` error and emits a
/// toast event on the toast channel — never silently drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimisticInsertError {
    /// The per-entity cap (default [`DEFAULT_PER_ENTITY_OPTIMISTIC_CAP`])
    /// has been reached for this entity.
    PerEntityCap,
    /// The global cap (default [`DEFAULT_GLOBAL_OPTIMISTIC_CAP`]) has been
    /// reached across all entities.
    GlobalCap,
}

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

    /// In-flight optimistic UI predictions, keyed by `entity_id` (TASK-064).
    ///
    /// Populated by the dispatcher's [`Self::insert_optimistic_entry`] after a
    /// successful `OutboundCommand` push, drained by reconciliation rules
    /// (success / error / timeout / state_changed) and by the
    /// `LastWriteWins` cancellation path. The Slint rendering layer
    /// (TASK-067) reads [`Self::pending_for_widget`] to drive the per-tile
    /// spinner.
    ///
    /// Multiple entries may live under one `entity_id` (rapid taps); the
    /// outer cap is [`Self::per_entity_optimistic_cap`] entries per entity
    /// and [`Self::global_optimistic_cap`] across all entities.
    optimistic: RwLock<HashMap<EntityId, Vec<OptimisticEntry>>>,

    /// Per-entity cap on concurrent optimistic entries
    /// (`locked_decisions.backpressure`). Defaults to
    /// [`DEFAULT_PER_ENTITY_OPTIMISTIC_CAP`].
    per_entity_optimistic_cap: usize,

    /// Global cap on concurrent optimistic entries across all entities
    /// (`locked_decisions.backpressure`). Defaults to
    /// [`DEFAULT_GLOBAL_OPTIMISTIC_CAP`].
    global_optimistic_cap: usize,

    /// Optional `WidgetActionMap` snapshot used to resolve
    /// [`Self::pending_for_widget`] queries from `WidgetId` → `EntityId`.
    ///
    /// Set once at startup via [`Self::set_widget_action_map`]; clones are
    /// cheap (`Arc`). When unset, [`Self::pending_for_widget`] returns
    /// `false` for every input — TASK-067's spinner sees "no pending" until
    /// the orchestrator wires the map.
    widget_action_map: RwLock<Option<Arc<WidgetActionMap>>>,
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
            optimistic: RwLock::new(HashMap::new()),
            per_entity_optimistic_cap: DEFAULT_PER_ENTITY_OPTIMISTIC_CAP,
            global_optimistic_cap: DEFAULT_GLOBAL_OPTIMISTIC_CAP,
            widget_action_map: RwLock::new(None),
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

    // -----------------------------------------------------------------------
    // Optimistic UI (TASK-064)
    // -----------------------------------------------------------------------

    /// Configure the per-entity cap on concurrent optimistic entries.
    ///
    /// Builder-style; chains with [`Self::with_global_optimistic_cap`].
    /// Default is [`DEFAULT_PER_ENTITY_OPTIMISTIC_CAP`] per
    /// `locked_decisions.backpressure`. Phase 4 `DeviceProfile` may override.
    #[must_use]
    pub fn with_per_entity_optimistic_cap(mut self, cap: usize) -> Self {
        self.per_entity_optimistic_cap = cap;
        self
    }

    /// Configure the global cap on concurrent optimistic entries.
    ///
    /// Builder-style; chains with [`Self::with_per_entity_optimistic_cap`].
    /// Default is [`DEFAULT_GLOBAL_OPTIMISTIC_CAP`] per
    /// `locked_decisions.backpressure`. Phase 4 `DeviceProfile` may override.
    #[must_use]
    pub fn with_global_optimistic_cap(mut self, cap: usize) -> Self {
        self.global_optimistic_cap = cap;
        self
    }

    /// Returns the current per-entity optimistic-entry cap.
    #[must_use]
    pub fn per_entity_optimistic_cap(&self) -> usize {
        self.per_entity_optimistic_cap
    }

    /// Returns the current global optimistic-entry cap.
    #[must_use]
    pub fn global_optimistic_cap(&self) -> usize {
        self.global_optimistic_cap
    }

    /// Install the dashboard `WidgetActionMap` so [`Self::pending_for_widget`]
    /// can resolve `WidgetId → EntityId`.
    ///
    /// Wired once at startup by `src/lib.rs` after the dashboard view spec is
    /// loaded. The Slint rendering layer (TASK-067) does not call this — it
    /// only reads [`Self::pending_for_widget`].
    pub fn set_widget_action_map(&self, map: Arc<WidgetActionMap>) {
        let mut guard = self
            .widget_action_map
            .write()
            .expect("LiveStore widget_action_map RwLock poisoned");
        *guard = Some(map);
    }

    /// **Cross-owner read API consumed by TASK-067 (per-tile spinner).**
    ///
    /// Returns `true` if any [`OptimisticEntry`] currently exists for the
    /// entity bound to `widget_id` (resolved via the previously-installed
    /// [`WidgetActionMap`]). Returns `false` when the widget has no entry in
    /// the map, no map has been installed yet, or the entity has zero
    /// pending optimistic entries.
    ///
    /// Per `locked_decisions.pending_state_read_api`, this is the **single**
    /// pending-state read API the slint-engineer binds to. TASK-067's spinner
    /// visibility binds to this method's return value, NOT a parallel
    /// pending-state path. Codex review 2026-04-28 caught the cross-owner
    /// risk (#14) of a parallel API diverging.
    #[must_use]
    pub fn pending_for_widget(&self, widget_id: &WidgetId) -> bool {
        // 1. Resolve widget_id → entity_id via the installed
        //    `WidgetActionMap`. Cloning the `Arc` releases the read-lock
        //    before the map lookup so concurrent
        //    `set_widget_action_map` writes are not blocked on this read.
        let map_arc = {
            let guard = self
                .widget_action_map
                .read()
                .expect("LiveStore widget_action_map RwLock poisoned");
            match guard.as_ref() {
                Some(arc) => Arc::clone(arc),
                None => return false,
            }
        };
        let Some(entry) = map_arc.lookup(widget_id) else {
            return false;
        };
        // 2. Test for any optimistic entry on that entity_id.
        let guard = self
            .optimistic
            .read()
            .expect("LiveStore optimistic RwLock poisoned");
        guard
            .get(&entry.entity_id)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// Insert a new optimistic entry, enforcing per-entity and global caps.
    ///
    /// Returns `Ok(())` on success; `Err(OptimisticInsertError::PerEntityCap)`
    /// or `Err(OptimisticInsertError::GlobalCap)` when capacity is saturated
    /// (`locked_decisions.backpressure`). The dispatcher converts either
    /// `Err` into `DispatchError::BackpressureRejected` plus a toast event —
    /// never silently drops.
    pub fn insert_optimistic_entry(
        &self,
        entry: OptimisticEntry,
    ) -> Result<(), OptimisticInsertError> {
        let mut guard = self
            .optimistic
            .write()
            .expect("LiveStore optimistic RwLock poisoned");

        // Global cap: total across all entities (Σ vec lengths).
        let global_total: usize = guard.values().map(|v| v.len()).sum();
        if global_total >= self.global_optimistic_cap {
            return Err(OptimisticInsertError::GlobalCap);
        }

        let bucket = guard.entry(entry.entity_id.clone()).or_default();
        if bucket.len() >= self.per_entity_optimistic_cap {
            return Err(OptimisticInsertError::PerEntityCap);
        }
        bucket.push(entry);
        Ok(())
    }

    /// Remove the optimistic entry with `(entity_id, request_id)` if present.
    ///
    /// Returns `Some(entry)` if the entry was found and removed (so the
    /// caller can trigger any side effects — e.g. emit a revert broadcast),
    /// `None` if the entry was not present (already drained by another
    /// reconciliation path, or cancelled by `LastWriteWins`).
    pub fn drop_optimistic_entry(
        &self,
        entity_id: &EntityId,
        request_id: u32,
    ) -> Option<OptimisticEntry> {
        let mut guard = self
            .optimistic
            .write()
            .expect("LiveStore optimistic RwLock poisoned");
        let bucket = guard.get_mut(entity_id)?;
        let pos = bucket.iter().position(|e| e.request_id == request_id)?;
        let removed = bucket.remove(pos);
        if bucket.is_empty() {
            guard.remove(entity_id);
        }
        Some(removed)
    }

    /// Drop and return ALL optimistic entries for `entity_id`.
    ///
    /// Used by the dispatcher's `LastWriteWins` cancellation path: a second
    /// gesture on the same widget cancels the pending entries (returning them
    /// so the new entry's `prior_state` can preserve the chain root). The
    /// new entry's `prior_state` is the FIRST cancelled entry's `prior_state`
    /// per `locked_decisions.action_timing` (the chain root, NOT the most
    /// recent cancelled `tentative_state`).
    pub fn drop_all_optimistic_entries(&self, entity_id: &EntityId) -> Vec<OptimisticEntry> {
        let mut guard = self
            .optimistic
            .write()
            .expect("LiveStore optimistic RwLock poisoned");
        guard.remove(entity_id).unwrap_or_default()
    }

    /// Returns `true` if an optimistic entry with `(entity_id, request_id)`
    /// is currently present.
    ///
    /// Used by the dispatcher's reconciliation task to detect whether its
    /// entry has already been removed (by an inbound `state_changed` event
    /// or by a `LastWriteWins` cancellation) before deciding whether to
    /// revert.
    #[must_use]
    pub fn has_optimistic_entry(&self, entity_id: &EntityId, request_id: u32) -> bool {
        let guard = self
            .optimistic
            .read()
            .expect("LiveStore optimistic RwLock poisoned");
        guard
            .get(entity_id)
            .map(|v| v.iter().any(|e| e.request_id == request_id))
            .unwrap_or(false)
    }

    /// Snapshot the current pending entries for `entity_id` (test/diagnostic).
    #[must_use]
    pub fn optimistic_entries_for(&self, entity_id: &EntityId) -> Vec<OptimisticEntry> {
        let guard = self
            .optimistic
            .read()
            .expect("LiveStore optimistic RwLock poisoned");
        guard.get(entity_id).cloned().unwrap_or_default()
    }

    /// Total number of optimistic entries across all entities (test/diagnostic).
    #[must_use]
    pub fn optimistic_total(&self) -> usize {
        let guard = self
            .optimistic
            .read()
            .expect("LiveStore optimistic RwLock poisoned");
        guard.values().map(|v| v.len()).sum()
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
    ///
    /// # Optimistic UI reconciliation (TASK-064)
    ///
    /// Per `locked_decisions.optimistic_reconciliation_key`, any optimistic
    /// entry on this entity whose `dispatched_at` is strictly less than the
    /// inbound entity's `last_changed` is dropped (rule 1: ack-success WITH
    /// state_changed). Attribute-only events leave entries intact (rule 3:
    /// `last_changed` does not advance for attribute-only updates).
    /// Removal events (`update.entity == None`) do NOT drop entries — that
    /// path is taken when HA reports the entity disappeared, which is not
    /// the optimistic-reconciliation seam (the entry will time out via
    /// rule 5).
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

        // Reconciliation rule 1 (ack-success WITH state_changed): drop any
        // optimistic entries on this entity whose `dispatched_at` predates
        // the inbound `last_changed`. Rule 3 (attribute-only state_changed)
        // is captured by the strict-greater-than: an attribute-only event
        // carries the SAME `last_changed`, so no entries are dropped.
        if let Some(ref entity) = update.entity {
            let new_last_changed = entity.last_changed;
            let mut guard = self
                .optimistic
                .write()
                .expect("LiveStore optimistic RwLock poisoned");
            if let Some(bucket) = guard.get_mut(&update.id) {
                bucket.retain(|entry| entry.dispatched_at >= new_last_changed);
                if bucket.is_empty() {
                    guard.remove(&update.id);
                }
            }
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
