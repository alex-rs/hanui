use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::actions::timing::ActionTimingOverride;

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
// BlankingPolicy (TASK-081)
// ---------------------------------------------------------------------------

/// Screen-blanking policy for a device profile.
///
/// Controls whether the display is blanked after a period of inactivity and,
/// if so, after how long. The Phase 5 blanking driver reads this value at
/// startup.
///
/// # Variants
///
/// * [`Self::Never`] — blanking is disabled; the display stays on
///   indefinitely. Default for the `desktop` profile where the launcher
///   manages screen power via OS settings.
/// * [`Self::Idle(Duration)`] — blank the display after the given idle period.
///   Default for `rpi4` and `opi_zero3` kiosk profiles; the canonical Phase 4
///   preset value is `Duration::from_secs(300)` (5 minutes) per
///   `locked_decisions.blanking_policy_yaml_config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlankingPolicy {
    /// Never blank; the OS/launcher handles display power.
    Never,
    /// Blank after the given idle duration.
    Idle(Duration),
}

// ---------------------------------------------------------------------------
// Density (TASK-081)
// ---------------------------------------------------------------------------

/// Grid-density hint for the Phase 4 layout engine.
///
/// Controls the horizontal and vertical spacing between tiles when the layout
/// engine packs the grid. `DeviceProfile.density` sets the default; a future
/// YAML override (`density` field) may override it at load time.
///
/// # Variants
///
/// * [`Self::Compact`] — tighter spacing, suitable for small or lower-DPI
///   displays (e.g. the `opi_zero3` 5-inch panel).
/// * [`Self::Regular`] — the standard spacing used by the `rpi4` preset.
/// * [`Self::Spacious`] — wider gaps for high-DPI or large displays. Default
///   for the `desktop` profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    /// Tighter tile spacing for small displays.
    Compact,
    /// Standard tile spacing.
    Regular,
    /// Wider tile spacing for large / high-DPI displays.
    Spacious,
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
///
/// Group A fields were present in Phase 3. Group B fields are new in Phase 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceProfile {
    // -----------------------------------------------------------------------
    // Group A — Phase 3 carryover
    // -----------------------------------------------------------------------
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

    // -----------------------------------------------------------------------
    // Group B — Phase 4 new fields
    // -----------------------------------------------------------------------
    /// Target frame period p95 budget in milliseconds.
    ///
    /// Phase 5 health socket asserts that p95 frame period stays at or below
    /// this value. Desktop = 16 ms (60 fps), rpi4 = 33 ms (30 fps),
    /// opi_zero3 = 50 ms (20 fps).
    pub target_frame_period_ms: u32,

    /// Idle CPU percentage cap enforced at Phase 5 acceptance.
    pub idle_cpu_pct_cap: u8,

    /// Maximum number of widgets allowed per view.
    ///
    /// Phase 4 validator rejects dashboard YAML whose views exceed this cap.
    pub max_widgets_per_view: usize,

    /// Maximum number of simultaneously active camera streams.
    ///
    /// Phase 6b decoder pool enforces this cap; requests above it are queued.
    pub max_simultaneous_camera_streams: usize,

    /// Whether touch input is expected on this device.
    ///
    /// Phase 4 uses this to decide whether swipe-navigation is instantiated.
    pub touch_input: bool,

    /// Maximum number of pending optimistic entries per entity.
    ///
    /// Phase 3 dispatcher enforces this; new dispatches at the cap are
    /// rejected with a user-visible error.
    pub pending_optimistic_per_entity: usize,

    /// Maximum number of pending optimistic entries across all entities.
    ///
    /// Phase 3 dispatcher enforces the global cap in addition to the per-entity cap.
    pub pending_optimistic_global: usize,

    /// Offline action queue capacity.
    ///
    /// Phase 3 offline queue enforces this; the oldest entry is aged out when
    /// a new entry would overflow.
    pub offline_queue_cap: usize,

    /// Maximum number of attributes shown in the `AttributesBody` more-info
    /// panel (Phase 3).
    pub attributes_body_max_attrs: usize,

    /// Maximum number of characters shown per attribute value in the
    /// `AttributesBody` more-info panel (Phase 3).
    pub attributes_body_max_chars: usize,

    /// Auto-dismiss delay for toast notifications in milliseconds.
    ///
    /// Phase 3 toast layer dismisses toasts after this delay. Also
    /// tap-to-dismiss is always available regardless of this value.
    pub toast_dismiss_ms: u64,

    /// Default camera polling interval in seconds (Phase 6b schema).
    pub camera_interval_default_s: u32,

    /// Minimum camera polling interval in seconds (Phase 6b schema validation).
    pub camera_interval_min_s: u32,

    /// Default history window in seconds (Phase 6b schema).
    pub history_window_default_s: u32,

    /// Maximum history window in seconds (Phase 6b schema).
    pub history_window_max_s: u32,

    /// HTTP cache total size in bytes (Phase 6.0 http.rs).
    pub http_cache_bytes: usize,

    /// HTTP cache TTL in seconds (Phase 6.0 http.rs).
    pub http_cache_ttl_s: u32,

    /// SmallVec inline capacity for the dependency index (Phase 6b visibility
    /// evaluator). Entries above this threshold spill to the heap.
    pub dep_index_inline_cap: usize,

    /// Number of fixed histogram buckets used by the Phase 5 frame-period
    /// metrics collector.
    pub frame_histogram_buckets: usize,

    /// SoC temperature ceiling in degrees Celsius (Phase 5 thermal soak).
    ///
    /// The desktop profile has no SoC sensor; this field is 0 for desktop.
    pub soc_temp_ceiling_c: u8,

    /// Reconnect burst RSS allowance above the idle steady-state in megabytes.
    ///
    /// Phase 2 soak asserts that peak RSS during a reconnect cycle does not
    /// exceed `idle_rss_mb_cap + reconnect_burst_rss_mb`.
    pub reconnect_burst_rss_mb: usize,

    /// Screen-blanking policy.
    ///
    /// Phase 5 blanking driver reads this value at startup.
    pub blanking_policy: BlankingPolicy,

    /// Optional per-profile overrides for action and gesture timing knobs.
    ///
    /// `None` means "use `ActionTiming::default()` unchanged". When `Some`,
    /// the dispatcher applies each non-`None` field on top of the defaults at
    /// startup.
    pub timing_overrides: Option<ActionTimingOverride>,

    /// Grid-density hint for the Phase 4 layout engine.
    pub density: Density,
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

/// Raspberry Pi 4 kiosk preset.
///
/// Values are sourced from the "rpi4" column of the Performance budgets table
/// in `docs/PHASES.md`. When updating this const, keep field names and values
/// in sync with that table.
pub const PROFILE_RPI4: DeviceProfile = DeviceProfile {
    // Group A
    tokio_workers: 2,
    max_entities: 4_096,
    max_simultaneous_animations: 3,
    animation_framerate_cap: 30,
    max_image_px: 1_280,
    ws_payload_cap: 16 * 1024 * 1024,
    snapshot_buffer_events: 5_000,
    idle_rss_mb_cap: 80,
    cpu_smoke_budget_pct: 30,
    url_action_mode: default_url_action_mode("rpi4"),
    // Group B
    target_frame_period_ms: 33,
    idle_cpu_pct_cap: 5,
    max_widgets_per_view: 32,
    max_simultaneous_camera_streams: 2,
    touch_input: true,
    pending_optimistic_per_entity: 4,
    pending_optimistic_global: 64,
    offline_queue_cap: 64,
    attributes_body_max_attrs: 32,
    attributes_body_max_chars: 256,
    toast_dismiss_ms: 4_000,
    camera_interval_default_s: 10,
    camera_interval_min_s: 5,
    history_window_default_s: 6 * 3_600,
    history_window_max_s: 24 * 3_600,
    http_cache_bytes: 32 * 1024 * 1024,
    http_cache_ttl_s: 300,
    dep_index_inline_cap: 8,
    frame_histogram_buckets: 100,
    soc_temp_ceiling_c: 75,
    reconnect_burst_rss_mb: 20,
    blanking_policy: BlankingPolicy::Idle(Duration::from_secs(300)),
    timing_overrides: None,
    density: Density::Regular,
};

/// Orange Pi Zero 3 kiosk preset.
///
/// Values are sourced from the "opi_zero3" column of the Performance budgets
/// table in `docs/PHASES.md`. When updating this const, keep field names and
/// values in sync with that table.
pub const PROFILE_OPI_ZERO3: DeviceProfile = DeviceProfile {
    // Group A
    tokio_workers: 2,
    max_entities: 2_048,
    max_simultaneous_animations: 2,
    animation_framerate_cap: 20,
    max_image_px: 800,
    ws_payload_cap: 8 * 1024 * 1024,
    snapshot_buffer_events: 2_500,
    idle_rss_mb_cap: 60,
    cpu_smoke_budget_pct: 50,
    url_action_mode: default_url_action_mode("opi_zero3"),
    // Group B
    target_frame_period_ms: 50,
    idle_cpu_pct_cap: 10,
    max_widgets_per_view: 20,
    max_simultaneous_camera_streams: 1,
    touch_input: true,
    pending_optimistic_per_entity: 4,
    pending_optimistic_global: 32,
    offline_queue_cap: 32,
    attributes_body_max_attrs: 32,
    attributes_body_max_chars: 256,
    toast_dismiss_ms: 4_000,
    camera_interval_default_s: 30,
    camera_interval_min_s: 10,
    history_window_default_s: 3 * 3_600,
    history_window_max_s: 12 * 3_600,
    http_cache_bytes: 16 * 1024 * 1024,
    http_cache_ttl_s: 300,
    dep_index_inline_cap: 8,
    frame_histogram_buckets: 100,
    soc_temp_ceiling_c: 80,
    reconnect_burst_rss_mb: 20,
    blanking_policy: BlankingPolicy::Idle(Duration::from_secs(300)),
    timing_overrides: None,
    density: Density::Compact,
};

/// Desktop dev-VM preset — the active profile for Phase 1, Phase 2, Phase 3,
/// and Phase 4.
///
/// Values are sourced verbatim from the "desktop" column of the Performance
/// budgets table in `docs/PHASES.md`. When updating this const, keep field
/// names and values in sync with that table.
///
/// `url_action_mode` is [`UrlActionMode::Always`] per
/// `locked_decisions.url_action_gating` — the desktop profile is the dev VM
/// where shelling out to `xdg-open` is the expected behaviour.
pub const PROFILE_DESKTOP: DeviceProfile = DeviceProfile {
    // Group A
    tokio_workers: 4,
    max_entities: 16_384,
    max_simultaneous_animations: 8,
    animation_framerate_cap: 60,
    max_image_px: 2_048,
    ws_payload_cap: 16 * 1024 * 1024,
    snapshot_buffer_events: 10_000,
    idle_rss_mb_cap: 120,
    cpu_smoke_budget_pct: 15,
    url_action_mode: default_url_action_mode("desktop"),
    // Group B
    target_frame_period_ms: 16,
    idle_cpu_pct_cap: 5,
    max_widgets_per_view: 64,
    max_simultaneous_camera_streams: 4,
    touch_input: false,
    pending_optimistic_per_entity: 8,
    pending_optimistic_global: 256,
    offline_queue_cap: 256,
    attributes_body_max_attrs: 64,
    attributes_body_max_chars: 512,
    toast_dismiss_ms: 4_000,
    camera_interval_default_s: 5,
    camera_interval_min_s: 1,
    history_window_default_s: 24 * 3_600,
    history_window_max_s: 168 * 3_600,
    http_cache_bytes: 128 * 1024 * 1024,
    http_cache_ttl_s: 600,
    dep_index_inline_cap: 8,
    frame_histogram_buckets: 100,
    soc_temp_ceiling_c: 0,
    reconnect_burst_rss_mb: 40,
    blanking_policy: BlankingPolicy::Never,
    timing_overrides: None,
    density: Density::Spacious,
};

/// Select a device profile by name.
///
/// Returns a reference to the matching preset constant:
/// * `"rpi4"` → `&PROFILE_RPI4`
/// * `"opi_zero3"` → `&PROFILE_OPI_ZERO3`
/// * `"desktop"` → `&PROFILE_DESKTOP`
/// * `None` or any unrecognised value → `&PROFILE_DESKTOP` (conservative
///   fallback; the desktop profile is always a safe default on the dev VM).
///
/// This is the single entry point for runtime profile selection. Phase 4
/// callers pass the YAML `profile:` field value here; the returned reference
/// is `'static` so it can be stored in a `OnceLock` or passed through `Arc`.
#[must_use]
pub fn select_profile(yaml_override: Option<&str>) -> &'static DeviceProfile {
    match yaml_override {
        Some("rpi4") => &PROFILE_RPI4,
        Some("opi_zero3") => &PROFILE_OPI_ZERO3,
        Some("desktop") | None => &PROFILE_DESKTOP,
        Some(_) => &PROFILE_DESKTOP,
    }
}

/// Map a typed [`ProfileKey`] to its corresponding static [`DeviceProfile`].
///
/// Total mapping (every variant of [`ProfileKey`] resolves to a preset):
/// * [`ProfileKey::Rpi4`] → `&PROFILE_RPI4`
/// * [`ProfileKey::OpiZero3`] → `&PROFILE_OPI_ZERO3`
/// * [`ProfileKey::Desktop`] → `&PROFILE_DESKTOP`
///
/// Used by the F4-bootstrap path in `src/lib.rs::run` (TASK-120a): the early
/// dashboard parse yields a typed `ProfileKey`, this helper converts it to the
/// static profile reference whose `tokio_workers` count seeds the Tokio
/// runtime builder. Distinct from [`select_profile`], which takes a free-form
/// string from older callers — this function is total, exhaustive, and admits
/// no fallback because [`ProfileKey`] is a closed enum. Adding a new variant
/// to [`ProfileKey`] is a compile-time forcing function to update this match.
///
/// [`ProfileKey`]: crate::dashboard::schema::ProfileKey
#[must_use]
pub fn profile_for_key(key: crate::dashboard::schema::ProfileKey) -> &'static DeviceProfile {
    use crate::dashboard::schema::ProfileKey;
    match key {
        ProfileKey::Rpi4 => &PROFILE_RPI4,
        ProfileKey::OpiZero3 => &PROFILE_OPI_ZERO3,
        ProfileKey::Desktop => &PROFILE_DESKTOP,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_desktop_is_copy() {
        // Verify that DeviceProfile is Copy by value-passing it.
        let p = PROFILE_DESKTOP;
        let _q = p; // would fail to compile if not Copy
        assert_eq!(p.tokio_workers, 4);
    }

    // -----------------------------------------------------------------------
    // UrlActionMode defaults — pinned by locked_decisions.url_action_gating
    //
    // The Phase 3 plan locks: rpi4 / opi_zero3 → Never; desktop → Always. The
    // Phase 3 active profile is desktop, so PROFILE_DESKTOP.url_action_mode
    // must be Always; the kiosk profiles read their value via
    // default_url_action_mode().
    // -----------------------------------------------------------------------

    #[test]
    fn profile_desktop_url_action_mode_is_always_for_desktop() {
        assert_eq!(PROFILE_DESKTOP.url_action_mode, UrlActionMode::Always);
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

    #[test]
    fn default_url_action_mode_same_length_different_bytes_falls_back_to_never() {
        // Exercises the byte-mismatch branch inside the `matches_bytes` loop
        // (`return false` after `if a[i] != b[i]`): a profile name whose byte
        // length equals a known target's length but whose bytes differ must
        // NOT match and must fall through to the conservative `Never`
        // fallback. Without this test the loader's inputs always differ in
        // length from every target, so the loop-body mismatch path never
        // executes and per-file coverage drops below the 100 % baseline.
        //
        // "rpi5" is the natural same-length adversarial input for "rpi4":
        // 4 bytes, identical at indices 0..2, differs at index 3
        // ('5' = 0x35 vs '4' = 0x34). The length-guard branch at the top of
        // `matches_bytes` is bypassed; only the per-byte comparison can
        // reject the input.
        assert_eq!(default_url_action_mode("rpi5"), UrlActionMode::Never);
    }

    #[test]
    fn default_url_action_mode_case_sensitivity_pins_documented_behaviour() {
        // `matches_bytes`' rustdoc states: "Comparison is exact (not
        // case-insensitive) because the canonical profile names in
        // docs/PHASES.md are lowercase." This test pins that contract: a
        // casing-only variant of the canonical "desktop" name must NOT
        // resolve to `Always` — it falls through to `Never`. "Desktop" is
        // 7 bytes (identical length to "desktop"), so the length-guard
        // branch in `matches_bytes` does NOT short-circuit; the per-byte
        // mismatch at index 0 ('D' = 0x44 vs 'd' = 0x64) is what rejects
        // the input. This guards against a refactor that "helpfully" adds
        // case-folding — which would silently broaden the shell-out gate on
        // the kiosk profiles if a YAML override mis-cased the name.
        assert_eq!(
            default_url_action_mode("Desktop"),
            UrlActionMode::Never,
            "case-sensitive comparison must reject mixed-case `Desktop` and \
             fall through to the conservative `Never` fallback — not `Always`"
        );
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

    // -----------------------------------------------------------------------
    // select_profile — TASK-081
    // -----------------------------------------------------------------------

    #[test]
    fn select_profile_rpi4_returns_rpi4_preset() {
        let p = select_profile(Some("rpi4"));
        assert_eq!(p.tokio_workers, 2);
        assert_eq!(p.max_entities, 4_096);
    }

    #[test]
    fn select_profile_opi_zero3_returns_opi_zero3_preset() {
        let p = select_profile(Some("opi_zero3"));
        assert_eq!(p.tokio_workers, 2);
        assert_eq!(p.max_entities, 2_048);
    }

    #[test]
    fn select_profile_desktop_returns_desktop_preset() {
        let p = select_profile(Some("desktop"));
        assert_eq!(p.tokio_workers, 4);
    }

    #[test]
    fn select_profile_none_returns_desktop_preset() {
        let p = select_profile(None);
        assert_eq!(p.tokio_workers, 4);
    }

    #[test]
    fn select_profile_unrecognised_returns_desktop_preset() {
        let p = select_profile(Some("future-tablet"));
        assert_eq!(p.tokio_workers, 4);
    }

    // -----------------------------------------------------------------------
    // profile_for_key — TASK-120a
    //
    // The typed `ProfileKey` accessor used by the F4-bootstrap path in
    // `src/lib.rs::run`. Asserting one preset value per variant pins the
    // mapping; if a future variant is added without extending the match,
    // the compile-time exhaustiveness check fails (no test needed for that).
    // -----------------------------------------------------------------------

    #[test]
    fn profile_for_key_rpi4_returns_rpi4_preset() {
        use crate::dashboard::schema::ProfileKey;
        let p = profile_for_key(ProfileKey::Rpi4);
        assert_eq!(p.tokio_workers, 2);
        assert_eq!(p.max_entities, 4_096);
    }

    #[test]
    fn profile_for_key_opi_zero3_returns_opi_zero3_preset() {
        use crate::dashboard::schema::ProfileKey;
        let p = profile_for_key(ProfileKey::OpiZero3);
        assert_eq!(p.tokio_workers, 2);
        assert_eq!(p.max_entities, 2_048);
    }

    #[test]
    fn profile_for_key_desktop_returns_desktop_preset() {
        use crate::dashboard::schema::ProfileKey;
        let p = profile_for_key(ProfileKey::Desktop);
        assert_eq!(p.tokio_workers, 4);
        assert_eq!(p.max_entities, 16_384);
    }

    /// Each `ProfileKey` variant must map to a *distinct* preset. Without this
    /// guard, a copy-paste error in `profile_for_key` (e.g. all three variants
    /// pointing at `PROFILE_DESKTOP`) would silently regress runtime behaviour
    /// on rpi4 / opi_zero3 hardware. The check uses value-equality against
    /// each `pub const` preset (pointer-equality is unreliable across `const`
    /// items, which the compiler may inline at each call site).
    #[test]
    fn profile_for_key_returns_distinct_presets_per_variant() {
        use crate::dashboard::schema::ProfileKey;
        let rpi4 = profile_for_key(ProfileKey::Rpi4);
        let opi = profile_for_key(ProfileKey::OpiZero3);
        let desktop = profile_for_key(ProfileKey::Desktop);
        assert_eq!(*rpi4, PROFILE_RPI4, "Rpi4 must map to PROFILE_RPI4");
        assert_eq!(
            *opi, PROFILE_OPI_ZERO3,
            "OpiZero3 must map to PROFILE_OPI_ZERO3"
        );
        assert_eq!(
            *desktop, PROFILE_DESKTOP,
            "Desktop must map to PROFILE_DESKTOP"
        );
        assert_ne!(*rpi4, *opi, "rpi4 and opi presets must not value-alias");
        assert_ne!(
            *rpi4, *desktop,
            "rpi4 and desktop presets must not value-alias"
        );
        assert_ne!(
            *opi, *desktop,
            "opi and desktop presets must not value-alias"
        );
    }

    // -----------------------------------------------------------------------
    // preset_values_match_phases_md_budgets_table — TASK-081 table-pin test
    //
    // Every numeric field of every preset is asserted explicitly. ~30 fields
    // × 3 presets ≈ 90 assertions. This test is verbose by design — a future
    // struct edit without a matching table edit MUST fail CI.
    // -----------------------------------------------------------------------

    #[test]
    fn preset_values_match_phases_md_budgets_table() {
        // --- PROFILE_RPI4 ---
        assert_eq!(PROFILE_RPI4.tokio_workers, 2);
        assert_eq!(PROFILE_RPI4.max_entities, 4_096);
        assert_eq!(PROFILE_RPI4.max_simultaneous_animations, 3);
        assert_eq!(PROFILE_RPI4.animation_framerate_cap, 30);
        assert_eq!(PROFILE_RPI4.max_image_px, 1_280);
        assert_eq!(PROFILE_RPI4.ws_payload_cap, 16 * 1024 * 1024);
        assert_eq!(PROFILE_RPI4.snapshot_buffer_events, 5_000);
        assert_eq!(PROFILE_RPI4.idle_rss_mb_cap, 80);
        assert_eq!(PROFILE_RPI4.cpu_smoke_budget_pct, 30);
        assert_eq!(PROFILE_RPI4.url_action_mode, UrlActionMode::Never);
        assert_eq!(PROFILE_RPI4.target_frame_period_ms, 33);
        assert_eq!(PROFILE_RPI4.idle_cpu_pct_cap, 5);
        assert_eq!(PROFILE_RPI4.max_widgets_per_view, 32);
        assert_eq!(PROFILE_RPI4.max_simultaneous_camera_streams, 2);
        const { assert!(PROFILE_RPI4.touch_input) };
        assert_eq!(PROFILE_RPI4.pending_optimistic_per_entity, 4);
        assert_eq!(PROFILE_RPI4.pending_optimistic_global, 64);
        assert_eq!(PROFILE_RPI4.offline_queue_cap, 64);
        assert_eq!(PROFILE_RPI4.attributes_body_max_attrs, 32);
        assert_eq!(PROFILE_RPI4.attributes_body_max_chars, 256);
        assert_eq!(PROFILE_RPI4.toast_dismiss_ms, 4_000);
        assert_eq!(PROFILE_RPI4.camera_interval_default_s, 10);
        assert_eq!(PROFILE_RPI4.camera_interval_min_s, 5);
        assert_eq!(PROFILE_RPI4.history_window_default_s, 6 * 3_600);
        assert_eq!(PROFILE_RPI4.history_window_max_s, 24 * 3_600);
        assert_eq!(PROFILE_RPI4.http_cache_bytes, 32 * 1024 * 1024);
        assert_eq!(PROFILE_RPI4.http_cache_ttl_s, 300);
        assert_eq!(PROFILE_RPI4.dep_index_inline_cap, 8);
        assert_eq!(PROFILE_RPI4.frame_histogram_buckets, 100);
        assert_eq!(PROFILE_RPI4.soc_temp_ceiling_c, 75);
        assert_eq!(PROFILE_RPI4.reconnect_burst_rss_mb, 20);
        assert!(
            matches!(PROFILE_RPI4.blanking_policy, BlankingPolicy::Idle(d) if d == Duration::from_secs(300))
        );
        assert!(PROFILE_RPI4.timing_overrides.is_none());
        assert_eq!(PROFILE_RPI4.density, Density::Regular);

        // --- PROFILE_OPI_ZERO3 ---
        assert_eq!(PROFILE_OPI_ZERO3.tokio_workers, 2);
        assert_eq!(PROFILE_OPI_ZERO3.max_entities, 2_048);
        assert_eq!(PROFILE_OPI_ZERO3.max_simultaneous_animations, 2);
        assert_eq!(PROFILE_OPI_ZERO3.animation_framerate_cap, 20);
        assert_eq!(PROFILE_OPI_ZERO3.max_image_px, 800);
        assert_eq!(PROFILE_OPI_ZERO3.ws_payload_cap, 8 * 1024 * 1024);
        assert_eq!(PROFILE_OPI_ZERO3.snapshot_buffer_events, 2_500);
        assert_eq!(PROFILE_OPI_ZERO3.idle_rss_mb_cap, 60);
        assert_eq!(PROFILE_OPI_ZERO3.cpu_smoke_budget_pct, 50);
        assert_eq!(PROFILE_OPI_ZERO3.url_action_mode, UrlActionMode::Never);
        assert_eq!(PROFILE_OPI_ZERO3.target_frame_period_ms, 50);
        assert_eq!(PROFILE_OPI_ZERO3.idle_cpu_pct_cap, 10);
        assert_eq!(PROFILE_OPI_ZERO3.max_widgets_per_view, 20);
        assert_eq!(PROFILE_OPI_ZERO3.max_simultaneous_camera_streams, 1);
        const { assert!(PROFILE_OPI_ZERO3.touch_input) };
        assert_eq!(PROFILE_OPI_ZERO3.pending_optimistic_per_entity, 4);
        assert_eq!(PROFILE_OPI_ZERO3.pending_optimistic_global, 32);
        assert_eq!(PROFILE_OPI_ZERO3.offline_queue_cap, 32);
        assert_eq!(PROFILE_OPI_ZERO3.attributes_body_max_attrs, 32);
        assert_eq!(PROFILE_OPI_ZERO3.attributes_body_max_chars, 256);
        assert_eq!(PROFILE_OPI_ZERO3.toast_dismiss_ms, 4_000);
        assert_eq!(PROFILE_OPI_ZERO3.camera_interval_default_s, 30);
        assert_eq!(PROFILE_OPI_ZERO3.camera_interval_min_s, 10);
        assert_eq!(PROFILE_OPI_ZERO3.history_window_default_s, 3 * 3_600);
        assert_eq!(PROFILE_OPI_ZERO3.history_window_max_s, 12 * 3_600);
        assert_eq!(PROFILE_OPI_ZERO3.http_cache_bytes, 16 * 1024 * 1024);
        assert_eq!(PROFILE_OPI_ZERO3.http_cache_ttl_s, 300);
        assert_eq!(PROFILE_OPI_ZERO3.dep_index_inline_cap, 8);
        assert_eq!(PROFILE_OPI_ZERO3.frame_histogram_buckets, 100);
        assert_eq!(PROFILE_OPI_ZERO3.soc_temp_ceiling_c, 80);
        assert_eq!(PROFILE_OPI_ZERO3.reconnect_burst_rss_mb, 20);
        assert!(
            matches!(PROFILE_OPI_ZERO3.blanking_policy, BlankingPolicy::Idle(d) if d == Duration::from_secs(300))
        );
        assert!(PROFILE_OPI_ZERO3.timing_overrides.is_none());
        assert_eq!(PROFILE_OPI_ZERO3.density, Density::Compact);

        // --- PROFILE_DESKTOP ---
        assert_eq!(PROFILE_DESKTOP.tokio_workers, 4);
        assert_eq!(PROFILE_DESKTOP.max_entities, 16_384);
        assert_eq!(PROFILE_DESKTOP.max_simultaneous_animations, 8);
        assert_eq!(PROFILE_DESKTOP.animation_framerate_cap, 60);
        assert_eq!(PROFILE_DESKTOP.max_image_px, 2_048);
        assert_eq!(PROFILE_DESKTOP.ws_payload_cap, 16 * 1024 * 1024);
        assert_eq!(PROFILE_DESKTOP.snapshot_buffer_events, 10_000);
        assert_eq!(PROFILE_DESKTOP.idle_rss_mb_cap, 120);
        assert_eq!(PROFILE_DESKTOP.cpu_smoke_budget_pct, 15);
        assert_eq!(PROFILE_DESKTOP.url_action_mode, UrlActionMode::Always);
        assert_eq!(PROFILE_DESKTOP.target_frame_period_ms, 16);
        assert_eq!(PROFILE_DESKTOP.idle_cpu_pct_cap, 5);
        assert_eq!(PROFILE_DESKTOP.max_widgets_per_view, 64);
        assert_eq!(PROFILE_DESKTOP.max_simultaneous_camera_streams, 4);
        const { assert!(!PROFILE_DESKTOP.touch_input) };
        assert_eq!(PROFILE_DESKTOP.pending_optimistic_per_entity, 8);
        assert_eq!(PROFILE_DESKTOP.pending_optimistic_global, 256);
        assert_eq!(PROFILE_DESKTOP.offline_queue_cap, 256);
        assert_eq!(PROFILE_DESKTOP.attributes_body_max_attrs, 64);
        assert_eq!(PROFILE_DESKTOP.attributes_body_max_chars, 512);
        assert_eq!(PROFILE_DESKTOP.toast_dismiss_ms, 4_000);
        assert_eq!(PROFILE_DESKTOP.camera_interval_default_s, 5);
        assert_eq!(PROFILE_DESKTOP.camera_interval_min_s, 1);
        assert_eq!(PROFILE_DESKTOP.history_window_default_s, 24 * 3_600);
        assert_eq!(PROFILE_DESKTOP.history_window_max_s, 168 * 3_600);
        assert_eq!(PROFILE_DESKTOP.http_cache_bytes, 128 * 1024 * 1024);
        assert_eq!(PROFILE_DESKTOP.http_cache_ttl_s, 600);
        assert_eq!(PROFILE_DESKTOP.dep_index_inline_cap, 8);
        assert_eq!(PROFILE_DESKTOP.frame_histogram_buckets, 100);
        assert_eq!(PROFILE_DESKTOP.soc_temp_ceiling_c, 0);
        assert_eq!(PROFILE_DESKTOP.reconnect_burst_rss_mb, 40);
        assert_eq!(PROFILE_DESKTOP.blanking_policy, BlankingPolicy::Never);
        assert!(PROFILE_DESKTOP.timing_overrides.is_none());
        assert_eq!(PROFILE_DESKTOP.density, Density::Spacious);
    }
}
