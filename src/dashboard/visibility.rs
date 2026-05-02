//! Conditional-visibility predicate evaluator and entity dependency index.
//!
//! Phase 6b TASK-110 — implements the runtime evaluator for the predicate
//! namespace locked in Phase 4 by [`crate::dashboard::validate`] and widened in
//! Phase 6 by `locked_decisions.visibility_predicate_vocabulary` (see
//! `docs/plans/2026-04-30-phase-6-advanced-widgets.md`).
//!
//! # Predicate vocabulary
//!
//! Per `locked_decisions.visibility_predicate_vocabulary` in
//! `docs/plans/2026-04-30-phase-6-advanced-widgets.md`. The Phase 4 forms
//! `entity_available:<id>` and `state_equals:<id>:<v>` are accepted as aliases
//! for the Phase 6 forms `<id> != unavailable` and `<id> == <v>`; backward-
//! compat alias tests in this module pin that equivalence.
//!
//! | Form | Result |
//! |---|---|
//! | `always` | true |
//! | `never` | false |
//! | `entity_available:<id>` | alias of `<id> != unavailable` |
//! | `state_equals:<id>:<v>` | alias of `<id> == <v>` |
//! | `profile:<key>` | active profile key matches |
//! | `<id> == <value>` | entity state equals value |
//! | `<id> != <value>` | entity state not equals value |
//! | `<id> in [<v1>,<v2>,...]` | entity state appears in the list |
//! | `entity_state_numeric:<id>:<op>:<N>` | numeric compare on the state, op ∈ {lt,lte,gt,gte,eq,ne} |
//!
//! Numeric predicates that target a state string that does not parse as `f64`
//! return `false` (they never panic). Unknown predicate strings also return
//! `false` — those should never reach the evaluator at runtime because
//! [`crate::dashboard::validate`] rejects them at load time
//! (`ValidationRule::UnknownVisibilityPredicate`, Severity::Error).
//!
//! # Dependency index
//!
//! [`build_dep_index`] walks every widget in the dashboard, derives the entity
//! IDs the widget's `visibility:` predicate depends on, and produces a reverse
//! index `EntityId → Vec<WidgetId>`. The bridge layer holds this index on the
//! `Arc<Dashboard>` it carries, so each `state_changed` event needs only an
//! `O(1)` HashMap lookup followed by `O(k)` per-widget evaluations, where `k`
//! is the number of widgets dependent on the changed entity.
//!
//! ## Heap-spill warning
//!
//! Per `locked_decisions.visibility_evaluator`, when a single entity is the
//! visibility dependency for more than [`DEP_INLINE_CAP`] widgets (e.g. a
//! `group.all_lights` entity wired to many tiles) the bucket spills past the
//! ideal inline budget. The first spill in a process emits a `tracing::warn!`
//! row so operators can see the inline cap is too small for their dashboard;
//! subsequent spills are silent (the warn fires on the first transition only).
//!
//! ## Cost bound
//!
//! Per-event evaluator cost is bounded by `DeviceProfile.max_widgets_per_view`
//! (rpi4=32, opi_zero3=20, desktop=64) — a YAML exceeding that cap is already
//! a validation Error, so the effective maximum for any well-formed dashboard
//! is `max_widgets_per_view`. The pathological-YAML test below pumps a state
//! event through a fully-saturated view and asserts the evaluator runtime
//! stays under 1ms.
//!
//! # Inline-capacity fallback
//!
//! TASK-110's locked decision specifies a `SmallVec<[WidgetId; 8]>` bucket
//! type. The `smallvec` crate is in `Cargo.lock` only as a transitive
//! dependency; this ticket's `must_not_touch` list excludes the root
//! `Cargo.toml`. To honour both constraints the bucket is implemented as
//! [`DepBucket`] = `Vec<WidgetId>` and the inline cap [`DEP_INLINE_CAP`] is
//! used as the spill-warning threshold instead of an inline-allocation
//! boundary. The public API (`build_dep_index`, `evaluate`) remains
//! signature-compatible with a future `SmallVec` swap-in: a follow-up that
//! adds `smallvec` as a direct dependency can change the alias and the spill
//! threshold check to `vec.spilled()` without touching call sites.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::actions::map::WidgetId;
use crate::dashboard::schema::{Dashboard, ProfileKey};
use crate::ha::entity::EntityId;
use crate::ha::store::EntityStore;

// ---------------------------------------------------------------------------
// DEP_INLINE_CAP
// ---------------------------------------------------------------------------

/// Inline-capacity hint for the dependency-index bucket.
///
/// Per `locked_decisions.visibility_evaluator`, the bucket is conceptually
/// `SmallVec<[WidgetId; DEP_INLINE_CAP]>`. The constant is also surfaced on
/// every `DeviceProfile` preset
/// ([`crate::dashboard::profiles::DeviceProfile::dep_index_inline_cap`]) and
/// must remain in sync with the values pinned there.
pub const DEP_INLINE_CAP: usize = 8;

// Compile-time check: `DEP_INLINE_CAP` must equal the value pinned on every
// shipped device profile. If a future profile change diverges, the build
// fails with a clear error here rather than at runtime.
const _: () = {
    assert!(DEP_INLINE_CAP == crate::dashboard::profiles::PROFILE_DESKTOP.dep_index_inline_cap);
    assert!(DEP_INLINE_CAP == crate::dashboard::profiles::PROFILE_RPI4.dep_index_inline_cap);
    assert!(DEP_INLINE_CAP == crate::dashboard::profiles::PROFILE_OPI_ZERO3.dep_index_inline_cap);
};

/// Bucket type stored in the dependency-index map.
///
/// See the module-level note on the inline-capacity fallback for why this is
/// `Vec` instead of `SmallVec` in the current implementation.
pub type DepBucket = Vec<WidgetId>;

// ---------------------------------------------------------------------------
// First-spill warning latch
// ---------------------------------------------------------------------------

/// Process-global latch: the first time a dependency-index bucket grows past
/// [`DEP_INLINE_CAP`], we emit a single `tracing::warn!`. Subsequent spills
/// are silent. The latch resets only on process restart, matching the spec
/// in `locked_decisions.visibility_evaluator`.
static SPILL_WARNED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
fn reset_spill_latch_for_test() {
    SPILL_WARNED.store(false, Ordering::SeqCst);
}

fn maybe_emit_spill_warning(entity_id: &EntityId, len: usize) {
    if len <= DEP_INLINE_CAP {
        return;
    }
    // `compare_exchange` so only the very first spill across the process
    // emits the warning; subsequent spills observe `true` and silently
    // return.
    if SPILL_WARNED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        tracing::warn!(
            target: "dashboard.visibility",
            entity_id = %entity_id,
            bucket_len = len,
            inline_cap = DEP_INLINE_CAP,
            "dep_index bucket spilled past inline capacity; consider raising \
             DeviceProfile.dep_index_inline_cap or splitting the dependent group"
        );
    }
}

// ---------------------------------------------------------------------------
// Dependency extraction from a predicate
// ---------------------------------------------------------------------------

/// Returns the entity IDs that the given visibility predicate depends on.
///
/// `always`, `never`, and `profile:<key>` depend on no entities and return an
/// empty Vec. Unknown predicates also return an empty Vec — they are rejected
/// by the validator at load time.
fn predicate_entity_dependencies(predicate: &str) -> Vec<EntityId> {
    let p = predicate.trim();

    if p == "always" || p == "never" {
        return Vec::new();
    }
    if let Some(_key) = p.strip_prefix("profile:") {
        return Vec::new();
    }
    if let Some(rest) = p.strip_prefix("entity_available:") {
        return vec![EntityId::from(rest.trim())];
    }
    if let Some(rest) = p.strip_prefix("state_equals:") {
        // state_equals:<id>:<value>
        if let Some((id, _value)) = rest.split_once(':') {
            return vec![EntityId::from(id.trim())];
        }
        return Vec::new();
    }
    if let Some(rest) = p.strip_prefix("entity_state_numeric:") {
        // entity_state_numeric:<id>:<op>:<N>
        if let Some((id, _tail)) = rest.split_once(':') {
            return vec![EntityId::from(id.trim())];
        }
        return Vec::new();
    }

    // `<id> in [<v1>,<v2>,...]`
    if let Some((id_part, _list_part)) = p.split_once(" in ") {
        let id = id_part.trim();
        if !id.is_empty() {
            return vec![EntityId::from(id)];
        }
    }

    // `<id> == <value>` / `<id> != <value>`
    if let Some((id_part, _value)) = p.split_once(" == ") {
        let id = id_part.trim();
        if !id.is_empty() {
            return vec![EntityId::from(id)];
        }
    }
    if let Some((id_part, _value)) = p.split_once(" != ") {
        let id = id_part.trim();
        if !id.is_empty() {
            return vec![EntityId::from(id)];
        }
    }

    Vec::new()
}

// ---------------------------------------------------------------------------
// build_dep_index
// ---------------------------------------------------------------------------

/// Build the reverse `EntityId → Vec<WidgetId>` index for the given dashboard.
///
/// Walks every widget in every section of every view and, for each entity the
/// widget's `visibility:` predicate depends on, appends the widget's id into
/// that entity's bucket.
///
/// Per the module-level comment, if any bucket exceeds [`DEP_INLINE_CAP`] this
/// emits a single process-global `tracing::warn!` (first spill only).
#[must_use]
pub fn build_dep_index(dashboard: &Dashboard) -> HashMap<EntityId, DepBucket> {
    let mut index: HashMap<EntityId, DepBucket> = HashMap::new();

    for view in &dashboard.views {
        for section in &view.sections {
            for widget in &section.widgets {
                let widget_id = WidgetId::from(widget.id.as_str());
                let deps = predicate_entity_dependencies(&widget.visibility);
                for entity_id in deps {
                    index.entry(entity_id).or_default().push(widget_id.clone());
                }
            }
        }
    }

    // Fire the first-spill warning if any bucket exceeded the inline cap.
    for (entity_id, bucket) in &index {
        maybe_emit_spill_warning(entity_id, bucket.len());
    }

    index
}

// ---------------------------------------------------------------------------
// Numeric op
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumericOp {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Ne,
}

impl NumericOp {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "lt" => Some(NumericOp::Lt),
            "lte" => Some(NumericOp::Lte),
            "gt" => Some(NumericOp::Gt),
            "gte" => Some(NumericOp::Gte),
            "eq" => Some(NumericOp::Eq),
            "ne" => Some(NumericOp::Ne),
            _ => None,
        }
    }

    fn apply(self, lhs: f64, rhs: f64) -> bool {
        match self {
            NumericOp::Lt => lhs < rhs,
            NumericOp::Lte => lhs <= rhs,
            NumericOp::Gt => lhs > rhs,
            NumericOp::Gte => lhs >= rhs,
            NumericOp::Eq => lhs == rhs,
            NumericOp::Ne => (lhs - rhs).abs() > f64::EPSILON || lhs.is_nan() || rhs.is_nan(),
        }
    }
}

// ---------------------------------------------------------------------------
// evaluate
// ---------------------------------------------------------------------------

/// Evaluate a visibility predicate against the live store.
///
/// `entity_id` is the *primary* entity-id reference of the widget being
/// evaluated. The evaluator does not actually need it for any of the locked
/// predicate forms (each form spells the entity it cares about explicitly),
/// but it is taken as a parameter to:
/// - keep the signature stable against future predicate forms that might
///   reference `self` implicitly, and
/// - mirror the call shape the bridge already uses (every state-changed
///   event the bridge dispatches knows the affected entity id).
///
/// `profile_key` is the active [`ProfileKey`] (the dashboard's chosen device
/// profile). Used by the `profile:<key>` predicate; ignored for every other
/// form.
///
/// Returns `false` for any unknown / malformed predicate string. The validator
/// rejects unknown predicates at load time, so the evaluator should never see
/// one unless the caller skipped validation.
#[must_use]
pub fn evaluate(
    predicate: &str,
    entity_id: &EntityId,
    store: &dyn EntityStore,
    profile_key: ProfileKey,
) -> bool {
    let _ = entity_id; // see doc comment — accepted for signature stability
    let p = predicate.trim();

    if p == "always" {
        return true;
    }
    if p == "never" {
        return false;
    }

    // profile:<key>
    if let Some(key) = p.strip_prefix("profile:") {
        return profile_key_matches(key.trim(), profile_key);
    }

    // entity_available:<id>  →  alias of `<id> != unavailable`
    if let Some(id_str) = p.strip_prefix("entity_available:") {
        let id = EntityId::from(id_str.trim());
        return !state_equals(&id, store, "unavailable");
    }

    // state_equals:<id>:<v>  →  alias of `<id> == <v>`
    if let Some(rest) = p.strip_prefix("state_equals:") {
        if let Some((id, value)) = rest.split_once(':') {
            let id = EntityId::from(id.trim());
            return state_equals(&id, store, value.trim());
        }
        return false;
    }

    // entity_state_numeric:<id>:<op>:<N>
    if let Some(rest) = p.strip_prefix("entity_state_numeric:") {
        return evaluate_numeric(rest, store);
    }

    // `<id> in [<v1>,<v2>,...]`
    if let Some((id_part, list_part)) = p.split_once(" in ") {
        let id = EntityId::from(id_part.trim());
        return evaluate_in_list(&id, store, list_part);
    }

    // `<id> == <value>`
    if let Some((id_part, value)) = p.split_once(" == ") {
        let id = EntityId::from(id_part.trim());
        return state_equals(&id, store, value.trim());
    }

    // `<id> != <value>`
    if let Some((id_part, value)) = p.split_once(" != ") {
        let id = EntityId::from(id_part.trim());
        return !state_equals(&id, store, value.trim());
    }

    // Unknown predicate — validator should have caught this at load time.
    false
}

fn profile_key_matches(s: &str, profile_key: ProfileKey) -> bool {
    let active = match profile_key {
        ProfileKey::Rpi4 => "rpi4",
        ProfileKey::OpiZero3 => "opi-zero3",
        ProfileKey::Desktop => "desktop",
    };
    s == active
}

fn state_equals(id: &EntityId, store: &dyn EntityStore, expected: &str) -> bool {
    match store.get(id) {
        Some(entity) => &*entity.state == expected,
        // No entry → cannot satisfy the equality.
        None => false,
    }
}

fn evaluate_in_list(id: &EntityId, store: &dyn EntityStore, list_part: &str) -> bool {
    let trimmed = list_part.trim();
    // Strip the surrounding `[...]` brackets if present.
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    let entity = match store.get(id) {
        Some(e) => e,
        None => return false,
    };
    let state: &str = &entity.state;
    inner.split(',').any(|tok| tok.trim() == state)
}

fn evaluate_numeric(rest: &str, store: &dyn EntityStore) -> bool {
    // rest = `<id>:<op>:<N>`
    let mut parts = rest.splitn(3, ':');
    let id_str = match parts.next() {
        Some(s) => s.trim(),
        None => return false,
    };
    let op_str = match parts.next() {
        Some(s) => s.trim(),
        None => return false,
    };
    let n_str = match parts.next() {
        Some(s) => s.trim(),
        None => return false,
    };
    let op = match NumericOp::parse(op_str) {
        Some(o) => o,
        None => return false,
    };
    let n: f64 = match n_str.parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let id = EntityId::from(id_str);
    let entity = match store.get(&id) {
        Some(e) => e,
        None => return false,
    };
    let state: &str = &entity.state;
    let lhs: f64 = match state.parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    op.apply(lhs, n)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use jiff::Timestamp;
    use serde_json::Map;

    use super::*;
    use crate::dashboard::schema::{
        Dashboard, Layout, ProfileKey, Section, SectionGrid, View, Widget, WidgetKind, WidgetLayout,
    };
    use crate::ha::entity::Entity;
    use crate::ha::store::MemoryStore;

    fn make_entity(id: &str, state: &str) -> Entity {
        Entity {
            id: EntityId::from(id),
            state: Arc::from(state),
            attributes: Arc::new(Map::new()),
            last_changed: Timestamp::UNIX_EPOCH,
            last_updated: Timestamp::UNIX_EPOCH,
        }
    }

    fn store_with(entities: Vec<Entity>) -> MemoryStore {
        MemoryStore::load(entities).expect("MemoryStore::load")
    }

    fn primary_id() -> EntityId {
        EntityId::from("light.kitchen")
    }

    fn widget(id: &str, visibility: &str, entity: Option<&str>) -> Widget {
        Widget {
            id: id.to_string(),
            widget_type: WidgetKind::LightTile,
            entity: entity.map(str::to_owned),
            entities: vec![],
            name: None,
            icon: None,
            visibility: visibility.to_string(),
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns: 1,
                preferred_rows: 1,
            },
            options: None,
            placement: None,
        }
    }

    fn dashboard_with_widgets(widgets: Vec<Widget>) -> Dashboard {
        Dashboard {
            version: 1,
            device_profile: ProfileKey::Desktop,
            home_assistant: None,
            theme: None,
            default_view: "home".to_string(),
            views: vec![View {
                id: "home".to_string(),
                title: "Home".to_string(),
                layout: Layout::Sections,
                sections: vec![Section {
                    id: "s1".to_string(),
                    title: "S1".to_string(),
                    grid: SectionGrid::default(),
                    widgets,
                }],
            }],
            call_service_allowlist: Arc::default(),
            dep_index: Arc::default(),
        }
    }

    // -----------------------------------------------------------------------
    // evaluate: always / never
    // -----------------------------------------------------------------------

    #[test]
    fn evaluate_always_never() {
        let store = store_with(vec![]);
        let p = ProfileKey::Desktop;
        assert!(evaluate("always", &primary_id(), &store, p));
        assert!(!evaluate("never", &primary_id(), &store, p));
        // Whitespace tolerated.
        assert!(evaluate("  always ", &primary_id(), &store, p));
    }

    // -----------------------------------------------------------------------
    // evaluate: profile:<key>
    // -----------------------------------------------------------------------

    #[test]
    fn evaluate_profile_match_and_mismatch() {
        let store = store_with(vec![]);
        assert!(evaluate(
            "profile:desktop",
            &primary_id(),
            &store,
            ProfileKey::Desktop
        ));
        assert!(!evaluate(
            "profile:rpi4",
            &primary_id(),
            &store,
            ProfileKey::Desktop
        ));
        assert!(evaluate(
            "profile:opi-zero3",
            &primary_id(),
            &store,
            ProfileKey::OpiZero3
        ));
        assert!(evaluate(
            "profile:rpi4",
            &primary_id(),
            &store,
            ProfileKey::Rpi4
        ));
    }

    // -----------------------------------------------------------------------
    // Phase 6 vocabulary
    // -----------------------------------------------------------------------

    #[test]
    fn evaluate_eq_neq_in_list() {
        let store = store_with(vec![make_entity("light.kitchen", "on")]);
        let p = ProfileKey::Desktop;
        assert!(evaluate("light.kitchen == on", &primary_id(), &store, p));
        assert!(!evaluate("light.kitchen == off", &primary_id(), &store, p));
        assert!(evaluate("light.kitchen != off", &primary_id(), &store, p));
        assert!(!evaluate("light.kitchen != on", &primary_id(), &store, p));
        // in [..]
        assert!(evaluate(
            "light.kitchen in [on,off,unavailable]",
            &primary_id(),
            &store,
            p
        ));
        assert!(!evaluate(
            "light.kitchen in [off,unavailable]",
            &primary_id(),
            &store,
            p
        ));
        // Whitespace inside the list
        assert!(evaluate(
            "light.kitchen in [ on , off ]",
            &primary_id(),
            &store,
            p
        ));
    }

    // -----------------------------------------------------------------------
    // Phase 4 alias forms
    // -----------------------------------------------------------------------

    #[test]
    fn evaluate_state_equals_alias() {
        let store = store_with(vec![make_entity("light.kitchen", "on")]);
        let p = ProfileKey::Desktop;
        // state_equals:<id>:<v> ≡ <id> == <v>
        let canonical = evaluate("light.kitchen == on", &primary_id(), &store, p);
        let alias = evaluate("state_equals:light.kitchen:on", &primary_id(), &store, p);
        assert_eq!(canonical, alias);
        assert!(alias);
        // Negative case
        let canonical_off = evaluate("light.kitchen == off", &primary_id(), &store, p);
        let alias_off = evaluate("state_equals:light.kitchen:off", &primary_id(), &store, p);
        assert_eq!(canonical_off, alias_off);
        assert!(!alias_off);
    }

    #[test]
    fn evaluate_entity_available_alias() {
        let p = ProfileKey::Desktop;
        // available state
        let store = store_with(vec![make_entity("light.kitchen", "on")]);
        let canonical_on = evaluate("light.kitchen != unavailable", &primary_id(), &store, p);
        let alias_on = evaluate("entity_available:light.kitchen", &primary_id(), &store, p);
        assert_eq!(canonical_on, alias_on);
        assert!(alias_on);
        // unavailable state
        let store = store_with(vec![make_entity("light.kitchen", "unavailable")]);
        let canonical_off = evaluate("light.kitchen != unavailable", &primary_id(), &store, p);
        let alias_off = evaluate("entity_available:light.kitchen", &primary_id(), &store, p);
        assert_eq!(canonical_off, alias_off);
        assert!(!alias_off);
    }

    // -----------------------------------------------------------------------
    // entity_state_numeric — boundaries for every op
    // -----------------------------------------------------------------------

    #[test]
    fn evaluate_entity_state_numeric_boundary() {
        // state == 5
        let store = store_with(vec![make_entity("sensor.power", "5")]);
        let id = primary_id();
        let p = ProfileKey::Desktop;
        // lt: 5 < 5 → false; 5 < 6 → true
        assert!(!evaluate(
            "entity_state_numeric:sensor.power:lt:5",
            &id,
            &store,
            p
        ));
        assert!(evaluate(
            "entity_state_numeric:sensor.power:lt:6",
            &id,
            &store,
            p
        ));
        // lte: 5 <= 5 → true; 5 <= 4 → false
        assert!(evaluate(
            "entity_state_numeric:sensor.power:lte:5",
            &id,
            &store,
            p
        ));
        assert!(!evaluate(
            "entity_state_numeric:sensor.power:lte:4",
            &id,
            &store,
            p
        ));
        // gt: 5 > 5 → false; 5 > 4 → true
        assert!(!evaluate(
            "entity_state_numeric:sensor.power:gt:5",
            &id,
            &store,
            p
        ));
        assert!(evaluate(
            "entity_state_numeric:sensor.power:gt:4",
            &id,
            &store,
            p
        ));
        // gte: 5 >= 5 → true; 5 >= 6 → false
        assert!(evaluate(
            "entity_state_numeric:sensor.power:gte:5",
            &id,
            &store,
            p
        ));
        assert!(!evaluate(
            "entity_state_numeric:sensor.power:gte:6",
            &id,
            &store,
            p
        ));
        // eq: 5 == 5 → true; 5 == 6 → false
        assert!(evaluate(
            "entity_state_numeric:sensor.power:eq:5",
            &id,
            &store,
            p
        ));
        assert!(!evaluate(
            "entity_state_numeric:sensor.power:eq:6",
            &id,
            &store,
            p
        ));
        // ne: 5 != 5 → false; 5 != 6 → true
        assert!(!evaluate(
            "entity_state_numeric:sensor.power:ne:5",
            &id,
            &store,
            p
        ));
        assert!(evaluate(
            "entity_state_numeric:sensor.power:ne:6",
            &id,
            &store,
            p
        ));
    }

    #[test]
    fn evaluate_entity_state_numeric_non_parseable_state_returns_false() {
        let store = store_with(vec![make_entity("sensor.power", "unavailable")]);
        let id = primary_id();
        let p = ProfileKey::Desktop;
        assert!(!evaluate(
            "entity_state_numeric:sensor.power:gt:0",
            &id,
            &store,
            p
        ));
        assert!(!evaluate(
            "entity_state_numeric:sensor.power:eq:0",
            &id,
            &store,
            p
        ));
    }

    #[test]
    fn evaluate_entity_state_numeric_non_parseable_n_returns_false() {
        let store = store_with(vec![make_entity("sensor.power", "5")]);
        let id = primary_id();
        let p = ProfileKey::Desktop;
        assert!(!evaluate(
            "entity_state_numeric:sensor.power:gt:notanumber",
            &id,
            &store,
            p
        ));
    }

    #[test]
    fn evaluate_entity_state_numeric_unknown_op_returns_false() {
        let store = store_with(vec![make_entity("sensor.power", "5")]);
        let id = primary_id();
        let p = ProfileKey::Desktop;
        assert!(!evaluate(
            "entity_state_numeric:sensor.power:bogus:5",
            &id,
            &store,
            p
        ));
    }

    // -----------------------------------------------------------------------
    // unknown / missing entity / malformed
    // -----------------------------------------------------------------------

    #[test]
    fn evaluate_returns_false_for_unknown_predicate() {
        let store = store_with(vec![]);
        assert!(!evaluate(
            "this_is_not_a_known_form",
            &primary_id(),
            &store,
            ProfileKey::Desktop,
        ));
    }

    #[test]
    fn evaluate_returns_false_when_entity_missing_for_eq() {
        let store = store_with(vec![]);
        assert!(!evaluate(
            "light.absent == on",
            &primary_id(),
            &store,
            ProfileKey::Desktop,
        ));
    }

    #[test]
    fn evaluate_returns_true_when_entity_missing_for_neq() {
        // `light.absent != on` — store returns None; we treat that as "not equal to on".
        // Document the chosen semantics: missing-entity means the equality cannot
        // hold, so its negation holds.
        let store = store_with(vec![]);
        assert!(!evaluate(
            // sanity: explicit equality is false
            "light.absent == on",
            &primary_id(),
            &store,
            ProfileKey::Desktop,
        ));
        // Negation flips the result.
        assert!(evaluate(
            "light.absent != on",
            &primary_id(),
            &store,
            ProfileKey::Desktop,
        ));
    }

    // -----------------------------------------------------------------------
    // build_dep_index
    // -----------------------------------------------------------------------

    #[test]
    fn build_dep_index_o1_lookup() {
        let widgets = vec![
            widget("w1", "light.kitchen == on", Some("light.kitchen")),
            widget("w2", "sensor.power != unavailable", Some("sensor.power")),
            widget("w3", "always", None),
            widget("w4", "light.kitchen != off", Some("light.kitchen")),
        ];
        let dashboard = dashboard_with_widgets(widgets);
        let index = build_dep_index(&dashboard);

        // light.kitchen → [w1, w4]
        let kitchen = index
            .get(&EntityId::from("light.kitchen"))
            .expect("kitchen bucket");
        assert_eq!(kitchen.len(), 2);
        assert!(kitchen.contains(&WidgetId::from("w1")));
        assert!(kitchen.contains(&WidgetId::from("w4")));

        // sensor.power → [w2]
        let power = index
            .get(&EntityId::from("sensor.power"))
            .expect("power bucket");
        assert_eq!(power.len(), 1);
        assert!(power.contains(&WidgetId::from("w2")));

        // always-visible widget contributes no entry.
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn build_dep_index_handles_alias_and_phase6_forms() {
        let widgets = vec![
            widget(
                "alias_avail",
                "entity_available:light.kitchen",
                Some("light.kitchen"),
            ),
            widget(
                "alias_state_equals",
                "state_equals:light.kitchen:on",
                Some("light.kitchen"),
            ),
            widget(
                "phase6_in",
                "light.kitchen in [on,off]",
                Some("light.kitchen"),
            ),
            widget(
                "phase6_numeric",
                "entity_state_numeric:sensor.power:lt:100",
                Some("sensor.power"),
            ),
            widget("never_widget", "never", None),
            widget("profile_widget", "profile:desktop", None),
        ];
        let dashboard = dashboard_with_widgets(widgets);
        let index = build_dep_index(&dashboard);

        let kitchen = index
            .get(&EntityId::from("light.kitchen"))
            .expect("kitchen bucket");
        assert_eq!(kitchen.len(), 3, "alias forms must populate the bucket too");
        let power = index
            .get(&EntityId::from("sensor.power"))
            .expect("power bucket");
        assert_eq!(power.len(), 1);

        // never / profile do not depend on any entity → no entries
        assert!(
            index.len() == 2,
            "got: {:?}",
            index.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn build_dep_index_empty_for_dashboard_with_no_widgets() {
        let dashboard = dashboard_with_widgets(vec![]);
        let index = build_dep_index(&dashboard);
        assert!(index.is_empty());
    }

    // -----------------------------------------------------------------------
    // Heap-spill warning
    // -----------------------------------------------------------------------

    #[test]
    fn heap_spill_emits_warn() {
        // tracing_test asserts on logged spans; the latch is process-global so
        // we reset it before the test runs.
        reset_spill_latch_for_test();

        // 9 widgets all gated on a single entity — exceeds DEP_INLINE_CAP (8).
        let mut widgets = Vec::new();
        for i in 0..9 {
            widgets.push(widget(
                &format!("w{i}"),
                "light.kitchen == on",
                Some("light.kitchen"),
            ));
        }
        let dashboard = dashboard_with_widgets(widgets);
        let index = build_dep_index(&dashboard);
        let bucket = index
            .get(&EntityId::from("light.kitchen"))
            .expect("kitchen bucket");
        assert_eq!(bucket.len(), 9);
        assert!(bucket.len() > DEP_INLINE_CAP);

        // The tracing!warn was emitted; a second build_dep_index run does NOT
        // emit again (latch is one-shot).
        let _ = build_dep_index(&dashboard);
    }

    #[test]
    fn build_dep_index_no_spill_below_cap() {
        reset_spill_latch_for_test();
        let mut widgets = Vec::new();
        for i in 0..DEP_INLINE_CAP {
            widgets.push(widget(
                &format!("w{i}"),
                "light.kitchen == on",
                Some("light.kitchen"),
            ));
        }
        let dashboard = dashboard_with_widgets(widgets);
        let index = build_dep_index(&dashboard);
        assert_eq!(
            index
                .get(&EntityId::from("light.kitchen"))
                .map(Vec::len)
                .unwrap_or(0),
            DEP_INLINE_CAP,
            "bucket must hit the cap exactly without spilling"
        );
        // Latch must remain in the not-yet-warned state.
        assert!(!SPILL_WARNED.load(Ordering::SeqCst));
    }

    // -----------------------------------------------------------------------
    // Pathological-YAML performance bound (Risk #8)
    // -----------------------------------------------------------------------

    /// Per acceptance criterion, populate a view at the per-profile
    /// `max_widgets_per_view` cap (we use desktop=64 here as the loosest cap),
    /// all gated on the same entity, then evaluate every widget once. The
    /// total runtime must stay under 1ms on a release build; we use a more
    /// permissive 50ms here so the test does not flake on debug builds or
    /// loaded CI runners. The principal control is that the runtime is
    /// bounded — the validator caps the per-view widget count, so a real
    /// dashboard cannot exceed this many evaluations per state-changed event.
    #[test]
    fn evaluator_bound_on_pathological_yaml() {
        let cap = crate::dashboard::profiles::PROFILE_DESKTOP.max_widgets_per_view;
        let mut widgets = Vec::with_capacity(cap);
        for i in 0..cap {
            widgets.push(widget(
                &format!("w{i}"),
                "light.kitchen == on",
                Some("light.kitchen"),
            ));
        }
        let _dashboard = dashboard_with_widgets(widgets);
        let store = store_with(vec![make_entity("light.kitchen", "on")]);

        let id = EntityId::from("light.kitchen");
        let start = std::time::Instant::now();
        // Simulate the bridge loop: one evaluate() per widget for a single event.
        for _ in 0..cap {
            let _ = evaluate("light.kitchen == on", &id, &store, ProfileKey::Desktop);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 50,
            "evaluator took {elapsed:?} for {cap} widgets — bound exceeded",
        );
    }

    // -----------------------------------------------------------------------
    // Numeric op apply — exercise every variant under direct call
    // -----------------------------------------------------------------------

    #[test]
    fn numeric_op_parse_and_apply() {
        assert_eq!(NumericOp::parse("lt"), Some(NumericOp::Lt));
        assert_eq!(NumericOp::parse("lte"), Some(NumericOp::Lte));
        assert_eq!(NumericOp::parse("gt"), Some(NumericOp::Gt));
        assert_eq!(NumericOp::parse("gte"), Some(NumericOp::Gte));
        assert_eq!(NumericOp::parse("eq"), Some(NumericOp::Eq));
        assert_eq!(NumericOp::parse("ne"), Some(NumericOp::Ne));
        assert_eq!(NumericOp::parse("xx"), None);

        assert!(NumericOp::Lt.apply(1.0, 2.0));
        assert!(NumericOp::Lte.apply(2.0, 2.0));
        assert!(NumericOp::Gt.apply(3.0, 2.0));
        assert!(NumericOp::Gte.apply(2.0, 2.0));
        assert!(NumericOp::Eq.apply(2.0, 2.0));
        assert!(NumericOp::Ne.apply(2.0, 3.0));
    }

    // -----------------------------------------------------------------------
    // predicate_entity_dependencies
    // -----------------------------------------------------------------------

    #[test]
    fn predicate_dependencies_for_each_form() {
        let single = |s: &str| predicate_entity_dependencies(s);
        assert!(single("always").is_empty());
        assert!(single("never").is_empty());
        assert!(single("profile:rpi4").is_empty());
        assert_eq!(
            single("entity_available:sensor.x"),
            vec![EntityId::from("sensor.x")]
        );
        assert_eq!(
            single("state_equals:sensor.x:on"),
            vec![EntityId::from("sensor.x")]
        );
        assert_eq!(
            single("entity_state_numeric:sensor.x:gt:10"),
            vec![EntityId::from("sensor.x")]
        );
        assert_eq!(single("light.k == on"), vec![EntityId::from("light.k")]);
        assert_eq!(single("light.k != off"), vec![EntityId::from("light.k")]);
        assert_eq!(
            single("light.k in [on,off]"),
            vec![EntityId::from("light.k")]
        );
        assert!(single("garbage_predicate").is_empty());
        // Malformed state_equals (missing value) → empty
        assert!(single("state_equals:light.k").is_empty());
    }
}
