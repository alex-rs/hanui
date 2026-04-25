//! Static asset loading and icon resolution.
//!
//! Phase 1: `icons` maps `mdi:*` identifiers to embedded SVG bytes,
//! rasterized once at startup via `resvg` into `slint::Image` values cached
//! behind `Arc` pointers.  Icons exceeding the configured pixel limit are
//! downscaled at startup.  Call [`icons::init`] once before the first frame.

pub mod icons;
