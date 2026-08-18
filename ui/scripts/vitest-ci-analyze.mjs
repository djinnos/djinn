/**
 * The decision logic of the UI test gate, separated from its I/O.
 *
 * `vitest-ci.mjs` spawns vitest, reads the JSON report and the ledger, then
 * hands both to `analyzeRun` below. Nothing here touches the filesystem, the
 * process, or the clock, so `vitest-ci-analyze.test.mjs` can drive every branch
 * with a synthetic report — including the ones that are expensive or awkward to
 * provoke for real (a dead worker, a collection-time throw, a vanished test).
 *
 * That split exists because the gate itself shipped untested and had a hole
 * straight through it: it counted failed *assertions*, and a file that throws at
 * module scope reports `status: "failed"` with **zero** assertions. All 35 of
 * its tests disappeared from the run and the gate printed PASSED (task `pxtd`).
 * A gate with no tests of its own is just another untested script.
 */

import path from "node:path";

/**
 * Decide whether a completed vitest run should fail the job.
 *
 * @param {object} args
 * @param {object} args.report            Parsed vitest JSON report.
 * @param {object} args.manifest          Parsed `test-known-failures.json`.
 * @param {string} args.uiRoot            Absolute path the report's file names are relative to.
 * @param {string[] | null} args.onDiskTestFiles
 *        Repo-relative test files vitest was expected to report on, or `null`
 *        when a filter narrowed the run and no whole-suite claim can be made.
 * @param {string} args.captured          vitest's combined stdout/stderr.
 * @param {number | null} args.code       vitest's exit code.
 * @returns {{problems: string[], nowPassing: string[], summary: object}}
 */
export function analyzeRun({
  report,
  manifest,
  uiRoot,
  onDiskTestFiles,
  captured = "",
  code = 0,
}) {
  const files = report.testResults ?? [];
  const rel = (name) => path.relative(uiRoot, name);

  /** file -> names of its failing assertions */
  const reported = new Map();
  for (const file of files) {
    reported.set(
      rel(file.name),
      (file.assertionResults ?? [])
        .filter((a) => a.status === "failed")
        .map((a) => a.fullName),
    );
  }

  const known = manifest.knownFailures ?? {};
  const knownTotal = Object.values(known).flat().length;
  const problems = [];

  // (1) Every test file on disk must have reported. A file that is silently
  //     absent is a dead worker, not a passing suite.
  if (onDiskTestFiles !== null) {
    const missing = onDiskTestFiles.filter((f) => !reported.has(f));
    if (missing.length > 0) {
      problems.push(
        `${missing.length} test file(s) on disk never reported a result:`,
        ...missing.map((f) => `    ${f}`),
        "  A missing file means its worker died mid-run. Run it on its own to see why.",
      );
    }
  }

  // (2) A file whose status is `failed` while none of its assertions failed did
  //     not fail a test — it failed to LOAD. A bad import, a syntax error or a
  //     throwing module-scope initialiser all look like this, and every test in
  //     the file is skipped rather than run. vitest exits 1; the assertion count
  //     never moves, so no count-based check can see it.
  const suiteErrors = files.filter(
    (f) =>
      f.status === "failed" &&
      !(f.assertionResults ?? []).some((a) => a.status === "failed"),
  );
  if (suiteErrors.length > 0) {
    problems.push(
      `${suiteErrors.length} test file(s) failed without a single failing test.`,
      "  That is a collection error: the file never loaded, so none of its tests ran.",
      ...suiteErrors.map((f) => {
        const reason = firstLine(f.message);
        return `    ${rel(f.name)}${reason ? ` — ${reason}` : ""}`;
      }),
    );
  }

  // (3) The suite must not shrink unannounced. The collection failure above cost
  //     35 tests while the failure count stayed at exactly the ledger's 23, so
  //     the gate needs a floor on the total as well as a ceiling on failures.
  const expectedTotal = manifest.totalTests;
  if (
    onDiskTestFiles !== null &&
    typeof expectedTotal === "number" &&
    typeof report.numTotalTests === "number" &&
    report.numTotalTests < expectedTotal
  ) {
    problems.push(
      `the run collected ${report.numTotalTests} test(s); test-known-failures.json records ${expectedTotal}.`,
      `  ${expectedTotal - report.numTotalTests} test(s) vanished without failing. A file that throws at import,`,
      "  a suite skipped by a stray `.skip`, or a deleted test all look like this.",
      '  If the reduction is intentional, lower "totalTests" in that file in the same commit.',
    );
  }

  // (4) Unhandled errors set vitest's exit code but never appear in the JSON
  //     report, so read them off the console output.
  if (/Unhandled Error/.test(captured)) {
    problems.push(
      "vitest reported an unhandled error during the run (see output above).",
    );
  }

  // (5) Compare the failure set against the recorded debt ledger.
  const unexpected = [];
  for (const [file, failures] of reported) {
    for (const name of failures) {
      if (!(known[file] ?? []).includes(name)) unexpected.push(`${file} > ${name}`);
    }
  }
  if (unexpected.length > 0) {
    problems.push(
      `${unexpected.length} test(s) failed that are not in test-known-failures.json:`,
      ...unexpected.map((n) => `    ${n}`),
      "  These are regressions. Fix them — do not add them to the ledger.",
    );
  }

  // A ledger entry that starts passing is reported loudly and nothing more.
  // Several are timing-sensitive, and a gate that goes red because a slow test
  // got lucky on a fast runner is a gate people learn to ignore.
  const nowPassing = [];
  for (const [file, names] of Object.entries(known)) {
    const failures = reported.get(file);
    if (failures === undefined) continue; // covered by the missing-file check
    for (const name of names) {
      if (!failures.includes(name)) nowPassing.push(`${file} > ${name}`);
    }
  }

  // Last resort: a non-zero exit with MORE failures than the allowance and no
  // named problem means vitest failed for a reason this wrapper does not model.
  // The comparison is `>` and not `!==` on purpose — `<` is the now-passing case
  // above, which must stay a reminder rather than a red build.
  if (code !== 0 && problems.length === 0 && report.numFailedTests > knownTotal) {
    problems.push(
      `vitest exited ${code} but the report shows ${report.numFailedTests} failure(s)`,
      `against a ${knownTotal}-failure allowance. Unexplained — treating as failure.`,
    );
  }

  return {
    problems,
    nowPassing,
    summary: {
      filesReported: reported.size,
      testsRun: report.numTotalTests,
      failures: report.numFailedTests,
      knownTotal,
      knownFiles: Object.keys(known).length,
    },
  };
}

function firstLine(message) {
  if (typeof message !== "string") return "";
  const line = message.split("\n").find((l) => l.trim().length > 0);
  return line ? line.trim().slice(0, 160) : "";
}
