#!/usr/bin/env node
/**
 * Build one Server Test shard's `ci-nextest-timing/v1` artifact from the REAL
 * per-test durations nextest printed, instead of dividing the shard's wall
 * clock uniformly across its tests.
 *
 * WHAT WAS WRONG
 * --------------
 * The step that produced this file used to be an inline jq expression:
 *
 *     .[$id] = ($elapsed / $count)
 *
 * i.e. every test in the shard was assigned the SAME number — the shard's total
 * wall clock divided by its test count. That artifact is structurally valid, is
 * accepted by `loadTiming`, and is therefore used for balancing — while
 * carrying no per-test signal whatsoever. `estimateDuration` returns the exact
 * sample when one exists, so the planner packed shards by a constant, which is
 * arithmetically identical to packing by TEST COUNT. Every published artifact
 * proved it: all 12,462 entries of the last one lay between 0.4599 and 0.56.
 *
 * The consequence is not "slightly worse balancing" — it is that the entire
 * duration-balancing mechanism was inert. Shards landed at 534s and 866s in the
 * same run, a 62% spread, because equal test counts are not equal durations:
 * one `#[tokio::test]` that awaits a Postgres round trip costs two orders of
 * magnitude more than a pure-function unit test, and the planner could not see
 * the difference.
 *
 * WHERE THE REAL DURATIONS COME FROM
 * ----------------------------------
 * They were already being captured and thrown away. The run step tees nextest's
 * output to `$RUNNER_TEMP/nextest-run.log` (for the failure-surfacing step),
 * and every settled attempt prints its own duration:
 *
 *     PASS [   0.011s] (2363/2842) djinn-runtime spec::tests::some_test
 *
 * This script parses those lines. The grammar and its traps (retry prefixes,
 * ANSI escapes wrapping the status token, the optional progress counter) are
 * documented in scripts/ci-shard-failure-summary.mjs, which parses the same
 * lines for a different field; `stripAnsi` is imported from there so the two
 * cannot drift apart.
 *
 * KEY-FORMAT RECONCILIATION (the part that silently breaks)
 * ---------------------------------------------------------
 * The log names a test as `<binary-id> <test-name>`. The planner's timing map
 * is keyed by `canonicalId(packageName, binaryName, testName)` —
 * `package|binary|test`. These are DIFFERENT strings, and nothing downstream
 * would report the difference: `loadTiming` silently drops every id that is not
 * in the current discovery set (they are indistinguishable from deleted tests),
 * so emitting log-shaped keys would yield a valid, fresh, correctly-versioned
 * artifact with zero usable samples — the same class of silent inertness this
 * script exists to remove.
 *
 * So the mapping is NOT reconstructed here. `parseDiscovery` — the planner's
 * own function, applied to the planner's own `nextest-plan/discovery.json` — is
 * the sole source of both sides: it yields `{ id, binaryId, testName }` per
 * test, from which this builds `"<binary-id> <test-name>" -> <canonical id>`.
 * The key written to the artifact is therefore the identical string the planner
 * computes for the same test, by construction rather than by agreement.
 *
 * FALLBACKS (a missing key would fail the publish job, not just degrade)
 * ---------------------------------------------------------------------
 * `nextest-timing-publish` asserts that shard i's timing ids equal shard i's
 * matrix row EXACTLY. So this file must contain an entry for every id in
 * `shard-test-ids.json` and for nothing else — omitting a test that never ran
 * is a hard failure of the gate, not a soft loss of signal.
 *
 *   * Per-test miss (test never ran because an earlier failure cancelled the
 *     run, was filtered out, or its line did not parse): the MEDIAN of this
 *     shard's observed durations. The planner's own `estimateDuration` uses a
 *     median for unknown tests, so this is its policy applied one layer up; and
 *     a median cannot skew the packing the way a 0 (test is free) or the
 *     elapsed-time mean (test is average) would.
 *   * Whole-log miss (log absent, unreadable, or nothing parsed at all): the
 *     old uniform `elapsed / count`, so the degenerate case is exactly the
 *     behaviour being replaced and never worse than it.
 *   * Unreadable discovery/test-ids: reported non-zero, and the caller falls
 *     back to the same uniform document.
 *
 * Retries: a test that prints `TRY 1 FAIL [0.526s]` then `TRY 2 PASS [0.501s]`
 * contributes MAX(attempts), not the sum and not the last. The per-attempt cost
 * is the stable property of the test; retry count is not reproducible across
 * runs, and the trailing Summary block re-prints the final attempt (max is
 * idempotent under that duplicate). `SLOW [> 60.000s]` lines are lower bounds
 * on a still-running test and are folded in the same way, so a test killed by
 * the run's cancellation is still known to be at least that expensive.
 *
 * Usage:
 *   node scripts/ci-nextest-timing.mjs \
 *     --discovery nextest-plan/discovery.json \
 *     --test-ids nextest-plan/shard-test-ids.json \
 *     --log "$RUNNER_TEMP/nextest-run.log" \
 *     --elapsed <shard wall clock seconds> \
 *     --generated-at <ISO 8601> \
 *     --output nextest-timing/timing-<index>.json
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';

import { parseDiscovery } from './ci-nextest-plan.mjs';
import { stripAnsi } from './ci-shard-failure-summary.mjs';

export const TIMING_VERSION = 'ci-nextest-timing/v1';

/**
 * {<runner timestamp> }?[indent]{TRY <n> }?<STATUS> [<duration>] {(<progress>) }?<binary-id> <test-name>
 *
 * Same grammar as ci-shard-failure-summary.mjs, with the duration bracket
 * captured instead of discarded. Verified against the captured shard log in
 * scripts/fixtures/shard-failure/ (nextest 0.9.133, the version CI runs).
 */
const STATUS_LINE =
  /^(?:\d{4}-\d{2}-\d{2}T[\d:.]+Z )?\s*(?:TRY\s+\d+\s+)?([A-Z][A-Z-]*)\s+\[([^\]]*)\]\s+(?:\([^)]*\)\s+)?(\S.*?)\s*$/;

/**
 * Statuses whose line carries a duration for a NAMED test.
 *
 * LEAK-FAIL precedes FAIL so the alternation does not split it. SLOW is a
 * lower bound (`[> 60.000s]`) rather than a settled duration and is folded in
 * as one — see the retry note in the header. `Summary` is not listed and could
 * not match anyway: it is not all-caps, and its trailing text is a sentence,
 * not a test id.
 */
const TIMED_STATUS = /^(?:PASS|LEAK-FAIL|LEAK|FAIL|TIMEOUT|SLOW|SIG[A-Z]+)$/;

/** Durations are written with millisecond precision; keep that, drop the noise. */
const DURATION_PRECISION = 3;

function round(value) {
  const factor = 10 ** DURATION_PRECISION;
  return Math.round(value * factor) / factor;
}

export function median(values) {
  if (values.length === 0) return null;
  const sorted = values.slice().sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  if (sorted.length % 2 === 1) return sorted[mid];
  return (sorted[mid - 1] + sorted[mid]) / 2;
}

/**
 * Parse the contents of a nextest duration bracket into seconds.
 *
 * nextest 0.9.133 prints plain seconds at every magnitude — `   0.011s` for a
 * unit test and ` 196.905s` for a whole run summary, both confirmed in the
 * captured log fixtures — so no unit reconstruction is needed for the format
 * CI actually emits. The hour/minute components are accepted anyway because a
 * future nextest that switched to `3m 16.90s` would otherwise make every long
 * test parse as `null` and silently collapse back to the median.
 *
 * Returns seconds, or null when the bracket is not a duration.
 */
export function parseDurationSeconds(text) {
  if (typeof text !== 'string') return null;
  // `SLOW` reports a lower bound as `> 60.000s`.
  const cleaned = text.trim().replace(/^>\s*/, '');
  const match = /^(?:(\d+)h\s*)?(?:(\d+)m\s*)?(\d+(?:\.\d+)?)s$/.exec(cleaned);
  if (match === null) return null;
  const hours = match[1] === undefined ? 0 : Number(match[1]);
  const minutes = match[2] === undefined ? 0 : Number(match[2]);
  const seconds = Number(match[3]);
  const total = hours * 3600 + minutes * 60 + seconds;
  if (!Number.isFinite(total) || total < 0) return null;
  return total;
}

/**
 * Parse one nextest status line into `{ status, id, seconds }`, or null.
 *
 * `id` is the normalized `<binary-id> <test-name>` exactly as
 * ci-shard-failure-summary.mjs normalizes it, which is the join key the
 * discovery index below is built against.
 */
export function parseTimedLine(line) {
  const match = STATUS_LINE.exec(stripAnsi(line));
  if (match === null) return null;
  const status = match[1];
  if (!TIMED_STATUS.test(status)) return null;
  const seconds = parseDurationSeconds(match[2]);
  if (seconds === null) return null;
  const id = match[3].replace(/\s+/g, ' ');
  if (id.length === 0) return null;
  return { status, id, seconds };
}

/**
 * Fold a whole nextest log into `"<binary-id> <test-name>" -> seconds`,
 * keeping the maximum across attempts (see the retry note in the header).
 */
export function parseLogDurations(logText) {
  const durations = new Map();
  if (typeof logText !== 'string') return durations;
  for (const line of logText.split('\n')) {
    const parsed = parseTimedLine(line);
    if (parsed === null) continue;
    const previous = durations.get(parsed.id);
    if (previous === undefined || parsed.seconds > previous) {
      durations.set(parsed.id, parsed.seconds);
    }
  }
  return durations;
}

/**
 * `"<binary-id> <test-name>" -> canonical planner id`, built from the
 * planner's own discovery parse. This is the reconciliation described in the
 * header; nothing else in this file constructs a canonical id.
 */
export function buildLogIdIndex(discoveryText) {
  const index = new Map();
  for (const test of parseDiscovery(discoveryText)) {
    index.set(`${test.binaryId} ${test.testName}`.replace(/\s+/g, ' '), test.id);
  }
  return index;
}

/**
 * Assemble the artifact.
 *
 * Returns `{ document, stats }`. `document.tests` has exactly one entry per id
 * in `shardTestIds`, in that order, because the publish job compares this key
 * set against the shard's matrix row for equality.
 */
export function buildTimingDocument({
  shardTestIds,
  logIdIndex,
  logDurations,
  elapsedSeconds,
  generatedAt,
}) {
  const ids = [];
  const seen = new Set();
  for (const id of shardTestIds) {
    if (typeof id !== 'string' || id.length === 0) continue;
    if (seen.has(id)) continue;
    seen.add(id);
    ids.push(id);
  }

  // Project the log's observations onto canonical ids, keeping only tests this
  // shard owns. A retry that ran on another shard cannot appear here, but the
  // filter is what makes that a property rather than a hope.
  const observed = new Map();
  for (const [logId, seconds] of logDurations) {
    const canonical = logIdIndex.get(logId);
    if (canonical === undefined) continue;
    if (!seen.has(canonical)) continue;
    observed.set(canonical, seconds);
  }

  const uniform = ids.length > 0 && Number.isFinite(elapsedSeconds) && elapsedSeconds >= 0
    ? elapsedSeconds / ids.length
    : 0;
  const observedMedian = median([...observed.values()]);
  // Whole-log miss: reproduce the old uniform document rather than emit zeros.
  const fallback = observedMedian === null ? uniform : observedMedian;

  const tests = {};
  for (const id of ids) {
    const seconds = observed.has(id) ? observed.get(id) : fallback;
    tests[id] = round(seconds);
  }

  const values = [...observed.values()];
  return {
    document: { version: TIMING_VERSION, generated_at: generatedAt, tests },
    stats: {
      total: ids.length,
      measured: observed.size,
      fallback: ids.length - observed.size,
      fallbackSeconds: round(fallback),
      min: values.length > 0 ? round(Math.min(...values)) : null,
      max: values.length > 0 ? round(Math.max(...values)) : null,
      median: observedMedian === null ? null : round(observedMedian),
      unmatchedLogIds: logDurations.size - observed.size,
    },
  };
}

function parseArgs(argv) {
  const args = {
    discovery: '',
    'test-ids': '',
    log: '',
    elapsed: '0',
    'generated-at': '',
    output: '',
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (!flag.startsWith('--') || !(flag.slice(2) in args)) {
      throw new Error(`ci-nextest-timing: unexpected argument ${JSON.stringify(flag)}`);
    }
    const value = argv[index + 1];
    if (value === undefined) throw new Error(`ci-nextest-timing: ${flag} needs a value`);
    args[flag.slice(2)] = value;
    index += 1;
  }
  for (const required of ['discovery', 'test-ids', 'log', 'generated-at', 'output']) {
    if (args[required] === '') throw new Error(`ci-nextest-timing: --${required} is required`);
  }
  return args;
}

function main(argv) {
  const args = parseArgs(argv);

  const shardTestIds = JSON.parse(readFileSync(args['test-ids'], 'utf8'));
  if (!Array.isArray(shardTestIds)) throw new Error('shard test ids must be a JSON array');
  const logIdIndex = buildLogIdIndex(readFileSync(args.discovery, 'utf8'));

  // The log is the one input that is legitimately allowed to be missing: the
  // shard can fail before nextest writes anything. Falling through with an
  // empty log yields the uniform document, which is what this step used to
  // emit unconditionally.
  const logText = existsSync(args.log) ? readFileSync(args.log, 'utf8') : '';

  const { document, stats } = buildTimingDocument({
    shardTestIds,
    logIdIndex,
    logDurations: parseLogDurations(logText),
    elapsedSeconds: Number(args.elapsed),
    generatedAt: args['generated-at'],
  });

  mkdirSync(dirname(args.output), { recursive: true });
  writeFileSync(args.output, `${JSON.stringify(document, null, 2)}\n`);

  // Printed so a future "the artifact is uniform again" regression is visible
  // in the job log without downloading the artifact: measured==0, or a min
  // equal to the max, is the signature of the defect this replaced.
  process.stdout.write(
    `::notice::Shard timing: ${stats.measured}/${stats.total} tests measured from the nextest log `
    + `(min=${stats.min}s median=${stats.median}s max=${stats.max}s); `
    + `${stats.fallback} unmeasured test(s) recorded at ${stats.fallbackSeconds}s; `
    + `${stats.unmatchedLogIds} log line(s) belonged to no test in this shard.\n`,
  );
}

if (process.argv[1] !== undefined && import.meta.url === `file://${process.argv[1]}`) {
  main(process.argv.slice(2));
}
