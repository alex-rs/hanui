# Implementation Phases

Detailed expansion of `docs/ROADMAP.md`. Each phase is sized so it can be merged independently and exercised on the dev VM (and where applicable, on a target SBC) before the next phase starts.

This document was reviewed against `docs/ARCHITECTURE.md`, `docs/ROADMAP.md`, and `docs/DASHBOARD_SCHEMA.md` by external reviewers (`codex`, `opencode`); their feedback is incorporated.

Conventions used below:
- **Objective** — the one-sentence outcome.
- **Deliverables** — concrete artifacts produced.
- **Tasks** — ordered work items.
- **Acceptance criteria** — testable signals of completion.
- **Out of scope** — explicit non-goals to prevent creep.
- **Dependencies / risks** — what can block or break the phase.

Cross-cutting principles:
- Don't create empty modules ahead of need. Add a module only in the phase whose deliverables populate it. (`docs/ARCHITECTURE.md` lists the eventual module map; PHASES.md schedules when each appears: `src/ha`/`src/dashboard`/`src/ui`/`src/assets` in Phase 1, `src/platform` first in Phase 2 with `config.rs`+`status.rs`, `src/actions` in Phase 3, `src/platform/device.rs` filled in Phase 5.)
- Don't hard-code structures that a later phase must rewrite. Where a Phase N feature will be data-driven by YAML in Phase N+M, the structure used in Phase N must be the same shape — only the source changes.
- Shared abstractions (HTTP client, store trait, action map, gesture timings) are introduced in the first phase that needs them, not the second.
- The dashboard YAML schema (`docs/DASHBOARD_SCHEMA.md`) is the source of truth for user-visible field names. Internal types may use different names for derived/computed values, but anything documented in the YAML schema is reflected in `src/dashboard/schema.rs` verbatim.
- **Every resource has a numeric cap.** Unbounded means broken on a 512 MB SBC. Caps live in the Performance budgets table below and are referenced from `DeviceProfile`. Adding a feature without a cap is a review-blocker.
- **Hot paths allocate nothing.** No `serde_json::Value` traversal in tile renders, no string formatting per frame, no per-event `HashMap` clones. Parse-to-typed once on ingress, store as `Arc` where shared, copy small `Copy` view models to Slint.

---

## Performance budgets

This table is the source of truth for numeric caps. Every phase task that allocates, animates, decodes, or polls references a value from here. `DeviceProfile` (defined in Phase 4) carries the profile-specific column for the active board.

| Resource | rpi4 | opi_zero3 | desktop | Where enforced |
|---|---|---|---|---|
| Target frame period p95 | ≤ 33 ms (30 fps) | ≤ 50 ms (20 fps) | ≤ 16 ms (60 fps) | Phase 5 health socket |
| Idle CPU | < 5 % | < 10 % | < 5 % | Phase 5 acceptance |
| Idle RSS | < 80 MB | < 60 MB | < 120 MB | Phase 5 acceptance |
| Tokio worker threads | 2 | 2 | 4 | Phase 1 runtime config |
| Max entities (store) | 4 096 | 2 048 | 16 384 | Phase 1 `MemoryStore`, Phase 2 `LiveStore` |
| Max widgets per view | 32 | 20 | 64 | Phase 4 validator |
| Max simultaneous animations | 3 | 2 | 8 | Phase 4 layout, Phase 1 press fx |
| Animation framerate cap | 30 fps | 20 fps | 60 fps | Phase 1 card_base, Phase 4 view-switcher |
| Max simultaneous camera streams | 2 | 1 | 4 | Phase 6b decoder pool |
| Max image px (longest dim) | 1280 | 800 | 2048 | Phase 1 icons, Phase 6b camera/artwork |
| Touch input expected | true | true | false | Phase 4 swipe instantiation |
| WS payload cap | 16 MiB | 8 MiB | 16 MiB | Phase 2 tungstenite config |
| Snapshot-buffer events | 5 000 | 2 500 | 10 000 | Phase 2 client; on overflow → drop and full resync |
| Visible-updates queue | latest-only per entity | latest-only | latest-only | Phase 2 bridge (overwrite, no FIFO) |
| Pending optimistic per entity | 4 | 4 | 8 | Phase 3 dispatcher; reject new at cap |
| Pending optimistic global | 64 | 32 | 256 | Phase 3 dispatcher |
| Offline action queue | 64 | 32 | 256 | Phase 3 queue |
| AttributesBody cap | 32 attrs / 256 chars | 32 / 256 | 64 / 512 | Phase 3 more-info |
| Toast auto-dismiss | 4 s | 4 s | 4 s | Phase 3 toast (also tap-to-dismiss) |
| CPU smoke budget (QEMU) | 30 % | 50 % | 15 % | Phase 2 emulated churn smoke |
| Reconnect burst RSS | ≤ steady + 20 MB | ≤ steady + 20 MB | ≤ steady + 40 MB | Phase 2 soak |
| Camera interval default / min | 10 s / 5 s | 30 s / 10 s | 5 s / 1 s | Phase 6b schema validation |
| History window default / max | 6 h / 24 h | 3 h / 12 h | 24 h / 168 h | Phase 6b schema validation |
| HTTP cache total bytes | 32 MiB | 16 MiB | 128 MiB | Phase 6.0 http.rs |
| HTTP cache TTL | 5 min | 5 min | 10 min | Phase 6.0 http.rs |
| dep_index inline cap (SmallVec) | 8 (heap above) | 8 | 8 | Phase 6b visibility evaluator |
| Histogram buckets (frame period) | 100 fixed | 100 fixed | 100 fixed | Phase 5 metrics |
| SoC temp ceiling | 75 °C | 80 °C | n/a | Phase 5 thermal soak |
| Visibility re-eval | entity-indexed | entity-indexed | entity-indexed | Phase 6b evaluator (no scan-all) |
| Idle wake-ups per second | ≤ 12.5 (flush) + ≤ 1 (watchdog) | ≤ 12.5 + ≤ 1 | (same) | Phases 2 and 5 |
| Screen blanking | event-driven (no polling) | event-driven | configurable | Phase 5 (binary, not launcher) |

**Bootstrap**: until Phase 4 lands `DeviceProfile`, Phase 1–3 read these values from a `const DEFAULT_PROFILE: DeviceProfile = …` in `src/dashboard/profiles.rs` (the file lands in Phase 1 with the `desktop` preset and a `// TODO Phase 4: add rpi4/opi_zero3` marker). Phase 4 fills in the other presets and the YAML/autodetect selector. This avoids forward references to a Phase 4 type.

---

## Phase 1: Native shell

### Objective
Stand up a Rust + Slint application that renders a static dashboard from fixture data on the dev VM. No Home Assistant connectivity yet, but the data model and module boundaries are the same shape that Phase 2 will plug into.

### Deliverables
- Cargo project at repo root (single crate; defer workspace split).
- Modules created **only where they have content this phase**: `src/ha`, `src/dashboard` (with bootstrap `profiles.rs::DEFAULT_PROFILE = DESKTOP`; full struct + presets land in Phase 4), `src/ui`, `src/assets`. (`src/platform` first appears in Phase 2 for `config.rs`/`status.rs`; `src/actions` in Phase 3; `src/platform/device.rs` in Phase 5.)
- `src/ha/store.rs` defining the `EntityStore` trait + `MemoryStore` impl that both fixture (Phase 1) and live (Phase 2) sources implement — the trait is the API the UI bridge talks to.
- `src/dashboard/view_spec.rs`: a typed, hand-built `Dashboard` value that mirrors the YAML schema shape so the bridge already consumes data via the eventual loader's output. The hand-built default populates `tap_action` / `hold_action` / `double_tap_action` fields on each widget (even if Phase 1 doesn't dispatch them) so Phase 3 has data to wire.
- `src/assets/icons.rs`: icon resolver mapping `mdi:*` ids (or a subset) to embedded SVG/PNG assets.
- `ui/slint/` with `theme.slint` (design tokens), `card_base.slint`, and three tiles: `light_tile`, `sensor_tile`, `entity_tile`. Widget view models carry the schema-named `preferred_columns` / `preferred_rows` fields plus an internal `placement: { col, row, span_cols, span_rows }` value computed by Phase 1's trivial packer (single column at default density). The schema field names are user-visible; `placement` is an internal derived value the layout engine in Phase 4 will produce.
- Theme module mapping HA/Lovelace CSS variables to Slint tokens (dark mode first).
- Fixture loader producing an `EntityStore`-implementing source from `examples/ha-states.json`.
- A default view rendering at least one tile per kind.
- `cargo run` binary that opens a window on the dev VM.

### Tasks
1. Initialize Cargo project; pin `slint`, `serde`, `serde_json`, `tokio` (rt-multi-thread, macros), `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`, and a single timestamp crate (`jiff` preferred — `chrono` acceptable). Decide once; use it for `last_changed`/`last_updated`. No ad-hoc string timestamps. Tokio runtime is built with `worker_threads(profile.tokio_workers)` reading from `DEFAULT_PROFILE` (Phase 4 swaps the value source to YAML/autodetect; the field name is stable). No default-`num_cpus` runtime.
2. Create only the modules with deliverables this phase (`ha`, `dashboard`, `ui`, `assets`). `lib.rs` + `main.rs` split so logic is unit-testable without spawning the UI.
3. Define core entity types in `src/ha/entity.rs`: `EntityId(SmolStr)`, `Entity { id, state: Arc<str>, attributes: Arc<serde_json::Map<String, Value>>, last_changed, last_updated }`. Wrap the heavy fields in `Arc` so `Entity` is cheap to clone; cloning an `Entity` does not deep-copy attributes. `EntityKind` derived from the entity domain prefix is a `Copy` enum.
4. Define `src/ha/store.rs`: `trait EntityStore: Send + Sync { fn get(&self, id: &EntityId) -> Option<Entity>; fn for_each<F: FnMut(&EntityId, &Entity)>(&self, f: F); fn subscribe(&self, ids: &[EntityId]) -> tokio::sync::broadcast::Receiver<EntityUpdate>; }`. `get` returns an `Entity` — cheap because attributes are `Arc`'d. `for_each` is a visitor instead of `iter()` so impls can hold an internal lock without leaking iterator lifetimes. Subscribe uses `tokio::sync::broadcast` with bounded capacity (channel cap = `Performance budgets: visible-updates queue` — i.e., 1; lagging receivers get `RecvError::Lagged` and the bridge resyncs from `get`). Phase 2's `LiveStore` implements the same trait — a compile-time `fn _assert_store<S: EntityStore>() {}` test exercises both impls. `MemoryStore` enforces `DEFAULT_PROFILE.max_entities` (table-defined); `load` errors if the fixture exceeds it.
5. Implement `src/ha/fixture.rs`: `load(path) -> Result<MemoryStore>` populating the in-memory store from `examples/ha-states.json`. Verify (and extend if needed) the fixture so it covers at least one entity per Phase-1 tile kind. **Hot-path discipline**: the loader parses JSON once into typed records; the bridge consumes typed records, never `serde_json::Value`.
6. Define `src/dashboard/view_spec.rs`: typed `Dashboard`/`View`/`Section`/`Widget` matching the shape of `docs/DASHBOARD_SCHEMA.md`. Hand-build one default `Dashboard` value here — Phase 4 replaces the construction site, not the type.
7. Implement `src/assets/icons.rs`: resolve `mdi:*` ids to embedded assets. Ship a small starter set covering the icons used by the default view (lightbulb, thermometer, generic). Icons are **decoded once at startup** into `Arc<slint::Image>` and stored in a `OnceLock<HashMap<&'static str, Arc<slint::Image>>>` (perfect-hash via `phf` is for static byte slices and isn't compatible with runtime-decoded `Image` values). The resolver returns a clone of the `Arc`. No decode-on-first-use, no per-render decode. Icons exceeding `DEFAULT_PROFILE.max_image_px` are downscaled at startup.
8. Build `ui/slint/theme.slint` with: surface/background/elevated, text-primary/secondary, accent, state-on/off/unavailable, radii, spacing scale, font sizes.
9. Build `ui/slint/card_base.slint`: rounded background, padding, border, focus ring, press/hover visual feedback (no action wiring yet). Press animation duration is a constant capped at 150 ms; only the pressed tile animates (no parallel cards-wide ripple). The animation count is bookkept in a global `active_animation_count` property and gated against `DEFAULT_PROFILE.max_simultaneous_animations`; a card requesting an animation while at the cap renders the end-state without animating. The framerate is capped at `DEFAULT_PROFILE.animation_framerate_cap`.
10. Implement `light_tile`, `sensor_tile`, `entity_tile`. Each consumes a typed view-model struct that includes `layout` fields (used as static grid placement this phase).
11. Implement `src/ui/bridge.rs`: take an `EntityStore + Dashboard` and emit Slint view models. **Parse `serde_json::Value` only at ingress**, store typed view-model fields, then per-frame the bridge only clones cheap `Copy`/`Arc<str>` values into Slint properties. Enforce in code review: any `Value` access in `src/ui/` is a bug.
12. Wire `main.rs`: init tracing, load fixture, build the hand-coded `Dashboard`, build view models, instantiate window, run event loop.
13. Add CI: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, plus a grep step that fails on inline hex colors in **all** `ui/slint/**/*.slint` files (tiles, `card_base`, future shared components) with a documented escape hatch (`// theme-allow:` line marker for cases like the theme file itself). One smoke test under `tests/` loads the fixture and asserts at least one entity per tile kind.

### Acceptance criteria
- `cargo run` on the dev VM opens a window showing the static dashboard with correct names, states, and icons drawn from the fixture.
- Tile colors come from theme tokens — verified by the CI grep check.
- Press feedback animates on tap; no action handlers wired.
- Missing/null attributes do not panic (verified by a fixture entity with empty attributes).
- Fixture covers all three Phase 1 tile kinds (verified by smoke test).
- View models carry the schema-named `preferred_columns`/`preferred_rows` fields plus a derived `placement` value; the same struct shape will be reused in Phase 4 with values sourced from YAML and `placement` produced by the layout engine.

### Out of scope
- HA network I/O, auth, WebSocket.
- User actions (`tap_action`, `hold_action`, `double_tap_action` are present on the type but not wired).
- YAML loading.
- SBC packaging.

### Dependencies / risks
- Slint version must include a software renderer fallback; the QEMU dev VM may not have hardware GL.
- Cargo project layout: stay single-crate. Workspace split is a Phase 5 concern if cross-compile artifacts demand it.

---

## Phase 2: Home Assistant state

### Objective
Add a live `EntityStore` impl backed by an authenticated Home Assistant WebSocket client that fetches all states, subscribes to state-change events, and feeds normalized updates into the existing UI bridge with batched, low-CPU repaints. The bridge code from Phase 1 changes only at its construction site.

### Deliverables
- `src/ha/client.rs`: async WebSocket client with auth, reconnect, and message multiplexing.
- `src/ha/protocol.rs`: typed enums for the HA message subset we use, **including a generic `result { id, success, error }` envelope** (Phase 3 needs it for service-call acks).
- `src/ha/live_store.rs`: a second `EntityStore` impl, sharing the trait from Phase 1.
- Connection lifecycle FSM: Connecting → Authenticating → Subscribing → Snapshotting → Services → Live → Reconnecting → Failed.
  - **Critical ordering**: subscribe to `state_changed` events **before** issuing `get_states`, then reconcile the snapshot against any events buffered during snapshot delivery. This is what HA's own frontend does; doing it the other way drops events.
- `src/ha/services.rs`: cache of `get_services` output, refreshed on connect and on `service_registered`/`service_removed` events. Phase 3's dispatcher reads this to pick `<domain>.toggle` vs `<domain>.turn_on/turn_off` per entity.
- UI bridge update batching: coalesce updates and dispatch on the Slint event loop with a configurable cadence (default 80ms, bounded by an asserted upper limit in tests, expressed in flushes-per-second). The bridge **gates** flushes on `ConnectionState`: in `Reconnecting`/`Failed`, the last successful frame stays on screen and a status banner appears; no stale-state flushes during reconnect.
- A single config layer (`src/platform/config.rs`, the first file in `src/platform`): env (`HA_URL`, `HA_TOKEN`) takes precedence over `config.toml` (`$XDG_CONFIG_HOME/hanui/config.toml`). The YAML dashboard adds a third source merged with documented precedence **env > config.toml > dashboard.yaml**. `home_assistant.token_env` is an env-var **name**: the loader (Phase 4) reads the field, then the config layer (this phase) does the actual `std::env::var` lookup. Phase 4 never calls `env::var` directly.
- `--fixture <path>` CLI flag preserved for offline dev.

### Tasks
1. Add deps: `tokio-tungstenite`, `futures`, `url`, `tokio` features for `time` and `sync`.
2. Implement `protocol.rs` with `serde_json` tagged enums for: `auth_required`, `auth { access_token }`, `auth_ok`, `auth_invalid`, `subscribe_events`, `get_states`, `event { state_changed }`, `get_services`, generic `result { id, success, error }`. Cover oversized/malformed frames with explicit deserialize-error paths.
3. Configure `tokio-tungstenite` with `max_message_size` and `max_frame_size` set from the active `DeviceProfile.ws_payload_cap` (8–16 MiB per profile, table-defined). On overflow: drop the connection and full-resync (don't allocate the oversize buffer).
4. Implement `client.rs`: connect; send `auth`; await `auth_ok`; **then** `subscribe_events { event_type: "state_changed" }`; **then** `get_states`; **then** `get_services`. Track outgoing message ids. Buffer state-changed events that arrive during snapshot in a bounded ring sized to `DeviceProfile.snapshot_buffer_events`. **On overflow** of that buffer: drop connection, increment a counter, full-resync. (Resync is cheaper than partial replay against an unknown gap.)
5. Implement reconnect with exponential backoff (min 1s, max 30s, jitter). On reconnect: re-subscribe and re-issue `get_states` and `get_services`. The reconnect snapshot is **diffed** against the current `LiveStore`: only entities whose `last_updated` changed produce broadcast events. The bulk-snapshot watch is updated atomically by replacing the `Arc<HashMap>` (see Task 6) — no per-entity churn during a steady reconnect. The Memory-soak test (Task 14) samples peak RSS during a forced reconnect.
6. Implement `live_store.rs`: apply snapshot, then incrementally apply events. Expose `snapshot()` returning `Arc<HashMap<EntityId, Entity>>` (cloning the `Arc` is O(1); readers never copy the map). Per-entity `tokio::sync::broadcast` subscriptions are created on-demand by the bridge and dropped when the visible set changes — there is **no permanent broadcast channel per entity**. Single source of truth: the `Arc<HashMap>`. Per-entity channels carry only `EntityUpdate { id }` (no payload); subscribers re-fetch from the snapshot. This avoids the bulk-vs-per-entity duplication.
7. Implement `services.rs`: parse `get_services` result into `ServiceRegistry { domain -> {service -> ServiceMeta}}`; expose `lookup(domain, service) -> Option<&ServiceMeta>` for Phase 3's dispatcher.
8. Define `ConnectionState` enum in `src/platform/status.rs`; expose a watch-style global the UI can read (online indicator, last-update timestamp). No `src/app` module.
9. Update `src/ui/bridge.rs` to subscribe per visible entity_id (per Task 6, channel created on-demand). Pending updates live in a `HashMap<EntityId, ()>` — **latest-overwrite semantics, no per-entity FIFO**: a flood of changes for the same entity collapses to one repaint. Flush via `slint::invoke_from_event_loop` at the cadence constant. Bridge reads `ConnectionState` and suspends flushes while not in `Live`; last good frame remains on screen.
10. Move fixture loader behind a `--fixture <path>` CLI flag.
11. Implement `src/platform/config.rs`: precedence env > config.toml > (YAML in Phase 4). Clear error message when token missing. Document the precedence rule in a doc-comment that Phase 4 must not contradict.
12. Integration tests against a mocked WebSocket server (`tokio-tungstenite` server side): auth success, auth failure, snapshot-then-event ordering, missed-event-on-reconnect resync, oversized payloads (above and below the configured cap), malformed JSON, `get_services` round-trip. WS-level test: request/result id-correlation (oneshot resolves on matching `result`, errors on mismatched id, times out on no-reply). The dispatcher-level reconciliation test lives in Phase 3 — Phase 2 only proves the WS plumbing.
13. Synthetic churn benchmark: 1000 entities, 50 changes/sec, **assert** flush rate ≤ `12.5 flushes/sec` (corresponds to 80ms cadence) for 60s. Runs in CI feature-gated.
14. Memory soak test: 10-minute run at 1000 entities / 50 ev/s, sample RSS every 30 s. **Three assertions**: (a) steady-state RSS growth ≤ 5 MB, (b) absolute peak RSS ≤ `profile.idle_rss_cap` (table-defined: 80 MB rpi4 / 60 MB opi_zero3 / 120 MB desktop), (c) peak RSS during a forced disconnect/reconnect burst ≤ steady-state + 20 MB. Reconnect-burst flush behavior is part of (c): the test forces a reconnect and asserts that post-resync flushes do not exceed the standard `12.5 flushes/sec` cadence (i.e., the visible-updates queue's latest-overwrite semantics correctly collapse the resync flood). Nightly job.
15. SBC-class CPU smoke (cross-build runs in QEMU user-mode emulation for `aarch64`): 60-s churn at 50 ev/s, assert CPU% ≤ `profile.cpu_smoke_budget_pct` (table-keyed: `rpi4` = 30, `opi_zero3` = 50, `desktop` = 15 — added to the budgets table as `cpu_smoke_budget_pct`). This is a sanity gate; real Pi/OPi numbers come in Phase 5.

### Acceptance criteria
- Connects to a real HA instance and renders live state for entities used by the default view.
- Toggling an entity in HA updates the corresponding tile within ~200ms without flicker.
- Killing the connection flips the status indicator within 5s; unblocking auto-reconnects and resyncs all state.
- Auth failure produces a single clear error log and a non-crashing offline UI.
- Memory bounded under 1000-entity / 50 ev/s churn for 10 minutes.
- Churn benchmark holds the asserted flush-count bound.
- Phase 1 acceptance still passes with `--fixture`.

### Out of scope
- User actions.
- YAML dashboard loading.
- TLS pinning, OAuth, HA Cloud.
- State persistence across restarts.

### Dependencies / risks
- HA token must be a long-lived access token; document in `README.md`.
- `tokio-tungstenite` `max_message_size` must be raised for large `get_states` payloads.
- Update batching is the main perf knob on SBCs; the CI bound assertion is the guardrail.

---

## Phase 3: Actions

### Objective
Make the dashboard interactive: tap, long-press, and double-tap dispatch typed actions to Home Assistant (toggle, call_service, more-info, navigate), with offline behavior limited to safe actions and visible feedback for every action. Action wiring is **data-driven from day one** — Phase 4 swaps the data source from in-code to YAML without touching the dispatcher.

### Deliverables
- `src/actions/schema.rs`: typed `Action` enum: `Toggle`, `CallService { domain, service, target, data }`, `MoreInfo`, `Navigate { view_id }`, `Url { href }`, `None`. With serde so Phase 4 can deserialize from YAML.
- `src/actions/map.rs`: a `WidgetActionMap` keyed by widget id holding `tap`/`hold`/`double_tap` `Action`s. Built in code in Phase 3, populated from YAML in Phase 4. **Not** a hard-coded match in `bridge.rs`.
- `src/actions/dispatcher.rs`: takes `(widget_id, gesture, store_view)` and emits the right WS frame or routes to in-app navigation/more-info. Uses the `ServiceRegistry` cache from Phase 2 (Task 7) so it never blindly assumes `<domain>.toggle` exists; falls back to `turn_on`/`turn_off` only when both are present in the registry.
- `src/actions/timing.rs`: a single `ActionTiming { gesture: GestureConfig, optimistic_timeout_ms, queue_max_age_ms, action_overlap_strategy: { LastWriteWins | DiscardConcurrent } }` struct. Phase 4's `DeviceProfile` includes a `timing_overrides: ActionTimingOverride` field that wins over defaults.
- `Url` action handling: in-app navigation only by default. Shelling to `xdg-open` is gated by a per-device-profile enum (`url_action_mode: UrlActionMode { Always | Never | Ask }`, default `Never` on `rpi4`/`opi_zero3`, `Always` on `desktop`). Disallowed `Url` actions show an error toast.
- A view-router so `Navigate { view_id }` actually switches the active view (the only view in Phase 3 is `default`, but the plumbing must exist or Phase 4 will need to retrofit the dispatcher).
- Slint gesture layer on `card_base` with timings tuned through a single `GestureConfig` struct.
- `more-info` modal as a generic shell. Phase 3 ships only the **toggle/state** body. The shell **already implements** the body-slot interface that Phase 6 will populate with per-domain bodies (`MoreInfoBody` trait / Slint `body` slot). No restructure in Phase 6; only new body impls.
- Optimistic UI with reconciliation by **service-call ack + entity_id + entity timestamp** (HA `state_changed` is not request-id keyed — reconciling that way is wrong and was a draft-stage bug). Out-of-order acks on the same entity are tie-broken by HA-reported `last_updated`: if a newer `state_changed` has already arrived, the older ack does not revert it.
- Offline action queue limited to **idempotent** actions only. The schema marks each `Action` variant with an `IDEMPOTENCY: { Idempotent, NonIdempotent }` const; the validator (Phase 4) checks `CallService` entries against an allowlist (`turn_on`, `turn_off`, `set_*`, etc.) and errors on unsafe service names. `Toggle` is non-idempotent and is therefore not queued — it fails loudly when offline.
- Per-tile pending indicator and an error toast.

### Tasks
1. Define `Action`/`ActionSpec` types with serde derives, plus an `Idempotency` marker per variant. `Url` carries an `href: String` and is dispatched to `src/actions/url.rs`.
2. Define `ActionTiming` in `src/actions/timing.rs`: `GestureConfig { tap_max_ms, hold_min_ms, double_tap_max_gap_ms, double_tap_enabled: bool }`, `optimistic_timeout_ms` (default 3000), `queue_max_age_ms` (default 60000), and `action_overlap_strategy: { LastWriteWins | DiscardConcurrent }` (default `LastWriteWins` on widget actions; controls what happens when a second gesture fires on the same widget while an action is still pending). Exposed to Slint via a global, including a derived boolean `arm_double_tap_timer = double_tap_enabled` so Slint reads one explicit field rather than inferring intent from a zero gap. Phase 4's `DeviceProfile` may override any of these; `double_tap_enabled = false` on `opi_zero3` by default.
3. Implement gesture detection in `card_base.slint` reading `ActionTiming.gesture`. The Slint side branches on `arm_double_tap_timer`: when `false`, the tap fires synchronously on touch-up (no timer armed); when `true`, the tap fires after `double_tap_max_gap_ms` if no second touch-down arrives. The hold timer only arms after a press starts.
4. Build `WidgetActionMap` in `src/actions/map.rs` and populate it in code from `view_spec` (the default `Dashboard` already carries `tap_action`/`hold_action`/`double_tap_action` from Phase 1). The dispatcher reads from the map; nothing is hard-coded in the bridge.
5. Implement dispatcher: translate `Toggle` to `call_service` by consulting `ServiceRegistry` (Phase 2 Task 7) — prefer `<domain>.toggle`, fall back to `<domain>.turn_on`/`turn_off` pair if absent, error if neither is registered. Translate `CallService` directly. Route `MoreInfo`/`Navigate` to UI without a network call. Route `Url` to `url.rs`.
6. Implement `src/actions/url.rs`: check the active `DeviceProfile.url_action_mode`; if `Always`, spawn `xdg-open` (or `Open With Default` per platform); if `Never`, emit an error toast and do not shell out; if `Ask`, defer to a Phase 6 confirmation dialog (no shell-out in Phase 3).
7. Add request/response correlation in the WS client (id → oneshot) so the dispatcher awaits success/error and surfaces it via the `result` envelope from Phase 2.
8. Implement the generic `more-info` modal: header (icon + name + state), `MoreInfoBody` slot (Phase 6 plugs domain-specific bodies in), action footer (close). Phase 3 ships an `AttributesBody` impl that lists current attributes with these caps: at most 32 attributes shown, each value truncated to 256 chars after a typed display formatter (no raw `to_string()` of arbitrary `Value`). Lazy-rendered: the body computes its rows when the modal opens, not on every entity update; reopens to refresh. Add a compile-time test that the trait is object-safe and that `AttributesBody: MoreInfoBody`.
9. Implement view-router: a `current_view: SharedString` global driven by `Navigate`. Single view this phase, but the API is wired.
10. Optimistic state: dispatcher writes a tentative entry keyed by `(entity_id, request_id, dispatched_at)`. Two caps from the budgets table: `pending_optimistic_per_entity` (default 4) and `pending_optimistic_global` (default 64). New dispatches at the per-entity cap return an `Err(BackpressureRejected)` and surface as a toast — no silent dropping. Reconcile when the matching `result` returns success. On `state_changed` for that entity, drop any optimistic entries with `dispatched_at` older than `event.last_updated` (newer truth wins). Revert on `result` error or `optimistic_timeout_ms` elapsed without ack.
11. Offline queue: FIFO with `queue_max_age_ms` aging out stale entries, capacity from `DeviceProfile.offline_queue_cap`, drop-oldest on overflow. The `Idempotency` marker on each `Action` variant is checked **at runtime** by the dispatcher before enqueue (the Phase 4 validator is a separate static check on YAML; Phase 3 cannot rely on it). Non-idempotent calls fail-fast offline with an error toast and are never queued.
12. Toast component for transient errors with **explicit dismiss**: auto-dismiss after `toast_dismiss_ms` (4 s, table-defined), tap-to-dismiss earlier, and a single visible at a time (newer replaces older). No toast queue persists across view changes. Per-tile spinner for in-flight actions.
13. Tests: dispatcher unit tests per `Action` variant; mock-WS integration covering success, error, reconnect-flush, capability fallback (toggle missing → turn_on/off pair → both missing), idempotency gating, optimistic-revert without flicker (golden frame compare on the affected tile during the revert window), out-of-order ack on same entity (newer `state_changed` already applied — older ack must not revert).

### Acceptance criteria
- Tapping a light tile toggles in HA within ~300ms; tile reflects new state without flicker.
- Long-press opens the generic more-info modal; closes on tap-outside or back-gesture.
- Disconnecting during an idempotent action queues it; reconnecting flushes in order; pending indicator visible.
- Non-idempotent action while disconnected returns a clear error toast and is not queued.
- Permission errors surface a toast and revert optimistic state without visible flicker (verified by the golden frame test).
- Action latency p95 < 400 ms on the dev VM. CPU during a 10 s sustained-tap test: ≤ 25 % single-core under emulated `rpi4` profile (measured by the Phase 2 Task 15 SBC-class CPU smoke harness).
- `Navigate { view_id }` to `default` is a no-op; the API exists.

### Out of scope
- YAML-driven action wiring (Phase 4).
- Per-domain more-info bodies (Phase 6).
- Confirmation dialogs / PIN entry (Phase 6).

### Dependencies / risks
- HA service errors arrive on the same WS channel as events — id correlation must be airtight.
- Gesture timings: `GestureConfig` is the one place to tune; resistive-touch SBCs may need different defaults set per device profile in Phase 4.
- Optimistic reconciliation must use service-ack + entity timestamps, never request-id against `state_changed`.

---

## Phase 4: Layout

### Objective
Replace the hand-coded `Dashboard` with a YAML loader that fully drives layout, multiple views, sections, grid sizing, density modes, and low-power caps. Includes prototype work to confirm Slint's grid primitives can express the spans we plan to support — done **before** the validator commits to a contract.

### Deliverables
- A schema update PR to `docs/DASHBOARD_SCHEMA.md` and `examples/dashboard.yaml`, landed **first** in this phase, defining:
  - `widgets[].layout.preferred_columns` and `preferred_rows` (already implied), plus explicit semantics for what they mean,
  - `widgets[].entities`, `name`, `icon`, `options`, plus the action fields,
  - `widgets[].visibility` (predicate spec — Phase 6 will implement, but the schema lock catches early breakage),
  - `views[].sections[].grid { columns, gap }`,
  - `device_profile` as an explicit enum (`rpi4 | opi_zero3 | desktop`, free-string values fail validation),
  - per-widget `options.camera.interval_seconds`, `options.history.window_seconds`, `options.fan.speed_count` / `options.fan.preset_modes`, `options.lock.pin_policy`, `options.alarm.pin_policy`. Numeric bounds are not hard-coded in the schema; they are enforced at validation time by `DeviceProfile.{camera_interval_min_s, history_window_max_s, …}`. The schema declares the fields exist; the active profile sets the bounds. (This avoids the schema-vs-table conflict from prior drafts.)
- A short Slint span prototype landed **before** the schema is finalized, demonstrating the column/row-span behavior the engine will emit. If Slint can't express what we need, the schema is trimmed and re-circulated **before** code commits to it. A formal "schema finalization" step closes the prototype loop and locks the schema for the rest of the phase.
- `src/dashboard/schema.rs`: Rust types matching the finalized schema.
- `src/dashboard/loader.rs`: YAML loader. Path resolution: `--config <file>` → `$XDG_CONFIG_HOME/hanui/dashboard.yaml`. **No silent fallback to `examples/dashboard.yaml`.** Missing config = error screen; invalid config = error screen with offending excerpt.
- `home_assistant.token_env`: the loader reads the env-var **name** field but the actual `std::env::var` lookup is delegated to `src/platform/config.rs` from Phase 2. The loader never calls `env::var` directly. No `${env:...}` interpolation syntax.
- `src/dashboard/validate.rs`: structured issue list with severities. `Error` halts load; `Warning` shows a banner but renders. **Idempotency rule** lives here: `CallService` actions are validated against the allowlist defined alongside the `Action` enum in Phase 3.
- `src/dashboard/layout.rs`: deterministic packing engine. **One algorithm, documented**: row-major first-fit, `preferred_columns`/`preferred_rows` honored where they fit, span-aware (a widget reserves `preferred_columns × preferred_rows` cells), wrap on row overflow, **fail validation** if a single widget's `preferred_columns` exceeds its section's `grid.columns` (this case has its own dedicated validator test).
- `src/dashboard/profiles.rs` extended to define `DeviceProfile` with **every** numeric field from the Performance budgets table. Required fields: `tokio_workers`, `target_frame_period_ms`, `idle_cpu_pct_cap`, `idle_rss_mb_cap`, `cpu_smoke_budget_pct`, `animation_framerate_cap`, `max_entities`, `max_widgets_per_view`, `max_simultaneous_animations`, `max_simultaneous_camera_streams`, `max_image_px`, `touch_input: bool`, `ws_payload_cap`, `snapshot_buffer_events`, `pending_optimistic_per_entity`, `pending_optimistic_global`, `offline_queue_cap`, `attributes_body_max_attrs`, `attributes_body_max_chars`, `toast_dismiss_ms`, `camera_interval_default_s`, `camera_interval_min_s`, `history_window_default_s`, `history_window_max_s`, `http_cache_bytes`, `http_cache_ttl_s`, `dep_index_inline_cap`, `frame_histogram_buckets`, `soc_temp_ceiling_c`, `reconnect_burst_rss_mb`, `blanking_policy: { Never | Idle(Duration) }`, `url_action_mode`, `timing_overrides: ActionTimingOverride`, `density`. Three presets `rpi4`, `opi_zero3`, `desktop` with the values from the budgets table. Constants are checked-in; not derived. The selector `select(yaml_profile, autodetected)` returns the active profile; YAML wins. **A unit test in `profiles.rs` asserts each preset's struct round-trips against the budgets table values** (manually-maintained pair, but explicit) so a table edit without a struct edit fails CI.
- `View` Slint component rendering positioned widgets via Slint layouts (informed by the prototype).
- View-switcher Slint component supporting **swipe** (touch gesture handler at the view edges), tap (tab strip), and dropdown — chosen per density mode.

### Tasks
1. Build the Slint span prototype (`examples/span_check.slint` plus a small Rust harness in `examples/span_check.rs` — these are checked-in deliverables, not ephemeral). Confirm Slint primitives can express the column/row-span behavior we need.
2. Draft `docs/DASHBOARD_SCHEMA.md` updates and `examples/dashboard.yaml` updates covering all widget fields, layout, `visibility`, `device_profile` enum values, and per-domain options bounds (camera interval min, etc.). Iterate against Task 1 findings.
3. **Schema finalization gate**: open a single PR locking the schema. After this PR merges, schema additions only happen via explicit follow-on PRs. The schema-lock test (Task 12) is the enforcement.
4. Add `serde_yaml`. Implement `src/dashboard/schema.rs` matching the locked schema 1:1. `device_profile` is a serde-tagged enum.
5. Implement `loader.rs`: read YAML, **read** `token_env` field as an env-var name, **delegate** the actual `std::env::var` lookup to `src/platform/config.rs` from Phase 2. Build typed `Dashboard`. Errors on missing config (no fallback to `examples/`).
6. Implement `validate.rs` returning `Vec<Issue { severity, path, message }>`. Severity rules are consistent with Task 12: `Error` halts load (no partial render); `Warning` renders with a banner. Rules:
   - **Error**: span overflow (single widget wider than its section's grid columns); unknown widget type; unknown predicate in `visibility`; non-allowlisted `CallService` used in `tap_action`/`hold_action`; per-view widget count exceeding `DeviceProfile.max_widgets_per_view`; camera widget interval below `camera_interval_min_s`; history widget window above `history_window_max_s`; PIN policy referencing a non-string `code_format` value.
   - **Warning** (renders, surfaces a banner): image options exceeding `max_image_px` (pre-decode downscale); camera interval between `camera_interval_min_s` and `camera_interval_default_s` (allowed but flagged).
7. Implement `layout.rs` using the documented packing algorithm. Add doc-comments naming the algorithm and documenting the wrap policy. **Test specifically** that a widget with `preferred_columns > section.grid.columns` produces a validator Error, not a silent clamp. The packer runs **once per `Dashboard` load** and caches `Vec<PositionedWidget>`; no per-frame layout work.
8. Implement `profiles.rs` with the expanded `DeviceProfile` struct + presets. Selector function: YAML overrides autodetect (autodetect is a no-op stub here; Phase 5 fills it in).
9. Add a `View` Slint component using primitives validated by Task 1.
10. Add a view-switcher with three input modes: tab strip (regular density), dropdown (compact density), and edge-swipe (only enabled when `DeviceProfile.touch_input == true`). Implement swipe via Slint touch handlers at the view edges with a horizontal-velocity threshold; on non-touch profiles the swipe handler is **not instantiated** at all (no idle event work). Switcher transitions cap their framerate at `DeviceProfile.animation_framerate_cap` (30 fps on `rpi4`, 20 fps on `opi_zero3`).
11. Wire `tap_action`/`hold_action`/`double_tap_action` from YAML through `WidgetActionMap` (the map shape already exists from Phase 3; only the source changes).
12. Validation reporting: fullscreen error screen with path + message + YAML excerpt on `Error`. Banner on `Warning`. Never silently render a partial dashboard.
13. Tests: a `tests/layout/` directory with paired `<n>.yaml` + `<n>.expected.json` golden fixtures (at least 6: single-widget, span-honored, wrap, span-overflow=Error, mixed widget kinds, multi-section); validation tests per error/warning kind; smoke test loading the updated `examples/dashboard.yaml` end-to-end. **Schema lock test** asserts every documented field in `docs/DASHBOARD_SCHEMA.md` round-trips through serde, and fails CI if a field is added/removed without updating the schema doc.

### Acceptance criteria
- `examples/dashboard.yaml` (the updated one) loads, validates, and renders correctly.
- Editing YAML and restarting reflects changes.
- Unknown widget type → error screen with the widget path.
- Missing dashboard config → error screen, no silent fallback.
- `device_profile: rpi4` in YAML overrides whatever autodetect returns.
- Switching views via swipe/tap/`Navigate` meets the active profile's `target_frame_period_ms` p95 (≤ 33 ms on emulated `rpi4`, ≤ 50 ms on emulated `opi_zero3`, ≤ 16 ms on `desktop`). Measured by the same harness as Phase 2 Task 15.
- Layout output is byte-identical across runs for identical input.

### Out of scope
- Hot reload (deferred).
- Custom themes beyond `mode` and `accent`.
- Conditional widgets (`visibility:`) — Phase 6.

### Dependencies / risks
- The Slint grid prototype in Task 3 is gating: don't write the validator until it works.
- YAML schema lock test prevents Phase 6 from accidentally breaking the schema.

---

## Phase 5: Deployment

### Objective
Ship the dashboard as a turnkey kiosk on Raspberry Pi 4 and Orange Pi Zero 3 (DietPi or RPi OS Lite / Armbian), booting fullscreen with a watchdog, structured logs, secure token injection, and a healthcheck endpoint.

### Deliverables
- A **release artifact pipeline**: tagged Git releases produce a `hanui-<version>-aarch64.tar.gz` (and best-effort `armv7`) in GitHub Releases (or equivalent host); `install.sh` downloads from a configurable base URL (`HANUI_RELEASE_URL`, default points at the canonical release host). `curl ... | bash` works because the script knows where to fetch the binary.
- `dist/install.sh`: idempotent installer that detects board, installs deps, downloads the matching release artifact, copies the binary to `/usr/local/bin/hanui`, installs systemd unit, creates the `hanui` system user, ensures `/etc/hanui/` exists, **drops `examples/dashboard.yaml` to `/etc/hanui/dashboard.yaml.example`** (operator copies and edits to `dashboard.yaml`), writes a permission-tight `/etc/hanui/hanui.env` template **without** the token (operator fills it in).
- `dist/systemd/hanui.service`: hardened unit (`User=hanui`, `Restart=on-failure`, `RestartSec=3s`, `WatchdogSec=30s`, `NoNewPrivileges=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `RuntimeDirectory=hanui` (so `/run/hanui/` is writable for the health socket under `ProtectSystem=strict`), `RuntimeDirectoryMode=0750`, `EnvironmentFile=/etc/hanui/hanui.env`). `ExecStart=/usr/local/bin/hanui-launcher.sh /etc/hanui/dashboard.yaml /run/hanui/health.sock` — the unit always goes through the launcher; the launcher is the only place that picks the compositor. The hardening flag set is documented in a comment block at the top so changes are reviewable.
- `dist/systemd/hanui-launcher.sh`: wrapper that picks `cage` (Wayland) or `weston` (fallback), sets `WAYLAND_DISPLAY`, exports `WLR_RENDERER=pixman` on GPU-less boards, then `exec`s the binary with `--config "$1" --health-socket "$2"`. **The hardening contract**: the launcher does not weaken what the unit enforces; it only sets env and execs. **Blanking is *not* the launcher's job** — the active `DeviceProfile.blanking_policy` is loaded from YAML by the binary, so the binary owns DPMS/swayidle setup. The launcher only handles compositor selection.
- Token injection via systemd `EnvironmentFile`, aligning with `home_assistant.token_env`. The operator's YAML names the env var; the env file defines it; the unit pulls it in. One source of truth.
- Cross-compile recipe: `dist/build.sh` producing `aarch64-unknown-linux-gnu` (primary, **gated by CI on every release tag**). `armv7-unknown-linux-gnueabihf` is best-effort and **explicitly unverified for end-to-end SBC behavior**: docs note that `armv7` ships without a smoke-test acceptance gate. **No `upx` in the main recipe** — an `--experimental-upx` flag exists but is off by default.
- Logging: structured JSON to stdout (systemd journal) when `INVOCATION_ID` is set; redact `HA_TOKEN` and any `Authorization`-style fields.
- Watchdog: `sd_notify(WATCHDOG=1)` ping from the event loop; loop stalls > `WatchdogSec` → systemd restart.
- Healthcheck: UNIX socket at `/run/hanui/health.sock` answering JSON `{ ws_connected, last_event_ts, frame_period_p95_ms }`. **`frame_period_p95_ms` replaces `fps_avg`** because frame-period percentile is meaningful on a software renderer that idles; an averaged FPS over an idle window is misleading.
- A `hanuictl health` subcommand that connects to the socket, parses the JSON, and exits non-zero if `ws_connected=false` for >60s (verified via `last_event_ts`). This is what monitoring/cron checks call.
- Device-profile autodetect filling in the Phase 4 stub: read `/proc/device-tree/model`, return the matching profile. YAML still overrides per Phase 4.
- `docs/DEPLOY.md` with board-specific notes (display rotation, touch calibration, screen blanking, autologin), recorded boot-to-dashboard time + idle CPU/RAM from real hardware, and an explicit "Validated SBCs" list (Pi 4 = green, OPi Zero 3 = green, anything armv7 = best-effort).

### Tasks
1. Add `sd-notify` crate; emit `READY=1`, `STATUS=...`, periodic `WATCHDOG=1` from the main loop.
2. Implement `--health-socket <path>`; bind UNIX socket at the given path; handle one-line JSON requests; emit `{ ws_connected, last_event_ts, frame_period_p95_ms }`.
3. Implement `hanuictl health` subcommand that opens the socket, reads the JSON, and exits non-zero if WS is down for >60 s. Accepts `--health-socket <path>` (default `/run/hanui/health.sock`) so an operator who launched the binary with a custom socket path can point `hanuictl` at the same path. Document the symmetry in `docs/DEPLOY.md`.
4. Implement `frame_period_p95_ms` collection: a fixed 100-bucket linear histogram (0–200 ms in 2 ms buckets) maintained as `[u16; 100]` plus a 60 s rolling reset. The render callback records via one `fetch_add` on the bucket index — no allocation, no syscall per frame. The socket reads the histogram and computes p95 on demand.
5. Switch tracing to JSON when `INVOCATION_ID` is set; redact tokens.
6. Build cross-compile pipeline using `cross` for `aarch64-unknown-linux-gnu`. **Build on every PR** (CI gate: `cargo build --release --target aarch64-unknown-linux-gnu` must pass) so a Phase 1–4 PR that breaks aarch64 fails CI immediately, not at the next release tag. Release-tag CI additionally runs `--target armv7-unknown-linux-gnueabihf` (best-effort, build-only — no SBC smoke gate). UPX is an opt-in experiment, not the recipe default.
7. Set up the release artifact pipeline: on `v*` tags, CI publishes `hanui-<version>-<arch>.tar.gz` to GitHub Releases (or chosen host). `install.sh` reads `HANUI_RELEASE_URL` (default = canonical host).
8. Author `dist/install.sh`: detect distro via `/etc/os-release`, install **per-board minimal package sets** (documented in `docs/DEPLOY.md`): `rpi4` = `cage seatd libwayland-client0`; `opi_zero3` = `cage seatd libwayland-client0` (no `weston` fallback unless explicitly requested — keeps OPi tight); `desktop` = `weston cage seatd libwayland-client0 mesa-utils`. Download the matching artifact via `curl`, verify SHA-256, create `hanui` user, create `/etc/hanui/`, write a template `hanui.env` (token line commented out), drop `dashboard.yaml.example` next to it.
9. Author `dist/systemd/hanui.service` with hardening flags, `RuntimeDirectory=hanui`, `EnvironmentFile=/etc/hanui/hanui.env`, `WatchdogSec=30s`, `ExecStart=/usr/local/bin/hanui-launcher.sh /etc/hanui/dashboard.yaml /run/hanui/health.sock`. Document each flag in a header comment.
10. Author `dist/systemd/hanui-launcher.sh` that picks compositor, sets env (`WAYLAND_DISPLAY`, `WLR_RENDERER=pixman` on GPU-less boards, `WLR_NO_HARDWARE_CURSORS=1` where needed), and `exec`s `/usr/local/bin/hanui --config "$1" --health-socket "$2"`. Matches the unit's hardening contract — env-only, no privilege relaxation.
11. **Binary-owned screen-blanking** (in `src/platform/blanking.rs`): after `Dashboard` loads and the active profile is selected, the binary configures blanking. `Never` → talks to the compositor's idle inhibit protocol (e.g., `wl_idle_inhibit`). `Idle(d)` → spawns `swayidle` as a child (or uses the Wayland idle-notify protocol directly when available); the binary is the parent process and SIGTERMs the child on shutdown. **No polling timers** — purely event-driven via the Wayland idle protocol.
12. Implement device-profile autodetect in `src/platform/device.rs`: parse `/proc/device-tree/model`, return matching `DeviceProfile` from Phase 4. The selector from Phase 4 already prefers YAML over autodetect; Phase 5 just supplies a real autodetect.
13. Smoke-test the install on a Pi 4 image and an OPi Zero 3 image. Record boot-to-dashboard time, idle CPU, idle RSS in `docs/DEPLOY.md`. armv7 boards are not on the smoke-test gate.
14. **Thermal soak**: 60-minute run on the Pi 4 and OPi Zero 3 with the dashboard active under a synthetic 10-events-per-second load; record SoC temperature every 30 s via `/sys/class/thermal/thermal_zone0/temp`. Acceptance: SoC temp stays ≤ 75 °C without a heatsink fan on Pi 4, ≤ 80 °C on OPi Zero 3 (per their respective documented throttle thresholds). Failures are documented in `docs/DEPLOY.md`, not silently ignored.
15. CI: `dist/install.sh` runs in a Debian Docker container with `--dry-run` to lint shell and unit files (`shellcheck`, `systemd-analyze verify`). **This is a sanity gate, not deployment proof** — the SBC smoke test (Task 13) and thermal soak (Task 14) are the real validation; document that distinction in `docs/DEPLOY.md`.

### Acceptance criteria
- Fresh DietPi image + `curl ... | bash` of `install.sh` boots into the dashboard fullscreen on next reboot, after the operator drops their `dashboard.yaml` and fills `hanui.env`.
- Idle CPU and RSS meet the active profile's `idle_cpu_pct_cap` / `idle_rss_mb_cap` from the budgets table (Pi 4: < 5 %, < 80 MB; OPi Zero 3: < 10 %, < 60 MB; desktop: < 5 %, < 120 MB).
- `pkill hanui` auto-restarts within `RestartSec`.
- Stalling the event loop (debug toggle) triggers a watchdog restart within `WatchdogSec`.
- `journalctl -u hanui` shows redacted JSON logs; no token leakage.
- Healthcheck socket returns expected JSON; `hanuictl health` exits non-zero when WS has been down >60s.
- YAML `device_profile` overrides autodetect on a board where they differ.
- All earlier phases pass on real SBCs, not just the dev VM.
- 60-minute thermal soak on Pi 4 stays ≤ 75 °C; OPi Zero 3 stays ≤ 80 °C.

### Out of scope
- **OTA / auto-update infrastructure entirely.** No update timer this phase; revisit when a release-and-update story exists.
- Web admin UI for configuration.
- Provisioning multiple devices from a central server.

### Dependencies / risks
- Cage / weston versions vary across distros; the launcher selects cleanly and surfaces a useful error if neither is present.
- GPU-less boards: confirm `WLR_RENDERER=pixman` works with the chosen Slint backend.
- systemd hardening can break audio/USB-touch on some boards; document escape hatches per board.
- `armv7` cross toolchains are flakier than `aarch64` — best-effort.

---

## Phase 6: Advanced widgets

This phase is split into three sub-phases (6a/6b/6c) by risk: simple widgets land first, the importer ships only after the widget surface is stable. Each sub-phase is independently mergeable, but they share a fixed prerequisite block (6.0) that must land first.

### 6.0: Cross-cutting prerequisites (one PR — must land before 6a)

#### Tasks
1. **Schema additions land first**: extend `docs/DASHBOARD_SCHEMA.md` and `src/dashboard/schema.rs` to cover every new widget kind and its `options` block:
   - climate: `min_temp`, `max_temp`, `step`, `hvac_modes`;
   - media: `transport_set: [play, pause, stop, next, prev]`, `volume_step`;
   - cover: `position_min: 0`, `position_max: 100`;
   - fan: `speed_count: u8` (number of discrete steps when `preset_modes` empty), `preset_modes: Vec<String>` (e.g., `["low","medium","high","auto"]`); validator errors if both empty;
   - lock: `pin_policy: { None | Required(length: u8) }`, `code_format: { Number | Any }` — same `pin_policy` enum as alarm;
   - alarm: `pin_policy: { None | Required(length: u8) | RequiredOnDisarm }`, `code_format: { Number | Any }`;
   - camera: `interval_seconds`, `url` (numeric bounds enforced by `DeviceProfile`, not the schema — see Phase 4 deliverable);
   - history: `window_seconds`, `max_points: { default: 60, max: 240 }` (the absolute upper bound is in the budgets table; per-widget cap can be lower);
   - **`visibility` predicates**: `entity == value`, `entity != value`, `entity in [..]`, `entity_state_numeric { <, <=, >, >=, ==, != } N`.
2. **Re-run the Phase 4 schema-lock test** in CI under this PR; failure means schema additions are not yet visible to the validator and must be fixed before merge.
3. **Shared HTTP layer**: introduce `src/ha/http.rs` with bearer auth (token sourced from the same Phase 2 config layer), response cache, per-host rate limit, retry budget, and a single `tracing` span name. Cache caps come from `DeviceProfile`: `http_cache_bytes` (32 MiB rpi4 / 16 MiB opi_zero3 / 128 MiB desktop) and `http_cache_ttl_s` (5 min / 5 min / 10 min). The cache stores **decoded buffers, not raw bytes** for image entries, with byte accounting that includes decode-expansion (RGBA = 4 × pixel count). Insertions exceeding the cap evict LRU. Per-entry max bytes = `DeviceProfile.max_image_px² × 4`; oversized entries are rejected at insert. Rate limit: ≤ 4 in-flight requests, ≤ 8 requests/second per host. Both history and camera REST clients (6b) consume this. **`http.rs` is owned by 6.0**, not 6b.
4. **Generic `more-info` body interface**: extend the modal scaffolded in Phase 3 (Phase 3 Task 8) by formalizing the `MoreInfoBody` contract — a Slint component slot with `(entity, store, dispatcher)` inputs. Document the contract in `src/ui/more_info.rs`. Phase 3's `AttributesBody` impl is the reference impl.
5. **Typed `Action` variants for setpoints/positions/transport**: add `SetTemperature`, `SetHvacMode`, `SetMediaVolume`, `MediaTransport(Play|Pause|Next|Prev|Stop)`, `SetCoverPosition`, `SetFanSpeed`, `Lock`/`Unlock` (with `confirmation: bool`), `AlarmArm { mode }` / `AlarmDisarm` (both consult the schema's `pin_policy` to decide whether to prompt for a code). Each variant carries an explicit `IDEMPOTENCY` marker (most are `Idempotent`; `MediaTransport(Next)`/`Prev` are `NonIdempotent`). Service mappings are spelled out in `src/actions/services.rs` — no "TBD".
6. **PIN entry component**: a Slint number-pad (or alphanumeric, when `code_format == Any`) modal returning the entered code via callback. Both lock and alarm widgets consult their `pin_policy` from the widget options (Task 1) to decide whether to prompt; for alarm, `RequiredOnDisarm` only prompts on `AlarmDisarm`. The resulting code is passed in `data.code` of the `call_service` payload (HA's standard for `alarm_control_panel.alarm_disarm` and `lock.unlock`). The PIN is **never** stored, logged (token-redacted log layer covers it), or persisted.

### 6a: Domain tiles (low risk)

#### Tasks
1. Implement `cover_tile`, `fan_tile`, `lock_tile`, `alarm_panel_tile` — each as a Slint component + Rust view model + per-domain `MoreInfoBody` impl using the 6.0 contract.
2. Wire `confirmation: true` on lock/alarm `Unlock`/`Disarm` actions to a confirm modal before dispatch.
3. Wire `pin_policy` on alarm tiles to the PIN entry component from 6.0 Task 6.
4. Tests: golden render for off/on/unavailable/error per widget; integration test for confirmation flow; integration test for PIN entry that asserts the code never appears in tracing output (validates the redact path).

### 6b: History + camera + climate + media (medium risk)

#### Tasks
1. Extend Phase 4 layout golden tests to include a `visibility_flip.yaml` fixture and verify zero layout flicker (no widgets repositioning) when a `visibility` predicate flips. **This must land before the visibility evaluator in Task 7** so the test is written against a known-failing state first.
2. Implement `src/ha/history.rs` over the 6.0 HTTP layer; debounced fetch on view change; window-aware downsampling capped at the schema's `max_points`.
3. Implement `src/ha/camera.rs` with a **pre-bounded decoder pool**: `CameraPool { workers: Vec<DecoderWorker>, image_buffers: ArrayQueue<DecodedImage>, slots: Semaphore(DeviceProfile.max_simultaneous_camera_streams) }`. Cameras configured beyond the cap render a placeholder + warning log **without instantiating a worker or buffer** (asserted by Phase 4 validator + a 6b unit test that loads a 3-camera YAML on `rpi4` and verifies validator surfaces an Error and runtime instantiates only 2 workers). Snapshot interval defaults from `DeviceProfile.camera_interval_default_s`, minimum from `camera_interval_min_s` — Phase 4 validator rejects YAML below the min. Each camera runs an interval timer; when the timer fires the camera checks **`worker.busy()`** before issuing a new fetch. If the worker is still decoding the previous frame, the new tick is **skipped (with a `frames_dropped_busy` counter, surfaced via tracing)**, not silently dropped — operators get a signal when their interval is too tight for the decoder. Decoded buffers live in the bounded `ArrayQueue` (no per-frame allocation); a busy decoder reuses the same RGBA buffer for the next frame after Slint releases the prior reference. Decoder concurrency = pool size — never more workers than slots.
4. Implement `climate_tile` + `MoreInfoBody` (setpoint slider issues `SetTemperature`; mode picker issues `SetHvacMode`).
5. Implement `media_player_tile` + `MoreInfoBody` (transport buttons + volume slider; artwork loaded async via `http.rs` cache).
6. Implement `camera_snapshot_tile` + fullscreen view (single still, tap-to-dismiss).
7. Implement `history_graph_tile` reading from `history.rs`; throttle repaints to ≤ 1/min unless visibility changes.
8. Implement the `visibility` predicate evaluator with an **entity-indexed dependency map** built once at dashboard load: `dep_index: HashMap<EntityId, SmallVec<[WidgetId; DEP_INLINE_CAP]>>` where `DEP_INLINE_CAP` (table value, default 8) sizes the SmallVec inline storage. A `static_assertions::const_assert!` confirms `DEP_INLINE_CAP == DeviceProfile::desktop().dep_index_inline_cap` at compile time. Group entities (e.g., `group.all_lights`) that exceed the inline cap fall to heap with a tracing warning so operators can see when the inline cap is too small for their dashboard. On each `state_changed`, the evaluator looks up affected widgets in O(1) and only re-evaluates their predicates. Per-widget cached `last_visible: bool` so unchanged outcomes don't trigger any UI work. Unknown predicates are rejected as Errors at validation time (Phase 4 hook), never at runtime.
9. Tests: golden render per widget at idle/active/unavailable; 30-min RSS soak for camera **on both Pi 4 and OPi Zero 3** with the per-profile cap honored; visibility-flip golden test (Task 1) now passes.

### 6c: Lovelace migration helper (highest unknown surface)

#### Tasks
1. Author `tools/lovelace-import/fixtures/`: at least 6 fixture pairs covering common real-world Lovelace dashboards (`entities`, `light`, `media-control`, `thermostat`, `picture-entity`, `glance`, `vertical-stack`, `horizontal-stack`). Each fixture is `<name>.lovelace.yaml` + `<name>.expected.hanui.yaml` + `<name>.expected.unmapped.txt`.
2. Define `MappingTable` enum listing every Lovelace card type the importer recognizes. Cards outside the enum go to an `# UNMAPPED:` block in the output. Document each mapping in `tools/lovelace-import/MAPPINGS.md`.
3. Implement `tools/lovelace-import/` standalone CLI consuming Lovelace YAML or storage JSON; emit a hanui `dashboard.yaml`-compatible document. The importer **runs the Phase 4 validator on its own output before writing** and refuses to emit a dashboard that exceeds `DeviceProfile.max_widgets_per_view` for the target profile (default `rpi4`, override via `--profile`). Over-budget imports either get split into multiple views (default) or fail with a clear error (`--no-split`).
4. **Integration path**: the importer **never overwrites** an existing `dashboard.yaml`. It writes to `dashboard.lovelace-import.yaml` next to the user's config and prints the merge instructions. No silent merging. Importer refuses to run if the target output already exists unless `--force` is passed.
5. E2E test runs the importer against the fixtures and diffs the output against the checked-in `expected.hanui.yaml` and `expected.unmapped.txt`. **CI fence**: a separate test asserts that every `WidgetKind` enum variant has at least one fixture exercising it as either a mapped output or an `UNMAPPED` entry; adding a new widget kind without updating fixtures fails CI. This catches silent fixture drift when Phase 6 grows.

### Phase 6 acceptance criteria (all sub-phases)
- Each new widget renders correctly in at least three fixture states (idle, active, unavailable).
- Climate setpoint slider issues `climate.set_temperature` and tile updates within Phase 3 latency targets.
- Media transport works against a real `media_player.*` entity; artwork async.
- Camera snapshot tile updates on schedule; 30-min soak on **both** Pi 4 and OPi Zero 3 stays bounded; fullscreen dismisses on tap.
- History graph renders ≤ 60 sample points by default; ≤ 1 repaint/min unless visibility changes.
- Lovelace importer round-trips the published sample set with ≤ 10% UNMAPPED cards; never writes to the live `dashboard.yaml`.
- Conditional `visibility:` hides/shows widgets without layout flicker (verified by Phase-4 golden tests extended for visibility).
- All earlier-phase acceptance criteria still pass on Pi 4 and OPi Zero 3.

### Out of scope
- Custom JS card execution (explicitly not supported per `docs/ARCHITECTURE.md`).
- Streaming MJPEG/WebRTC camera (snapshots only this phase).
- Long-history TimescaleDB-style queries beyond `/api/history/period`.
- A full visual editor for the dashboard YAML.

### Dependencies / risks
- HA history endpoint is heavy; the shared HTTP cache and per-window debounce are load-bearing.
- Camera per-profile cap is the binding constraint on cheap boards — the OPi Zero 3 soak is the gate.
- Lovelace YAML is a moving target; importer must warn loudly and never silently lose configuration.
- Conditional visibility interacts with the Phase-4 packer; test interactions are part of 6b's gate.
