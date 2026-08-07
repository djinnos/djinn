#!/usr/bin/env node
/**
 * Surface retry-ABSORBED flakes on the Server Test shards — on SUCCESS.
 *
 * `server/.config/nextest.toml` sets `retries = { count = 2 }` on
 * `[profile.ci]`, which `pull-request` / `merge-group` / `full-validation` all
 * inherit. Every test therefore gets three attempts, and a test that fails once
 * and passes on a retry produces a GREEN lane and no signal anywhere.
 *
 * The size of that blind spot was measured, not guessed. Across 619 shard logs
 * from 80 SUCCESSFUL merge_group runs, 33 of the 80 runs (41.2%) contained at
 * least one retry-absorbed flake — 34 instances in total, silently swallowed.
 * The visible CI failure rate over the same window is 2.1%, so the true flake
 * rate is roughly twenty times what anyone could see.
 *
 * This script closes that gap. It is PURE TELEMETRY:
 *   * it always exits 0 — a parse problem degrades to a `::notice::`, never to
 *     a red lane, because a reporting bug must not fail a run whose tests
 *     passed;
 *   * it does not touch `retries`. Whether to keep three attempts is a separate
 *     decision, and it cannot be made honestly until the cost is visible.
 *
 * Parsing is NOT reimplemented here. `scripts/ci-shard-failure-summary.mjs`
 * already encodes the two properties of real nextest output that a naive grep
 * gets wrong — the `TRY <n> ` prefix and the SGR escapes that wrap the status
 * token together with its indentation (see that file's header, and task 8hrv).
 * A second parser would drift from the first; this one imports it.
 *
 * Usage:
 *   node scripts/ci-retry-flake-summary.mjs --shard <name> --log <path> \
 *     [--summary <path>] [--json <path>]
 *   node scripts/ci-retry-flake-summary.mjs --aggregate <dir> [--summary <path>]
 */
import { appendFileSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

import {
  isFailureStatus,
  isSuccessStatus,
  parseStatusLine,
} from './ci-shard-failure-summary.mjs';

export const REPORT_VERSION = 'ci-retry-flake/v1';

/**
 * Fold a shard's nextest log into per-test attempt records.
 *
 * Every attempt line for a test is a row in the same ledger, and nextest
 * repeats the FINAL attempt inside its Summary block, so the fold takes the
 * max/min rather than counting lines — counting would double the last attempt
 * of every failing test.
 *
 * Returns `{ absorbed, persistent }`, both in first-seen order:
 *   * `absorbed`  — failed at least once, then reported PASS/LEAK. The lane is
 *                   green ONLY because retries were spent. This is the number
 *                   nothing else in CI reports.
 *   * `persistent` — was retried and never recovered. The lane is red and the
 *                   existing failure summary already names it; it is carried
 *                   here so the retry ledger is complete rather than partial.
 */
export function extractRetryOutcomes(logText) {
  /** @type {Map<string, {id: string, firstSeen: number, maxAttempt: number, failedAt: number[], passedAt: number|null}>} */
  const tests = new Map();
  let ordinal = 0;

  for (const line of String(logText).split('\n')) {
    const parsed = parseStatusLine(line);
    if (parsed === null) continue;
    let record = tests.get(parsed.id);
    if (record === undefined) {
      record = { id: parsed.id, firstSeen: ordinal, maxAttempt: 0, failedAt: [], passedAt: null };
      ordinal += 1;
      tests.set(parsed.id, record);
    }
    record.maxAttempt = Math.max(record.maxAttempt, parsed.attempt);
    if (isFailureStatus(parsed.status)) {
      if (!record.failedAt.includes(parsed.attempt)) record.failedAt.push(parsed.attempt);
      continue;
    }
    if (isSuccessStatus(parsed.status)) {
      record.passedAt = record.passedAt === null
        ? parsed.attempt
        : Math.min(record.passedAt, parsed.attempt);
    }
  }

  const ordered = [...tests.values()].sort((a, b) => a.firstSeen - b.firstSeen);
  const absorbed = [];
  const persistent = [];
  for (const record of ordered) {
    if (record.failedAt.length === 0) continue;
    if (record.passedAt !== null) {
      absorbed.push({
        id: record.id,
        attempts: Math.max(record.passedAt, ...record.failedAt),
        failedAttempts: record.failedAt.length,
        passedOnAttempt: record.passedAt,
      });
      continue;
    }
    if (record.maxAttempt > 1) {
      persistent.push({
        id: record.id,
        attempts: record.maxAttempt,
        failedAttempts: record.failedAt.length,
        passedOnAttempt: null,
      });
    }
  }
  return { absorbed, persistent };
}

/**
 * Build the machine-readable per-shard report consumed by `--aggregate`.
 */
export function buildShardReport({ shardName, logPath, logText }) {
  if (typeof logText !== 'string') {
    return {
      version: REPORT_VERSION,
      shard: shardName,
      logPath,
      observed: false,
      absorbedCount: 0,
      absorbed: [],
      persistent: [],
    };
  }
  const { absorbed, persistent } = extractRetryOutcomes(logText);
  return {
    version: REPORT_VERSION,
    shard: shardName,
    logPath,
    observed: true,
    absorbedCount: absorbed.length,
    absorbed,
    persistent,
  };
}

const plural = (count, word) => `${count} ${word}${count === 1 ? '' : 's'}`;

/**
 * Render the shard-level annotation and step-summary block.
 *
 * A clean shard renders NOTHING. The step summary of a green run is read by
 * people looking for something wrong; padding it with "no flakes here" from
 * eight shards on every run is how a real signal gets tuned out.
 */
export function renderShardRetrySummary({ shardName, logPath, logText }) {
  const report = buildShardReport({ shardName, logPath, logText });
  const shard = `Server Test ${shardName}`;

  if (!report.observed) {
    return {
      report,
      annotation: `::notice title=${shard} retry telemetry::no nextest log at ${logPath}; retry-absorbed flakes could not be counted for this shard.`,
      summary: '',
    };
  }
  if (report.absorbed.length === 0 && report.persistent.length === 0) {
    return { report, annotation: '', summary: '' };
  }

  const lines = [];
  if (report.absorbed.length > 0) {
    lines.push(`### ${shard} — ${plural(report.absorbed.length, 'retry-absorbed flake')} (lane is GREEN)`);
    lines.push('');
    const subject = report.absorbed.length === 1
      ? 'This test failed and then passed on a re-run. It did'
      : 'These tests failed and then passed on a re-run. They did';
    lines.push(
      `${subject} not fail this lane only because \`[profile.ci]\` in ` +
      '`server/.config/nextest.toml` sets `retries = { count = 2 }`. ' +
      'Without retries this lane would be RED.',
    );
    lines.push('');
    lines.push('| test | attempts | failed attempts | passed on |');
    lines.push('| --- | --: | --: | --: |');
    for (const entry of report.absorbed) {
      lines.push(`| \`${entry.id}\` | ${entry.attempts} | ${entry.failedAttempts} | attempt ${entry.passedOnAttempt} |`);
    }
    lines.push('');
  }
  if (report.persistent.length > 0) {
    lines.push(`#### ${shard} — ${plural(report.persistent.length, 'test')} exhausted every retry`);
    lines.push('');
    for (const entry of report.persistent) {
      lines.push(`- \`${entry.id}\` — failed all ${entry.attempts} attempts`);
    }
    lines.push('');
  }

  const annotation = report.absorbed.length > 0
    ? `::warning title=${shard}: ${plural(report.absorbed.length, 'retry-absorbed flake')}::${report.absorbed.map((e) => `${e.id} (passed on attempt ${e.passedOnAttempt} of ${e.attempts})`).join('; ')}`
    : '';

  return { report, annotation, summary: `${lines.join('\n')}\n` };
}

/**
 * Fold the per-shard reports into the run-level count.
 *
 * The run-level number is the one that matters: a single shard reporting one
 * absorbed flake reads as noise, while "this run absorbed 3 flakes" is the
 * statistic that made 41.2% of green runs quietly flaky.
 */
export function renderRunAggregate(reports) {
  const valid = reports.filter((r) => r !== null && typeof r === 'object' && r.version === REPORT_VERSION);
  const withFlakes = valid.filter((r) => Array.isArray(r.absorbed) && r.absorbed.length > 0);
  const absorbedCount = withFlakes.reduce((sum, r) => sum + r.absorbed.length, 0);

  if (valid.length === 0) {
    return {
      absorbedCount: 0,
      shardCount: 0,
      annotation: '::notice title=Retry-absorbed flakes::no shard retry reports were available for this run.',
      summary: '',
    };
  }
  if (absorbedCount === 0) {
    return {
      absorbedCount: 0,
      shardCount: valid.length,
      annotation: '',
      summary: [
        '### Retry-absorbed flakes: none',
        '',
        `All ${plural(valid.length, 'shard')} passed without spending a retry.`,
        '',
      ].join('\n'),
    };
  }

  const lines = [
    `### Retry-absorbed flakes: ${absorbedCount} in this run`,
    '',
    `This run is GREEN, but ${plural(absorbedCount, 'test')} across ` +
    `${plural(withFlakes.length, 'shard')} passed only after a re-run. ` +
    'With `retries = { count = 2 }` removed from `[profile.ci]` ' +
    '(`server/.config/nextest.toml`) this run would have been RED.',
    '',
    '| shard | test | attempts | passed on |',
    '| --- | --- | --: | --: |',
  ];
  for (const report of withFlakes) {
    for (const entry of report.absorbed) {
      lines.push(`| ${report.shard} | \`${entry.id}\` | ${entry.attempts} | attempt ${entry.passedOnAttempt} |`);
    }
  }
  lines.push('');

  return {
    absorbedCount,
    shardCount: valid.length,
    annotation: `::warning title=Retry-absorbed flakes: ${absorbedCount} in this run::${withFlakes.map((r) => `${r.shard}: ${r.absorbed.map((e) => e.id).join(', ')}`).join('; ')}`,
    summary: `${lines.join('\n')}\n`,
  };
}

function parseArgs(argv) {
  const args = { shard: '', log: '', summary: '', json: '', aggregate: '' };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (['--shard', '--log', '--summary', '--json', '--aggregate'].includes(flag)) {
      if (value === undefined) throw new Error(`ci-retry-flake-summary: ${flag} needs a value`);
      args[flag.slice(2)] = value;
      index += 1;
      continue;
    }
    throw new Error(`ci-retry-flake-summary: unexpected argument ${JSON.stringify(flag)}`);
  }
  if (args.aggregate === '') {
    if (args.shard === '') throw new Error('ci-retry-flake-summary: --shard is required');
    if (args.log === '') throw new Error('ci-retry-flake-summary: --log is required');
  }
  return args;
}

function readLog(path) {
  try {
    return readFileSync(path, 'utf8');
  } catch {
    return undefined;
  }
}

function emit({ annotation, summary }, summaryPath) {
  if (annotation !== '') process.stdout.write(`${annotation}\n`);
  if (summary !== '' && summaryPath !== '') appendFileSync(summaryPath, summary);
}

function runShard(args) {
  const result = renderShardRetrySummary({
    shardName: args.shard,
    logPath: args.log,
    logText: readLog(args.log),
  });
  emit(result, args.summary);
  if (args.json !== '') {
    mkdirSync(dirname(args.json), { recursive: true });
    writeFileSync(args.json, `${JSON.stringify(result.report, null, 2)}\n`);
  }
}

function runAggregate(args) {
  let files = [];
  try {
    files = readdirSync(args.aggregate).filter((name) => name.endsWith('.json')).sort();
  } catch {
    files = [];
  }
  const reports = [];
  for (const name of files) {
    try {
      reports.push(JSON.parse(readFileSync(join(args.aggregate, name), 'utf8')));
    } catch {
      // A single unreadable shard report must not suppress the others.
      process.stdout.write(`::notice title=Retry telemetry::could not read ${name}; skipped.\n`);
    }
  }
  emit(renderRunAggregate(reports), args.summary);
}

function main(argv) {
  // Every failure path lands here. This step is telemetry attached to lanes
  // that already passed: it reports what it could not do and exits 0.
  try {
    const args = parseArgs(argv);
    if (args.aggregate !== '') runAggregate(args);
    else runShard(args);
  } catch (error) {
    const detail = String(error && error.message ? error.message : error).replace(/\r?\n/g, ' ');
    process.stdout.write(`::notice title=Retry telemetry unavailable::${detail}\n`);
  }
  process.exitCode = 0;
}

if (process.argv[1] !== undefined && import.meta.url === `file://${process.argv[1]}`) {
  main(process.argv.slice(2));
}
