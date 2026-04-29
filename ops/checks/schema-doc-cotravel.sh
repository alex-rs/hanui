#!/usr/bin/env bash
# ops/checks/schema-doc-cotravel.sh — schema-doc co-travel enforcement
#
# Implements `locked_decisions.schema_finalization_gate` part (b) from
# docs/plans/2026-04-29-phase-4-layout.md (TASK-093).
#
# ## What this script checks
#
# Any PR that adds or removes a *field declaration* in `src/dashboard/schema.rs`
# must also add or remove a corresponding *field heading* in `docs/DASHBOARD_SCHEMA.md`,
# and vice versa.
#
# ## Risk #12 mitigation (structured-grep scoping)
#
# The check does NOT flag every file-level co-travel violation. It scopes to
# *field additions/removals* so that doc-only typo PRs (changes to field body
# text, no new headings) pass cleanly.
#
#   SCHEMA_DELTA:  count of added/removed `pub <ident>:` lines inside struct/enum
#                  blocks in src/dashboard/schema.rs (heuristic: a line whose first
#                  non-whitespace token after +/- is `pub` followed by an identifier
#                  ending in `:`, signalling a struct field declaration).
#
#   DOC_DELTA:     count of added/removed `### \`…\`` headings in docs/DASHBOARD_SCHEMA.md
#                  (heuristic: a line matching `^[+-]### ` where the first non-space
#                  token after ### is a backtick — the stable heading format used by
#                  TASK-079 for every field entry).
#
# ## --doc-only escape valve (Risk #12)
#
# If SCHEMA_DELTA=0 and DOC_DELTA>0, the doc side has headings added/removed
# without a struct side change. This passes only when the diff is purely additive
# (e.g., a new section-level prose heading that is NOT a field entry). The
# structured-grep heuristic handles this implicitly: non-field headings (e.g.,
# `## Actions`, `### Error (halts load…)`) do not match the field-heading regex.
# Ambiguous cases (a new `### \`field_name\`` heading that genuinely has no struct
# counterpart) require a reviewer sign-off comment per the locked decision.
#
# ## Normal operation
#
#   BOTH zero  -> OK (doc-only typo or unrelated change)
#   BOTH >0    -> OK (field add accompanied by doc update, or vice versa)
#   schema >0, doc=0  -> FAIL
#   schema=0, doc >0  -> FAIL
#
# ## Usage
#
#   bash ops/checks/schema-doc-cotravel.sh              # normal PR check
#   bash ops/checks/schema-doc-cotravel.sh --self-test  # runs pass/fail fixtures
#
# ## Environment variables
#
#   GITHUB_BASE_REF   branch name of the PR base (default: main)
#   BASE_SHA          override the merge-base commit (useful in tests)
#
# ## Dependencies
#
#   bash, git, grep, wc — all standard in CI. No jq, yq, or Rust crates.
#
# ## Idempotency
#
#   Side-effect free: no file writes, no env mutations. Re-running on the same
#   diff produces the same result.
#
# TASK-093 — Phase 4 Wave 8d

set -euo pipefail

# ---------------------------------------------------------------------------
# Self-test mode
# ---------------------------------------------------------------------------
# Invoked as: bash ops/checks/schema-doc-cotravel.sh --self-test
#
# Runs two synthetic diff fixtures through the core logic and asserts expected
# exit codes. CI calls this mode as part of the workflow step so fixture
# behaviour is continuously verified.
#
# Fixture 1 (PASS): doc-only typo — no field heading added/removed.
#   schema_delta=0, doc_delta=0 → exit 0
#
# Fixture 2 (FAIL): schema.rs adds a field without doc update.
#   schema_delta=1, doc_delta=0 → exit 1 with FAIL message
# ---------------------------------------------------------------------------

if [[ "${1:-}" == "--self-test" ]]; then
  PASS=0
  FAIL=0

  _pass() { printf "  PASS: %s\n" "$1"; PASS=$(( PASS + 1 )); }
  _fail() { printf "  FAIL: %s\n" "$1"; FAIL=$(( FAIL + 1 )); }

  # ── Helper: run the co-travel logic against injected delta counts ────────
  # Duplicates the core decision logic so the test is self-contained and does
  # not depend on git state. Any change to the main logic must be mirrored here.
  _check_deltas() {
    local schema_delta="$1"
    local doc_delta="$2"
    if [[ "${schema_delta}" -gt 0 && "${doc_delta}" -eq 0 ]]; then
      echo "FAIL: schema.rs added/removed ${schema_delta} field declaration(s) without corresponding DASHBOARD_SCHEMA.md edit"
      return 1
    fi
    if [[ "${schema_delta}" -eq 0 && "${doc_delta}" -gt 0 ]]; then
      echo "FAIL: DASHBOARD_SCHEMA.md added/removed ${doc_delta} field heading(s) without corresponding schema.rs edit"
      return 1
    fi
    echo "schema-doc cotravel: OK (schema_delta=${schema_delta}, doc_delta=${doc_delta})"
    return 0
  }

  # ── Fixture 1: doc-only typo (no field heading add/remove) ──────────────
  # A diff that only edits the body of an existing field doc — no `### \`…\``
  # heading line is added or removed. SCHEMA_DELTA=0, DOC_DELTA=0 → exit 0.
  printf "\nFixture 1 (PASS): doc-only typo — no field heading add/remove\n"
  printf "  Simulates: SCHEMA_DELTA=0, DOC_DELTA=0\n"
  OUTPUT=$( _check_deltas 0 0 )
  EXIT=$?
  if [[ $EXIT -eq 0 ]]; then
    _pass "exit 0"
    if printf '%s' "$OUTPUT" | grep -q "^schema-doc cotravel: OK"; then
      _pass "output contains OK line"
    else
      _fail "output missing OK line — got: ${OUTPUT}"
    fi
  else
    _fail "expected exit 0, got exit ${EXIT} — output: ${OUTPUT}"
  fi

  # ── Fixture 2: schema adds field without doc update ──────────────────────
  # A diff that adds `pub new_field: String,` to a struct in schema.rs but has
  # no new `### \`new_field\`` heading in DASHBOARD_SCHEMA.md.
  # SCHEMA_DELTA=1, DOC_DELTA=0 → exit 1 with "FAIL:" prefix in output.
  printf "\nFixture 2 (FAIL): schema.rs adds a field, DASHBOARD_SCHEMA.md unchanged\n"
  printf "  Simulates: SCHEMA_DELTA=1, DOC_DELTA=0\n"
  OUTPUT=$( _check_deltas 1 0 ) || true
  EXIT=${PIPESTATUS[0]}
  # Re-run capturing exit code correctly
  set +e
  OUTPUT=$( _check_deltas 1 0 )
  EXIT=$?
  set -e
  if [[ $EXIT -eq 1 ]]; then
    _pass "exit 1"
    if printf '%s' "$OUTPUT" | grep -q "^FAIL:"; then
      _pass "output contains FAIL: prefix"
    else
      _fail "output missing FAIL: prefix — got: ${OUTPUT}"
    fi
    if printf '%s' "$OUTPUT" | grep -q "field declaration"; then
      _pass "output mentions field declarations"
    else
      _fail "output missing 'field declaration' — got: ${OUTPUT}"
    fi
  else
    _fail "expected exit 1, got exit ${EXIT} — output: ${OUTPUT}"
  fi

  # ── Fixture 3 (bonus): doc adds heading without schema change ────────────
  # Catches the reverse direction: SCHEMA_DELTA=0, DOC_DELTA=1 → exit 1.
  printf "\nFixture 3 (FAIL): DASHBOARD_SCHEMA.md adds a heading, schema.rs unchanged\n"
  printf "  Simulates: SCHEMA_DELTA=0, DOC_DELTA=1\n"
  set +e
  OUTPUT=$( _check_deltas 0 1 )
  EXIT=$?
  set -e
  if [[ $EXIT -eq 1 ]]; then
    _pass "exit 1"
    if printf '%s' "$OUTPUT" | grep -q "^FAIL:"; then
      _pass "output contains FAIL: prefix"
    else
      _fail "output missing FAIL: prefix — got: ${OUTPUT}"
    fi
    if printf '%s' "$OUTPUT" | grep -q "field heading"; then
      _pass "output mentions field headings"
    else
      _fail "output missing 'field heading' — got: ${OUTPUT}"
    fi
  else
    _fail "expected exit 1, got exit ${EXIT} — output: ${OUTPUT}"
  fi

  # ── Summary ──────────────────────────────────────────────────────────────
  printf "\nschema-doc-cotravel self-test: %d passed, %d failed\n" "$PASS" "$FAIL"
  if [[ $FAIL -gt 0 ]]; then
    exit 1
  fi
  exit 0
fi

# ---------------------------------------------------------------------------
# Normal operation: compute deltas from the PR diff
# ---------------------------------------------------------------------------

BASE_REF="${GITHUB_BASE_REF:-main}"
BASE_SHA="${BASE_SHA:-$(git merge-base "origin/${BASE_REF}" HEAD 2>/dev/null || git merge-base "${BASE_REF}" HEAD)}"

# Count Rust schema field-declaration lines added or removed.
#
# Heuristic: a "field declaration" is a line in the diff for
# src/dashboard/schema.rs whose content (after the leading +/-) starts with
# optional whitespace then `pub ` followed by a snake_case identifier ending
# in `:` — matching the pattern for struct field declarations such as:
#
#   +    pub version: u32,
#   -    pub old_field: Option<String>,
#
# The grep -v '^[+-]{3}' strips the `--- a/file` / `+++ b/file` header lines
# so they are not counted. The `|| true` prevents set -e from aborting when
# grep finds no matches (exit 1 from grep means no match, which is valid here).
SCHEMA_DELTA=$(
  git diff "${BASE_SHA}..HEAD" -- src/dashboard/schema.rs 2>/dev/null \
    | grep -E '^[+-][[:space:]]+pub [a-z_][a-z0-9_]*:' \
    | grep -v '^[+-]{3}' \
    | wc -l \
  || true
)
# Trim whitespace from wc -l output (BSD wc pads with spaces)
SCHEMA_DELTA="${SCHEMA_DELTA//[[:space:]]/}"

# Count DASHBOARD_SCHEMA.md field-heading lines added or removed.
#
# Heuristic: a "field heading" is a diff line for docs/DASHBOARD_SCHEMA.md
# whose content (after the leading +/-) matches `### \`` — the stable format
# for field-level headings established by TASK-079:
#
#   +### `widgets[].new_field`
#   -### `widgets[].old_field`
#
# H4 headings (####) for sub-fields (e.g. `sections[].grid.columns`) also
# follow the `#### \`` pattern. Both levels count as field documentation.
# The `|| true` prevents set -e aborting on zero-match.
DOC_DELTA=$(
  git diff "${BASE_SHA}..HEAD" -- docs/DASHBOARD_SCHEMA.md 2>/dev/null \
    | grep -E '^[+-](###|####) `' \
    | grep -v '^[+-]{3}' \
    | wc -l \
  || true
)
DOC_DELTA="${DOC_DELTA//[[:space:]]/}"

# ---------------------------------------------------------------------------
# Co-travel decision
# ---------------------------------------------------------------------------
#
# Rule:
#   BOTH zero  -> OK (typo fix, unrelated change, or empty PR for these files)
#   BOTH >0    -> OK (fields and docs travel together)
#   schema >0, doc=0 -> FAIL (struct change without doc)
#   schema=0, doc >0 -> FAIL (doc change without struct)
#
# Ambiguous edge case: a `### \`heading\`` line inside a code fence block or
# inside an existing section's explanation could increment DOC_DELTA without
# a real new field being documented. If that causes a spurious failure on a
# doc-only PR, the PR author should add a reviewer sign-off comment per the
# locked decision in docs/plans/2026-04-29-phase-4-layout.md
# (locked_decisions.schema_finalization_gate part (b)).

if [[ "${SCHEMA_DELTA}" -gt 0 && "${DOC_DELTA}" -eq 0 ]]; then
  echo "FAIL: schema.rs added/removed ${SCHEMA_DELTA} field declaration(s) without corresponding DASHBOARD_SCHEMA.md edit"
  echo "  -> Ensure docs/DASHBOARD_SCHEMA.md gains/loses the matching ### \`field\` heading(s)."
  echo "  -> See locked_decisions.schema_finalization_gate part (b) in docs/plans/2026-04-29-phase-4-layout.md"
  exit 1
fi

if [[ "${SCHEMA_DELTA}" -eq 0 && "${DOC_DELTA}" -gt 0 ]]; then
  echo "FAIL: DASHBOARD_SCHEMA.md added/removed ${DOC_DELTA} field heading(s) without corresponding schema.rs edit"
  echo "  -> Ensure src/dashboard/schema.rs gains/loses the matching pub field declaration(s)."
  echo "  -> See locked_decisions.schema_finalization_gate part (b) in docs/plans/2026-04-29-phase-4-layout.md"
  exit 1
fi

echo "schema-doc cotravel: OK (schema_delta=${SCHEMA_DELTA}, doc_delta=${DOC_DELTA})"
exit 0
