#!/usr/bin/env node
/**
 * CI entry point for the UI test suite (`pnpm test:ci`).
 *
 * Why this wrapper exists
 * ----------------------------------------------------------------------------
 * `pnpm test` used to exit **0** while 23 tests failed, so the `UI Frontend`
 * job's test step could not fail on a broken UI test and every UI regression
 * merged green (task `73h8`).
 *
 * The cause was not a reporter setting: one test file
 * (`src/hooks/useSigmaGraph.test.tsx`) drove an unbounded React
 * mount/teardown loop and killed its vitest worker with
 * `ERR_WORKER_OUT_OF_MEMORY`. In a multi-worker full-suite run that left the
 * vitest main process awaiting a result that never arrived, with nothing else
 * keeping the event loop alive — so node drained and exited **0**, before the
 * summary was printed and before vitest ever assigned `process.exitCode = 1`.
 * A silent exit 0 with 135 of 136 files reported.
 *
 * The loop is fixed, and vitest's own exit code is correct again. But "the
 * runner died quietly and node exited 0" is a failure mode no exit-code check
 * can see, so this wrapper does not trust the exit code alone. It requires
 * positive evidence that the run finished:
 *
 *   1. vitest must have written a machine-readable JSON report. A truncated
 *      run never reaches the reporter, so a missing report is a hard failure.
 *   2. Every `*.test.ts(x)` file on disk must appear in that report. A file
 *      whose worker died is missing from it — the exact 73h8 signature.
 *   3. The run must not have logged an unhandled error.
 *   4. Every failure must be listed in `test-known-failures.json`. Anything
 *      else is a regression and fails the job.
 *
 * A listed failure that passes is reported loudly but does NOT fail the job:
 * several of them are timing-sensitive, and a gate that goes red because a
 * slow test got lucky on a fast runner is a gate people learn to ignore.
 */

import { spawn } from "node:child_process";
import { readFileSync, existsSync, mkdtempSync, rmSync } from "node:fs";
import { readdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const UI_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const MANIFEST_PATH = path.join(UI_ROOT, "test-known-failures.json");
const TEST_FILE_RE = /\.test\.tsx?$/;

/** Walk `src` for the test files vitest is expected to report on. */
async function collectTestFiles(dir, acc = []) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "node_modules" || entry.name.startsWith(".")) continue;
      await collectTestFiles(full, acc);
    } else if (TEST_FILE_RE.test(entry.name)) {
      acc.push(path.relative(UI_ROOT, full));
    }
  }
  return acc;
}

/** Run vitest, streaming its output through while also capturing it. */
function runVitest(args) {
  return new Promise((resolve) => {
    const child = spawn(
      process.execPath,
      [path.join(UI_ROOT, "node_modules", "vitest", "vitest.mjs"), ...args],
      { cwd: UI_ROOT, stdio: ["inherit", "pipe", "pipe"] },
    );
    let captured = "";
    child.stdout.on("data", (chunk) => {
      captured += chunk;
      process.stdout.write(chunk);
    });
    child.stderr.on("data", (chunk) => {
      captured += chunk;
      process.stderr.write(chunk);
    });
    child.on("close", (code, signal) => resolve({ code, signal, captured }));
  });
}

function fail(...lines) {
  console.error("");
  console.error("UI test gate FAILED");
  for (const line of lines) console.error(`  ${line}`);
  console.error("");
  process.exit(1);
}

const passthrough = process.argv.slice(2);
const hasFilter = passthrough.some((arg) => !arg.startsWith("-"));

const reportDir = mkdtempSync(path.join(tmpdir(), "djinn-ui-vitest-"));
const reportPath = path.join(reportDir, "results.json");

const { code, signal, captured } = await runVitest([
  "run",
  "--pool=vmThreads",
  "--reporter=default",
  "--reporter=json",
  `--outputFile.json=${reportPath}`,
  ...passthrough,
]);

// (1) No report at all means the run never reached its reporter. That is what
//     the OOM'd worker did: exit 0, no summary, no report.
if (!existsSync(reportPath)) {
  rmSync(reportDir, { recursive: true, force: true });
  fail(
    `vitest exited with code ${code}${signal ? ` (signal ${signal})` : ""} but wrote no JSON report.`,
    "The run was truncated — a worker most likely died (out of memory) and the",
    "main process drained its event loop before reporting. Treating as failure.",
  );
}

let report;
try {
  report = JSON.parse(readFileSync(reportPath, "utf8"));
} catch (err) {
  fail(`vitest's JSON report at ${reportPath} is unparseable: ${err.message}`);
} finally {
  rmSync(reportDir, { recursive: true, force: true });
}

const reported = new Map();
for (const file of report.testResults ?? []) {
  const rel = path.relative(UI_ROOT, file.name);
  reported.set(
    rel,
    file.assertionResults
      .filter((a) => a.status === "failed")
      .map((a) => a.fullName),
  );
}

const problems = [];

// (2) Every test file on disk must have reported. A file that is silently
//     absent is a dead worker, not a passing suite.
if (!hasFilter) {
  const onDisk = await collectTestFiles(path.join(UI_ROOT, "src"));
  const missing = onDisk.filter((f) => !reported.has(f));
  if (missing.length > 0) {
    problems.push(
      `${missing.length} test file(s) on disk never reported a result:`,
      ...missing.map((f) => `    ${f}`),
      "  A missing file means its worker died mid-run. Run it on its own to see why.",
    );
  }
}

// (3) Unhandled errors set vitest's exit code but never appear in the JSON
//     report, so read them off the console output.
if (/Unhandled Error/.test(captured)) {
  problems.push(
    "vitest reported an unhandled error during the run (see output above).",
  );
}

// (4) Compare the failure set against the recorded debt ledger.
const manifest = JSON.parse(readFileSync(MANIFEST_PATH, "utf8"));
const known = manifest.knownFailures ?? {};
const knownTotal = Object.values(known).flat().length;

const unexpected = [];
for (const [file, failures] of reported) {
  for (const name of failures) {
    if (!(known[file] ?? []).includes(name)) unexpected.push(`${file} > ${name}`);
  }
}

const nowPassing = [];
for (const [file, names] of Object.entries(known)) {
  const failures = reported.get(file);
  if (failures === undefined) continue; // covered by the missing-file check
  for (const name of names) {
    if (!failures.includes(name)) nowPassing.push(`${file} > ${name}`);
  }
}

console.log("");
console.log("UI test gate");
console.log(`  test files reported : ${reported.size}`);
console.log(`  tests run           : ${report.numTotalTests}`);
console.log(`  failures            : ${report.numFailedTests}`);
console.log(
  `  known-red allowance : ${knownTotal} in ${Object.keys(known).length} file(s) (test-known-failures.json)`,
);

if (nowPassing.length > 0) {
  console.log("");
  console.log(
    `  ${nowPassing.length} test(s) listed in test-known-failures.json now PASS.`,
  );
  console.log("  Delete them from that file so the ledger keeps shrinking:");
  for (const name of nowPassing) console.log(`    ${name}`);
}

if (unexpected.length > 0) {
  problems.push(
    `${unexpected.length} test(s) failed that are not in test-known-failures.json:`,
    ...unexpected.map((n) => `    ${n}`),
    "  These are regressions. Fix them — do not add them to the ledger.",
  );
}

// A non-zero exit with no explanation the checks above could name means vitest
// failed for a reason this wrapper does not model. Never swallow that.
if (code !== 0 && problems.length === 0 && report.numFailedTests !== knownTotal) {
  problems.push(
    `vitest exited ${code} but the report shows ${report.numFailedTests} failure(s)`,
    `against a ${knownTotal}-failure allowance. Unexplained — treating as failure.`,
  );
}

if (problems.length > 0) fail(...problems);

console.log("");
console.log("UI test gate PASSED (no failures outside the recorded ledger).");
