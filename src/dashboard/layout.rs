//! Row-major first-fit layout packer for dashboard sections.
//!
//! # Algorithm (locked: `locked_decisions.layout_algorithm`)
//!
//! The packer turns a flat list of widgets into a `Vec<PositionedWidget>`
//! describing each widget's top-left grid cell and its column/row span.
//!
//! Placement rules (executed in config order — no sorting):
//! 1. `span_cols = widget.layout.preferred_columns`.
//!    The packer is **only called after the validator (TASK-083) confirms no
//!    span overflow** (`preferred_columns ≤ grid_columns`). Callers MUST
//!    validate first; the packer's behaviour on Error-level input is undefined.
//!    A `debug_assert!` documents the precondition but is not a runtime guard.
//! 2. `span_rows = widget.layout.preferred_rows` (TASK-078 returned GREEN;
//!    full multi-row span semantics apply — no kill-switch clamping).
//! 3. Find the leftmost-topmost free cell range of size `span_cols × span_rows`
//!    in the current row-major grid. If no range fits in the current row,
//!    advance to the next row (wrap).
//! 4. Mark those cells occupied; record `PositionedWidget`.
//!
//! # Span prototype kill-switch (TASK-078 verdict: GREEN)
//!
//! `locked_decisions.span_prototype_kill_switch` verdict consumed: **GREEN**.
//! The prototype demonstrated all three required behaviors:
//! - A widget spanning 3 columns in a 4-column grid renders at the correct
//!   visual width.
//! - A widget spanning 2 rows and 2 columns does not collapse to a single row.
//! - Nested section grids honor their own column counts independently.
//!
//! Therefore `preferred_rows` is honored fully — `span_rows = preferred_rows`
//! with no clamping. The View (TASK-085) renders spans via
//! `HorizontalLayout + horizontal-stretch:N` / `VerticalLayout + vertical-stretch:N`.
//!
//! # Cache contract (`locked_decisions.layout_algorithm`)
//!
//! `pack` runs ONCE per `Dashboard` load (called by the loader after validation
//! passes). The resulting `Vec<PositionedWidget>` is cached per section and
//! reused for every render. No per-frame layout work. The cache is recomputed
//! only on the next load (restart-only per `locked_decisions.hot_reload_posture`).
//!
//! # Determinism contract
//!
//! Identical input → byte-identical output across runs. Guaranteed by:
//! - Config-order widget iteration (no sorting, no HashMap).
//! - Pure function: no global state, no RNG, no I/O.
//! - `u8`/`u16` arithmetic with explicit overflow behaviour.
//!
//! # Parent plan
//!
//! `docs/plans/2026-04-29-phase-4-layout.md` — relevant decisions:
//! `layout_algorithm`, `span_prototype_kill_switch`, `hot_reload_posture`.

use crate::dashboard::schema::Widget;

// ---------------------------------------------------------------------------
// PositionedWidget
// ---------------------------------------------------------------------------

/// A widget after the packer has assigned it a grid cell.
///
/// Produced by [`pack`]; consumed by the Slint `View` component (TASK-085).
///
/// # Fields
///
/// - `widget_id`: the `Widget.id` string from the YAML config.
/// - `col`: zero-based column index of the top-left cell.
/// - `row`: zero-based row index of the top-left cell.
/// - `span_cols`: number of columns occupied.
/// - `span_rows`: number of rows occupied.
///
/// `row` is `u16` because tall sections can have many auto-generated rows;
/// `span_cols` and `span_rows` are `u8` because they are bounded by the
/// `preferred_columns`/`preferred_rows` schema fields (both `u8`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionedWidget {
    /// Stable identifier matching `Widget.id` from the YAML config.
    pub widget_id: String,
    /// Zero-based column index of the widget's top-left cell.
    pub col: u8,
    /// Zero-based row index of the widget's top-left cell.
    ///
    /// `u16` accommodates tall sections with many auto-generated rows; in
    /// practice Phase 4 sections are bounded by `DeviceProfile.max_widgets_per_view`.
    pub row: u16,
    /// Number of grid columns the widget occupies.
    pub span_cols: u8,
    /// Number of grid rows the widget occupies.
    pub span_rows: u8,
}

// ---------------------------------------------------------------------------
// Grid occupancy map (internal)
// ---------------------------------------------------------------------------

/// Sparse occupancy map for the packer's first-fit search.
///
/// Each element of `occupied` is an `(col, row)` pair. We use a `Vec` of
/// occupied cells rather than a 2-D array because the grid height is
/// unbounded at pack time (widgets wrap to new rows dynamically). Lookup is
/// `O(occupied × span_cols × span_rows)` — acceptable for the typical
/// dashboard widget counts (≤ 64 per `DeviceProfile.max_widgets_per_view`).
struct OccupancyMap {
    /// Set of occupied `(col, row)` cells.
    occupied: Vec<(u8, u16)>,
    /// Width of the grid in columns.
    columns: u8,
}

impl OccupancyMap {
    fn new(columns: u8) -> Self {
        Self {
            occupied: Vec::new(),
            columns,
        }
    }

    /// Returns `true` if all cells in the `span_cols × span_rows` rectangle
    /// whose top-left corner is `(start_col, start_row)` are free.
    fn is_range_free(&self, start_col: u8, start_row: u16, span_cols: u8, span_rows: u8) -> bool {
        for r in 0..u16::from(span_rows) {
            for c in 0..span_cols {
                if self.occupied.contains(&(start_col + c, start_row + r)) {
                    return false;
                }
            }
        }
        true
    }

    /// Marks all cells in the `span_cols × span_rows` rectangle as occupied.
    fn mark_occupied(&mut self, start_col: u8, start_row: u16, span_cols: u8, span_rows: u8) {
        for r in 0..u16::from(span_rows) {
            for c in 0..span_cols {
                self.occupied.push((start_col + c, start_row + r));
            }
        }
    }

    /// Finds the leftmost-topmost free cell range of size `span_cols × span_rows`.
    ///
    /// Scans row-major (left to right, top to bottom), wrapping to the next
    /// row when the remaining columns in the current row are insufficient to
    /// hold `span_cols`.
    ///
    /// Returns `(col, row)` of the top-left corner of the first fitting range.
    /// Panics are not possible here because the grid rows are unbounded
    /// (we generate new rows on demand).
    fn find_placement(&self, span_cols: u8, span_rows: u8) -> (u8, u16) {
        let mut row: u16 = 0;
        loop {
            let mut col: u8 = 0;
            while col + span_cols <= self.columns {
                if self.is_range_free(col, row, span_cols, span_rows) {
                    return (col, row);
                }
                col += 1;
            }
            // No fit in this row; try the next.
            row += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// pack
// ---------------------------------------------------------------------------

/// Pack a flat widget list into a `Vec<PositionedWidget>` using the
/// row-major first-fit algorithm.
///
/// # Parameters
///
/// - `widgets`: ordered slice of widgets in **config order**. The packer
///   places them in the order they arrive — no sorting occurs.
/// - `grid_columns`: the number of columns declared in `section.grid.columns`.
///   The caller (loader, TASK-082) supplies this from `Section.grid.columns`
///   once that field is added to `Section` by the TASK-083 additive schema
///   change. For now, callers supply it explicitly.
///
/// # Preconditions
///
/// - All `widget.layout.preferred_columns ≤ grid_columns` (the validator
///   (TASK-083) enforces this; `SpanOverflow` is an Error that halts load).
/// - `grid_columns ≥ 1`.
///
/// Both preconditions are asserted via `debug_assert!` for documentation and
/// debug-build checking; they are NOT runtime-guarded in release builds
/// (callers MUST validate first).
///
/// # Returns
///
/// `Vec<PositionedWidget>` in the same order as `widgets` (config order).
/// The result is byte-identical for identical input across repeated calls.
///
/// # Complexity
///
/// `O(n² × max_span)` where `n` is `widgets.len()`. Acceptable for Phase 4
/// section sizes (≤ 64 widgets per `DeviceProfile.max_widgets_per_view`).
pub fn pack(widgets: &[Widget], grid_columns: u8) -> Vec<PositionedWidget> {
    debug_assert!(grid_columns >= 1, "grid_columns must be ≥ 1");

    let mut map = OccupancyMap::new(grid_columns);
    let mut result = Vec::with_capacity(widgets.len());

    for widget in widgets.iter() {
        let span_cols = widget.layout.preferred_columns;
        let span_rows = widget.layout.preferred_rows;

        // Precondition: the validator (TASK-083) rejects span overflow before
        // the packer is ever called. This debug_assert documents the contract
        // but is intentionally not a runtime guard in release builds.
        debug_assert!(
            span_cols <= grid_columns,
            "widget '{}' preferred_columns ({span_cols}) > grid_columns ({grid_columns}); \
             validator must reject this before calling pack",
            widget.id
        );
        // span_rows ≥ 1 is the minimum sensible value; 0 would produce an
        // empty rectangle. The schema type is u8, so 0 is technically valid
        // YAML but semantically nonsense. Assert for documentation.
        debug_assert!(
            span_rows >= 1,
            "widget '{}' preferred_rows must be ≥ 1",
            widget.id
        );

        let (col, row) = map.find_placement(span_cols, span_rows);
        map.mark_occupied(col, row, span_cols, span_rows);

        result.push(PositionedWidget {
            widget_id: widget.id.clone(),
            col,
            row,
            span_cols,
            span_rows,
        });
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::schema::{Widget, WidgetKind, WidgetLayout};

    /// Build a minimal `Widget` with the given id suffix, preferred_columns,
    /// and preferred_rows. All other fields are irrelevant to the packer.
    fn make_widget(id: &str, preferred_columns: u8, preferred_rows: u8) -> Widget {
        Widget {
            id: id.to_string(),
            widget_type: WidgetKind::LightTile,
            entity: None,
            entities: vec![],
            name: None,
            icon: None,
            tap_action: None,
            hold_action: None,
            double_tap_action: None,
            layout: WidgetLayout {
                preferred_columns,
                preferred_rows,
            },
            options: None,
            placement: None,
            visibility: "always".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // Required acceptance-criteria tests
    // -----------------------------------------------------------------------

    /// `pack_same_section_twice_is_byte_equal`: packing the same widget list
    /// twice must produce the exact same `Vec<PositionedWidget>` (byte-equal
    /// via `==` on the full vec, not just length). This is the type-level
    /// determinism gate.
    #[test]
    fn pack_same_section_twice_is_byte_equal() {
        let widgets = vec![
            make_widget("w1", 2, 1),
            make_widget("w2", 2, 1),
            make_widget("w3", 1, 2),
        ];
        let first = pack(&widgets, 4);
        let second = pack(&widgets, 4);
        assert_eq!(
            first, second,
            "packer output must be byte-identical for identical input"
        );
    }

    /// `pack_preserves_config_order`: the resulting `PositionedWidget` list
    /// is in W1, W2, W3 order — no sorting occurs.
    #[test]
    fn pack_preserves_config_order() {
        let widgets = vec![
            make_widget("w1", 1, 1),
            make_widget("w2", 1, 1),
            make_widget("w3", 1, 1),
        ];
        let result = pack(&widgets, 4);
        assert_eq!(result.len(), 3);
        // Widget IDs must match config order (the string IDs from the input).
        assert_eq!(result[0].widget_id, "w1", "first result must be w1");
        assert_eq!(result[1].widget_id, "w2", "second result must be w2");
        assert_eq!(result[2].widget_id, "w3", "third result must be w3");
        // Also assert columns increase left-to-right for 1-wide widgets.
        assert_eq!(result[0].col, 0);
        assert_eq!(result[1].col, 1);
        assert_eq!(result[2].col, 2);
    }

    /// `pack_wraps_to_next_row_when_no_fit`: 3-wide widgets in a 4-col grid.
    /// Widget 1 → (col=0, row=0). Row 0 has only 1 free column after widget 1,
    /// which is too narrow for widget 2 (span_cols=3). Widget 2 wraps to
    /// (col=0, row=1). Widget 3 wraps to (col=0, row=2).
    #[test]
    fn pack_wraps_to_next_row_when_no_fit() {
        let widgets = vec![
            make_widget("w1", 3, 1),
            make_widget("w2", 3, 1),
            make_widget("w3", 3, 1),
        ];
        let result = pack(&widgets, 4);
        assert_eq!(result.len(), 3);
        assert_eq!(
            (result[0].col, result[0].row),
            (0, 0),
            "w1 must be at (0, 0)"
        );
        assert_eq!(
            (result[1].col, result[1].row),
            (0, 1),
            "w2 must wrap to (0, 1)"
        );
        assert_eq!(
            (result[2].col, result[2].row),
            (0, 2),
            "w3 must wrap to (0, 2)"
        );
    }

    /// `pack_empty_section_is_empty_vec`: packing an empty widget list returns
    /// `Vec::new()`.
    #[test]
    fn pack_empty_section_is_empty_vec() {
        let result = pack(&[], 4);
        assert!(
            result.is_empty(),
            "empty widget list must produce empty vec"
        );
    }

    // -----------------------------------------------------------------------
    // Additional implementation-guidance tests (from task description)
    // -----------------------------------------------------------------------

    /// `pack_single_widget_at_origin`: 1 widget, 4-col grid →
    /// `[{col:0, row:0, span_cols:1, span_rows:1}]`.
    #[test]
    fn pack_single_widget_at_origin() {
        let widgets = vec![make_widget("w1", 1, 1)];
        let result = pack(&widgets, 4);
        assert_eq!(result.len(), 1);
        let pw = &result[0];
        assert_eq!(pw.col, 0);
        assert_eq!(pw.row, 0);
        assert_eq!(pw.span_cols, 1);
        assert_eq!(pw.span_rows, 1);
    }

    /// `pack_two_widgets_pack_horizontally`: 2 widgets, 4-col grid, second
    /// placed at col=1 (immediately right of the first).
    #[test]
    fn pack_two_widgets_pack_horizontally() {
        let widgets = vec![make_widget("w1", 1, 1), make_widget("w2", 1, 1)];
        let result = pack(&widgets, 4);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].col, 0);
        assert_eq!(result[1].col, 1);
        assert_eq!(result[0].row, 0);
        assert_eq!(result[1].row, 0);
    }

    /// `pack_overflow_wraps_to_next_row`: 5 1-wide widgets in a 4-col grid.
    /// First 4 fill row 0; fifth wraps to row=1, col=0.
    #[test]
    fn pack_overflow_wraps_to_next_row() {
        let widgets: Vec<Widget> = (1..=5)
            .map(|i| make_widget(&format!("w{i}"), 1, 1))
            .collect();
        let result = pack(&widgets, 4);
        assert_eq!(result.len(), 5);
        for (i, pw) in result.iter().enumerate().take(4) {
            assert_eq!(pw.col, i as u8, "widget {i} must be in col {i}");
            assert_eq!(pw.row, 0, "widget {i} must be in row 0");
        }
        assert_eq!(result[4].col, 0, "widget 4 must wrap to col 0");
        assert_eq!(result[4].row, 1, "widget 4 must wrap to row 1");
    }

    /// `pack_honors_colspan`: widget with `preferred_columns: 3`, 4-col grid
    /// → `span_cols: 3`; the next widget is placed at col=3 (only 1 free
    /// cell remaining in that row).
    #[test]
    fn pack_honors_colspan() {
        let widgets = vec![make_widget("w1", 3, 1), make_widget("w2", 1, 1)];
        let result = pack(&widgets, 4);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].span_cols, 3);
        assert_eq!(result[0].col, 0);
        assert_eq!(result[0].row, 0);
        assert_eq!(
            result[1].col, 3,
            "w2 must be placed at col=3 (the only free cell)"
        );
        assert_eq!(result[1].row, 0);
        assert_eq!(result[1].span_cols, 1);
    }

    /// `pack_honors_rowspan`: widget with `preferred_rows: 2` reserves 2 rows;
    /// next widget placed below (not beside) the tall widget.
    #[test]
    fn pack_honors_rowspan() {
        // 1-col grid so we can assert the vertical placement easily.
        let widgets = vec![make_widget("w1", 1, 2), make_widget("w2", 1, 1)];
        let result = pack(&widgets, 1);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].col, 0);
        assert_eq!(result[0].row, 0);
        assert_eq!(result[0].span_rows, 2);
        // w2 must start at row=2 because rows 0 and 1 are occupied by w1.
        assert_eq!(result[1].col, 0);
        assert_eq!(result[1].row, 2, "w2 must be placed below the 2-row widget");
        assert_eq!(result[1].span_rows, 1);
    }

    /// `pack_first_fit_finds_leftmost_topmost`: 3-col grid, widget 1 is 2-wide,
    /// widget 2 is 1-wide → widget 2 placed at col=2 (leftmost free cell in
    /// the row, not a new row).
    #[test]
    fn pack_first_fit_finds_leftmost_topmost() {
        let widgets = vec![make_widget("w1", 2, 1), make_widget("w2", 1, 1)];
        let result = pack(&widgets, 3);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].col, 0);
        assert_eq!(result[0].row, 0);
        assert_eq!(result[0].span_cols, 2);
        assert_eq!(
            result[1].col, 2,
            "w2 must be placed at col=2, not a new row"
        );
        assert_eq!(result[1].row, 0);
        assert_eq!(result[1].span_cols, 1);
    }

    /// `pack_is_deterministic_byte_identical`: pack the same section twice with
    /// deliberately rebuilt input structs; assert results are `==`.
    #[test]
    fn pack_is_deterministic_byte_identical() {
        // Build the same input twice from scratch to rule out any shared state.
        let build_widgets = || -> Vec<Widget> {
            vec![
                make_widget("alpha", 2, 1),
                make_widget("beta", 1, 2),
                make_widget("gamma", 3, 1),
                make_widget("delta", 1, 1),
            ]
        };
        let first = pack(&build_widgets(), 4);
        let second = pack(&build_widgets(), 4);
        assert_eq!(
            first, second,
            "two independent packer runs on identical input must produce == output"
        );
    }

    /// Rowspan across adjacent columns: two side-by-side tall widgets do not
    /// collide; the packer tracks all occupied cells.
    #[test]
    fn pack_two_adjacent_tall_widgets_do_not_collide() {
        // 2-col grid; w1 = 1×2, w2 = 1×2; w3 = 1×1 should land at row=2.
        let widgets = vec![
            make_widget("w1", 1, 2),
            make_widget("w2", 1, 2),
            make_widget("w3", 1, 1),
        ];
        let result = pack(&widgets, 2);
        assert_eq!(result.len(), 3);
        // w1 at (0, 0), w2 at (1, 0).
        assert_eq!((result[0].col, result[0].row), (0, 0));
        assert_eq!((result[1].col, result[1].row), (1, 0));
        // Both tall widgets occupy rows 0 and 1; w3 must wrap to row=2.
        assert_eq!(
            result[2].row, 2,
            "w3 must not collide with tall widget rows"
        );
    }

    /// Rowspan interleaving: a narrow widget fills the gap beside a tall widget.
    /// 3-col grid; w1 = 2×2 (occupies cols 0-1, rows 0-1); w2 = 1×1 should
    /// fit at (col=2, row=0), not below.
    #[test]
    fn pack_narrow_widget_fills_gap_beside_tall_widget() {
        let widgets = vec![make_widget("w1", 2, 2), make_widget("w2", 1, 1)];
        let result = pack(&widgets, 3);
        assert_eq!(result.len(), 2);
        assert_eq!((result[0].col, result[0].row), (0, 0));
        assert_eq!(result[0].span_cols, 2);
        assert_eq!(result[0].span_rows, 2);
        // w2 is 1-wide and 1-tall; the first free cell is (col=2, row=0).
        assert_eq!(
            (result[1].col, result[1].row),
            (2, 0),
            "w2 must fit beside the tall widget"
        );
        assert_eq!(result[1].span_cols, 1);
    }
}
