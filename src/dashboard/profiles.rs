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
}

/// Desktop preset — the only profile that ships in Phase 1.
///
/// Values are sourced verbatim from the "desktop" column of the Performance
/// budgets table in `docs/PHASES.md` lines 25-63.
///
// TODO Phase 4: add rpi4/opi_zero3
pub const DEFAULT_PROFILE: DeviceProfile = DeviceProfile {
    tokio_workers: 4,
    max_entities: 16_384,
    max_simultaneous_animations: 8,
    animation_framerate_cap: 60,
    max_image_px: 2_048,
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
    }
}
