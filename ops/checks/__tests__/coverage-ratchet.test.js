#!/usr/bin/env node
/**
 * coverage-ratchet.test.js — fixture-driven regression tests for coverage-ratchet.js
 *
 * Three cases required by TASK-024 AC:
 *   (a) unchanged  — baseline pct == current pct  -> exit 0
 *   (b) regressed  — baseline pct=80, current pct=70  -> exit 1, names file in error
 *   (c) deletion   — baseline has file, current does not -> exit 0 (deletion allowed)
 *
 * Run: node ops/checks/__tests__/coverage-ratchet.test.js
 * Exit 0 = all tests passed; Exit 1 = one or more tests failed.
 */

"use strict";

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execSync } = require("node:child_process");

// ── Helpers ──────────────────────────────────────────────────────────────────

let passed = 0;
let failed = 0;

function assert(label, condition, details) {
  if (condition) {
    console.log(`  PASS: ${label}`);
    passed++;
  } else {
    console.error(`  FAIL: ${label}${details ? ` — ${details}` : ""}`);
    failed++;
  }
}

/**
 * Write JSON fixture files to a temp dir and run coverage-ratchet.js.
 * Returns { exitCode, stdout, stderr }.
 */
function runRatchet(baselineFiles, currentFiles) {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "ratchet-test-"));

  // Baseline doc
  const baselineDoc = { _meta: {}, files: baselineFiles };
  const baselinePath = path.join(tmp, "baseline.json");
  fs.writeFileSync(baselinePath, JSON.stringify(baselineDoc));

  // Current doc (cargo llvm-cov --json shape)
  // currentFiles is: { "relative/path.rs": pctNumber }
  // We use relative paths directly as filenames so relativize() returns them unchanged
  // (when REPO_ROOT is set to an empty string the path.normalize strips nothing useful;
  //  instead we prefix filenames with REPO_ROOT so relativize() strips it correctly).
  const fakeRoot = tmp;
  const filesArray = Object.entries(currentFiles).map(([relPath, pct]) => ({
    filename: path.join(fakeRoot, relPath),
    summary: {
      lines: {
        count: 100,
        covered: Math.round(pct),
        percent: pct,
      },
    },
  }));
  const currentDoc = {
    data: [{ files: filesArray, totals: {} }],
    type: "llvm.coverage.json.export",
    version: "2.0.1",
  };
  const currentPath = path.join(tmp, "current.json");
  fs.writeFileSync(currentPath, JSON.stringify(currentDoc));

  const ratchetScript = path.resolve(__dirname, "../coverage-ratchet.js");

  let stdout = "";
  let stderr = "";
  let exitCode = 0;

  try {
    const result = execSync(
      `node ${ratchetScript}`,
      {
        env: {
          ...process.env,
          COVERAGE_BASELINE_PATH: baselinePath,
          COVERAGE_CURRENT_PATH: currentPath,
          REPO_ROOT: fakeRoot,
        },
        encoding: "utf8",
      },
    );
    stdout = result;
  } catch (e) {
    exitCode = e.status || 1;
    stdout = e.stdout || "";
    stderr = e.stderr || "";
  }

  // Clean up
  fs.rmSync(tmp, { recursive: true, force: true });

  return { exitCode, stdout, stderr };
}

// ── Test (a): unchanged — baseline pct == current pct -> exit 0 ──────────────

console.log("\nTest (a): unchanged pct -> exit 0");
{
  const baseline = {
    "src/foo.rs": { lines_total: 100, lines_covered: 80, pct: 80 },
  };
  const current = {
    "src/foo.rs": 80,
  };
  const { exitCode, stdout } = runRatchet(baseline, current);
  assert("exit code is 0", exitCode === 0, `got ${exitCode}`);
  assert(
    "stdout contains OK",
    stdout.includes("OK"),
    `stdout: ${stdout.trim()}`,
  );
}

// ── Test (b): regressed — baseline pct=80, current pct=70 -> exit 1 ─────────

console.log("\nTest (b): regressed pct (80 -> 70) -> exit 1 naming the file");
{
  const baseline = {
    "src/bar.rs": { lines_total: 100, lines_covered: 80, pct: 80 },
  };
  const current = {
    "src/bar.rs": 70,
  };
  const { exitCode, stderr, stdout } = runRatchet(baseline, current);
  assert("exit code is 1", exitCode === 1, `got ${exitCode}`);
  const combined = stdout + stderr;
  assert(
    "error names the regressed file",
    combined.includes("src/bar.rs"),
    `output: ${combined.trim()}`,
  );
  assert(
    "error mentions baseline pct",
    combined.includes("80"),
    `output: ${combined.trim()}`,
  );
  assert(
    "error mentions current pct",
    combined.includes("70"),
    `output: ${combined.trim()}`,
  );
}

// ── Test (c): deletion — baseline has file, current doesn't -> exit 0 ────────

console.log("\nTest (c): file absent from current (deletion allowed) -> exit 0");
{
  const baseline = {
    "src/deleted.rs": { lines_total: 50, lines_covered: 40, pct: 80 },
  };
  // current is empty — no entries at all
  const current = {};
  const { exitCode, stdout } = runRatchet(baseline, current);
  assert("exit code is 0", exitCode === 0, `got ${exitCode}`);
  assert(
    "stdout contains OK",
    stdout.includes("OK"),
    `stdout: ${stdout.trim()}`,
  );
}

// ── Summary ──────────────────────────────────────────────────────────────────

console.log(`\ncoverage-ratchet tests: ${passed} passed, ${failed} failed`);
if (failed > 0) {
  process.exit(1);
}
