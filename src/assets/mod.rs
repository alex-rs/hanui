//! Static asset loading and icon resolution.
//!
//! Phase 1: `icons.rs` maps `mdi:*` identifiers to embedded SVG/PNG bytes,
//! decoded once at startup into a cached image type — see TASK-007.
//! Icons exceeding the configured pixel limit are downscaled at startup.
