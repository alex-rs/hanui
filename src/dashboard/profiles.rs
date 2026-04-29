use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// UrlActionMode (TASK-063)
// ---------------------------------------------------------------------------

/// Per-profile gate for the `Url` action's external-process boundary.
///
/// The `Url` action shells out to `xdg-open`; this enum decides whether the
/// shell-out is permitted, blocked, or deferred to a confirmation dialog.
/// `DeviceProfile.url_action_mode` selects the active branch at dispatch
/// time. The gate exists per `docs/plans/2026-04-28-phase-3-actions.md`
/// `locked_decisions.url_action_gating`.
///
/// # Variants
///
/// * [`Self::Always`] — shell out to `xdg-open` directly. Phase 3 default for
///   the `desktop` profile (the dev VM).
/// * [`Self::Never`] — emit a "URL actions are disabled on this device profile"
///   toast and do not shell out. Phase 3 default for the `rpi4` and
///   `opi_zero3` kiosk profiles, which should not spawn browsers.
/// * [`Self::Ask`] — emit a "Confirmation dialog comes in Phase 6" toast and
///   do not shell out. Phase 6 owns the actual confirmation-dialog UI per
///   `docs/PHASES.md` line 219; this variant ships its handler shape today
///   so Phase 6 only swaps the Ask branch handler without reshaping the enum.
///
/// # Phase 4 forward-compat (YAML override source)
///
/// Phase 4 populates `DeviceProfile.url_action_mode` from the YAML loader (the
/// override source). The serde derives are present from day one so Phase 4
/// does not need to reshape this type — only populate its values from YAML.
/// The wire form is kebab-case (`always`, `never`, `ask`) to match
/// `docs/DASHBOARD_SCHEMA.md` conventions.
///
/// # Phase 6 forward-compat (Ask handler swap-in)
///
/// Phase 6 swaps the `Ask` branch handler from "emit a 'Phase 6' toast" to
/// "open a confirmation dialog and shell out on confirm". The enum shape and
/// `non_exhaustive` flag are picked so Phase 6 does not need a schema
/// migration — only the handler implementation changes.
///
/// # `#[non_exhaustive]`
///
/// Set per `locked_decisions.url_action_gating` so future variants (e.g. an
/// allowlist-based `AllowList(Vec<String>)`) can be added in later phases
/// without breaking external serde consumers — Phase 4 YAML loaders must
/// always carry an explicit fallback branch.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UrlActionMode {
    /// Shell out to `xdg-open`. Default for the `desktop` profile.
    Always,
    /// Emit an error toast; never shell out. Default for `rpi4` /
    /// `opi_zero3` kiosk profiles.
    Never,
    /// Emit a "Confirmation dialog comes in Phase 6" toast; never shell out
    /// in Phase 3. Phase 6 swaps in the actual confirmation dialog handler.
    Ask,
}

/// Resolve the Phase-3 default [`UrlActionMode`] for a named profile.
///
/// `rpi4` and `opi_zero3` kiosk SBC profiles default to [`UrlActionMode::Never`]
/// — kiosks should not spawn browsers under any circumstance. `desktop` (the
/// Phase 3 dev VM target) defaults to [`UrlActionMode::Always`].
///
/// Profiles not listed (forward-compat: Phase 5 may add more) default to
/// [`UrlActionMode::Never`] — the conservative branch — until the YAML loader
/// (Phase 4) sets an explicit value.
///
/// This helper is the single source of truth for those defaults; Phase 5
/// preset constants will read from it when constructing the rpi4 / opi_zero3
/// profile values.
#[must_use]
pub const fn default_url_action_mode(profile_name: &str) -> UrlActionMode {
    let bytes = profile_name.as_bytes();
    if matches_bytes(bytes, b"desktop") {
        UrlActionMode::Always
    } else if matches_bytes(bytes, b"rpi4") || matches_bytes(bytes, b"opi_zero3") {
        UrlActionMode::Never
    } else {
        // Conservative fallback for unknown profile names — Phase 4 YAML
        // overrides this when an explicit value is supplied.
        UrlActionMode::Never
    }
}

/// const-friendly byte-slice equality check for [`default_url_action_mode`].
///
/// `str` equality methods are not const-stable on this MSRV; this helper
/// expresses the byte-by-byte comparison the const context needs. Comparison
/// is exact (not case-insensitive) because the canonical profile names in
/// `docs/PHASES.md` are lowercase.
const fn matches_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ---------------------------------------------------------------------------
// DeviceProfile
// ---------------------------------------------------------------------------

/// Per-device performance budget profile.
///
/// All fields are plain numeric types or [`Copy`] enums, so the struct is
/// `Copy` and can be passed by value into runtime initialization without
/// borrow gymnastics.
///
/// Source of truth for numeric caps: `docs/PHASES.md` "Performance budgets"
/// table. When updating this struct, keep field names and values in sync with
/// that table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceProfile {
    /// Number of Tokio worker threads for the async runtime.
    ///
    /// Phase 1: `main.rs` runtime builder reads this field; no `num_cpus`
    /// default is permitted.
    pub tokio_workers: usize,

    /// Maximum number of entities the in-memory store will accept.
    ///
    /// `MemoryStore::load` returns an error if the fixture exceeds this limit.
    pub max_entities: usize,

    /// Maximum number of simultaneously running tile animations.
    ///
    /// `card_base.slint` gates new animations against this value; a card
    /// requesting an animation while at the cap renders the end-state
    /// immediately without animating.
    pub max_simultaneous_animations: usize,

    /// Animation framerate cap in frames per second.
    ///
    /// Used by `card_base.slint` to derive the timer interval for animations.
    pub animation_framerate_cap: u32,

    /// Maximum image size (longest dimension, in pixels) accepted by the icon
    /// resolver.
    ///
    /// Icons exceeding this value are downscaled at startup before being
    /// stored in the `OnceLock` cache.
    pub max_image_px: u32,

    /// Maximum WebSocket message and frame size in bytes accepted from HA.
    ///
    /// Phase 2 consumer: `src/ha/client.rs` passes this value to
    /// `tokio-tungstenite` `max_message_size` / `max_frame_size` config.
    /// On overflow, the connection is dropped and a full resync is initiated.
    ///
    /// Source: `docs/PHASES.md` Performance budgets table — "WS payload cap"
    /// desktop value (16 MiB).
    pub ws_payload_cap: usize,

    /// Capacity of the ring buffer that holds state-changed events arriving
    /// from HA during the initial snapshot (`get_states`) fetch.
    ///
    /// Phase 2 consumer: `src/ha/client.rs` allocates this ring at
    /// connection time. On overflow the connection is dropped; after 3
    /// consecutive overflows within 60 s the FSM transitions to `Failed`.
    ///
    /// Source: `docs/PHASES.md` Performance budgets table — "Snapshot-buffer
    /// events" desktop value (10 000).
    pub snapshot_buffer_events: usize,

    /// Maximum idle resident set size in megabytes.
    ///
    /// Phase 2 consumer: `tests/soak/memory.rs` (TASK-039) asserts that
    /// absolute peak RSS does not exceed this cap at 1000 entities / 50 ev/s.
    ///
    /// Source: `docs/PHASES.md` Performance budgets table — "Idle RSS cap"
    /// desktop value (120 MB).
    pub idle_rss_mb_cap: usize,

    /// CPU usage budget as a percentage, measured under rpi4-class QEMU
    /// user-mode emulation.
    ///
    /// Phase 2 consumer: `tests/smoke/sbc_cpu.rs` (TASK-040) asserts that
    /// CPU% stays below this value during 60 s of 50 ev/s churn on aarch64
    /// QEMU. Real SBC numbers on physical hardware are a Phase 5 acceptance
    /// gate, not Phase 2.
    ///
    /// Source: `docs/PHASES.md` Performance budgets table — "CPU smoke budget
    /// (QEMU)" rpi4 value (30 %).
    pub cpu_smoke_budget_pct: u8,

    /// Per-profile gate for the `Url` action shell-out boundary.
    ///
    /// Selects how `Action::Url { href }` is handled at dispatch time:
    /// shell out to `xdg-open` ([`UrlActionMode::Always`]), refuse and toast
    /// ([`UrlActionMode::Never`]), or defer to a Phase-6 confirmation dialog
    /// ([`UrlActionMode::Ask`]). See [`UrlActionMode`] for the full
    /// per-profile defaults.
    ///
    /// `security-engineer` review of TASK-063 enforces that this is the only
    /// gate on the `xdg-open` shell-out path, and that `href` is sourced from
    /// the action spec — never from live entity state.
    pub url_action_mode: UrlActionMode,
}

/// Desktop preset — the active profile for Phase 1, Phase 2, and Phase 3.
///
/// Values are sourced verbatim from the "desktop" column of the Performance
/// budgets table in `docs/PHASES.md`. When updating this const, keep field
/// names and values in sync with that table.
///
/// `url_action_mode` is [`UrlActionMode::Always`] per
/// `locked_decisions.url_action_gating` — the desktop profile is the dev VM
/// where shelling out to `xdg-open` is the expected behaviour.
///
/// rpi4 / opi_zero3 presets land in Phase 5. Their `url_action_mode` will be
/// [`UrlActionMode::Never`] per [`default_url_action_mode`].
pub const DEFAULT_PROFILE: DeviceProfile = DeviceProfile {
    tokio_workers: 4,
    max_entities: 16_384,
    max_simultaneous_animations: 8,
    animation_framerate_cap: 60,
    max_image_px: 2_048,
    ws_payload_cap: 16 * 1024 * 1024,
    snapshot_buffer_events: 10_000,
    idle_rss_mb_cap: 120,
    cpu_smoke_budget_pct: 30,
    url_action_mode: default_url_action_mode("desktop"),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_copy() {
        // Verify that DeviceProfile is Copy by value-passing it.
        let p = DEFAULT_PROFILE;
        let _q = p; // would fail to compile if not Copy
        assert_eq!(p.tokio_workers, 4);
    }

    #[test]
    fn default_profile_desktop_values_match_phases_md() {
        assert_eq!(DEFAULT_PROFILE.tokio_workers, 4);
        assert_eq!(DEFAULT_PROFILE.max_entities, 16_384);
        assert_eq!(DEFAULT_PROFILE.max_simultaneous_animations, 8);
        assert_eq!(DEFAULT_PROFILE.animation_framerate_cap, 60);
        assert_eq!(DEFAULT_PROFILE.max_image_px, 2_048);
        // Phase 2 budget fields — values from docs/PHASES.md Performance
        // budgets table, desktop column.
        assert_eq!(DEFAULT_PROFILE.ws_payload_cap, 16 * 1024 * 1024);
        assert_eq!(DEFAULT_PROFILE.snapshot_buffer_events, 10_000);
        assert_eq!(DEFAULT_PROFILE.idle_rss_mb_cap, 120);
        assert_eq!(DEFAULT_PROFILE.cpu_smoke_budget_pct, 30);
    }

    // -----------------------------------------------------------------------
    // UrlActionMode defaults — pinned by locked_decisions.url_action_gating
    //
    // The Phase 3 plan locks: rpi4 / opi_zero3 → Never; desktop → Always. The
    // Phase 3 active profile is desktop, so DEFAULT_PROFILE.url_action_mode
    // must be Always; the kiosk profiles (Phase 5 land their full preset
    // structs) read their value via default_url_action_mode().
    // -----------------------------------------------------------------------

    #[test]
    fn default_profile_url_action_mode_is_always_for_desktop() {
        assert_eq!(DEFAULT_PROFILE.url_action_mode, UrlActionMode::Always);
    }

    #[test]
    fn default_url_action_mode_for_rpi4_is_never() {
        assert_eq!(default_url_action_mode("rpi4"), UrlActionMode::Never);
    }

    #[test]
    fn default_url_action_mode_for_opi_zero3_is_never() {
        assert_eq!(default_url_action_mode("opi_zero3"), UrlActionMode::Never);
    }

    #[test]
    fn default_url_action_mode_for_desktop_is_always() {
        assert_eq!(default_url_action_mode("desktop"), UrlActionMode::Always);
    }

    #[test]
    fn default_url_action_mode_unknown_profile_falls_back_to_never() {
        // Conservative fallback for forward-compat: Phase 5 may add more
        // profile names; until they have explicit values, the safe default
        // is Never (no shell-out).
        assert_eq!(
            default_url_action_mode("future-tablet"),
            UrlActionMode::Never
        );
        assert_eq!(default_url_action_mode(""), UrlActionMode::Never);
    }

    // -----------------------------------------------------------------------
    // UrlActionMode serde — kebab-case wire form
    //
    // Phase 4 will deserialize this enum from YAML; the wire form must match
    // the kebab-case convention from docs/DASHBOARD_SCHEMA.md so the YAML
    // loader does not need a schema migration. The serde derives are present
    // from day one (locked_decisions.url_action_gating).
    // -----------------------------------------------------------------------

    #[test]
    fn url_action_mode_serializes_as_kebab_case() {
        for (mode, expected) in [
            (UrlActionMode::Always, "\"always\""),
            (UrlActionMode::Never, "\"never\""),
            (UrlActionMode::Ask, "\"ask\""),
        ] {
            let json = serde_json::to_string(&mode).expect("serialize must succeed");
            assert_eq!(
                json, expected,
                "UrlActionMode::{mode:?} must serialize as {expected}"
            );
        }
    }

    #[test]
    fn url_action_mode_round_trips_through_json() {
        for mode in [
            UrlActionMode::Always,
            UrlActionMode::Never,
            UrlActionMode::Ask,
        ] {
            let json = serde_json::to_string(&mode).expect("serialize");
            let decoded: UrlActionMode =
                serde_json::from_str(&json).expect("deserialize round-trip");
            assert_eq!(decoded, mode);
        }
    }

    #[test]
    fn url_action_mode_pascal_case_is_rejected() {
        // Defensive: if someone removes the kebab-case rename the PascalCase
        // literal would silently regress. This test pins the kebab-case
        // requirement.
        let result: Result<UrlActionMode, _> = serde_json::from_str("\"Always\"");
        assert!(
            result.is_err(),
            "PascalCase `Always` must NOT deserialize; only kebab-case `always` is accepted"
        );
    }
}
