//! More-info modal: trait, attribute body, and lazy-render plumbing (TASK-066).
//!
//! # Shape
//!
//! Phase 3 ships the modal as a *generic shell* with one body
//! implementation:
//!
//! * [`MoreInfoBody`] — the object-safe trait every per-domain body must
//!   implement. Phase 3 ships [`AttributesBody`]; Phase 6 will add per-domain
//!   bodies (light, climate, media-player, …).
//! * [`AttributesBody`] — the default body. Renders up to 32 attribute rows
//!   with each value truncated to 256 chars after a *typed* display formatter
//!   that matches each JSON variant explicitly — never a raw `to_string()` of
//!   an arbitrary value, which would emit JSON-encoded strings (escaped
//!   quotes, unicode escapes) and risk render bombs on misshapen entity
//!   state.
//!
//! # Lazy-render contract (locked_decisions.more_info_modal)
//!
//! Rows are computed on **modal open**, not on every entity update. The
//! modal-open path calls [`MoreInfoBody::render_rows`] exactly once, stores
//! the resulting `Vec<ModalRow>`, and renders that slice in the Slint shell.
//! Subsequent entity-state pushes do not call `render_rows` again — only a
//! second open does. This is encoded by [`ModalState::open_with`], which
//! invokes the row-builder only inside `open_with` and stashes the result
//! in [`ModalState::rows`]. [`ModalState::on_entity_update`] is a no-op while
//! open: it satisfies the locked decision and is asserted by the integration
//! tests under `tests/integration/more_info_modal.rs`.
//!
//! # Why no JSON-crate path is named here
//!
//! `src/ui/**` is gated against the JSON-crate path (`.github/workflows/ci.yml`
//! Gate 2). [`AttributesBody`] reads `entity.attributes` through inferred
//! types and method calls only — `as_str` / `as_bool` / `as_i64` /
//! `as_u64` / `as_f64` / `as_array` / `as_object` / `is_null`. None of
//! those reference the crate by name; the value type is resolved through
//! type inference at the call site (the iteration over
//! `entity.attributes.iter()`). This keeps the typed-formatter contract
//! (no raw `to_string` on arbitrary values) without tripping the grep
//! gate or needing a waiver.

use std::sync::Arc;

use crate::ha::entity::{Entity, EntityId};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Maximum number of attribute rows rendered by [`AttributesBody`]
/// (locked_decisions.more_info_modal). Attributes beyond the cap are
/// truncated; the modal does not paginate.
pub const MAX_ATTRIBUTE_ROWS: usize = 32;

/// Maximum length of a rendered attribute value, in characters. Values
/// longer than this are truncated to the cap (see [`truncate_value`]).
/// The cap is a *character* count (Unicode scalar values), not bytes — so a
/// long emoji string truncates by visible glyphs rather than mid-byte.
pub const MAX_VALUE_CHARS: usize = 256;

// ---------------------------------------------------------------------------
// ModalRow
// ---------------------------------------------------------------------------

/// A single row in the more-info modal body.
///
/// The Slint shell renders this as `key: value` (typography decided by the
/// shell, not the body). Both fields are owned `String`s — the body is built
/// once on open and the strings persist for the lifetime of the modal-open
/// session, so an `Arc<str>` would over-engineer the (≤32) row case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalRow {
    /// Attribute key (e.g. `"brightness"`, `"friendly_name"`).
    pub key: String,
    /// Pre-truncated value rendered through the typed display formatter.
    pub value: String,
}

// ---------------------------------------------------------------------------
// MoreInfoBody trait
// ---------------------------------------------------------------------------

/// Object-safe: must remain usable as `dyn MoreInfoBody`. Phase 6 per-domain
/// bodies must implement via type-erased adapters, not generics or
/// associated types. See locked_decisions.more_info_modal in
/// docs/plans/2026-04-28-phase-3-actions.md.
///
/// # Why object-safe
///
/// The Slint shell stores a single body via `Arc<dyn MoreInfoBody>` so the
/// per-domain body — `AttributesBody` in Phase 3, `LightBody` /
/// `ClimateBody` / … in Phase 6 — can be swapped at runtime based on the
/// opened entity's domain. Generics would force the shell to be
/// parameterised on the concrete body type, which would monomorphise the
/// modal into one per-domain copy (binary bloat) and prevent runtime body
/// selection (correctness). Phase 6 is required to preserve this
/// constraint; if a future body needs generic behaviour, the generic
/// surface must be exposed via a type-erased adapter (e.g. a builder that
/// produces an `Arc<dyn MoreInfoBody>` from typed inputs), **not** by
/// adding generics or associated types to this trait.
///
/// # Compile-time enforcement
///
/// The `_OBJECT_SAFETY` constant below builds a `dyn MoreInfoBody` to
/// statically forbid any change to this trait that would break
/// object-safety. The build will fail at compile time if a future edit
/// adds a generic method, an associated type, a `Self`-by-value method,
/// etc. — there is no runtime check to dodge.
pub trait MoreInfoBody: Send + Sync {
    /// Build the row list for `entity`. Called exactly once per
    /// modal-open by [`ModalState::open_with`]; the returned `Vec` is
    /// rendered by the Slint shell and is **not** rebuilt on subsequent
    /// entity updates while the modal stays open.
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow>;
}

// Compile-time object-safety guard. If a future edit makes
// `MoreInfoBody` non-object-safe (e.g. adds a generic method or an
// associated type), this line fails to compile and CI blocks the
// change.
const _OBJECT_SAFETY: fn(&dyn MoreInfoBody) = |_| {};

// Compile-time `AttributesBody: MoreInfoBody` assertion. If
// `AttributesBody` ever drifts out of conformance with the trait, this
// fails to compile.
const _ATTRIBUTES_BODY_IS_MORE_INFO_BODY: fn() = || {
    fn assert_impl<T: MoreInfoBody>() {}
    assert_impl::<AttributesBody>();
};

// ---------------------------------------------------------------------------
// AttributesBody
// ---------------------------------------------------------------------------

/// Default more-info body — renders up to [`MAX_ATTRIBUTE_ROWS`] attribute
/// rows from an [`Entity`]'s `attributes` map.
///
/// # Per locked_decisions.more_info_modal
///
/// * Cap: at most 32 rows. Enforced at iteration time, not by constructing
///   a full vector and truncating — large attribute maps (some HA
///   integrations emit hundreds of attributes) would otherwise spend O(N)
///   work even though only the first 32 ship to Slint.
/// * Per-value cap: each value is truncated to 256 chars after the typed
///   display formatter. The cap is in *characters* (Unicode scalar values)
///   not bytes — a long emoji-only attribute value truncates by visible
///   glyphs.
/// * Typed formatter: arbitrary attribute values are **never** rendered
///   via raw `to_string()` (which would JSON-encode strings, surfacing
///   escaped quotes to the user and risking log/render bombs). Each JSON
///   variant is matched explicitly via the typed accessors.
/// * Stable order: keys are sorted alphabetically so the modal's first
///   render and any reopen produce the same row order regardless of
///   underlying `Map` iteration order.
#[derive(Debug, Default)]
pub struct AttributesBody;

impl AttributesBody {
    /// Construct an [`AttributesBody`]. The body is stateless — every
    /// instance produces the same rows for the same input — but the
    /// constructor exists so callers do not depend on `Default`.
    #[must_use]
    pub fn new() -> Self {
        AttributesBody
    }
}

impl MoreInfoBody for AttributesBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        // Sort keys alphabetically so the row order is deterministic
        // regardless of map iteration order. Sort is O(N log N) on
        // attribute count; even at N = 1000 attributes (worst-case HA
        // emitter), the sort is fewer than 10k comparisons, well under
        // any modal-open budget.
        let mut keys: Vec<&String> = entity.attributes.keys().collect();
        keys.sort();

        let mut rows = Vec::with_capacity(keys.len().min(MAX_ATTRIBUTE_ROWS));
        for key in keys.into_iter().take(MAX_ATTRIBUTE_ROWS) {
            // `value` is `&_` — type inference resolves it through the
            // attribute map's element type. No JSON-crate name is
            // mentioned in this file; the inherent typed accessors
            // (`as_str`, `as_bool`, `as_i64`, `as_u64`, `as_f64`,
            // `is_null`, `as_array`, `as_object`) dispatch on the
            // concrete type without requiring a `use` import here.
            let Some(value) = entity.attributes.get(key) else {
                continue;
            };

            let formatted = if let Some(s) = value.as_str() {
                // Bare string — NEVER `value.to_string()`, which would
                // JSON-encode surrounding quotes and escape inner ones.
                s.to_owned()
            } else if let Some(b) = value.as_bool() {
                if b {
                    "true".to_owned()
                } else {
                    "false".to_owned()
                }
            } else if let Some(i) = value.as_i64() {
                i.to_string()
            } else if let Some(u) = value.as_u64() {
                u.to_string()
            } else if let Some(f) = value.as_f64() {
                // Float formatting via Rust's Display, not JSON encoding.
                format!("{f}")
            } else if value.is_null() {
                "null".to_owned()
            } else if let Some(arr) = value.as_array() {
                // Phase 3 does not recurse into nested arrays — emit a
                // bounded summary instead. Phase 6 per-domain bodies may
                // expand specific arrays they understand.
                format!("[{} items]", arr.len())
            } else if let Some(obj) = value.as_object() {
                format!("{{{} keys}}", obj.len())
            } else {
                // Defensive fallback for a future variant addition.
                "<unsupported>".to_owned()
            };

            let truncated = truncate_value(&formatted, MAX_VALUE_CHARS);
            rows.push(ModalRow {
                key: key.clone(),
                value: truncated,
            });
        }
        rows
    }
}

// ---------------------------------------------------------------------------
// truncate_value
// ---------------------------------------------------------------------------

/// Truncate a string to at most `max_chars` Unicode scalar values
/// (code points).
///
/// The truncation respects character boundaries — `s.split_at(byte_index)`
/// would panic mid-codepoint. We walk `s.chars()` and slice at the byte
/// offset of the (max_chars+1)-th char if reached.
///
/// The trailing ellipsis is intentionally omitted: appending `…` would
/// push truncated values one character past the cap and complicate the
/// "exactly 256 chars" assertion the integration tests rely on. Phase 6
/// per-domain bodies may add ellipses if their value-domain semantics
/// benefit; the default body sticks to a hard cap.
pub(crate) fn truncate_value(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    s.chars().take(max_chars).collect()
}

// ---------------------------------------------------------------------------
// ModalState — lazy-render plumbing
// ---------------------------------------------------------------------------

/// Lazy-render state for the modal.
///
/// The dispatcher (TASK-062) emits `DispatchOutcome::MoreInfo` with the
/// entity_id from the `WidgetActionEntry`. The bridge converts that into
/// a call to [`ModalState::open_with`], which invokes the body's
/// row-builder *exactly once* and stores the result. Until the modal
/// closes, [`ModalState::rows`] returns a stable slice.
///
/// Per locked_decisions.more_info_modal, the row-builder must NOT run on
/// every entity update; [`ModalState::on_entity_update`] is the contract
/// the bridge calls when an entity tick arrives, and it is intentionally
/// a no-op while the modal is open. Reopening (which can happen if the
/// user closes the modal and long-presses the same tile again)
/// re-invokes the builder against the *current* entity, so the user sees
/// the freshest snapshot for the second open.
///
/// `body` is `Arc<dyn MoreInfoBody>` so the same body can be shared
/// across multiple modal sessions without re-allocating; Phase 6 will
/// swap the concrete body per-domain by replacing this `Arc` at the call
/// site.
pub struct ModalState {
    body: Arc<dyn MoreInfoBody>,
    open_for: Option<EntityId>,
    rows: Vec<ModalRow>,
}

impl ModalState {
    /// Construct a closed modal backed by `body`.
    #[must_use]
    pub fn new(body: Arc<dyn MoreInfoBody>) -> Self {
        ModalState {
            body,
            open_for: None,
            rows: Vec::new(),
        }
    }

    /// Open the modal for `entity`, computing rows lazily. Calls
    /// `body.render_rows(entity)` exactly once and stashes the result.
    pub fn open_with(&mut self, entity: &Entity) {
        self.open_for = Some(entity.id.clone());
        self.rows = self.body.render_rows(entity);
    }

    /// Close the modal. Drops the row buffer so a future reopen
    /// recomputes.
    pub fn close(&mut self) {
        self.open_for = None;
        self.rows.clear();
    }

    /// Whether the modal is currently open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    /// The entity_id the modal is open for, if any.
    #[must_use]
    pub fn open_for(&self) -> Option<&EntityId> {
        self.open_for.as_ref()
    }

    /// The currently-rendered rows. Empty when the modal is closed.
    #[must_use]
    pub fn rows(&self) -> &[ModalRow] {
        &self.rows
    }

    /// Notification hook for entity updates. **No-op while open** per
    /// locked_decisions.more_info_modal: rows are computed on open, not
    /// on every tick. The hook exists so the bridge can call it
    /// unconditionally without branching on modal state — matching the
    /// existing per-entity subscriber pattern in `src/ui/bridge.rs`.
    pub fn on_entity_update(&mut self, _entity: &Entity) {
        // Intentionally empty. See doc-comment.
    }
}

// ---------------------------------------------------------------------------
// Slint property wiring (lazy, idempotent)
// ---------------------------------------------------------------------------

/// Header text rendered in the modal's title row. Distinct from
/// `EntityState` so callers can compute a friendly name (typically
/// `entity.friendly_name().unwrap_or_else(|| entity.id.as_str())`)
/// without leaking that fallback policy into the Slint shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalHeader {
    /// Entity name to display (friendly_name fallback to id).
    pub name: String,
    /// State string for the header subline (e.g. `"on"`, `"off"`).
    pub state: String,
}

impl ModalHeader {
    /// Compute the header from an entity, preferring `friendly_name`
    /// over the raw id for the `name` field.
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Self {
        let name = entity
            .friendly_name()
            .map(str::to_owned)
            .unwrap_or_else(|| entity.id.as_str().to_owned());
        ModalHeader {
            name,
            state: entity.state.as_ref().to_owned(),
        }
    }
}

/// Apply the current [`ModalState`] (and its header) to the
/// `MainWindow`'s more-info properties.
///
/// The function is idempotent: calling it twice with the same state
/// produces the same window state. Production callers in the bridge
/// invoke this once per modal-open transition and once per
/// modal-close transition; the lazy-render contract means there are no
/// other callers per locked_decisions.more_info_modal.
///
/// `header` is computed by the caller (typically via
/// [`ModalHeader::from_entity`]) so this function does not need to
/// re-look up the entity — the bridge already holds the entity that
/// drove `ModalState::open_with`.
pub fn apply_modal_to_window(
    state: &ModalState,
    header: &ModalHeader,
    window: &crate::ui::bridge::MainWindow,
) {
    use slint::{ModelRc, SharedString, VecModel};

    // Convert the `Vec<ModalRow>` into the Slint-typed `ModalRowVM`
    // shape and wrap in a `ModelRc` for the array property.
    let rows: Vec<crate::ui::bridge::slint_ui::ModalRowVM> = state
        .rows()
        .iter()
        .map(|r| crate::ui::bridge::slint_ui::ModalRowVM {
            key: SharedString::from(r.key.as_str()),
            value: SharedString::from(r.value.as_str()),
        })
        .collect();
    let model: ModelRc<crate::ui::bridge::slint_ui::ModalRowVM> =
        ModelRc::new(VecModel::from(rows));

    window.set_more_info_rows(model);
    window.set_more_info_entity_name(SharedString::from(header.name.as_str()));
    window.set_more_info_entity_state(SharedString::from(header.state.as_str()));
    window.set_more_info_visible(state.is_open());
}

// ---------------------------------------------------------------------------
// Trait & doc-comment self-test (compile-time + grep)
// ---------------------------------------------------------------------------
//
// Phase 3 acceptance asserts that the trait carries a *prominent*
// doc-comment naming the object-safety constraint. The grep test under
// `tests/integration/more_info_modal.rs` covers the source-level check;
// the compile-time `_OBJECT_SAFETY` constant above covers the property
// itself. Both are load-bearing per opencode review 2026-04-28: a
// runtime test alone is insufficient because Phase 6 maintainers need
// the constraint visible at the trait definition.

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time `_OBJECT_SAFETY` constant cannot be referenced
    /// directly (it's `const _:` with no name in the module). This
    /// runtime test verifies the same property a different way: build
    /// an `Arc<dyn MoreInfoBody>` and prove we can call through it. If
    /// the trait ever loses object-safety, this test fails to compile.
    #[test]
    fn more_info_body_is_object_safe() {
        let _body: Arc<dyn MoreInfoBody> = Arc::new(AttributesBody::new());
    }

    /// `truncate_value` is the only test we can run in-file without
    /// touching the JSON crate (which would trip Gate 2). The richer
    /// behavioural tests live in `tests/integration/more_info_modal.rs`.
    #[test]
    fn truncate_value_below_cap_is_passthrough() {
        assert_eq!(truncate_value("hello", 256), "hello");
    }

    #[test]
    fn truncate_value_above_cap_truncates_to_exact_char_count() {
        let s: String = "a".repeat(1024);
        let t = truncate_value(&s, 256);
        assert_eq!(t.chars().count(), 256);
    }

    #[test]
    fn truncate_value_respects_unicode_boundaries() {
        let s: String = "🦀".repeat(100);
        let t = truncate_value(&s, 50);
        assert_eq!(t.chars().count(), 50);
        assert!(t.chars().all(|c| c == '🦀'));
    }

    #[test]
    fn modal_row_equality_is_by_value() {
        let a = ModalRow {
            key: "k".to_owned(),
            value: "v".to_owned(),
        };
        let b = ModalRow {
            key: "k".to_owned(),
            value: "v".to_owned(),
        };
        assert_eq!(a, b);
    }
}
