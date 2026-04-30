# Performance Contract

This document is the canonical performance reference for hanui. It specifies
the per-profile budgets that all PR authors must respect, the rules governing
`DeviceProfile` wiring, the bench-CI policy that enforces regressions
mechanically, and the `performance-engineer` agent's gating authority.

Source authority: `PERFORMANCE_AUDIT.md` (2026-04-30, 12/13 findings VALID),
`docs/plans/2026-04-30-phase-7-performance.md`, and
`.claude/agents/performance-engineer.md`.

Audiences:
- **PR authors** — what budgets must my code respect?
- **`performance-engineer` agent** — what is the gating contract?
- **Future CTOs** — what was decided and why?

---

## Per-Profile Budget Table

Source: `PERFORMANCE_AUDIT.md` §"Suggested Performance Targets" (lines 404-413).

These are pragmatic targets for a low-power SBC dashboard. All three profiles
have corresponding constants in `src/dashboard/profiles.rs`:
`PROFILE_OPI_ZERO3`, `PROFILE_RPI4`, `PROFILE_DESKTOP`.

| Metric | OPI Zero 3 | Raspberry Pi 4 | Desktop |
|---|---|---|---|
| Initial dashboard build | < 250 ms | < 500 ms | not budgeted |
| Incremental visible entity update (CPU outside Slint render) | < 5 ms | < 5 ms | not budgeted |
| Full store clone in per-event path | not allowed | not allowed | not allowed |
| Steady idle CPU (HA quiet, no animations) | near zero | near zero | near zero |
| Burst — 50 ev/s allocation growth | not proportional to entity count | not proportional to entity count | not proportional to entity count |
| Reconnect snapshot diff complexity | O(total_entities + changed_entities) | O(total_entities + changed_entities) | O(total_entities + changed_entities) |
| UI model update on state event | changed rows only via `set_row_data` | changed rows only via `set_row_data` | changed rows only via `set_row_data` |

Profile field values (entity caps, animation caps, Tokio workers, frame-period
budgets, RSS caps) are the authoritative numbers; read them directly from the
constant declarations in `src/dashboard/profiles.rs`. The table above records
the audit-derived latency targets that the constants must collectively satisfy.

The OPI and RPI budgets are the binding constraints. The desktop profile is
a development convenience, not a performance gate.

---

## Profile-Wiring Contract

Source: F4 findings (TASK-120a, TASK-120b). Active from Wave 3 of Phase 7.

### Rule 1 — Injection, not global reads

Every consumer of a profile-bound limit MUST receive `&DeviceProfile` from the
runtime. Consumers include:

- `WsClient::new` — WebSocket caps (`ws_payload_cap`, `snapshot_buffer_events`)
- `assets::icons::init` — image size limit (`max_image_px`)
- `LiveBridge::spawn` — widget cap (`max_widgets_per_view`), animation limits
- `validate()` callsites — entity and widget cap validation
- `tests/smoke/sbc_cpu.rs` — CPU budget assertion (`cpu_smoke_budget_pct`)

No consumer may read a profile from a module-level global or from a hard-coded
literal. The selected profile is chosen once in `src/lib.rs::run` (post-F4)
and threaded through to every consumer from there.

### Rule 2 — `PROFILE_DESKTOP` usage restriction

`PROFILE_DESKTOP` may appear ONLY in:

a. Explicit desktop-profile test cases (inside `#[cfg(test)]` blocks that name
   the desktop profile as the test subject).
b. Named default-fallback paths in non-runtime code — for example,
   `select_profile(None)` returning `&PROFILE_DESKTOP` as the conservative
   fallback is acceptable because the fallback is named and documented.
c. The `profiles.rs` declaration itself (`pub const PROFILE_DESKTOP: DeviceProfile = ...`).

`PROFILE_DESKTOP` MUST NOT appear in:

- `src/lib.rs::run` or `src/lib.rs::run_with_live_store` as the runtime's
  active profile after TASK-120a merges
- `src/ha/client.rs::new` argument wiring after TASK-120b merges
- `src/ui/bridge.rs::LiveBridge::spawn` argument wiring after TASK-120b merges
- `src/dashboard/validate.rs::validate` argument wiring after TASK-120b merges
- `tests/smoke/sbc_cpu.rs` budget reads after TASK-122 merges

Enforcement grep (run after TASK-120b + TASK-122 merge):

```
grep -nF 'PROFILE_DESKTOP' \
  src/lib.rs src/ha/client.rs src/ui/bridge.rs \
  src/dashboard/validate.rs tests/smoke/sbc_cpu.rs
```

Any match outside the explicit allowlist above is a `performance-engineer`
gate blocker (gate criterion #4 in `.claude/agents/performance-engineer.md`).

### Rule 3 — New `DeviceProfile` fields

When a new field is added to `DeviceProfile`, every profile constant
(`PROFILE_RPI4`, `PROFILE_OPI_ZERO3`, `PROFILE_DESKTOP`) MUST set it
explicitly with a value derived from that profile's overall sizing target.
Falling back to a `Default` implementation is a gate blocker (criterion #10).
Defaults hide tuning intent and silently let SBC profiles inherit desktop
values.

---

## Bench-CI Policy

Source: F9 finding (TASK-116). Active from Wave 1 of Phase 7 (TASK-116 merge).

### Trigger

`benches/churn.rs` runs on every PR that touches any of:

- `src/ha/live_store.rs`
- `src/ha/client.rs`
- `src/ha/protocol.rs`
- `src/ui/bridge.rs`

This is wired into `.github/workflows/ci.yml` by TASK-116. PRs that do not
touch those files are not subject to the churn bench gate.

### Scenarios

Four canonical bench scenarios are defined in `benches/churn.rs`:

1. 20 widgets / 2,048 entities / OPI Zero 3 profile
2. 32 widgets / 4,096 entities / Raspberry Pi 4 profile
3. Bursty updates — 1 changed visible entity at 50 ev/s
4. Reconnect diff — many changed entities

Each scenario configures explicit Criterion warm-up time and sample count to
reduce noise on shared CI runners (Risk #9 in the Phase 7 plan).

### Regression thresholds

A PR is blocked by `performance-engineer` if any scenario regresses beyond:

| Metric | Threshold |
|---|---|
| p50 wall time | +5% |
| p95 wall time | +10% |
| Allocation count per event | +1 (zero tolerance; per-event paths must stay non-allocating) |
| Total allocation bytes | +10% |

All comparisons are against `benches/baseline.json`, a tracked artifact in the
repository. The baseline is captured per scenario; the runner type is documented
in the baseline file's header comment.

### Bench-broken-blocker clause

If `benches/churn.rs` does not compile, `performance-engineer` MUST block any
PR touching the four hot-path files listed above with the comment:

> bench broken; coordinate with `devex-engineer` to restore `benches/churn.rs`
> (TASK-115 F9) before changing hot-path code.

This applies unconditionally. A change cannot claim "no regression" when the
measuring instrument is absent. The clause was active from TASK-117 (Wave 2)
forwards; TASK-116 itself is explicitly exempted because it is the PR that
restores the bench.

### Baseline regeneration chain

Any regeneration of `benches/baseline.json` requires ALL of the following,
in order:

1. `devex-engineer` authors the updated baseline
2. `performance-engineer` reviews for scenario representativeness and runner
   consistency
3. `ci-gatekeeper` reviews the protected-path change
4. Human posts an `infra-approved:` trailer on the commit

No agent may regenerate the baseline unilaterally. The `performance-engineer`
agent is explicitly forbidden from authoring `benches/baseline.json` updates
(see `.claude/agents/performance-engineer.md` §Escalation).

---

## `performance-engineer` Agent Authority

Source: `.claude/agents/performance-engineer.md` (canonical; grep there for
the full text of any clause summarized below).

### Gating contract

Hot-path PRs require BOTH `ci-gatekeeper/approved` AND `performance/approved`
on the merge SHA before merging. Neither gate alone is sufficient. A PR with
only `ci-gatekeeper/approved` is NOT ready to merge if it touches a hot-path
file.

`performance-engineer` is the SOLE authority for posting `performance/approved`
or `performance/blocked` via the Statuses API. No other agent or human may post
that check on behalf of the agent.

### Hot-path file list

The canonical hot-path file list is in `.claude/agents/performance-engineer.md`
§"Hot-path files (the gating surface)". Do not duplicate it here; always grep
that file for the authoritative list. At time of writing the surface covers
`src/ha/live_store.rs`, `src/ha/client.rs`, `src/ha/protocol.rs`,
`src/ui/bridge.rs`, `src/lib.rs::run`, `src/dashboard/profiles.rs`,
`src/dashboard/loader.rs`, animated `.slint` widgets, `benches/churn.rs`,
`benches/dispatcher.rs`, and `tests/smoke/sbc_cpu.rs`.

### Bench-broken-blocker clause

While `benches/churn.rs` does not compile, `performance-engineer` MUST block
every hot-path PR with a standard message naming the missing prerequisite
(TASK-115 F9). This is not punitive — it is the only way to prevent
unverifiable regression claims. The agent cannot approve a hot-path change
when the measuring instrument is absent. The clause is fully active from
Wave 2 (TASK-117) onwards.

### Ten gate criteria (summaries)

For the full text of each criterion, grep
`.claude/agents/performance-engineer.md` by criterion number.

1. **No clone-on-write in per-event paths** — `apply_event` and any per-event
   handler must not clone the full entity map, tile list, or subscription set.
   Exception: reconnect/bootstrap one-shot snapshot.
2. **No hot-path full-store walk** — `build_tiles` and any 12.5 Hz path must
   not call `store.for_each`; use an atomic counter updated on `apply_event`
   instead.
3. **Bench result regression detection** — compare against `benches/baseline.json`
   for all four scenarios; block on thresholds listed in Section 3 above.
4. **`DeviceProfile` wiring compliance** — `PROFILE_DESKTOP` must not appear
   in runtime wiring paths; enforce by grep (see Section 2 above).
5. **SBC CPU budget conformance** — `tests/smoke/sbc_cpu.rs` must use the
   selected profile's `cpu_smoke_budget_pct`, not `PROFILE_DESKTOP`'s.
6. **Animation budget enforcement** — new animations in `.slint` files must be
   conditional on state, gated by `AnimationBudget.at-capacity` or
   profile-stepped; always-on animations are an immediate blocker.
7. **WebSocket subscription scope (post-F7)** — `WsClient` must use filtered
   subscriptions for three event types, not `event_type: None`.
8. **Loader-validator wiring (post-F5)** — `loader.rs` must call
   `validate(&dashboard, profile)` before returning; the `vec![]` stub is
   a blocker.
9. **JSON deserialization clone audit** — `InboundMsg::deserialize` must not
   clone the full `serde_json::Value` for known message variants.
10. **Profile field additions** — every new `DeviceProfile` field must be
    set explicitly in all three profile constants; no implicit `Default`.

### Anti-collusion specifics

`performance-engineer` will not post `performance/approved` while a
`ci-gatekeeper` block is open on the same PR. It will not approve a SHA it
did not review. It will escalate to `security-engineer` any PR that attempts
to disable a perf check via a waiver-like mechanism. PRs that raise regression
thresholds without `devex-engineer` joint review are blocked.

Full anti-collusion rules: `.claude/agents/performance-engineer.md`
§"Anti-collusion specifics".
