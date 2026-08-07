import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';

import {
  REPORT_VERSION,
  buildShardReport,
  extractRetryOutcomes,
  renderRunAggregate,
  renderShardRetrySummary,
} from './ci-retry-flake-summary.mjs';

const SCRIPT = resolve('scripts/ci-retry-flake-summary.mjs');
const WORKFLOW = resolve('.github/workflows/quality-gate.yml');
const FIXTURES = resolve('scripts/fixtures/shard-failure');
const fixture = (name) => readFileSync(join(FIXTURES, name), 'utf8');

const FLAKY_TEST =
  'djinn-server server::chat::tests::handler::completions_rejects_non_uuid_session';
const AUDIT_TEST =
  'djinn-provider::stream_event_consumer_audit checked_in_classification_covers_every_production_stream_event_match';

const run = (args, options = {}) =>
  execFileSync('node', [SCRIPT, ...args], { encoding: 'utf8', ...options });

test('a TRY 2 that passes is reported as a retry-absorbed flake', () => {
  // Real captured shard output: TRY 1 FAIL -> DELAY -> TRY 2 PASS, and the
  // Summary line says "2842 passed (1 flaky)". The lane was GREEN.
  const log = fixture('nextest-flaky-recovered.log');
  const { absorbed, persistent } = extractRetryOutcomes(log);
  assert.deepEqual(absorbed, [
    { id: FLAKY_TEST, attempts: 2, failedAttempts: 1, passedOnAttempt: 2 },
  ]);
  assert.deepEqual(persistent, []);

  const { annotation, summary } = renderShardRetrySummary({
    shardName: 'shard-1',
    logPath: '/tmp/nextest-run.log',
    logText: log,
  });
  assert.match(annotation, /^::warning title=Server Test shard-1: 1 retry-absorbed flake::/);
  assert.ok(annotation.includes(FLAKY_TEST), 'the annotation names the flaky test');
  assert.match(summary, /1 retry-absorbed flake \(lane is GREEN\)/);
  assert.match(summary, /Without retries this lane would be RED/);
  assert.ok(summary.includes(`\`${FLAKY_TEST}\``), 'the summary names the flaky test');
});

test('multiple absorbed flakes are counted, including one absorbed by the LAST retry', () => {
  const { absorbed, persistent } = extractRetryOutcomes(fixture('nextest-retries-absorbed.log'));
  assert.deepEqual(absorbed, [
    { id: FLAKY_TEST, attempts: 2, failedAttempts: 1, passedOnAttempt: 2 },
    {
      id: 'djinn-k8s session::tests::kill_session_then_has_session_is_false',
      attempts: 3,
      failedAttempts: 2,
      passedOnAttempt: 3,
    },
    // TIMEOUT on attempt 1, then LEAK (a pass) on attempt 2: both non-FAIL
    // statuses still have to fold into the same ledger.
    {
      id: 'djinn-worker cancel::tests::cancellation_drains_inflight',
      attempts: 2,
      failedAttempts: 1,
      passedOnAttempt: 2,
    },
  ]);
  assert.deepEqual(persistent, []);
});

test('a clean shard log produces no annotation and no step-summary block', () => {
  const log = fixture('nextest-no-failure-lines.log');
  assert.deepEqual(extractRetryOutcomes(log), { absorbed: [], persistent: [] });

  const { annotation, summary, report } = renderShardRetrySummary({
    shardName: 'shard-2',
    logPath: '/tmp/nextest-run.log',
    logText: log,
  });
  assert.equal(annotation, '', 'a clean shard must add nothing to the run annotations');
  assert.equal(summary, '', 'a clean shard must add nothing to the job page');
  assert.equal(report.absorbedCount, 0);
});

test('an un-retried failure is not a flake, and an exhausted retry is not absorbed', () => {
  // Plain FAIL, no TRY prefix: nothing was retried, so there is nothing to report.
  assert.deepEqual(extractRetryOutcomes(fixture('nextest-plain-fail.log')).absorbed, []);
  assert.deepEqual(extractRetryOutcomes(fixture('nextest-plain-fail.log')).persistent, []);

  // TRY 1/2/3 all FAIL: retries were spent and did NOT absorb it. Counting this
  // as an absorbed flake would inflate the very number this script exists to
  // report; the lane is red and the failure summary already names it.
  const { absorbed, persistent } = extractRetryOutcomes(fixture('nextest-try-n-fail.log'));
  assert.deepEqual(absorbed, []);
  assert.deepEqual(persistent, [
    { id: AUDIT_TEST, attempts: 3, failedAttempts: 3, passedOnAttempt: null },
  ]);

  // nextest repeats the final attempt inside its Summary block. Counting lines
  // instead of folding attempt numbers would report 4 attempts, not 3.
  assert.equal(persistent[0].attempts, 3);

  const { annotation } = renderShardRetrySummary({
    shardName: 'shard-4',
    logPath: '/tmp/nextest-run.log',
    logText: fixture('nextest-try-n-fail.log'),
  });
  assert.equal(annotation, '', 'a red lane must not raise a flake warning');
});

test('an absent log degrades to a notice, never to a failure claim', () => {
  const { annotation, summary, report } = renderShardRetrySummary({
    shardName: 'shard-3',
    logPath: '/tmp/nextest-run.log',
    logText: undefined,
  });
  assert.match(annotation, /^::notice title=Server Test shard-3 retry telemetry::/);
  assert.doesNotMatch(annotation, /^::error/);
  assert.equal(summary, '');
  assert.equal(report.observed, false);
});

test('the machine-readable shard report carries the version and the count', () => {
  const report = buildShardReport({
    shardName: 'shard-1',
    logPath: '/tmp/nextest-run.log',
    logText: fixture('nextest-retries-absorbed.log'),
  });
  assert.equal(report.version, REPORT_VERSION);
  assert.equal(report.shard, 'shard-1');
  assert.equal(report.absorbedCount, 3);
  assert.equal(report.absorbedCount, report.absorbed.length);
});

test('the run-level aggregate sums absorbed flakes across shards', () => {
  const shard = (name, logName) =>
    buildShardReport({ shardName: name, logPath: 'x', logText: fixture(logName) });
  const { absorbedCount, shardCount, annotation, summary } = renderRunAggregate([
    shard('shard-1', 'nextest-flaky-recovered.log'),
    shard('shard-2', 'nextest-no-failure-lines.log'),
    shard('shard-3', 'nextest-retries-absorbed.log'),
  ]);
  assert.equal(shardCount, 3);
  assert.equal(absorbedCount, 4);
  assert.match(annotation, /^::warning title=Retry-absorbed flakes: 4 in this run::/);
  assert.match(summary, /### Retry-absorbed flakes: 4 in this run/);
  assert.match(summary, /This run is GREEN/);
  assert.match(summary, /\| shard-1 \|/);
  assert.match(summary, /\| shard-3 \|/);
  assert.doesNotMatch(summary, /\| shard-2 \|/);
});

test('a run with no absorbed flakes says so and raises no warning', () => {
  const clean = buildShardReport({
    shardName: 'shard-1',
    logPath: 'x',
    logText: fixture('nextest-no-failure-lines.log'),
  });
  const { absorbedCount, annotation, summary } = renderRunAggregate([clean, clean]);
  assert.equal(absorbedCount, 0);
  assert.equal(annotation, '');
  assert.match(summary, /Retry-absorbed flakes: none/);
});

test('the aggregate ignores rows that are not this report version', () => {
  const { absorbedCount, annotation } = renderRunAggregate([
    null,
    { version: 'something-else/v9', absorbed: [{ id: 'x', attempts: 2, passedOnAttempt: 2 }] },
  ]);
  assert.equal(absorbedCount, 0);
  assert.match(annotation, /^::notice title=Retry-absorbed flakes::/);
});

test('CLI: an absorbed flake is written to the step summary and a JSON report', () => {
  const workdir = mkdtempSync(join(tmpdir(), 'retry-flake-'));
  const summaryPath = join(workdir, 'step-summary.md');
  const jsonPath = join(workdir, 'flake/flake-0.json');
  const stdout = run([
    '--shard', 'shard-1',
    '--log', join(FIXTURES, 'nextest-flaky-recovered.log'),
    '--summary', summaryPath,
    '--json', jsonPath,
  ]);

  assert.match(stdout, /^::warning title=Server Test shard-1: 1 retry-absorbed flake::/);
  assert.match(readFileSync(summaryPath, 'utf8'), /retry-absorbed flake \(lane is GREEN\)/);
  const report = JSON.parse(readFileSync(jsonPath, 'utf8'));
  assert.equal(report.version, REPORT_VERSION);
  assert.equal(report.absorbedCount, 1);
});

test('CLI: a clean log writes nothing to the step summary and exits 0', () => {
  const workdir = mkdtempSync(join(tmpdir(), 'retry-flake-'));
  const summaryPath = join(workdir, 'step-summary.md');
  writeFileSync(summaryPath, '');
  const stdout = run([
    '--shard', 'shard-2',
    '--log', join(FIXTURES, 'nextest-no-failure-lines.log'),
    '--summary', summaryPath,
  ]);
  assert.equal(stdout, '');
  assert.equal(readFileSync(summaryPath, 'utf8'), '');
});

test('CLI: every degraded path exits 0 with a notice — telemetry never reds a lane', () => {
  const workdir = mkdtempSync(join(tmpdir(), 'retry-flake-'));

  // Missing log.
  assert.match(
    run(['--shard', 'shard-3', '--log', join(workdir, 'absent.log')]),
    /^::notice title=Server Test shard-3 retry telemetry::no nextest log at/,
  );

  // Unreadable log: a directory where a file was expected. readFileSync throws
  // EISDIR, and the script must still exit 0 and say only what it observed.
  mkdirSync(join(workdir, 'a-directory'));
  assert.match(
    run(['--shard', 'shard-3', '--log', join(workdir, 'a-directory')]),
    /^::notice title=Server Test shard-3 retry telemetry::/,
  );

  // Malformed content: a binary blob is not a nextest log. It parses to nothing.
  const binary = join(workdir, 'binary.log');
  writeFileSync(binary, Buffer.from([0, 1, 2, 255, 254, 10, 0, 66, 10]));
  assert.equal(run(['--shard', 'shard-3', '--log', binary]), '');

  // Bad arguments must not throw out of the process either.
  assert.match(run(['--nonsense']), /^::notice title=Retry telemetry unavailable::/);
  assert.match(run(['--shard', 'shard-3']), /^::notice title=Retry telemetry unavailable::/);
});

test('CLI: --aggregate folds the per-shard JSON reports into one run-level count', () => {
  const workdir = mkdtempSync(join(tmpdir(), 'retry-flake-'));
  const reportDir = join(workdir, 'reports');
  mkdirSync(reportDir);
  for (const [index, [shard, logName]] of [
    ['shard-1', 'nextest-flaky-recovered.log'],
    ['shard-2', 'nextest-no-failure-lines.log'],
    ['shard-3', 'nextest-retries-absorbed.log'],
  ].entries()) {
    run([
      '--shard', shard,
      '--log', join(FIXTURES, logName),
      '--json', join(reportDir, `flake-${index}.json`),
    ]);
  }
  // A corrupt sibling must not suppress the shards that did parse.
  writeFileSync(join(reportDir, 'flake-9.json'), '{not json');

  const summaryPath = join(workdir, 'run-summary.md');
  const stdout = run(['--aggregate', reportDir, '--summary', summaryPath]);
  assert.match(stdout, /::warning title=Retry-absorbed flakes: 4 in this run::/);
  assert.match(readFileSync(summaryPath, 'utf8'), /### Retry-absorbed flakes: 4 in this run/);

  // An aggregate directory that does not exist is a notice, not a failure.
  assert.match(
    run(['--aggregate', join(workdir, 'nope')]),
    /^::notice title=Retry-absorbed flakes::no shard retry reports/,
  );
});

test('the workflow wires this script in and keeps it non-blocking', () => {
  const workflow = readFileSync(WORKFLOW, 'utf8');
  assert.ok(
    workflow.includes('scripts/ci-retry-flake-summary.mjs'),
    'the shard must call the tested retry parser',
  );
  assert.ok(
    workflow.includes('node --test scripts/ci-retry-flake-summary.test.mjs'),
    'preflight must run this contract',
  );
  // The whole point is reporting on GREEN runs: a step gated on failure() would
  // reproduce exactly the blind spot this exists to close.
  assert.ok(
    workflow.includes('name: Surface retry-absorbed flakes'),
    'the shard-level telemetry step must exist',
  );
  // Telemetry must never be a merge-blocking lane.
  const gateNeeds = workflow.slice(workflow.indexOf('  quality-gate:'));
  assert.equal(
    gateNeeds.includes('- nextest-flake-report'),
    false,
    'the flake report must not become a required check',
  );
});
