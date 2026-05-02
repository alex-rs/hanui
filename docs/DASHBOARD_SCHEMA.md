# Dashboard Schema

This document is the **source of truth** for the `dashboard.yaml` configuration
file consumed by `src/dashboard/loader.rs`. The schema was locked at Phase 4
(`docs/plans/2026-04-29-phase-4-layout.md`, `locked_decisions.schema_finalization_gate`)
and extended in Phase 6 (`docs/plans/2026-04-30-phase-6-advanced-widgets.md`,
`locked_decisions.schema_finalization_gate` part (a) follow-on).

After the lock, schema additions require an explicit follow-on PR; any diff to
`src/dashboard/schema.rs` must be accompanied by a diff to this document (enforced
by the CI co-travel check added in TASK-093).

The schema-lock round-trip test in `tests/integration/schema_lock.rs` (TASK-089)
asserts that every field documented here round-trips through serde using the types
in `src/dashboard/schema.rs`. A field present in the doc but absent from the struct
(or vice versa) fails CI.

---

## Top-level structure

```yaml
version: 1
device_profile: desktop          # rpi4 | opi-zero3 | desktop  (kebab-case)
home_assistant:
  url: "ws://homeassistant.local:8123/api/websocket"
  token_env: "HA_TOKEN"          # env-var NAME; actual lookup delegated to platform layer
theme:
  mode: dark                     # dark | light
  accent: "#03a9f4"
default_view: home               # must match an id in views[]
views:
  - id: home
    title: Home
    layout: sections
    sections:
      - id: lights
        title: Lights
        grid:
          columns: 4             # u8; number of grid columns in this section
          gap: 8                 # u8; gap between cells in logical pixels
        widgets:
          - id: kitchen_light
            type: light_tile
            entity: light.kitchen
            entities: []
            name: "Kitchen Light"
            icon: "mdi:lightbulb"
            visibility: "always"
            tap_action:
              action: call-service
              domain: light
              service: turn_on
              target: light.kitchen
            hold_action:
              action: more-info
            double_tap_action:
              action: none
            layout:
              preferred_columns: 2
              preferred_rows: 1
            options: {}
```

---

## `version`

**Type**: `u32`  
**Required**: yes  
**Allowed value**: `1` (the only valid version in Phase 4; free-number fails validation)

Version is a future migration discriminant. The loader rejects any value other than `1`
with an Error.

---

## `device_profile`

**Type**: kebab-case string literal  
**Required**: no (defaults to `desktop` when absent)  
**Allowed values**: `rpi4`, `opi-zero3`, `desktop`

Free-string values fail validation (`ValidationRule::UnknownDeviceProfile`).
The kebab-case strings match the `ProfileKey` serde tagging contract defined in
`src/dashboard/profiles.rs` (TASK-080). The selector function
`select_profile(yaml_override: Option<&str>) -> &'static DeviceProfile` returns
the named preset when present; if absent, falls back to `PROFILE_DESKTOP` (Phase 5
fills in real autodetect).

Do not write `opi_zero3` (underscore). The YAML key uses a hyphen: `opi-zero3`.

---

## `home_assistant`

### `home_assistant.url`

**Type**: `String` (WebSocket URL)  
**Required**: yes  
**Example**: `ws://homeassistant.local:8123/api/websocket`

The WebSocket endpoint for the Home Assistant API. The loader stores the value as
a string; the WS client (Phase 2) opens the connection.

### `home_assistant.token_env`

**Type**: `String` (environment variable name)  
**Required**: yes  
**Example**: `HA_TOKEN`

The **name** of an environment variable that holds the long-lived access token. The
loader reads this field as a plain string. The actual `std::env::var` lookup is
delegated to `src/platform/config.rs` from Phase 2. The loader never calls
`env::var` directly. There is no `${env:...}` interpolation syntax in the YAML
value — the value is always a plain identifier.

---

## `theme`

### `theme.mode`

**Type**: `String`  
**Allowed values**: `dark`, `light`  
**Default**: `dark`

### `theme.accent`

**Type**: CSS hex color string  
**Example**: `#03a9f4`

---

## `views[]`

An ordered list of view definitions. At least one view is required.

### `views[].id`

**Type**: `String` (identifier, no spaces)  
**Required**: yes  
**Uniqueness**: IDs must be unique across all views in the config.

### `views[].title`

**Type**: `String`  
**Required**: yes  
Human-readable label shown in the view-switcher.

### `views[].layout`

**Type**: `String`  
**Allowed value**: `sections`  
**Required**: yes  
Phase 4 supports only `sections` layout. Other values fail validation.

### `views[].sections[]`

An ordered list of section definitions within the view.

---

## `sections[]`

### `sections[].id`

**Type**: `String`  
**Required**: yes  
Unique within the parent view.

### `sections[].title`

**Type**: `String`  
**Required**: yes

### `sections[].grid`

Grid parameters for the section.

#### `sections[].grid.columns`

**Type**: `u8`  
**Required**: yes  
Number of columns in the grid. A widget with `preferred_columns > grid.columns`
triggers a validator Error (`ValidationRule::SpanOverflow`). The packer is only
called after the validator confirms no span overflow.

#### `sections[].grid.gap`

**Type**: `u8`  
**Default**: `8`  
Gap between grid cells in logical pixels.

---

## `widgets[]`

### `widgets[].id`

**Type**: `String`  
**Required**: yes  
Unique within the section.

### `widgets[].type`

**Type**: `String`  
**Required**: yes  
**Registered values** (Phase 1–4 set): `light_tile`, `sensor_tile`, `entity_tile`,
`camera`, `history`, `fan`, `lock`, `alarm`  
**Phase 6 additions**: `cover`, `media_player`, `climate`, `power_flow`

An unknown value fails validation (`ValidationRule::UnknownWidgetType`).

### `widgets[].entity`

**Type**: `String` (Home Assistant entity ID, e.g., `light.kitchen`)  
**Required**: no (some multi-entity widgets use `entities[]` instead)  
Single primary entity for the widget.

### `widgets[].entities`

**Type**: `Vec<String>` (list of Home Assistant entity IDs)  
**Default**: `[]`  
Multiple entity binding for widgets that aggregate state across several entities
(e.g., a sensor group tile). Either `entity` or `entities` (or both) may be
present; neither is required at the schema level. Validation rules per widget type
may impose further constraints.

### `widgets[].name`

**Type**: `String`  
**Required**: no  
Display name shown on the tile. When absent, the widget renders the entity's
`friendly_name` from the HA state store.

### `widgets[].icon`

**Type**: `String` (MDI icon identifier, e.g., `mdi:lightbulb`)  
**Required**: no  
When absent, falls back to the entity's domain icon.

### `widgets[].visibility`

**Type**: `String` (predicate expression)  
**Default**: `always`

Phase 4 locks the predicate namespace even though evaluation lands in Phase 6.
Phase 6 widens the namespace per `locked_decisions.visibility_predicate_vocabulary`.
Known predicates are stored as opaque strings and passed through. Unknown
predicates fail validation with `ValidationRule::UnknownVisibilityPredicate`.

See **Visibility predicates** section below for the full namespace.

### `widgets[].tap_action` / `widgets[].hold_action` / `widgets[].double_tap_action`

**Type**: action object (see **Actions** section below)  
**Default for each**: `{ action: none }`

---

## Actions

Actions are kebab-case in YAML (matching Phase 3's serde renames in
`src/actions/schema.rs`):

```yaml
# Toggle entity state (non-idempotent; not queued offline)
tap_action:
  action: toggle

# Call a Home Assistant service (idempotent allowlist; see validate.rs)
tap_action:
  action: call-service
  domain: light
  service: turn_on
  target: light.kitchen    # optional
  data: {}                  # optional; arbitrary JSON object

# Open the entity's more-info modal
hold_action:
  action: more-info

# Navigate to a named view
tap_action:
  action: navigate
  view-id: security

# Open a URL (gated by DeviceProfile.url_action_mode)
tap_action:
  action: url
  href: "https://www.home-assistant.io/"

# No-op
double_tap_action:
  action: none
```

Action key MUST be kebab-case: `call-service`, `more-info`, `navigate`, `url`,
`toggle`, `none`. PascalCase or snake_case values fail deserialization.

`CallService` actions are validated against a per-domain allowlist at load time
(`ValidationRule::NonAllowlistedCallService`). The allowlist is defined alongside
the `Action` enum (TASK-083). Non-allowlisted services are an Error.

**URL action gating**: the `url` action dispatches to `src/actions/url.rs`, which
reads `DeviceProfile.url_action_mode`:
- `Always` — spawn `xdg-open` with the href.
- `Never` — show an error toast; do not shell out.
- `Ask` — defer to a Phase 6 confirmation dialog; no shell-out in Phase 4.

The `url_action_mode` field is documented in **`device_profile` and profiles**
above. Note: the YAML field name is `url_action_mode`, not `allow_url_actions`
(the former name was a Phase 3 draft artifact, corrected in TASK-063).

---

## `widgets[].layout`

### `widgets[].layout.preferred_columns`

**Type**: `u8`  
**Default**: `1`

The number of grid columns the widget prefers to span. The packer honors this value
when the section grid has enough free columns. If `preferred_columns >
section.grid.columns`, the validator emits `ValidationRule::SpanOverflow` (Error)
and load halts. There is no silent clamp.

### `widgets[].layout.preferred_rows`

**Type**: `u8`  
**Default**: `1`

The number of grid rows the widget prefers to span. Per the TASK-078 prototype
verdict (`PROTOTYPE_VERDICT: GREEN`), Slint's `GridLayout` can express multi-row
spans, so this field carries full multi-row span semantics — a widget with
`preferred_rows: 2` occupies two grid rows. The packer reserves
`preferred_columns × preferred_rows` cells in the first-fit pass.

Implementation note for TASK-084: Slint `GridLayout colspan/rowspan` does not give
proportional widths/heights. Column proportions are achieved with
`HorizontalLayout + horizontal-stretch: N`; row proportions with
`VerticalLayout + vertical-stretch: N`. The user-facing schema key remains
`preferred_rows` regardless of the Slint primitive used.

---

## `widgets[].options`

Per-widget typed options block. The `options` field is a map keyed by domain name.
Only the sub-block matching the widget's domain is read; other keys are ignored.
Numeric bounds are NOT embedded in the schema types — they are enforced at
validation time by the active `DeviceProfile` (see **Bounds** section).

### `options.camera`

```yaml
options:
  camera:
    interval_seconds: 5    # u32; see Bounds section
    url: "http://..."      # String; snapshot or MJPEG stream URL (Phase 6)
```

`interval_seconds`: snapshot refresh interval in seconds. Validated against
`DeviceProfile.camera_interval_min_s`.

`url`: (Phase 6) stream URL for the camera feed. Required field — specifies the
snapshot or MJPEG endpoint to poll.

### `options.history`

```yaml
options:
  history:
    window_seconds: 86400    # u32; see Bounds section
    max_points: 60           # u32; default 60, max 240 (Phase 6)
```

`window_seconds`: width of the history window in seconds. Validated against
`DeviceProfile.history_window_max_s`.

`max_points`: (Phase 6) maximum number of data points after LTTB downsampling.
Default `60`. Validator enforces max `240` per `locked_decisions.history_render_path`.
Values exceeding `240` are `ValidationRule::HistoryMaxPointsExceeded` (Error).

### `options.fan`

```yaml
options:
  fan:
    speed_count: 3                  # u32; number of discrete speed steps
    preset_modes: ["low", "high"]   # Vec<String>; named preset mode labels
```

`speed_count`: number of discrete speed levels (0 means not supported).  
`preset_modes`: list of named presets exposed in the fan control UI.

### `options.lock`

```yaml
options:
  lock:
    pin_policy: none                    # PinPolicy::None
    # or:
    pin_policy:
      required:
        length: 4                       # u8; expected PIN length
        code_format: number             # number | any
    require_confirmation_on_unlock: false   # bool; default false (Phase 6)
```

`pin_policy`: controls PIN requirements for lock/unlock. See **`pin_policy`** section.

`require_confirmation_on_unlock`: (Phase 6) when `true`, the UI shows a confirmation
dialog before dispatching an `Unlock` action. Per `locked_decisions.confirmation_on_lock_unlock`:
this flag lives in `WidgetOptions::Lock`, NOT in the Action variant, so offline
queue replay skips the confirmation prompt correctly (the action was already confirmed
at the original dispatch time).

### `options.alarm`

```yaml
options:
  alarm:
    pin_policy: none                    # PinPolicy::None
    # or:
    pin_policy:
      required:
        length: 4
        code_format: number
    # or (alarm-only):
    pin_policy:
      required_on_disarm:
        length: 4
        code_format: number
```

`pin_policy`: controls PIN requirements for alarm arm/disarm. See **`pin_policy`** section.
`RequiredOnDisarm` is valid only on alarm widgets (not lock). The validator emits
`ValidationRule::PinPolicyRequiredOnDisarmOnLock` (Error) if used on a lock widget.

### `options.cover`

**Phase 6 addition** per `locked_decisions.cover_position_bounds`.

```yaml
options:
  cover:
    position_min: 0     # u8; minimum position (inclusive), 0..=100
    position_max: 100   # u8; maximum position (inclusive), 0..=100
```

`position_min`: minimum position value for the position slider UI. Must be ≤ `position_max`
and ≤ 100. The Slint `PositionSlider` component is bounded by these values at render time.

`position_max`: maximum position value. Must be ≥ `position_min` and ≤ 100.

Validation: `position_min > position_max` or either value > 100 is
`ValidationRule::CoverPositionOutOfBounds` (Error).

### `options.media_player`

**Phase 6 addition**.

```yaml
options:
  media_player:
    transport_set:         # Vec<MediaTransport>
      - play
      - pause
      - stop
      - next              # NonIdempotent — advances track
      - prev              # NonIdempotent — goes back
      - volume_up
      - volume_down
      - mute
    volume_step: 0.05     # f32; volume step per tap, 0.0 < step ≤ 1.0
```

`transport_set`: the set of transport controls to expose in the media player UI.
Must contain at least one entry (empty set is `ValidationRule::MediaTransportNotAllowed` Error).
Allowed values: `play`, `pause`, `stop`, `next`, `prev`, `volume_up`, `volume_down`, `mute`.
Per `locked_decisions.idempotency_marker_phase6_variants`: `Next` and `Prev` are
NonIdempotent (must not be queued offline; fail loudly if dispatched while offline).

`volume_step`: volume increment/decrement per tap. Must be > 0.0. A zero or negative
value is `ValidationRule::MediaTransportNotAllowed` (Error).

### `options.climate`

**Phase 6 addition**.

```yaml
options:
  climate:
    min_temp: 16.0          # f32; minimum setpoint temperature
    max_temp: 30.0          # f32; maximum setpoint temperature; must be > min_temp
    step: 0.5               # f32; setpoint adjustment step; must be > 0.0
    hvac_modes:             # Vec<String>; free strings per locked_decisions.hvac_mode_vocabulary
      - heat
      - cool
      - heat_cool
      - off
```

`min_temp`: minimum setpoint temperature (°C or °F, per HA's unit configuration).

`max_temp`: maximum setpoint temperature. Must be strictly greater than `min_temp`.
`min_temp >= max_temp` is `ValidationRule::ClimateMinMaxTempInvalid` (Error).

`step`: setpoint adjustment increment. Must be > 0.0.
`step <= 0.0` is `ValidationRule::ClimateMinMaxTempInvalid` (Error).

`hvac_modes`: free strings per `locked_decisions.hvac_mode_vocabulary` — HA allows
custom HVAC modes beyond the standard set. The UI picker shows only the modes listed
here. Standard HA modes: `off`, `heat`, `cool`, `heat_cool`, `auto`, `dry`, `fan_only`.

### `options.power_flow`

**Phase 6 addition** (6d sub-phase). Per `locked_decisions.power_flow_subphase_placement`,
detailed validator rules (`PowerFlowGridEntityNotPower`, `PowerFlowBatteryWithoutSoC`,
`PowerFlowIndividualLaneCountExceeded`) are owned by TASK-094.

```yaml
options:
  power_flow:
    grid_entity: sensor.grid_power             # String; required; must be power-class sensor
    solar_entity: sensor.solar_power           # String; optional
    battery_entity: sensor.battery_power       # String; optional; requires battery_soc_entity
    battery_soc_entity: sensor.battery_soc     # String; optional; required if battery_entity set
    home_entity: sensor.home_power             # String; optional
```

`grid_entity`: entity ID for the grid connection. Required. The TASK-094 validator
checks this is a `sensor` domain entity with state class `power`
(`ValidationRule::PowerFlowGridEntityNotPower`, Error).

`solar_entity`: optional solar production entity.

`battery_entity`: optional battery entity. When present, `battery_soc_entity` is
also required; omitting it is `ValidationRule::PowerFlowBatteryWithoutSoC` (Warning).

`battery_soc_entity`: optional state-of-charge entity (0–100 integer sensor). Required
when `battery_entity` is set.

`home_entity`: optional home consumption entity.

---

## `pin_policy`

**Phase 6 change**: `pin_policy` is now an enum (previously a struct with `code_format: String`).
Per `locked_decisions.pin_policy_migration`.

The three variants:

```yaml
# No PIN required
pin_policy: none

# PIN required on every action
pin_policy:
  required:
    length: 4          # u8; expected PIN length in digits/characters
    code_format: number   # number | any

# PIN required only on disarm (ALARM widgets only; Error on lock widgets)
pin_policy:
  required_on_disarm:
    length: 4
    code_format: number
```

**`code_format`** (closed enum, replaces the previous free-string field):
- `number` — PIN must consist of digits only.
- `any` — PIN may contain any characters.

**`PinPolicy::RequiredOnDisarm`** is valid ONLY on `WidgetOptions::Alarm`. A lock
widget with `required_on_disarm` is `ValidationRule::PinPolicyRequiredOnDisarmOnLock`
(Error). Alarm widgets accept all three variants.

---

## Bounds

Numeric option bounds are NOT embedded in the schema types in
`src/dashboard/schema.rs`. They depend on the active `DeviceProfile` at validation
time and are checked exclusively in `src/dashboard/validate.rs`.

| Option field | Bound source on DeviceProfile | Violation rule | Severity |
|---|---|---|---|
| `options.camera.interval_seconds` (min) | `camera_interval_min_s` | `CameraIntervalBelowMin` | Error |
| `options.camera.interval_seconds` (range) | `camera_interval_min_s` .. `camera_interval_default_s` | (Warning if below default but above min) | Warning |
| `options.history.window_seconds` (max) | `history_window_max_s` | `HistoryWindowAboveMax` | Error |
| `options.history.max_points` (max) | constant 240 | `HistoryMaxPointsExceeded` | Error |
| `options.{image}.px` (max) | `max_image_px` | (pre-decode downscale; operator notified) | Warning |
| `options.cover.position_min/max` | 0..=100 range + min ≤ max | `CoverPositionOutOfBounds` | Error |
| `options.climate.min_temp/max_temp/step` | min < max, step > 0 | `ClimateMinMaxTempInvalid` | Error |
| `options.media_player.volume_step` | > 0.0 | `MediaTransportNotAllowed` | Error |
| `options.media_player.transport_set` | non-empty | `MediaTransportNotAllowed` | Error |
| `options.power_flow.battery_soc_entity` | required if battery_entity set | `PowerFlowBatteryWithoutSoC` | Warning |

Preset values for each profile:

| Field | `rpi4` | `opi-zero3` | `desktop` |
|---|---|---|---|
| `camera_interval_min_s` | 5 | 10 | 1 |
| `camera_interval_default_s` | 10 | 30 | 5 |
| `history_window_max_s` | 86400 | 43200 | 604800 |
| `max_image_px` | 1280 | 800 | 2048 |
| `max_widgets_per_view` | 32 | 20 | 64 |

**Migration path when bounds tighten**: if `DeviceProfile` bounds are tightened
(e.g., `camera_interval_min_s` raised), previously valid YAML may become invalid on
the next load. The error screen shows the offending field path and the current bound
from the active profile so operators know exactly what to edit. There is no automatic
migration or silent clamping; the config must be edited manually.

---

## Visibility predicates

Phase 4 locked the predicate namespace; Phase 6 widens it per
`locked_decisions.visibility_predicate_vocabulary`. The schema stores predicate strings
as opaque values and passes them through. An unknown predicate (not in the list below)
is a `ValidationRule::UnknownVisibilityPredicate` Error at load time.

**Full predicate namespace (Phase 4 + Phase 6)**:

| Predicate | Description | Phase |
|---|---|---|
| `always` | Widget is always visible (default). | Phase 4 |
| `never` | Widget is never rendered (useful for disabling without removing). | Phase 4 |
| `entity_available:<entity_id>` | Visible when the named entity's state is not `unavailable` or `unknown`. Alias for `<id> != unavailable`. | Phase 4 |
| `state_equals:<entity_id>:<value>` | Visible when the named entity's state string equals the given value. Alias for `<id> == <value>`. | Phase 4 |
| `profile:<profile_key>` | Visible only on the named device profile (`rpi4`, `opi-zero3`, `desktop`). | Phase 4 |
| `<entity_id> == <value>` | Entity state string equality. | Phase 6 |
| `<entity_id> != <value>` | Entity state string inequality. | Phase 6 |
| `<entity_id> in [<v1>,<v2>,...]` | Entity state is in the given list. | Phase 6 |
| `entity_state_numeric:<entity_id>:<op>:<N>` | Numeric comparison: `op` is `lt`/`lte`/`gt`/`gte`/`eq`/`ne`; `N` is f64-parseable. Example: `entity_state_numeric:sensor.temp:gt:20`. | Phase 6 |

Phase 4 alias forms (`entity_available:*`, `state_equals:*`) remain valid for
backward compatibility with dashboards authored during Phase 4 testing.

Evaluation logic ships in Phase 6 (TASK-110). Phase 4 and 6.0 only validate the namespace.

---

## Validation severity

Severity rules are verbatim from `locked_decisions.validation_severity` in
`docs/plans/2026-04-29-phase-4-layout.md` and
`locked_decisions.validation_rule_identifiers` in
`docs/plans/2026-04-30-phase-6-advanced-widgets.md`. Implementers MUST NOT soften or
harden these without a plan amendment.

### Error (halts load; no partial render)

- **`SpanOverflow`**: a single widget's `preferred_columns > section.grid.columns`.
- **`UnknownWidgetType`**: `type:` value not in the registered `WidgetKind` set.
- **`UnknownVisibilityPredicate`**: `visibility:` value not in the locked predicate namespace.
- **`NonAllowlistedCallService`**: a `call-service` action references a service not in the per-domain allowlist.
- **`MaxWidgetsPerViewExceeded`**: widget count in a view exceeds `DeviceProfile.max_widgets_per_view`.
- **`CameraIntervalBelowMin`**: `options.camera.interval_seconds < DeviceProfile.camera_interval_min_s`.
- **`HistoryWindowAboveMax`**: `options.history.window_seconds > DeviceProfile.history_window_max_s`.
- **`HistoryMaxPointsExceeded`**: `options.history.max_points > 240` (Phase 6).
- **`PinPolicyRequiredOnDisarmOnLock`**: `options.lock.pin_policy` is `RequiredOnDisarm` — only valid on alarm widgets (Phase 6, replaces `PinPolicyInvalidCodeFormat`).
- **`CoverPositionOutOfBounds`**: `options.cover.position_min > position_max` or either bound > 100 (Phase 6).
- **`ClimateMinMaxTempInvalid`**: `options.climate.min_temp >= max_temp` or `step <= 0.0` (Phase 6).
- **`MediaTransportNotAllowed`**: `options.media_player.transport_set` is empty or `volume_step <= 0.0` (Phase 6).

### Warning (renders with a banner; does not halt load)

- **`ImageOptionExceedsMaxPx`**: image options exceed `DeviceProfile.max_image_px`; pre-decode downscale is applied and the operator is notified.
- **`CameraIntervalBelowDefault`**: `options.camera.interval_seconds` is between `camera_interval_min_s` and `camera_interval_default_s` — allowed but flagged because the interval is tighter than the profile's recommended default.
- **`PowerFlowBatteryWithoutSoC`**: `options.power_flow.battery_entity` is set but `battery_soc_entity` is absent — the SoC label cannot be rendered (Phase 6, owned by TASK-094).

Severity changes require a plan amendment approved by the founder. The schema-lock
test in TASK-089 includes a `severity_pin` test that asserts each rule's severity by
name, so softening a rule silently is detected automatically.

---

## Migration notes

When `DeviceProfile` bounds tighten (e.g., `camera_interval_min_s` increases),
previously valid YAML may fail validation on next load. The error screen names the
offending field path (e.g., `views[0].sections[0].widgets[2].options.camera.interval_seconds`)
and the current bound from the active profile. No automatic migration or silent
clamping occurs. Operators must edit the config file manually.

There is no hot-reload in Phase 4/6. Restart is the update mechanism.

### Phase 6 migration: `PinPolicy` struct → enum

**Before (Phase 4)**:
```yaml
options:
  lock:
    pin_policy:
      code_format: "Number"
```

**After (Phase 6)**:
```yaml
options:
  lock:
    pin_policy:
      required:
        length: 4
        code_format: number
    # or:
    pin_policy: none
```

The `code_format: "Number"` or `code_format: "Any"` string form is no longer valid.
Use `pin_policy: none` or `pin_policy: { required: { length: N, code_format: number|any } }`.

---

## Runtime-only fields (not part of the YAML schema)

Some fields on the in-memory `Dashboard` struct exist for runtime indexing only.
They are annotated `#[serde(default, skip)]` in `src/dashboard/schema.rs`, are
NOT serialized to nor deserialized from YAML, and MUST NOT appear in any
`dashboard.yaml` file authored by an operator. The Phase 4/6 round-trip test
(`round_trip_dashboard_yaml_is_byte_equal` in `src/dashboard/schema.rs`) pins
this contract.

### `dep_index`

**Type**: `Arc<HashMap<EntityId, Vec<WidgetId>>>` (runtime-only)
**Authored by**: `src/dashboard/visibility.rs::build_dep_index` (Phase 6b TASK-110)
**User-facing**: no — populated by the loader after the YAML is parsed and
validated.

The reverse `EntityId → Vec<WidgetId>` index used by the bridge layer to resolve,
in O(1), which widgets need a visibility re-evaluation when a given entity
changes state. Per `locked_decisions.dep_index_partial_eq`, the manual
`PartialEq` impl for `Dashboard` compares this field structurally (NOT by
`Arc::ptr_eq`).

### `call_service_allowlist`

**Type**: `Arc<BTreeSet<(String, String)>>` (runtime-only)
**Authored by**: `src/dashboard/validate.rs::validate` (Phase 4 TASK-083)
**User-facing**: no — populated by the validator after the YAML is parsed.

The per-config allowlist of `(domain, service)` pairs used at runtime to gate
`CallService` action dispatches. See
`locked_decisions.call_service_allowlist_runtime_access`.
