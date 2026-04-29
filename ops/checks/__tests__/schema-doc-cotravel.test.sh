#!/usr/bin/env bash
# ops/checks/__tests__/schema-doc-cotravel.test.sh
#
# Standalone fixture-based regression tests for ops/checks/schema-doc-cotravel.sh.
#
# Tests the script by feeding synthetic git diff output via a temp-dir
# git repository so the real git diff path is exercised end-to-end.
#
# Two required fixtures per TASK-093 acceptance criteria:
#   Fixture A (PASS): doc-only typo — no field heading add/remove → exit 0
#   Fixture B (FAIL): schema.rs adds a field, DASHBOARD_SCHEMA.md unchanged → exit 1
#
# Run: bash ops/checks/__tests__/schema-doc-cotravel.test.sh
# Exit 0 = all tests passed; Exit 1 = one or more tests failed.

set -euo pipefail

PASS=0
FAIL=0
SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/schema-doc-cotravel.sh"

if [[ ! -f "$SCRIPT" ]]; then
  printf "ERROR: script not found at %s\n" "$SCRIPT"
  exit 1
fi

_pass() { printf "  PASS: %s\n" "$1"; PASS=$(( PASS + 1 )); }
_fail() { printf "  FAIL: %s\n" "$1"; FAIL=$(( FAIL + 1 )); }

# ---------------------------------------------------------------------------
# Helper: create an ephemeral git repo, make a commit, amend HEAD with the
# desired delta, run the script, return the exit code and output.
# ---------------------------------------------------------------------------
# Arguments:
#   $1 — schema.rs content to ADD in the commit (empty string = no change)
#   $2 — DASHBOARD_SCHEMA.md content to ADD in the commit (empty string = no change)
#
# The helper creates:
#   - base commit: empty files in src/dashboard/schema.rs and docs/DASHBOARD_SCHEMA.md
#   - HEAD commit: applies the additions
#
# BASE_SHA is exported so the script uses the base commit as the merge-base.
# ---------------------------------------------------------------------------
run_fixture() {
  local schema_add="$1"
  local doc_add="$2"

  local tmp
  tmp=$(mktemp -d)

  # Initialize a throw-away git repo
  git -C "$tmp" init -q
  git -C "$tmp" config user.email "test@example.com"
  git -C "$tmp" config user.name "Test"

  # Create directory structure
  mkdir -p "$tmp/src/dashboard"
  mkdir -p "$tmp/docs"

  # Base commit: empty files
  touch "$tmp/src/dashboard/schema.rs"
  touch "$tmp/docs/DASHBOARD_SCHEMA.md"
  git -C "$tmp" add -A
  git -C "$tmp" commit -q -m "base"

  # Record the base SHA so schema-doc-cotravel.sh uses it as the merge-base
  local base_sha
  base_sha=$(git -C "$tmp" rev-parse HEAD)

  # HEAD commit: apply the desired delta
  if [[ -n "$schema_add" ]]; then
    printf '%s\n' "$schema_add" >> "$tmp/src/dashboard/schema.rs"
  fi
  if [[ -n "$doc_add" ]]; then
    printf '%s\n' "$doc_add" >> "$tmp/docs/DASHBOARD_SCHEMA.md"
  fi
  git -C "$tmp" add -A
  git -C "$tmp" commit -q -m "delta" || {
    # Nothing to commit means both files unchanged — make an empty delta commit
    git -C "$tmp" commit -q --allow-empty -m "delta"
  }

  # Run the script from inside the temp repo, with BASE_SHA overridden
  local output exit_code
  set +e
  output=$(cd "$tmp" && BASE_SHA="$base_sha" bash "$SCRIPT" 2>&1)
  exit_code=$?
  set -e

  rm -rf "$tmp"

  printf '%s\n' "$exit_code" "$output"
}

# ---------------------------------------------------------------------------
# Fixture A: doc-only typo — no field heading, no struct field → PASS (exit 0)
#
# The diff modifies only the body text of an existing field section in
# DASHBOARD_SCHEMA.md without adding/removing any `### \`…\`` heading. No
# field declarations are changed in schema.rs. Both deltas are 0 → OK.
# ---------------------------------------------------------------------------
printf "\nFixture A (PASS): doc-only typo — body text change, no heading add/remove\n"
{
  # schema.rs: add a plain comment (not a field declaration)
  SCHEMA_ADD="// Updated internal comment — not a pub field"
  # DASHBOARD_SCHEMA.md: add body text not matching the heading pattern
  DOC_ADD="Updated description paragraph — no new ### heading."

  result=$(run_fixture "$SCHEMA_ADD" "$DOC_ADD")
  exit_code=$(printf '%s' "$result" | head -1)
  output=$(printf '%s' "$result" | tail -n +2)

  if [[ "$exit_code" -eq 0 ]]; then
    _pass "exit 0"
  else
    _fail "expected exit 0, got ${exit_code} — output: ${output}"
  fi
  if printf '%s' "$output" | grep -q "schema-doc cotravel: OK"; then
    _pass "output contains OK line"
  else
    _fail "output missing OK line — got: ${output}"
  fi
}

# ---------------------------------------------------------------------------
# Fixture B: schema.rs adds a field, DASHBOARD_SCHEMA.md unchanged → FAIL (exit 1)
#
# The diff adds `    pub new_sensor: Option<String>,` to a struct in schema.rs
# but no `### \`new_sensor\`` heading in DASHBOARD_SCHEMA.md.
# SCHEMA_DELTA=1, DOC_DELTA=0 → exit 1 with "FAIL:" message.
# ---------------------------------------------------------------------------
printf "\nFixture B (FAIL): schema.rs adds a field without doc update\n"
{
  SCHEMA_ADD="    pub new_sensor: Option<String>,"
  DOC_ADD=""  # No doc change

  result=$(run_fixture "$SCHEMA_ADD" "$DOC_ADD")
  exit_code=$(printf '%s' "$result" | head -1)
  output=$(printf '%s' "$result" | tail -n +2)

  if [[ "$exit_code" -eq 1 ]]; then
    _pass "exit 1"
  else
    _fail "expected exit 1, got ${exit_code} — output: ${output}"
  fi
  if printf '%s' "$output" | grep -q "^FAIL:"; then
    _pass "output contains FAIL: prefix"
  else
    _fail "output missing FAIL: prefix — got: ${output}"
  fi
  if printf '%s' "$output" | grep -q "field declaration"; then
    _pass "output mentions field declarations"
  else
    _fail "output missing 'field declaration' — got: ${output}"
  fi
}

# ---------------------------------------------------------------------------
# Fixture C: both schema and doc change → PASS (exit 0)
#
# Co-travel satisfied: a new field in schema.rs accompanies a new heading in
# DASHBOARD_SCHEMA.md. Both deltas >0 → OK.
# ---------------------------------------------------------------------------
printf "\nFixture C (PASS): field added to schema.rs AND heading added to DASHBOARD_SCHEMA.md\n"
{
  SCHEMA_ADD="    pub new_sensor: Option<String>,"
  DOC_ADD="### \`new_sensor\`

**Type**: \`Option<String>\`
**Required**: no"

  result=$(run_fixture "$SCHEMA_ADD" "$DOC_ADD")
  exit_code=$(printf '%s' "$result" | head -1)
  output=$(printf '%s' "$result" | tail -n +2)

  if [[ "$exit_code" -eq 0 ]]; then
    _pass "exit 0"
  else
    _fail "expected exit 0, got ${exit_code} — output: ${output}"
  fi
  if printf '%s' "$output" | grep -q "schema-doc cotravel: OK"; then
    _pass "output contains OK line"
  else
    _fail "output missing OK line — got: ${output}"
  fi
}

# ---------------------------------------------------------------------------
# Fixture D: doc adds heading, schema.rs unchanged → FAIL (exit 1)
#
# Reverse direction: SCHEMA_DELTA=0, DOC_DELTA=1 → exit 1.
# ---------------------------------------------------------------------------
printf "\nFixture D (FAIL): DASHBOARD_SCHEMA.md adds a heading, schema.rs unchanged\n"
{
  SCHEMA_ADD=""
  DOC_ADD="### \`undocumented_field\`

**Type**: \`String\`
**Required**: yes"

  result=$(run_fixture "$SCHEMA_ADD" "$DOC_ADD")
  exit_code=$(printf '%s' "$result" | head -1)
  output=$(printf '%s' "$result" | tail -n +2)

  if [[ "$exit_code" -eq 1 ]]; then
    _pass "exit 1"
  else
    _fail "expected exit 1, got ${exit_code} — output: ${output}"
  fi
  if printf '%s' "$output" | grep -q "^FAIL:"; then
    _pass "output contains FAIL: prefix"
  else
    _fail "output missing FAIL: prefix — got: ${output}"
  fi
  if printf '%s' "$output" | grep -q "field heading"; then
    _pass "output mentions field headings"
  else
    _fail "output missing 'field heading' — got: ${output}"
  fi
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
printf "\nschema-doc-cotravel fixture tests: %d passed, %d failed\n" "$PASS" "$FAIL"
if [[ $FAIL -gt 0 ]]; then
  exit 1
fi
exit 0
