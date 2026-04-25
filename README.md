# HA Native Slint Claude Kit

Claude Code kit for building a native Rust + Slint Home Assistant dashboard for low-power SBCs.

## Included skills

```text
.claude/skills/
  project-architecture/
  html-to-slint-widget/
  ha-state-engine/
  ha-action-dispatcher/
  dashboard-layout-engine/
  theme-and-design-tokens/
  kiosk-runtime/
  render-optimization/
```

The 7 core skills are `html-to-slint-widget`, `ha-state-engine`, `ha-action-dispatcher`, `dashboard-layout-engine`, `theme-and-design-tokens`, `kiosk-runtime`, and `render-optimization`. `project-architecture` is included as an umbrella skill.

## Included agent

```text
.claude/agents/ha-native-dashboard-architect.md
```

## Included docs

```text
docs/ARCHITECTURE.md
docs/DASHBOARD_SCHEMA.md
docs/ROADMAP.md
```

## Install

Copy `.claude/` into your project root and restart/reload Claude Code.

## Example prompts

```text
Use the ha-native-dashboard-architect agent to create the initial Rust + Slint project skeleton from docs/ARCHITECTURE.md.
Use the ha-state-engine skill to implement a fixture-backed entity store.
Use the html-to-slint-widget skill to port this Lovelace light card to Slint.
Use the kiosk-runtime skill to add DietPi systemd deployment files for Raspberry Pi 4 and Orange Pi Zero 3.
```
