---
name: project-architecture
description: Maintain the Rust + Slint native Home Assistant dashboard architecture, module boundaries, repo layout, and implementation roadmap.
allowed-tools: Read, Write, Edit, MultiEdit, Glob, Grep, Bash
---

# Project Architecture

## Product goal

Build a low-power native dashboard for Home Assistant that runs on SBCs without WebKit, Chromium, Electron, or a browser DOM.

Primary targets: Raspberry Pi 4 4GB, Orange Pi Zero 3 2GB, DietPi, Raspberry Pi OS Lite, Armbian, Debian minimal.

## System architecture

```text
Slint Native UI
  ↑ typed view models + callbacks
UI Bridge Layer
  ↑
Dashboard Runtime Core: layout engine / state engine / action dispatcher
  ↑        ↑             ↑
HA Client Config Loader  Asset/Icon Cache
```

## Repository layout

```text
ha-native-dashboard/
  Cargo.toml
  build.rs
  config/dashboard.yaml
  fixtures/ha-states.json
  src/
    main.rs
    app.rs
    ha/{client,auth,websocket,entity,store,normalize,fixtures}.rs
    actions/{dispatcher,schema,service_call,safety}.rs
    dashboard/{schema,loader,layout,validation,view_model}.rs
    ui/{bridge,models,callbacks}.rs
    assets/{icons,images}.rs
    platform/{config,health,watchdog}.rs
  ui/
    app.slint
    theme.slint
    components/{card,section,icon,loading,error_state}.slint
    widgets/{entity_tile,light_tile,sensor_tile,climate_tile,switch_tile,media_tile,camera_snapshot_tile,graph_tile}.slint
  packaging/systemd/ha-native-dashboard.service
  docs/{ARCHITECTURE,DASHBOARD_SCHEMA,ROADMAP}.md
```

## Module ownership

- `ha`: Home Assistant communication and entity state. Must not import Slint.
- `actions`: action mapping and HA service calls. Must not import Slint components.
- `dashboard`: schema, layout, validation, widget configs. Must not talk directly to HA network.
- `ui`: mapping to Slint models and callbacks. May depend on `ha`, `actions`, and `dashboard`.
- `assets`: icons, images, caches.
- `platform`: health, watchdog, CLI args, platform config.

## Data flow

Startup: load config → connect HA → fetch states → normalize entities → build dashboard → compute visible view → create Slint UI → bind callbacks.

Runtime: HA event → EntityStore update → dirty visible widgets → batch update → Slint model update.

User action: Slint callback → UI Bridge → Action Dispatcher → HA service/local navigation → eventual HA state update.

## Quality gates

No WebView dependency, state engine tests pass, action dispatcher tests pass, fixture mode works, dashboard schema validates, low-power mode considered.
