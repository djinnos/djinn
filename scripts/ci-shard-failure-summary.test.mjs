import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';

import {
  extractFailedTests,
  parseStatusLine,
  renderShardFailure,
  stripAnsi,
} from './ci-shard-failure-summary.mjs';

const SCRIPT = resolve('scripts/ci-shard-failure-summary.mjs');
const WORKFLOW = resolve('.github/workflows/quality-gate.yml');
const FIXTURES = resolve('scripts/fixtures/shard-failure');

// The fixtures are slices of the real captured shard log from run 30149467240 /
// job 89657358613 (task rwpk), timestamps stripped so they match what the
// workflow's `tee` writes. ANSI escapes are preserved byte-for-byte: nextest
// colorizes on CI and that is half of why the old grep never matched.
const fixture = (name) => readFileSync(join(FIXTURES, name), 'utf8');

const ESC = String.fromCharCode(27);
const AUDIT_TEST =
  'djinn-provider::stream_event_consumer_audit checked_in_classification_covers_every_production_stream_event_match';

// The pattern the workflow used before task 8hrv, as a POSIX-ERE-equivalent
// JS regex. It is asserted against the fixtures so the defect cannot come back
// disguised as a different spelling.
const OLD_PATTERN = /^[ \t]*FAIL \[/m;

test('TRY-n retry failures are the shape nextest actually emits, and the old pattern misses them', () => {
  const log = fixture('nextest-try-n-fail.log');
  assert.equal(OLD_PATTERN.test(log), false, 'fixture must contain no bare FAIL line for the old pattern to find');
  assert.match(stripAnsi(log), /^\s*TRY 3 FAIL \[/m);
});

test('a log whose only failure records are TRY-n FAIL is classified as a test failure', () => {
  const log = fixture('nextest-try-n-fail.log');
  assert.deepEqual(extractFailedTests(log), [AUDIT_TEST]);

  const { failedTests, annotation, summary } = renderShardFailure({
    shardName: 'shard-1',
    logPath: '/tmp/nextest-run.log',
    logText: log,
  });
  assert.deepEqual(failedTests, [AUDIT_TEST]);
  assert.equal(annotation, `::error title=Server Test shard-1 failed::${AUDIT_TEST}`);
  assert.match(summary, /Failing tests:/);
  assert.match(summary, new RegExp(`- \`${AUDIT_TEST}\``.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));

  // The whole point: the build/migration/setup diagnostic must NOT be chosen.
  for (const text of [annotation, summary]) {
    assert.doesNotMatch(text, /build/i);
    assert.doesNotMatch(text, /migration/i);
    assert.doesNotMatch(text, /setup/i);
    assert.doesNotMatch(text, /no failure status lines/);
  }
});

test('ordinary FAIL records stay recognized and duplicate IDs are deduplicated', () => {
  // Each ID appears twice in the fixture: once live, once in the Summary block.
  const failed = extractFailedTests(fixture('nextest-plain-fail.log'));
  assert.deepEqual(failed, [
    'djinn-db repositories::note::tests::rrf_orders_by_fused_rank',
    AUDIT_TEST,
  ]);
  assert.equal(new Set(failed).size, failed.length);
});

test('a log with no failure record of any kind still selects the generic diagnostic', () => {
  const log = fixture('nextest-no-failure-lines.log');
  assert.deepEqual(extractFailedTests(log), []);

  const { failedTests, annotation, summary } = renderShardFailure({
    shardName: 'shard-2',
    logPath: '/tmp/nextest-run.log',
    logText: log,
  });
  assert.deepEqual(failedTests, []);
  assert.match(annotation, /^::error title=Server Test shard-2 failed::/);
  assert.match(annotation, /contains no failure status lines \(FAIL \/ TRY n FAIL \/ TIMEOUT \/ SIG\*\)/);
  assert.match(annotation, /last red step in this job's log/);
  assert.match(summary, /Observed: nextest ran and its log/);

  // The generic branch reports what it observed. It must not assert a cause it
  // has not established — that claim is what sent nineteen sessions to the
  // wrong layer.
  for (const text of [annotation, summary]) {
    assert.doesNotMatch(text, /build, migration, or setup/i);
    assert.doesNotMatch(text, /concluded failure without nextest FAIL lines/);
  }
});

test('an absent nextest log is reported as an absent log, not as a build failure', () => {
  const { failedTests, annotation, summary } = renderShardFailure({
    shardName: 'shard-3',
    logPath: '/tmp/nextest-run.log',
    logText: undefined,
  });
  assert.deepEqual(failedTests, []);
  assert.match(annotation, /no nextest log was produced at \/tmp\/nextest-run\.log/);
  assert.doesNotMatch(annotation, /build, migration, or setup/i);
  assert.match(summary, /no nextest log was produced/);
});

test('a retry that recovered is not reported as this shard failing test', () => {
  assert.deepEqual(extractFailedTests(fixture('nextest-flaky-recovered.log')), []);
});

test('status-line grammar: failure statuses in, run noise out', () => {
  const cases = [
    ['        FAIL [   0.002s] (2/3) nxprobe nxtests::plain_fail', 'nxprobe nxtests::plain_fail'],
    ['  TRY 3 FAIL [   0.483s] (2477/2842) crate test_name', 'crate test_name'],
    ['     SIGABRT [   0.003s] (1/3) nxprobe nxtests::aborts', 'nxprobe nxtests::aborts'],
    ['     TIMEOUT [   1.001s] (3/3) nxprobe nxtests::times_out', 'nxprobe nxtests::times_out'],
    ['   LEAK-FAIL [   0.004s] (4/9) crate leaky_test', 'crate leaky_test'],
    // No progress counter: older nextest releases, and the plan-time output.
    ['        FAIL [   0.005s] crate older_nextest_shape', 'crate older_nextest_shape'],
    // The Actions-API download prefixes every line with a runner timestamp.
    ['2026-07-25T07:45:23.7159008Z   TRY 3 FAIL [   0.483s] (2477/2842) crate downloaded_log_shape', 'crate downloaded_log_shape'],
  ];
  for (const [line, id] of cases) {
    assert.deepEqual(extractFailedTests(line), [id], line);
  }

  const noise = [
    '     Summary [ 196.905s] 2480/2842 tests run: 2479 passed, 1 failed, 0 skipped',
    '  Cancelling due to test failure: 3 tests still running',
    '    test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured',
    '    test checked_in_classification ... FAILED',
    '        SLOW [>  30.000s] (───) crate slow_test',
    ' TERMINATING [>   1.000s] (───) nxprobe nxtests::times_out',
    '        PASS [   0.008s] (1/2) crate passing_test',
    '        LEAK [   0.008s] (2/2) crate leaky_but_passing_test',
    '   DELAY 2/3 by   0.625s (───) crate retried_test',
    'error: test run failed',
  ];
  for (const line of noise) {
    assert.deepEqual(extractFailedTests(line), [], line);
  }
});

test('escape sequences and split-color test paths normalize to `<binary-id> <test-name>`', () => {
  // nextest colors the module path and the `::` separator independently, so the
  // stripped line must not gain or lose whitespace around them.
  const line = `${ESC}[32;1m        FAIL${ESC}[0m [   0.008s] (2365/2842) ${ESC}[35;1mdjinn-runtime${ESC}[0m ${ESC}[36mspec::tests${ESC}[0m${ESC}[36m::${ESC}[0m${ESC}[34;1mtask_run_spec_roundtrips${ESC}[0m`;
  assert.equal(
    stripAnsi(line),
    '        FAIL [   0.008s] (2365/2842) djinn-runtime spec::tests::task_run_spec_roundtrips',
  );
  assert.deepEqual(parseStatusLine(line), {
    status: 'FAIL',
    id: 'djinn-runtime spec::tests::task_run_spec_roundtrips',
  });
});

test('CLI prints the annotation and appends the job summary block', () => {
  const workdir = mkdtempSync(join(tmpdir(), 'shard-failure-'));
  const summaryPath = join(workdir, 'step-summary.md');
  const stdout = execFileSync('node', [
    SCRIPT,
    '--shard', 'shard-1',
    '--log', join(FIXTURES, 'nextest-try-n-fail.log'),
    '--summary', summaryPath,
  ], { encoding: 'utf8' });

  assert.equal(stdout, `::error title=Server Test shard-1 failed::${AUDIT_TEST}\n`);
  const summary = readFileSync(summaryPath, 'utf8');
  assert.match(summary, /### Server Test shard-1 — FAILED/);
  assert.ok(summary.includes(AUDIT_TEST), 'summary names the failing test');
});

test('CLI treats a missing log as a missing log', () => {
  const stdout = execFileSync('node', [
    SCRIPT,
    '--shard', 'shard-4',
    '--log', join(FIXTURES, 'does-not-exist.log'),
  ], { encoding: 'utf8' });
  assert.match(stdout, /no nextest log was produced at/);
  assert.doesNotMatch(stdout, /build, migration, or setup/i);
});

test('the shard step delegates to this script and the old pattern is gone', () => {
  const workflow = readFileSync(WORKFLOW, 'utf8');
  assert.ok(
    workflow.includes('scripts/ci-shard-failure-summary.mjs'),
    'the shard failure step must call the tested parser',
  );
  assert.ok(
    workflow.includes('node --test scripts/ci-shard-failure-summary.test.mjs'),
    'preflight must run this contract',
  );
  assert.equal(
    workflow.includes("grep -E '^[[:space:]]*FAIL \\['"),
    false,
    'the untestable inline grep must not come back',
  );
  assert.equal(
    workflow.includes('shard concluded failure without nextest FAIL lines'),
    false,
    'the cause-asserting diagnostic must not come back',
  );
});
