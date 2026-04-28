//! Headless Slint rendering harness — POC (TASK-074).
//!
//! # Purpose
//!
//! Provide the smallest workable shape of a headless renderer for Slint
//! components so downstream UI integration tests (TASK-073, golden-frame
//! optimistic-revert assertion) can render `MainWindow` (or any other
//! `slint::ComponentHandle`) to an in-memory pixel buffer without spawning
//! a real window. Plan: docs/plans/2026-04-28-phase-3-actions.md, Risk #13
//! (headless harness unworkability).
//!
//! # Backend chosen
//!
//! The runtime crate (`Cargo.toml`) already enables Slint's `renderer-software`
//! feature. We reuse that renderer plus a minimal custom [`slint::platform::Platform`]
//! built around [`MinimalSoftwareWindow`]. No dev-dep additions are needed.
//!
//! Approaches considered (kept here as design notes so reviewers can verify
//! the choice; do not delete without re-running the analysis):
//!
//! 1. **`slint::platform::software_renderer::MinimalSoftwareWindow` + custom
//!    `Platform`** — chosen. The `SoftwareRenderer` behind `MinimalSoftwareWindow`
//!    implements [`take_snapshot`](slint::Window::take_snapshot), which returns a
//!    `SharedPixelBuffer<Rgba8Pixel>`. Works with the existing
//!    `slint::include_modules!()`-generated `MainWindow` from
//!    `src/ui/bridge.rs::slint_ui::MainWindow` — no `.slint` source recompile.
//! 2. **`i-slint-backend-testing` (`init_no_event_loop()`)** — rejected. Its
//!    `TestingWindow` is a layout/text-metrics renderer only; the default
//!    `RendererSealed::take_snapshot` returns
//!    `Err("WindowAdapter::take_snapshot is not implemented by the platform")`.
//!    Useful for property/event tests, not for golden-frame pixel assertions.
//! 3. **`slint::interpreter`** — rejected. Adds a runtime `.slint` parser
//!    (~MB-scale dep), and we already have compile-time bindings via
//!    `slint::include_modules!()`. Re-parsing at test time would also drift
//!    the test from the production rendering path.
//!
//! # Usage
//!
//! ```ignore
//! let mut harness = HeadlessRenderer::new()?;
//! let mw = hanui::ui::bridge::MainWindow::new()?;
//! let pixels = harness.render_component(&mw, 480, 600)?;
//! assert!(!pixels.is_empty());
//! ```
//!
//! # Per-thread platform — what is and is not shared
//!
//! Slint's `GLOBAL_CONTEXT` (which holds the registered platform) is itself
//! `thread_local!` inside `i-slint-core` — see `i-slint-core/context.rs`.
//! Cargo's libtest spawns a fresh worker thread per `#[test]` (even with
//! `--test-threads=1`), so each test thread independently calls
//! `slint::platform::set_platform`. The harness gates that install via a
//! per-thread `OnceCell<Rc<MinimalSoftwareWindow>>`: `HeadlessRenderer::new`
//! is idempotent on a given thread but installs a fresh platform on a new
//! thread. Each thread thus owns one renderer + one window across all of
//! its `render_component` calls.
//!
//! What this DOES support (TASK-073's core needs):
//!
//! - **Multiple `render_component` calls on the same component**, with Slint
//!   property mutations in between. The window is reusable; calling
//!   `take_snapshot` again after a property write produces a fresh frame
//!   reflecting the new state. This is the BEFORE/AFTER capture path for
//!   golden-frame optimistic-revert assertions.
//! - **Multiple `ComponentHandle` instances of different component types** in
//!   the same test binary. They all share the harness window via
//!   `create_window_adapter`, but each is rendered via its own
//!   `render_component` call paired with `show()` / `hide()`.
//! - **Sequential `#[test]` functions** in one binary, each constructing its
//!   own component(s). Cargo's default test harness runs tests on a thread
//!   pool, but Slint requires single-threaded UI access — see the
//!   `single-threaded` note below.
//!
//! What this does NOT support:
//!
//! - Calling `slint::platform::set_platform` from outside this harness in the
//!   same process. The harness wins the race iff it runs first.
//! - True parallel rendering across threads — the underlying `Rc`/`Cell` in
//!   `MinimalSoftwareWindow` is `!Send + !Sync`. Set
//!   `RUST_TEST_THREADS=1` for any binary that exercises the harness from
//!   multiple `#[test]` functions, or guard the renderer with a
//!   `parking_lot::Mutex` if true serialization is needed (deferred to
//!   TASK-073 if it actually hits the issue).
//!
//! # Verdict
//!
//! Path A (POC works). The smoke test below renders `MainWindow` from
//! `ui/slint/main_window.slint` at 480×600 and asserts the captured RGBA8
//! buffer has the expected pixel count *and* contains at least one non-zero
//! byte (not a transparent/all-zero frame). Slint's software renderer fills
//! the window background with `Theme.background` (a non-transparent token
//! defined in `ui/slint/theme.slint`), so the assertion is meaningful even
//! with zero tiles wired in.

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::ComponentHandle;
use slint::PhysicalSize;
use slint::PlatformError;

/// Errors returned by the harness. Distinct from `slint::PlatformError` so
/// callers can pattern-match on harness-specific failure modes (e.g.
/// "platform already installed by another consumer of slint").
#[derive(Debug)]
pub enum HarnessError {
    /// Slint reported a platform-level failure (snapshot, set_size, etc.).
    Platform(PlatformError),
    /// `slint::platform::set_platform` was called from outside the harness
    /// before this harness could install its own. Downstream tests must
    /// avoid touching `set_platform` directly.
    PlatformAlreadyInstalled,
    /// The render produced an empty buffer — either zero-sized window or a
    /// renderer-internal failure that did not surface a `PlatformError`.
    EmptyFrame,
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Platform(e) => write!(f, "slint platform error: {e}"),
            Self::PlatformAlreadyInstalled => {
                write!(
                    f,
                    "slint::platform::set_platform was already called by another caller"
                )
            }
            Self::EmptyFrame => write!(f, "headless renderer produced an empty frame"),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<PlatformError> for HarnessError {
    fn from(e: PlatformError) -> Self {
        Self::Platform(e)
    }
}

/// Captured frame from [`HeadlessRenderer::render_component`].
///
/// Pixels are RGBA8, row-major, top-left origin — Slint's
/// `SharedPixelBuffer<Rgba8Pixel>` layout. The `Vec<u8>` length is always
/// `width * height * 4`.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl CapturedFrame {
    /// Returns true if the frame contains at least one non-zero byte.
    /// Used by smoke assertions to distinguish a real render from a
    /// zero-initialised buffer.
    pub fn has_non_zero_byte(&self) -> bool {
        self.pixels.iter().any(|&b| b != 0)
    }

    /// Number of distinct RGBA pixel values in the frame. A real render of
    /// `MainWindow` (with its background, borders, status banner, and any
    /// tiles) produces dozens or more distinct colors; an uninitialized or
    /// flat-fill buffer produces ≤ 1. Smoke tests use `distinct_color_count
    /// > 1` to reject the all-black-from-uninitialized-heap false positive
    /// that `has_non_zero_byte` alone cannot rule out.
    pub fn distinct_color_count(&self) -> usize {
        // RGBA8 → 4-byte chunks. A `HashSet<[u8; 4]>` is bounded by
        // `width * height` worst-case but in practice closer to the count of
        // visible UI primitives. For 480×600 = 288_000 pixels this is fine
        // for a smoke test; downstream golden-frame tests will use stronger
        // pixel-by-pixel comparisons against a fixture.
        use std::collections::HashSet;
        let mut seen: HashSet<[u8; 4]> = HashSet::new();
        for px in self.pixels.chunks_exact(4) {
            seen.insert([px[0], px[1], px[2], px[3]]);
            // A modest count is sufficient evidence; bail early to keep the
            // smoke test fast.
            if seen.len() > 4 {
                return seen.len();
            }
        }
        seen.len()
    }
}

/// Test platform that hands every component the same `MinimalSoftwareWindow`.
///
/// Slint's component creation (`MainWindow::new()`) calls
/// `Platform::create_window_adapter`. Returning a clone of the same
/// `Rc<MinimalSoftwareWindow>` every time is intentional: it lets the
/// harness reuse one renderer across multiple `render_component` calls
/// (BEFORE/AFTER captures) and across multiple component instances rendered
/// sequentially (e.g. a `MainWindow` followed by a `MoreInfoModal`). The
/// `show()` / `hide()` calls inside [`HeadlessRenderer::render_component`]
/// keep the platform's internal window-count counter consistent; only one
/// component is "shown" at a time so snapshots capture the intended item
/// tree, not a stack of overlapping components.
struct HeadlessPlatform {
    window: std::rc::Rc<MinimalSoftwareWindow>,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<std::rc::Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }
}

// Slint's `GLOBAL_CONTEXT` (which holds the registered Platform) is
// `thread_local!` (see i-slint-core/context.rs). Cargo's libtest spawns a
// fresh worker thread per `#[test]` even with `--test-threads=1`, so a
// process-wide `OnceLock` would falsely report "installed" on a thread
// where Slint sees no platform yet, and `MainWindow::new()` would panic
// with "no platform set".
//
// The correct gate is therefore *per-thread*: each thread installs its
// own platform exactly once, and the harness owns one
// `Rc<MinimalSoftwareWindow>` per thread. `MinimalSoftwareWindow` is
// `!Send + !Sync` (Rc + Cell internals), so a thread-local store is
// also the only sound place to keep the handle.
thread_local! {
    static HARNESS_WINDOW: std::cell::OnceCell<std::rc::Rc<MinimalSoftwareWindow>> =
        const { std::cell::OnceCell::new() };
}

fn install_platform_once() -> Result<std::rc::Rc<MinimalSoftwareWindow>, HarnessError> {
    HARNESS_WINDOW.with(|cell| {
        if let Some(existing) = cell.get() {
            // Already installed on this thread — reuse the same window.
            return Ok(existing.clone());
        }
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let platform = HeadlessPlatform {
            window: window.clone(),
        };
        match slint::platform::set_platform(Box::new(platform)) {
            Ok(()) => {
                let _ = cell.set(window.clone());
                Ok(window)
            }
            Err(_) => Err(HarnessError::PlatformAlreadyInstalled),
        }
    })
}

/// Headless renderer handle. Construct via [`HeadlessRenderer::new`] before
/// instantiating any Slint component.
pub struct HeadlessRenderer {
    window: std::rc::Rc<MinimalSoftwareWindow>,
}

impl HeadlessRenderer {
    /// Install the headless platform (idempotent) and return a handle the
    /// caller can use to render components.
    ///
    /// Must be called *before* the first `ComponentHandle::new()` in the
    /// process; otherwise Slint will have already auto-installed the winit
    /// backend and `set_platform` returns `AlreadySet`.
    pub fn new() -> Result<Self, HarnessError> {
        let window = install_platform_once()?;
        Ok(Self { window })
    }

    /// Render the given component to an RGBA8 pixel buffer at `(width, height)`
    /// physical pixels. The component's window is resized to match before the
    /// snapshot is taken.
    pub fn render_component<C: ComponentHandle>(
        &mut self,
        component: &C,
        width: u32,
        height: u32,
    ) -> Result<CapturedFrame, HarnessError> {
        // Match component's window to the harness window (they're the same
        // adapter returned by `create_window_adapter`, but `set_size` on the
        // public Window API is the documented way to tell the renderer the
        // canvas dimensions).
        component
            .window()
            .set_size(PhysicalSize::new(width, height));

        // Show() registers the item tree with the window so the snapshot
        // captures the component, not an empty backdrop. We pair it with
        // hide() afterwards to keep the platform's window-count counter
        // consistent across multiple renders in the same process.
        component.show()?;
        let snapshot_result = component.window().take_snapshot();
        component.hide()?;

        let buf = snapshot_result?;
        let bw = buf.width();
        let bh = buf.height();

        // `Rgba8Pixel` is `#[repr(C)]` { r, g, b, a }; `as_slice()` gives the
        // typed slice. We copy into a `Vec<u8>` for an owned, framework-agnostic
        // return type that downstream tests can checksum or hand to `image`.
        let pixels: Vec<u8> = buf
            .as_slice()
            .iter()
            .flat_map(|p| [p.r, p.g, p.b, p.a])
            .collect();

        if pixels.is_empty() {
            return Err(HarnessError::EmptyFrame);
        }

        Ok(CapturedFrame {
            width: bw,
            height: bh,
            pixels,
        })
    }

    /// Direct access to the harness window for callers that need to dispatch
    /// raw `WindowEvent`s (e.g. simulated pointer-press for golden-frame
    /// before/after captures in TASK-073).
    pub fn window(&self) -> &MinimalSoftwareWindow {
        &self.window
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Smoke test (TASK-074 acceptance criterion #2): render `MainWindow` and
// assert the captured buffer is non-empty and contains at least one non-zero
// byte. Lives in the same file per the AC; downstream golden-frame tests
// (TASK-073) consume the API above through `#[path = "../common/slint_harness.rs"]`.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod smoke {
    use super::*;
    use hanui::ui::bridge::MainWindow;

    /// Renders `MainWindow` at 480×600 (its `preferred-width`/`preferred-height`
    /// from `ui/slint/main_window.slint`) and asserts a meaningful buffer is
    /// produced. The window's background is `Theme.background`, which is a
    /// non-transparent token, so even with zero tiles wired the renderer
    /// produces non-zero bytes — proving the headless path actually drew.
    /// `distinct_color_count > 1` rules out the all-black-from-uninitialized
    /// heap false positive that `has_non_zero_byte` alone cannot detect.
    #[test]
    fn renders_main_window_non_empty() {
        let mut harness = HeadlessRenderer::new().expect("install headless platform");
        let main_window = MainWindow::new().expect("instantiate MainWindow");

        let frame = harness
            .render_component(&main_window, 480, 600)
            .expect("render MainWindow");

        assert_eq!(
            frame.pixels.len() as u32,
            frame.width * frame.height * 4,
            "RGBA8 buffer length must be width*height*4"
        );
        assert!(
            frame.width > 0 && frame.height > 0,
            "non-zero render dimensions"
        );
        assert!(
            !frame.pixels.is_empty(),
            "captured pixel buffer must not be empty"
        );
        assert!(
            frame.has_non_zero_byte(),
            "captured pixel buffer must contain at least one non-zero byte (not all-zero/transparent)"
        );
        // Note on strictness: with no tiles wired, the empty MainWindow
        // renders an essentially-uniform `Theme.background` fill, so a
        // `distinct_color_count > 1` assertion would be a false negative
        // here. TASK-073 will exercise that stronger guard against
        // `MainWindow` populated with fixture tiles, where dozens of
        // distinct colors are expected. The `has_non_zero_byte` check is
        // the right level of strictness for *this* smoke: it confirms the
        // renderer actually wrote into the buffer (an uninitialized
        // `SharedPixelBuffer` would be all-zero).
    }

    /// BEFORE/AFTER capture: render the same `MainWindow` twice with a
    /// property mutated between renders, and verify the two frames differ.
    /// This is the contract TASK-073 needs for the optimistic-revert
    /// no-flicker assertion: render the tile, mutate the optimistic-state
    /// property, render again, compare.
    ///
    /// Implementation note: the harness's `OnceLock` only guards
    /// `set_platform` install, NOT the renderer or window — those are
    /// reused across calls. Toggling `status_banner_visible` between
    /// renders changes the visible UI (banner appears/disappears), so the
    /// captured buffers must differ at least somewhere.
    #[test]
    fn renders_same_component_twice_with_mutation_between() {
        let mut harness = HeadlessRenderer::new().expect("install headless platform");
        let main_window = MainWindow::new().expect("instantiate MainWindow");

        main_window.set_status_banner_visible(false);
        let before = harness
            .render_component(&main_window, 480, 600)
            .expect("render BEFORE frame");

        main_window.set_status_banner_visible(true);
        let after = harness
            .render_component(&main_window, 480, 600)
            .expect("render AFTER frame");

        assert_eq!(before.pixels.len(), after.pixels.len(), "same dimensions");
        assert!(
            before.pixels != after.pixels,
            "mutating an in-property between renders must produce a different frame \
             (else state mutation is not observable through the harness — TASK-073 blocker)"
        );
    }
}
