//! Gesture- and action-timing knobs read by the Phase 3 dispatcher and by the
//! Slint gesture layer.
//!
//! Two struct-shaped configs are exported here:
//!
//! 1. [`GestureConfig`] — the per-press timing values the Slint gesture layer
//!    (TASK-060) reads when wiring tap / hold / double-tap timers in
//!    `card_base.slint`. Phase 3 hardwires the defaults; Phase 4
//!    `DeviceProfile.timing_overrides` may override them at startup.
//!
//! 2. [`ActionTiming`] — the dispatcher- and reconciliation-layer timing knobs
//!    (TASK-062 / TASK-064): how long an optimistic UI entry may stay pending
//!    before reverting, how long an offline-queued idempotent action may sit in
//!    the queue before age-out, and which [`ActionOverlapStrategy`] resolves a
//!    second gesture on the same widget while a first dispatch is still
//!    pending.
//!
//! # Locked decisions
//!
//! Defaults below are pinned by
//! `docs/plans/2026-04-28-phase-3-actions.md` `locked_decisions.gesture_config`
//! and `locked_decisions.action_timing`. The `arm_double_tap_timer` derived
//! field exists because Slint must not infer "double-tap is on" from a
//! `double_tap_max_gap_ms == 0` value (zero is also a valid disabled-marker in
//! some downstream configs, and the resulting branch divergence has bitten
//! Slint apps before). The Rust side computes the boolean once and exposes it
//! verbatim to Slint.
//!
//! # Phase 4 forward-compat
//!
//! All three types derive `Serialize` + `Deserialize` so Phase 4
//! `DeviceProfile.timing_overrides` can populate them from YAML without
//! reshaping the type. [`ActionOverlapStrategy`] is `#[non_exhaustive]` so
//! adding a new variant in a future phase is a non-breaking change for serde
//! consumers.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// GestureConfig
// ---------------------------------------------------------------------------

/// Per-press timing thresholds the Slint gesture layer reads.
///
/// All `*_ms` values are wall-clock milliseconds. `Copy` is intentional — the
/// struct is small (4 × `u64`/`bool`) and is read on every gesture event;
/// passing by value avoids any aliasing concern when the Rust side and the
/// Slint side both hold a snapshot.
///
/// # Field semantics
///
/// * `tap_max_ms` — a press whose press-to-release interval is `<= tap_max_ms`
///   is classified as a tap. Above the threshold, it becomes a hold candidate.
/// * `hold_min_ms` — a press still active at `>= hold_min_ms` fires the hold
///   action. Tile UIs typically arm the hold timer on press-down and disarm
///   on early release.
/// * `double_tap_max_gap_ms` — the maximum gap between the release of tap N
///   and the press of tap N+1 for the pair to count as a double-tap.
/// * `double_tap_enabled` — explicit enable/disable flag for the double-tap
///   path. Read indirectly by Slint via [`Self::arm_double_tap_timer`] —
///   never inferred from `double_tap_max_gap_ms == 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GestureConfig {
    /// Press shorter than this counts as a tap. Default: 200 ms.
    pub tap_max_ms: u64,
    /// Press at least this long fires the hold action. Default: 500 ms.
    pub hold_min_ms: u64,
    /// Maximum gap between two taps to count as a double-tap. Default: 300 ms.
    pub double_tap_max_gap_ms: u64,
    /// Whether the double-tap path is wired at all. Default: `true`.
    pub double_tap_enabled: bool,
}

impl GestureConfig {
    /// Whether the Slint side should arm the post-tap double-tap timer.
    ///
    /// Equals [`Self::double_tap_enabled`] verbatim. Exposed as a derived
    /// boolean so the Slint global has one explicit `arm_double_tap_timer`
    /// property and never infers intent from a zero `double_tap_max_gap_ms`
    /// value (per `locked_decisions.gesture_config`).
    #[must_use]
    pub const fn arm_double_tap_timer(&self) -> bool {
        self.double_tap_enabled
    }
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            tap_max_ms: 200,
            hold_min_ms: 500,
            double_tap_max_gap_ms: 300,
            double_tap_enabled: true,
        }
    }
}

// ---------------------------------------------------------------------------
// ActionOverlapStrategy
// ---------------------------------------------------------------------------

/// How the dispatcher resolves a second gesture on the same widget while a
/// first action is still in-flight.
///
/// Phase 3 wires only [`Self::LastWriteWins`] at runtime; the variant set is
/// declared in full here so Phase 4 `DeviceProfile.timing_overrides` can flip
/// the strategy without reshaping the type.
///
/// `#[non_exhaustive]` is set so future variants (e.g. `Coalesce`) can be
/// added without breaking external serde consumers — Phase 4 YAML loaders
/// must always carry an explicit fallback branch.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ActionOverlapStrategy {
    /// The pending optimistic entry is cancelled (no revert — the new entry's
    /// `prior_state` is the old entry's `prior_state`, preserving the chain)
    /// and the new action is dispatched. Phase 3 default.
    #[default]
    LastWriteWins,
    /// The second gesture is dropped without a toast or a state change.
    /// Available shape today; runtime selection is Phase 4.
    DiscardConcurrent,
}

// ---------------------------------------------------------------------------
// ActionTiming
// ---------------------------------------------------------------------------

/// Dispatcher- and reconciliation-layer timing knobs.
///
/// All `*_ms` values are wall-clock milliseconds. The struct is `Copy` for the
/// same reason as [`GestureConfig`] — it is small and frequently read.
///
/// # Field semantics
///
/// * `gesture` — the per-press [`GestureConfig`] block, embedded so a single
///   `ActionTiming` value carries everything the dispatcher and the Slint
///   gesture layer need at startup.
/// * `optimistic_timeout_ms` — how long an optimistic UI entry may stay
///   pending before TASK-064's revert path fires. Default: 3000 ms.
/// * `queue_max_age_ms` — how long an idempotent action may sit in TASK-065's
///   offline queue before being aged out. Default: 60000 ms.
/// * `action_overlap_strategy` — see [`ActionOverlapStrategy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionTiming {
    /// Per-press gesture thresholds.
    pub gesture: GestureConfig,
    /// Optimistic-entry revert deadline. Default: 3000 ms.
    pub optimistic_timeout_ms: u64,
    /// Offline-queue age-out. Default: 60000 ms.
    pub queue_max_age_ms: u64,
    /// Overlap policy. Default: [`ActionOverlapStrategy::LastWriteWins`].
    pub action_overlap_strategy: ActionOverlapStrategy,
}

impl Default for ActionTiming {
    fn default() -> Self {
        Self {
            gesture: GestureConfig::default(),
            optimistic_timeout_ms: 3000,
            queue_max_age_ms: 60000,
            action_overlap_strategy: ActionOverlapStrategy::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// ActionTimingOverride
// ---------------------------------------------------------------------------

/// Per-profile override knobs that a [`DeviceProfile`] may apply on top of the
/// [`ActionTiming`] defaults.
///
/// All fields are `Option<_>`: `None` means "keep the `ActionTiming` default
/// for this field". This lets a profile tune one or two knobs (e.g. only
/// `tap_max_ms`) without having to specify the entire set.
///
/// # Phase 4 role
///
/// Phase 4 `DeviceProfile.timing_overrides` carries an
/// `Option<ActionTimingOverride>`.  When `None`, the dispatcher uses
/// [`ActionTiming::default()`] unchanged.  When `Some`, the dispatcher
/// applies each non-`None` field on top of the defaults at startup —
/// the merge is the caller's responsibility, not done here.
///
/// # Derive rationale
///
/// `Default` is derived so an all-`None` value is expressible as
/// `ActionTimingOverride::default()`, which keeps the Phase 4 const presets
/// terse.  `Copy` + `Eq` are derived for the same reasons as [`ActionTiming`]:
/// the struct is small, lives on the stack, and is compared in tests.
///
/// [`DeviceProfile`]: crate::dashboard::profiles::DeviceProfile
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ActionTimingOverride {
    /// Override for the tap classification threshold (ms). `None` = use default (200 ms).
    pub tap_max_ms: Option<u64>,
    /// Override for the hold-action trigger threshold (ms). `None` = use default (500 ms).
    pub hold_min_ms: Option<u64>,
    /// Override for the double-tap gap window (ms). `None` = use default (300 ms).
    pub double_tap_max_gap_ms: Option<u64>,
    /// Override for the optimistic-entry revert deadline (ms). `None` = use default (3 000 ms).
    pub optimistic_timeout_ms: Option<u64>,
    /// Override for the offline-queue age-out (ms). `None` = use default (60 000 ms).
    pub queue_max_age_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // GestureConfig defaults — pinned by locked_decisions.gesture_config
    // -----------------------------------------------------------------------

    #[test]
    fn gesture_config_defaults_match_locked_decisions() {
        let g = GestureConfig::default();
        assert_eq!(g.tap_max_ms, 200);
        assert_eq!(g.hold_min_ms, 500);
        assert_eq!(g.double_tap_max_gap_ms, 300);
        assert!(g.double_tap_enabled);
    }

    #[test]
    fn arm_double_tap_timer_equals_double_tap_enabled() {
        // Per locked_decisions.gesture_config: the derived boolean is the
        // *only* signal Slint reads — Slint must never infer "double-tap is
        // on" from a zero double_tap_max_gap_ms value. This test pins the
        // invariant for both polarities.
        let on = GestureConfig {
            double_tap_enabled: true,
            ..GestureConfig::default()
        };
        assert!(on.arm_double_tap_timer());
        assert_eq!(on.arm_double_tap_timer(), on.double_tap_enabled);

        let off = GestureConfig {
            double_tap_enabled: false,
            ..GestureConfig::default()
        };
        assert!(!off.arm_double_tap_timer());
        assert_eq!(off.arm_double_tap_timer(), off.double_tap_enabled);
    }

    #[test]
    fn arm_double_tap_timer_ignores_zero_gap() {
        // Defensive: even when double_tap_max_gap_ms is 0 (the value Slint
        // would naively read as "disabled"), arm_double_tap_timer must
        // continue to track double_tap_enabled. This is the exact scenario
        // locked_decisions.gesture_config calls out.
        let zero_gap_but_enabled = GestureConfig {
            double_tap_max_gap_ms: 0,
            double_tap_enabled: true,
            ..GestureConfig::default()
        };
        assert!(zero_gap_but_enabled.arm_double_tap_timer());

        let zero_gap_and_disabled = GestureConfig {
            double_tap_max_gap_ms: 0,
            double_tap_enabled: false,
            ..GestureConfig::default()
        };
        assert!(!zero_gap_and_disabled.arm_double_tap_timer());
    }

    // -----------------------------------------------------------------------
    // ActionTiming defaults — pinned by locked_decisions.action_timing
    // -----------------------------------------------------------------------

    #[test]
    fn action_timing_defaults_match_locked_decisions() {
        let t = ActionTiming::default();
        assert_eq!(t.optimistic_timeout_ms, 3000);
        assert_eq!(t.queue_max_age_ms, 60000);
        assert_eq!(
            t.action_overlap_strategy,
            ActionOverlapStrategy::LastWriteWins
        );
        // Embedded GestureConfig defaults must match the standalone defaults
        // — there is exactly one source of truth for those numbers.
        assert_eq!(t.gesture, GestureConfig::default());
    }

    #[test]
    fn action_overlap_strategy_default_is_last_write_wins() {
        // Phase 3 hardwires LastWriteWins. DiscardConcurrent is reachable
        // only via Phase 4 DeviceProfile.timing_overrides.
        assert_eq!(
            ActionOverlapStrategy::default(),
            ActionOverlapStrategy::LastWriteWins
        );
    }

    // -----------------------------------------------------------------------
    // Serde round-trip — Phase 4 YAML loader compat
    // -----------------------------------------------------------------------

    #[test]
    fn gesture_config_round_trips_through_json() {
        let original = GestureConfig::default();
        let json = serde_json::to_string(&original).expect("serialize must succeed");
        let decoded: GestureConfig = serde_json::from_str(&json).expect("deserialize must succeed");
        assert_eq!(decoded, original);
    }

    #[test]
    fn action_timing_round_trips_through_json() {
        let original = ActionTiming::default();
        let json = serde_json::to_string(&original).expect("serialize must succeed");
        let decoded: ActionTiming = serde_json::from_str(&json).expect("deserialize must succeed");
        assert_eq!(decoded, original);
    }

    #[test]
    fn action_overlap_strategy_round_trips_both_variants() {
        for variant in [
            ActionOverlapStrategy::LastWriteWins,
            ActionOverlapStrategy::DiscardConcurrent,
        ] {
            let json = serde_json::to_string(&variant).expect("serialize must succeed");
            let decoded: ActionOverlapStrategy =
                serde_json::from_str(&json).expect("deserialize must succeed");
            assert_eq!(decoded, variant);
        }
    }

    // -----------------------------------------------------------------------
    // ActionTimingOverride — TASK-081
    // -----------------------------------------------------------------------

    #[test]
    fn action_timing_override_default_is_all_none() {
        let o = ActionTimingOverride::default();
        assert!(o.tap_max_ms.is_none());
        assert!(o.hold_min_ms.is_none());
        assert!(o.double_tap_max_gap_ms.is_none());
        assert!(o.optimistic_timeout_ms.is_none());
        assert!(o.queue_max_age_ms.is_none());
    }

    #[test]
    fn action_timing_override_is_copy() {
        let a = ActionTimingOverride {
            tap_max_ms: Some(150),
            ..ActionTimingOverride::default()
        };
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn action_timing_override_partial_set_preserves_none_fields() {
        let o = ActionTimingOverride {
            optimistic_timeout_ms: Some(5000),
            ..ActionTimingOverride::default()
        };
        assert!(o.tap_max_ms.is_none());
        assert!(o.hold_min_ms.is_none());
        assert!(o.double_tap_max_gap_ms.is_none());
        assert_eq!(o.optimistic_timeout_ms, Some(5000));
        assert!(o.queue_max_age_ms.is_none());
    }
}
