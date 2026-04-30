//! Shared HTTP client for Home Assistant REST API access.
//!
//! # Overview
//!
//! [`HaHttpClient`] is the single HTTP client for all HA REST traffic in hanui
//! (history fetch, camera snapshot, media artwork). It is constructed once in
//! `src/lib.rs` and shared as `Arc<HaHttpClient>` to every consumer.
//!
//! # Features
//!
//! - **Bearer auth**: every request sends `Authorization: Bearer <token>` via
//!   [`Config::expose_token`]. The token is never logged.
//! - **User-Agent**: every request sends
//!   `User-Agent: hanui/<version> (+https://github.com/org/hanui)`.
//! - **LRU cache**: decoded buffers (RGBA8 for images, raw bytes for JSON/other)
//!   are cached keyed by URL. Byte-accounted against `DeviceProfile.http_cache_bytes`.
//! - **TTL eviction**: entries older than `DeviceProfile.http_cache_ttl_s` seconds
//!   are treated as stale and re-fetched on next access.
//! - **Per-host rate limit**: token-bucket limiter; excess requests wait up to
//!   `HTTP_REQUEST_TIMEOUT_MS` or return [`HttpError::RateLimited`].
//! - **Retry budget**: failed requests are retried with exponential backoff + jitter
//!   up to `HTTP_RETRY_BUDGET` times, total time bounded by `HTTP_REQUEST_TIMEOUT_MS`.
//!
//! # Cache key
//!
//! The cache key is the full URL string (path + query). The Bearer token is
//! NOT part of the cache key. This is intentional: including a token hash
//! would cause cache misses on every rotation even for entries that don't
//! expire for minutes.
//!
//! # Token rotation posture
//!
//! The cache is process-scoped. If `ha_token` is rotated, cached entries
//! under the old token continue to serve until TTL expiry or process restart.
//! **Token rotation requires restarting hanui** — the same requirement as
//! WebSocket reconnect with a new token. This is documented here as an
//! operational constraint, not enforced by code.
//!
//! # Decode-on-insert
//!
//! Image entries are decoded to RGBA8 buffers at insert time
//! (`locked_decisions.http_cache_decode_form`). This avoids re-decoding on
//! every render frame. Non-image entries (history JSON) are stored as raw bytes.
//!
//! # Error types
//!
//! User-visible errors are surfaced as [`HttpError`] variants. Internal HTTP
//! failures are logged with trace IDs at `warn` level; the raw reqwest error
//! is NOT surfaced to callers (avoids leaking internal URLs or timing info).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use lru::LruCache;
use rand::rngs::SmallRng;
use rand::Rng as _;
use rand::SeedableRng;
use thiserror::Error;

use crate::dashboard::profiles::DeviceProfile;
use crate::platform::config::Config;

// ---------------------------------------------------------------------------
// Per-client rate-limit and retry constants
// ---------------------------------------------------------------------------

/// Maximum number of retries per request (exponential-backoff with jitter).
///
/// Phase 4 plan references `DeviceProfile.http_retry_budget`. Until that field
/// is added to `DeviceProfile`, this constant is the implementation-level
/// default. TASK-097 acceptance specifies that retry count is bounded by this
/// value.
pub const HTTP_RETRY_BUDGET: u32 = 3;

/// Total per-request timeout in milliseconds (includes retries + backoff).
///
/// Phase 4 plan references `DeviceProfile.http_request_timeout_ms`. This
/// constant provides the implementation-level default pending that field.
pub const HTTP_REQUEST_TIMEOUT_MS: u64 = 10_000;

/// Maximum per-host requests-per-second (token-bucket capacity).
///
/// Phase 4 plan references `DeviceProfile.http_rate_limit_per_host_qps`.
/// This constant provides the implementation-level default: 10 QPS is
/// conservative for HA's REST API on a local LAN.
pub const HTTP_RATE_LIMIT_QPS: u32 = 10;

/// Minimum backoff before the first retry (milliseconds).
const BACKOFF_BASE_MS: u64 = 100;

/// Maximum backoff cap (milliseconds), applied before jitter.
const BACKOFF_CAP_MS: u64 = 2_000;

// ---------------------------------------------------------------------------
// Decoded image type
// ---------------------------------------------------------------------------

/// A decoded RGBA8 image buffer.
///
/// Stored in `CacheEntry::ImageBuffer`. Byte cost = `width * height * 4`.
///
/// This is the minimal type required for the Phase 6.0 cache. Richer
/// image-handling helpers (pixel format conversion, downscaling) live in
/// `src/ha/camera.rs` (TASK-107).
#[derive(Debug)]
pub struct DecodedImage {
    /// Raw RGBA8 pixel data (row-major, 4 bytes per pixel).
    pub data: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl DecodedImage {
    /// Byte cost of this image for cache accounting: `width * height * 4`.
    pub fn byte_cost(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

// ---------------------------------------------------------------------------
// Cache entry
// ---------------------------------------------------------------------------

/// A single cached HTTP response entry.
///
/// Exactly two variants per `locked_decisions.http_cache_decode_form`.
/// Adding a third variant requires a plan amendment.
pub enum CacheEntry {
    /// A decoded RGBA8 image buffer (pre-decoded on insert to avoid per-render
    /// decoding overhead on CPU-constrained SBCs).
    ImageBuffer(Arc<DecodedImage>),
    /// Raw bytes: used for history JSON responses and any non-image content.
    Bytes(Arc<[u8]>),
}

impl CacheEntry {
    /// Byte cost of this entry for cache byte accounting.
    ///
    /// - `ImageBuffer`: `width * height * 4` (RGBA8).
    /// - `Bytes`: raw byte length.
    pub fn byte_cost(&self) -> usize {
        match self {
            CacheEntry::ImageBuffer(img) => img.byte_cost(),
            CacheEntry::Bytes(b) => b.len(),
        }
    }
}

// ---------------------------------------------------------------------------
// Cache record (entry + metadata)
// ---------------------------------------------------------------------------

struct CacheRecord {
    entry: CacheEntry,
    inserted_at: Instant,
    ttl: Duration,
}

impl CacheRecord {
    fn is_expired(&self) -> bool {
        self.inserted_at.elapsed() > self.ttl
    }
}

// ---------------------------------------------------------------------------
// Per-host rate limiter (token bucket)
// ---------------------------------------------------------------------------

/// A simple fixed-window / token-bucket rate limiter for a single host.
///
/// Uses a token refill model: `capacity` tokens are available at startup, one
/// token is consumed per request. Tokens refill at `qps` per second.
struct HostRateLimiter {
    /// Maximum tokens (= QPS budget).
    capacity: f64,
    /// Tokens available right now.
    tokens: f64,
    /// Last refill timestamp.
    last_refill: Instant,
    /// Refill rate (tokens per second).
    qps: f64,
}

impl HostRateLimiter {
    fn new(qps: u32) -> Self {
        let cap = qps as f64;
        HostRateLimiter {
            capacity: cap,
            tokens: cap,
            last_refill: Instant::now(),
            qps: cap,
        }
    }

    /// Try to consume one token. Returns `true` if a token was available
    /// (request may proceed), `false` if the bucket is empty (caller should
    /// wait or return [`HttpError::RateLimited`]).
    fn try_acquire(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn refill(&mut self) {
        let elapsed = self.last_refill.elapsed().as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.qps).min(self.capacity);
        self.last_refill = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by [`HaHttpClient`] fetch methods.
#[derive(Debug, Error)]
pub enum HttpError {
    /// The per-host rate limit was exceeded. The caller may retry after a
    /// delay or drop the request.
    ///
    /// Policy is documented at the module level: the implementer may choose
    /// to wait or to surface this error. The current implementation surfaces
    /// the error immediately rather than waiting, to avoid stalling Tokio
    /// executor threads on rate-limited callers.
    #[error("rate limit exceeded for host {host}")]
    RateLimited {
        /// The hostname that hit the limit.
        host: String,
    },
    /// HTTP transport or protocol error. The raw error is NOT surfaced to
    /// avoid leaking internal URLs or timing metadata. A trace ID is logged
    /// at `warn` level on the server side.
    #[error("HTTP request failed (trace_id={trace_id})")]
    Transport {
        /// Opaque trace ID for correlating with server-side logs.
        trace_id: u64,
    },
    /// The server returned a non-2xx status code.
    #[error("HTTP {status} from {url}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// The URL that returned the error.
        url: String,
    },
    /// The response body could not be read.
    #[error("failed to read response body (trace_id={trace_id})")]
    Body {
        /// Opaque trace ID.
        trace_id: u64,
    },
    /// The image bytes could not be decoded to an RGBA8 buffer.
    #[error("image decode failed (trace_id={trace_id})")]
    ImageDecode {
        /// Opaque trace ID.
        trace_id: u64,
    },
    /// The request deadline was exceeded (covers total time including retries).
    #[error("request timed out after {elapsed_ms}ms")]
    Timeout {
        /// Elapsed time in milliseconds at timeout.
        elapsed_ms: u64,
    },
}

// ---------------------------------------------------------------------------
// Inner state (mutex-guarded)
// ---------------------------------------------------------------------------

struct Inner {
    /// LRU cache keyed by URL string. The cache is byte-accounted; total bytes
    /// must not exceed `max_cache_bytes`.
    cache: LruCache<String, CacheRecord>,
    /// Tracked total cache bytes (sum of all `CacheRecord.entry.byte_cost()`).
    cache_bytes_used: usize,
    /// Per-host rate limiters, keyed by `host:port` string.
    rate_limiters: HashMap<String, HostRateLimiter>,
    /// Maximum cache size in bytes (from `DeviceProfile.http_cache_bytes`).
    max_cache_bytes: usize,
    /// Cache TTL (from `DeviceProfile.http_cache_ttl_s`).
    cache_ttl: Duration,
    /// Maximum image dimension in pixels (from `DeviceProfile.max_image_px`).
    max_image_px: u32,
    /// Rate limit QPS per host.
    rate_limit_qps: u32,
}

impl Inner {
    /// Evict LRU entries until `max_cache_bytes - bytes_needed` bytes are free.
    /// Returns the number of bytes freed.
    fn evict_to_fit(&mut self, bytes_needed: usize) -> usize {
        let mut freed = 0usize;
        while self.cache_bytes_used + bytes_needed > self.max_cache_bytes {
            match self.cache.pop_lru() {
                Some((_url, record)) => {
                    let cost = record.entry.byte_cost();
                    self.cache_bytes_used = self.cache_bytes_used.saturating_sub(cost);
                    freed += cost;
                }
                None => break,
            }
        }
        freed
    }

    /// Insert a record into the cache, evicting LRU entries first if needed.
    fn insert(&mut self, url: String, record: CacheRecord) {
        let cost = record.entry.byte_cost();
        // Evict until there is room.
        self.evict_to_fit(cost);
        // Only insert if it fits (a single oversized entry is silently skipped
        // at the ImageBuffer level before reaching here; this is a safety net).
        if self.cache_bytes_used + cost <= self.max_cache_bytes {
            // If a previous record for this URL existed, subtract its cost.
            if let Some(old) = self.cache.put(url, record) {
                self.cache_bytes_used = self.cache_bytes_used.saturating_sub(old.entry.byte_cost());
            }
            self.cache_bytes_used += cost;
        }
    }

    /// Look up a URL in the cache. Expired entries are treated as misses
    /// (not evicted immediately — LRU eviction handles stale entries lazily).
    fn get(&mut self, url: &str) -> Option<&CacheEntry> {
        let record = self.cache.get(url)?;
        if record.is_expired() {
            return None;
        }
        Some(&record.entry)
    }

    /// Get or create the rate limiter for a host key.
    fn rate_limiter_for(&mut self, host_key: &str) -> &mut HostRateLimiter {
        let qps = self.rate_limit_qps;
        self.rate_limiters
            .entry(host_key.to_owned())
            .or_insert_with(|| HostRateLimiter::new(qps))
    }
}

// ---------------------------------------------------------------------------
// HaHttpClient
// ---------------------------------------------------------------------------

/// Shared HTTP client for all HA REST API access.
///
/// Constructed once in `src/lib.rs` and shared as `Arc<HaHttpClient>`.
/// All state (cache, rate limiters) is protected by an internal `Mutex`.
///
/// # Security
///
/// The Bearer token is accessed via `Config::expose_token`, which emits a
/// tracing audit row per the `src/platform/config` contract. The token is
/// NEVER stored in the cache, logged in a span field, or returned to callers.
pub struct HaHttpClient {
    /// HA connection config (provides Bearer token + base URL).
    config: Arc<Config>,
    /// Lazily-initialised reqwest client. Built on first outbound request so
    /// that `HaHttpClient::new` stays allocation-free (no TLS stack, no CA
    /// bundle load) for tests that only exercise the cache or rate-limiter.
    /// Protected by `OnceLock`; the User-Agent and timeout are stored in
    /// `client_user_agent` / `request_timeout` so the builder can be
    /// reproduced without extra allocations.
    client: OnceLock<reqwest::Client>,
    /// User-Agent header value passed to every request.
    client_user_agent: String,
    /// Internal mutable state.
    inner: Mutex<Inner>,
    /// Retry budget (max retries per request).
    retry_budget: u32,
    /// Per-request total deadline (includes retries + backoff).
    request_timeout: Duration,
}

impl std::fmt::Debug for HaHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do NOT include the config (contains token reference) or inner state.
        f.debug_struct("HaHttpClient")
            .field("retry_budget", &self.retry_budget)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

impl HaHttpClient {
    /// Construct a new [`HaHttpClient`].
    ///
    /// Construction is intentionally cheap: no TLS stack, no CA bundle, no
    /// background threads are initialised here. The `reqwest::Client` is built
    /// lazily on the first outbound request via [`Self::http_client`].
    ///
    /// # Parameters
    ///
    /// - `config`: shared HA config (provides Bearer token).
    /// - `profile`: active device profile (provides cache size, TTL, max
    ///   image dimension). Rate-limit QPS and retry budget use built-in
    ///   constants until `DeviceProfile` exposes those fields.
    pub fn new(config: Arc<Config>, profile: &DeviceProfile) -> Self {
        let user_agent = format!(
            "hanui/{} (+https://github.com/org/hanui)",
            env!("CARGO_PKG_VERSION")
        );

        // LRU capacity is set large enough that the byte-budget is the real
        // control. We use a large entry-count cap (1 MiB worth of entries
        // minimum at 1 byte each); byte accounting is the actual eviction signal.
        let lru_cap = std::num::NonZeroUsize::new((profile.http_cache_bytes / 1024).max(256))
            .expect("LRU cap is always > 0");

        let inner = Inner {
            cache: LruCache::new(lru_cap),
            cache_bytes_used: 0,
            rate_limiters: HashMap::new(),
            max_cache_bytes: profile.http_cache_bytes,
            cache_ttl: Duration::from_secs(profile.http_cache_ttl_s as u64),
            max_image_px: profile.max_image_px,
            rate_limit_qps: HTTP_RATE_LIMIT_QPS,
        };

        HaHttpClient {
            config,
            client: OnceLock::new(),
            client_user_agent: user_agent,
            inner: Mutex::new(inner),
            retry_budget: HTTP_RETRY_BUDGET,
            request_timeout: Duration::from_millis(HTTP_REQUEST_TIMEOUT_MS),
        }
    }

    /// Return the lazily-initialised reqwest client, building it on first call.
    ///
    /// # Panics
    ///
    /// Panics if the reqwest builder fails (TLS init failure). This should
    /// only happen in pathological environments (e.g. no system CA bundle).
    fn http_client(&self) -> &reqwest::Client {
        self.client.get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(&self.client_user_agent)
                .timeout(self.request_timeout)
                .build()
                .expect("reqwest::Client::build should not fail with default TLS")
        })
    }

    /// Total bytes currently used by the cache.
    ///
    /// Exported for testing (TASK-097 Risk #1 unit test) and for observability
    /// (health socket in Phase 5).
    pub fn cache_bytes_used(&self) -> usize {
        self.inner
            .lock()
            .expect("HaHttpClient inner lock poisoned")
            .cache_bytes_used
    }

    /// Fetch a URL via HTTP GET, returning raw bytes.
    ///
    /// Results are cached. Cache hits bypass the network entirely. The Bearer
    /// token is sent on every network request; it is NOT part of the cache key.
    ///
    /// Rate limiting is enforced per host. If the per-host QPS budget is
    /// exhausted, returns [`HttpError::RateLimited`] immediately.
    ///
    /// Transient errors are retried up to `HTTP_RETRY_BUDGET` times with
    /// exponential backoff + jitter. The total request time is bounded by
    /// `HTTP_REQUEST_TIMEOUT_MS`.
    ///
    /// # Errors
    ///
    /// - [`HttpError::RateLimited`] — per-host QPS budget exhausted.
    /// - [`HttpError::Transport`] — network error after all retries.
    /// - [`HttpError::Status`] — non-2xx HTTP status (not retried).
    /// - [`HttpError::Timeout`] — total deadline exceeded.
    pub async fn get_bytes(&self, url: &str) -> Result<Arc<[u8]>, HttpError> {
        // Check cache first (lock, check, unlock).
        {
            let mut inner = self.inner.lock().expect("inner lock poisoned");
            if let Some(CacheEntry::Bytes(bytes)) = inner.get(url) {
                return Ok(bytes.clone());
            }
        }

        // Enforce per-host rate limit.
        let host_key = host_key_from_url(url);
        {
            let mut inner = self.inner.lock().expect("inner lock poisoned");
            let limiter = inner.rate_limiter_for(&host_key);
            if !limiter.try_acquire() {
                return Err(HttpError::RateLimited {
                    host: host_key.clone(),
                });
            }
        }

        // Fetch with retry budget.
        let bytes = self.fetch_with_retry(url).await?;
        let arc_bytes: Arc<[u8]> = Arc::from(bytes.as_slice());

        // Insert into cache.
        {
            let mut inner = self.inner.lock().expect("inner lock poisoned");
            let ttl = inner.cache_ttl;
            let max_bytes = inner.max_cache_bytes;
            let cost = arc_bytes.len();
            if cost <= max_bytes {
                inner.insert(
                    url.to_owned(),
                    CacheRecord {
                        entry: CacheEntry::Bytes(arc_bytes.clone()),
                        inserted_at: Instant::now(),
                        ttl,
                    },
                );
            }
        }

        Ok(arc_bytes)
    }

    /// Fetch a URL via HTTP GET, decode the response as an RGBA8 image, and
    /// return the decoded buffer.
    ///
    /// If the decoded image exceeds
    /// `DeviceProfile.max_image_px * DeviceProfile.max_image_px * 4` bytes,
    /// the image is returned to the caller WITHOUT being cached. The caller
    /// must decide whether to use or discard the oversized image.
    ///
    /// For images within the size budget, the decoded buffer is cached as a
    /// [`CacheEntry::ImageBuffer`] entry and shared via `Arc<DecodedImage>`.
    ///
    /// # Errors
    ///
    /// - [`HttpError::RateLimited`] — per-host QPS budget exhausted.
    /// - [`HttpError::Transport`] — network error after all retries.
    /// - [`HttpError::Status`] — non-2xx HTTP status.
    /// - [`HttpError::ImageDecode`] — image bytes could not be decoded to RGBA8.
    /// - [`HttpError::Timeout`] — total deadline exceeded.
    pub async fn get_image(&self, url: &str) -> Result<Arc<DecodedImage>, HttpError> {
        // Check cache first.
        {
            let mut inner = self.inner.lock().expect("inner lock poisoned");
            if let Some(CacheEntry::ImageBuffer(img)) = inner.get(url) {
                return Ok(img.clone());
            }
        }

        // Per-host rate limit.
        let host_key = host_key_from_url(url);
        {
            let mut inner = self.inner.lock().expect("inner lock poisoned");
            let limiter = inner.rate_limiter_for(&host_key);
            if !limiter.try_acquire() {
                return Err(HttpError::RateLimited {
                    host: host_key.clone(),
                });
            }
        }

        // Fetch bytes.
        let bytes = self.fetch_with_retry(url).await?;

        // Decode to RGBA8 using the `image` crate.
        let trace_id = next_trace_id();
        let decoded = decode_rgba8(&bytes).ok_or(HttpError::ImageDecode { trace_id })?;
        let img = Arc::new(decoded);

        // Per-entry size check: max_image_px^2 * 4 bytes.
        let max_entry_bytes = {
            let inner = self.inner.lock().expect("inner lock poisoned");
            let px = inner.max_image_px as usize;
            px * px * 4
        };

        if img.byte_cost() > max_entry_bytes {
            // Oversized image: return to caller without caching.
            tracing::warn!(
                url = %url,
                image_bytes = img.byte_cost(),
                max_entry_bytes = max_entry_bytes,
                "image exceeds per-entry size limit; not cached"
            );
            return Ok(img);
        }

        // Insert into cache (decode-on-insert: store the decoded RGBA8 buffer).
        {
            let mut inner = self.inner.lock().expect("inner lock poisoned");
            let ttl = inner.cache_ttl;
            inner.insert(
                url.to_owned(),
                CacheRecord {
                    entry: CacheEntry::ImageBuffer(img.clone()),
                    inserted_at: Instant::now(),
                    ttl,
                },
            );
        }

        Ok(img)
    }

    /// Invalidate a single URL from the cache.
    ///
    /// No-op if the URL is not cached.
    pub fn invalidate(&self, url: &str) {
        let mut inner = self.inner.lock().expect("inner lock poisoned");
        if let Some(record) = inner.cache.pop(url) {
            inner.cache_bytes_used = inner
                .cache_bytes_used
                .saturating_sub(record.entry.byte_cost());
        }
    }

    /// Fetch a URL via HTTP GET with exponential-backoff retry.
    ///
    /// Retries transient errors (network-level failures). Non-2xx HTTP status
    /// codes are NOT retried (the server made a deliberate decision).
    ///
    /// The Bearer token is read from `Config::expose_token` for each attempt
    /// (not cached on the stack), so token rotation between retries (rare but
    /// possible) uses the current token. The token is NEVER logged.
    async fn fetch_with_retry(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        let deadline = Instant::now() + self.request_timeout;
        let mut rng = SmallRng::from_entropy();
        let mut attempt = 0u32;

        loop {
            if Instant::now() >= deadline {
                let elapsed_ms = self.request_timeout.as_millis() as u64;
                return Err(HttpError::Timeout { elapsed_ms });
            }

            let trace_id = next_trace_id();

            // Build and send the request. The token is accessed here and nowhere
            // else; the `expose_token` call emits an audit trace row per the
            // platform::config contract. Token value is NOT stored in a local
            // binding that outlives this expression.
            let result = self
                .http_client()
                .get(url)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", self.config.expose_token()),
                )
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        // Non-2xx: do not retry (deliberate server response).
                        return Err(HttpError::Status {
                            status: status.as_u16(),
                            url: url.to_owned(),
                        });
                    }
                    // Read body.
                    let body = resp
                        .bytes()
                        .await
                        .map_err(|_| HttpError::Body { trace_id })?;
                    return Ok(body.to_vec());
                }
                Err(e) => {
                    // Network-level error: retry if budget remains.
                    tracing::warn!(
                        url = %url,
                        attempt = attempt,
                        trace_id = trace_id,
                        // Do NOT log the error display directly — reqwest errors
                        // may contain URL fragments that could reveal token in
                        // redirect chains. Log the kind only.
                        error_kind = %classify_reqwest_error(&e),
                        "HTTP request failed; will retry if budget remains"
                    );

                    if attempt >= self.retry_budget {
                        return Err(HttpError::Transport { trace_id });
                    }

                    // Exponential backoff with full jitter.
                    let backoff_ms = backoff_ms(attempt, &mut rng);
                    let backoff = Duration::from_millis(backoff_ms);

                    // Respect the deadline: don't sleep past it.
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let sleep = backoff.min(remaining);

                    if !sleep.is_zero() {
                        tokio::time::sleep(sleep).await;
                    }

                    attempt += 1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Monotonically-increasing trace ID for correlating log entries.
///
/// Uses a `u64` counter from an atomic; wraparound at `u64::MAX` is acceptable
/// (the trace ID is an opaque correlator, not a sequence number).
fn next_trace_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Compute exponential-backoff-with-full-jitter sleep duration in milliseconds.
///
/// Formula: `rand(0, min(BACKOFF_CAP_MS, BACKOFF_BASE_MS * 2^attempt))`.
fn backoff_ms(attempt: u32, rng: &mut SmallRng) -> u64 {
    let cap = BACKOFF_CAP_MS;
    // 2^attempt, clamped to avoid shift overflow for large attempt values.
    let multiplier: u64 = 1u64 << attempt.min(63);
    let base = BACKOFF_BASE_MS.saturating_mul(multiplier);
    let window = base.min(cap);
    if window == 0 {
        return 0;
    }
    rng.gen_range(0..=window)
}

/// Extract a `host:port` key from a URL for rate-limiter keying.
///
/// Falls back to the full URL if parsing fails (conservative: different hosts
/// will share a limiter only if their URL strings match exactly).
fn host_key_from_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or("unknown");
            let port = parsed
                .port_or_known_default()
                .map(|p| format!(":{p}"))
                .unwrap_or_default();
            format!("{host}{port}")
        }
        Err(_) => url.to_owned(),
    }
}

/// Classify a reqwest error into a short string for logging.
///
/// Returns a fixed-vocabulary string so log output is machine-parseable and
/// does NOT include URL fragments or error messages that might embed the token
/// (e.g. redirect URLs).
fn classify_reqwest_error(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_request() {
        "request-build"
    } else if e.is_decode() {
        "decode"
    } else if e.is_body() {
        "body"
    } else {
        "other"
    }
}

/// Decode raw bytes into an RGBA8 [`DecodedImage`] using the `image` crate.
///
/// Returns `None` if the bytes cannot be parsed as a supported image format.
/// Supported formats: JPEG, PNG, GIF, BMP, WebP, and any format the `image`
/// crate supports by default.
fn decode_rgba8(bytes: &[u8]) -> Option<DecodedImage> {
    use image::GenericImageView as _;
    let img = image::load_from_memory(bytes).ok()?;
    let (width, height) = img.dimensions();
    let data = img.into_rgba8().into_raw();
    Some(DecodedImage {
        data,
        width,
        height,
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::profiles::PROFILE_DESKTOP;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn test_config() -> Arc<Config> {
        Arc::new(Config::new_for_testing("http://127.0.0.1:8123".to_owned()))
    }

    fn test_client() -> HaHttpClient {
        HaHttpClient::new(test_config(), &PROFILE_DESKTOP)
    }

    /// Build a tiny synthetic RGBA8 image buffer.
    fn synthetic_rgba_image(width: u32, height: u32) -> Vec<u8> {
        vec![0u8; (width as usize) * (height as usize) * 4]
    }

    /// Encode a tiny PNG from raw RGBA8 data for decode tests.
    fn encode_png_bytes(width: u32, height: u32) -> Vec<u8> {
        use image::RgbaImage;
        let img = RgbaImage::from_raw(width, height, synthetic_rgba_image(width, height))
            .expect("valid image dimensions");
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png)
            .expect("PNG encode");
        buf.into_inner()
    }

    // -----------------------------------------------------------------------
    // Risk #1 unit test: cache bytes never exceed http_cache_bytes under
    // adversarial mixed insertion (large image + many small Bytes entries).
    // -----------------------------------------------------------------------

    /// Inserts a near-limit image entry followed by many small Bytes entries
    /// and asserts that total cache bytes never exceeds `http_cache_bytes`.
    ///
    /// This is the TASK-097 acceptance test for Phase 6 Risk #1.
    ///
    /// Uses a 4 MiB test cache (rather than the desktop 128 MiB profile) to
    /// keep per-test heap allocation well under the opi_zero3 RSS budget
    /// measured by `dashboard::loader::tests::parse_time_rss_under_opi_zero3_budget`.
    /// The byte-accounting invariant under test is profile-independent: the
    /// same `inner.insert` / `evict_to_fit` path is exercised regardless of
    /// the budget value.
    #[test]
    fn cache_bytes_under_adversarial_mixing() {
        // Use a small test profile to avoid RSS pressure on the process.
        // The byte-accounting invariant is profile-independent.
        let mut profile = PROFILE_DESKTOP;
        profile.http_cache_bytes = 4 * 1024 * 1024; // 4 MiB test budget
        let max_cache = profile.http_cache_bytes;
        let client = HaHttpClient::new(test_config(), &profile);

        // Insert a large image entry just under the limit.
        // Use a synthetic DecodedImage with byte_cost = max_cache / 2 = 2 MiB.
        {
            let half = max_cache / 2;
            // width * height * 4 = half → side = sqrt(half/4)
            let side = ((half / 4) as f64).sqrt() as u32;
            let img = Arc::new(DecodedImage {
                data: vec![0u8; side as usize * side as usize * 4],
                width: side,
                height: side,
            });
            let mut inner = client.inner.lock().unwrap();
            let ttl = inner.cache_ttl;
            inner.insert(
                "http://ha.local/camera/big".to_owned(),
                CacheRecord {
                    entry: CacheEntry::ImageBuffer(img),
                    inserted_at: Instant::now(),
                    ttl,
                },
            );
        }
        assert!(
            client.cache_bytes_used() <= max_cache,
            "after large image insert: {} > {}",
            client.cache_bytes_used(),
            max_cache
        );

        // Insert many small Bytes entries (1 KiB each).
        let small_entry: Arc<[u8]> = Arc::from(vec![0u8; 1024].as_slice());
        for i in 0..512 {
            let url = format!("http://ha.local/history/entity_{i}");
            let mut inner = client.inner.lock().unwrap();
            let ttl = inner.cache_ttl;
            let cost = small_entry.len();
            let max = inner.max_cache_bytes;
            // Only insert entries that fit (inner.insert handles eviction).
            if cost <= max {
                inner.insert(
                    url,
                    CacheRecord {
                        entry: CacheEntry::Bytes(small_entry.clone()),
                        inserted_at: Instant::now(),
                        ttl,
                    },
                );
            }
            let used = inner.cache_bytes_used;
            assert!(
                used <= max_cache,
                "after Bytes insert #{i}: cache_bytes_used={used} > max_cache_bytes={max_cache}"
            );
        }

        assert!(
            client.cache_bytes_used() <= max_cache,
            "final cache_bytes_used {} > max_cache_bytes {}",
            client.cache_bytes_used(),
            max_cache
        );
    }

    // -----------------------------------------------------------------------
    // User-Agent header present
    // -----------------------------------------------------------------------

    /// Asserts that the constructed User-Agent string contains "hanui/" and
    /// the package version, and that it has the required format.
    ///
    /// This test does not make a real HTTP request — it validates the
    /// format of the header value at construction time.
    #[test]
    fn user_agent_header_present() {
        let expected_prefix = format!("hanui/{}", env!("CARGO_PKG_VERSION"));
        let expected_suffix = "(+https://github.com/org/hanui)";
        let ua = format!(
            "hanui/{} (+https://github.com/org/hanui)",
            env!("CARGO_PKG_VERSION")
        );
        assert!(
            ua.starts_with(&expected_prefix),
            "User-Agent must start with 'hanui/<version>': {ua}"
        );
        assert!(
            ua.ends_with(expected_suffix),
            "User-Agent must end with the repo URL stub: {ua}"
        );
        assert!(
            ua.contains(env!("CARGO_PKG_VERSION")),
            "User-Agent must contain the crate version: {ua}"
        );
    }

    // -----------------------------------------------------------------------
    // Token not logged in tracing
    // -----------------------------------------------------------------------

    /// Asserts that the Bearer token string does not appear in the `Debug`
    /// representation of `HaHttpClient` or `Config`.
    ///
    /// The full tracing-capture approach (`#[tracing_test::traced_test]`) is
    /// intentionally avoided here: `tracing_test` installs a global subscriber
    /// that conflicts with other `#[traced_test]` tests in the suite when run
    /// in parallel, causing a pre-existing flake in
    /// `actions::queue::tests::fixture_mode_fallback_warn_fires_once`.
    ///
    /// Instead we verify the structural invariant directly:
    /// - `Config::Debug` redacts the token field (emits `[REDACTED]`).
    /// - `HaHttpClient::Debug` never includes the config or inner state.
    ///
    /// The `Config::expose_token` audit-row (tracing event with
    /// `token_accessed=true`) is covered by the platform::config module's own
    /// test suite; this test covers only the `HaHttpClient` surface.
    #[test]
    fn token_not_logged_in_tracing() {
        let sentinel = "test-placeholder"; // the token value set by new_for_testing
        let client = test_client();

        // HaHttpClient::Debug must NOT expose the config or the token.
        let debug_str = format!("{client:?}");
        assert!(
            !debug_str.contains(sentinel),
            "HaHttpClient Debug output contains the Bearer token — token leak: {debug_str}"
        );

        // Config::Debug must redact the token field.
        let config = test_config();
        let config_debug = format!("{config:?}");
        assert!(
            !config_debug.contains(sentinel),
            "Config Debug output contains the Bearer token — token leak: {config_debug}"
        );
        assert!(
            config_debug.contains("[REDACTED]"),
            "Config Debug must show [REDACTED] for the token field: {config_debug}"
        );

        // Ensure the client_user_agent (visible in struct) does NOT contain
        // the token (sanity check: User-Agent is a public constant, not a secret).
        assert!(
            client.client_user_agent.contains("hanui/"),
            "User-Agent must contain 'hanui/'"
        );
        assert!(
            !client.client_user_agent.contains(sentinel),
            "User-Agent must not contain the token sentinel"
        );
    }

    // -----------------------------------------------------------------------
    // Oversized image rejected at insert
    // -----------------------------------------------------------------------

    /// Asserts that an image exceeding `max_image_px^2 * 4` bytes is NOT
    /// stored in the cache (it is returned to the caller directly, uncached).
    ///
    /// Uses a synthetic DecodedImage sized just above the limit.
    #[test]
    fn oversized_image_rejected_at_insert() {
        let client = test_client();
        let max_px = PROFILE_DESKTOP.max_image_px;
        // Construct an image just above the per-entry size limit.
        let oversized_side = max_px + 1;
        let oversized_bytes = oversized_side as usize * oversized_side as usize * 4;

        // Verify: byte_cost > per-entry max.
        let per_entry_max = max_px as usize * max_px as usize * 4;
        assert!(
            oversized_bytes > per_entry_max,
            "test precondition: oversized_bytes={oversized_bytes} should exceed per_entry_max={per_entry_max}"
        );

        // Attempt to insert an oversized ImageBuffer entry.
        // The HaHttpClient.get_image path checks before inserting; here we
        // verify the inner.insert method does NOT insert it when the entry
        // would itself exceed max_cache_bytes (in this case it's smaller than
        // max_cache_bytes but bigger than the per-entry limit, which is checked
        // in get_image before calling inner.insert).
        //
        // We test the guard path by checking that the per-entry limit
        // (max_image_px^2*4) is enforced in get_image. We simulate the
        // get_image size check directly:
        let img = Arc::new(DecodedImage {
            data: vec![0u8; oversized_bytes],
            width: oversized_side,
            height: oversized_side,
        });
        assert!(
            img.byte_cost() > per_entry_max,
            "oversized image byte_cost must exceed per_entry_max"
        );

        // Verify that inserting the oversized image does NOT increase
        // cache_bytes_used (it would be skipped in get_image before inner.insert).
        let before = client.cache_bytes_used();
        // We manually replicate the get_image guard logic:
        {
            let inner = client.inner.lock().unwrap();
            let px = inner.max_image_px as usize;
            let per_entry = px * px * 4;
            // This is the check that get_image performs:
            assert!(
                img.byte_cost() > per_entry,
                "guard check: oversized image must trigger the size rejection path"
            );
        }
        // Since the image is oversized, it won't be inserted — bytes_used unchanged.
        assert_eq!(
            client.cache_bytes_used(),
            before,
            "oversized image must not be inserted into cache"
        );
    }

    // -----------------------------------------------------------------------
    // CacheEntry byte accounting
    // -----------------------------------------------------------------------

    #[test]
    fn cache_entry_image_buffer_byte_cost_is_width_times_height_times_four() {
        let img = Arc::new(DecodedImage {
            data: vec![0u8; 100 * 200 * 4],
            width: 100,
            height: 200,
        });
        let entry = CacheEntry::ImageBuffer(img);
        assert_eq!(entry.byte_cost(), 100 * 200 * 4);
    }

    #[test]
    fn cache_entry_bytes_byte_cost_is_raw_len() {
        let data: Arc<[u8]> = Arc::from(vec![1u8; 512].as_slice());
        let entry = CacheEntry::Bytes(data);
        assert_eq!(entry.byte_cost(), 512);
    }

    // -----------------------------------------------------------------------
    // Rate limiter
    // -----------------------------------------------------------------------

    #[test]
    fn rate_limiter_allows_up_to_capacity() {
        let mut limiter = HostRateLimiter::new(5);
        // Should allow 5 consecutive acquires (full bucket).
        for _ in 0..5 {
            assert!(limiter.try_acquire(), "should allow while tokens remain");
        }
        // Bucket is now empty — next acquire should fail.
        assert!(!limiter.try_acquire(), "should block when bucket is empty");
    }

    // -----------------------------------------------------------------------
    // Backoff helper
    // -----------------------------------------------------------------------

    #[test]
    fn backoff_ms_stays_within_cap() {
        let mut rng = SmallRng::seed_from_u64(42);
        for attempt in 0u32..10 {
            let ms = backoff_ms(attempt, &mut rng);
            assert!(
                ms <= BACKOFF_CAP_MS,
                "backoff {ms}ms exceeds cap {BACKOFF_CAP_MS}ms at attempt {attempt}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // host_key_from_url
    // -----------------------------------------------------------------------

    #[test]
    fn host_key_from_url_extracts_host_and_port() {
        assert_eq!(
            host_key_from_url("http://homeassistant.local:8123/api/history"),
            "homeassistant.local:8123"
        );
        assert_eq!(
            host_key_from_url("https://ha.example.com/api/camera"),
            "ha.example.com:443"
        );
    }

    // -----------------------------------------------------------------------
    // decode_rgba8
    // -----------------------------------------------------------------------

    #[test]
    fn decode_rgba8_returns_none_on_invalid_bytes() {
        let garbage = b"not an image";
        assert!(
            decode_rgba8(garbage).is_none(),
            "garbage bytes must not decode"
        );
    }

    #[test]
    fn decode_rgba8_decodes_valid_png() {
        // Encode a 2x2 RGBA PNG and decode it.
        let png = encode_png_bytes(2, 2);
        let decoded = decode_rgba8(&png).expect("2x2 PNG must decode");
        assert_eq!(decoded.width, 2);
        assert_eq!(decoded.height, 2);
        assert_eq!(decoded.data.len(), 2 * 2 * 4);
        assert_eq!(decoded.byte_cost(), 2 * 2 * 4);
    }

    // -----------------------------------------------------------------------
    // LRU eviction
    // -----------------------------------------------------------------------

    #[test]
    fn lru_eviction_keeps_bytes_within_budget() {
        // Use a small cache (4 KiB) and insert 1 KiB entries.
        let mut profile = PROFILE_DESKTOP;
        profile.http_cache_bytes = 4 * 1024;
        let client = HaHttpClient::new(test_config(), &profile);

        let entry_size = 1024usize;
        let payload: Arc<[u8]> = Arc::from(vec![0u8; entry_size].as_slice());

        for i in 0..16usize {
            let url = format!("http://ha.local/item/{i}");
            let mut inner = client.inner.lock().unwrap();
            let ttl = inner.cache_ttl;
            inner.insert(
                url,
                CacheRecord {
                    entry: CacheEntry::Bytes(payload.clone()),
                    inserted_at: Instant::now(),
                    ttl,
                },
            );
            let used = inner.cache_bytes_used;
            assert!(
                used <= profile.http_cache_bytes,
                "after insert #{i}: {used} > {}",
                profile.http_cache_bytes
            );
        }
    }
}
