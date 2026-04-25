---
name: ha-action-dispatcher
description: Map native Slint widget callbacks to Home Assistant actions and services, replacing Lovelace tap/hold/double-tap action handling.
allowed-tools: Read, Write, Edit, MultiEdit, Glob, Grep, Bash
---

# Home Assistant Action Dispatcher

## Purpose

Implement user actions from Slint widgets to Home Assistant. The UI emits typed callbacks. Rust maps callbacks to HA service calls, navigation, details panels, or local actions.

## Responsibilities

Tap/hold/double-tap action mapping, service call construction, domain-specific toggle behavior, validation, unavailable/offline handling, optimistic updates, action retry/offline queue, logging.

## Core rule

Slint components must not know Home Assistant service names.

Good: `clicked => { root.primary-action(); }`

Rust: `dispatcher.dispatch(widget_id, ActionTrigger::Tap).await?;`

## Core types

```rust
pub enum ActionTrigger { Tap, Hold, DoubleTap, SliderChanged, StepUp, StepDown }

pub enum UserAction {
    None,
    Toggle { entity_id: String },
    MoreInfo { entity_id: String },
    Navigate { path: String },
    CallService { domain: String, service: String, entity_id: Option<String>, data: serde_json::Value },
    SetValue { entity_id: String, value: serde_json::Value },
}

pub struct WidgetActions {
    pub tap: UserAction,
    pub hold: UserAction,
    pub double_tap: UserAction,
}
```

## Lovelace mapping

`toggle` → `Toggle`; `more-info` → `MoreInfo`; `navigate` → `Navigate`; `call-service` → `CallService`; `none` → `None`.

## Safety

Require explicit confirmation or explicit controls for unlock, garage/cover open, disarm alarm, destructive scripts, dangerous scenes, unknown arbitrary service calls.

## Offline behavior

Local navigation should work; service calls should fail visibly or queue if configured; queued actions must show pending state; never silently pretend failure succeeded.
