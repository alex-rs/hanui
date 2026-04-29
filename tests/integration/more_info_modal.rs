//! Integration tests for the more-info modal (TASK-066).
//!
//! These tests live outside `src/ui/**` so they can construct
//! [`Entity`] values with arbitrary [`serde_json::Value`] attributes
//! (the `src/ui/**` grep gate forbids the JSON crate path inside the
//! production source — the typed-formatter contract is enforced by
//! ensuring the Rust callers in `src/ui/more_info.rs` never name the
//! JSON crate, and these tests verify that the typed formatter actually
//! produces the right output for every JSON variant).
//!
//! # Coverage
//!
//! * Compile-time `Object-safe: dyn MoreInfoBody` constructibility (the
//!   `_OBJECT_SAFETY` constant in `more_info.rs` already encodes this at
//!   compile time; this file additionally verifies the runtime path).
//! * Compile-time `AttributesBody: MoreInfoBody` (the
//!   `_ATTRIBUTES_BODY_IS_MORE_INFO_BODY` constant in `more_info.rs`
//!   encodes this; this file uses the trait object to call through it
//!   end-to-end).
//! * Doc-comment grep — the trait-level doc-comment must literally
//!   include the object-safety constraint phrase per
//!   locked_decisions.more_info_modal (opencode review 2026-04-28
//!   blocker — compile-time test alone is insufficient because Phase 6
//!   maintainers need the constraint visible at the trait definition).
//! * 32-attr cap.
//! * 256-char value truncation.
//! * Lazy-render: row-builder NOT invoked on entity-update while open.
//! * Lazy-render: reopen recomputes against current attributes.
//! * Typed formatter rejects raw `Value::to_string()` pattern (no JSON
//!   quotes, no JSON escapes).
//! * Slint render integration via the harness from TASK-074: modal
//!   renders to a non-empty pixel buffer with the open/closed mutation
//!   reflected in the captured frame.

#![allow(clippy::cast_possible_truncation)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use jiff::Timestamp;
use serde_json::{json, Map, Value};
// `ComponentHandle` is not strictly needed here because we exclusively
// use the inherent setters/getters on `MainWindow`; tests that call
// `.window()` would need it. Keeping the import out avoids a
// `unused_imports` warning under `-D warnings`.

use hanui::ha::entity::{Entity, EntityId};
use hanui::ui::bridge::MainWindow;
use hanui::ui::more_info::{
    apply_modal_to_window, AttributesBody, ModalHeader, ModalRow, ModalState, MoreInfoBody,
    MAX_ATTRIBUTE_ROWS, MAX_VALUE_CHARS,
};

use super::slint_harness::HeadlessRenderer;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn entity_with_attrs(id: &str, attrs: Map<String, Value>) -> Entity {
    Entity {
        id: EntityId::from(id),
        state: Arc::from("on"),
        attributes: Arc::new(attrs),
        last_changed: Timestamp::UNIX_EPOCH,
        last_updated: Timestamp::UNIX_EPOCH,
    }
}

// ---------------------------------------------------------------------------
// Compile-time / runtime object-safety
// ---------------------------------------------------------------------------

#[test]
fn more_info_body_is_constructible_as_trait_object() {
    // If MoreInfoBody were not object-safe, this line would fail to
    // compile — matching the compile-time `_OBJECT_SAFETY` constant in
    // src/ui/more_info.rs. We build through `Arc<dyn ...>` because
    // production code uses `Arc<dyn MoreInfoBody>` to share bodies
    // across modal sessions per locked_decisions.more_info_modal.
    let body: Arc<dyn MoreInfoBody> = Arc::new(AttributesBody::new());
    let attrs = Map::new();
    let entity = entity_with_attrs("light.kitchen", attrs);
    let rows = body.render_rows(&entity);
    assert!(rows.is_empty());
}

#[test]
fn attributes_body_implements_more_info_body() {
    // `_ATTRIBUTES_BODY_IS_MORE_INFO_BODY` covers this at compile time;
    // exercising the impl through a trait method here proves the
    // assertion is observable at runtime too.
    fn assert_impl<T: MoreInfoBody>() {}
    assert_impl::<AttributesBody>();
}

// ---------------------------------------------------------------------------
// Doc-comment grep — Phase 6 forward-compat constraint visibility
// ---------------------------------------------------------------------------

#[test]
fn more_info_body_doc_comment_names_object_safety_constraint() {
    // Read the production source and verify the trait-level doc-comment
    // literally states the object-safety + Phase 6 constraint per
    // locked_decisions.more_info_modal. Opencode review 2026-04-28
    // flagged compile-time tests alone as insufficient — Phase 6
    // maintainers need the constraint visible at the trait definition.
    let src = include_str!("../../src/ui/more_info.rs");

    // Walk lines and capture the contiguous doc-comment block
    // immediately preceding `pub trait MoreInfoBody`.
    let mut doc_block = String::new();
    let mut found_trait = false;
    for line in src.lines() {
        if line.starts_with("pub trait MoreInfoBody") {
            found_trait = true;
            break;
        }
        let trimmed = line.trim_start();
        if let Some(stripped) = trimmed.strip_prefix("///") {
            doc_block.push_str(stripped);
            doc_block.push('\n');
        } else if !trimmed.is_empty() && !trimmed.starts_with("//") {
            // A non-doc, non-comment line resets the candidate block
            // (we only count the IMMEDIATELY-preceding block).
            doc_block.clear();
        }
    }

    assert!(
        found_trait,
        "did not find `pub trait MoreInfoBody` declaration in src/ui/more_info.rs"
    );
    assert!(
        doc_block.contains("Object-safe") || doc_block.contains("object-safe"),
        "trait-level doc-comment must mention object-safety constraint, got:\n{doc_block}"
    );
    assert!(
        doc_block.contains("dyn MoreInfoBody"),
        "trait-level doc-comment must reference `dyn MoreInfoBody` to make the constraint searchable"
    );
    assert!(
        doc_block.contains("Phase 6"),
        "trait-level doc-comment must reference Phase 6 forward-compat constraint"
    );
    assert!(
        doc_block.contains("type-erased adapters") || doc_block.contains("not generics"),
        "trait-level doc-comment must spell out the no-generics constraint"
    );
    assert!(
        doc_block.contains("locked_decisions.more_info_modal")
            || doc_block.contains("more_info_modal"),
        "trait-level doc-comment must cite locked_decisions.more_info_modal so future maintainers can find the source of truth"
    );
}

// ---------------------------------------------------------------------------
// 32-attribute cap
// ---------------------------------------------------------------------------

#[test]
fn attributes_body_caps_rendering_at_32_rows_for_100_attribute_entity() {
    let mut attrs = Map::new();
    for i in 0..100 {
        attrs.insert(format!("attr_{i:03}"), json!(format!("value_{i}")));
    }
    let entity = entity_with_attrs("light.kitchen", attrs);

    let rows = AttributesBody::new().render_rows(&entity);
    assert_eq!(rows.len(), MAX_ATTRIBUTE_ROWS);
    // The 32 rendered rows must be the alphabetically-first 32, since
    // sort is load-bearing for cap determinism. attr_000..attr_031.
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.key, format!("attr_{i:03}"));
    }
}

#[test]
fn attributes_body_under_cap_renders_all_rows() {
    let mut attrs = Map::new();
    for i in 0..5 {
        attrs.insert(format!("k_{i}"), json!(format!("v_{i}")));
    }
    let entity = entity_with_attrs("light.kitchen", attrs);
    let rows = AttributesBody::new().render_rows(&entity);
    assert_eq!(rows.len(), 5);
}

#[test]
fn attributes_body_empty_attrs_renders_zero_rows() {
    let entity = entity_with_attrs("light.kitchen", Map::new());
    let rows = AttributesBody::new().render_rows(&entity);
    assert!(rows.is_empty());
}

// ---------------------------------------------------------------------------
// 256-char value truncation
// ---------------------------------------------------------------------------

#[test]
fn attributes_body_truncates_long_string_value_to_256_chars() {
    let mut attrs = Map::new();
    attrs.insert("payload".to_owned(), json!("a".repeat(1024)));
    let entity = entity_with_attrs("light.kitchen", attrs);

    let rows = AttributesBody::new().render_rows(&entity);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].value.chars().count(), MAX_VALUE_CHARS);
    assert!(rows[0].value.chars().all(|c| c == 'a'));
}

#[test]
fn attributes_body_short_value_is_not_padded_or_extended() {
    let mut attrs = Map::new();
    attrs.insert("short".to_owned(), json!("hi"));
    let entity = entity_with_attrs("light.kitchen", attrs);
    let rows = AttributesBody::new().render_rows(&entity);
    assert_eq!(rows[0].value, "hi");
}

#[test]
fn attributes_body_truncation_respects_unicode_codepoints() {
    // 4-byte char × 1000 chars = 4000 bytes. The cap is on chars,
    // not bytes — truncation must yield exactly MAX_VALUE_CHARS chars
    // and a valid UTF-8 string (no mid-codepoint slice).
    let mut attrs = Map::new();
    attrs.insert("crab".to_owned(), json!("🦀".repeat(1000)));
    let entity = entity_with_attrs("light.kitchen", attrs);

    let rows = AttributesBody::new().render_rows(&entity);
    assert_eq!(rows[0].value.chars().count(), MAX_VALUE_CHARS);
    assert!(rows[0].value.chars().all(|c| c == '🦀'));
}

// ---------------------------------------------------------------------------
// Typed display formatter — rejects raw `Value::to_string()` pattern
// ---------------------------------------------------------------------------

#[test]
fn formatter_renders_string_without_json_quotes_or_escapes() {
    // `serde_json::Value::to_string` on a JSON string returns
    // `"\"foo\""` — including surrounding quotes. The typed formatter
    // emits the bare string. This is the load-bearing check for "no
    // raw to_string of arbitrary Value" per locked_decisions.more_info_modal.
    let mut attrs = Map::new();
    attrs.insert("name".to_owned(), json!("Kitchen Light"));
    let entity = entity_with_attrs("light.kitchen", attrs);

    let rows = AttributesBody::new().render_rows(&entity);
    let v = rows[0].value.as_str();
    assert_eq!(v, "Kitchen Light");
    assert!(
        !v.starts_with('"'),
        "typed formatter must not surround string with JSON quotes"
    );
}

#[test]
fn formatter_strips_json_escapes_from_strings_with_embedded_quotes() {
    let mut attrs = Map::new();
    attrs.insert("quote".to_owned(), json!("he said \"hi\""));
    let entity = entity_with_attrs("light.kitchen", attrs);

    let rows = AttributesBody::new().render_rows(&entity);
    let v = rows[0].value.as_str();
    assert_eq!(v, "he said \"hi\"");
    assert!(
        !v.contains("\\\""),
        "typed formatter must not produce JSON-escaped inner quotes"
    );
}

#[test]
fn formatter_renders_bool_as_word_token() {
    let mut attrs = Map::new();
    attrs.insert("flag_t".to_owned(), json!(true));
    attrs.insert("flag_f".to_owned(), json!(false));
    let entity = entity_with_attrs("light.kitchen", attrs);

    let rows = AttributesBody::new().render_rows(&entity);
    let by_key: std::collections::HashMap<_, _> = rows
        .iter()
        .map(|r| (r.key.as_str(), r.value.as_str()))
        .collect();
    assert_eq!(by_key.get("flag_t"), Some(&"true"));
    assert_eq!(by_key.get("flag_f"), Some(&"false"));
}

#[test]
fn formatter_renders_integer_as_decimal_digits() {
    let mut attrs = Map::new();
    attrs.insert("brightness".to_owned(), json!(180));
    let entity = entity_with_attrs("light.kitchen", attrs);

    let rows = AttributesBody::new().render_rows(&entity);
    assert_eq!(rows[0].value, "180");
}

#[test]
fn formatter_renders_float_via_rust_display_not_json() {
    let mut attrs = Map::new();
    attrs.insert("temperature".to_owned(), json!(21.5));
    let entity = entity_with_attrs("sensor.temp", attrs);
    let rows = AttributesBody::new().render_rows(&entity);
    // Rust's Display for f64 emits "21.5"; serde_json::to_string on a
    // float also happens to emit "21.5", but the load-bearing
    // assertion is that we did not emit JSON-syntactic decoration
    // around it.
    assert_eq!(rows[0].value, "21.5");
}

#[test]
fn formatter_renders_null_as_null_token() {
    let mut attrs = Map::new();
    attrs.insert("missing".to_owned(), Value::Null);
    let entity = entity_with_attrs("sensor.x", attrs);
    let rows = AttributesBody::new().render_rows(&entity);
    assert_eq!(rows[0].value, "null");
}

#[test]
fn formatter_renders_array_as_bounded_summary_not_recursive_dump() {
    let mut attrs = Map::new();
    attrs.insert("items".to_owned(), json!(["a", "b", "c"]));
    let entity = entity_with_attrs("sensor.x", attrs);
    let rows = AttributesBody::new().render_rows(&entity);
    assert_eq!(rows[0].value, "[3 items]");
    assert!(
        !rows[0].value.contains('"'),
        "array summary must not contain a JSON-encoded element list"
    );
}

#[test]
fn formatter_renders_object_as_bounded_summary_not_recursive_dump() {
    let mut nested = Map::new();
    nested.insert("a".to_owned(), json!(1));
    nested.insert("b".to_owned(), json!(2));
    let mut attrs = Map::new();
    attrs.insert("nested".to_owned(), Value::Object(nested));
    let entity = entity_with_attrs("sensor.x", attrs);
    let rows = AttributesBody::new().render_rows(&entity);
    assert_eq!(rows[0].value, "{2 keys}");
}

// ---------------------------------------------------------------------------
// Lazy-render contract
// ---------------------------------------------------------------------------

/// Test body that increments a counter on every `render_rows` call.
struct CountingBody {
    calls: AtomicUsize,
}

impl CountingBody {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl MoreInfoBody for CountingBody {
    fn render_rows(&self, _entity: &Entity) -> Vec<ModalRow> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        vec![ModalRow {
            key: "count".to_owned(),
            value: "row".to_owned(),
        }]
    }
}

#[test]
fn modal_state_starts_closed() {
    let state = ModalState::new(Arc::new(AttributesBody::new()));
    assert!(!state.is_open());
    assert!(state.rows().is_empty());
    assert!(state.open_for().is_none());
}

#[test]
fn open_with_invokes_row_builder_exactly_once() {
    let body = Arc::new(CountingBody::new());
    let mut state = ModalState::new(body.clone());
    let entity = entity_with_attrs("light.kitchen", Map::new());

    state.open_with(&entity);
    assert_eq!(body.count(), 1);
    assert!(state.is_open());
    assert_eq!(state.rows().len(), 1);
    assert_eq!(state.open_for(), Some(&EntityId::from("light.kitchen")));
}

#[test]
fn row_builder_is_not_invoked_on_entity_update_while_open() {
    // Load-bearing locked-decision assertion: while the modal is open,
    // entity ticks must NOT recompute rows.
    let body = Arc::new(CountingBody::new());
    let mut state = ModalState::new(body.clone());
    let entity = entity_with_attrs("light.kitchen", Map::new());

    state.open_with(&entity);
    assert_eq!(body.count(), 1);

    for _ in 0..50 {
        state.on_entity_update(&entity);
    }
    assert_eq!(
        body.count(),
        1,
        "entity updates while open must NOT invoke render_rows (locked_decisions.more_info_modal lazy-render)"
    );
}

#[test]
fn reopen_recomputes_against_current_entity_attributes() {
    let mut state = ModalState::new(Arc::new(AttributesBody::new()));

    let mut attrs1 = Map::new();
    attrs1.insert("brightness".to_owned(), json!("low"));
    let e1 = entity_with_attrs("light.kitchen", attrs1);

    state.open_with(&e1);
    let rows_first = state.rows().to_vec();
    assert_eq!(rows_first.len(), 1);
    assert_eq!(rows_first[0].value, "low");

    state.close();
    assert!(!state.is_open());
    assert!(state.rows().is_empty());

    let mut attrs2 = Map::new();
    attrs2.insert("brightness".to_owned(), json!("high"));
    let e2 = entity_with_attrs("light.kitchen", attrs2);

    state.open_with(&e2);
    let rows_second = state.rows().to_vec();
    assert_eq!(rows_second.len(), 1);
    assert_eq!(rows_second[0].value, "high");
    assert_ne!(rows_first, rows_second);
}

#[test]
fn close_clears_rows_and_open_for() {
    let mut state = ModalState::new(Arc::new(AttributesBody::new()));
    let mut attrs = Map::new();
    attrs.insert("k".to_owned(), json!("v"));
    let entity = entity_with_attrs("light.kitchen", attrs);

    state.open_with(&entity);
    assert!(state.is_open());

    state.close();
    assert!(!state.is_open());
    assert!(state.rows().is_empty());
    assert!(state.open_for().is_none());
}

// ---------------------------------------------------------------------------
// ModalHeader::from_entity — friendly_name fallback policy
// ---------------------------------------------------------------------------

#[test]
fn modal_header_uses_friendly_name_when_present() {
    let mut attrs = Map::new();
    attrs.insert("friendly_name".to_owned(), json!("Kitchen"));
    let entity = entity_with_attrs("light.kitchen", attrs);
    let header = ModalHeader::from_entity(&entity);
    assert_eq!(header.name, "Kitchen");
    assert_eq!(header.state, "on");
}

#[test]
fn modal_header_falls_back_to_entity_id_when_friendly_name_missing() {
    let entity = entity_with_attrs("light.kitchen", Map::new());
    let header = ModalHeader::from_entity(&entity);
    assert_eq!(header.name, "light.kitchen");
}

// ---------------------------------------------------------------------------
// Slint render integration (TASK-074 harness consumed)
// ---------------------------------------------------------------------------

#[test]
fn modal_overlay_renders_and_visible_state_changes_pixel_buffer() {
    // Acceptance: modal renders without panic; toggling
    // `more-info-visible` between captures produces different pixel
    // buffers (proves the overlay actually composites with body rows
    // visible). This is the Slint-side render integration acceptance.
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    // Build a body and an open ModalState with two attribute rows.
    let body = Arc::new(AttributesBody::new());
    let mut state = ModalState::new(body);
    let mut attrs = Map::new();
    attrs.insert("friendly_name".to_owned(), json!("Kitchen Light"));
    attrs.insert("brightness".to_owned(), json!(180));
    let entity = entity_with_attrs("light.kitchen", attrs);
    state.open_with(&entity);
    let header = ModalHeader::from_entity(&entity);

    // BEFORE: modal closed.
    window.set_more_info_visible(false);
    let frame_closed = harness
        .render_component(&window, 480, 600)
        .expect("render closed-modal frame");

    // AFTER: open the modal via the documented bridge helper.
    apply_modal_to_window(&state, &header, &window);
    let frame_open = harness
        .render_component(&window, 480, 600)
        .expect("render open-modal frame");

    assert_eq!(
        frame_closed.pixels.len(),
        frame_open.pixels.len(),
        "frames must be the same dimensions"
    );
    assert!(
        frame_open.has_non_zero_byte(),
        "open-modal frame must have non-zero pixels"
    );
    assert_ne!(
        frame_closed.pixels, frame_open.pixels,
        "toggling more-info-visible must produce a different pixel buffer (proves overlay actually composites)"
    );
}

#[test]
fn apply_modal_to_window_writes_visibility_from_modal_state() {
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    // Closed state → visibility must be false on the window.
    let closed = ModalState::new(Arc::new(AttributesBody::new()));
    let header = ModalHeader {
        name: "x".to_owned(),
        state: "y".to_owned(),
    };
    apply_modal_to_window(&closed, &header, &window);
    assert!(!window.get_more_info_visible());

    // Render once so the harness exercises the property write at the
    // platform level (not just the Rust property bag).
    let _ = harness
        .render_component(&window, 480, 600)
        .expect("render after close");

    // Open state → visibility must be true.
    let body = Arc::new(AttributesBody::new());
    let mut open = ModalState::new(body);
    let entity = entity_with_attrs("light.kitchen", Map::new());
    open.open_with(&entity);
    apply_modal_to_window(&open, &header, &window);
    assert!(window.get_more_info_visible());
}

#[test]
fn apply_modal_to_window_writes_row_count_to_property() {
    let mut harness = HeadlessRenderer::new().expect("install headless platform");
    let window = MainWindow::new().expect("instantiate MainWindow");

    let mut attrs = Map::new();
    for i in 0..5 {
        attrs.insert(format!("k_{i}"), json!(format!("v_{i}")));
    }
    let entity = entity_with_attrs("light.kitchen", attrs);

    let body = Arc::new(AttributesBody::new());
    let mut state = ModalState::new(body);
    state.open_with(&entity);
    let header = ModalHeader::from_entity(&entity);
    apply_modal_to_window(&state, &header, &window);

    // Render and assert the visible state matches.
    let _ = harness
        .render_component(&window, 480, 600)
        .expect("render with rows");

    // Slint exposes ModelRc<ModalRowVM> — we read it back via the
    // generated getter and assert its len. The Slint-typed model
    // method `row_count` is the documented accessor.
    let rows = window.get_more_info_rows();
    use slint::Model as _;
    assert_eq!(rows.row_count(), 5);
}
