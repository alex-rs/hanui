---
name: ha-state-engine
description: Design and implement the Rust Home Assistant state engine that replaces Lovelace hass object with typed, batched, subscription-driven state for Slint.
allowed-tools: Read, Write, Edit, MultiEdit, Glob, Grep, Bash
---

# Home Assistant State Engine

## Purpose

Implement the Rust state layer that feeds Slint widgets. This replaces Lovelace `hass` with a typed, efficient, subscription-driven store.

## Responsibilities

WebSocket lifecycle, authentication, state subscription, entity cache, typed entity normalization, derived state, visible-entity filtering, update batching, reconnect recovery, stale/offline indicators, fixture mode.

The Slint UI must not parse arbitrary HA JSON.

## Architecture

```text
HA WebSocket → HaClient → EntityStore → DerivedStateMapper → VisibleSubscriptionIndex → SlintViewModelBridge
```

## Core types

```rust
pub type EntityId = String;

#[derive(Clone, Debug)]
pub struct EntityState {
    pub entity_id: EntityId,
    pub domain: String,
    pub object_id: String,
    pub state: String,
    pub friendly_name: String,
    pub unit: Option<String>,
    pub icon: Option<String>,
    pub area: Option<String>,
    pub device_class: Option<String>,
    pub attributes: serde_json::Value,
    pub unavailable: bool,
}

#[derive(Clone, Debug)]
pub struct DerivedEntityState {
    pub entity_id: EntityId,
    pub title: String,
    pub primary_value: String,
    pub secondary_value: Option<String>,
    pub icon_key: String,
    pub active: bool,
    pub unavailable: bool,
    pub warning: bool,
}
```

## Update model

HA events arrive → update EntityStore → mark affected entities dirty → coalesce for 16–100 ms → push only visible widget view-model changes.

## Reconnect behavior

Mark offline, keep last state, show stale indicator, reconnect with exponential backoff, resync full state, diff cache, push changed visible widgets.

## Tests

Entity ID parsing, unavailable/unknown handling, light/switch active state, sensor unit formatting, dirty batching, visible-only updates, reconnect diff, fixture loading.
