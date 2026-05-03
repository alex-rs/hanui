//! Icon resolver for `mdi:*` identifiers.
//!
//! # Decode path
//!
//! SVG bytes (embedded via `include_bytes!`) are rasterized once at startup
//! using `resvg` + `usvg` + `tiny-skia`.  The rasterized RGBA pixels are
//! wrapped in a `slint::SharedPixelBuffer<Rgba8Pixel>` and turned into a
//! `slint::Image` via `Image::from_rgba8_premultiplied` (tiny-skia produces
//! premultiplied alpha).  Each `Image` is heap-allocated behind an `Arc` and
//! stored in a `OnceLock<HashMap>` so decoding is guaranteed to happen exactly
//! once, before any frame is drawn.
//!
//! # Slint `svg` feature
//!
//! Slint 1.16 does not expose an `svg` cargo feature that enables
//! `Image::load_from_svg_data`.  The software renderer backend pulls `resvg`
//! transitively, so we surface that same pipeline as a direct dependency
//! rather than adding a net-new crate.  No SVG feature flag is needed.
//!
//! # Downscaling
//!
//! Before rasterizing, the target render size is clamped so that its longest
//! dimension does not exceed `PROFILE_DESKTOP.max_image_px` (2 048 px for the
//! desktop profile).  Because these MDI icons have a 24 × 24 viewBox they will
//! never exceed the cap in practice, but the cap is enforced unconditionally so
//! the resolver is correct for any future icon addition.
//!
//! # Thread safety
//!
//! `slint::Image` is not `Sync` because some `ImageInner` variants hold a
//! `VRc<*mut ()>` (vtable pointer for backend-owned textures, SVG decoders,
//! etc.).  The images stored here are **exclusively** constructed via
//! `Image::from_rgba8_premultiplied`, which produces `ImageInner::EmbeddedImage
//! { cache_key: Invalid, buffer: RGBA8Premultiplied(...) }` — a plain data
//! variant that contains only a `SharedPixelBuffer` (reference-counted, `Send +
//! Sync`).  No thread-local cache entry, no vtable pointer, no mutable state
//! after construction.  `SyncImage` asserts this invariant.
//!
//! # Licensing
//!
//! The embedded icons (`lightbulb`, `thermometer`, `help-circle`, plus the
//! Phase 6 expansion: `fan`, `door-closed-lock`, `garage`, `shield-home`,
//! `lightning-bolt`, `lightning-bolt-circle`, `camera`, `ceiling-light`,
//! `thermostat`, `television-play`, `window-shutter`, `home-assistant`,
//! `motion-sensor`) are sourced from the Material Design Icons project
//! (<https://github.com/Templarian/MaterialDesign-SVG>), distributed under the
//! Apache License 2.0.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

use crate::dashboard::profiles::PROFILE_DESKTOP;

// ---------------------------------------------------------------------------
// Embedded SVG assets
// ---------------------------------------------------------------------------

/// Raw SVG bytes for each bundled icon.
///
/// `include_bytes!` paths are relative to this source file.
const LIGHTBULB_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/lightbulb.svg");
const THERMOMETER_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/thermometer.svg");
const FALLBACK_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/help-circle.svg");

// Phase 6 expansion: every `mdi:*` identifier referenced by
// `examples/dashboard.yaml` and `fixture_dashboard()` must resolve to a real
// asset (non-fallback) per locked_decisions.icon_registry_completeness.
const FAN_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/fan.svg");
const DOOR_CLOSED_LOCK_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/door-closed-lock.svg");
const GARAGE_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/garage.svg");
const SHIELD_HOME_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/shield-home.svg");
const LIGHTNING_BOLT_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/lightning-bolt.svg");
const LIGHTNING_BOLT_CIRCLE_SVG: &[u8] =
    include_bytes!("../../assets/icons/mdi/lightning-bolt-circle.svg");
const CAMERA_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/camera.svg");
const CEILING_LIGHT_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/ceiling-light.svg");
const THERMOSTAT_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/thermostat.svg");
const TELEVISION_PLAY_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/television-play.svg");
const WINDOW_SHUTTER_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/window-shutter.svg");
const HOME_ASSISTANT_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/home-assistant.svg");
const MOTION_SENSOR_SVG: &[u8] = include_bytes!("../../assets/icons/mdi/motion-sensor.svg");

// ---------------------------------------------------------------------------
// Thread-safety wrapper
// ---------------------------------------------------------------------------

/// Newtype that asserts `Send + Sync` for a `slint::Image` that was created
/// exclusively via `Image::from_rgba8_premultiplied`.
///
/// # Safety
///
/// `slint::Image` is marked `!Sync` because `ImageInner` can hold a
/// `VRc<*mut ()>` (backend textures, SVG trees, HTML images).  The only
/// `ImageInner` variant that does **not** touch thread-local state or raw
/// pointers is `EmbeddedImage { cache_key: Invalid, buffer }`, which is the
/// variant produced by `Image::from_rgba8_premultiplied`.
///
/// `SharedImageBuffer::RGBA8Premultiplied` wraps a `SharedPixelBuffer<Rgba8Pixel>`,
/// which is `Send + Sync` (it is a reference-counted, immutable byte buffer).
/// There is no interior mutability and no thread-local access after construction.
///
/// The `ICONS` `OnceLock` is written exactly once before any UI thread starts,
/// and is never mutated afterwards.  Reads from multiple threads are therefore
/// safe.
struct SyncImage(Image);

/// SAFETY: see `SyncImage` doc comment above.
unsafe impl Send for SyncImage {}
/// SAFETY: see `SyncImage` doc comment above.
unsafe impl Sync for SyncImage {}

// ---------------------------------------------------------------------------
// Global icon cache
// ---------------------------------------------------------------------------

/// Populated once at startup by [`init`]; never mutated thereafter.
static ICONS: OnceLock<HashMap<&'static str, Arc<SyncImage>>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialise the icon cache.
///
/// Must be called once before [`resolve`].  Subsequent calls are no-ops (the
/// `OnceLock` guarantees exactly-once initialisation even across threads).
///
/// # Panics
///
/// Panics if any bundled SVG asset fails to rasterize.  This is acceptable
/// because a failure here indicates a broken build artifact, not a runtime
/// condition.
pub fn init() {
    ICONS.get_or_init(|| {
        let max_px = PROFILE_DESKTOP.max_image_px;
        let mut map = HashMap::new();
        map.insert(
            "mdi:lightbulb",
            Arc::new(SyncImage(rasterize(LIGHTBULB_SVG, max_px))),
        );
        map.insert(
            "mdi:thermometer",
            Arc::new(SyncImage(rasterize(THERMOMETER_SVG, max_px))),
        );
        map.insert(
            "mdi:help-circle",
            Arc::new(SyncImage(rasterize(FALLBACK_SVG, max_px))),
        );

        // Phase 6 expansion. Every `mdi:*` referenced by
        // `examples/dashboard.yaml` and `fixture_dashboard()` is registered
        // here so that no widget falls back to the question-mark glyph.
        // Adding a new identifier: drop the SVG under `assets/icons/mdi/`,
        // add an `include_bytes!` constant above, and append a `map.insert`
        // call here. Each rasterization is one-shot (during `init`) and
        // produces a `SharedPixelBuffer`-backed `Image` (Send + Sync).
        map.insert("mdi:fan", Arc::new(SyncImage(rasterize(FAN_SVG, max_px))));
        map.insert(
            "mdi:door-closed-lock",
            Arc::new(SyncImage(rasterize(DOOR_CLOSED_LOCK_SVG, max_px))),
        );
        map.insert(
            "mdi:garage",
            Arc::new(SyncImage(rasterize(GARAGE_SVG, max_px))),
        );
        map.insert(
            "mdi:shield-home",
            Arc::new(SyncImage(rasterize(SHIELD_HOME_SVG, max_px))),
        );
        map.insert(
            "mdi:lightning-bolt",
            Arc::new(SyncImage(rasterize(LIGHTNING_BOLT_SVG, max_px))),
        );
        map.insert(
            "mdi:lightning-bolt-circle",
            Arc::new(SyncImage(rasterize(LIGHTNING_BOLT_CIRCLE_SVG, max_px))),
        );
        map.insert(
            "mdi:camera",
            Arc::new(SyncImage(rasterize(CAMERA_SVG, max_px))),
        );
        map.insert(
            "mdi:ceiling-light",
            Arc::new(SyncImage(rasterize(CEILING_LIGHT_SVG, max_px))),
        );
        map.insert(
            "mdi:thermostat",
            Arc::new(SyncImage(rasterize(THERMOSTAT_SVG, max_px))),
        );
        map.insert(
            "mdi:television-play",
            Arc::new(SyncImage(rasterize(TELEVISION_PLAY_SVG, max_px))),
        );
        map.insert(
            "mdi:window-shutter",
            Arc::new(SyncImage(rasterize(WINDOW_SHUTTER_SVG, max_px))),
        );
        map.insert(
            "mdi:home-assistant",
            Arc::new(SyncImage(rasterize(HOME_ASSISTANT_SVG, max_px))),
        );
        map.insert(
            "mdi:motion-sensor",
            Arc::new(SyncImage(rasterize(MOTION_SENSOR_SVG, max_px))),
        );
        map
    });
}

/// Resolve an `mdi:*` identifier to a `slint::Image`.
///
/// Returns a clone of the cached image for the requested icon, or the fallback
/// icon (`mdi:help-circle`) for any unrecognised identifier.  The caller
/// **never** receives an error; the fallback is always present because it is
/// registered during [`init`].
///
/// Cloning a `slint::Image` backed by `EmbeddedImage` bumps the
/// `SharedPixelBuffer` reference count — no pixel data is copied.
///
/// # Panics
///
/// Panics if [`init`] has not been called first (the `OnceLock` is unset).
pub fn resolve(id: &str) -> Image {
    let cache = ICONS
        .get()
        .expect("icons::init() must be called before icons::resolve()");
    let sync_image = cache
        .get(id)
        .or_else(|| cache.get("mdi:help-circle"))
        .expect("fallback icon must be present in cache");
    // Clone the inner Image out of the SyncImage wrapper.  `slint::Image`
    // is `Clone`; this bumps the underlying `SharedPixelBuffer` refcount —
    // no pixel data is copied.
    sync_image.0.clone()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Rasterize `svg_bytes` to a `slint::Image`.
///
/// The output dimensions are computed from the SVG's intrinsic size, capped so
/// that `max(width, height) <= max_px`.  When the SVG is smaller than `max_px`
/// in both dimensions it is rendered at its natural size.
///
/// tiny-skia fills the pixmap with premultiplied RGBA, which maps directly to
/// `Image::from_rgba8_premultiplied`.
fn rasterize(svg_bytes: &[u8], max_px: u32) -> Image {
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(svg_bytes, &opt).expect("bundled SVG asset must be valid");

    let svg_size = tree.size();
    let (render_w, render_h) = clamped_size(svg_size.width(), svg_size.height(), max_px);

    let mut pixmap =
        tiny_skia::Pixmap::new(render_w, render_h).expect("render dimensions must be non-zero");

    let sx = render_w as f32 / svg_size.width();
    let sy = render_h as f32 / svg_size.height();
    let transform = tiny_skia::Transform::from_scale(sx, sy);

    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let pixel_buffer =
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(pixmap.data(), render_w, render_h);
    Image::from_rgba8_premultiplied(pixel_buffer)
}

/// Return `(width, height)` scaled down uniformly so that the longest
/// dimension does not exceed `max_px`.  Returns the original dimensions when
/// they already fit.
fn clamped_size(w: f32, h: f32, max_px: u32) -> (u32, u32) {
    let max_f = max_px as f32;
    let longest = w.max(h);
    if longest <= max_f {
        (w.ceil() as u32, h.ceil() as u32)
    } else {
        let scale = max_f / longest;
        let new_w = (w * scale).ceil() as u32;
        let new_h = (h * scale).ceil() as u32;
        (new_w.max(1), new_h.max(1))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: initialise once across all tests in this module.
    fn ensure_init() {
        init();
    }

    #[test]
    fn resolve_lightbulb_returns_non_fallback() {
        ensure_init();
        // The lightbulb and fallback must be distinct pixel buffers.
        let lightbulb = resolve("mdi:lightbulb");
        let fallback = resolve("mdi:help-circle");
        // Images are not `PartialEq` by pointer; compare pixel data dimensions
        // as a proxy (both are 24 × 24, but their pixel content differs).
        // The real assertion: lightbulb must NOT be the same Arc as fallback.
        // `resolve` always creates a fresh Arc wrapping a clone of the Image,
        // so ptr_eq on the Arc won't work.  Instead, verify the pixel contents
        // differ.
        let lb_pixels = lightbulb
            .to_rgba8()
            .expect("lightbulb image must have rgba8 pixel data");
        let fb_pixels = fallback
            .to_rgba8()
            .expect("fallback image must have rgba8 pixel data");
        assert_ne!(
            lb_pixels.as_bytes(),
            fb_pixels.as_bytes(),
            "lightbulb pixel data must differ from fallback"
        );
    }

    #[test]
    fn resolve_thermometer_returns_non_fallback() {
        ensure_init();
        let thermometer = resolve("mdi:thermometer");
        let fallback = resolve("mdi:help-circle");
        let th_pixels = thermometer
            .to_rgba8()
            .expect("thermometer image must have rgba8 pixel data");
        let fb_pixels = fallback
            .to_rgba8()
            .expect("fallback image must have rgba8 pixel data");
        assert_ne!(
            th_pixels.as_bytes(),
            fb_pixels.as_bytes(),
            "thermometer pixel data must differ from fallback"
        );
    }

    #[test]
    fn resolve_unknown_id_returns_fallback() {
        ensure_init();
        let unknown = resolve("mdi:nonexistent");
        let fallback = resolve("mdi:help-circle");
        let unk_pixels = unknown
            .to_rgba8()
            .expect("unknown-id result must have rgba8 pixel data");
        let fb_pixels = fallback
            .to_rgba8()
            .expect("fallback image must have rgba8 pixel data");
        assert_eq!(
            unk_pixels.as_bytes(),
            fb_pixels.as_bytes(),
            "unknown id must return the same pixel data as the fallback"
        );
    }

    #[test]
    fn resolve_non_mdi_prefix_returns_fallback() {
        ensure_init();
        let not_mdi = resolve("not-an-mdi-id");
        let fallback = resolve("mdi:help-circle");
        let nm_pixels = not_mdi
            .to_rgba8()
            .expect("non-mdi-id result must have rgba8 pixel data");
        let fb_pixels = fallback
            .to_rgba8()
            .expect("fallback image must have rgba8 pixel data");
        assert_eq!(
            nm_pixels.as_bytes(),
            fb_pixels.as_bytes(),
            "non-mdi id must return the same pixel data as the fallback"
        );
    }

    #[test]
    fn clamped_size_within_budget_unchanged() {
        let (w, h) = clamped_size(24.0, 24.0, 2048);
        assert_eq!((w, h), (24, 24));
    }

    #[test]
    fn clamped_size_portrait_oversized_is_downscaled() {
        // tall image: 1024 x 4096, cap at 2048
        let (w, h) = clamped_size(1024.0, 4096.0, 2048);
        assert!(h <= 2048, "height must not exceed cap");
        assert!(w <= 2048, "width must not exceed cap");
        // aspect ratio preserved: w/h ≈ 1024/4096 = 0.25
        let ratio = w as f32 / h as f32;
        assert!(
            (ratio - 0.25).abs() < 0.01,
            "aspect ratio must be preserved"
        );
    }

    #[test]
    fn clamped_size_landscape_oversized_is_downscaled() {
        // wide image: 4096 x 1024, cap at 2048
        let (w, h) = clamped_size(4096.0, 1024.0, 2048);
        assert!(w <= 2048, "width must not exceed cap");
        assert!(h <= 2048, "height must not exceed cap");
        let ratio = w as f32 / h as f32;
        assert!((ratio - 4.0).abs() < 0.01, "aspect ratio must be preserved");
    }

    #[test]
    fn clamped_size_square_oversized_is_downscaled() {
        let (w, h) = clamped_size(4096.0, 4096.0, 2048);
        assert_eq!((w, h), (2048, 2048));
    }

    #[test]
    fn init_is_idempotent() {
        // Calling init() twice must not panic or corrupt the cache.
        init();
        init();
        let img = resolve("mdi:lightbulb");
        let fallback = resolve("mdi:help-circle");
        let img_pixels = img
            .to_rgba8()
            .expect("lightbulb image must have rgba8 pixel data");
        let fb_pixels = fallback
            .to_rgba8()
            .expect("fallback image must have rgba8 pixel data");
        assert_ne!(
            img_pixels.as_bytes(),
            fb_pixels.as_bytes(),
            "after double-init, lightbulb must still differ from fallback"
        );
    }

    #[test]
    fn resolved_images_have_non_zero_dimensions() {
        ensure_init();
        for id in &["mdi:lightbulb", "mdi:thermometer", "mdi:help-circle"] {
            let img = resolve(id);
            assert!(img.size().width > 0, "{id}: width must be > 0");
            assert!(img.size().height > 0, "{id}: height must be > 0");
        }
    }

    /// Phase 6 dashboard icon-registry expansion regression guard.
    ///
    /// Every `mdi:*` identifier referenced by `examples/dashboard.yaml` /
    /// `fixture_dashboard()` MUST resolve to a real (non-fallback) raster.
    /// Previously the bridge fell back to `mdi:help-circle` for any name
    /// other than the three Phase 1 icons, producing a question-mark glyph
    /// on every Phase 6 tile. This test pins each name to a distinct raster.
    #[test]
    fn phase6_mdi_icons_all_resolve_to_non_fallback_rasters() {
        ensure_init();
        let fallback = resolve("mdi:help-circle");
        let fb_pixels = fallback
            .to_rgba8()
            .expect("fallback icon must have rgba8 pixel data");

        for id in &[
            "mdi:fan",
            "mdi:door-closed-lock",
            "mdi:garage",
            "mdi:shield-home",
            "mdi:lightning-bolt",
            "mdi:lightning-bolt-circle",
            "mdi:camera",
            "mdi:ceiling-light",
            "mdi:thermostat",
            "mdi:television-play",
            "mdi:window-shutter",
            "mdi:home-assistant",
            "mdi:motion-sensor",
        ] {
            let img = resolve(id);
            let img_pixels = img
                .to_rgba8()
                .expect("phase6 icon: image must have rgba8 pixel data");
            assert_ne!(
                img_pixels.as_bytes(),
                fb_pixels.as_bytes(),
                "{id} must resolve to a real raster, not the help-circle fallback"
            );
            assert!(img.size().width > 0, "{id}: width must be > 0");
            assert!(img.size().height > 0, "{id}: height must be > 0");
        }
    }
}
