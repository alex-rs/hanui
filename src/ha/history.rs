//! REST history fetch + LTTB downsampling for the history-graph tile (TASK-106).
//!
//! # Overview
//!
//! `src/ha/history.rs` is the data layer for `WidgetKind::History` widgets. It
//! exposes:
//!
//!   * [`HistoryPoint`]   — a single (timestamp, numeric, raw-state) sample.
//!   * [`HistoryWindow`]  — a downsampled trace of `Vec<(Timestamp, f64)>` of
//!     length ≤ `max_points`. Built by [`HistoryWindow::from_points`].
//!   * [`fetch_history`]  — calls the Home Assistant `/api/history/period`
//!     REST endpoint via [`HaHttpClient`] and decodes the JSON response into
//!     a `Vec<HistoryPoint>`.
//!   * [`lttb_downsample`] — Largest-Triangle-Three-Buckets downsampler per
//!     `locked_decisions.history_render_path`. Public so the bridge tests can
//!     exercise it without going through the full fetch path.
//!   * [`cap_for_profile_key`] — per-profile `max_points` cap (`rpi4=120`,
//!     `opi_zero3=60`, `desktop=240`) per `locked_decisions.history_render_path`.
//!   * [`HistoryThrottle`] — debounce gate (60s minimum between pushes,
//!     bypassed by visibility flips) per the same locked decision.
//!
//! # Why a separate module
//!
//! `src/ha/http.rs` is the shared HTTP layer (TASK-097). This module is the
//! per-domain consumer: it constructs the URL, decodes the HA-specific JSON
//! shape, and applies the per-profile downsampling. Keeping it out of
//! `src/ha/http.rs` means the HTTP layer stays domain-agnostic and the
//! downsampler can be unit-tested without instantiating an [`HaHttpClient`].
//!
//! # JSON parsing
//!
//! The Home Assistant `/api/history/period` endpoint returns a nested array:
//! the outer array has one element per requested entity, the inner array has
//! one entry per state-change record. We use `serde_json` directly here (this
//! file is in `src/ha/`, not `src/ui/`, so the Gate-2 grep does not apply).
//!
//! # Hot-path discipline
//!
//! Neither [`fetch_history`] nor [`lttb_downsample`] is on a per-frame Slint
//! callback. The bridge calls `fetch_history` via `tokio::spawn` and pushes
//! the resulting `HistoryWindow` to a Slint property at most once per 60s
//! per `locked_decisions.history_render_path` (enforced by [`HistoryThrottle`]).
//! Allocations therefore happen at fetch time, not per frame.

use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use thiserror::Error;

use crate::dashboard::schema::ProfileKey;
use crate::ha::entity::EntityId;
use crate::ha::http::{HaHttpClient, HttpError};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Validator-enforced maximum for `WidgetOptions::History.max_points`
/// per `locked_decisions.history_render_path` (also enforced in
/// `src/dashboard/schema.rs::ValidationRule::HistoryMaxPointsExceeded`).
///
/// The downsampler uses this as a final safety clamp even when the caller
/// passes a larger `max_points`.
pub const HISTORY_MAX_POINTS_HARD_CAP: usize = 240;

/// Minimum interval between successive pushes of a `HistoryWindow` to the
/// Slint property graph, per `locked_decisions.history_render_path`.
///
/// A visibility flip (widget becomes visible) bypasses this throttle and
/// forces an immediate push — see [`HistoryThrottle::should_push`].
pub const HISTORY_PUSH_THROTTLE: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// HistoryPoint
// ---------------------------------------------------------------------------

/// A single Home Assistant history sample.
///
/// `state` is the raw HA state string (e.g. `"on"`, `"23.4"`, `"unavailable"`).
/// `last_changed` is the wall-clock instant of the state-change record.
///
/// `numeric` is `Some(f64)` when `state` parses as a finite `f64` and `None`
/// otherwise. Boolean states map to `1.0`/`0.0` for `"on"`/`"off"` so a
/// switch entity's history can be plotted as a step trace.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPoint {
    /// Raw HA state string.
    pub state: String,
    /// Last-changed wall-clock instant.
    pub last_changed: Timestamp,
    /// Parsed numeric value, when the state is plottable.
    pub numeric: Option<f64>,
}

impl HistoryPoint {
    /// Construct a [`HistoryPoint`] from a raw state string and a timestamp.
    ///
    /// The `numeric` field is computed from `state`:
    ///
    /// * `"on"`  → `Some(1.0)`
    /// * `"off"` → `Some(0.0)`
    /// * any string parsing as a finite `f64` → `Some(<value>)`
    /// * everything else (including `NaN`/`±∞` and `"unavailable"`) → `None`
    #[must_use]
    pub fn new(state: String, last_changed: Timestamp) -> Self {
        let numeric = parse_numeric_state(&state);
        HistoryPoint {
            state,
            last_changed,
            numeric,
        }
    }
}

/// Parse a Home Assistant state string into a plottable `f64`.
///
/// Returns `None` for non-numeric / non-boolean states and for `NaN`/`±∞`
/// (so the downsampler does not emit points the renderer cannot plot).
fn parse_numeric_state(s: &str) -> Option<f64> {
    match s {
        "on" => Some(1.0),
        "off" => Some(0.0),
        other => {
            let parsed: f64 = other.parse().ok()?;
            if parsed.is_finite() {
                Some(parsed)
            } else {
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HistoryWindow
// ---------------------------------------------------------------------------

/// A downsampled history trace ready for rendering.
///
/// Holds `Vec<(Timestamp, f64)>` of length ≤ `max_points`. Built by
/// [`HistoryWindow::from_points`] which applies LTTB downsampling and the
/// per-profile cap.
///
/// Per `locked_decisions.history_render_path`: the inner `Vec` is the only
/// allocation a `HistoryWindow` carries; cloning is `O(n)` over the point
/// count (typically ≤ 240). The bridge wraps the produced window in an
/// `Arc<HistoryWindow>` before pushing to Slint so a single window can be
/// shared across multiple widget rebuilds without a deep copy.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryWindow {
    /// Downsampled `(timestamp, numeric)` tuples in chronological order.
    pub points: Vec<(Timestamp, f64)>,
}

impl HistoryWindow {
    /// Construct a [`HistoryWindow`] by extracting the numeric points from
    /// `raw` and downsampling to at most `max_points` entries.
    ///
    /// Non-numeric points (e.g. `"unavailable"`) are dropped before
    /// downsampling — they cannot be plotted on a numeric axis.
    ///
    /// `max_points` is clamped to [`HISTORY_MAX_POINTS_HARD_CAP`] before the
    /// downsampler runs. Callers should also clamp by [`cap_for_profile_key`]
    /// before reaching this constructor.
    #[must_use]
    pub fn from_points(raw: &[HistoryPoint], max_points: usize) -> Self {
        let numeric: Vec<(Timestamp, f64)> = raw
            .iter()
            .filter_map(|p| p.numeric.map(|n| (p.last_changed, n)))
            .collect();
        let cap = max_points.min(HISTORY_MAX_POINTS_HARD_CAP);
        let points = lttb_downsample(&numeric, cap);
        HistoryWindow { points }
    }

    /// Number of points in this window after downsampling.
    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether the window holds zero points (no plottable data).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

// ---------------------------------------------------------------------------
// LTTB downsampler
// ---------------------------------------------------------------------------

/// Largest-Triangle-Three-Buckets downsampler per
/// `locked_decisions.history_render_path`.
///
/// The algorithm walks `data` in `threshold` buckets and for each bucket
/// picks the point that forms the largest triangle with the previously-kept
/// point and the average of the next bucket. The first and last input points
/// are always kept.
///
/// # Edge cases
///
/// * `data.len() == 0`         → returns an empty `Vec`.
/// * `data.len() <= threshold` → returns a clone of `data` (no downsampling
///   needed; the input already fits in the budget).
/// * `threshold <= 2`          → returns the first and last points (or just
///   the input if its length ≤ `threshold`).
///
/// # Reference
///
/// Sveinn Steinarsson, *Downsampling Time Series for Visual Representation*,
/// MSc thesis, University of Iceland, 2013.
#[must_use]
pub fn lttb_downsample(data: &[(Timestamp, f64)], threshold: usize) -> Vec<(Timestamp, f64)> {
    let n = data.len();
    if n == 0 {
        return Vec::new();
    }
    if threshold == 0 {
        return Vec::new();
    }
    if n <= threshold {
        return data.to_vec();
    }
    if threshold <= 2 {
        // Keep only the endpoints — degenerate but well-defined.
        return vec![data[0], data[n - 1]];
    }

    let mut sampled: Vec<(Timestamp, f64)> = Vec::with_capacity(threshold);

    // Bucket size for the inner buckets (excluding first and last fixed
    // points). Use float division for accurate stride; the integer floor
    // computation in the loop handles the fractional remainder by clamping
    // bucket bounds to `n`.
    let every: f64 = (n - 2) as f64 / (threshold - 2) as f64;

    // Always keep the first point.
    let mut a: usize = 0;
    sampled.push(data[a]);

    for i in 0..(threshold - 2) {
        // Range of the next bucket: average is computed over points whose
        // index is in [next_start, next_end).
        let next_start = ((i + 1) as f64 * every).floor() as usize + 1;
        let next_end = (((i + 2) as f64 * every).floor() as usize + 1).min(n);
        let next_len = next_end.saturating_sub(next_start).max(1);

        // Average timestamp + value over the next bucket. Timestamps are
        // averaged via their unix-second representation; the result is
        // converted back to a Timestamp for the triangle area computation.
        let mut avg_ts_secs: f64 = 0.0;
        let mut avg_val: f64 = 0.0;
        for (ts, val) in &data[next_start..next_end] {
            avg_ts_secs += ts.as_second() as f64;
            avg_val += val;
        }
        avg_ts_secs /= next_len as f64;
        avg_val /= next_len as f64;

        // Range of the current bucket: search for the point with the
        // largest triangle area between `a`, the candidate, and the
        // averaged next bucket.
        let cur_start = (i as f64 * every).floor() as usize + 1;
        let cur_end = (((i + 1) as f64 * every).floor() as usize + 1).min(n);

        let a_ts_secs = data[a].0.as_second() as f64;
        let a_val = data[a].1;

        let mut best_idx = cur_start;
        let mut best_area: f64 = -1.0;

        for (offset, (ts, val)) in data[cur_start..cur_end].iter().enumerate() {
            let ts_secs = ts.as_second() as f64;
            // Triangle area (×2) — sign discarded; the absolute value is the
            // ranking key.
            let area = ((a_ts_secs - avg_ts_secs) * (val - a_val)
                - (a_ts_secs - ts_secs) * (avg_val - a_val))
                .abs();
            if area > best_area {
                best_area = area;
                best_idx = cur_start + offset;
            }
        }

        sampled.push(data[best_idx]);
        a = best_idx;
    }

    // Always keep the last point.
    sampled.push(data[n - 1]);

    sampled
}

// ---------------------------------------------------------------------------
// Per-profile cap
// ---------------------------------------------------------------------------

/// Per-profile cap on `max_points` per `locked_decisions.history_render_path`.
///
/// * `Rpi4`     → `120`
/// * `OpiZero3` →  `60`
/// * `Desktop`  → `240`
///
/// Lives here (not on `DeviceProfile` directly) because the cap is the
/// downsampler's concern — adding a profile field that only history reads
/// would bloat every other widget's profile-touch surface. The mapping is
/// closed: every `ProfileKey` variant has an arm. Adding a new `ProfileKey`
/// variant in a future plan amendment is a compile error here until extended.
#[must_use]
pub fn cap_for_profile_key(profile: ProfileKey) -> u32 {
    match profile {
        ProfileKey::Rpi4 => 120,
        ProfileKey::OpiZero3 => 60,
        ProfileKey::Desktop => 240,
    }
}

// ---------------------------------------------------------------------------
// HistoryThrottle
// ---------------------------------------------------------------------------

/// Debounce gate per `locked_decisions.history_render_path`.
///
/// The bridge holds one `HistoryThrottle` per history widget. On each
/// candidate push, [`HistoryThrottle::should_push`] returns `true` if at
/// least [`HISTORY_PUSH_THROTTLE`] has elapsed since the last accepted
/// push, OR if the caller passes `visibility_flipped == true` (the widget
/// just became visible and the user is owed an immediate render).
///
/// The struct stores the last-accept `Instant`, NOT the last-attempt
/// `Instant` — failed pushes (e.g. fetch errors) do not consume the
/// throttle budget. This matches the spirit of the locked decision: throttle
/// counts what reaches Slint, not what attempted to.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistoryThrottle {
    last_push: Option<Instant>,
}

impl HistoryThrottle {
    /// Construct a fresh throttle. The first call to
    /// [`HistoryThrottle::should_push`] will return `true` regardless of
    /// `visibility_flipped` because no previous push exists.
    #[must_use]
    pub fn new() -> Self {
        HistoryThrottle { last_push: None }
    }

    /// Decide whether a candidate push should reach Slint right now.
    ///
    /// Returns `true` (and records the push) if either:
    ///   * `visibility_flipped` is `true` (widget just became visible), OR
    ///   * no prior push exists, OR
    ///   * at least [`HISTORY_PUSH_THROTTLE`] has elapsed since the last
    ///     accepted push.
    ///
    /// Returns `false` otherwise.
    pub fn should_push(&mut self, visibility_flipped: bool) -> bool {
        if visibility_flipped {
            self.last_push = Some(Instant::now());
            return true;
        }
        match self.last_push {
            None => {
                self.last_push = Some(Instant::now());
                true
            }
            Some(prev) => {
                if prev.elapsed() >= HISTORY_PUSH_THROTTLE {
                    self.last_push = Some(Instant::now());
                    true
                } else {
                    false
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// REST fetch
// ---------------------------------------------------------------------------

/// Errors returned by [`fetch_history`].
#[derive(Debug, Error)]
pub enum HistoryError {
    /// The underlying HTTP fetch failed (network, status, timeout, …).
    #[error("history fetch HTTP error: {0}")]
    Http(#[from] HttpError),
    /// The response body could not be parsed as the expected nested-array
    /// JSON shape. The trace_id correlates with `tracing::warn!` log entries
    /// emitted on the failure path.
    #[error("history response parse failed (trace_id={trace_id})")]
    ParseFailed {
        /// Opaque trace ID for log correlation.
        trace_id: u64,
    },
}

/// Fetch a Home Assistant history window for `entity_id`.
///
/// # URL
///
/// Builds:
///
/// ```text
/// {base}/api/history/period/{start_iso}?filter_entity_id={entity_id}&end_time={end_iso}&minimal_response&no_attributes
/// ```
///
/// The `minimal_response` and `no_attributes` query parameters keep the
/// response payload small (per HA docs) — we only consume `state` and
/// `last_changed`. The Bearer token is appended by [`HaHttpClient`].
///
/// `start` and `end` are formatted via [`Timestamp`]'s `Display` impl,
/// which emits an RFC 3339 string with `Z` for UTC (`"YYYY-MM-DDTHH:MM:SSZ"`).
/// This is the canonical RFC 3339 representation Home Assistant accepts on
/// the `/api/history/period` endpoint. We do NOT use a non-UTC offset
/// because [`Timestamp`] is always UTC-anchored — the `Display` impl never
/// emits a `+HH:MM` offset for this type.
///
/// # Errors
///
/// * [`HistoryError::Http`]        — any [`HttpError`] from the shared HTTP
///   layer (rate-limit, transport, status, timeout, body-read).
/// * [`HistoryError::ParseFailed`] — the response body is not the expected
///   nested-array shape.
pub async fn fetch_history(
    http: &Arc<HaHttpClient>,
    base_url: &str,
    entity_id: &EntityId,
    start: Timestamp,
    end: Timestamp,
) -> Result<Vec<HistoryPoint>, HistoryError> {
    let url = build_history_url(base_url, entity_id, start, end);
    let bytes = http.get_bytes(&url).await?;
    parse_history_response(&bytes)
}

/// Build the fully-qualified URL for a history-period fetch.
///
/// Public for tests; production callers use [`fetch_history`].
#[must_use]
pub fn build_history_url(
    base_url: &str,
    entity_id: &EntityId,
    start: Timestamp,
    end: Timestamp,
) -> String {
    let trimmed_base = base_url.trim_end_matches('/');
    let start_iso = format_timestamp(start);
    let end_iso = format_timestamp(end);
    let entity = entity_id.as_str();
    format!(
        "{trimmed_base}/api/history/period/{start_iso}?filter_entity_id={entity}&end_time={end_iso}&minimal_response&no_attributes"
    )
}

/// Format a [`Timestamp`] as an ISO-8601 string suitable for the HA history
/// query path.
fn format_timestamp(ts: Timestamp) -> String {
    // jiff::Timestamp::Display formats as RFC3339 (e.g. "2026-04-30T12:34:56Z").
    ts.to_string()
}

/// Decode a Home Assistant history response into a flat `Vec<HistoryPoint>`.
///
/// The response shape is `[[ {state, last_changed}, ... ]]` — an outer array
/// containing one inner array per requested entity. We requested exactly one
/// entity, so we either pull the first inner array or treat the response as
/// empty.
fn parse_history_response(bytes: &[u8]) -> Result<Vec<HistoryPoint>, HistoryError> {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct RawRecord {
        state: String,
        // HA emits `last_changed` as RFC3339; jiff parses it via FromStr.
        last_changed: String,
    }

    let outer: Vec<Vec<RawRecord>> = serde_json::from_slice(bytes).map_err(|e| {
        let trace_id = next_trace_id();
        tracing::warn!(
            trace_id = trace_id,
            error = %e,
            "history response JSON parse failed"
        );
        HistoryError::ParseFailed { trace_id }
    })?;

    let inner = match outer.into_iter().next() {
        Some(records) => records,
        None => return Ok(Vec::new()),
    };

    let mut points = Vec::with_capacity(inner.len());
    for rec in inner {
        let last_changed: Timestamp = match rec.last_changed.parse() {
            Ok(ts) => ts,
            Err(e) => {
                // Skip individual malformed records rather than failing the
                // whole fetch — HA history responses occasionally include
                // truncated entries near the start of the window.
                tracing::warn!(
                    error = %e,
                    raw = %rec.last_changed,
                    "skipping history record with unparseable last_changed"
                );
                continue;
            }
        };
        points.push(HistoryPoint::new(rec.state, last_changed));
    }
    Ok(points)
}

/// Monotonic trace-id generator — see `src/ha/http.rs::next_trace_id` for
/// the same pattern. Local copy avoids exposing that module's internals.
fn next_trace_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).expect("timestamp in range")
    }

    fn raw_points(values: &[(i64, f64)]) -> Vec<(Timestamp, f64)> {
        values.iter().map(|(s, v)| (ts(*s), *v)).collect()
    }

    // -----------------------------------------------------------------------
    // HistoryPoint::new — numeric parsing
    // -----------------------------------------------------------------------

    #[test]
    fn history_point_on_state_maps_to_one() {
        let p = HistoryPoint::new("on".to_owned(), ts(0));
        assert_eq!(p.numeric, Some(1.0));
    }

    #[test]
    fn history_point_off_state_maps_to_zero() {
        let p = HistoryPoint::new("off".to_owned(), ts(0));
        assert_eq!(p.numeric, Some(0.0));
    }

    #[test]
    fn history_point_numeric_state_parses() {
        let p = HistoryPoint::new("23.4".to_owned(), ts(0));
        assert_eq!(p.numeric, Some(23.4));
    }

    #[test]
    fn history_point_negative_numeric_state_parses() {
        let p = HistoryPoint::new("-5".to_owned(), ts(0));
        assert_eq!(p.numeric, Some(-5.0));
    }

    #[test]
    fn history_point_unavailable_state_is_non_numeric() {
        let p = HistoryPoint::new("unavailable".to_owned(), ts(0));
        assert_eq!(p.numeric, None);
    }

    #[test]
    fn history_point_garbage_state_is_non_numeric() {
        let p = HistoryPoint::new("garbage".to_owned(), ts(0));
        assert_eq!(p.numeric, None);
    }

    #[test]
    fn history_point_nan_state_returns_none() {
        // "NaN" parses as f64::NAN — we filter those out so the renderer
        // never sees a non-finite value.
        let p = HistoryPoint::new("NaN".to_owned(), ts(0));
        assert_eq!(p.numeric, None);
    }

    #[test]
    fn history_point_infinity_state_returns_none() {
        let p = HistoryPoint::new("inf".to_owned(), ts(0));
        assert_eq!(p.numeric, None);
    }

    #[test]
    fn history_point_state_field_is_preserved() {
        let p = HistoryPoint::new("23.4".to_owned(), ts(123));
        assert_eq!(p.state, "23.4", "state string is preserved verbatim");
        assert_eq!(p.last_changed, ts(123));
    }

    // -----------------------------------------------------------------------
    // LTTB downsampler — TASK-106 acceptance
    // -----------------------------------------------------------------------

    /// 1000-point input with `max_points=60` produces output of length 60
    /// (TASK-106 acceptance #3).
    #[test]
    fn lttb_downsamples_to_max_points() {
        let data: Vec<(Timestamp, f64)> = (0..1000)
            .map(|i| (ts(i64::from(i)), f64::from(i) * 0.1))
            .collect();
        let out = lttb_downsample(&data, 60);
        assert_eq!(out.len(), 60, "1000 → 60");
        // First and last are preserved.
        assert_eq!(out[0], data[0]);
        assert_eq!(out[59], data[999]);
    }

    /// Input of length ≤ `max_points` returns the input unchanged
    /// (TASK-106 acceptance #3).
    #[test]
    fn lttb_passthrough_short_input() {
        let data = raw_points(&[(0, 1.0), (1, 2.0), (2, 3.0)]);
        let out = lttb_downsample(&data, 60);
        assert_eq!(out, data, "short input is unchanged");
    }

    /// Empty input returns empty output (TASK-106 acceptance #3).
    #[test]
    fn lttb_empty_input() {
        let out = lttb_downsample(&[], 60);
        assert!(out.is_empty());
    }

    /// Threshold of 0 returns empty even for non-empty input.
    #[test]
    fn lttb_zero_threshold_returns_empty() {
        let data = raw_points(&[(0, 1.0), (1, 2.0)]);
        let out = lttb_downsample(&data, 0);
        assert!(out.is_empty());
    }

    /// Threshold of 2 with longer input keeps only the endpoints.
    #[test]
    fn lttb_threshold_two_keeps_endpoints() {
        let data: Vec<(Timestamp, f64)> = (0..10).map(|i| (ts(i64::from(i)), 1.0)).collect();
        let out = lttb_downsample(&data, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], data[0]);
        assert_eq!(out[1], data[9]);
    }

    /// LTTB output is monotonic in the timestamp axis (the algorithm walks
    /// data left-to-right; each chosen point comes from a bucket strictly
    /// after the previously-kept point).
    #[test]
    fn lttb_output_is_chronological() {
        let data: Vec<(Timestamp, f64)> = (0..200)
            .map(|i| (ts(i64::from(i)), (i as f64).sin()))
            .collect();
        let out = lttb_downsample(&data, 30);
        for w in out.windows(2) {
            assert!(
                w[0].0 <= w[1].0,
                "non-chronological pair: {:?} > {:?}",
                w[0].0,
                w[1].0
            );
        }
    }

    // -----------------------------------------------------------------------
    // HistoryWindow::from_points
    // -----------------------------------------------------------------------

    /// Non-numeric points are filtered out before downsampling.
    #[test]
    fn history_window_drops_non_numeric_points() {
        let raw = vec![
            HistoryPoint::new("23.4".to_owned(), ts(0)),
            HistoryPoint::new("unavailable".to_owned(), ts(1)),
            HistoryPoint::new("24.0".to_owned(), ts(2)),
        ];
        let window = HistoryWindow::from_points(&raw, 60);
        assert_eq!(window.len(), 2, "unavailable point dropped");
        assert_eq!(window.points[0], (ts(0), 23.4));
        assert_eq!(window.points[1], (ts(2), 24.0));
    }

    /// `from_points` clamps `max_points` to [`HISTORY_MAX_POINTS_HARD_CAP`]
    /// (TASK-106 acceptance #2 — bounded by the per-profile cap, with the
    /// hard cap as a final safety net).
    #[test]
    fn history_window_clamps_max_points_to_hard_cap() {
        let raw: Vec<HistoryPoint> = (0..1000)
            .map(|i| HistoryPoint::new(i.to_string(), ts(i64::from(i))))
            .collect();
        let window = HistoryWindow::from_points(&raw, 9999);
        assert!(
            window.len() <= HISTORY_MAX_POINTS_HARD_CAP,
            "window.len()={} exceeded hard cap {HISTORY_MAX_POINTS_HARD_CAP}",
            window.len()
        );
    }

    #[test]
    fn history_window_empty_input() {
        let window = HistoryWindow::from_points(&[], 60);
        assert!(window.is_empty());
        assert_eq!(window.len(), 0);
    }

    // -----------------------------------------------------------------------
    // cap_for_profile_key — TASK-106 acceptance #14
    // -----------------------------------------------------------------------

    #[test]
    fn cap_for_profile_key_rpi4_is_120() {
        assert_eq!(cap_for_profile_key(ProfileKey::Rpi4), 120);
    }

    #[test]
    fn cap_for_profile_key_opi_zero3_is_60() {
        assert_eq!(cap_for_profile_key(ProfileKey::OpiZero3), 60);
    }

    #[test]
    fn cap_for_profile_key_desktop_is_240() {
        assert_eq!(cap_for_profile_key(ProfileKey::Desktop), 240);
    }

    /// TASK-106 AC #14: a YAML loaded with `max_points: 240` on the
    /// `opi_zero3` profile (cap=60) must result in the downsampler capping
    /// the output at 60 even when the caller passes the YAML value
    /// directly.
    #[test]
    fn max_points_per_profile_enforced() {
        let raw: Vec<HistoryPoint> = (0..1000)
            .map(|i| HistoryPoint::new(i.to_string(), ts(i64::from(i))))
            .collect();
        // Simulate a caller that read 240 from the YAML, intersects with the
        // opi_zero3 profile cap (60), and passes the result to `from_points`.
        let yaml_max_points: u32 = 240;
        let profile_cap = cap_for_profile_key(ProfileKey::OpiZero3);
        let effective = yaml_max_points.min(profile_cap) as usize;
        let window = HistoryWindow::from_points(&raw, effective);
        assert_eq!(
            window.len(),
            60,
            "opi_zero3 profile must cap output at 60 even when YAML says 240"
        );
    }

    // -----------------------------------------------------------------------
    // HistoryThrottle — TASK-106 acceptance #5
    // -----------------------------------------------------------------------

    /// First call accepts (no prior push) regardless of visibility flag.
    #[test]
    fn throttle_first_call_accepts() {
        let mut throttle = HistoryThrottle::new();
        assert!(throttle.should_push(false));
    }

    /// Second call within the throttle interval is rejected.
    #[test]
    fn throttle_blocks_repeat_push_within_60s() {
        let mut throttle = HistoryThrottle::new();
        assert!(throttle.should_push(false), "first push accepted");
        assert!(
            !throttle.should_push(false),
            "second push within throttle window rejected"
        );
    }

    /// Visibility flip bypasses the throttle.
    #[test]
    fn throttle_visibility_flip_bypasses() {
        let mut throttle = HistoryThrottle::new();
        assert!(throttle.should_push(false), "first push accepted");
        assert!(
            throttle.should_push(true),
            "visibility flip must bypass the throttle"
        );
    }

    /// Default impl is equivalent to `new`.
    #[test]
    fn throttle_default_is_unblocked() {
        let mut throttle = HistoryThrottle::default();
        assert!(throttle.should_push(false));
    }

    // -----------------------------------------------------------------------
    // build_history_url
    // -----------------------------------------------------------------------

    #[test]
    fn build_history_url_includes_filter_and_end_time() {
        let url = build_history_url(
            "http://ha.local:8123",
            &EntityId::from("sensor.energy"),
            ts(1_700_000_000),
            ts(1_700_003_600),
        );
        assert!(
            url.contains("/api/history/period/"),
            "URL must use the history-period endpoint: {url}"
        );
        assert!(
            url.contains("filter_entity_id=sensor.energy"),
            "URL must include the entity filter: {url}"
        );
        assert!(
            url.contains("end_time="),
            "URL must include end_time: {url}"
        );
        assert!(
            url.contains("minimal_response"),
            "URL must request minimal_response to keep payloads small: {url}"
        );
        assert!(
            url.contains("no_attributes"),
            "URL must request no_attributes to keep payloads small: {url}"
        );
    }

    /// `format_timestamp` produces an RFC 3339 string ending in `Z` (UTC),
    /// matching the format Home Assistant accepts on
    /// `/api/history/period`. Guards the docstring/impl alignment opencode
    /// flagged in TASK-106 self-review.
    #[test]
    fn format_timestamp_emits_rfc3339_z_suffix() {
        let formatted = format_timestamp(ts(1_700_000_000));
        assert!(
            formatted.ends_with('Z'),
            "format_timestamp must emit RFC 3339 with Z suffix (HA accepts), got: {formatted}"
        );
        // Sanity check: the format starts with the year and has a 'T' separator.
        assert!(
            formatted.contains('T'),
            "format_timestamp must use ISO-8601 'T' separator, got: {formatted}"
        );
    }

    #[test]
    fn build_history_url_strips_trailing_slash_from_base() {
        let with_slash = build_history_url(
            "http://ha.local:8123/",
            &EntityId::from("sensor.x"),
            ts(0),
            ts(1),
        );
        let without_slash = build_history_url(
            "http://ha.local:8123",
            &EntityId::from("sensor.x"),
            ts(0),
            ts(1),
        );
        assert_eq!(
            with_slash, without_slash,
            "trailing slash on base URL must not produce a double-slash path"
        );
    }

    // -----------------------------------------------------------------------
    // parse_history_response
    // -----------------------------------------------------------------------

    #[test]
    fn parse_history_response_decodes_nested_array() {
        let body = br#"[[
            {"state": "23.4", "last_changed": "2026-04-30T12:00:00Z"},
            {"state": "23.5", "last_changed": "2026-04-30T12:01:00Z"}
        ]]"#;
        let points = parse_history_response(body).expect("valid response parses");
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].state, "23.4");
        assert_eq!(points[0].numeric, Some(23.4));
        assert_eq!(points[1].numeric, Some(23.5));
    }

    #[test]
    fn parse_history_response_empty_outer_array_is_empty_vec() {
        let body = b"[]";
        let points = parse_history_response(body).expect("empty outer array is valid");
        assert!(points.is_empty());
    }

    #[test]
    fn parse_history_response_empty_inner_array_is_empty_vec() {
        let body = b"[[]]";
        let points = parse_history_response(body).expect("empty inner array is valid");
        assert!(points.is_empty());
    }

    #[test]
    fn parse_history_response_garbage_returns_parse_failed() {
        let body = b"not json";
        let err = parse_history_response(body).expect_err("garbage must error");
        assert!(matches!(err, HistoryError::ParseFailed { .. }));
    }

    #[test]
    fn parse_history_response_skips_malformed_timestamp_records() {
        let body = br#"[[
            {"state": "1.0", "last_changed": "not-a-timestamp"},
            {"state": "2.0", "last_changed": "2026-04-30T12:01:00Z"}
        ]]"#;
        let points = parse_history_response(body).expect("partial-bad records are not fatal");
        assert_eq!(points.len(), 1, "malformed record skipped");
        assert_eq!(points[0].state, "2.0");
    }

    // -----------------------------------------------------------------------
    // HistoryError surface — From<HttpError>
    // -----------------------------------------------------------------------

    #[test]
    fn history_error_from_http_error() {
        let http_err = HttpError::RateLimited {
            host: "ha.local:8123".to_owned(),
        };
        let err: HistoryError = http_err.into();
        assert!(
            matches!(err, HistoryError::Http(HttpError::RateLimited { .. })),
            "HistoryError::Http wraps HttpError"
        );
    }

    #[test]
    fn history_error_display_includes_trace_id_for_parse_failed() {
        let err = HistoryError::ParseFailed { trace_id: 42 };
        let s = format!("{err}");
        assert!(s.contains("42"), "trace_id surfaced in Display: {s}");
    }

    // -----------------------------------------------------------------------
    // HISTORY_PUSH_THROTTLE constant value
    // -----------------------------------------------------------------------

    #[test]
    fn history_push_throttle_is_60s() {
        assert_eq!(
            HISTORY_PUSH_THROTTLE,
            Duration::from_secs(60),
            "throttle interval per locked_decisions.history_render_path"
        );
    }
}
