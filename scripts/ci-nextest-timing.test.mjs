// Contract for scripts/ci-nextest-timing.mjs.
//
// The defect this replaces is the reason for the shape of these assertions.
// The previous producer emitted a document that was:
//
//   * correctly versioned,
//   * correctly timestamped,
//   * keyed with exactly the right ids,
//   * accepted by the planner's own `loadTiming`, and
//   * completely useless — every value was `elapsed / test_count`, so a
//     duration-balanced plan and a test-count-balanced plan were the same plan.
//
// Nothing about "the artifact was produced" or "the artifact validates" could
// have caught that. So the tests below assert the VALUES (a real spread, real
// per-test numbers) and the KEY FORMAT (the planner's `package|binary|test`,
// NOT the log's `binary-id test-name`), which are the two ways this can go
// silently inert again.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test, { describe, it } from 'node:test';

import { loadTiming, parseDiscovery } from './ci-nextest-plan.mjs';
import { scriptCode } from './lib/source-text.mjs';
import {
  buildLogIdIndex,
  buildTimingDocument,
  parseDurationSeconds,
  parseLogDurations,
  parseTimedLine,
} from './ci-nextest-timing.mjs';

const SCRIPT = resolve('scripts/ci-nextest-timing.mjs');
const WORKFLOW = resolve('.github/workflows/quality-gate.yml');
const FIXTURES = resolve('scripts/fixtures/nextest-timing');
const SHARD_FAILURE_FIXTURES = resolve('scripts/fixtures/shard-failure');

const fixture = (name) => readFileSync(join(FIXTURES, name), 'utf8');

const DISCOVERY = fixture('discovery.json');
const LOG = fixture('nextest-run.log');
const SHARD_TEST_IDS = JSON.parse(fixture('shard-test-ids.json'));

// Canonical ids written out literally, NOT derived from parseDiscovery. If the
// producer and this file both derived them the key-format assertion would be
// vacuous — the whole failure mode is the two sides agreeing with each other
// and disagreeing with the planner.
const AUDIT_CANONICAL =
  'djinn-provider|stream_event_consumer_audit|checked_in_classification_covers_every_production_stream_event_match';
const AUDIT_LOG_ID =
  'djinn-provider::stream_event_consumer_audit checked_in_classification_covers_every_production_stream_event_match';
const FLAKY_CANONICAL =
  'djinn-server|djinn-server|server::chat::tests::handler::completions_rejects_non_uuid_session';
const NEVER_RAN_CANONICAL =
  'djinn-runtime|djinn-runtime|spec::tests::never_reached_before_cancellation';
const SLOW_CANONICAL = 'djinn-server|coordinator_dispatch|dispatch_settles_under_sustained_load';

function build({ log = LOG, elapsedSeconds = 196.905 } = {}) {
  return buildTimingDocument({
    shardTestIds: SHARD_TEST_IDS,
    logIdIndex: buildLogIdIndex(DISCOVERY),
    logDurations: parseLogDurations(log),
    elapsedSeconds,
    generatedAt: '2026-08-06T11:45:20Z',
  });
}

test('durations parse in every shape nextest can print one', () => {
  // The two confirmed against the captured CI log: nextest 0.9.133 prints
  // plain seconds at both ends of the range.
  assert.equal(parseDurationSeconds('   0.011s'), 0.011);
  assert.equal(parseDurationSeconds(' 196.905s'), 196.905);
  // SLOW reports a lower bound.
  assert.equal(parseDurationSeconds('>  60.000s'), 60);
  // Accepted defensively: a nextest that switched to composite units must not
  // make every long test silently fall back to the median.
  assert.equal(parseDurationSeconds('1m 05.32s'), 65.32);
  assert.equal(parseDurationSeconds('1h 2m 3.000s'), 3723);
  // Not durations.
  assert.equal(parseDurationSeconds('─────────'), null);
  assert.equal(parseDurationSeconds(''), null);
});

test('status lines yield the log-shaped id and its duration, through the ANSI', () => {
  const parsed = parseTimedLine(LOG.split('\n')[2]);
  assert.deepEqual(parsed, {
    status: 'PASS',
    id: 'djinn-runtime spec::tests::task_run_report_bincode_roundtrip',
    seconds: 0.007,
  });

  // Non-test lines that live in the same log and share its prefix shape.
  for (const line of LOG.split('\n')) {
    const p = parseTimedLine(line);
    if (p === null) continue;
    assert.doesNotMatch(p.id, /tests run/, 'the Summary line is not a test');
    assert.doesNotMatch(p.id, /nextest profile/, 'the run header is not a test');
  }
});

test('a real captured CI log parses to real, differing durations', () => {
  // scripts/fixtures/shard-failure/* are slices of the log from run
  // 30149467240 / job 89657358613 — untouched bytes, borrowed here so this
  // parser is proven against output nobody wrote for it.
  const captured = readFileSync(join(SHARD_FAILURE_FIXTURES, 'nextest-try-n-fail.log'), 'utf8');
  const durations = parseLogDurations(captured);
  assert.equal(durations.get('djinn-runtime spec::tests::task_run_report_bincode_roundtrip'), 0.007);
  assert.equal(durations.get(AUDIT_LOG_ID), 0.526, 'max across TRY 1/2/3 attempts');
  assert.ok(
    new Set(durations.values()).size > 1,
    'a real log must not produce a single repeated value',
  );
  for (const id of durations.keys()) {
    assert.doesNotMatch(id, /^\d/, 'a progress counter must never be mistaken for a binary id');
  }
});

test('keys are the planner canonical id, never the log id', () => {
  const { document } = build();

  assert.ok(
    Object.hasOwn(document.tests, AUDIT_CANONICAL),
    'the artifact must be keyed by package|binary|test',
  );
  assert.ok(
    !Object.hasOwn(document.tests, AUDIT_LOG_ID),
    'a log-shaped key would be silently discarded by loadTiming as a deleted test',
  );

  // The reconciliation, stated as the property that matters: every key this
  // artifact publishes is a key the planner will actually look up.
  const discovered = new Set(parseDiscovery(DISCOVERY).map(({ id }) => id));
  for (const key of Object.keys(document.tests)) {
    assert.ok(discovered.has(key), `key ${key} is not a discovered test id`);
  }
});

test('the planner accepts the document and keeps every sample', () => {
  const { document } = build();
  const discoveredIds = new Set(parseDiscovery(DISCOVERY).map(({ id }) => id));
  const timing = loadTiming(JSON.stringify(document), {
    discoveredIds,
    now: Date.parse('2026-08-06T12:00:00Z'),
  });

  assert.equal(timing.valid, true, timing.reason ?? '');
  // The failure mode is silent partial loss: loadTiming drops unknown keys
  // without a word, so "valid" alone proves nothing.
  assert.equal(
    timing.timings.size,
    SHARD_TEST_IDS.length,
    'every emitted key must survive the planner filter',
  );
  assert.equal(timing.timings.get(SLOW_CANONICAL), 62.481);
});

test('the value distribution is NOT uniform — the entire point', () => {
  const { document, stats } = build();
  const values = Object.values(document.tests);
  const distinct = new Set(values);

  assert.ok(distinct.size > 1, 'a uniform document is the defect being fixed');
  assert.equal(Math.min(...values), 0.007);
  assert.equal(Math.max(...values), 120);
  // The old producer's entire spread was under 1.22x (0.4599..0.56 across
  // 12,462 entries). Anything in that neighbourhood means the parse fell
  // through to the fallback for effectively everything.
  assert.ok(
    Math.max(...values) / Math.min(...values) > 100,
    'real per-test durations span orders of magnitude',
  );
  assert.equal(stats.measured, 8);
  assert.equal(stats.fallback, 1);
});

test('per-test durations are the measured ones, with retries folded to the max', () => {
  const { document } = build();
  assert.equal(document.tests[AUDIT_CANONICAL], 0.483);
  // TRY 1 FAIL 0.526s then TRY 2 PASS 0.501s.
  assert.equal(document.tests[FLAKY_CANONICAL], 0.526);
  // SLOW's `> 60.000s` lower bound is superseded by the settled 62.481s.
  assert.equal(document.tests[SLOW_CANONICAL], 62.481);
  // LEAK and TIMEOUT lines carry durations too and must not be dropped.
  assert.equal(document.tests['djinn-server|coordinator_dispatch|pool_teardown_leaks_a_child'], 3.204);
  assert.equal(document.tests['djinn-server|coordinator_dispatch|wedged_admission_probe'], 120);
});

test('a test that never printed a line still gets an entry, at the median', () => {
  const { document } = build();
  // Required, not merely nice: nextest-timing-publish asserts this file's key
  // set EQUALS the shard's matrix row, so an omission reds the gate.
  assert.deepEqual(
    Object.keys(document.tests).slice().sort(),
    SHARD_TEST_IDS.slice().sort(),
    'exactly the shard test ids, no more and no fewer',
  );

  // Median of the eight measured values (0.007, 0.011, 0.483, 0.526, 0.74,
  // 3.204, 62.481, 120) = (0.526 + 0.74) / 2.
  assert.equal(document.tests[NEVER_RAN_CANONICAL], 0.633);
});

test('an unparseable or absent log degrades to the old uniform document', () => {
  // Not a nicety: this is the behaviour being replaced, so the degenerate case
  // is by construction never worse than the status quo.
  const { document, stats } = build({ log: '', elapsedSeconds: 180 });
  const values = Object.values(document.tests);
  assert.equal(stats.measured, 0);
  assert.equal(new Set(values).size, 1);
  assert.equal(values[0], 20); // 180 / 9
});

test('log lines for tests outside this shard are ignored', () => {
  const { stats } = buildTimingDocument({
    shardTestIds: [AUDIT_CANONICAL],
    logIdIndex: buildLogIdIndex(DISCOVERY),
    logDurations: parseLogDurations(LOG),
    elapsedSeconds: 100,
    generatedAt: '2026-08-06T11:45:20Z',
  });
  assert.equal(stats.measured, 1);
  assert.equal(stats.unmatchedLogIds, 7, 'the other shards\' tests are dropped, not merged in');
});

test('the CLI writes the publish job\'s schema to disk', () => {
  const dir = mkdtempSync(join(tmpdir(), 'nextest-timing-'));
  const output = join(dir, 'timing-0.json');
  const stdout = execFileSync('node', [
    SCRIPT,
    '--discovery', join(FIXTURES, 'discovery.json'),
    '--test-ids', join(FIXTURES, 'shard-test-ids.json'),
    '--log', join(FIXTURES, 'nextest-run.log'),
    '--elapsed', '196',
    '--generated-at', '2026-08-06T11:45:20Z',
    '--output', output,
  ], { encoding: 'utf8' });

  const document = JSON.parse(readFileSync(output, 'utf8'));

  // The exact assertions nextest-timing-publish makes over each shard file.
  assert.equal(document.version, 'ci-nextest-timing/v1');
  assert.equal(typeof document.generated_at, 'string');
  assert.equal(typeof document.tests, 'object');
  assert.ok(!Array.isArray(document.tests));
  assert.ok(Object.keys(document.tests).every((id) => typeof id === 'string' && id.length > 0));
  assert.ok(Object.values(document.tests).every((v) => typeof v === 'number' && Number.isFinite(v) && v >= 0));
  assert.deepEqual(Object.keys(document.tests).sort(), SHARD_TEST_IDS.slice().sort());

  // generated_at must stay in the second-precision form the workflow passes
  // (`date -u +%Y-%m-%dT%H:%M:%SZ`); the restore-side jq parses it with
  // fromdateiso8601, which is strptime and rejects a fractional part.
  assert.match(document.generated_at, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/);

  assert.match(stdout, /::notice::Shard timing: 8\/9 tests measured/);
});

test('a missing log file is survivable at the CLI, not a crash', () => {
  const dir = mkdtempSync(join(tmpdir(), 'nextest-timing-'));
  const output = join(dir, 'timing-0.json');
  execFileSync('node', [
    SCRIPT,
    '--discovery', join(FIXTURES, 'discovery.json'),
    '--test-ids', join(FIXTURES, 'shard-test-ids.json'),
    '--log', join(dir, 'does-not-exist.log'),
    '--elapsed', '90',
    '--generated-at', '2026-08-06T11:45:20Z',
    '--output', output,
  ], { encoding: 'utf8' });
  const document = JSON.parse(readFileSync(output, 'utf8'));
  assert.equal(new Set(Object.values(document.tests)).size, 1);
  assert.equal(document.tests[NEVER_RAN_CANONICAL], 10); // 90 / 9
});

// Comments are stripped before these run: the retired jq expression is quoted
// verbatim in the workflow's own explanatory comment, and a ban that matched
// prose would red the build with no behavioural change.
describe('workflow wiring', () => {
  const source = () => scriptCode(readFileSync(WORKFLOW, 'utf8'));

  it('produces the timing artifact from the log, not from the wall clock', () => {
    const code = source();
    assert.match(code, /scripts\/ci-nextest-timing\.mjs/, 'the shard step must run the parser');
    assert.match(code, /--log "\$RUNNER_TEMP\/nextest-run\.log"/, 'the parser must be fed the teed log');
    assert.match(code, /--discovery nextest-plan\/discovery\.json/, 'the canonical-id mapping comes from discovery');
  });

  it('keeps the shard exit status untouched', () => {
    // The step reports the TEST result. A timing generator that changed the
    // step's exit code would turn a balancing detail into a gate.
    const code = source();
    assert.match(code, /status=\$\{PIPESTATUS\[0\]\}/);
    assert.match(code, /\n {10}exit "\$status"\n/, 'the step must still end by exiting the cargo status');
  });

  it('never lets a timing failure red the shard', () => {
    // nextest-timing-publish requires one file per shard, so "no file" is an
    // outage while "coarse file" is only the status quo ante.
    const code = source();
    assert.match(code, /\|\| \[ ! -s "\$timing_file" \]/, 'an empty output must trigger the fallback');
    assert.match(code, /reduce \$ids\[\] as \$id/, 'the uniform document survives as the last-resort fallback');
  });

  it('runs this contract in CI', () => {
    assert.match(source(), /node --test scripts\/ci-nextest-timing\.test\.mjs/);
  });
});
