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
 *   3. No file may be `failed` with zero failing assertions. That is a file
 *      that never LOADED — a bad import or a module-scope throw — and every
 *      test in it was skipped rather than run.
 *   4. The run must collect at least as many tests as the ledger records.
 *   5. The run must not have logged an unhandled error.
 *   6. Every failure must be listed in `test-known-failures.json`. Anything
 *      else is a regression and fails the job.
 *
 * Checks 3 and 4 exist because the first version of this wrapper counted failed
 * *assertions* only. A module-scope `throw` in `refinementEvidenceStatus.test.ts`
 * took 35 tests out of the run; the failure count stayed at exactly the ledger's
 * 23, so nothing fired and the gate printed PASSED while vitest itself exited 1
 * (task `pxtd`). The wrapper was strictly worse than a bare exit-code check for
 * that failure mode.
 *
 * A listed failure that passes is reported loudly but does NOT fail the job:
 * several of them are timing-sensitive, and a gate that goes red because a
 * slow test got lucky on a fast runner is a gate people learn to ignore.
 *
 * The decision logic lives in `vitest-ci-analyze.mjs` so it can be unit-tested;
 * this file is its I/O shell and runs those unit tests before every CI run.
 */

import { spawn } from "node:child_process";
import { readFileSync, existsSync, mkdtempSync, rmSync } from "node:fs";
import { readdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { analyzeRun } from "./vitest-ci-analyze.mjs";

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

/**
 * Run the gate's own unit tests before running the suite it gates.
 *
 * The gate is the last thing standing between a broken UI test and `main`, and
 * it shipped once with a hole straight through it. Wiring its self-test here
 * rather than into the workflow means the two can never drift apart: there is
 * no `pnpm test:ci` that skips it.
 */
async function runSelfTest() {
  const testFile = path.join(UI_ROOT, "scripts", "vitest-ci-analyze.test.mjs");
  const child = spawn(process.execPath, ["--test", testFile], {
    cwd: UI_ROOT,
    stdio: ["inherit", "pipe", "pipe"],
  });
  let out = "";
  child.stdout.on("data", (c) => (out += c));
  child.stderr.on("data", (c) => (out += c));
  const code = await new Promise((resolve) => child.on("close", resolve));
  if (code !== 0) {
    console.error(out);
    fail(
      `the gate's own unit tests (scripts/vitest-ci-analyze.test.mjs) exited ${code}.`,
      "  The gate cannot vouch for the suite while it cannot vouch for itself.",
    );
  }
  // node's TAP reporter writes `# pass N`; its spec reporter writes `ℹ pass N`.
  const summary = /^(?:#|ℹ) pass (\d+)/m.exec(out);
  console.log(
    `UI test gate self-test: ${summary ? summary[1] : "?"} check(s) passed.`,
  );
}

const passthrough = process.argv.slice(2);
const hasFilter = passthrough.some((arg) => !arg.startsWith("-"));

await runSelfTest();

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

const manifest = JSON.parse(readFileSync(MANIFEST_PATH, "utf8"));

const { problems, nowPassing, summary } = analyzeRun({
  report,
  manifest,
  uiRoot: UI_ROOT,
  onDiskTestFiles: hasFilter
    ? null
    : await collectTestFiles(path.join(UI_ROOT, "src")),
  captured,
  code,
});

console.log("");
console.log("UI test gate");
console.log(`  test files reported : ${summary.filesReported}`);
console.log(`  tests run           : ${summary.testsRun} (ledger records ${manifest.totalTests})`);
console.log(`  failures            : ${summary.failures}`);
console.log(
  `  known-red allowance : ${summary.knownTotal} in ${summary.knownFiles} file(s) (test-known-failures.json)`,
);

if (nowPassing.length > 0) {
  console.log("");
  console.log(
    `  ${nowPassing.length} test(s) listed in test-known-failures.json now PASS.`,
  );
  console.log("  Delete them from that file so the ledger keeps shrinking.");
  console.log("  This is a reminder, not a failure — see the module doc above.");
  for (const name of nowPassing) console.log(`    ${name}`);
}

if (problems.length > 0) fail(...problems);

console.log("");
console.log("UI test gate PASSED (no failures outside the recorded ledger).");
