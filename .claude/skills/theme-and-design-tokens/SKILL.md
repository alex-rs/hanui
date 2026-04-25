---
name: theme-and-design-tokens
description: Create a shared Slint theme/token system that maps Home Assistant/Lovelace CSS variables to native colors, spacing, typography, and state styles.
allowed-tools: Read, Write, Edit, MultiEdit, Glob, Grep, Bash
---

# Theme and Design Tokens

## Purpose

Implement the dashboard visual system. Preserve Lovelace/Home Assistant style semantics without carrying CSS or browser styling into native code.

## Responsibilities

Global Slint theme tokens, light/dark mode, HA theme variable mapping, typography scale, spacing scale, card surfaces, state colors, domain colors, contrast rules.

## Core rule

Widgets must not define arbitrary one-off colors or spacing.

Widgets consume semantic tokens: `Theme.card-background`, `Theme.primary-text`, `Theme.card-radius`.

## Recommended `theme.slint`

```slint
export global Theme {
    in property <color> background: #111418;
    in property <color> surface: #171a1f;
    in property <color> card-background: #1c1f24;
    in property <color> card-active-background: #263241;
    in property <color> primary-text: #f5f5f5;
    in property <color> secondary-text: #aeb4bd;
    in property <color> disabled-text: #6f7782;
    in property <color> divider: #303640;
    in property <color> accent: #03a9f4;
    in property <color> active: #fdd663;
    in property <color> success: #4caf50;
    in property <color> warning: #ff9800;
    in property <color> error: #f44336;
    in property <length> card-radius: 14px;
    in property <length> card-padding: 14px;
    in property <length> gap-sm: 6px;
    in property <length> gap-md: 10px;
    in property <length> gap-lg: 16px;
    in property <length> font-caption: 12px;
    in property <length> font-body: 15px;
    in property <length> font-title: 17px;
    in property <length> font-state: 26px;
}
```

## HA variable mapping

`--primary-background-color` → `Theme.background`; `--card-background-color`/`--ha-card-background` → `Theme.card-background`; `--primary-text-color` → `Theme.primary-text`; `--secondary-text-color` → `Theme.secondary-text`; `--accent-color` → `Theme.accent`; `--state-icon-active-color` → `Theme.active`.

## Accessibility

Active/inactive must not rely on color only. Unavailable must have text/icon change. Touch targets should be at least 44 px. Avoid low-contrast gray-on-gray.
