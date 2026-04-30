# Hanui Performance Audit

Date: 2026-04-30

Scope: read-only audit of the native Slint Home Assistant dashboard with a focus on low-power SBC targets. The only repository change from this audit is this markdown file.

## Executive Summary

The main performance risk is not Slint itself. It is the amount of whole-application work done for small HA state changes:

- Every incremental HA entity update clones the entire entity map in `LiveStore`.
- Every dirty UI flush rebuilds every visible tile, then replaces all Slint models.
- Some rebuild work is proportional to all HA entities, not just configured dashboard widgets.
- Device profiles for Raspberry Pi / Orange Pi exist, but much of the runtime still uses the desktop profile.
- The main LiveBridge churn benchmark is currently broken, so the project lacks an active guard for the most important hot path.

For a low-power SBC, the highest-return work is to make the store update path incremental, then make the Slint model update path incremental, then wire the real device profile before runtime/client/UI setup.

## Commands Run

- `cargo test --no-run`
  - Result: passed.
  - Note: Cargo warned that `benches/churn.rs` and `benches/dispatcher.rs` are present in multiple build targets.

- `cargo test --features bench --test dispatcher_bench --no-run`
  - Result: passed.

- `cargo test --features bench --test churn --no-run`
  - Result: failed to compile.
  - `benches/churn.rs:216:22`: expected `Option<WidgetOptions>`, found `Vec<_>`.
  - `benches/churn.rs:225:25`: expected `ProfileKey`, found `String`.

## Priority Findings

### 1. Full Entity Map Clone On Every HA Event

Severity: high

Code:

- `src/ha/live_store.rs:617`
- `src/ha/live_store.rs:626`
- `src/ha/live_store.rs:635`

`LiveStore::apply_event` clones the complete `HashMap<EntityId, Entity>` for each incremental state update:

```rust
let mut new_map: HashMap<EntityId, Entity> = (**guard).clone();
```

That makes one small HA state change proportional to the full HA entity count. At profile limits this is expensive:

- Orange Pi profile: up to 2,048 entities.
- Raspberry Pi profile: up to 4,096 entities.
- Desktop profile: up to 16,384 entities.

At 50 events/sec, even a 2,000 entity store means repeatedly cloning thousands of entities and their map structure. This is the highest risk CPU and allocation bottleneck found.

There is a second-order issue during reconnect catch-up. `src/ha/client.rs:1350` applies a fresh snapshot, then `diff_and_broadcast` broadcasts changed entities. If those broadcasts call back into `apply_event`, a reconnect with many changed entities can become effectively O(changed_entities * total_entities).

Recommendations:

- Replace clone-on-write for incremental events with in-place mutation under a `RwLock<HashMap<...>>`.
- Keep full snapshot replacement for reconnect/bootstrap, but do not route reconnect diff through an update path that clones the full map per changed entity.
- Batch event application at the UI flush cadence if the UI only needs latest values every 80 ms.
- If immutable snapshots are required, consider a persistent map with structural sharing rather than cloning the entire `HashMap`.
- Add a benchmark that applies 1,000 state changes to a 2,048 and 4,096 entity store and records allocations/time.

Expected impact: large CPU and memory allocation reduction under normal HA event traffic.

### 2. UI Flush Rebuilds All Tiles And Replaces All Models

Severity: high

Code:

- `src/ui/bridge.rs:229`
- `src/ui/bridge.rs:1010`
- `src/ui/bridge.rs:1041`
- `src/ui/bridge.rs:1057`
- `src/lib.rs:611`
- `src/lib.rs:628`

`LiveBridge` drains pending entity IDs, but does not use those IDs for targeted updates. Instead, every non-empty flush calls `build_tiles`, then `SlintSink::write_tiles` replaces the light, sensor, and entity models with fresh `VecModel`s.

The current path does this at up to 12.5 Hz:

1. Drain pending entity IDs.
2. Rebuild every tile VM from dashboard config and store state.
3. Convert Rust strings/icons/placement into Slint VM rows.
4. Split tile VMs by kind.
5. Replace all three Slint models.

`SlintSink::write_tiles` explicitly does the conversion on the Slint UI thread via `invoke_from_event_loop`. The comments note that string-to-`SharedString` work and icon `Arc` bumps happen there.

Recommendations:

- Keep stable `VecModel<T>` instances and update changed rows with `set_row_data`.
- Build an index at load time: `EntityId -> [(model_kind, row_index, widget_id)]`.
- Split tile data into static fields and dynamic fields:
  - Static: widget title, icon, placement, kind, precision, unit.
  - Dynamic: state text, unavailable flag, pending flag, last changed display.
- Use full rebuild only for config reload, view switch, or full resync.
- Move as much conversion as possible off the UI thread, then invoke only the minimal row update.

Expected impact: large reduction in UI thread work and allocation pressure, especially when only one or two entities change.

### 3. `build_tiles` Performs A Full Store Walk On Every Rebuild

Severity: high

Code:

- `src/ui/bridge.rs:233`
- `src/ui/bridge.rs:240`
- `src/ha/live_store.rs:714`

`build_tiles` starts by walking the entire store with `store.for_each`, only to count entities and log a diagnostic. This makes each UI rebuild proportional to total HA entity count before any widget work starts.

For a dashboard with 20 visible widgets and 2,000 HA entities, this means the flush cost scales with 2,000 entities even when only 20 can be displayed.

Recommendations:

- Remove this diagnostic from the hot path.
- If entity count is needed, maintain a counter in `LiveStore`.
- Gate the full walk behind debug-only tracing or a rare diagnostics command.
- Ensure tile rebuild cost is proportional to widget count, not HA entity count.

Expected impact: meaningful CPU reduction on installs with many HA entities and few dashboard widgets.

### 4. SBC Profiles Exist But Runtime Uses Desktop Defaults

Severity: high

Code:

- `src/lib.rs:85`
- `src/lib.rs:89`
- `src/dashboard/profiles.rs:393`
- `src/dashboard/profiles.rs:437`
- `src/dashboard/profiles.rs:486`
- `src/dashboard/profiles.rs:538`

The project has dedicated profiles for Raspberry Pi 4 and Orange Pi Zero 3, including lower worker counts, smaller entity caps, lower animation caps, and lower widget limits. However, the main runtime uses `PROFILE_DESKTOP` while building the Tokio runtime:

```rust
worker_threads(PROFILE_DESKTOP.tokio_workers)
```

Other runtime paths also appear to use desktop constants for image limits, WebSocket payload caps, smoke-test CPU budget, and animation budget defaults.

This undermines the purpose of the device profiles. A dashboard configured for an SBC can still boot with desktop limits and desktop background concurrency.

Recommendations:

- Load dashboard config early enough to select `device_profile` before creating the Tokio runtime.
- Pass the selected `DeviceProfile` through runtime setup, HA client setup, icon rasterization, bridge setup, validation, and test budgets.
- Treat use of `PROFILE_DESKTOP` outside tests and desktop default paths as suspicious.
- Add a test that loads an `opi-zero3` dashboard and asserts the runtime/client/UI setup receives the OPI profile values.

Expected impact: lower baseline CPU, memory, and scheduling overhead on SBC deployments.

### 5. Dashboard Validation Is Not Wired Into Loading

Severity: high

Code:

- `src/dashboard/loader.rs:174`
- `src/dashboard/loader.rs:200`
- `src/dashboard/validate.rs:352`

The loader currently contains a validation stub:

```rust
let issues: Vec<Issue> = vec![];
```

The real validator exists in `src/dashboard/validate.rs`, but the loader does not call it. That means performance-protecting constraints such as widget caps, layout bounds, camera interval policy, and service allowlist checks are not enforced at load time.

This matters for SBC performance because profile limits only help if invalid dashboards are rejected before runtime.

Recommendations:

- Select the profile during load.
- Call `validate(&dashboard, selected_profile)`.
- Fail fast for over-budget widget counts, invalid spans, excessive refresh intervals, and disallowed service actions.
- Add fixture tests for `rpi4` and `opi-zero3` dashboards that exceed profile limits and must fail.

Expected impact: prevents pathological configs from reaching the UI hot path.

### 6. One Subscriber Task Per Visible Entity

Severity: medium

Code:

- `src/ui/bridge.rs:880`
- `src/ui/bridge.rs:888`
- `src/ui/bridge.rs:931`

`LiveBridge::spawn` starts one Tokio task per unique visible entity. At current profile widget caps this is acceptable, but it creates scheduler overhead and one broadcast receiver per visible entity.

The larger issue is lag handling:

- `src/ui/bridge.rs:956`
- `src/ui/bridge.rs:960`

When a subscriber lags, it loops over all subscribed IDs and locks the pending map once per ID. If multiple subscribers lag at the same time, the recovery path can become noisy and approach O(n^2) behavior.

Recommendations:

- Use a central fan-in task that receives entity updates and marks pending IDs.
- On lag, set a single "full resync needed" flag instead of marking every entity from every lagging subscriber.
- If keeping the current structure, batch the pending-map lock once per lag event.

Expected impact: lower scheduler and lock overhead during bursts.

### 7. WebSocket Subscribes To All Events

Severity: medium

Code:

- `src/ha/client.rs:1226`
- `src/ha/client.rs:1239`
- `src/ha/client.rs:1505`
- `src/ha/client.rs:1515`

The WebSocket client subscribes with `event_type: None`, which means all HA events are delivered. The handler only uses `state_changed` and a small set of service lifecycle events; unknown events are ignored after parsing.

On a busy HA install, receiving all events increases JSON parse, dispatch, and allocation pressure even when most events cannot affect the dashboard.

Recommendations:

- Prefer filtered subscriptions for `state_changed` and the specific service lifecycle events needed.
- If HA requires separate subscriptions, add explicit ACK handling for each.
- Optimize inbound deserialization so unknown events do not require cloning/parsing large raw payloads.
- Add a synthetic test with a high ratio of irrelevant events to measure parse overhead.

Expected impact: lower background CPU on busy HA instances.

### 8. Inbound Message Deserialization Clones Raw JSON

Severity: medium

Code:

- `src/ha/protocol.rs`

The custom `InboundMsg` deserializer parses inbound JSON into `serde_json::Value`, clones that value, and then attempts to deserialize the known message shape. This preserves unknown payloads but adds allocation and clone work for every inbound message.

Recommendations:

- Deserialize into a borrowed/raw envelope first, inspect `type`, and only materialize the full payload needed for that variant.
- Keep raw unknown payload support, but avoid cloning known messages.
- Benchmark deserialization for a typical `state_changed` event payload.

Expected impact: moderate CPU/allocation reduction under event load.

### 9. The Main Churn Benchmark Does Not Compile

Severity: medium

Code:

- `benches/churn.rs:1`
- `benches/churn.rs:19`
- `benches/churn.rs:216`
- `benches/churn.rs:225`

`benches/churn.rs` describes the exact kind of benchmark this project needs for LiveBridge and UI churn, but it currently fails to compile due to stale schema usage.

Recommendations:

- Update `WidgetConfig.options` usage to `None` or `Some(WidgetOptions { ... })`.
- Update `DashboardConfig.device_profile` usage to `ProfileKey::Desktop` or the desired profile key.
- Run this benchmark in CI or nightly performance checks.
- Add benchmark cases for:
  - 20 widgets / 2,048 entities / OPI profile.
  - 32 widgets / 4,096 entities / RPI profile.
  - bursty updates with only 1 changed visible entity.
  - reconnect diff with many changed entities.

Expected impact: restores a guardrail for the most important runtime path.

### 10. SBC Smoke Test Does Not Cover The UI Hot Path

Severity: medium

Code:

- `tests/smoke/sbc_cpu.rs:5`
- `tests/smoke/sbc_cpu.rs:207`
- `tests/smoke/sbc_cpu.rs:218`
- `tests/smoke/sbc_cpu.rs:237`

The smoke test measures WebSocket ingestion and `LiveStore`, but not `LiveBridge`, Slint model conversion, or UI thread updates. It also injects updates across only 10 unique entities and uses `PROFILE_DESKTOP.cpu_smoke_budget_pct`.

Recommendations:

- Add a headless bridge test with a fake `TileSink` that records full rebuild time and allocation count.
- Add a Slint-enabled test or benchmark that measures `SlintSink::write_tiles` conversion cost.
- Use selected SBC profile budgets, not desktop budget, for SBC smoke tests.
- Include at least one scenario where total HA entities are much larger than visible widgets.

Expected impact: performance regressions in the current bottleneck become visible before deployment.

### 11. Pending Spinner Uses Per-Frame Animation Tick And Trig

Severity: low to medium

Code:

- `ui/slint/card_base.slint:351`
- `ui/slint/card_base.slint:365`
- `ui/slint/card_base.slint:391`
- `ui/slint/card_base.slint:392`

Pending cards instantiate a spinner driven by `animation-tick()` and compute `cos`/`sin` for position. The subtree only exists while pending, which is good, but multiple simultaneous pending cards can produce continuous per-frame work under the software renderer.

Recommendations:

- Use a stepped spinner at 10-15 fps on SBC profiles.
- Consider a small pre-rendered spinner sprite or icon state sequence.
- Enforce a hard cap on active pending animations.
- Verify that `AnimationBudget.active-count` is updated by runtime code; otherwise, `AnimationBudget.at-capacity` is not an effective throttle.

Expected impact: small to moderate CPU reduction during service-action bursts.

### 12. Layout Packer Is Quadratic But Probably Not Hot

Severity: low

Code:

- `src/dashboard/layout.rs:95`
- `src/dashboard/layout.rs:119`
- `src/dashboard/layout.rs:197`

The layout packer uses a vector of occupied cells and checks `.contains` while placing widgets. The file documents the O(n^2 * max_span) behavior. This is acceptable if layout runs only at load/reload time and widget caps are enforced.

Recommendations:

- Do not prioritize this before the store and UI hot paths.
- Once validation is wired, profile caps should keep this bounded.
- If dashboard reload becomes frequent, replace occupied-cell `Vec` with a row bitset or fixed grid occupancy array.

Expected impact: low unless dashboards reload often or validation remains disabled.

### 13. Experimental HTTP Module Is Inactive And Not Build-Ready

Severity: medium if enabled, low while inactive

Code:

- `src/ha/mod.rs:7`
- `src/ha/mod.rs:13`
- `src/ha/http.rs:55`
- `src/ha/http.rs:56`
- `src/ha/http.rs:249`
- `src/ha/http.rs:252`

`src/ha/http.rs` exists in the working tree but is not exported by `src/ha/mod.rs`, so it is not active in the build inspected by Cargo. If enabled as-is, it appears not build-ready:

- It imports `lru` and `reqwest`, but those dependencies are not present in `Cargo.toml`.
- It references profile fields such as `http_rate_limit_per_host_qps`, `http_retry_budget`, and `http_request_timeout_ms` that do not exist in the current `DeviceProfile`.
- Its cache state is split across separate mutexes even though comments imply shared protection.

Recommendations:

- Decide whether this module is intended for the current product.
- If yes, add the missing dependencies and profile fields intentionally, then compile it under CI.
- Keep HTTP client/cache instances shared; avoid per-widget clients.
- Store cache entries and byte accounting under one lock or one coherent cache state object.
- If no, remove or park it outside the active source tree to avoid accidental integration.

Expected impact: prevents future compile breaks and avoids adding background network/cache overhead accidentally.

## Positive Observations

- `LiveBridge` throttles UI flushes to an 80 ms interval and uses `MissedTickBehavior::Skip`.
- The pending map stores latest dirty entity IDs instead of queueing every event.
- The UI is forced to Slint's software renderer, which is appropriate for predictable SBC deployment.
- `EntityId` and `WidgetId` use `SmolStr`, which is a good fit for repeated identifiers.
- Entity fields use `Arc<str>` and attribute maps use `Arc`, so entity clones are cheaper than deep string clones.
- Dispatcher paths mostly use non-blocking `try_send`, which avoids backpressuring the UI thread.
- Icons are rasterized once during initialization rather than per frame.
- The Slint files generally avoid always-on animations; pending spinner work is conditional.

## Recommended Implementation Order

1. Fix `LiveStore::apply_event` so a single HA event does not clone the full entity map.
2. Remove the store-wide `for_each` diagnostic from `build_tiles`.
3. Replace full Slint model replacement with targeted row updates.
4. Wire selected `DeviceProfile` through runtime, client, bridge, icon, validation, and tests.
5. Call the real dashboard validator from the loader.
6. Fix and run `benches/churn.rs`.
7. Add a bridge/UI performance smoke test for SBC profiles.
8. Optimize WebSocket subscription filtering and inbound deserialization.
9. Reduce subscriber-task and lag-recovery overhead.
10. Tune Slint pending animations for software-rendered SBC profiles.

## Suggested Performance Targets

These are pragmatic targets for a low-power SBC dashboard:

- Initial dashboard build: under 250 ms for OPI profile, under 500 ms for RPI profile.
- Incremental visible entity update: under 5 ms CPU outside Slint render, no full store clone.
- UI model update: only changed rows updated for normal state events.
- Steady idle CPU: near zero when HA is quiet and no animations are active.
- Burst behavior: 50 HA events/sec should not allocate proportional to total entity count per event.
- Reconnect snapshot diff: O(total_entities + changed_entities), not O(total_entities * changed_entities).

## Final Assessment

The project has a solid structure for an SBC dashboard, especially with explicit device profiles and a throttled bridge. The current bottlenecks are mostly architectural hot-path issues rather than isolated micro-optimizations.

The most important change is to stop doing whole-store and whole-model work for single-entity updates. Once the store and UI bridge become incremental, the existing profile limits and software-rendered Slint UI should be much easier to keep within SBC CPU and memory budgets.
