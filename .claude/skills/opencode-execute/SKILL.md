---
name: opencode-execute
description: Dispatch a backlog task to the opencode executor via the oc-execute.sh dispatcher. Use when the project lead routes a low-risk task to the cost-saving delegation tier instead of a Claude subagent.
---

# opencode-execute

Dispatches a single backlog task (`docs/backlog/TASK-NNN.md`) to the opencode executor, which uses a non-Claude model to write code in a retry loop, then commits, pushes, and opens a PR.

## Canonical invocation

```bash
bash ops/opencode/oc-execute.sh TASK-NNN [--type feat|fix|chore|docs|test|refactor]
```

The dispatcher handles everything: lock acquisition, eligibility precheck, opencode invocation, scope check, commit, push, PR creation, frontmatter writeback (`commit:` + `pr:` into the TASK file), CI watch, and telemetry append.

## When to invoke (routing decision)

Use opencode-execute when ALL of the following are true:
- Task owner is NOT `security-engineer`, `ci-gatekeeper`, or `billing-engineer`
- `files_allowlist` does NOT overlap any protected path (.claude/agents/**,CLAUDE.md,ops/checks/**,ops/pre-receive/**,docs/waivers.yaml,coverage/baseline.json,.github/workflows/**, `CLAUDE.md`, `.claude/agents/**`, `.claude/skills/**`)
- All `depends_on` tasks are `done`
- Task is low-risk-of-judgment — straightforward code changes, not cross-cutting architecture
- Task is NOT a stateful frontend interaction (focus management, conditional rendering trees)

## When NOT to invoke (use Claude subagent instead)

- Task touches protected paths or is owned by a restricted agent
- Task requires architectural judgment or cross-cutting changes
- Task involves the database schema or shared types as primary changes
- Task involves stateful frontend interactions
- Previous opencode attempt on the same task failed

## Eligibility precheck (fail-fast, no opencode call)

The dispatcher refuses the task before invoking opencode if:
1. Owner is in `{security-engineer, ci-gatekeeper, billing-engineer}`
2. `files_allowlist` intersects protected paths
3. Any `depends_on` task is not `done`
4. Any path-like backticked symbol in `acceptance_criteria` is not greppable under the project source directories

Ineligible tasks exit with code 2 and `status: ineligible` in telemetry.

## Failure modes

| Exit code | Meaning | Recovery |
|---|---|---|
| 0 | Success — PR open, CI watched | Proceed to gate review |
| 1 | Invocation error (bad args, missing tools) | Fix invocation |
| 2 | Eligibility precheck failed | Route to Claude subagent |
| 3 | Scope violation (files outside allowlist) | Route to Claude subagent |
| 4 | Opencode execution failed after max iterations | Route to Claude subagent |
| 5 | CI failed or gatekeeper approval not green | Investigate CI failure |
| 75 | Lock contention (another run in progress) | Wait and retry |
| 124 | 30-min wall-clock cap exceeded | Route to Claude subagent |

## After execution

Run the gate review bundler:

```bash
bash ops/opencode/oc-gate.sh TASK-NNN          # Phase 1: review bundle
bash ops/opencode/oc-gate.sh TASK-NNN --record pass   # Phase 2: record outcome
```

## Lock file

The dispatcher acquires an exclusive lock via `flock`. Only one `oc-execute.sh` run can proceed at a time. Ad-hoc `opencode run` invocations with write intent must check for this lock and refuse if absent — use the `opencode-review` skill for review-only work.

## Telemetry

Each run appends one JSON line to `ops/telemetry/opencode-spend.jsonl`:
```json
{"task_id":"TASK-NNN","model":"opencode/default","iterations":N,"wall_time_seconds":N,"status":"..."}
```

The gate script's `--record` phase later appends `cto_review_wall_time_seconds` and `cto_review_outcome` to the same line.

## Bot account

Automated CI checks and PR comments are posted as %%BOT_USERNAME%%.
