# Dashboard Schema

This document is the **source of truth** for the `dashboard.yaml` configuration
file consumed by `src/dashboard/loader.rs`. The schema is locked at Phase 4
(`docs/plans/2026-04-29-phase-4-layout.md`, `locked_decisions.schema_finalization_gate`).
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
**Registered values** (Phase 1 set): `light_tile`, `sensor_tile`, `entity_tile`  
**Forward-compat values** (schema-locked in Phase 4, renderer in Phase 6+):
`camera_tile`, `history_tile`, `fan_tile`, `lock_tile`, `alarm_tile`

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
Known predicates are stored as opaque strings and passed through. Unknown
predicates fail validation with `ValidationRule::UnknownVisibilityPredicate`.

See **Visibility predicates** section below for the locked namespace.

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
```

`interval_seconds`: snapshot refresh interval in seconds. Validated against
`DeviceProfile.camera_interval_min_s`.

### `options.history`

```yaml
options:
  history:
    window_seconds: 86400    # u32; see Bounds section
```

`window_seconds`: width of the history window in seconds. Validated against
`DeviceProfile.history_window_max_s`.

### `options.fan`

```yaml
options:
  fan:
    speed_count: 3                  # u8; number of discrete speed steps
    preset_modes: ["low", "high"]   # Vec<String>; named preset mode labels
```

`speed_count`: number of discrete speed levels (0 means not supported).  
`preset_modes`: list of named presets exposed in the fan control UI.

### `options.lock`

```yaml
options:
  lock:
    pin_policy:
      code_format: "Number"    # String; the HA lock's code_format attribute value
```

`pin_policy.code_format`: must be a string value. A non-string value is a
`ValidationRule::PinPolicyInvalidCodeFormat` Error.

### `options.alarm`

```yaml
options:
  alarm:
    pin_policy:
      code_format: "Number"    # String; the HA alarm's code_format attribute value
```

Same structure as `options.lock.pin_policy`.

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
| `options.{image}.px` (max) | `max_image_px` | (pre-decode downscale; operator notified) | Warning |

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

Phase 4 locks the predicate namespace even though evaluation lands in Phase 6.
The schema stores predicate strings as opaque values and passes them through. An
unknown predicate (not in the list below) is a `ValidationRule::UnknownVisibilityPredicate`
Error at load time, so schemas written for Phase 4 do not silently ignore predicates
that future clients would evaluate.

**Locked predicate namespace (Phase 4)**:

| Predicate | Description |
|---|---|
| `always` | Widget is always visible (default). |
| `never` | Widget is never rendered (useful for disabling without removing). |
| `entity_available:<entity_id>` | Visible when the named entity's state is not `unavailable` or `unknown`. |
| `state_equals:<entity_id>:<value>` | Visible when the named entity's state string equals the given value. |
| `profile:<profile_key>` | Visible only on the named device profile (`rpi4`, `opi-zero3`, `desktop`). |

Predicates not in this list fail validation. Evaluation logic ships in Phase 6.

---

## Validation severity

Severity rules are verbatim from `locked_decisions.validation_severity` in
`docs/plans/2026-04-29-phase-4-layout.md`. Implementers MUST NOT soften or harden
these without a plan amendment.

### Error (halts load; no partial render)

- **`SpanOverflow`**: a single widget's `preferred_columns > section.grid.columns`.
- **`UnknownWidgetType`**: `type:` value not in the registered `WidgetKind` set.
- **`UnknownVisibilityPredicate`**: `visibility:` value not in the locked predicate namespace (Phase 4 locks the namespace even though evaluation is Phase 6).
- **`NonAllowlistedCallService`**: a `call-service` action references a service not in the per-domain allowlist.
- **`MaxWidgetsPerViewExceeded`**: widget count in a view exceeds `DeviceProfile.max_widgets_per_view`.
- **`CameraIntervalBelowMin`**: `options.camera.interval_seconds < DeviceProfile.camera_interval_min_s`.
- **`HistoryWindowAboveMax`**: `options.history.window_seconds > DeviceProfile.history_window_max_s`.
- **`PinPolicyInvalidCodeFormat`**: `options.lock.pin_policy.code_format` or `options.alarm.pin_policy.code_format` is not a string value.

### Warning (renders with a banner; does not halt load)

- **`ImageSizeAboveMax`**: image options exceed `DeviceProfile.max_image_px`; pre-decode downscale is applied and the operator is notified.
- **`CameraIntervalBelowDefault`**: `options.camera.interval_seconds` is between `camera_interval_min_s` and `camera_interval_default_s` — allowed but flagged because the interval is tighter than the profile's recommended default.

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

There is no hot-reload in Phase 4. Restart is the update mechanism.
