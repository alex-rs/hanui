#!/usr/bin/env bash
# checks/forbidden-tokens.sh — forbidden-token scanner
#
# Scans the files passed as arguments for tokens that require a waiver.
# Called by lefthook pre-commit (on staged files) and by the pre-receive
# hook (on the full diff). The pre-receive invocation is the actual control;
# this script is shared between both callers for consistency.
#
# Scanned extensions: .ts .tsx .js .jsx .mts .cts .mjs .cjs .go
# Other file types (shell, YAML, Markdown) are skipped — the forbidden tokens
# are only meaningful in TypeScript, JavaScript, and Go source code.
#
# Forbidden tokens (any of these in a source file requires a docs/waivers.yaml
# entry AND an ISSUE-### reference on the same or immediately adjacent line):
#   .only          — test isolation escape hatch
#   .skip          — test isolation escape hatch
#   xit            — Jasmine/Jest skip alias
#   fdescribe      — Jasmine/Jest focus alias
#   @ts-ignore     — TypeScript type suppression
#   @ts-expect-error — TypeScript type suppression (stricter variant)
#   eslint-disable — ESLint suppression (any form)
#   //nolint       — Go linter suppression
#
# Exit codes:
#   0 — no violations found
#   1 — violations found (commit blocked)
#
# Usage: forbidden-tokens.sh [file ...]
# If no files are passed, exit 0 (nothing to check).

set -euo pipefail

if [[ $# -eq 0 ]]; then
  exit 0
fi

# Source file extensions that are subject to the forbidden-token check.
is_source_file() {
  local file="$1"
  case "$file" in
    *.ts|*.tsx|*.js|*.jsx|*.mts|*.cts|*.mjs|*.cjs|*.go)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

# Patterns that are forbidden without a waiver.
# Each pattern is a grep -E extended regex.
PATTERNS=(
  '\.only\b'
  '\.skip\b'
  '\bxit\b'
  '\bfdescribe\b'
  '@ts-ignore'
  '@ts-expect-error'
  'eslint-disable'
  '//nolint'
)

FOUND=0

for FILE in "$@"; do
  # Skip non-existent paths (deleted files passed by lefthook).
  [[ -f "$FILE" ]] || continue

  # Skip files that are not TS/JS/Go source.
  is_source_file "$FILE" || continue

  # Skip binary files.
  if file --brief --mime-encoding "$FILE" 2>/dev/null | grep -q 'binary'; then
    continue
  fi

  for PATTERN in "${PATTERNS[@]}"; do
    # grep -n prints line numbers; -E for extended regex.
    MATCHES=$(grep -En "$PATTERN" "$FILE" 2>/dev/null || true)
    if [[ -z "$MATCHES" ]]; then
      continue
    fi

    while IFS= read -r MATCH; do
      [[ -z "$MATCH" ]] && continue
      LINE_NO=$(printf '%s' "$MATCH" | cut -d: -f1)
      LINE_CONTENT=$(printf '%s' "$MATCH" | cut -d: -f2-)

      # Check for a waiver reference: ISSUE-### on the same line OR on the
      # immediately preceding or following line.
      PREV_LINE=$(( LINE_NO > 1 ? LINE_NO - 1 : 1 ))
      NEXT_LINE=$(( LINE_NO + 1 ))

      CONTEXT=$(sed -n "${PREV_LINE},${NEXT_LINE}p" "$FILE" 2>/dev/null || true)

      if printf '%s' "$CONTEXT" | grep -qE 'ISSUE-[0-9]+'; then
        # Waiver reference present — still need a docs/waivers.yaml entry,
        # but that check runs server-side. Allow the commit to proceed locally.
        continue
      fi

      printf '[forbidden-tokens] %s:%s: forbidden token matched pattern '\''%s'\''\n' \
        "$FILE" "$LINE_NO" "$PATTERN"
      printf '  %s\n' "$LINE_CONTENT"
      printf '  --> Add ISSUE-### on adjacent line AND a docs/waivers.yaml entry to waive.\n'
      FOUND=1
    done <<< "$MATCHES"
  done
done

if [[ $FOUND -ne 0 ]]; then
  printf '\n[forbidden-tokens] Commit blocked. Fix violations above or add waivers.\n'
  printf '[forbidden-tokens] See your governance docs for the waiver process.\n'
  exit 1
fi

exit 0
