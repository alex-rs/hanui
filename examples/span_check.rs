// examples/span_check.rs — Phase 4 kill-switch harness for proportional span layout.
//
// # Purpose
//
// Confirms that Slint can express the three proportional-layout behaviors
// required by Phase 4's `preferred_columns` / `preferred_rows` schema
// (docs/plans/2026-04-29-phase-4-layout.md,
// `locked_decisions.span_prototype_kill_switch`). This binary is the GATE
// for Phase 4 Wave 1: if any of the three behaviors fails, the harness
// prints `PROTOTYPE_VERDICT: SCOPE_REDUCE` and exits with code 1, surfacing
// the dead-end before TASK-079 locks the schema.
//
// # Key finding documented in examples/span_check.slint
//
// Slint's `GridLayout` with `colspan`/`rowspan` does NOT give proportional
// width/height based on span count. The correct Slint mechanism for
// proportional column/row spans is:
//   - HorizontalLayout + `horizontal-stretch: N` for column proportions.
//   - VerticalLayout + `vertical-stretch: N` for row proportions.
// The harness tests these mechanisms, which are what the Phase 4 packer
// MUST use when emitting Slint layout expressions.
//
// # Test geometry (see span_check.slint for full geometry rationale)
//
//   Window: 400×600 physical pixels.
//
//   Behavior 1 (y=0, h=100px):
//     HorizontalLayout: b1_widget (h-stretch:3) + filler (h-stretch:1).
//     b1_widget.width == 3/4 × 400 = 300px ± 1px.
//
//   Behavior 2 (y=100px, h=400px):
//     VerticalLayout: b2_widget (v-stretch:2) + 2 × filler (v-stretch:1).
//     b2_widget.height == 2/4 × 400 = 200px ± 1px.
//
//   Behavior 3 (y=500px, h=100px):
//     VerticalLayout holding HLayout-A (4 cols) and HLayout-B (2 cols).
//     b3_widget is first item in HLayout-B: width == 400/2 = 200px ± 1px.
//     Must NOT be 100px (HLayout-A's column unit).
//
// # Dependencies
//
// Only `slint` (already a workspace dep) and the standard library are used.
// No `tokio`, no `serde_yaml_ng`, no Phase 4 modules.
// Verified by `cargo tree --example span_check`.
//
// # Platform setup
//
// Same MinimalSoftwareWindow-based headless platform pattern as
// `tests/common/slint_harness.rs` (TASK-074). No event loop is started.

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
use slint::platform::{Platform, WindowAdapter};
use slint::ComponentHandle;
use slint::PhysicalSize;
use slint::PlatformError;
use std::rc::Rc;

// The Slint compiler generates Rust bindings for `SpanCheckWindow` into
// `OUT_DIR/span_check.rs` via the `build.rs` compilation step (TASK-078).
// We include them via the env-var printed by build.rs, mirroring the
// gesture_test_window pattern in `src/ui/bridge.rs`.
include!(env!("HANUI_SPAN_CHECK_INCLUDE"));

// ── Headless platform ───────────────────────────────────────────────────────
struct HeadlessPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }
}

fn main() {
    // ── Platform setup ──────────────────────────────────────────────────
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(HeadlessPlatform {
        window: window.clone(),
    }))
    .expect("install headless platform");

    // ── Component instantiation ─────────────────────────────────────────
    let component = SpanCheckWindow::new().expect("instantiate SpanCheckWindow");

    // 400×600 physical pixels. Scale factor = 1.0 (MinimalSoftwareWindow
    // default), so 1 logical px == 1 physical px.
    component.window().set_size(PhysicalSize::new(400, 600));

    // Show and trigger a layout + render pass. After draw_if_needed,
    // all element width/height properties are computed.
    component.show().expect("show SpanCheckWindow");
    let mut pixel_buf = vec![Rgb565Pixel::default(); 400 * 600];
    window.draw_if_needed(|renderer| {
        renderer.render(pixel_buf.as_mut_slice(), 400);
    });

    // ── Read layout measurements ────────────────────────────────────────
    // Slint's <length> maps to f32 in Rust bindings (logical pixels).
    let b1_width = component.get_b1_widget_width();
    let b2_height = component.get_b2_widget_height();
    let b3_width = component.get_b3_widget_width();

    component.hide().expect("hide SpanCheckWindow");

    // ── Tolerance ───────────────────────────────────────────────────────
    // ±1px absorbs integer rounding in Slint's layout engine.
    let tol: f32 = 1.0;

    // ── Behavior 1: 3-of-4 column proportion via HorizontalLayout ───────
    // h-stretch:3 out of total 4 → width = 3/4 × 400 = 300px.
    // Mechanism: HorizontalLayout with proportional horizontal-stretch.
    // Implication for packer: emit HorizontalLayout + h-stretch:N, NOT
    // GridLayout colspan (which does not give proportional widths in Slint).
    let b1_expected: f32 = 300.0;
    let b1_pass = (b1_width - b1_expected).abs() <= tol;
    if b1_pass {
        println!(
            "BEHAVIOR_1: PASS — horizontal-stretch:3 in 4-unit HLayout: \
             width={b1_width}px (expected {b1_expected}±{tol}px)"
        );
    } else {
        println!(
            "BEHAVIOR_1: FAIL — horizontal-stretch:3 in 4-unit HLayout: \
             width={b1_width}px (expected {b1_expected}±{tol}px)"
        );
    }

    // ── Behavior 2: 2-of-4 row proportion via VerticalLayout ────────────
    // v-stretch:2 out of total 4 → height = 2/4 × 400 = 200px (exact).
    // Mechanism: VerticalLayout with proportional vertical-stretch.
    // Implication for packer: emit VerticalLayout + v-stretch:N, NOT
    // GridLayout rowspan (which does not give proportional heights in Slint).
    let b2_expected: f32 = 200.0;
    let b2_pass = (b2_height - b2_expected).abs() <= tol;
    if b2_pass {
        println!(
            "BEHAVIOR_2: PASS — vertical-stretch:2 in 4-unit VLayout (400px section): \
             height={b2_height}px (expected {b2_expected}±{tol}px)"
        );
    } else {
        println!(
            "BEHAVIOR_2: FAIL — vertical-stretch:2 in 4-unit VLayout (400px section): \
             height={b2_height}px (expected {b2_expected}±{tol}px)"
        );
    }

    // ── Behavior 3: sibling sections honor independent column counts ─────
    // Section A (4 items) and Section B (2 items) both receive 400px width.
    // b3_widget is the first item of Section B → width = 400/2 = 200px.
    // Must NOT be 100px (which is Section A's column unit).
    let b3_expected: f32 = 200.0;
    let b3_pass = (b3_width - b3_expected).abs() <= tol;
    if b3_pass {
        println!(
            "BEHAVIOR_3: PASS — 2-item sibling section (beside 4-item section): \
             b3_widget={b3_width}px (expected {b3_expected}±{tol}px, \
             NOT 100px from the 4-item section)"
        );
    } else {
        println!(
            "BEHAVIOR_3: FAIL — 2-item sibling section (beside 4-item section): \
             b3_widget={b3_width}px (expected {b3_expected}±{tol}px)"
        );
    }

    // ── Verdict ─────────────────────────────────────────────────────────
    // ALL three must pass for GREEN. Any failure → SCOPE_REDUCE.
    let all_pass = b1_pass && b2_pass && b3_pass;
    if all_pass {
        println!("PROTOTYPE_VERDICT: GREEN");
        std::process::exit(0);
    } else {
        println!("PROTOTYPE_VERDICT: SCOPE_REDUCE");
        if !b1_pass {
            println!(
                "  SCOPE_REDUCE_DETAIL: Behavior 1 failed — \
                 column-proportion mechanism not working; schema trim required"
            );
        }
        if !b2_pass {
            println!(
                "  SCOPE_REDUCE_DETAIL: Behavior 2 failed — \
                 preferred_rows>1 becomes informational; packer always single-row; \
                 span-overflow rule amended per locked_decisions.span_prototype_kill_switch"
            );
        }
        if !b3_pass {
            println!(
                "  SCOPE_REDUCE_DETAIL: Behavior 3 failed — \
                 sibling section isolation not guaranteed; \
                 section layouts must be fully flat"
            );
        }
        std::process::exit(1);
    }
}
