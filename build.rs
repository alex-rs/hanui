// build.rs — Slint compilation entry points.
//
// Compiles four Slint entry points:
//
//   1. `ui/slint/main_window.slint` — production `MainWindow` and the
//      `AnimationBudget` / `GestureConfigGlobal` re-exports the bridge wires.
//      Picked up by `slint::include_modules!()` in `src/ui/bridge.rs`.
//
//   2. `ui/slint/gesture_test_window.slint` — TASK-060 integration-test
//      window that hosts a single `CardBase` plus three `out` counters
//      forwarded from the gesture callbacks. Compiled to a distinct output
//      file and pulled in via an explicit `include!` from
//      `src/ui/bridge.rs::gesture_test_slint` so its generated types do
//      NOT collide with the production `MainWindow` bindings.
//
//   3. `ui/slint/view.slint` — Phase 4 `View` component that renders a flat
//      `Vec<PositionedTile>` in a proportional grid (TASK-085). Compiled to a
//      distinct output file via `HANUI_VIEW_INCLUDE` so the golden-frame tests
//      (TASK-089) can include `ViewWindow` types without pulling in the full
//      production `MainWindow` symbol set. The TASK-086 bridge wires the View
//      via the production `MainWindow` import chain; this separate compile is
//      for the harness path only.
//
//   4. `ui/slint/view_switcher.slint` — Phase 4 density-gated view navigator
//      (TASK-086). Compiled to a distinct output file via
//      `HANUI_VIEW_SWITCHER_INCLUDE` so bridge.rs tests can directly
//      instantiate `ViewSwitcher` without pulling in the full production
//      `MainWindow` symbol set. Production wiring uses this same include path
//      inside `src/ui/bridge.rs`'s `view_switcher_slint` submodule.
//
// Why two compiles instead of co-locating `GestureTestWindow` inside
// `main_window.slint`: the test wrapper carries no production code path
// and should not bloat the runtime binary's component table or create
// symbols that look like a stale production component to readers of
// `bridge.rs`. A separate compile keeps the production module and the
// test module disjoint at the Slint level.
//
// Cargo automatically re-runs `build.rs` when any tracked input changes;
// `slint_build::compile_with_output_path` returns the dependency list so
// we can emit our own `cargo:rerun-if-changed` directives for both
// entry points.

use std::env;
use std::path::PathBuf;

fn main() {
    // Production entry point. `slint_build::compile` emits the
    // `SLINT_INCLUDE_GENERATED` env that `slint::include_modules!()`
    // consumes and prints `cargo:rerun-if-changed` lines for every
    // transitively-imported `.slint` file.
    slint_build::compile("ui/slint/main_window.slint")
        .expect("compile ui/slint/main_window.slint with slint-build");

    // Test entry point. We use `compile_with_output_path` (rather than
    // a second `compile()` call, which would clobber `SLINT_INCLUDE_GENERATED`)
    // so the bridge can pick up the generated file via a direct `include!`.
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let test_input = manifest_dir.join("ui/slint/gesture_test_window.slint");
    let test_output = out_dir.join("gesture_test_window.rs");

    let deps = slint_build::compile_with_output_path(
        &test_input,
        &test_output,
        slint_build::CompilerConfiguration::default(),
    )
    .expect("compile ui/slint/gesture_test_window.slint with slint-build");

    // `compile_with_output_path` does NOT emit `cargo:rerun-if-changed`
    // automatically (it is meant to be usable outside cargo too) — emit
    // them ourselves so editing the test wrapper or any of its transitive
    // imports rebuilds the test code path.
    for dep in deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
    // Expose the generated path to the bridge via an env var the
    // `include!` line consumes. Keeping the indirection (rather than
    // hard-coding `concat!(env!("OUT_DIR"), "/gesture_test_window.rs")`
    // in the bridge) lets a future build refactor relocate the file
    // without editing two places.
    println!(
        "cargo:rustc-env=HANUI_GESTURE_TEST_INCLUDE={}",
        test_output.display()
    );

    // Span-check prototype (TASK-078) — Phase 4 kill-switch gate.
    // Compiles `examples/span_check.slint` to a separate output file so
    // `examples/span_check.rs` can include the generated Rust bindings via
    // the `HANUI_SPAN_CHECK_INCLUDE` env var (mirrors the gesture_test_window
    // pattern above). This keeps the generated types for the prototype
    // isolated from the production `MainWindow` bindings; the `include!` in
    // `examples/span_check.rs` pulls in `SpanCheckWindow` only for that
    // binary, not for the runtime or any integration-test binary.
    let span_check_input = manifest_dir.join("examples/span_check.slint");
    let span_check_output = out_dir.join("span_check.rs");

    let span_check_deps = slint_build::compile_with_output_path(
        &span_check_input,
        &span_check_output,
        slint_build::CompilerConfiguration::default(),
    )
    .expect("compile examples/span_check.slint with slint-build");

    for dep in span_check_deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
    println!(
        "cargo:rustc-env=HANUI_SPAN_CHECK_INCLUDE={}",
        span_check_output.display()
    );

    // View component (TASK-085) — Phase 4 proportional-grid View.
    //
    // Compiles `ui/slint/view.slint` to a separate output file so the
    // golden-frame harness (TASK-089) can include the generated `PositionedTile`
    // struct and `View` component bindings via `HANUI_VIEW_INCLUDE` without
    // pulling in the full production `MainWindow` symbol set.
    //
    // This compile also ensures that `view.slint` is in the build graph:
    // `cargo build` will fail if the component has a syntax or type error,
    // satisfying the acceptance criterion
    // "verified by `cargo build` succeeding after the new component is
    //  referenced from the build graph."
    //
    // The TASK-086 bridge will wire the View through the production
    // `main_window.slint` import chain (adding `import { View } from "view.slint"`
    // to main_window.slint); this separate compile is used ONLY by the harness.
    let view_input = manifest_dir.join("ui/slint/view.slint");
    let view_output = out_dir.join("view.rs");

    let view_deps = slint_build::compile_with_output_path(
        &view_input,
        &view_output,
        slint_build::CompilerConfiguration::default(),
    )
    .expect("compile ui/slint/view.slint with slint-build");

    for dep in view_deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
    println!(
        "cargo:rustc-env=HANUI_VIEW_INCLUDE={}",
        view_output.display()
    );

    // ViewSwitcher component (TASK-086) — density-gated view navigator.
    //
    // Compiles `ui/slint/view_switcher.slint` to a separate output file so
    // the bridge's `view_switcher_slint` submodule and its `#[cfg(test)]`
    // density × view-count table tests can instantiate `ViewSwitcher` without
    // pulling in the full production `MainWindow` symbol set.
    //
    // This compile ensures `view_switcher.slint` is in the build graph:
    // `cargo build` will fail if the component has a syntax or type error.
    let view_switcher_input = manifest_dir.join("ui/slint/view_switcher.slint");
    let view_switcher_output = out_dir.join("view_switcher.rs");

    let view_switcher_deps = slint_build::compile_with_output_path(
        &view_switcher_input,
        &view_switcher_output,
        slint_build::CompilerConfiguration::default(),
    )
    .expect("compile ui/slint/view_switcher.slint with slint-build");

    for dep in view_switcher_deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
    println!(
        "cargo:rustc-env=HANUI_VIEW_SWITCHER_INCLUDE={}",
        view_switcher_output.display()
    );

    // PinEntry modal component (TASK-100) — standalone PIN entry window.
    //
    // Compiles `ui/slint/pin_entry.slint` to a separate output file so the
    // bridge's `pin_entry_slint` submodule can instantiate `PinEntryWindow`
    // without pulling in the full production `MainWindow` symbol set. This
    // mirrors the pattern used by `gesture_test_window.slint` (TASK-060) and
    // `view_switcher.slint` (TASK-086).
    //
    // This compile puts `pin_entry.slint` in the build graph:
    // `cargo build` will fail if the component has a syntax or type error
    // (satisfying the "Slint component compile gate" acceptance criterion in
    // TASK-100).
    //
    // The generated `PinEntryWindow` type is accessible to `src/ui/bridge.rs`
    // via `include!(env!("HANUI_PIN_ENTRY_INCLUDE"))` inside the
    // `pin_entry_slint` submodule.
    let pin_entry_input = manifest_dir.join("ui/slint/pin_entry.slint");
    let pin_entry_output = out_dir.join("pin_entry.rs");

    let pin_entry_deps = slint_build::compile_with_output_path(
        &pin_entry_input,
        &pin_entry_output,
        slint_build::CompilerConfiguration::default(),
    )
    .expect("compile ui/slint/pin_entry.slint with slint-build");

    for dep in pin_entry_deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
    println!(
        "cargo:rustc-env=HANUI_PIN_ENTRY_INCLUDE={}",
        pin_entry_output.display()
    );

    // CoverTile component (TASK-102) — Phase 6 Wave 2 cover tile.
    //
    // Compiles `ui/slint/cover_tile.slint` to a separate output file so
    // the bridge's `cover_tile_slint` submodule can reference the generated
    // `CoverTile`, `CoverTileVM`, and `CoverTilePlacement` types without
    // pulling in the full production `MainWindow` symbol set. This mirrors
    // the pattern used by `gesture_test_window.slint` (TASK-060),
    // `view_switcher.slint` (TASK-086), and `pin_entry.slint` (TASK-100).
    //
    // This compile puts `cover_tile.slint` in the build graph: `cargo build`
    // fails if the component has a syntax or type error (satisfying the
    // "Slint component compile gate" acceptance criterion in TASK-102).
    //
    // The generated types are accessible to `src/ui/bridge.rs` via
    // `include!(env!("HANUI_COVER_TILE_INCLUDE"))` inside the
    // `cover_tile_slint` submodule. Future work (when `main_window.slint` is
    // amended in a subsequent ticket) will swap to the production import
    // chain and this separate compile will become test-only.
    let cover_tile_input = manifest_dir.join("ui/slint/cover_tile.slint");
    let cover_tile_output = out_dir.join("cover_tile.rs");

    let cover_tile_deps = slint_build::compile_with_output_path(
        &cover_tile_input,
        &cover_tile_output,
        slint_build::CompilerConfiguration::default(),
    )
    .expect("compile ui/slint/cover_tile.slint with slint-build");

    for dep in cover_tile_deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
    println!(
        "cargo:rustc-env=HANUI_COVER_TILE_INCLUDE={}",
        cover_tile_output.display()
    );

    // FanTile component (TASK-103) — Phase 6 Wave 2 fan tile.
    //
    // Compiles `ui/slint/fan_tile.slint` to a separate output file so
    // the bridge's `fan_tile_slint` submodule can reference the generated
    // `FanTile`, `FanTileVM`, and `FanTilePlacement` types without
    // pulling in the full production `MainWindow` symbol set. This mirrors
    // the pattern used by `gesture_test_window.slint` (TASK-060),
    // `view_switcher.slint` (TASK-086), `pin_entry.slint` (TASK-100),
    // and `cover_tile.slint` (TASK-102).
    //
    // This compile puts `fan_tile.slint` in the build graph: `cargo build`
    // fails if the component has a syntax or type error (satisfying the
    // "Slint component compile gate" acceptance criterion in TASK-103).
    //
    // The generated types are accessible to `src/ui/bridge.rs` via
    // `include!(env!("HANUI_FAN_TILE_INCLUDE"))` inside the
    // `fan_tile_slint` submodule. Future work (when `main_window.slint` is
    // amended in a subsequent ticket) will swap to the production import
    // chain and this separate compile will become test-only.
    let fan_tile_input = manifest_dir.join("ui/slint/fan_tile.slint");
    let fan_tile_output = out_dir.join("fan_tile.rs");

    let fan_tile_deps = slint_build::compile_with_output_path(
        &fan_tile_input,
        &fan_tile_output,
        slint_build::CompilerConfiguration::default(),
    )
    .expect("compile ui/slint/fan_tile.slint with slint-build");

    for dep in fan_tile_deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
    println!(
        "cargo:rustc-env=HANUI_FAN_TILE_INCLUDE={}",
        fan_tile_output.display()
    );

    // LockTile component (TASK-104) — Phase 6 Wave 2 lock tile.
    //
    // Compiles `ui/slint/lock_tile.slint` to a separate output file so
    // the bridge's `lock_tile_slint` submodule can reference the generated
    // `LockTile`, `LockTileVM`, and `LockTilePlacement` types without
    // pulling in the full production `MainWindow` symbol set. This mirrors
    // the pattern used by `gesture_test_window.slint` (TASK-060),
    // `view_switcher.slint` (TASK-086), `pin_entry.slint` (TASK-100),
    // `cover_tile.slint` (TASK-102), and `fan_tile.slint` (TASK-103).
    //
    // This compile puts `lock_tile.slint` in the build graph: `cargo build`
    // fails if the component has a syntax or type error (satisfying the
    // "Slint component compile gate" acceptance criterion in TASK-104).
    //
    // The generated types are accessible to `src/ui/bridge.rs` via
    // `include!(env!("HANUI_LOCK_TILE_INCLUDE"))` inside the
    // `lock_tile_slint` submodule. Future work (when `main_window.slint` is
    // amended in a subsequent ticket) will swap to the production import
    // chain and this separate compile will become test-only.
    let lock_tile_input = manifest_dir.join("ui/slint/lock_tile.slint");
    let lock_tile_output = out_dir.join("lock_tile.rs");

    let lock_tile_deps = slint_build::compile_with_output_path(
        &lock_tile_input,
        &lock_tile_output,
        slint_build::CompilerConfiguration::default(),
    )
    .expect("compile ui/slint/lock_tile.slint with slint-build");

    for dep in lock_tile_deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
    println!(
        "cargo:rustc-env=HANUI_LOCK_TILE_INCLUDE={}",
        lock_tile_output.display()
    );

    // AlarmPanelTile component (TASK-105) — Phase 6 Wave 2 alarm-panel tile.
    //
    // Compiles `ui/slint/alarm_panel_tile.slint` to a separate output file so
    // the bridge's `alarm_panel_tile_slint` submodule can reference the
    // generated `AlarmPanelTile`, `AlarmTileVM`, and `AlarmTilePlacement`
    // types without pulling in the full production `MainWindow` symbol set.
    // Mirrors the pattern used by `cover_tile.slint` (TASK-102),
    // `fan_tile.slint` (TASK-103), and `lock_tile.slint` (TASK-104).
    //
    // This compile puts `alarm_panel_tile.slint` in the build graph:
    // `cargo build` fails if the component has a syntax or type error
    // (satisfying the "Slint component compile gate" acceptance criterion in
    // TASK-105).
    //
    // The generated types are accessible to `src/ui/bridge.rs` via
    // `include!(env!("HANUI_ALARM_PANEL_TILE_INCLUDE"))` inside the
    // `alarm_panel_tile_slint` submodule.
    let alarm_panel_tile_input = manifest_dir.join("ui/slint/alarm_panel_tile.slint");
    let alarm_panel_tile_output = out_dir.join("alarm_panel_tile.rs");

    let alarm_panel_tile_deps = slint_build::compile_with_output_path(
        &alarm_panel_tile_input,
        &alarm_panel_tile_output,
        slint_build::CompilerConfiguration::default(),
    )
    .expect("compile ui/slint/alarm_panel_tile.slint with slint-build");

    for dep in alarm_panel_tile_deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
    println!(
        "cargo:rustc-env=HANUI_ALARM_PANEL_TILE_INCLUDE={}",
        alarm_panel_tile_output.display()
    );

    // HistoryGraphTile component (TASK-106) — Phase 6 Wave 2 history-graph tile.
    //
    // Compiles `ui/slint/history_graph_tile.slint` to a separate output file so
    // the bridge's `history_graph_tile_slint` submodule can reference the
    // generated `HistoryGraphTile`, `HistoryGraphTileVM`, `HistoryPoint`, and
    // `HistoryGraphTilePlacement` types without pulling in the full production
    // `MainWindow` symbol set. Mirrors the pattern used by `cover_tile.slint`
    // (TASK-102), `fan_tile.slint` (TASK-103), `lock_tile.slint` (TASK-104),
    // and `alarm_panel_tile.slint` (TASK-105).
    //
    // This compile puts `history_graph_tile.slint` in the build graph:
    // `cargo build` fails if the component has a syntax or type error
    // (satisfying the "Slint component compile gate" acceptance criterion in
    // TASK-106).
    //
    // The generated types are accessible to `src/ui/bridge.rs` via
    // `include!(env!("HANUI_HISTORY_GRAPH_TILE_INCLUDE"))` inside the
    // `history_graph_tile_slint` submodule.
    let history_graph_tile_input = manifest_dir.join("ui/slint/history_graph_tile.slint");
    let history_graph_tile_output = out_dir.join("history_graph_tile.rs");

    let history_graph_tile_deps = slint_build::compile_with_output_path(
        &history_graph_tile_input,
        &history_graph_tile_output,
        slint_build::CompilerConfiguration::default(),
    )
    .expect("compile ui/slint/history_graph_tile.slint with slint-build");

    for dep in history_graph_tile_deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
    println!(
        "cargo:rustc-env=HANUI_HISTORY_GRAPH_TILE_INCLUDE={}",
        history_graph_tile_output.display()
    );
}
