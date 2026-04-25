---
name: html-to-slint-widget
description: Convert HTML/CSS/JavaScript dashboard widgets, including Home Assistant Lovelace custom cards, into native Slint components with Rust-backed state.
allowed-tools: Read, Write, Edit, MultiEdit, Glob, Grep, Bash
---

# HTML to Slint Widget Migration

## Purpose

Convert existing HTML/CSS/JavaScript widgets into native Slint components for a low-power Home Assistant dashboard.

## Core rule

Do not translate browser code line-by-line. Translate intent.

| Web concept | Native replacement |
|---|---|
| HTML structure | Slint component tree |
| CSS layout | Slint layout primitives |
| CSS variables | Slint theme tokens |
| DOM events | Slint callbacks |
| JavaScript computed state | Rust helpers or Slint bindings |
| `hass` object | typed Rust entity state |
| HA service calls | Rust action dispatcher |

## Workflow

1. Inspect source files.
2. Identify inputs, entities, states, actions, browser-only dependencies, CSS variables, layout behavior.
3. Classify: good native candidate, partial candidate, or poor candidate.
4. Create or update Slint component.
5. Create or update Rust adapter model.
6. Wire callbacks to action dispatcher.
7. Add fixture state.

## Portability classes

Good: entity tile, switch, light tile, sensor tile, thermostat, alarm keypad, media controls, simple lists, simple graphs.

Partial: canvas charts, animated CSS cards, third-party JS cards, camera cards, weather cards with complex SVG.

Poor: arbitrary HTML renderer, iframe card, full markdown/HTML renderer, dashboard editor, drag/drop builder, browser-only canvas app.

## Slint component rules

Each widget should expose typed `in property` values, emit callbacks instead of calling HA directly, support unavailable/error/loading states, support compact/normal/expanded density, avoid browser/runtime dependencies, avoid large dynamic assets, and keep the component tree shallow.

## Home Assistant custom card mapping

Web cards commonly expose `setConfig(config)`, `set hass(hass)`, `getCardSize()`, and `getGridOptions()`.

Native equivalents: `NativeWidgetConfig`, `EntitySnapshot`, `WidgetLayoutHint`, and typed callbacks.

## Migration report

Report preserved behavior, changed behavior, unsupported browser behavior, memory risk, CPU/GPU risk, and recommended target.
