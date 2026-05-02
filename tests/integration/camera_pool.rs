//! Integration tests for [`hanui::ha::camera::CameraPool`] (TASK-107).
//!
//! These tests exercise the bounded-pool overload contract from
//! `locked_decisions.camera_pool_shape` + Risk #4 across the public API
//! surface — without requiring a live HTTP endpoint.
//!
//! * `pool_workers_eq_slots_at_init` — at construction, the worker count
//!   matches the available semaphore permits (the "never more workers
//!   than slots" invariant).
//! * `overload_increments_counter` — when concurrent fetches exceed the
//!   pool size, the rejected fetches surface [`CameraError::PoolBusy`]
//!   and `frames_dropped_busy` increments. Driven against an unreachable
//!   HTTP endpoint (port 1, the IANA-reserved "tcpmux" slot that has no
//!   listener) so we exercise the production code path end-to-end.
//!
//! The HTTP fetch path itself is unit-tested in `src/ha/http.rs`; here we
//! exercise the pool's permit accounting via the same `fetch_snapshot`
//! call site production callers will use.

use std::sync::{Arc, Mutex as StdMutex};

use hanui::dashboard::profiles::PROFILE_DESKTOP;
use hanui::dashboard::schema::ProfileKey;
use hanui::ha::camera::{cap_for_profile_key, CameraError, CameraPool};
use hanui::ha::entity::EntityId;
use hanui::ha::http::HaHttpClient;
use hanui::platform::config::Config;

// ---------------------------------------------------------------------------
// Env serialisation (shared with other integration tests in this binary)
// ---------------------------------------------------------------------------

/// Serialise env-mutation tests to avoid races between parallel test
/// threads. Other integration tests in this binary (loader, ws_client,
/// command_tx) hold a `static ENV_LOCK` of their own; ours is independent
/// and held for the duration of the `Config::from_env` call only.
static ENV_LOCK: StdMutex<()> = StdMutex::new(());

/// Build a `Config` pointing at `url` (typically `127.0.0.1:1` so the
/// connect attempt fails fast). Equivalent to the `make_config` helper in
/// `tests/integration/command_tx.rs`; duplicated here to keep this test
/// file self-contained.
fn make_config(url: &str, token: &str) -> Config {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: serialised by ENV_LOCK; the lock is held only for the
    // synchronous env mutation + Config::from_env parse and dropped
    // before the first `.await`.
    unsafe {
        std::env::set_var("HA_URL", url);
        std::env::set_var("HA_TOKEN", token);
    }
    let config = Config::from_env().expect("test config");
    unsafe {
        std::env::remove_var("HA_URL");
        std::env::remove_var("HA_TOKEN");
    }
    config
}

// ---------------------------------------------------------------------------
// Worker count == slots at init (locked_decisions.camera_pool_shape)
// ---------------------------------------------------------------------------

/// At construction, `workers.len() == slots.available_permits()` for every
/// profile. This is the "never more workers than slots" invariant from
/// `locked_decisions.camera_pool_shape`.
#[test]
fn pool_workers_eq_slots_at_init() {
    for profile in [ProfileKey::Rpi4, ProfileKey::OpiZero3, ProfileKey::Desktop] {
        let pool = CameraPool::new(profile);
        let expected = cap_for_profile_key(profile);
        assert_eq!(
            pool.worker_count(),
            expected,
            "worker_count must match cap_for_profile_key for {profile:?}"
        );
        assert_eq!(
            pool.available_slots(),
            expected,
            "available_slots must match cap_for_profile_key for {profile:?}"
        );
        assert_eq!(
            pool.worker_count(),
            pool.available_slots(),
            "workers.len() must equal slots at init for {profile:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Overload increments frames_dropped_busy (Risk #4 mitigation)
// ---------------------------------------------------------------------------

/// Two concurrent camera fetches against a pool sized 1 cause at least one
/// of them to surface [`CameraError::PoolBusy`] and increment the
/// `frames_dropped_busy` counter — the per-Risk #4 acceptance contract.
///
/// The test binds a local TCP listener that accepts connections but never
/// writes a response. The first fetch's HTTP connect succeeds and then
/// blocks waiting for response bytes (holding the pool's only permit
/// throughout). The second fetch tries to acquire while the permit is
/// held, hits `try_acquire_owned` failure, and bumps the counter.
///
/// The test asserts on the counter and the `CameraError::PoolBusy`
/// variant — the underlying HTTP failure mode of the holding fetch
/// (timeout / aborted) is not part of TASK-107's contract.
#[tokio::test]
async fn overload_increments_counter() {
    // Bind a TCP listener that accepts but never responds. The OS
    // picks a free port via `:0`. Spawn an acceptor task that stashes
    // every accepted stream in a Vec so the connection stays open
    // (dropping the stream would close the connection and unblock the
    // first fetch immediately).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind localhost listener");
    let bound = listener.local_addr().expect("local_addr");
    let _accept_task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _peer)) = listener.accept().await {
            held.push(stream);
        }
    });

    // Tighten timeouts so even the holding fetch returns within a
    // bounded window — keeps the test fast while still leaving plenty
    // of time for the second fetch to attempt acquire and fail.
    let mut profile = PROFILE_DESKTOP;
    profile.http_request_timeout_ms = 1_500;
    profile.http_retry_budget = 0;

    let config = Arc::new(make_config(
        "ws://127.0.0.1:1/api/websocket",
        "stub-camera-pool-test-token",
    ));
    let http = Arc::new(HaHttpClient::new(config, &profile));

    // Pool sized 1: only one decoder can run at a time.
    let pool = Arc::new(CameraPool::with_size(1));
    assert_eq!(
        pool.frames_dropped_busy(),
        0,
        "fresh pool starts with frames_dropped_busy=0"
    );

    let entity = EntityId::from("camera.front_door");
    let url = format!("http://{bound}/snapshot.jpg");

    // Spawn task A. It holds the permit through the HTTP fetch (which
    // blocks on the never-responding listener until the request_timeout).
    let pool_a = Arc::clone(&pool);
    let http_a = Arc::clone(&http);
    let entity_a = entity.clone();
    let url_a = url.clone();
    let task_a =
        tokio::spawn(async move { pool_a.fetch_snapshot(&entity_a, &url_a, &http_a).await });

    // Deterministic synchronisation: poll `available_slots()` until it
    // drops to 0 — that is the OBSERVABLE signal that task A has acquired
    // the permit (it sits inside `fetch_snapshot` past the
    // `try_acquire_owned` call and is now in the HTTP fetch). This avoids
    // the latent timing race a hard sleep would carry on a loaded CI host.
    //
    // Bound the wait at 1.5s — well under the 1500ms request_timeout, so
    // task A's permit is still held when we proceed. If we time out
    // here, the test fails with a clear diagnostic instead of producing
    // a non-deterministic PoolBusy / not-PoolBusy outcome.
    let acquired_deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1_500);
    while pool.available_slots() != 0 {
        if tokio::time::Instant::now() >= acquired_deadline {
            panic!(
                "task A did not acquire the pool permit within 1500ms; \
                 available_slots={}; frames_dropped_busy={}",
                pool.available_slots(),
                pool.frames_dropped_busy()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Permit is now held by task A. Task B's fetch must hit PoolBusy.
    let res_b = pool.fetch_snapshot(&entity, &url, &http).await;

    // Assert task B surfaces PoolBusy deterministically (no race).
    assert!(
        matches!(&res_b, Err(CameraError::PoolBusy)),
        "task B must surface CameraError::PoolBusy when permit is held; got {res_b:?}"
    );

    // The counter must have incremented to exactly 1 — the deterministic
    // synchronisation above means we know task A acquired and task B was
    // the only fetch that hit the busy path.
    assert_eq!(
        pool.frames_dropped_busy(),
        1,
        "frames_dropped_busy must be exactly 1 after one PoolBusy fetch; got {}",
        pool.frames_dropped_busy()
    );

    // Drain task A so its eventual error doesn't leak into the test
    // runner output. The result is irrelevant — we asserted on task B
    // and the counter.
    let _ = task_a.await;
}
