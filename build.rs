// build.rs — Slint compilation entry point (TASK-043).
//
// Compiles `ui/slint/main_window.slint` into Rust code that
// `slint::include_modules!()` in `src/ui/bridge.rs` picks up at compile
// time. The Slint compiler resolves `import` directives transitively, so
// this single entry-point pulls in `theme.slint`, `card_base.slint`, and
// the three tile component files.
//
// Before TASK-043 the `MainWindow` component lived inline in `bridge.rs`
// via `slint::slint!{}`; that approach was a Phase 1 workaround for a
// `files_allowlist` constraint. Lifting the component into a real `.slint`
// file lets the CI hex-color gate (`.github/workflows/ci.yml` § Gate 1,
// which scans `ui/slint/**/*.slint`) protect it natively.
//
// Cargo automatically re-runs `build.rs` when any tracked input changes;
// `slint_build::compile` emits the appropriate `cargo:rerun-if-changed`
// directives for every `.slint` file it touches, so editing any of the
// transitively imported tile/theme files triggers a rebuild.
fn main() {
    slint_build::compile("ui/slint/main_window.slint")
        .expect("compile ui/slint/main_window.slint with slint-build");
}
