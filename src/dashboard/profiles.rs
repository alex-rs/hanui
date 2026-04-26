/// Per-device performance budget profile.
///
/// All fields are plain numeric types, so the struct is `Copy` and can be
/// passed by value into runtime initialization without borrow gymnastics.
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
}

/// Desktop preset — the active profile for Phase 1 and Phase 2.
///
/// Values are sourced verbatim from the "desktop" column of the Performance
/// budgets table in `docs/PHASES.md`. When updating this const, keep field
/// names and values in sync with that table.
///
/// rpi4 / opi_zero3 presets land in Phase 5.
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
}
