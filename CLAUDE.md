# hanui

HA native UI

## CTO role

The main Claude Code session plays the CTO role. There is no `.claude/agents/cto.md` — CTO is the orchestrator, not a subagent. Full contract in `docs/cto.md`.

Session-start load order: `CLAUDE.md` → `docs/cto.md` → every non-archived `docs/backlog/TASK-NNN.md` → the single plan file in `docs/plans/` with `status: active`. Heavy docs load on demand, not automatically.

CTO has two delegation tiers:
- **Claude subagent** (primary) — full Agent-tool session with domain-expert context. Required for: security-engineer/ci-gatekeeper-owned tasks, tasks touching protected paths, cross-cutting architecture, stateful interactions.
- **Opencode executor** (cost-saving) — `bash ops/opencode/oc-execute.sh TASK-NNN`. Eligible only when: owner is not restricted, `files_allowlist` does not overlap protected paths, all `depends_on` are done, and path-like acceptance-criteria symbols are greppable. See `docs/cto.md` § Routing decision tree.

CTO rules (full list in `docs/cto.md`):
- Delegates via Agent tool using the outbound contract; never authors code a subagent owns.
- Verifies completion via `ops/checks/verify-task-done.sh TASK-NNN`, not subagent self-report.
- Substantive requests (≥3 tasks or multi-subagent) require a founder-approved plan file before delegation fires.
- Cannot post `ci-gatekeeper/approved`, cannot post `infra-approved:`, cannot override `security-engineer` veto, cannot manufacture tasks.
- Invokes `ci-gatekeeper` only via a distinct Agent-tool session, never in-conversation.

## Repo layout

<!-- Fill in your project's directory structure below -->
- `src/` or `apps/` — application code
- `packages/` — shared packages (if monorepo)
- `docs/` — runbooks, architecture notes
- `docs/cto.md` — CTO role definition
- `docs/backlog/` — one file per task (`TASK-NNN.md`); archive under `docs/backlog/archive/YYYY-MM.md`
- `docs/plans/` — founder-approved plan files (`YYYY-MM-DD-<slug>.md`)
- `docs/waivers.yaml` — single-file ledger of every lint/test/mutation waiver (mandatory expiry, `ci-gatekeeper`-approved)
- `ops/pre-receive/` — server-side enforcement hooks
- `ops/checks/` — shared check scripts (forbidden-tokens, lockfile-drift, test-deletion, `verify-task-done.sh`, etc.)
- `coverage/baseline.json` — per-file coverage baseline, ratchet target
- `knip.baseline.json` — dead-code baseline (rust projects)
- `.claude/agents/` — subagent definitions (protected path)

## Tech stack

- **Language**: rust
- **Package manager**: cargo
- **Database**: none

<!-- Add your framework, queue, observability, secrets, and other stack choices here -->

## Coding conventions

- Strict mode for rust — no `any` types, no unsafe casts without an explanatory comment.
- Linter enforced in CI; client-side hooks are a latency optimization, not a control. Same rules run server-side in pre-receive.
- Commit style: Conventional Commits (`feat:`, `fix:`, `chore:`, etc.).
- Branch naming: `task/NNN-short-slug` (e.g. `task/013-self-review-protocol`). Always include a 2–4 word kebab-case slug — bare task numbers are not allowed.
- Every new environment variable added to the shared env schema and validated at startup.
- **Dead code is deleted, not commented out or flagged with TODO.** Git history is the archive. If you think it might be needed later, it won't be — delete it.

## Testing expectations

- Unit tests for every helper, counter, and state-machine transition.
- Integration tests for the core happy paths and their principal failure modes (bad credentials, budget exceeded, external 5xx, webhook replay, network timeout).
- **E2E split by tag**: `@smoke` runs on every PR (<5 min budget), `@full` runs nightly + pre-release. Every user-visible flow must cover ≥1 error path.
- **Per-file coverage ratchet** against `coverage/baseline.json`. Global floor is a floor, not a goal; per-file is the real control.
- **Mutation testing** on changed files; PR fails if changed-file mutation score < baseline - 2%.
- **Determinism**: `TZ=UTC`, seeded RNG, randomized test order, unit suite runs twice per CI with different seeds to surface order-dependence.

## Security rules (non-negotiable)

- **Never log secrets, tokens, or full request/response bodies.** Trace IDs + sanitized metadata only.
- **Never commit secrets.** `.env.local` gitignored; secrets in a vault or CI secrets store.
- **Every secret access writes an audit log row** (no plaintext in the row).
- Dependency changes require lockfile review and SBOM update.

## When to invoke which subagent

<!-- BEGIN routing -->
- CI pipeline / GitHub Actions / lefthook / lint configs → `ci-gatekeeper`
- CI config / dev environment / coverage baselines → `devex-engineer`
- Plan drafting / task breakdown → `task-planner`
- Threat modeling / secrets / dependency hygiene → `security-engineer`
- API routes / database / authentication / background jobs → `backend-engineer`
- other cloud / Docker / deployment / server provisioning / TLS → `infra-engineer`
<!-- END routing -->

## Cross-agent escalation matrix

<!-- BEGIN escalation -->
| Trigger | Primary owner | Must also review | Has veto |
|---|---|---|---|
| CI pipeline change | `devex-engineer` | `ci-gatekeeper` | `ci-gatekeeper` |
| Protected path change | `ci-gatekeeper` | `security-engineer` | `ci-gatekeeper` + human |
| New secret type or key-handling path | `security-engineer` | — | `security-engineer` |
| Waiver addition or renewal | author agent | `ci-gatekeeper` | `ci-gatekeeper` |
| Required `approved` check posted on PR | `ci-gatekeeper` (sole authority) | — | `ci-gatekeeper` |
| Retry circuit-breaker freeze | `ci-gatekeeper` | `devex-engineer` or human | `ci-gatekeeper` |
| Abuse response posture change | `infra-engineer` | `security-engineer` | `security-engineer` |
| Deployment pipeline change | `infra-engineer` | `devex-engineer` | — |
<!-- END escalation -->

## Agent-driven development guardrails (non-negotiable)

Because every commit in this repo is authored by an agent, every rule below is mechanically enforced. Full spec: `docs/dev/ci-pipeline.md` (Section I is the justification).

- **Feature is not done until full CI is green AND `ci-gatekeeper` has posted the `approved` required check.** No self-approval.
- **Never `--no-verify`**. Client hooks are latency; pre-receive is the control. Bypassing a hook doesn't skip the rule — it just delays failure to push time.
- **Never modify `.claude/agents/**,CLAUDE.md,ops/checks/**,ops/pre-receive/**,docs/waivers.yaml,coverage/baseline.json,.github/workflows/**`, CI workflows, lint configs, `CLAUDE.md`, or `.claude/agents/**` without both `ci-gatekeeper` approval AND a human `infra-approved:` trailer.** An agent must not be able to modify the rules it is measured by.
- **Forbidden tokens** (blocked at pre-receive): .only,.skip,xit,fdescribe,#[allow(dead_code)] — unless accompanied by `ISSUE-###` reference on the same/adjacent line AND a `docs/waivers.yaml` entry.
- **Every waiver has a mandatory expiry** (max 90 days). Expired waivers fail CI until renewed or fixed. 14-day default for `kind: flaky-test`.
- **Test integrity is enforced**: test-deletion detector, assertion-count ratchet, per-file coverage ratchet, mutation-score delta on changed files. Deleting a failing test instead of fixing it is a blocking violation.
- **Retry circuit breaker**: 2 failed attempts per check-signature per PR. 3rd failure freezes the PR until `devex-engineer` or a human unblocks.
- **`ci-gatekeeper` never authors product code.** If a gatekeeper task would require editing application or package source, it's the wrong agent.

## Self-review-before-commit protocol

Before every `git commit` in a subagent session, the agent invokes `opencode-review` passing three inputs: the task_id, a one-paragraph summary of what changed and why, and a structural diff consisting of file paths plus changed line ranges only — NOT full patch content (no `+`/`-` content lines).

Structural-diff format example (line ranges only, no content):
```
src/lib/foo.ts: +12..28, -5..7
packages/shared/env.ts: +3..4
```
This tells the reviewer which hunks to ask about; the reviewer cannot read the actual diff content, which is intentional — it forces the executor to defend each hunk verbally.

(a) Before every `git commit`, the agent invokes `opencode-review` passing [task_id, one-paragraph context, structural diff = file paths + changed line ranges only, NOT full patch content].
(b) If the review returns any blocker, the agent addresses it and re-requests review.
(c) Maximum 3 review iterations per task.
(d) If iteration 3 still returns blockers, the agent does NOT commit — it writes back `status: failed` with `blocked_reason` naming the unresolved blockers and escalates to CTO via the standard inbound contract.

## Forbidden patterns

Mechanical enforcement catches most; this list documents intent so reviewers can spot sophisticated evasions.
- Tautological assertions (`expect(x).toBeDefined()` on a value the test just created; `expect.anything()` without co-assertions).
- Mock bloat replacing integration coverage — prefer fixtures + ephemeral stack.
- Snapshot-only tests without behavioral assertions.
- Broad lint-disable at file top to silence one line.
- "TODO: fix later" in place of a waiver entry.
- Rewording tests to pass without changing the production code they covered.
- Adding `@flaky` waiver for a test that is failing for real reasons (not flaky).

## Local dev setup

```bash
make dev       # boots local stack
make hooks     # installs client-side git hooks
make test      # runs full test suite
```

Copy `.env.example` to `.env.local` and fill in credentials. All Git work is against the local git server; pushes hit pre-receive enforcement immediately. CI runs on the same compose stack — `docker-compose.ci.yml` mirrors dev exactly.
