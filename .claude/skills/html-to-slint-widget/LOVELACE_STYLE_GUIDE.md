# Lovelace-to-Slint Style Guide

## Goal

Native widgets should preserve the Home Assistant Lovelace mental model: dashboard → view → section/grid/masonry → card → feature/control.

## Visual language

Default card: radius 12–16 px, padding 12–16 px, gap 6–12 px, minimum touch target 44 px, preferred tile min size 150x96 px.

Avoid desktop chrome, menu bars, title bars, dense tables, hover-only controls, and browser-like styling.

## Information hierarchy

1. entity identity: name/icon/area
2. primary state: on/off, temperature, number, mode
3. secondary attributes: unit, brightness, humidity, battery
4. controls: toggle, slider, selector, plus/minus, action row

## Required visual states

normal, active/on, inactive/off, unavailable, warning/problem, loading, pressed/focused.

## Theme tokens

Use a shared Slint `Theme` global, not hard-coded widget colors.

```slint
export global Theme {
    in property <color> background: #111418;
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

## Domain conventions

| Domain | Primary display | Main controls |
|---|---|---|
| `light` | on/off + brightness | toggle, brightness |
| `switch` | on/off | toggle |
| `sensor` | value + unit | none |
| `binary_sensor` | detected/clear | none |
| `climate` | current + target temp | temp step, mode |
| `cover` | open/closed/position | open/stop/close |
| `fan` | on/off + percentage | toggle/speed |
| `media_player` | playing/paused/source | playback/volume |
| `alarm_control_panel` | armed/disarmed | mode/keypad |
| `lock` | locked/unlocked | lock/unlock |
| `camera` | snapshot/stream state | open/details |

## Low-power rules

Raspberry Pi 4 4GB can handle many simple cards, simple charts, and occasional snapshots. Orange Pi Zero 3 2GB should use fewer visible widgets, no continuous animations, snapshots over live video, smaller history buffers, and paged dashboards.
