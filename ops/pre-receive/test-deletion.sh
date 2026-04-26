#!/usr/bin/env bash
# pre-receive/test-deletion.sh — test-deletion detector.
#
# Blocks pushes where test files are deleted or have net line-count decrease
# without a corresponding docs/waivers.yaml entry (kind: test-removal-approved)
# OR a commit trailer `test-removal-approved:`.
#
# Test file patterns:
#   *.test.ts, *.test.tsx, *.test.js, *.test.jsx
#   *.spec.ts, *.spec.tsx, *.spec.js, *.spec.jsx
#   **/__tests__/**
#   *_test.go
#   tests/*.rs, tests/**/*.rs    (Rust integration test crates — standard cargo layout)
#   *_test.rs                    (Rust sibling test files)
#   Any deleted .rs file whose pre-deletion content contained a #[test] attribute
#
# Context passed by pre-receive dispatcher:
#   HOOK_COMMITS_FILE  — one commit sha per line
#   HOOK_FILES_FILE    — one changed filename per line
#   GIT_DIR            — set by the git server to the bare repo path
#
# Exit 0 = clean. Exit 1 = violation (push blocked).
#
# Regression scenario for Rust test deletion (manual reproduction):
#   In a local checkout, create then delete a Rust test file:
#
#     echo '#[cfg(test)] mod t { #[test] fn works() {} }' > tests/del_me.rs
#     git add tests/del_me.rs && git commit -m 'add rust test'
#     git rm tests/del_me.rs && git commit -m 'remove rust test'
#     git push origin HEAD
#
#   Expected: push blocked — "[test-deletion] Test file(s) deleted without
#   test-removal-approved trailer" because tests/del_me.rs matches
#   */tests/*.rs AND its pre-deletion content contained #[test].

set -uo pipefail

EMPTY_TREE="4b825dc642cb6eb9a060e54bf8d69288fbee4904"

git_files_in_commit() {
  local sha="$1"
  if git cat-file -e "${sha}^" 2>/dev/null; then
    git diff-tree --no-commit-id -r --name-status "$sha" 2>/dev/null || true
  else
    git diff-tree --no-commit-id -r --name-status "${EMPTY_TREE}" "$sha" 2>/dev/null || true
  fi
}

git_numstat_in_commit() {
  local sha="$1"
  if git cat-file -e "${sha}^" 2>/dev/null; then
    git diff-tree --no-commit-id -r --numstat "$sha" 2>/dev/null || true
  else
    git diff-tree --no-commit-id -r --numstat "${EMPTY_TREE}" "$sha" 2>/dev/null || true
  fi
}

is_test_file() {
  local f="$1"
  case "$f" in
    # ── TypeScript / JavaScript ──────────────────────────────────────────────
    *.test.ts|*.test.tsx|*.test.js|*.test.jsx) return 0 ;;
    *.spec.ts|*.spec.tsx|*.spec.js|*.spec.jsx) return 0 ;;
    */__tests__/*)                              return 0 ;;
    # ── Go ───────────────────────────────────────────────────────────────────
    *_test.go)                                  return 0 ;;
    # ── Rust — integration test crates (standard cargo layout) ───────────────
    # Shell case globs do not support **: cover up to three directory levels.
    # Deeper paths are caught by the rs_file_had_test_attribute second-pass check
    # below, which inspects pre-deletion content for #[test] annotations.
    */tests/*.rs)                               return 0 ;;
    */tests/*/*.rs)                             return 0 ;;
    */tests/*/*/*.rs)                           return 0 ;;
    # ── Rust — sibling test files (less common but valid pattern) ────────────
    *_test.rs)                                  return 0 ;;
    *) return 1 ;;
  esac
}

# rs_file_had_test_attribute checks whether a deleted .rs file's pre-deletion
# content (at SHA^) contained a #[test] attribute.  Returns 0 if it did.
# This is the backstop for .rs files that do not match a filename pattern above
# (e.g. src/ha/store.rs with an inline #[cfg(test)] mod tests block, or
# deeply-nested tests/**/**/*.rs files beyond the case glob depth limit).
rs_file_had_test_attribute() {
  local sha="$1"
  local filepath="$2"
  # git cat-file -e guards against the root-commit case: if SHA has no parent
  # (first commit in the repo), git show SHA^:filepath would fail. In that
  # edge case we return 1 (false) and let the pattern-based check above handle
  # known test-file patterns. Inline-test-only files added in the root commit
  # that do not match a pattern are not flagged — an acceptable gap given that
  # production repos rarely delete files in the same push that created them.
  if ! git cat-file -e "${sha}^" 2>/dev/null; then
    return 1
  fi
  # Match either #[test] (individual test function) or #[cfg(test)] (test module).
  # Both indicate test code. A file with only #[cfg(test)] and no #[test] can still
  # contain helper functions, fixtures, or submodules used exclusively by tests.
  git show "${sha}^:${filepath}" 2>/dev/null | grep -qE '#\[(test|cfg\(test\))\]'
}

COMMITS_FILE="${HOOK_COMMITS_FILE:-/dev/null}"
FILES_FILE="${HOOK_FILES_FILE:-/dev/null}"

TOTAL_ADDED=0
TOTAL_REMOVED=0
DELETED_TEST_FILES=()

while IFS= read -r SHA; do
  [[ -z "$SHA" ]] && continue

  # Check for deleted test files.
  # The test-removal-approved trailer must appear on the commit that
  # performs the deletion — a trailer on a different commit in the same
  # push is not sufficient.
  COMMIT_MSG_FOR_SHA=$(git log -1 --format='%B' "$SHA" 2>/dev/null || true)
  COMMIT_APPROVES_REMOVAL=false
  if printf '%s' "$COMMIT_MSG_FOR_SHA" | grep -qiE '^test-removal-approved:'; then
    COMMIT_APPROVES_REMOVAL=true
  fi

  while IFS=$'\t' read -r STATUS FILENAME; do
    [[ -z "$FILENAME" ]] && continue
    if [[ "$STATUS" == D ]]; then
      # Pass 1: pattern-based test file match (TS/JS/Go/Rust layout patterns)
      if is_test_file "$FILENAME"; then
        if ! $COMMIT_APPROVES_REMOVAL; then
          DELETED_TEST_FILES+=("${SHA:0:8}:${FILENAME}")
        fi
        continue
      fi
      # Pass 2 (Rust only): deleted .rs file that previously contained #[test]
      # This catches inline test modules (src/foo.rs with #[cfg(test)] blocks)
      # that do not match a file-name pattern but are still tests.
      if [[ "$FILENAME" == *.rs ]]; then
        if rs_file_had_test_attribute "$SHA" "$FILENAME"; then
          if ! $COMMIT_APPROVES_REMOVAL; then
            DELETED_TEST_FILES+=("${SHA:0:8}:${FILENAME} (contained #[test])")
          fi
        fi
      fi
    fi
  done < <(git_files_in_commit "$SHA")

  # Count added/removed lines in test files.
  while IFS=$'\t' read -r ADDED REMOVED FILE; do
    [[ -z "$FILE" ]] && continue
    is_test_file "$FILE" || continue
    [[ "$ADDED" =~ ^[0-9]+$ ]]   && TOTAL_ADDED=$(( TOTAL_ADDED + ADDED ))
    [[ "$REMOVED" =~ ^[0-9]+$ ]] && TOTAL_REMOVED=$(( TOTAL_REMOVED + REMOVED ))
  done < <(git_numstat_in_commit "$SHA")
done < "$COMMITS_FILE"

FOUND=0

if [[ ${#DELETED_TEST_FILES[@]} -gt 0 ]]; then
  echo "[test-deletion] Test file(s) deleted without test-removal-approved trailer:" >&2
  for F in "${DELETED_TEST_FILES[@]}"; do
    echo "  deleted: $F" >&2
  done
  FOUND=1
fi

if [[ $TOTAL_REMOVED -gt $TOTAL_ADDED ]]; then
  NET=$(( TOTAL_REMOVED - TOTAL_ADDED ))
  echo "[test-deletion] Net test line reduction: ${NET} lines removed across test files." >&2
  echo "  Add 'test-removal-approved: <reason>' trailer + docs/waivers.yaml entry." >&2
  FOUND=1
fi

if [[ $FOUND -ne 0 ]]; then
  # Check if docs/waivers.yaml in the latest commit has a waiver AND was pushed.
  LATEST_SHA=""
  while IFS= read -r SHA; do
    [[ -n "$SHA" ]] && LATEST_SHA="$SHA"
  done < "$COMMITS_FILE"

  if [[ -n "$LATEST_SHA" ]]; then
    WAIVERS=$(git show "${LATEST_SHA}:docs/waivers.yaml" 2>/dev/null || true)
    if printf '%s' "$WAIVERS" | grep -qE '^[[:space:]]*kind:[[:space:]]*test-removal-approved'; then
      WAIVERS_IN_PUSH=false
      while IFS= read -r FILE; do
        if [[ "$FILE" == "docs/waivers.yaml" ]]; then
          WAIVERS_IN_PUSH=true
          break
        fi
      done < "$FILES_FILE"
      if $WAIVERS_IN_PUSH; then
        echo "[test-deletion] Waiver found in docs/waivers.yaml — allowing test removal." >&2
        exit 0
      fi
    fi
  fi

  echo "" >&2
  echo "[test-deletion] Push blocked." >&2
  echo "  Add 'test-removal-approved: <reason>' trailer AND a docs/waivers.yaml entry." >&2
  exit 1
fi

exit 0
