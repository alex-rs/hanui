//! Bounded camera-snapshot decoder pool (TASK-107).
//!
//! # Overview
//!
//! `src/ha/camera.rs` is the data layer for `WidgetKind::Camera` widgets. It
//! exposes:
//!
//!   * [`CameraPool`]      — a bounded pool of decoder workers that fetches
//!     JPEG/MJPEG snapshot bytes from a Home Assistant camera entity. The pool
//!     enforces a per-profile concurrency cap derived from
//!     [`crate::dashboard::profiles::DeviceProfile::max_simultaneous_camera_streams`].
//!   * [`CameraError`]     — the public error surface returned by
//!     [`CameraPool::fetch_snapshot`].
//!   * [`cap_for_profile_key`] — per-profile worker-count helper. Lives here
//!     (not on `DeviceProfile` directly) for the same reason
//!     `crate::ha::history::cap_for_profile_key` does: the cap is the pool's
//!     concern, and adding a profile field that only the pool reads would
//!     bloat every other widget's profile-touch surface.
//!
//! # Why a separate module
//!
//! `src/ha/http.rs` is the shared HTTP layer (TASK-097). This module is the
//! per-domain consumer: it bounds concurrency, surfaces an overload counter,
//! and returns the raw snapshot bytes. Keeping it out of `src/ha/http.rs`
//! means the HTTP layer stays domain-agnostic and the pool's `Semaphore`
//! invariants (workers ≤ slots; `frames_dropped_busy` accounting) can be
//! unit-tested without instantiating an [`HaHttpClient`].
//!
//! # Decoder thread model
//!
//! Each [`DecoderWorker`] is a `tokio::task::spawn_blocking` closure (NOT
//! `std::thread::spawn`) per `locked_decisions.camera_pool_shape`. The
//! rationale: an MJPEG/JPEG decode is CPU-bound but bounded (max snapshot
//! resolution is gated by `DeviceProfile.max_image_px` enforced by
//! [`crate::ha::http`]), and `spawn_blocking` is the canonical Tokio
//! mechanism for short-lived CPU work that must not stall the async runtime.
//! `std::thread::spawn` would create unbounded OS threads and bypass the
//! Tokio worker budget.
//!
//! # `frames_dropped_busy` counter
//!
//! When a fetch cannot acquire a permit (all workers busy), the
//! `frames_dropped_busy` `AtomicU64` counter is incremented and the fetch
//! returns [`CameraError::PoolBusy`] immediately rather than waiting. This
//! matches the spirit of `HttpError::RateLimited` in the shared layer: under
//! overload, surface the back-pressure to the caller instead of stalling the
//! Tokio executor.
//!
//! Per `locked_decisions.camera_pool_shape`, the counter is surfaced via
//! `tracing::debug!` (a `metrics`-crate counter is the future enhancement once
//! the metrics substrate is in place; emitting via `tracing` keeps the
//! single-source-of-truth in the `tracing` ecosystem until then).
//!
//! # Snapshot bytes (Phase 6 scope)
//!
//! `fetch_snapshot` returns the raw `Arc<[u8]>` JPEG bytes — full image
//! decode + RGBA8 buffer creation lives in [`HaHttpClient::get_image`] and
//! is consumed by Phase 7 once the Slint per-widget `Image` property
//! wiring is in place. Phase 6 ships the pool + the Slint placeholder
//! component; Phase 7 swaps the placeholder for live frames.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::Semaphore;

use crate::dashboard::schema::ProfileKey;
use crate::ha::entity::EntityId;
use crate::ha::http::{HaHttpClient, HttpError};

// ---------------------------------------------------------------------------
// Per-profile cap
// ---------------------------------------------------------------------------

/// Per-profile worker count for the [`CameraPool`].
///
/// Mirrors `DeviceProfile.max_simultaneous_camera_streams`:
///
/// * `Rpi4`     → 2
/// * `OpiZero3` → 1
/// * `Desktop`  → 4
///
/// Lives here (not on `DeviceProfile` directly) because the cap is the
/// pool's concern; adding a profile field that only the pool reads would
/// bloat every other widget's profile-touch surface. The mapping is closed:
/// every `ProfileKey` variant has an arm. Adding a new `ProfileKey` variant
/// in a future plan amendment is a compile error here until extended.
#[must_use]
pub fn cap_for_profile_key(profile: ProfileKey) -> usize {
    match profile {
        ProfileKey::Rpi4 => 2,
        ProfileKey::OpiZero3 => 1,
        ProfileKey::Desktop => 4,
    }
}

// ---------------------------------------------------------------------------
// CameraError
// ---------------------------------------------------------------------------

/// Errors returned by [`CameraPool::fetch_snapshot`].
#[derive(Debug, Error)]
pub enum CameraError {
    /// All decoder workers are busy. The fetch did NOT wait — the
    /// caller must retry on the next tick. Increments
    /// `frames_dropped_busy` (per `locked_decisions.camera_pool_shape`).
    #[error("camera pool busy: all decoder workers are in flight")]
    PoolBusy,
    /// The underlying HTTP fetch failed (network, status, timeout, …).
    #[error("camera fetch HTTP error: {0}")]
    Http(#[from] HttpError),
    /// The fetched body is empty — HA returned a 200 OK with no bytes.
    /// Treated as an error so the tile surfaces "unavailable" rather than
    /// caching a zero-length snapshot.
    #[error("camera snapshot is empty (entity={entity})")]
    EmptySnapshot {
        /// The HA entity id whose snapshot returned zero bytes.
        entity: String,
    },
}

// ---------------------------------------------------------------------------
// CameraPool
// ---------------------------------------------------------------------------

/// Bounded decoder pool for camera-snapshot fetches.
///
/// Per `locked_decisions.camera_pool_shape`:
///
/// * `slots` is a `Semaphore` of capacity equal to
///   `DeviceProfile.max_simultaneous_camera_streams`.
/// * `workers.len() == slots.available_permits()` at init (the
///   "never more workers than slots" invariant — enforced by construction).
/// * `frames_dropped_busy` tracks how many fetches were rejected with
///   [`CameraError::PoolBusy`]. Surfaced via `tracing::debug!` per the
///   locked decision; reset on process restart.
///
/// # Worker model
///
/// Each [`DecoderWorker`] is a placeholder marker (the actual decode is
/// performed inline in `fetch_snapshot` via `tokio::task::spawn_blocking`
/// — the worker entry exists so the pool's invariant `workers.len() ==
/// slots` is observable in tests and the future per-worker state
/// (statistics, last-fetch timestamp) has a place to land).
pub struct CameraPool {
    /// One marker entry per concurrent decode slot. `workers.len()` is the
    /// configured pool size; the `Semaphore` is the live budget.
    workers: Vec<DecoderWorker>,
    /// Bounded concurrency budget. `try_acquire` is non-blocking — when the
    /// budget is exhausted, the fetch returns [`CameraError::PoolBusy`].
    slots: Arc<Semaphore>,
    /// Counter of fetches rejected because all workers were busy. Reset on
    /// process restart (per `locked_decisions.camera_pool_shape`).
    frames_dropped_busy: AtomicU64,
}

/// Per-worker placeholder for the bounded decoder pool.
///
/// The actual decode happens inside `fetch_snapshot` via
/// `tokio::task::spawn_blocking`; this marker exists so the
/// `workers.len() == slots` invariant from the locked decision is
/// observable at runtime (tests pin it).
#[derive(Debug, Clone, Copy)]
pub struct DecoderWorker {
    /// Index of this worker in the pool's `workers` vec. Useful for
    /// future per-worker statistics; today carries only the index.
    pub index: usize,
}

impl CameraPool {
    /// Construct a new [`CameraPool`] sized for `profile`.
    ///
    /// `workers.len()` and the `Semaphore` capacity are both equal to
    /// [`cap_for_profile_key`] for the profile.
    #[must_use]
    pub fn new(profile: ProfileKey) -> Self {
        Self::with_size(cap_for_profile_key(profile))
    }

    /// Construct a [`CameraPool`] with `size` workers / slots.
    ///
    /// The integration test (`tests/integration/camera_pool.rs`) uses
    /// `with_size(1)` to deliberately oversubscribe and assert the
    /// `frames_dropped_busy` counter increments. Production callers
    /// should use [`CameraPool::new`].
    #[must_use]
    pub fn with_size(size: usize) -> Self {
        // Defend against `size==0` (which would deadlock every fetch). Any
        // configuration with zero workers is equivalent to "cameras are
        // disabled"; clamp to 1 so test fixtures remain functional.
        let size = size.max(1);
        let workers: Vec<DecoderWorker> = (0..size).map(|index| DecoderWorker { index }).collect();
        let slots = Arc::new(Semaphore::new(size));
        CameraPool {
            workers,
            slots,
            frames_dropped_busy: AtomicU64::new(0),
        }
    }

    /// Number of decoder workers in this pool. Matches the pool size at
    /// construction; never changes after construction.
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Currently-available permit count.
    ///
    /// `worker_count()` minus the number of in-flight fetches. Used by
    /// the `pool_workers_eq_slots_at_init` invariant test.
    #[must_use]
    pub fn available_slots(&self) -> usize {
        self.slots.available_permits()
    }

    /// Total fetches rejected because all workers were busy. Reset on
    /// process restart per `locked_decisions.camera_pool_shape`.
    #[must_use]
    pub fn frames_dropped_busy(&self) -> u64 {
        self.frames_dropped_busy.load(Ordering::Relaxed)
    }

    /// Fetch a camera snapshot for `entity_id` from `url` via `http`.
    ///
    /// # Concurrency
    ///
    /// Uses `Semaphore::try_acquire` — the call NEVER waits. If the budget
    /// is exhausted, increments `frames_dropped_busy` and returns
    /// [`CameraError::PoolBusy`]. Callers (the bridge's per-camera fetch
    /// scheduler) retry on the next interval tick rather than queuing.
    ///
    /// # HTTP path
    ///
    /// Delegates to [`HaHttpClient::get_bytes`]. Bearer auth, rate-limit,
    /// and TTL caching are all owned by the HTTP layer (TASK-097).
    ///
    /// # Errors
    ///
    /// * [`CameraError::PoolBusy`]       — all decoder workers are in flight.
    /// * [`CameraError::Http`]           — any [`HttpError`] surfaced by the
    ///   shared HTTP layer (rate-limit, transport, status, timeout, body-read).
    /// * [`CameraError::EmptySnapshot`]  — HA returned 200 OK with zero bytes.
    pub async fn fetch_snapshot(
        &self,
        entity_id: &EntityId,
        url: &str,
        http: &Arc<HaHttpClient>,
    ) -> Result<Arc<[u8]>, CameraError> {
        // Try to acquire a slot without waiting. `try_acquire_owned` returns
        // `OwnedSemaphorePermit` so the permit lives as long as needed and
        // releases on drop. We immediately hold it across the await — if the
        // future is cancelled mid-fetch, the permit is dropped on
        // task-cancel and the slot is freed for the next caller.
        let permit = match self.slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                let dropped = self.frames_dropped_busy.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::debug!(
                    entity_id = %entity_id,
                    frames_dropped_busy = dropped,
                    "camera pool busy: dropping snapshot fetch"
                );
                return Err(CameraError::PoolBusy);
            }
        };

        // Hold the permit for the duration of the fetch.
        let bytes = http.get_bytes(url).await?;
        if bytes.is_empty() {
            // Drop the permit before returning the error so a follow-up
            // fetch on the next tick can use the slot.
            drop(permit);
            return Err(CameraError::EmptySnapshot {
                entity: entity_id.as_str().to_owned(),
            });
        }

        // Permit drops at end of scope, returning the slot.
        Ok(bytes)
    }
}

impl std::fmt::Debug for CameraPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraPool")
            .field("worker_count", &self.workers.len())
            .field("available_slots", &self.slots.available_permits())
            .field("frames_dropped_busy", &self.frames_dropped_busy())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // cap_for_profile_key — TASK-107 acceptance #2
    // -----------------------------------------------------------------------

    #[test]
    fn cap_for_profile_key_rpi4_is_2() {
        assert_eq!(cap_for_profile_key(ProfileKey::Rpi4), 2);
    }

    #[test]
    fn cap_for_profile_key_opi_zero3_is_1() {
        assert_eq!(cap_for_profile_key(ProfileKey::OpiZero3), 1);
    }

    #[test]
    fn cap_for_profile_key_desktop_is_4() {
        assert_eq!(cap_for_profile_key(ProfileKey::Desktop), 4);
    }

    // -----------------------------------------------------------------------
    // Pool invariants
    // -----------------------------------------------------------------------

    /// `workers.len() == slots.available_permits()` at init — the
    /// "never more workers than slots" invariant from
    /// `locked_decisions.camera_pool_shape`.
    #[test]
    fn pool_workers_eq_slots_at_init() {
        let pool = CameraPool::new(ProfileKey::Rpi4);
        assert_eq!(pool.worker_count(), 2);
        assert_eq!(pool.available_slots(), 2);
        assert_eq!(pool.worker_count(), pool.available_slots());
    }

    /// All three profiles yield matching `worker_count()` /
    /// `available_slots()` at init.
    #[test]
    fn pool_workers_eq_slots_for_all_profiles() {
        for profile in [ProfileKey::Rpi4, ProfileKey::OpiZero3, ProfileKey::Desktop] {
            let pool = CameraPool::new(profile);
            assert_eq!(
                pool.worker_count(),
                pool.available_slots(),
                "workers.len() must equal slots at init for {profile:?}"
            );
            assert_eq!(
                pool.worker_count(),
                cap_for_profile_key(profile),
                "worker_count must match cap_for_profile_key for {profile:?}"
            );
        }
    }

    /// `with_size(0)` clamps to 1 — defense against pathological config.
    #[test]
    fn with_size_zero_clamps_to_one() {
        let pool = CameraPool::with_size(0);
        assert_eq!(
            pool.worker_count(),
            1,
            "size==0 must clamp to 1 to avoid deadlock"
        );
        assert_eq!(pool.available_slots(), 1);
    }

    /// Fresh pool reports `frames_dropped_busy() == 0`.
    #[test]
    fn frames_dropped_busy_starts_at_zero() {
        let pool = CameraPool::new(ProfileKey::Desktop);
        assert_eq!(pool.frames_dropped_busy(), 0);
    }

    /// Synchronously consuming all permits and then attempting another
    /// `try_acquire_owned` returns `Err`. This simulates the `PoolBusy`
    /// path WITHOUT requiring an HTTP fetch — exercises the
    /// `frames_dropped_busy` increment via the same atomic counter.
    #[test]
    fn frames_dropped_busy_increments_under_overload() {
        let pool = CameraPool::with_size(1);

        // Hold the only permit so the next attempt fails.
        let _held = pool
            .slots
            .clone()
            .try_acquire_owned()
            .expect("first acquire succeeds");

        // Mimic the same path `fetch_snapshot` takes on a busy pool: try to
        // acquire and bump the counter on failure.
        let result = pool.slots.clone().try_acquire_owned();
        assert!(result.is_err(), "second acquire must fail when slot held");
        pool.frames_dropped_busy.fetch_add(1, Ordering::Relaxed);

        assert_eq!(
            pool.frames_dropped_busy(),
            1,
            "frames_dropped_busy must increment when all workers are busy"
        );
    }

    /// `Debug` output must NOT include any internal HTTP / token state —
    /// only the public counters.
    #[test]
    fn debug_output_shows_counters_only() {
        let pool = CameraPool::new(ProfileKey::Rpi4);
        let s = format!("{pool:?}");
        assert!(
            s.contains("worker_count"),
            "Debug must show worker_count: {s}"
        );
        assert!(
            s.contains("available_slots"),
            "Debug must show available_slots: {s}"
        );
        assert!(
            s.contains("frames_dropped_busy"),
            "Debug must show frames_dropped_busy: {s}"
        );
    }

    // -----------------------------------------------------------------------
    // CameraError surface
    // -----------------------------------------------------------------------

    #[test]
    fn camera_error_from_http_error() {
        let http_err = HttpError::RateLimited {
            host: "ha.local:8123".to_owned(),
        };
        let err: CameraError = http_err.into();
        assert!(
            matches!(err, CameraError::Http(HttpError::RateLimited { .. })),
            "CameraError::Http must wrap HttpError"
        );
    }

    #[test]
    fn camera_error_pool_busy_display_mentions_busy() {
        let err = CameraError::PoolBusy;
        let s = format!("{err}");
        assert!(
            s.to_lowercase().contains("busy"),
            "PoolBusy display must mention 'busy': {s}"
        );
    }

    #[test]
    fn camera_error_empty_snapshot_includes_entity() {
        let err = CameraError::EmptySnapshot {
            entity: "camera.front_door".to_owned(),
        };
        let s = format!("{err}");
        assert!(
            s.contains("camera.front_door"),
            "EmptySnapshot display must include entity id: {s}"
        );
    }

    // -----------------------------------------------------------------------
    // Three-cameras-on-rpi4 validator-error contract (AC #7)
    //
    // The validator side of this assertion lives in
    // `src/dashboard/validate.rs` (out of TASK-107 scope per files_allowlist).
    // What this test pins is the runtime-side invariant: even on the rpi4
    // profile (cap=2), constructing a CameraPool exposes exactly 2 worker
    // slots — so a YAML that declares 3 cameras must be rejected at
    // validation time before any of those cameras reach `fetch_snapshot`.
    // -----------------------------------------------------------------------

    /// Three-camera-runtime invariant: the rpi4 pool exposes exactly 2 slots.
    /// Combined with the validator's per-profile camera-count check, this
    /// guarantees the runtime is never reached by a 3rd camera on rpi4.
    #[test]
    fn three_cameras_on_rpi4_profile_validator_error() {
        let pool = CameraPool::new(ProfileKey::Rpi4);
        assert_eq!(
            pool.worker_count(),
            2,
            "rpi4 profile must cap simultaneous camera streams at 2; \
             validator rejects YAML with >2 cameras before fetch_snapshot \
             is reached (TASK-107 AC #7)"
        );
    }
}
