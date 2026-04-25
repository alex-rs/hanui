---
name: dashboard-layout-engine
description: Design and implement a native dashboard layout engine that maps Lovelace views, sections, grids, and card sizing into efficient Slint layouts.
allowed-tools: Read, Write, Edit, MultiEdit, Glob, Grep, Bash
---

# Dashboard Layout Engine

## Purpose

Implement native dashboard layout behavior. Preserve dashboard intent in a predictable, native, low-overhead layout; do not clone Lovelace masonry exactly.

## Responsibilities

Dashboard schema, view/section/page structure, widget placement, responsive rules, density modes, visible widget calculation, pagination/virtualization, card sizing hints, unsupported Lovelace layout fallback.

## Model

```text
Dashboard
  View[]
    Section[]
      Widget[]
```

Do not render one giant dashboard on low-power hardware.

## Core types

```rust
pub struct Dashboard { pub views: Vec<View>, pub default_view: String }
pub struct View { pub id: String, pub title: String, pub icon: Option<String>, pub sections: Vec<Section>, pub layout: ViewLayout }
pub struct Section { pub id: String, pub title: Option<String>, pub widgets: Vec<WidgetConfig> }
pub enum ViewLayout { Grid, Sections, MasonryCompat }
pub struct WidgetLayoutHint { pub min_columns: u8, pub preferred_columns: u8, pub max_columns: u8, pub min_rows: u8, pub preferred_rows: u8, pub max_rows: u8 }
```

## Lovelace mapping

Dashboard → `Dashboard`; view → `View`; section → `Section`; card → `WidgetConfig`; `getCardSize()` → rows; `getGridOptions()` → columns/rows; masonry → compatibility placement.

## Responsive breakpoints

Narrow: 1–2 columns; medium: 3–4 columns; wide: 5–8 columns; wall panel: fixed configured columns.

## Density modes

Compact: minimal state/toggle. Normal: standard card. Expanded: attributes/features visible.

## Virtualization

Render only active page/section, avoid all-widget animation, avoid full dashboard model churn, keep hidden widgets in Rust config.
