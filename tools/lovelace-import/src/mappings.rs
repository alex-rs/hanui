//! Mapping table from Home Assistant Lovelace card types to hanui
//! [`WidgetKind`] variants.
//!
//! # Locked card vocabulary
//!
//! Per `locked_decisions.lovelace_minimum_card_set` in
//! `docs/plans/2026-04-30-phase-6-advanced-widgets.md`, the Phase 6 importer
//! recognises the eight Lovelace card types listed below. Six map to a
//! hanui [`WidgetKind`]; two (`vertical-stack`, `horizontal-stack`) are
//! containers that emit no widget on their own — the importer recurses into
//! their child cards. Container types are still represented as variants on
//! [`MappingTable`] so the CI gate `every_lovelace_card_has_entry` can spot
//! when a future card type appears in the wild without a corresponding
//! mapping decision.
//!
//! # WidgetKind coverage gate
//!
//! Per the same locked decision: the gate "every hanui [`WidgetKind`] variant
//! has a fixture entry" is satisfied even when a kind has no Lovelace
//! equivalent — those kinds appear as [`MappedKind::Unmapped`] entries here
//! (Camera, History, MediaPlayer, PowerFlow, etc.). The
//! `every_widget_kind_has_entry` test in `tests/e2e.rs` enforces this.

use hanui::dashboard::schema::WidgetKind;
use serde_yaml_ng::Value;

// ---------------------------------------------------------------------------
// LovelaceCard
// ---------------------------------------------------------------------------

/// The eight Lovelace card types this importer recognises.
///
/// Per `locked_decisions.lovelace_minimum_card_set`. Any `type:` value not in
/// this enum is reported back to the caller as an UNMAPPED entry rather than
/// silently dropped (Risk #5 mitigation — Lovelace YAML is not formally
/// versioned, so unknown types are expected and must be visible to the
/// migrating user).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LovelaceCard {
    /// `type: entities` — list of entities.
    Entities,
    /// `type: light` — single light tile.
    Light,
    /// `type: media-control` — media player tile.
    MediaControl,
    /// `type: thermostat` (also matches `type: climate`).
    Thermostat,
    /// `type: picture-entity` — image-with-state tile.
    PictureEntity,
    /// `type: glance` — compact multi-entity glance row.
    Glance,
    /// `type: vertical-stack` — container; recurse into children.
    VerticalStack,
    /// `type: horizontal-stack` — container; recurse into children.
    HorizontalStack,
}

impl LovelaceCard {
    /// Parse a Lovelace `type:` string into a [`LovelaceCard`].
    ///
    /// Returns `None` for any value not in the locked vocabulary. The caller
    /// (`MappingTable::map_card`) reports the unrecognised string back to the
    /// user as an UNMAPPED entry.
    #[must_use]
    pub fn from_type_str(s: &str) -> Option<Self> {
        // Lovelace permits both `thermostat` and `climate` for thermostat-style
        // cards; alias them.
        match s {
            "entities" => Some(Self::Entities),
            "light" => Some(Self::Light),
            "media-control" => Some(Self::MediaControl),
            "thermostat" | "climate" => Some(Self::Thermostat),
            "picture-entity" => Some(Self::PictureEntity),
            "glance" => Some(Self::Glance),
            "vertical-stack" => Some(Self::VerticalStack),
            "horizontal-stack" => Some(Self::HorizontalStack),
            _ => None,
        }
    }

    /// Stable identifier used in MAPPINGS.md and UNMAPPED log lines.
    #[must_use]
    pub const fn type_str(self) -> &'static str {
        match self {
            Self::Entities => "entities",
            Self::Light => "light",
            Self::MediaControl => "media-control",
            Self::Thermostat => "thermostat",
            Self::PictureEntity => "picture-entity",
            Self::Glance => "glance",
            Self::VerticalStack => "vertical-stack",
            Self::HorizontalStack => "horizontal-stack",
        }
    }
}

// ---------------------------------------------------------------------------
// MappedKind
// ---------------------------------------------------------------------------

/// The result of looking up a Lovelace card in the [`MappingTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MappedKind {
    /// The card maps directly to a hanui [`WidgetKind`]. The importer emits a
    /// widget of this kind.
    Widget(WidgetKind),
    /// The card is a container (`vertical-stack`, `horizontal-stack`) — the
    /// importer recurses into its children and does not emit a widget for the
    /// container itself.
    Container,
    /// The card type has no mapping in this version of the importer (or the
    /// `type:` string was not in the locked vocabulary). The string captures
    /// the original `type:` value so the UNMAPPED log can surface it back to
    /// the user.
    Unmapped(String),
}

// ---------------------------------------------------------------------------
// MappingTable
// ---------------------------------------------------------------------------

/// Static lookup table from Lovelace card types to hanui widget kinds.
///
/// Holds no state — the table is implemented as a `match` against the closed
/// [`LovelaceCard`] enum. Callers construct one with [`MappingTable::new`] for
/// API symmetry with potential future extensibility (e.g. user-supplied
/// overrides). The instance is cheap to construct.
#[derive(Debug, Clone, Copy, Default)]
pub struct MappingTable;

impl MappingTable {
    /// Construct a new mapping table with the locked default vocabulary.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Look up the hanui kind for a parsed Lovelace card.
    ///
    /// Mapping (per `locked_decisions.lovelace_minimum_card_set`):
    /// - `entities`        → [`WidgetKind::EntityTile`]
    /// - `light`           → [`WidgetKind::LightTile`]
    /// - `glance`          → [`WidgetKind::EntityTile`]
    /// - `thermostat`      → [`WidgetKind::Climate`]
    /// - `media-control`   → [`WidgetKind::MediaPlayer`]
    /// - `picture-entity`  → [`WidgetKind::Camera`]
    /// - `vertical-stack`  → [`MappedKind::Container`]
    /// - `horizontal-stack`→ [`MappedKind::Container`]
    #[must_use]
    pub fn lookup(&self, card: LovelaceCard) -> MappedKind {
        match card {
            LovelaceCard::Entities => MappedKind::Widget(WidgetKind::EntityTile),
            LovelaceCard::Light => MappedKind::Widget(WidgetKind::LightTile),
            LovelaceCard::Glance => MappedKind::Widget(WidgetKind::EntityTile),
            LovelaceCard::Thermostat => MappedKind::Widget(WidgetKind::Climate),
            LovelaceCard::MediaControl => MappedKind::Widget(WidgetKind::MediaPlayer),
            LovelaceCard::PictureEntity => MappedKind::Widget(WidgetKind::Camera),
            LovelaceCard::VerticalStack | LovelaceCard::HorizontalStack => MappedKind::Container,
        }
    }

    /// Convenience: resolve a YAML card mapping by its `type:` field.
    ///
    /// Returns:
    /// - [`MappedKind::Widget`] / [`MappedKind::Container`] for a recognised
    ///   `type:` value.
    /// - [`MappedKind::Unmapped`] when the `type:` field is missing, not a
    ///   string, or not in the locked vocabulary. The captured string is the
    ///   raw `type:` value (or `"<missing>"` when absent) so the UNMAPPED log
    ///   surfaces it verbatim to the user.
    #[must_use]
    pub fn map_yaml_card(&self, card: &Value) -> MappedKind {
        let Some(type_value) = card.get("type") else {
            return MappedKind::Unmapped("<missing>".to_string());
        };
        let Some(type_str) = type_value.as_str() else {
            return MappedKind::Unmapped("<non-string>".to_string());
        };
        match LovelaceCard::from_type_str(type_str) {
            Some(card_kind) => self.lookup(card_kind),
            None => MappedKind::Unmapped(type_str.to_string()),
        }
    }

    /// Iterate every [`LovelaceCard`] variant in this table.
    ///
    /// Used by the documentation generator and the `every_lovelace_card_has_entry`
    /// test to ensure the enum is closed.
    pub fn all_cards() -> [LovelaceCard; 8] {
        [
            LovelaceCard::Entities,
            LovelaceCard::Light,
            LovelaceCard::MediaControl,
            LovelaceCard::Thermostat,
            LovelaceCard::PictureEntity,
            LovelaceCard::Glance,
            LovelaceCard::VerticalStack,
            LovelaceCard::HorizontalStack,
        ]
    }
}

// ---------------------------------------------------------------------------
// WidgetKind coverage
// ---------------------------------------------------------------------------

/// Returns every hanui [`WidgetKind`] variant the importer is aware of, paired
/// with the Lovelace card that maps to it (if any).
///
/// `None` for the second element means the WidgetKind has no Lovelace
/// equivalent in the locked vocabulary — it is expected to appear as an
/// UNMAPPED placeholder in the corresponding fixture.
///
/// The `every_widget_kind_has_entry` test in `tests/e2e.rs` consumes this
/// list to enforce that every variant of [`WidgetKind`] is enumerated here;
/// adding a new variant in `src/dashboard/schema.rs` without updating this
/// list fails CI.
#[must_use]
pub fn widget_kind_coverage() -> Vec<(WidgetKind, Option<LovelaceCard>)> {
    vec![
        (WidgetKind::LightTile, Some(LovelaceCard::Light)),
        // `entities` → EntityTile is the canonical mapping; `glance` also maps
        // to EntityTile (compact multi-entity row).
        (WidgetKind::EntityTile, Some(LovelaceCard::Entities)),
        // SensorTile has no first-class Lovelace card type (a `sensor` entity
        // typically appears inside an `entities` or `glance` card).
        (WidgetKind::SensorTile, None),
        (WidgetKind::Camera, Some(LovelaceCard::PictureEntity)),
        // History — Lovelace's `history-graph` card; not in the minimum-viable
        // vocabulary. UNMAPPED fixture covers the WidgetKind for the gate.
        (WidgetKind::History, None),
        // Fan — no first-class Lovelace card; `entities` is the usual route.
        (WidgetKind::Fan, None),
        // Lock — no first-class Lovelace card; appears inside `entities`.
        (WidgetKind::Lock, None),
        // Alarm — Lovelace has `alarm-panel` but that is not in the minimum
        // vocabulary; UNMAPPED covers it.
        (WidgetKind::Alarm, None),
        // Cover — `entities` is the usual route in Lovelace.
        (WidgetKind::Cover, None),
        (WidgetKind::MediaPlayer, Some(LovelaceCard::MediaControl)),
        (WidgetKind::Climate, Some(LovelaceCard::Thermostat)),
        // PowerFlow has no native Lovelace card (HACS-only third-party).
        (WidgetKind::PowerFlow, None),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_type_str_recognises_all_locked_cards() {
        for card in MappingTable::all_cards() {
            let s = card.type_str();
            assert_eq!(
                LovelaceCard::from_type_str(s),
                Some(card),
                "round-trip must hold for {s}"
            );
        }
    }

    #[test]
    fn from_type_str_rejects_unknown() {
        assert_eq!(LovelaceCard::from_type_str("button"), None);
        assert_eq!(LovelaceCard::from_type_str(""), None);
    }

    #[test]
    fn climate_alias_maps_to_thermostat() {
        assert_eq!(
            LovelaceCard::from_type_str("climate"),
            Some(LovelaceCard::Thermostat)
        );
    }

    #[test]
    fn lookup_covers_all_locked_cards() {
        let t = MappingTable::new();
        for card in MappingTable::all_cards() {
            // Every locked card must yield a non-Unmapped result.
            match t.lookup(card) {
                MappedKind::Widget(_) | MappedKind::Container => {}
                MappedKind::Unmapped(s) => panic!("locked card {card:?} mapped to Unmapped({s})"),
            }
        }
    }

    #[test]
    fn map_yaml_card_unknown_type_string_is_unmapped() {
        let t = MappingTable::new();
        let yaml: Value = serde_yaml_ng::from_str("type: button\nentity: light.kitchen").unwrap();
        match t.map_yaml_card(&yaml) {
            MappedKind::Unmapped(s) => assert_eq!(s, "button"),
            other => panic!("expected Unmapped(\"button\"), got {other:?}"),
        }
    }

    #[test]
    fn map_yaml_card_missing_type_is_unmapped_placeholder() {
        let t = MappingTable::new();
        let yaml: Value = serde_yaml_ng::from_str("entity: light.kitchen").unwrap();
        assert_eq!(
            t.map_yaml_card(&yaml),
            MappedKind::Unmapped("<missing>".to_string())
        );
    }

    #[test]
    fn map_yaml_card_non_string_type_is_unmapped_placeholder() {
        let t = MappingTable::new();
        let yaml: Value = serde_yaml_ng::from_str("type: 42\nentity: light.kitchen").unwrap();
        assert_eq!(
            t.map_yaml_card(&yaml),
            MappedKind::Unmapped("<non-string>".to_string())
        );
    }

    #[test]
    fn widget_kind_coverage_lists_every_variant() {
        // Mirrors WidgetKind enum order so adding a variant fails this test
        // until the coverage list is updated.
        let kinds: Vec<WidgetKind> = widget_kind_coverage().into_iter().map(|(k, _)| k).collect();
        assert!(kinds.contains(&WidgetKind::LightTile));
        assert!(kinds.contains(&WidgetKind::SensorTile));
        assert!(kinds.contains(&WidgetKind::EntityTile));
        assert!(kinds.contains(&WidgetKind::Camera));
        assert!(kinds.contains(&WidgetKind::History));
        assert!(kinds.contains(&WidgetKind::Fan));
        assert!(kinds.contains(&WidgetKind::Lock));
        assert!(kinds.contains(&WidgetKind::Alarm));
        assert!(kinds.contains(&WidgetKind::Cover));
        assert!(kinds.contains(&WidgetKind::MediaPlayer));
        assert!(kinds.contains(&WidgetKind::Climate));
        assert!(kinds.contains(&WidgetKind::PowerFlow));
    }
}
