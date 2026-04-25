#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')

if [[ -z "$FILE_PATH" || ! -f "$FILE_PATH" ]]; then
  exit 0
fi

PROJECT_DIR="${CLAUDE_PROJECT_DIR:-.}"
LINT_OUTPUT=""
LINT_FAILED=0

case "$FILE_PATH" in
  *.rs)
    if command -v cargo &>/dev/null; then
      LINT_OUTPUT=$(cargo clippy --message-format short 2>&1 | grep -F "$(basename "$FILE_PATH")" || true)
      [[ -n "$LINT_OUTPUT" ]] && LINT_FAILED=1
    fi
    ;;
esac

if (( LINT_FAILED )); then
  echo "$LINT_OUTPUT" >&2
  exit 2
fi

exit 0
