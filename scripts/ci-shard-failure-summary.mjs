#!/usr/bin/env node
/**
 * Run-level failure summary for the Server Test shards (quality-gate.yml).
 *
 * WHY THIS IS A SCRIPT AND NOT AN INLINE grep
 *
 * The shard step used to grep `^[[:space:]]*FAIL \[` out of the tee'd nextest
 * log, and when that found nothing it emitted a message that *asserted a
 * cause*: "shard concluded failure without nextest FAIL lines (build,
 * migration, or setup step)". That annotation is what the board surfaces as
 * failure evidence, so nineteen remediation sessions on task rwpk were told,
 * authoritatively and wrongly, to go look at a build/migration/setup step. The
 * real failure was an ordinary assertion in
 * `djinn-provider::stream_event_consumer_audit`.
 *
 * Two independent reasons the old pattern could not match. Both confirmed
 * against the captured shard log of run 30149467240 (job 89657358613) and
 * reproduced locally with cargo-nextest 0.9.133:
 *
 *   1. Retries. `pull-request`, `merge-group` and `full-validation` all inherit
 *      `[profile.ci]`'s `retries = { count = 2, ... }` (server/.config/nextest.toml),
 *      so a genuinely failing test never prints a bare `FAIL` line — every
 *      attempt, the final one, and its repeat in the Summary block are all
 *      prefixed: `TRY 3 FAIL [   0.483s] ...`.
 *   2. Color. quality-gate.yml sets `CARGO_TERM_COLOR: always` workflow-wide, so
 *      nextest emits SGR escapes even into a pipe, and they wrap the status
 *      token together with its indentation:
 *      `<ESC>[31;1m        FAIL<ESC>[0m [   0.002s]`. The old pattern's
 *      `^[[:space:]]*` anchor and its space before `[` are therefore both wrong
 *      even for an un-retried failure.
 *
 * Status-line grammar (nextest 0.9.133), after escapes are stripped:
 *
 *     [indent]{TRY <n> }?<STATUS> [<duration>] {(<progress>) }?<binary-id> <test-name>
 *
 * The progress counter — `(2477/2842)` once the total is settled, `(───)` while
 * it is not — is why stripping only the timing bracket left the old sed
 * emitting `(2477/2842) <binary-id> <test-name>` as the "test id". It is
 * optional in the pattern because older nextest releases omit it.
 *
 * Escapes also wrap each segment of a test path independently
 * (`<ESC>[36mspec::tests<ESC>[0m<ESC>[36m::<ESC>[0m<ESC>[34;1mfoo<ESC>[0m`), so
 * stripping them is what reassembles `spec::tests::foo`.
 *
 * PASS/LEAK lines are parsed too, but only to *retract* a candidate: a flaky
 * test prints `TRY 1 FAIL` and then `TRY 2 PASS`, and blaming this shard's
 * failure on it would be the same misattribution pointed the other way.
 *
 * Usage:
 *   node scripts/ci-shard-failure-summary.mjs --shard <name> --log <path> \
 *     [--summary <path>]
 *
 * Writes the `::error::` annotation to stdout and, when --summary names a file,
 * appends the job-page summary block to it.
 */
import { appendFileSync, existsSync, readFileSync } from 'node:fs';

/**
 * SGR/CSI escape sequences.
 *
 * Two spellings are accepted: the real ESC byte, which is what nextest writes
 * and what the workflow parses, and the two-character `^[` transliteration that
 * `cat -v`-style captures leave behind (the log a human downloads and pastes
 * often went through one). ESC is built from a char code so no invisible byte
 * is checked into this source.
 */
const ANSI_ESCAPE = new RegExp(`(?:${String.fromCharCode(27)}|\\^\\[)\\[[0-9;?]*[ -/]*[@-~]`, 'g');

/**
 * {<log decoration> }?[indent]{TRY <n> }?<STATUS> [<duration>] {(<progress>) }?<rest>
 *
 * The workflow feeds the tee'd runner-side log, which carries no decoration.
 * The optional prefixes are for the copies a human ever has in hand, so the
 * same parser can be pointed at them while debugging: the log archive
 * downloaded from Actions prefixes every line with an ISO timestamp, and
 * `gh run view --log` prefixes `<job name>\t<step name>\t<ISO timestamp> `.
 */
const STATUS_LINE = new RegExp(
  '^(?:[^\\t]*\\t[^\\t]*\\t)?' + // gh run view --log job/step decoration
    '(?:\\d{4}-\\d{2}-\\d{2}T[\\d:.]+Z )?' + // Actions log-archive timestamp
    '\\s*(?:TRY\\s+\\d+\\s+)?' + // retry attempt marker
    '([A-Z][A-Z-]*)\\s+' + // status token
    '\\[[^\\]]*\\]\\s+' + // [   0.483s]
    '(?:\\([^)]*\\)\\s+)?' + // (2477/2842) or (───), when present
    '(\\S.*?)\\s*$', // <binary-id> <test-name>
);

/** Terminal failure statuses. LEAK-FAIL precedes FAIL so it is not truncated. */
const FAILURE_STATUS = /^(?:LEAK-FAIL|FAIL|TIMEOUT|ABORT|SIG[A-Z]+)$/;

/** Statuses meaning the test ultimately succeeded on that attempt. */
const SUCCESS_STATUS = /^(?:PASS|LEAK)$/;

/** The first `error:` line nextest/cargo wrote, if any — observed, not construed. */
const ERROR_LINE = /^\s*error(?:\[[A-Za-z0-9]+\])?:\s*(\S.*?)\s*$/;

/** How many trailing log lines the job-page summary quotes when no test failed. */
const TAIL_LINES = 15;

/** Per-line cap on quoted log text: a panic dump can be one enormous line. */
const MAX_QUOTED_CHARS = 400;

export function stripAnsi(text) {
  return String(text).replace(ANSI_ESCAPE, '');
}

function clamp(line, limit = MAX_QUOTED_CHARS) {
  return line.length <= limit ? line : `${line.slice(0, limit)}… (truncated)`;
}

/**
 * Parse one nextest status line into `{ status, id }`, or null when the line is
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
 * A test that later reports PASS/LEAK on a retry did not fail this run.
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

/** The first `error:` line in the log, or null. Quoted verbatim as evidence. */
export function firstErrorLine(logText) {
  for (const line of stripAnsi(logText).split('\n')) {
    const match = ERROR_LINE.exec(line);
    if (match !== null) return clamp(match[1]);
  }
  return null;
}

/** The last non-empty lines of the log, escapes stripped, for the summary block. */
export function logTail(logText, limit = TAIL_LINES) {
  return stripAnsi(logText)
    .split('\n')
    .map((line) => line.replace(/\s+$/, ''))
    .filter((line) => line !== '')
    .slice(-limit)
    .map((line) => clamp(line));
}

/**
 * Render the run-level annotation and the job-page summary block.
 *
 * The no-failing-test branch reports only what it observed — whether a log
 * exists, that it holds no failure status line, and the first `error:` line it
 * does hold. It deliberately does not name a layer. Concluding "build,
 * migration, or setup step" from the absence of matches is a claim about the
 * log dressed up as a claim about the run, and that is the claim that cost
 * nineteen sessions.
 */
export function renderShardFailure({ shardName, logPath, logText }) {
  const shard = `Server Test ${shardName}`;
  const haveLog = typeof logText === 'string';
  const failedTests = haveLog ? extractFailedTests(logText) : [];

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

  const observed = haveLog
    ? `nextest ran and its log (${logPath}) holds no failure status line (FAIL / TRY n FAIL / TIMEOUT / SIG*)`
    : `no nextest log was produced at ${logPath}`;
  const errorLine = haveLog ? firstErrorLine(logText) : null;
  const evidence = errorLine === null ? '' : ` First error line in that log: "${errorLine}".`;
  const tail = haveLog ? logTail(logText) : [];

  return {
    failedTests,
    annotation:
      `::error title=${shard} failed::${observed}.${evidence}` +
      " This does not identify which step failed — read the last red step in this job's log.",
    summary: [
      `### ${shard} — FAILED`,
      '',
      `Observed: ${observed}.`,
      // Not wrapped in backticks: cargo error text routinely contains them
      // (`linking with \`cc\` failed`) and nesting them breaks the rendering.
      ...(errorLine === null ? [] : ['', `First \`error:\` line in that log: ${errorLine}`]),
      '',
      "This does not identify which step failed — read the last red step in this job's log.",
      // Four-backtick fence so a log line containing a triple fence cannot end it.
      ...(tail.length === 0
        ? []
        : ['', `Last ${tail.length} log lines:`, '', '````text', ...tail, '````']),
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
