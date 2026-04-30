# Performance Contract (stub)

> **WIP — full contract in TASK-129.** This stub gives Wave 2 authors
> immediate access to the per-profile budget numbers without waiting for the
> full document. The full document — complete budget table, profile-wiring
> contract, bench-CI policy, and `performance-engineer` authority — is
> TASK-129 (Wave 2 closure gate; must merge before TASK-120a is delegated).
>
> Source-of-truth for the numbers below: `PERFORMANCE_AUDIT.md`
> § "Suggested Performance Targets" (lines 404-413).

## Per-Profile Budgets

| Metric                                  | OPI Zero 3       | Raspberry Pi 4   | Desktop          |
|-----------------------------------------|------------------|------------------|------------------|
| Initial dashboard build                 | < 250 ms         | < 500 ms         | < 1 s            |
| Incremental visible entity update (CPU) | < 5 ms           | < 5 ms           | < 5 ms           |
| UI model update                         | only changed rows| only changed rows| only changed rows|
| Steady idle CPU                         | near zero        | near zero        | near zero        |
| Burst (50 ev/s) — alloc per event       | sub-linear in N  | sub-linear in N  | sub-linear in N  |
| Reconnect snapshot diff                 | O(total + diff)  | O(total + diff)  | O(total + diff)  |

These are pragmatic targets for a low-power SBC dashboard. The "incremental
visible entity update" budget is **outside Slint render** — it counts only
hanui's own `apply_event` + `build_tiles` work.

## Bench CI policy (TASK-116)

Hot-path PRs (any change to `src/ha/live_store.rs`, `src/ha/client.rs`,
`src/ha/protocol.rs`, `src/ui/bridge.rs`, `benches/churn.rs`, or
`benches/baseline.json`) trigger the `hot-path-bench` job in
`.github/workflows/ci.yml`. The job runs four scenarios from
`benches/churn.rs`:

| Scenario                        | What it measures                          |
|---------------------------------|-------------------------------------------|
| `opi_profile_20w_2048e`         | `build_tiles` with total >> visible       |
| `rpi_profile_32w_4096e`         | `build_tiles` at RPI4 caps                |
| `bursty_one_visible_change`     | `apply_event` + `build_tiles`, hot loop   |
| `reconnect_diff_full_cap`       | reconnect-style: 4096 events then tiles   |

Each scenario uses **explicit warm-up iterations and sample count** (named
constants in `benches/churn.rs` — Risk #9 mitigation from
`docs/plans/2026-04-30-phase-7-performance.md`). The captured baseline lives
in `benches/baseline.json` with a runner annotation.

## Regression thresholds (enforced by performance-engineer agent)

From TASK-117 onwards (Wave 2), every hot-path PR must pass the
`performance-engineer` joint gate. The agent compares the PR's bench output
against `benches/baseline.json` and blocks on any of:

- p50 wall time per scenario regresses by **> 5 %**.
- p95 wall time per scenario regresses by **> 10 %**.
- Allocations per event grow by **+1 or more**.
- Total bytes allocated per scenario grows by **> 10 %**.

The bench-broken-blocker clause (the `performance-engineer` agent blocks any
hot-path PR while `benches/churn.rs` does not compile) is **exempted** for
TASK-116 only — TASK-116 *is* the restoration PR.

## Baseline regeneration

`benches/baseline.json` is a protected artifact. Regenerating it requires
the same approval chain as any other protected-path change:

- `devex-engineer` author
- `performance-engineer` review (representative scenarios + appropriate runner)
- `ci-gatekeeper` review
- human `infra-approved:` trailer

This gate exists because a baseline captured on a slower-than-typical runner
gives all subsequent PRs false headroom.

## See also

- `PERFORMANCE_AUDIT.md` — full 13-finding audit; this stub draws budgets from §"Suggested Performance Targets".
- `docs/plans/2026-04-30-phase-7-performance.md` — active plan for Phase 7.
- `docs/backlog/TASK-116.md` — this PR.
- `docs/backlog/TASK-129.md` — full document (Wave 2 closure gate).
- `.claude/agents/performance-engineer.md` — gate authority + bench-broken-blocker contract.
