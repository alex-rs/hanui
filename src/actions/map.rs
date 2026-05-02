//! `WidgetActionMap` — the dispatcher's lookup table from `WidgetId` to
//! `(entity_id, tap, hold, double_tap)`.
//!
//! Phase 3 ships this module with an in-code builder driven from
//! [`crate::dashboard::schema::Dashboard`] (the canonical Phase 4 typed
//! schema; TASK-082 migrated from the retired `view_spec::Dashboard`).
//! Phase 4 wires the YAML loader output here without changing the lookup API
//! or the dispatcher (locked_decisions.phase4_forward_compat).
//!
//! # Why each entry carries `entity_id`
//!
//! Per `docs/plans/2026-04-28-phase-3-actions.md`
//! `locked_decisions.more_info_modal`, the WidgetActionMap entry is the
//! single source of truth for the entity associated with a widget at
//! dispatch time. The dispatcher reads `entity_id` from the entry for both
//! WS dispatch (Toggle / CallService default-target) and more-info modal
//! routing — it never performs a second lookup against the dashboard schema
//! while resolving an action (Risk #12).
//!
//! Widgets that have no associated HA entity are skipped by
//! [`WidgetActionMap::from_dashboard`]: there is nothing for the dispatcher
//! to route on. The Phase 4 YAML loader enforces the same invariant at
//! parse time.
//!
//! # Widget-id collision policy
//!
//! If two widgets share the same `id` (intentional or accidental), the
//! **last** occurrence in document order wins. `HashMap`'s natural `insert`
//! behaviour provides this: the earlier entry is silently overwritten.
//! A `tracing::warn!` is emitted at population time so operators can detect
//! collisions from logs. Phase 5 may harden this to a load-time error if
//! required.

use std::collections::HashMap;
use std::fmt;

use smol_str::SmolStr;

use crate::actions::Action;
use crate::dashboard::schema::Dashboard;
use crate::ha::entity::EntityId;

// ---------------------------------------------------------------------------
// WidgetId
// ---------------------------------------------------------------------------

/// Newtype wrapper around the user-authored widget id (`Widget.id`).
///
/// Kept structurally identical to [`EntityId`]: a [`SmolStr`] inside, so
/// short ids (the common case) avoid heap allocation. The dispatcher hashes
/// `WidgetId` directly without going through the `String` representation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WidgetId(SmolStr);

impl WidgetId {
    /// Returns the string slice of this widget id.
    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for WidgetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl From<&str> for WidgetId {
    fn from(s: &str) -> Self {
        WidgetId(SmolStr::new(s))
    }
}

impl From<String> for WidgetId {
    fn from(s: String) -> Self {
        WidgetId(SmolStr::new(s))
    }
}

impl AsRef<str> for WidgetId {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

// ---------------------------------------------------------------------------
// WidgetActionEntry
// ---------------------------------------------------------------------------

/// One row of the [`WidgetActionMap`].
///
/// Carries the widget's associated entity id alongside the three gesture
/// actions. Per `locked_decisions.more_info_modal`, `entity_id` is present
/// even when none of the action variants references it — the dispatcher
/// reads it directly for `MoreInfo` modal routing and for Toggle's domain
/// resolution, so a single source of truth lives here.
///
/// Action fields use `Action::None` as the absent sentinel, matching the
/// canonical schema. A widget with no `tap_action` configured therefore has
/// `tap: Action::None` rather than a separate `Option` indirection — the
/// dispatcher's match handles `Action::None` as a no-op.
#[derive(Debug, Clone, PartialEq)]
pub struct WidgetActionEntry {
    /// The HA entity id this widget is bound to. Required for both
    /// dispatch (Toggle / CallService default-target) and `MoreInfo`
    /// routing.
    pub entity_id: EntityId,
    /// Action fired on a single tap.
    pub tap: Action,
    /// Action fired on a long press.
    pub hold: Action,
    /// Action fired on a double tap.
    pub double_tap: Action,
}

// ---------------------------------------------------------------------------
// WidgetActionMap
// ---------------------------------------------------------------------------

/// Lookup table from [`WidgetId`] to [`WidgetActionEntry`].
///
/// Built from a [`Dashboard`] via [`WidgetActionMap::from_dashboard`].
/// The constructor walks every `view → section → widget` triple in the
/// dashboard; Phase 3 used `fixture_dashboard()` as the source, Phase 4
/// uses the YAML-loaded `Dashboard` returned by `loader::load`. The read
/// API (lookup, len, is_empty) is unchanged — only the data source changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WidgetActionMap {
    inner: HashMap<WidgetId, WidgetActionEntry>,
}

impl WidgetActionMap {
    /// Construct an empty map.
    #[must_use]
    pub fn new() -> Self {
        WidgetActionMap {
            inner: HashMap::new(),
        }
    }

    /// Build a [`WidgetActionMap`] from a [`Dashboard`].
    ///
    /// Walks every view → section → widget. For each widget that has an
    /// `entity` set, inserts an entry keyed by `widget.id` carrying:
    ///
    /// * `entity_id`         — `widget.entity` (parsed via [`EntityId::from`]).
    /// * `tap` / `hold` / `double_tap` — the corresponding `Option<Action>`
    ///   field, defaulting to [`Action::None`] when the field is `None`.
    ///
    /// Widgets without an `entity` are skipped: they have nothing for the
    /// dispatcher to route on. The YAML loader refuses configs that pair a
    /// non-`None` action with a missing entity.
    ///
    /// If the dashboard contains duplicate widget ids across views or
    /// sections the **last** occurrence wins (documented gotcha; see
    /// module-level comment on the collision policy). A `tracing::warn!` is
    /// emitted so operators can detect collisions from logs.
    ///
    /// All callers — including the `--fixture` path in `src/lib.rs` and the
    /// Phase 4 live-HA path — call this function; only the `Dashboard` they
    /// pass in differs (fixture vs. YAML-loaded).
    #[must_use]
    pub fn from_dashboard(spec: &Dashboard) -> Self {
        let mut map = HashMap::new();
        for view in &spec.views {
            for section in &view.sections {
                for widget in &section.widgets {
                    let Some(entity_str) = widget.entity.as_deref() else {
                        continue;
                    };
                    let widget_id = WidgetId::from(widget.id.as_str());
                    if map.contains_key(&widget_id) {
                        tracing::warn!(
                            widget_id = %widget_id,
                            "duplicate widget id in dashboard — last occurrence wins"
                        );
                    }
                    let entry = WidgetActionEntry {
                        entity_id: EntityId::from(entity_str),
                        tap: widget.tap_action.clone().unwrap_or(Action::None),
                        hold: widget.hold_action.clone().unwrap_or(Action::None),
                        double_tap: widget.double_tap_action.clone().unwrap_or(Action::None),
                    };
                    map.insert(widget_id, entry);
                }
            }
        }
        WidgetActionMap { inner: map }
    }

    /// Look up the action entry for a widget.
    ///
    /// Returns `None` when the widget has no entry — typically because it
    /// has no associated HA entity, or because the widget id was never
    /// registered. The dispatcher treats `None` as
    /// [`crate::actions::dispatcher::DispatchError::UnknownWidget`].
    #[must_use]
    pub fn lookup(&self, widget_id: &WidgetId) -> Option<&WidgetActionEntry> {
        self.inner.get(widget_id)
    }

    /// Return the number of registered widgets. Test/debug helper.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the map has zero entries. Test/debug helper.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Insert (or replace) an entry directly.
    ///
    /// Used by tests that build minimal fixtures without going through a
    /// full `Dashboard`. Production callers go via
    /// [`WidgetActionMap::from_dashboard`].
    pub fn insert(&mut self, widget_id: WidgetId, entry: WidgetActionEntry) {
        self.inner.insert(widget_id, entry);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::fixture::fixture_dashboard;

    // -----------------------------------------------------------------------
    // WidgetId
    // -----------------------------------------------------------------------

    #[test]
    fn widget_id_round_trips_str_form() {
        let id = WidgetId::from("kitchen_light");
        assert_eq!(id.as_str(), "kitchen_light");
        assert_eq!(id.to_string(), "kitchen_light");
        let s: &str = id.as_ref();
        assert_eq!(s, "kitchen_light");
    }

    #[test]
    fn widget_id_from_owned_string_works() {
        let owned = String::from("entity_tile_living");
        let id = WidgetId::from(owned);
        assert_eq!(id.as_str(), "entity_tile_living");
    }

    #[test]
    fn widget_ids_compare_by_value() {
        let a = WidgetId::from("kitchen_light");
        let b = WidgetId::from("kitchen_light");
        let c = WidgetId::from("hallway_temperature");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // -----------------------------------------------------------------------
    // WidgetActionMap basics
    // -----------------------------------------------------------------------

    #[test]
    fn new_map_is_empty() {
        let map = WidgetActionMap::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
        assert!(map.lookup(&WidgetId::from("anything")).is_none());
    }

    #[test]
    fn insert_then_lookup_returns_inserted_entry() {
        let mut map = WidgetActionMap::new();
        let entry = WidgetActionEntry {
            entity_id: EntityId::from("light.kitchen"),
            tap: Action::Toggle,
            hold: Action::MoreInfo,
            double_tap: Action::None,
        };
        map.insert(WidgetId::from("kitchen_light"), entry.clone());
        let looked = map
            .lookup(&WidgetId::from("kitchen_light"))
            .expect("inserted entry must be visible");
        assert_eq!(looked, &entry);
        assert_eq!(map.len(), 1);
    }

    // -----------------------------------------------------------------------
    // entity_id presence on every entry — locked_decisions.more_info_modal
    // -----------------------------------------------------------------------

    #[test]
    fn every_entry_built_from_dashboard_carries_entity_id() {
        // This is the Phase 3 invariant from
        // `locked_decisions.more_info_modal` and acceptance criterion.
        // No entry is allowed to omit `entity_id`.
        let dashboard = fixture_dashboard();
        let map = WidgetActionMap::from_dashboard(&dashboard);
        assert!(
            !map.is_empty(),
            "fixture_dashboard must produce at least one entry"
        );
        for (widget_id, entry) in &map.inner {
            // entity_id must be a non-empty string — entry construction
            // skips widgets where `entity` is None, so reaching this loop
            // already proves entity_id was populated. Defence-in-depth:
            // assert it's non-empty too.
            assert!(
                !entry.entity_id.as_str().is_empty(),
                "entity_id must be non-empty for widget {widget_id}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // from_dashboard round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn from_dashboard_populates_kitchen_light_with_toggle_and_more_info() {
        let dashboard = fixture_dashboard();
        let map = WidgetActionMap::from_dashboard(&dashboard);
        let entry = map
            .lookup(&WidgetId::from("kitchen_light"))
            .expect("kitchen_light fixture entry must be present");
        assert_eq!(entry.entity_id, EntityId::from("light.kitchen"));
        assert_eq!(entry.tap, Action::Toggle);
        assert_eq!(entry.hold, Action::MoreInfo);
        assert_eq!(entry.double_tap, Action::None);
    }

    #[test]
    fn from_dashboard_defaults_missing_actions_to_action_none() {
        // hallway_temperature in the fixture has all three actions set to
        // None; the map entry must reflect Action::None for each.
        let dashboard = fixture_dashboard();
        let map = WidgetActionMap::from_dashboard(&dashboard);
        let entry = map
            .lookup(&WidgetId::from("hallway_temperature"))
            .expect("hallway_temperature fixture entry must be present");
        assert_eq!(
            entry.entity_id,
            EntityId::from("sensor.hallway_temperature")
        );
        assert_eq!(entry.tap, Action::None);
        assert_eq!(entry.hold, Action::None);
        assert_eq!(entry.double_tap, Action::None);
    }

    #[test]
    fn from_dashboard_preserves_navigate_action_on_double_tap() {
        // living_room_entity in the fixture has a double_tap_action of
        // Navigate { view_id: "home" }. The map must round-trip the
        // payload without re-encoding the variant.
        let dashboard = fixture_dashboard();
        let map = WidgetActionMap::from_dashboard(&dashboard);
        let entry = map
            .lookup(&WidgetId::from("living_room_entity"))
            .expect("living_room_entity fixture entry must be present");
        assert_eq!(entry.entity_id, EntityId::from("switch.outlet_1"));
        assert_eq!(entry.tap, Action::Toggle);
        assert_eq!(entry.hold, Action::MoreInfo);
        assert_eq!(
            entry.double_tap,
            Action::Navigate {
                view_id: "home".to_owned(),
            }
        );
    }

    #[test]
    fn from_dashboard_skips_widgets_without_entity() {
        // Build a widget with `entity: None` and assert it is omitted.
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_owned(),
            views: vec![View {
                id: "home".to_owned(),
                title: "Home".to_owned(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "overview".to_owned(),
                    title: "Overview".to_owned(),
                    widgets: vec![Widget {
                        id: "no_entity_widget".to_owned(),
                        widget_type: WidgetKind::EntityTile,
                        entity: None,
                        entities: vec![],
                        name: None,
                        icon: None,
                        tap_action: Some(Action::Toggle),
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 1,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: None,
                        visibility: "always".to_string(),
                    }],
                }],
            }],
        };
        let map = WidgetActionMap::from_dashboard(&dashboard);
        assert!(
            map.lookup(&WidgetId::from("no_entity_widget")).is_none(),
            "widget without `entity` must be omitted from the map"
        );
        assert!(
            map.is_empty(),
            "no entries should be produced when only entity-less widgets exist"
        );
    }

    #[test]
    fn from_dashboard_traverses_multiple_sections_and_views() {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        // Two views, each with one widget, both bound to entities.
        fn make_widget(id: &str, entity: &str) -> Widget {
            Widget {
                id: id.to_owned(),
                widget_type: WidgetKind::EntityTile,
                entity: Some(entity.to_owned()),
                entities: vec![],
                name: None,
                icon: None,
                tap_action: Some(Action::Toggle),
                hold_action: None,
                double_tap_action: None,
                layout: WidgetLayout {
                    preferred_columns: 1,
                    preferred_rows: 1,
                },
                options: None,
                placement: None,
                visibility: "always".to_string(),
            }
        }
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_owned(),
            views: vec![
                View {
                    id: "home".to_owned(),
                    title: "Home".to_owned(),
                    layout: Layout::Sections,
                    sections: vec![Section {
                        grid: crate::dashboard::schema::SectionGrid::default(),
                        id: "main".to_owned(),
                        title: "Main".to_owned(),
                        widgets: vec![make_widget("alpha", "switch.alpha")],
                    }],
                },
                View {
                    id: "office".to_owned(),
                    title: "Office".to_owned(),
                    layout: Layout::Sections,
                    sections: vec![Section {
                        grid: crate::dashboard::schema::SectionGrid::default(),
                        id: "desk".to_owned(),
                        title: "Desk".to_owned(),
                        widgets: vec![make_widget("beta", "light.beta")],
                    }],
                },
            ],
        };
        let map = WidgetActionMap::from_dashboard(&dashboard);
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.lookup(&WidgetId::from("alpha")).unwrap().entity_id,
            EntityId::from("switch.alpha")
        );
        assert_eq!(
            map.lookup(&WidgetId::from("beta")).unwrap().entity_id,
            EntityId::from("light.beta")
        );
    }

    #[test]
    fn from_dashboard_duplicate_widget_id_last_occurrence_wins() {
        // Exercises the `tracing::warn!` collision branch: two widgets with the
        // same id; the second one's `tap_action` must overwrite the first.
        // This pins the documented "last occurrence wins" policy and covers
        // the warn-emit code path that would otherwise be uncovered.
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };
        fn make_widget_with_action(id: &str, entity: &str, action: Action) -> Widget {
            Widget {
                id: id.to_owned(),
                widget_type: WidgetKind::EntityTile,
                entity: Some(entity.to_owned()),
                entities: vec![],
                name: None,
                icon: None,
                tap_action: Some(action),
                hold_action: None,
                double_tap_action: None,
                layout: WidgetLayout {
                    preferred_columns: 1,
                    preferred_rows: 1,
                },
                options: None,
                placement: None,
                visibility: "always".to_string(),
            }
        }
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_owned(),
            views: vec![View {
                id: "home".to_owned(),
                title: "Home".to_owned(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "main".to_owned(),
                    title: "Main".to_owned(),
                    widgets: vec![
                        // First occurrence: tap → Toggle.
                        make_widget_with_action("dup_id", "switch.alpha", Action::Toggle),
                        // Second occurrence (collision): tap → MoreInfo. Overwrites the first.
                        make_widget_with_action("dup_id", "switch.beta", Action::MoreInfo),
                    ],
                }],
            }],
        };
        let map = WidgetActionMap::from_dashboard(&dashboard);
        // Only one entry — second occurrence won.
        assert_eq!(map.len(), 1);
        let entry = map
            .lookup(&WidgetId::from("dup_id"))
            .expect("dup_id present");
        assert_eq!(entry.entity_id, EntityId::from("switch.beta"));
        assert_eq!(entry.tap, Action::MoreInfo);
    }

    #[test]
    fn lookup_unknown_widget_returns_none() {
        let dashboard = fixture_dashboard();
        let map = WidgetActionMap::from_dashboard(&dashboard);
        assert!(map.lookup(&WidgetId::from("does_not_exist")).is_none());
    }

    // -----------------------------------------------------------------------
    // TASK-088 acceptance tests
    // -----------------------------------------------------------------------

    /// Acceptance: a Dashboard with zero widgets produces an empty
    /// WidgetActionMap (no panic, no error).
    #[test]
    fn populate_from_empty_dashboard_yields_empty_map() {
        use crate::dashboard::schema::{Dashboard, Layout, ProfileKey, Section, View};
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_owned(),
            views: vec![View {
                id: "home".to_owned(),
                title: "Home".to_owned(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "s1".to_owned(),
                    title: "Empty".to_owned(),
                    widgets: vec![],
                }],
            }],
        };
        let map = WidgetActionMap::from_dashboard(&dashboard);
        assert!(
            map.is_empty(),
            "Dashboard with zero widgets must produce an empty WidgetActionMap"
        );
        assert_eq!(map.len(), 0);
    }

    /// Acceptance: a Dashboard with one widget carrying all three action kinds
    /// round-trips every action variant correctly.
    ///
    /// Simulates what the YAML loader would produce for:
    /// ```yaml
    /// tap_action:
    ///   action: call-service
    ///   domain: light
    ///   service: turn_on
    /// hold_action:
    ///   action: more-info
    /// double_tap_action:
    ///   action: navigate
    ///   view-id: settings
    /// ```
    #[test]
    fn tap_hold_double_tap_propagate_from_yaml() {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };

        let tap = Action::CallService {
            domain: "light".to_owned(),
            service: "turn_on".to_owned(),
            target: Some("light.kitchen".to_owned()),
            data: None,
        };
        let hold = Action::MoreInfo;
        let double_tap = Action::Navigate {
            view_id: "settings".to_owned(),
        };

        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_owned(),
            views: vec![View {
                id: "home".to_owned(),
                title: "Home".to_owned(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "lights".to_owned(),
                    title: "Lights".to_owned(),
                    widgets: vec![Widget {
                        id: "kitchen_light".to_owned(),
                        widget_type: WidgetKind::LightTile,
                        entity: Some("light.kitchen".to_owned()),
                        entities: vec![],
                        name: None,
                        icon: None,
                        tap_action: Some(tap.clone()),
                        hold_action: Some(hold.clone()),
                        double_tap_action: Some(double_tap.clone()),
                        layout: WidgetLayout {
                            preferred_columns: 2,
                            preferred_rows: 2,
                        },
                        options: None,
                        placement: None,
                        visibility: "always".to_string(),
                    }],
                }],
            }],
        };

        let map = WidgetActionMap::from_dashboard(&dashboard);
        assert_eq!(map.len(), 1, "must produce exactly one entry");

        let entry = map
            .lookup(&WidgetId::from("kitchen_light"))
            .expect("kitchen_light entry must be present");

        assert_eq!(
            entry.entity_id,
            EntityId::from("light.kitchen"),
            "entity_id must match the widget's entity field"
        );
        assert_eq!(
            entry.tap, tap,
            "tap action must round-trip: CallService(light.turn_on)"
        );
        assert_eq!(entry.hold, hold, "hold action must round-trip: MoreInfo");
        assert_eq!(
            entry.double_tap, double_tap,
            "double_tap action must round-trip: Navigate(settings)"
        );
    }

    /// Acceptance: a widget with NO explicit tap_action produces
    /// `tap == Action::None`.
    #[test]
    fn populate_from_loaded_dashboard() {
        use crate::dashboard::schema::{
            Dashboard, Layout, ProfileKey, Section, View, Widget, WidgetKind, WidgetLayout,
        };

        // Widget with entity but no actions set — models the YAML-loaded
        // case where all three action fields are absent from the YAML.
        let dashboard = Dashboard {
            call_service_allowlist: std::sync::Arc::new(std::collections::BTreeSet::new()),
            dep_index: std::sync::Arc::default(),
            version: 1,
            device_profile: ProfileKey::Rpi4,
            home_assistant: None,
            theme: None,
            default_view: "home".to_owned(),
            views: vec![View {
                id: "home".to_owned(),
                title: "Home".to_owned(),
                layout: Layout::Sections,
                sections: vec![Section {
                    grid: crate::dashboard::schema::SectionGrid::default(),
                    id: "sensors".to_owned(),
                    title: "Sensors".to_owned(),
                    widgets: vec![Widget {
                        id: "temp_sensor".to_owned(),
                        widget_type: WidgetKind::SensorTile,
                        entity: Some("sensor.temp".to_owned()),
                        entities: vec![],
                        name: None,
                        icon: None,
                        // No actions set — simulates a YAML config that omits
                        // tap_action / hold_action / double_tap_action entirely.
                        tap_action: None,
                        hold_action: None,
                        double_tap_action: None,
                        layout: WidgetLayout {
                            preferred_columns: 2,
                            preferred_rows: 1,
                        },
                        options: None,
                        placement: None,
                        visibility: "always".to_string(),
                    }],
                }],
            }],
        };

        let map = WidgetActionMap::from_dashboard(&dashboard);
        assert_eq!(map.len(), 1, "one widget with entity must yield one entry");

        let entry = map
            .lookup(&WidgetId::from("temp_sensor"))
            .expect("temp_sensor entry must be present");

        assert_eq!(
            entry.entity_id,
            EntityId::from("sensor.temp"),
            "entity_id must match YAML entity field"
        );
        // All three actions absent from YAML → Action::None in the map.
        assert_eq!(
            entry.tap,
            Action::None,
            "missing tap_action must produce Action::None"
        );
        assert_eq!(
            entry.hold,
            Action::None,
            "missing hold_action must produce Action::None"
        );
        assert_eq!(
            entry.double_tap,
            Action::None,
            "missing double_tap_action must produce Action::None"
        );
    }

    /// Mechanical gate (Risk #8): assert that the production function
    /// `from_dashboard` in this file does NOT call `default_dashboard()`.
    ///
    /// This test reads the source of `src/actions/map.rs` at test time and
    /// asserts the forbidden string is absent outside the `mod tests` block.
    /// Any future regression where in-code fixture construction sneaks into
    /// the production path is caught here before reaching CI.
    #[test]
    fn default_dashboard_not_called_in_production_paths() {
        let source = std::fs::read_to_string("src/actions/map.rs")
            .expect("src/actions/map.rs must be readable from cargo's cwd");

        // Split the file at the `mod tests` boundary. Everything before
        // that marker is production code; everything after is test-only.
        let mod_tests_marker = "#[cfg(test)]\nmod tests {";
        let production_section = source.split(mod_tests_marker).next().unwrap_or(&source);

        assert!(
            !production_section.contains("default_dashboard"),
            "`default_dashboard` must not appear in the production code path of \
             src/actions/map.rs (Risk #8). It was found before `mod tests {{`."
        );
    }
}
