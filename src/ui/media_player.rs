//! Media-player widget view-model and per-frame state derivation (TASK-109).
//!
//! # Hot-path discipline
//!
//! [`MediaPlayerVM::from_entity`] is invoked at entity-change time, NOT per
//! render. The bridge's `build_tiles` / `apply_row_updates` paths call it
//! once per `media_player.*` state-change event; the resulting
//! [`MediaPlayerVM`] is then projected into the Slint-typed
//! `MediaPlayerTileVM` (in `bridge.rs`) and pushed via the row-update
//! path. No allocation occurs in any per-frame Slint callback.
//!
//! # State vocabulary (Home Assistant `media_player.*` entity)
//!
//! Home Assistant exposes the following canonical states for
//! `media_player.*` entities:
//!
//!   * `"playing"`     — actively playing media.
//!   * `"paused"`      — playback paused.
//!   * `"idle"`        — powered on, no media active.
//!   * `"on"`          — powered on (some integrations report `on` instead
//!     of `idle` when no media is loaded).
//!   * `"off"`         — powered off.
//!   * `"standby"`     — device in low-power state.
//!   * `"buffering"`   — loading media.
//!   * `"unavailable"` — not reachable.
//!
//! `MediaPlayerVM` encodes only the **derived view-state** the tile needs:
//!   * `state` — canonical HA state string forwarded verbatim.
//!   * `media_title` — current track title from the `media_title` attribute.
//!   * `artist` — current track artist from the `media_artist` attribute.
//!   * `volume_level` — current volume in 0.0..=1.0 from the `volume_level`
//!     attribute. Out-of-range values are clamped.
//!   * `is_playing` — `true` when the entity state is `"playing"`.
//!
//! # Why no `Vec` fields (lesson from TASK-103 / TASK-105 / TASK-107 / TASK-108)
//!
//! `MediaPlayerVM` deliberately carries no `Vec` fields. Source list /
//! sound-mode list / transport-set lists live on
//! [`crate::dashboard::schema::WidgetOptions::MediaPlayer`] (read at
//! modal-open time by [`crate::ui::more_info::MediaPlayerBody`] and the
//! transport dispatcher), NOT on the per-frame tile VM. Allocating a
//! `Vec` per state-change event for a list that the tile renderer never
//! reads would be wasted work; the lesson learned in `FanVM` and
//! reinforced by every per-domain VM since is to keep the per-frame VM
//! lean and scalar.
//!
//! # JSON-crate discipline (CI Gate 2)
//!
//! `src/ui/**` is gated against direct references to the JSON crate by
//! `.github/workflows/ci.yml` Gate 2. The implementation reads
//! `entity.attributes` only via inferred-type accessors (`.as_str`,
//! `.as_f64`, `.as_i64`, `.as_u64`) — never the JSON-crate `Value`
//! type by name.

use crate::ha::entity::Entity;

// ---------------------------------------------------------------------------
// MediaPlayerVM
// ---------------------------------------------------------------------------

/// Per-frame derived view-state for a `media_player.*` entity.
///
/// Built by [`MediaPlayerVM::from_entity`] at entity-change time. The bridge
/// then projects this into a Slint `MediaPlayerTileVM` and pushes the row
/// update — see `src/ui/bridge.rs::compute_media_player_tile_vm`.
///
/// # Field semantics
///
/// * `state` — canonical HA state string forwarded verbatim to the Slint
///   tile for the hero label.
/// * `media_title` — present when HA reports `media_title` on the entity
///   attributes; absent otherwise. Forwarded as the track-title label on
///   the tile.
/// * `artist` — present when HA reports `media_artist`; absent otherwise.
///   Forwarded as the secondary label below the title.
/// * `volume_level` — present when HA reports `volume_level` (a `f32` /
///   `f64` numeric attribute in 0.0..=1.0). Clamped to that range; values
///   outside it are clamped rather than dropped (volume is a continuous
///   control and a misbehaving integration emitting `1.5` should still
///   render the slider at the maximum).
/// * `is_playing` — `true` when the state string equals `"playing"`.
///   Drives the "active" visual variant on the tile.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaPlayerVM {
    /// Canonical HA state string for the tile hero label.
    pub state: String,
    /// Currently-playing track title (HA `media_title`), if present.
    pub media_title: Option<String>,
    /// Currently-playing track artist (HA `media_artist`), if present.
    pub artist: Option<String>,
    /// Current volume in 0.0..=1.0 (HA `volume_level`), if present.
    pub volume_level: Option<f32>,
    /// True when state equals `"playing"`.
    pub is_playing: bool,
}

impl MediaPlayerVM {
    /// Construct a [`MediaPlayerVM`] from a live [`Entity`] snapshot.
    ///
    /// # State mapping
    ///
    /// | HA state         | `is_playing` |
    /// |------------------|--------------|
    /// | `"playing"`      | true         |
    /// | `"paused"`       | false        |
    /// | `"idle"`         | false        |
    /// | `"on"`           | false        |
    /// | `"off"`          | false        |
    /// | `"standby"`      | false        |
    /// | `"buffering"`    | false        |
    /// | `"unavailable"`  | false        |
    /// | other / vendor   | false        |
    ///
    /// `state` is forwarded verbatim — the Slint tile renders the value
    /// as the hero label. The boolean `is_playing` is precomputed here
    /// so the Slint side does not branch on the state-string vocabulary.
    ///
    /// `volume_level` is read from the HA `volume_level` attribute and
    /// clamped to 0.0..=1.0 (a misbehaving integration reporting outside
    /// that range still renders a sane slider; see field doc).
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Self {
        let state = entity.state.as_ref();
        let is_playing = state == "playing";

        let media_title = read_string_attribute(entity, "media_title");
        let artist = read_string_attribute(entity, "media_artist");
        let volume_level = read_volume_level_attribute(entity);

        MediaPlayerVM {
            state: state.to_owned(),
            media_title,
            artist,
            volume_level,
            is_playing,
        }
    }
}

// ---------------------------------------------------------------------------
// Attribute accessors (read by the bridge and `more_info::MediaPlayerBody`)
// ---------------------------------------------------------------------------

/// Read the `volume_level` attribute as `f32` clamped to 0.0..=1.0.
///
/// HA's media-player integration exposes the current volume as a numeric
/// attribute in 0.0..=1.0. We accept any of `as_f64` / `as_i64` / `as_u64`
/// because HA integrations vary (most emit floats, but some integer-only
/// devices emit `1` for full volume).
///
/// Returns `None` when the attribute is absent or the JSON value is not
/// numeric. Out-of-range values are **clamped** to 0.0..=1.0 (volume is
/// a continuous control; rendering an out-of-spec value as `None` would
/// hide a misbehaving integration's volume from the user).
///
/// `NaN` and infinity are treated as absent — they are not coerce-able
/// to a sensible volume.
#[must_use]
pub fn read_volume_level_attribute(entity: &Entity) -> Option<f32> {
    let value = entity.attributes.get("volume_level")?;
    let raw = if let Some(f) = value.as_f64() {
        if !f.is_finite() {
            return None;
        }
        f as f32
    } else if let Some(i) = value.as_i64() {
        i as f32
    } else if let Some(u) = value.as_u64() {
        u as f32
    } else {
        return None;
    };

    Some(raw.clamp(0.0, 1.0))
}

/// Read the `source` attribute as a `String` if present.
///
/// HA's media-player integration exposes the active input source (e.g.
/// `"HDMI 1"`, `"Spotify"`) under `source`. Used by
/// [`crate::ui::more_info::MediaPlayerBody`].
#[must_use]
pub fn read_source_attribute(entity: &Entity) -> Option<String> {
    read_string_attribute(entity, "source")
}

/// Read the `sound_mode` attribute as a `String` if present.
///
/// HA's media-player integration exposes the active sound mode (e.g.
/// `"Movie"`, `"Music"`, `"Stereo"`) under `sound_mode`. Used by
/// [`crate::ui::more_info::MediaPlayerBody`].
#[must_use]
pub fn read_sound_mode_attribute(entity: &Entity) -> Option<String> {
    read_string_attribute(entity, "sound_mode")
}

/// Read the `media_album_name` attribute as a `String` if present.
#[must_use]
pub fn read_album_attribute(entity: &Entity) -> Option<String> {
    read_string_attribute(entity, "media_album_name")
}

/// Internal: read a string-typed attribute by key.
///
/// Returns `None` when the attribute is absent or non-string. Empty
/// strings are returned as `Some("")`; the caller (more-info body / tile
/// projection) chooses whether to skip empty values.
fn read_string_attribute(entity: &Entity, key: &str) -> Option<String> {
    entity.attributes.get(key)?.as_str().map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::entity::EntityId;
    use std::sync::Arc;

    /// Construct a minimal [`Entity`] with an empty attribute map. Mirrors
    /// the helper in `src/ui/cover.rs::tests` / `src/ui/climate.rs::tests`
    /// — uses `Arc::default()` so we do not name the JSON crate (Gate 2).
    fn minimal_entity(id: &str, state: &str) -> Entity {
        Entity {
            id: EntityId::from(id),
            state: Arc::from(state),
            attributes: Arc::default(),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    /// Construct an [`Entity`] carrying attributes parsed from a YAML/JSON
    /// snippet. Uses `serde_yaml_ng` to stay inside Gate 2 — the JSON
    /// crate path must not appear textually anywhere in `src/ui/**`.
    fn entity_with_attrs(state: &str, attrs_json: &str) -> Entity {
        let map = serde_yaml_ng::from_str(attrs_json)
            .expect("test snippet must parse as a YAML/JSON map");
        Entity {
            id: EntityId::from("media_player.test"),
            state: Arc::from(state),
            attributes: Arc::new(map),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    // -----------------------------------------------------------------------
    // State mapping (TASK-109 acceptance: from_entity for state=="playing"
    // populates track info; for state=="unavailable" produces unavailable
    // sentinel)
    // -----------------------------------------------------------------------

    /// `from_entity` for `state == "playing"` populates the track info
    /// fields when the corresponding attributes are present, and sets
    /// `is_playing = true`.
    #[test]
    fn from_entity_playing_state() {
        let entity = entity_with_attrs(
            "playing",
            r#"{"media_title":"Bohemian Rhapsody","media_artist":"Queen","volume_level":0.5}"#,
        );
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.state, "playing");
        assert!(vm.is_playing, "playing state must produce is_playing=true");
        assert_eq!(vm.media_title.as_deref(), Some("Bohemian Rhapsody"));
        assert_eq!(vm.artist.as_deref(), Some("Queen"));
        assert_eq!(vm.volume_level, Some(0.5));
    }

    /// `from_entity` for `state == "paused"` keeps the track info but
    /// sets `is_playing = false`.
    #[test]
    fn from_entity_paused_state() {
        let entity = entity_with_attrs(
            "paused",
            r#"{"media_title":"Yesterday","media_artist":"The Beatles"}"#,
        );
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.state, "paused");
        assert!(!vm.is_playing, "paused state must produce is_playing=false");
        assert_eq!(vm.media_title.as_deref(), Some("Yesterday"));
        assert_eq!(vm.artist.as_deref(), Some("The Beatles"));
    }

    /// `from_entity` for `state == "idle"` produces `is_playing = false`
    /// and forwards the state string verbatim.
    #[test]
    fn from_entity_idle_state() {
        let entity = minimal_entity("media_player.tv", "idle");
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.state, "idle");
        assert!(!vm.is_playing, "idle state must produce is_playing=false");
        assert!(vm.media_title.is_none());
        assert!(vm.artist.is_none());
        assert!(vm.volume_level.is_none());
    }

    /// `from_entity` for `state == "off"` produces `is_playing = false`.
    #[test]
    fn from_entity_off_state() {
        let entity = minimal_entity("media_player.tv", "off");
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.state, "off");
        assert!(!vm.is_playing, "off state must produce is_playing=false");
    }

    /// `from_entity` for `state == "unavailable"` produces an
    /// unavailable sentinel: state forwarded verbatim, `is_playing=false`,
    /// no attribute reads (the entity was unreachable).
    #[test]
    fn from_entity_unavailable_state() {
        let entity = minimal_entity("media_player.tv", "unavailable");
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.state, "unavailable");
        assert!(
            !vm.is_playing,
            "unavailable state must produce is_playing=false"
        );
        assert!(vm.media_title.is_none());
        assert!(vm.artist.is_none());
        assert!(vm.volume_level.is_none());
    }

    /// Vendor-specific media-player states (HA allows custom states for
    /// some integrations) must not silently coerce to playing — only the
    /// canonical `"playing"` string activates the playing visual.
    #[test]
    fn from_entity_vendor_specific_state_is_not_playing() {
        let entity = minimal_entity("media_player.tv", "buffering");
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.state, "buffering");
        assert!(
            !vm.is_playing,
            "vendor state must NOT produce is_playing=true"
        );
    }

    // -----------------------------------------------------------------------
    // volume_level clamping (TASK-109 acceptance: volume_level reads
    // HA's volume_level attribute clamped to 0.0..=1.0)
    // -----------------------------------------------------------------------

    /// `volume_level` reads HA's `volume_level` attribute as `f32`.
    #[test]
    fn volume_level_reads_attribute() {
        let entity = entity_with_attrs("playing", r#"{"volume_level":0.42}"#);
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.volume_level, Some(0.42));
    }

    /// `volume_level` clamps a value above 1.0 to 1.0.
    #[test]
    fn volume_level_clamped_above_one() {
        let entity = entity_with_attrs("playing", r#"{"volume_level":1.5}"#);
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(
            vm.volume_level,
            Some(1.0),
            "above-range volume must clamp to 1.0"
        );
    }

    /// `volume_level` clamps a negative value to 0.0.
    #[test]
    fn volume_level_clamped_below_zero() {
        let entity = entity_with_attrs("playing", r#"{"volume_level":-0.25}"#);
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(
            vm.volume_level,
            Some(0.0),
            "negative volume must clamp to 0.0"
        );
    }

    /// `volume_level` exactly at 0.0 / 1.0 round-trips unchanged.
    #[test]
    fn volume_level_clamped_at_boundaries() {
        let zero = entity_with_attrs("playing", r#"{"volume_level":0.0}"#);
        assert_eq!(MediaPlayerVM::from_entity(&zero).volume_level, Some(0.0));
        let one = entity_with_attrs("playing", r#"{"volume_level":1.0}"#);
        assert_eq!(MediaPlayerVM::from_entity(&one).volume_level, Some(1.0));
    }

    /// Integer-typed `volume_level` (rare but observed on some integrations
    /// emitting `1` for full) still parses via the `as_i64` / `as_u64`
    /// fallback branches.
    #[test]
    fn volume_level_accepts_integer_value() {
        let entity = entity_with_attrs("playing", r#"{"volume_level":1}"#);
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.volume_level, Some(1.0));
    }

    /// Non-numeric `volume_level` resolves to `None`.
    #[test]
    fn volume_level_string_is_none() {
        let entity = entity_with_attrs("playing", r#"{"volume_level":"loud"}"#);
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.volume_level, None);
    }

    /// `NaN` / infinity in `volume_level` resolves to `None` (we cannot
    /// safely render a NaN slider position).
    #[test]
    fn volume_level_non_finite_is_none() {
        // `serde_yaml_ng` does not parse `NaN` from a JSON snippet, but
        // we can encode infinity via `.inf` in YAML.
        let entity = entity_with_attrs("playing", "{volume_level: .inf}");
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.volume_level, None);
    }

    /// Absent `volume_level` resolves to `None`.
    #[test]
    fn volume_level_absent_is_none() {
        let entity = minimal_entity("media_player.tv", "playing");
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.volume_level, None);
    }

    // -----------------------------------------------------------------------
    // media_title / media_artist
    // -----------------------------------------------------------------------

    #[test]
    fn media_title_reads_attribute() {
        let entity = entity_with_attrs("playing", r#"{"media_title":"Hey Jude"}"#);
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.media_title.as_deref(), Some("Hey Jude"));
    }

    #[test]
    fn media_title_absent_is_none() {
        let entity = minimal_entity("media_player.tv", "playing");
        let vm = MediaPlayerVM::from_entity(&entity);
        assert!(vm.media_title.is_none());
    }

    #[test]
    fn media_title_non_string_is_none() {
        let entity = entity_with_attrs("playing", r#"{"media_title":42}"#);
        let vm = MediaPlayerVM::from_entity(&entity);
        assert!(vm.media_title.is_none());
    }

    #[test]
    fn artist_reads_attribute() {
        let entity = entity_with_attrs("playing", r#"{"media_artist":"Adele"}"#);
        let vm = MediaPlayerVM::from_entity(&entity);
        assert_eq!(vm.artist.as_deref(), Some("Adele"));
    }

    #[test]
    fn artist_absent_is_none() {
        let entity = minimal_entity("media_player.tv", "playing");
        let vm = MediaPlayerVM::from_entity(&entity);
        assert!(vm.artist.is_none());
    }

    // -----------------------------------------------------------------------
    // Attribute helpers (used by MediaPlayerBody)
    // -----------------------------------------------------------------------

    #[test]
    fn read_source_attribute_present() {
        let entity = entity_with_attrs("playing", r#"{"source":"HDMI 1"}"#);
        assert_eq!(read_source_attribute(&entity).as_deref(), Some("HDMI 1"));
    }

    #[test]
    fn read_source_attribute_absent() {
        let entity = minimal_entity("media_player.tv", "playing");
        assert_eq!(read_source_attribute(&entity), None);
    }

    #[test]
    fn read_sound_mode_attribute_present() {
        let entity = entity_with_attrs("playing", r#"{"sound_mode":"Movie"}"#);
        assert_eq!(read_sound_mode_attribute(&entity).as_deref(), Some("Movie"));
    }

    #[test]
    fn read_sound_mode_attribute_absent() {
        let entity = minimal_entity("media_player.tv", "playing");
        assert_eq!(read_sound_mode_attribute(&entity), None);
    }

    #[test]
    fn read_album_attribute_present() {
        let entity = entity_with_attrs("playing", r#"{"media_album_name":"Abbey Road"}"#);
        assert_eq!(read_album_attribute(&entity).as_deref(), Some("Abbey Road"));
    }

    #[test]
    fn read_album_attribute_absent() {
        let entity = minimal_entity("media_player.tv", "playing");
        assert_eq!(read_album_attribute(&entity), None);
    }

    #[test]
    fn read_source_attribute_non_string_is_none() {
        let entity = entity_with_attrs("playing", r#"{"source":42}"#);
        assert_eq!(read_source_attribute(&entity), None);
    }

    // -----------------------------------------------------------------------
    // No-Vec invariant (TASK-103 / TASK-105 / TASK-107 / TASK-108 lesson)
    // -----------------------------------------------------------------------

    /// Compile-time assertion: `MediaPlayerVM` does NOT carry a `Vec`
    /// field. Because `f32: !Eq` we cannot use the alarm/camera `Eq`
    /// trick; instead we assert via `mem::size_of` that the struct shape
    /// matches the expected scalar layout. A future edit that adds a
    /// `Vec<…>` field would push the size past the budget below.
    ///
    /// On 64-bit Linux the layout is approximately:
    ///   - `String` state (24)
    ///   - `Option<String>` media_title (24 + tag, 24)
    ///   - `Option<String>` artist (24)
    ///   - `Option<f32>` volume_level (8)
    ///   - `bool` is_playing (1, padded)
    /// Total fits comfortably in 96 bytes; a `Vec<String>` field would
    /// add at least 24 bytes.
    #[test]
    fn media_player_vm_remains_lean() {
        assert!(
            std::mem::size_of::<MediaPlayerVM>() <= 104,
            "MediaPlayerVM has grown past the lean-shape budget; \
             did someone add a Vec field?"
        );
    }
}
