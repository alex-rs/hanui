---
name: render-optimization
description: Optimize Slint widgets and Rust state flow for low CPU/GPU/RAM use on Raspberry Pi 4 and Orange Pi Zero 3-class SBCs.
allowed-tools: Read, Write, Edit, MultiEdit, Glob, Grep, Bash
---

# Render Optimization

## Purpose

Review and implement UI performance for low-power SBC dashboards.

Target hardware: Raspberry Pi 4 4GB, Orange Pi Zero 3 2GB, similar ARM SBCs.

## Goals

Low idle CPU, 30 FPS target unless justified, short state-driven animations only, predictable bounded memory.

## Core rules

1. Do not update Slint on every HA event.
2. Batch state updates in Rust.
3. Render only visible widgets.
4. Keep component trees shallow.
5. Avoid continuous animations.
6. Avoid heavy shadows, filters, and gradients.
7. Downsample charts before UI.
8. Preprocess icons/assets.
9. Use snapshots for cameras unless live video is essential.
10. Prefer page/section navigation over endless scrolling on 2GB boards.

## Slint rules

Prefer simple `Rectangle`, `Text`, pre-sized `Image`, `Path` for simple graphs, small models, static bindings.

Avoid deeply nested repeated layouts, large dynamic images, per-frame property changes, large icon font runtime, continuously animating gradients/spinners, hidden view rendering.

## Defaults

Raspberry Pi 4: batch latency 50 ms, max chart points 240, max visible widgets 60, simple animations only.

Orange Pi Zero 3: batch latency 100 ms, max chart points 120, max visible widgets 24–36, animations mostly off.

## Chart rules

Precompute charts in Rust. Do not pass raw HA history responses to Slint.

## Camera rules

Default to snapshots, explicit refresh interval, tap to details. Avoid multiple live streams, high-res decode on UI thread, and refreshing hidden cameras.
