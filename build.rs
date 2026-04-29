// build.rs — Slint compilation entry points.
//
// Compiles two Slint entry points:
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
}
