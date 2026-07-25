#!/usr/bin/env node
/**
 * Run-level failure summary for the Server Test shards (quality-gate.yml).
 *
 * The shard step that used to do this inline grepped `^[[:space:]]*FAIL \[`.
 * That pattern misses what nextest actually writes, and the fallback branch it
 * fell through to did not merely say nothing — it asserted a cause:
 * "shard concluded failure without nextest FAIL lines (build, migration, or
 * setup step)". Nineteen remediation sessions on task rwpk were sent to hunt a
 * build problem that never existed; the real failure was an ordinary assertion
 * in djinn-provider::stream_event_consumer_audit.
 *
 * Two things break the old pattern. Both were confirmed against a real captured
 * shard log (run 30149467240, job 89657358613) and against cargo-nextest
 * 0.9.133 locally:
 *
 *   1. Retries. The `pull-request`/`merge-group`/`full-validation` profiles all
 *      inherit `[profile.ci]`'s `retries = { count = 2, ... }` (see
 *      server/.config/nextest.toml), so a genuinely failing test never prints a
 *      bare `FAIL` line at all — every attempt, including the final one and its
 *      repeat in the Summary block, is prefixed: `TRY 3 FAIL [   0.483s] ...`.
 *   2. Color. nextest emits SGR escapes on CI, and they wrap the status token
 *      together with its indentation: `<ESC>[31;1m  TRY 3 FAIL<ESC>[0m [   0.483s]`.
 *      So even a no-retry `FAIL` line does not start with whitespace-then-FAIL
 *      and does not have a space between `FAIL` and `[`. The old pattern could
 *      not have matched an un-retried failure on CI either.
 *
 * Observed status-line grammar (nextest 0.9.133), after escapes are stripped:
 *
 *     [indent]{TRY <n> }?<STATUS> [<duration>] {(<progress>) }?<binary-id> <test-name>
 *
 * The progress counter — `(2477/2842)` on a settled attempt, `(───)` while the
 * count is still unknown — is why stripping only the timing bracket left the
 * old sed emitting `(2477/2842) binary test` as the "test id". It is optional
 * here because older nextest releases do not print it.
 *
 * Failure statuses observed: FAIL, SIGABRT, TIMEOUT. SIG<NAME> is matched
 * generically and LEAK-FAIL is included by name; both are terminal failures in
 * the same grammar. PASS/LEAK lines are parsed too, but only to *retract* a
 * candidate: a flaky test prints `TRY 1 FAIL` and then `TRY 2 PASS`, and
 * reporting it as this shard's failing test would be the same misattribution
 * pointed the other way.
 *
 * Usage:
 *   node scripts/ci-shard-failure-summary.mjs --shard <name> --log <path> \
 *     [--summary <path>]
 *
 * Prints the `::error::` annotation to stdout and, when --summary names a file,
 * appends the job-page summary block to it.
 */
import { appendFileSync, existsSync, readFileSync } from 'node:fs';

/**
 * SGR/CSI escape sequences, as nextest writes them when it forces color on CI.
 * Built from a char code so no invisible ESC byte is checked into this source.
 */
const ANSI_ESCAPE = new RegExp(`${String.fromCharCode(27)}\\[[0-9;?]*[ -/]*[@-~]`, 'g');

/**
 * {<runner timestamp> }?[indent]{TRY <n> }?<STATUS> [<duration>] {(<progress>) }?<rest>
 *
 * The workflow feeds the tee'd runner-side log, which has no timestamps. The
 * optional ISO prefix is for the other log a human ever has in hand — the one
 * downloaded from the Actions API, where every line is timestamp-prefixed — so
 * the same parser can be pointed at it while debugging.
 */
const STATUS_LINE =
  /^(?:\d{4}-\d{2}-\d{2}T[\d:.]+Z )?\s*(?:TRY\s+\d+\s+)?([A-Z][A-Z-]*)\s+\[[^\]]*\]\s+(?:\([^)]*\)\s+)?(\S.*?)\s*$/;

/** Terminal failure statuses. LEAK-FAIL precedes FAIL so it is not split. */
const FAILURE_STATUS = /^(?:LEAK-FAIL|FAIL|TIMEOUT|SIG[A-Z]+)$/;

/** Statuses that mean the test ultimately succeeded on that attempt. */
const SUCCESS_STATUS = /^(?:PASS|LEAK)$/;

export function stripAnsi(text) {
  return String(text).replace(ANSI_ESCAPE, '');
}

/**
 * Parse one nextest status line into `{ status, id }`, or null if the line is
 * not a status line. `id` is the normalized `<binary-id> <test-name>`.
 */
export function parseStatusLine(line) {
  const match = STATUS_LINE.exec(stripAnsi(line));
  if (match === null) return null;
  const status = match[1];
  if (!FAILURE_STATUS.test(status) && !SUCCESS_STATUS.test(status)) return null;
  const id = match[2].replace(/\s+/g, ' ');
  if (id.length === 0) return null;
  return { status, id };
}

/**
 * Failing test IDs in the order nextest first reported them, deduplicated.
 * A test that later reports PASS/LEAK on a retry is not a failure of this run.
 */
export function extractFailedTests(logText) {
  const failed = [];
  const seen = new Set();
  const recovered = new Set();
  for (const line of stripAnsi(logText).split('\n')) {
    const parsed = parseStatusLine(line);
    if (parsed === null) continue;
    if (SUCCESS_STATUS.test(parsed.status)) {
      recovered.add(parsed.id);
      continue;
    }
    if (seen.has(parsed.id)) continue;
    seen.add(parsed.id);
    failed.push(parsed.id);
  }
  return failed.filter((id) => !recovered.has(id));
}

/**
 * Render the run-level annotation and the job-page summary block.
 *
 * The no-failure-lines wording states only what was observed. The previous
 * wording concluded the cause ("build, migration, or setup step") from the
 * absence of matches, which is exactly the claim that cost nineteen sessions;
 * absence of failure lines is evidence about the log, not about the step.
 */
export function renderShardFailure({ shardName, logPath, logText }) {
  const shard = `Server Test ${shardName}`;
  const failedTests = typeof logText === 'string' ? extractFailedTests(logText) : [];

  if (failedTests.length > 0) {
    return {
      failedTests,
      annotation: `::error title=${shard} failed::${failedTests.join('; ')}`,
      summary: [
        `### ${shard} — FAILED`,
        '',
        'Failing tests:',
        '',
        ...failedTests.map((id) => `- \`${id}\``),
        '',
      ].join('\n'),
    };
  }

  const observed = typeof logText === 'string'
    ? `nextest ran and its log (${logPath}) contains no failure status lines (FAIL / TRY n FAIL / TIMEOUT / SIG*)`
    : `no nextest log was produced at ${logPath}`;
  return {
    failedTests,
    annotation: `::error title=${shard} failed::${shard} failed and ${observed}; the failing step is the last red step in this job's log.`,
    summary: [
      `### ${shard} — FAILED`,
      '',
      `Observed: ${observed}.`,
      '',
      "This does not identify which layer failed — check the last red step in this job's log.",
      '',
    ].join('\n'),
  };
}

function parseArgs(argv) {
  const args = { shard: '', log: '', summary: '' };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (flag === '--shard' || flag === '--log' || flag === '--summary') {
      if (value === undefined) throw new Error(`ci-shard-failure-summary: ${flag} needs a value`);
      args[flag.slice(2)] = value;
      index += 1;
      continue;
    }
    throw new Error(`ci-shard-failure-summary: unexpected argument ${JSON.stringify(flag)}`);
  }
  if (args.shard === '') throw new Error('ci-shard-failure-summary: --shard is required');
  if (args.log === '') throw new Error('ci-shard-failure-summary: --log is required');
  return args;
}

function main(argv) {
  const args = parseArgs(argv);
  const logText = existsSync(args.log) ? readFileSync(args.log, 'utf8') : undefined;
  const { annotation, summary } = renderShardFailure({
    shardName: args.shard,
    logPath: args.log,
    logText,
  });
  process.stdout.write(`${annotation}\n`);
  if (args.summary !== '') appendFileSync(args.summary, `${summary}\n`);
}

if (process.argv[1] !== undefined && import.meta.url === `file://${process.argv[1]}`) {
  main(process.argv.slice(2));
}
