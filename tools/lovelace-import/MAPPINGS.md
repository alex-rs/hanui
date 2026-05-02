# Lovelace → hanui card mapping

This document is the source of truth for the eight Lovelace card types the
importer recognises and how each maps to a hanui `WidgetKind`. The vocabulary
is locked by `locked_decisions.lovelace_minimum_card_set` in
`docs/plans/2026-04-30-phase-6-advanced-widgets.md`.

## Card type → hanui WidgetKind

| Lovelace `type:`     | Hanui `WidgetKind` | Notes                                              |
|----------------------|--------------------|----------------------------------------------------|
| `entities`           | `EntityTile`       | Entity list; `entities:` array forwarded as-is.    |
| `glance`             | `EntityTile`       | Compact multi-entity row, same target tile.        |
| `light`              | `LightTile`        | Single-light tile.                                 |
| `thermostat`         | `Climate`          | Also alias for Lovelace `type: climate`.           |
| `media-control`      | `MediaPlayer`      | Single-media-player tile.                          |
| `picture-entity`     | `Camera`           | Lovelace's image-with-state card.                  |
| `vertical-stack`     | (container)        | Container; importer recurses into `cards:`.        |
| `horizontal-stack`   | (container)        | Container; importer recurses into `cards:`.        |

Any `type:` value not in the table above is preserved as an UNMAPPED entry
(see "UNMAPPED log format" below) and surfaced to the user — never silently
dropped. This is the Risk #5 mitigation: Lovelace YAML is not formally
versioned, so unknown types are expected.

## Container handling

`vertical-stack` and `horizontal-stack` cards do not produce a hanui widget
themselves. The importer recurses into the container's `cards:` field and
emits the resulting widgets in order into the same `Section`. Hanui has no
equivalent layout concept at the importer's level — the user re-flows the
imported widgets into the desired grid by hand.

## WidgetKind coverage

The importer's `mappings::widget_kind_coverage()` lists every hanui
`WidgetKind` variant and the Lovelace card (if any) that maps to it. The
`every_widget_kind_has_entry` test in `tests/e2e.rs` enforces that adding a
new variant in `src/dashboard/schema.rs` also updates the coverage list.

Variants without a Lovelace equivalent in the locked vocabulary
(`SensorTile`, `History`, `Fan`, `Lock`, `Alarm`, `Cover`, `PowerFlow`)
are reachable in Lovelace dashboards via the generic `entities`/`glance`
cards, or by hand-editing the imported YAML — the importer does not invent
mappings the locked vocabulary does not authorise.

## Fixture file naming

Every fixture under `fixtures/` is a triple:

| Suffix                          | Role                                              |
|---------------------------------|---------------------------------------------------|
| `<name>.lovelace.yaml`          | Lovelace input.                                   |
| `<name>.expected.hanui.yaml`    | Expected hanui YAML (byte-equal to importer out). |
| `<name>.expected.unmapped.txt`  | Expected UNMAPPED log lines, one per line.        |

The `<name>` segment uses snake_case to match the corresponding test name in
`tests/e2e.rs` (e.g. `vertical_stack` → `round_trip_vertical_stack`).

The mandatory six fixtures (per `locked_decisions.lovelace_minimum_card_set`)
are:

- `entities`
- `glance`
- `light`
- `thermostat`
- `vertical_stack`
- `horizontal_stack`

The repository also ships fixtures for `media_control`, `picture_entity`, and
an UNMAPPED placeholder (`unmapped_button`) so the CI gate sees the full
locked vocabulary plus an UNMAPPED-handling exemplar.

## UNMAPPED log format

The CLI writes a stderr summary and appends a `# UNMAPPED:` comment block to
the emitted YAML. Each entry is one line in the format:

```
view=<view-title> card=<card-index> type=<lovelace-type-string>
```

Where `<card-index>` is the zero-based position of the card in the view's
top-level `cards:` array (container children flatten into the same view's
section). The `<lovelace-type-string>` is the verbatim `type:` value from the
input. The pseudo-values `<missing>` and `<non-string>` are emitted when the
`type:` field is absent or not a YAML string.

## CLI policy

Per `locked_decisions.lovelace_import_output_path`:

- Default output path: `dashboard.lovelace-import.yaml` in cwd.
- `--output <path>` overrides the default.
- `--force` bypasses the existing-file check for any path EXCEPT one whose
  basename equals `dashboard.yaml` (the production file). The importer NEVER
  overwrites the production file, regardless of `--force`.
- `--stdout` writes the YAML to stdout instead of a file. The UNMAPPED log
  still goes to stderr in this mode.
