/**
 * Unit tests for the UI test gate's decision logic (`vitest-ci-analyze.mjs`).
 *
 * Run by `node --test`, which `vitest-ci.mjs` invokes before it spawns vitest —
 * so these run on every CI execution of `pnpm test:ci`. They are deliberately
 * NOT vitest tests: the gate must not depend on the suite it is gating.
 *
 * Every case below corresponds to a failure mode that reached `main` or was
 * demonstrated against a shipped version of the gate.
 */

import assert from "node:assert/strict";
import test from "node:test";

import { analyzeRun } from "./vitest-ci-analyze.mjs";

const UI_ROOT = "/repo/ui";

/** A file entry shaped like vitest's JSON reporter output. */
function file(name, assertions = []) {
  return {
    name: `${UI_ROOT}/${name}`,
    status: assertions.some((a) => a.status === "failed") ? "failed" : "passed",
    assertionResults: assertions,
  };
}

const pass = (fullName) => ({ fullName, status: "passed" });
const failed = (fullName) => ({ fullName, status: "failed" });

/** A file that threw at module scope: failed, no assertions at all. */
function collectionFailure(name, message = "Error: boom") {
  return {
    name: `${UI_ROOT}/${name}`,
    status: "failed",
    message,
    assertionResults: [],
  };
}

const MANIFEST = {
  totalTests: 10,
  knownFailures: {
    "src/a.test.ts": ["a > known red"],
  },
};

/** A clean run: 10 tests, the single ledger failure, vitest exit 1. */
function greenRun() {
  return {
    report: {
      numTotalTests: 10,
      numFailedTests: 1,
      testResults: [
        file("src/a.test.ts", [failed("a > known red"), pass("a > ok")]),
        file("src/b.test.ts", [
          pass("b > 1"),
          pass("b > 2"),
          pass("b > 3"),
          pass("b > 4"),
          pass("b > 5"),
          pass("b > 6"),
          pass("b > 7"),
          pass("b > 8"),
        ]),
      ],
    },
    manifest: MANIFEST,
    uiRoot: UI_ROOT,
    onDiskTestFiles: ["src/a.test.ts", "src/b.test.ts"],
    captured: "",
    code: 1,
  };
}

test("baseline: the ledger's failures alone keep the gate green", () => {
  const { problems, nowPassing } = analyzeRun(greenRun());
  assert.deepEqual(problems, []);
  assert.deepEqual(nowPassing, []);
});

test("a file that fails at collection fails the gate", () => {
  // The pxtd defect: a module-scope throw reports `failed` with zero
  // assertions, so the failure count never moves off the ledger's allowance.
  const run = greenRun();
  run.report.testResults.push(
    collectionFailure("src/c.test.ts", "Error: adv: collection-time failure probe"),
  );
  run.onDiskTestFiles.push("src/c.test.ts");

  const { problems } = analyzeRun(run);
  assert.equal(run.report.numFailedTests, 1, "failure count is unchanged");
  assert.ok(
    problems.some((p) => p.includes("failed without a single failing test")),
    `expected a collection-error problem, got: ${JSON.stringify(problems)}`,
  );
  assert.ok(
    problems.some((p) => p.includes("src/c.test.ts")),
    "the offending file must be named",
  );
  assert.ok(
    problems.some((p) => p.includes("adv: collection-time failure probe")),
    "the throw's message must be surfaced",
  );
});

test("a collection failure is caught even when its file is also missing-checked off", () => {
  // A filtered run makes no whole-suite claim, so the missing-file and
  // total-count checks are off. The collection check must still bite.
  const run = greenRun();
  run.onDiskTestFiles = null;
  run.report.testResults.push(collectionFailure("src/c.test.ts"));

  const { problems } = analyzeRun(run);
  assert.ok(problems.some((p) => p.includes("failed without a single failing test")));
});

test("a drop in the collected test count fails the gate", () => {
  const run = greenRun();
  run.report.numTotalTests = 9; // one test silently vanished
  const { problems } = analyzeRun(run);
  assert.ok(
    problems.some((p) => p.includes("the run collected 9 test(s)")),
    `expected a count-drop problem, got: ${JSON.stringify(problems)}`,
  );
  assert.ok(problems.some((p) => p.includes("1 test(s) vanished")));
});

test("a growing suite does not fail the gate", () => {
  const run = greenRun();
  run.report.numTotalTests = 11;
  assert.deepEqual(analyzeRun(run).problems, []);
});

test("a filtered run does not apply the whole-suite checks", () => {
  const run = greenRun();
  run.onDiskTestFiles = null;
  run.report.numTotalTests = 2;
  run.report.testResults = [file("src/a.test.ts", [failed("a > known red")])];
  assert.deepEqual(analyzeRun(run).problems, []);
});

test("a test file that never reported fails the gate", () => {
  const run = greenRun();
  run.onDiskTestFiles.push("src/dead.test.ts");
  const { problems } = analyzeRun(run);
  assert.ok(
    problems.some((p) => p.includes("never reported a result")),
    `expected a missing-file problem, got: ${JSON.stringify(problems)}`,
  );
  assert.ok(problems.some((p) => p.includes("src/dead.test.ts")));
});

test("a failure outside the ledger fails the gate", () => {
  const run = greenRun();
  run.report.testResults[1].assertionResults[0] = failed("b > 1");
  run.report.testResults[1].status = "failed";
  run.report.numFailedTests = 2;
  const { problems } = analyzeRun(run);
  assert.ok(
    problems.some((p) => p.includes("not in test-known-failures.json")),
    `expected an unexpected-failure problem, got: ${JSON.stringify(problems)}`,
  );
  assert.ok(problems.some((p) => p.includes("src/b.test.ts > b > 1")));
});

test("an unhandled error fails the gate", () => {
  const run = greenRun();
  run.captured = "stderr: Unhandled Error\nsomething escaped a worker";
  const { problems } = analyzeRun(run);
  assert.ok(problems.some((p) => p.includes("unhandled error")));
});

test("a ledger entry that starts passing is a reminder, not a failure", () => {
  // The documented anti-flake contract. The previous catch-all compared
  // `numFailedTests !== knownTotal`, so fixing a known-red test turned the
  // build red with an "Unexplained" message — exactly the wrong incentive.
  const run = greenRun();
  run.report.testResults[0].assertionResults = [pass("a > known red"), pass("a > ok")];
  run.report.testResults[0].status = "passed";
  run.report.numFailedTests = 0;
  run.code = 1; // vitest still non-zero, e.g. from an unrelated non-test signal

  const { problems, nowPassing } = analyzeRun(run);
  assert.deepEqual(nowPassing, ["src/a.test.ts > a > known red"]);
  assert.deepEqual(
    problems,
    [],
    `a now-passing ledger entry must not fail the job, got: ${JSON.stringify(problems)}`,
  );
});

test("more failures than the allowance with nothing else to blame still fails", () => {
  // The catch-all. Contrived: a failure the per-assertion walk cannot see.
  const run = greenRun();
  run.report.numFailedTests = 5;
  const { problems } = analyzeRun(run);
  assert.ok(
    problems.some((p) => p.includes("Unexplained")),
    `expected the catch-all to fire, got: ${JSON.stringify(problems)}`,
  );
});

test("summary reports the counts the console prints", () => {
  const { summary } = analyzeRun(greenRun());
  assert.deepEqual(summary, {
    filesReported: 2,
    testsRun: 10,
    failures: 1,
    knownTotal: 1,
    knownFiles: 1,
  });
});
