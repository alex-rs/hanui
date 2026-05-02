//! Power-flow widget view-model and per-frame state derivation (TASK-094).
//!
//! # Hot-path discipline
//!
//! [`PowerFlowVM::from_entity`] is invoked at entity-change time, NOT per
//! render. Because the power-flow tile binds up to five entities (grid /
//! solar / battery / battery_soc / home), the bridge calls
//! [`PowerFlowVM::from_entity`] once per state-change event for any of the
//! configured entities; the resulting [`PowerFlowVM`] is then projected
//! into the Slint-typed `PowerFlowTileVM` (in `bridge.rs`) and pushed via
//! the row-update path. No allocation occurs in any per-frame Slint
//! callback.
//!
//! # Power-watt convention (Home Assistant)
//!
//! Home Assistant emits power-class sensors as numeric `state` values in
//! watts (`unit_of_measurement: W`). The sign convention used by the
//! `power-flow-card-plus` Lovelace card is:
//!
//!   * `grid_w` — positive = importing from grid; negative = exporting to grid.
//!   * `battery_w` — positive = charging; negative = discharging.
//!   * `solar_w` / `home_w` — non-negative magnitudes.
//!
//! HA integrations vary in sign convention; the bridge forwards the raw
//! numeric state verbatim and lets the operator align the entity sign
//! convention via dashboard YAML (Phase 7 may add a `sign_invert` knob,
//! out of scope here).
//!
//! # Why no `Vec` fields (lesson from TASK-103 / TASK-105 / TASK-107 /
//!   TASK-108 / TASK-109)
//!
//! `PowerFlowVM` deliberately carries no `Vec` fields. The full set of
//! configured entities (grid / solar / battery / soc / home) is encoded
//! as five `Option<f32>` scalars instead of a `Vec<Lane>`. The lesson
//! reinforced by every per-domain VM since `FanVM` (TASK-103) is to keep
//! the per-frame VM lean and scalar; allocating a `Vec` per state-change
//! event for a list that the tile renderer would index by lane name
//! would be wasted work.
//!
//! # JSON-crate discipline (CI Gate 2)
//!
//! `src/ui/**` is gated against direct references to the JSON crate by
//! `.github/workflows/ci.yml` Gate 2. The implementation reads
//! `entity.state` only — power sensors emit their numeric value as the
//! entity state, not as an attribute.

use crate::ha::entity::Entity;
use crate::ui::bridge::TilePlacement;
use crate::ui::more_info::{ModalRow, MoreInfoBody};

// ---------------------------------------------------------------------------
// PowerFlowVM
// ---------------------------------------------------------------------------

/// Per-frame derived view-state for a `power_flow` widget.
///
/// Built by [`PowerFlowVM::from_entity`] at entity-change time for any
/// of the configured entities (grid / solar / battery / battery_soc /
/// home). Each call accepts one entity at a time; the bridge composes a
/// full [`PowerFlowVM`] by chaining reads against the live store.
///
/// # Field semantics
///
/// * `name` — user-visible label (typically the widget name from YAML).
/// * `solar_w` — solar production in watts (≥ 0). `None` when no solar
///   entity is configured or the entity is unavailable.
/// * `grid_w` — grid power flow in watts. Positive = importing,
///   negative = exporting. `None` when the entity is unavailable.
/// * `home_w` — home consumption in watts (≥ 0). `None` when no home
///   entity is configured or the entity is unavailable.
/// * `battery_w` — battery power flow in watts. Positive = charging,
///   negative = discharging. `None` when no battery entity is configured.
/// * `battery_pct` — battery state-of-charge in 0..=100. `None` when no
///   `battery_soc_entity` is configured.
/// * `icon_id` — design-token id (typically `mdi:lightning-bolt-circle`);
///   resolved to bytes by the asset layer (TASK-010).
/// * `placement` — packed grid coordinates and span. Static this phase.
/// * `pending` — true while an `OptimisticEntry` exists for any of the
///   power-flow entities. Drives the per-tile spinner overlay on
///   CardBase (TASK-067).
#[derive(Debug, Clone, PartialEq)]
pub struct PowerFlowVM {
    /// User-visible label for the power-flow tile.
    pub name: String,
    /// Solar production in watts (≥ 0), if reported.
    pub solar_w: Option<f32>,
    /// Grid flow in watts; positive = import, negative = export.
    pub grid_w: Option<f32>,
    /// Home consumption in watts (≥ 0), if reported.
    pub home_w: Option<f32>,
    /// Battery flow in watts; positive = charging, negative = discharging.
    pub battery_w: Option<f32>,
    /// Battery state-of-charge in 0..=100, if reported.
    pub battery_pct: Option<f32>,
    /// Design-token icon id (typically `mdi:lightning-bolt-circle`).
    pub icon_id: String,
    /// Computed grid placement assigned by the packer.
    pub placement: TilePlacement,
    /// Per-tile spinner gate (TASK-067). Default `false`.
    pub pending: bool,
}

impl PowerFlowVM {
    /// Read a single entity's state as a power-watts numeric value.
    ///
    /// HA exposes power-class sensors as numeric `state` strings (e.g.
    /// `"1234.5"`). We accept any of `f64` / `i64` / `u64` parses
    /// because integration variants emit different numeric shapes (some
    /// integer-only inverters emit `"500"`; some smart meters emit
    /// `"500.0"`). `NaN` and infinity are treated as absent.
    ///
    /// Returns `None` when:
    ///   * The entity state is `"unavailable"` / `"unknown"` (HA's standard
    ///     unavailable sentinels — we do not surface these as `0.0`).
    ///   * The state string fails to parse as a finite numeric value.
    #[must_use]
    pub fn read_power_watts(entity: &Entity) -> Option<f32> {
        let state = entity.state.as_ref();
        if matches!(state, "unavailable" | "unknown") {
            return None;
        }
        let f: f64 = state.parse().ok()?;
        if !f.is_finite() {
            return None;
        }
        Some(f as f32)
    }

    /// Read a single entity's state as a battery state-of-charge percentage
    /// in 0..=100.
    ///
    /// HA's state-of-charge sensors emit `"0"`..=`"100"` numeric strings.
    /// Out-of-range values are clamped (a misbehaving integration emitting
    /// `"105"` should still render the tile sensibly).
    ///
    /// Returns `None` when the entity state is `"unavailable"` / `"unknown"`
    /// or when the state fails to parse as a finite numeric value.
    #[must_use]
    pub fn read_battery_pct(entity: &Entity) -> Option<f32> {
        let state = entity.state.as_ref();
        if matches!(state, "unavailable" | "unknown") {
            return None;
        }
        let f: f64 = state.parse().ok()?;
        if !f.is_finite() {
            return None;
        }
        Some((f as f32).clamp(0.0, 100.0))
    }
}

// ---------------------------------------------------------------------------
// PowerFlowBody (more-info modal body)
// ---------------------------------------------------------------------------

/// More-info body for `power_flow` widgets (TASK-094).
///
/// Renders one `ModalRow` per configured power-flow entity, threading
/// each through [`PowerFlowVM::read_power_watts`] /
/// [`PowerFlowVM::read_battery_pct`] so the modal sees the same numeric
/// values the tile renders.
///
/// # Carried state
///
/// `PowerFlowBody` is **stateful** — unlike the other per-domain bodies,
/// the body needs to know which auxiliary entities (solar / battery /
/// battery_soc / home) to pull rows for, and the modal's `Entity`
/// argument is the **primary** entity (the grid sensor). The body is
/// constructed once at modal-open time by `body_for_widget` from the
/// `WidgetOptions::PowerFlow` block.
///
/// Per `locked_decisions.more_info_modal`, the body is invoked exactly
/// once per modal-open — so the per-call attribute reads are not on a
/// hot path.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PowerFlowBody {
    /// Optional solar entity reading (already resolved at body-construct
    /// time — read at `body_for_widget` invocation, NOT at `render_rows`).
    pub solar_w: Option<f32>,
    /// Optional battery flow reading.
    pub battery_w: Option<f32>,
    /// Optional battery state-of-charge reading.
    pub battery_pct: Option<f32>,
    /// Optional home consumption reading.
    pub home_w: Option<f32>,
}

impl PowerFlowBody {
    /// Construct a [`PowerFlowBody`] with the auxiliary entity values
    /// already resolved by the bridge.
    ///
    /// The bridge looks up `solar_entity` / `battery_entity` /
    /// `battery_soc_entity` / `home_entity` against the live store at
    /// modal-open time and passes the resulting `Option<f32>` values
    /// here. If an auxiliary entity is unavailable or not configured,
    /// the corresponding row is suppressed.
    #[must_use]
    pub fn new(
        solar_w: Option<f32>,
        battery_w: Option<f32>,
        battery_pct: Option<f32>,
        home_w: Option<f32>,
    ) -> Self {
        PowerFlowBody {
            solar_w,
            battery_w,
            battery_pct,
            home_w,
        }
    }
}

impl MoreInfoBody for PowerFlowBody {
    fn render_rows(&self, entity: &Entity) -> Vec<ModalRow> {
        // Capacity 5 covers the worst case (grid_w + solar_w + battery_w
        // + battery_pct + home_w) without growing.
        let mut rows = Vec::with_capacity(5);

        // grid_w row — always emitted, derived from the primary entity
        // passed in by the modal (the grid sensor). Forwarded as a fixed
        // 1-decimal watts string so the modal does not surface trailing
        // zeros from a `%g` formatter.
        let grid_w = PowerFlowVM::read_power_watts(entity).unwrap_or(0.0);
        rows.push(ModalRow {
            key: "grid_w".to_owned(),
            value: format_watts(grid_w),
        });

        // solar_w row, only when the bridge supplied a solar reading at
        // body-construct time.
        if let Some(solar) = self.solar_w {
            rows.push(ModalRow {
                key: "solar_w".to_owned(),
                value: format_watts(solar),
            });
        }

        // battery_w row, only when the bridge supplied a battery flow.
        if let Some(battery) = self.battery_w {
            rows.push(ModalRow {
                key: "battery_w".to_owned(),
                value: format_watts(battery),
            });
        }

        // battery_pct row, only when the bridge supplied a state-of-charge
        // reading. Rendered as a percentage with no decimals (HA SoC
        // sensors emit integer percentages).
        if let Some(pct) = self.battery_pct {
            rows.push(ModalRow {
                key: "battery_pct".to_owned(),
                value: format!("{}%", pct.round() as i32),
            });
        }

        // home_w row, only when the bridge supplied a home consumption.
        if let Some(home) = self.home_w {
            rows.push(ModalRow {
                key: "home_w".to_owned(),
                value: format_watts(home),
            });
        }

        rows
    }
}

/// Format a power-watts value with one decimal place and a `W` suffix.
///
/// Pulled out as a free function so the row formatting is reusable from
/// tests without re-rendering through the trait. Negative values render
/// with a leading minus (the export / discharge convention) — the
/// modal does not collapse signs.
fn format_watts(w: f32) -> String {
    format!("{w:.1} W")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::entity::EntityId;
    use std::sync::Arc;

    /// Construct a minimal [`Entity`] with an empty attribute map. Mirrors
    /// the helper in `src/ui/cover.rs::tests` / `src/ui/climate.rs::tests`
    /// — uses `Arc::default()` so we do not name the JSON crate (Gate 2).
    fn minimal_entity(id: &str, state: &str) -> Entity {
        Entity {
            id: EntityId::from(id),
            state: Arc::from(state),
            attributes: Arc::default(),
            last_changed: jiff::Timestamp::UNIX_EPOCH,
            last_updated: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    // -----------------------------------------------------------------------
    // read_power_watts
    // -----------------------------------------------------------------------

    /// Standard numeric power value parses correctly.
    #[test]
    fn read_power_watts_positive() {
        let entity = minimal_entity("sensor.grid_power", "1234.5");
        assert_eq!(PowerFlowVM::read_power_watts(&entity), Some(1234.5));
    }

    /// Negative values (export / discharge) preserve sign.
    #[test]
    fn read_power_watts_negative() {
        let entity = minimal_entity("sensor.grid_power", "-500.0");
        assert_eq!(PowerFlowVM::read_power_watts(&entity), Some(-500.0));
    }

    /// Integer-typed power values still parse via the `f64` round-trip.
    #[test]
    fn read_power_watts_integer_value() {
        let entity = minimal_entity("sensor.grid_power", "1234");
        assert_eq!(PowerFlowVM::read_power_watts(&entity), Some(1234.0));
    }

    /// Zero is a real reading (idle), not absent.
    #[test]
    fn read_power_watts_zero() {
        let entity = minimal_entity("sensor.grid_power", "0");
        assert_eq!(PowerFlowVM::read_power_watts(&entity), Some(0.0));
    }

    /// `unavailable` state is rejected (HA's standard unavailable sentinel).
    #[test]
    fn read_power_watts_unavailable() {
        let entity = minimal_entity("sensor.grid_power", "unavailable");
        assert_eq!(PowerFlowVM::read_power_watts(&entity), None);
    }

    /// `unknown` state is also rejected.
    #[test]
    fn read_power_watts_unknown() {
        let entity = minimal_entity("sensor.grid_power", "unknown");
        assert_eq!(PowerFlowVM::read_power_watts(&entity), None);
    }

    /// A non-numeric state string fails to parse.
    #[test]
    fn read_power_watts_non_numeric() {
        let entity = minimal_entity("sensor.grid_power", "high");
        assert_eq!(PowerFlowVM::read_power_watts(&entity), None);
    }

    /// `NaN` / infinity are not finite and resolve to None.
    #[test]
    fn read_power_watts_non_finite() {
        let nan = minimal_entity("sensor.grid_power", "NaN");
        assert_eq!(PowerFlowVM::read_power_watts(&nan), None);
        let inf = minimal_entity("sensor.grid_power", "inf");
        assert_eq!(PowerFlowVM::read_power_watts(&inf), None);
    }

    // -----------------------------------------------------------------------
    // read_battery_pct
    // -----------------------------------------------------------------------

    /// Standard SoC value parses correctly.
    #[test]
    fn read_battery_pct_in_range() {
        let entity = minimal_entity("sensor.battery_soc", "75");
        assert_eq!(PowerFlowVM::read_battery_pct(&entity), Some(75.0));
    }

    /// SoC value above 100 is clamped (defensive against misbehaving
    /// integrations).
    #[test]
    fn read_battery_pct_above_100_clamped() {
        let entity = minimal_entity("sensor.battery_soc", "120");
        assert_eq!(PowerFlowVM::read_battery_pct(&entity), Some(100.0));
    }

    /// Negative SoC value is clamped to 0.
    #[test]
    fn read_battery_pct_negative_clamped() {
        let entity = minimal_entity("sensor.battery_soc", "-5");
        assert_eq!(PowerFlowVM::read_battery_pct(&entity), Some(0.0));
    }

    /// 0% is a real reading (fully drained).
    #[test]
    fn read_battery_pct_zero() {
        let entity = minimal_entity("sensor.battery_soc", "0");
        assert_eq!(PowerFlowVM::read_battery_pct(&entity), Some(0.0));
    }

    /// `unavailable` state resolves to None.
    #[test]
    fn read_battery_pct_unavailable() {
        let entity = minimal_entity("sensor.battery_soc", "unavailable");
        assert_eq!(PowerFlowVM::read_battery_pct(&entity), None);
    }

    /// Non-numeric SoC fails to parse.
    #[test]
    fn read_battery_pct_non_numeric() {
        let entity = minimal_entity("sensor.battery_soc", "full");
        assert_eq!(PowerFlowVM::read_battery_pct(&entity), None);
    }

    /// Infinite SoC is not finite — resolves to None.
    #[test]
    fn read_battery_pct_non_finite() {
        let entity = minimal_entity("sensor.battery_soc", "inf");
        assert_eq!(PowerFlowVM::read_battery_pct(&entity), None);
    }

    // -----------------------------------------------------------------------
    // PowerFlowBody::render_rows — branch coverage
    // -----------------------------------------------------------------------

    /// All-None auxiliary readings: the body emits ONLY the grid_w row.
    #[test]
    fn render_rows_grid_only() {
        let body = PowerFlowBody::new(None, None, None, None);
        let entity = minimal_entity("sensor.grid_power", "1500");
        let rows = body.render_rows(&entity);
        assert_eq!(rows.len(), 1, "no auxiliary entities → 1 row");
        assert_eq!(rows[0].key, "grid_w");
        assert_eq!(rows[0].value, "1500.0 W");
    }

    /// Solar reading is surfaced when present.
    #[test]
    fn render_rows_solar_branch() {
        let body = PowerFlowBody::new(Some(2000.0), None, None, None);
        let entity = minimal_entity("sensor.grid_power", "0");
        let rows = body.render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "solar_w" && r.value == "2000.0 W"),
            "solar_w row must be present: {rows:?}"
        );
    }

    /// Battery flow is surfaced when present.
    #[test]
    fn render_rows_battery_w_branch() {
        let body = PowerFlowBody::new(None, Some(-300.5), None, None);
        let entity = minimal_entity("sensor.grid_power", "0");
        let rows = body.render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "battery_w" && r.value == "-300.5 W"),
            "battery_w row must be present: {rows:?}"
        );
    }

    /// Battery state-of-charge is surfaced when present.
    #[test]
    fn render_rows_battery_pct_branch() {
        let body = PowerFlowBody::new(None, None, Some(82.4), None);
        let entity = minimal_entity("sensor.grid_power", "0");
        let rows = body.render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "battery_pct" && r.value == "82%"),
            "battery_pct row must be present (rounded to integer): {rows:?}"
        );
    }

    /// Home consumption is surfaced when present.
    #[test]
    fn render_rows_home_branch() {
        let body = PowerFlowBody::new(None, None, None, Some(750.0));
        let entity = minimal_entity("sensor.grid_power", "0");
        let rows = body.render_rows(&entity);
        assert!(
            rows.iter()
                .any(|r| r.key == "home_w" && r.value == "750.0 W"),
            "home_w row must be present: {rows:?}"
        );
    }

    /// All auxiliary entities present: 5 rows in the expected order.
    #[test]
    fn render_rows_all_branches_present() {
        let body = PowerFlowBody::new(Some(1500.0), Some(-200.0), Some(50.0), Some(900.0));
        let entity = minimal_entity("sensor.grid_power", "-400");
        let rows = body.render_rows(&entity);
        assert_eq!(rows.len(), 5, "all auxiliary entities → 5 rows");
        assert_eq!(rows[0].key, "grid_w");
        assert_eq!(rows[0].value, "-400.0 W");
        assert_eq!(rows[1].key, "solar_w");
        assert_eq!(rows[2].key, "battery_w");
        assert_eq!(rows[3].key, "battery_pct");
        assert_eq!(rows[4].key, "home_w");
    }

    /// Grid entity in `unavailable` state still emits a grid_w row using
    /// the fall-back zero (the modal must always reflect that the grid
    /// entity is configured, even when its current reading is unknown).
    #[test]
    fn render_rows_grid_unavailable_uses_zero() {
        let body = PowerFlowBody::new(None, None, None, None);
        let entity = minimal_entity("sensor.grid_power", "unavailable");
        let rows = body.render_rows(&entity);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "grid_w");
        assert_eq!(rows[0].value, "0.0 W");
    }

    // -----------------------------------------------------------------------
    // format_watts
    // -----------------------------------------------------------------------

    /// `format_watts` formats a positive value with one decimal place.
    #[test]
    fn format_watts_positive() {
        assert_eq!(format_watts(1234.56), "1234.6 W");
    }

    /// `format_watts` preserves the sign for negative values.
    #[test]
    fn format_watts_negative() {
        assert_eq!(format_watts(-500.0), "-500.0 W");
    }

    /// `format_watts` of zero still renders the trailing decimal.
    #[test]
    fn format_watts_zero() {
        assert_eq!(format_watts(0.0), "0.0 W");
    }

    // -----------------------------------------------------------------------
    // No-Vec invariant (TASK-103 / TASK-105 / TASK-107 / TASK-108 /
    // TASK-109 lesson)
    // -----------------------------------------------------------------------

    /// Compile-time-ish assertion: `PowerFlowVM` does NOT carry a `Vec`
    /// field. We assert this structurally via `mem::size_of`. Five
    /// `Option<f32>` discriminants (8 bytes each = 40), one `String`
    /// (24), one `String` icon_id (24), `TilePlacement` (~16), and a
    /// `bool` (1 + padding) — total well under any plausible `Vec`-bearing
    /// struct.
    #[test]
    fn power_flow_vm_remains_lean() {
        // A Vec<f32> field would push this past the budget.
        assert!(
            std::mem::size_of::<PowerFlowVM>() <= 144,
            "PowerFlowVM has grown past the lean-shape budget; \
             did someone add a Vec field? actual = {}",
            std::mem::size_of::<PowerFlowVM>()
        );
    }
}
