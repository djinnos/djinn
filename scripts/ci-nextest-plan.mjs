#!/usr/bin/env node
// scripts/ci-nextest-plan.mjs
// Deterministic, timing-fed nextest shard planning engine.
// Current nextest discovery is the sole authority: timing data may only influence
// load balancing, never add, remove, or select tests.

import { readFile, writeFile, rename, mkdir } from 'node:fs/promises';
import { stdin, stderr, stdout } from 'node:process';
import { randomUUID } from 'node:crypto';
import { dirname, join, basename } from 'node:path';

export const TIMING_VERSION = 'ci-nextest-timing/v1';
export const PROOF_VERSION = 'ci-nextest-plan/v1';
export const FALLBACK_DURATION_SECONDS = 30;
export const DEFAULT_MAX_AGE_DAYS = 7;
export const PR_DEFAULT_SHARDS = 2;
export const PR_MAX_SHARDS = 4;
export const WIDE_DEFAULT_SHARDS = 4;
export const COLD_START_SHARDS = 4;
export const PR_WIDEN_TEST_THRESHOLD = 200;
export const PR_WIDEN_DURATION_THRESHOLD_SECONDS = 600;

const ID_SEP = '|';

/**
 * Build a canonical, stable test identity from package/binary/test-name context.
 * Cargo package and binary names cannot contain the separator, so the identity is
 * unambiguous even if a custom test name contains it.
 */
export function canonicalId(packageName, binaryName, testName) {
  return `${packageName}${ID_SEP}${binaryName}${ID_SEP}${testName}`;
}

function parseIsoOrNumber(value) {
  if (typeof value === 'number') return value;
  if (typeof value === 'string') {
    const n = Number(value);
    if (!Number.isNaN(n)) return n;
    const d = Date.parse(value);
    if (!Number.isNaN(d)) return d;
  }
  return null;
}

function parseNow(value) {
  if (typeof value === 'number') return value;
  const parsed = parseIsoOrNumber(value);
  if (parsed === null) throw new Error(`Invalid timestamp: ${value}`);
  return parsed;
}

function median(values) {
  if (values.length === 0) return null;
  const sorted = values.slice().sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  if (sorted.length % 2 === 1) return sorted[mid];
  return (sorted[mid - 1] + sorted[mid]) / 2;
}

/**
 * Parse the JSON emitted by `cargo nextest list --message-format json` into a
 * stable, sorted list of discovered tests. Only tests that match the discovery
 * filter (`filter-match.status === 'matches'`) are returned.
 *
 * Throws if a required identity field is missing or if two tests normalize to
 * the same identity.
 */
export function parseDiscovery(json) {
  let summary;
  try {
    summary = JSON.parse(json);
  } catch (err) {
    throw new Error(`Invalid discovery JSON: ${err.message}`);
  }
  if (!summary || typeof summary !== 'object') {
    throw new Error('Discovery JSON must be an object');
  }
  const suites = summary['rust-suites'];
  if (!suites || typeof suites !== 'object' || Array.isArray(suites)) {
    throw new Error('Discovery JSON missing rust-suites map');
  }

  const tests = [];
  const seen = new Set();

  for (const [binaryId, suite] of Object.entries(suites)) {
    if (!suite || typeof suite !== 'object') continue;
    const status = suite.status;
    if (status !== undefined && status !== 'listed') continue;

    const packageName = suite['package-name'];
    const binaryName = suite['binary-name'];
    if (typeof packageName !== 'string' || packageName.length === 0) {
      throw new Error(`Malformed suite ${binaryId}: missing or invalid package-name`);
    }
    if (typeof binaryName !== 'string' || binaryName.length === 0) {
      throw new Error(`Malformed suite ${binaryId}: missing or invalid binary-name`);
    }

    const testCases = suite.testcases;
    if (!testCases || typeof testCases !== 'object') continue;

    for (const [testName, testCase] of Object.entries(testCases)) {
      if (!testCase || typeof testCase !== 'object') continue;
      const filterMatch = testCase['filter-match'];
      if (filterMatch && filterMatch.status !== 'matches') continue;

      const id = canonicalId(packageName, binaryName, testName);
      if (seen.has(id)) {
        throw new Error(`Duplicate discovered test identity: ${id}`);
      }
      seen.add(id);
      tests.push({
        id,
        binaryId,
        packageName,
        binaryName,
        testName,
      });
    }
  }

  return tests.sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

/**
 * Load and validate a timing data JSON document.
 *
 * Returns `{ valid, reason, timings }` where `timings` is a Map from canonical
 * test id to duration in seconds. Timing entries whose ids are not present in
 * the current discovery are discarded as deleted samples.
 */
export function loadTiming(json, {
  discoveredIds = new Set(),
  now = Date.now(),
  maxAgeMs = DEFAULT_MAX_AGE_DAYS * 24 * 60 * 60 * 1000,
} = {}) {
  let summary;
  try {
    summary = JSON.parse(json);
  } catch (err) {
    return { valid: false, reason: `Invalid timing JSON: ${err.message}`, timings: new Map() };
  }
  if (!summary || typeof summary !== 'object') {
    return { valid: false, reason: 'Timing JSON must be an object', timings: new Map() };
  }
  if (summary.version !== TIMING_VERSION) {
    return {
      valid: false,
      reason: `Timing version mismatch: expected ${TIMING_VERSION}, got ${summary.version}`,
      timings: new Map(),
    };
  }
  const generatedAt = parseIsoOrNumber(summary['generated_at']);
  if (generatedAt === null || Number.isNaN(generatedAt)) {
    return { valid: false, reason: 'Timing missing generated_at', timings: new Map() };
  }
  if (now - generatedAt > maxAgeMs) {
    return { valid: false, reason: 'Timing data is stale', timings: new Map() };
  }
  const rawTests = summary.tests;
  if (!rawTests || typeof rawTests !== 'object' || Array.isArray(rawTests)) {
    return { valid: false, reason: 'Timing missing tests map', timings: new Map() };
  }

  const timings = new Map();
  for (const [id, duration] of Object.entries(rawTests)) {
    if (!discoveredIds.has(id)) continue; // deleted timing entry
    if (typeof duration !== 'number' || Number.isNaN(duration) || duration < 0) continue;
    timings.set(id, duration);
  }
  return { valid: true, reason: null, timings };
}

/**
 * Estimate a test's duration in seconds.
 *
 * 1. Exact timing sample if present.
 * 2. Median of timed tests in the same package.
 * 3. Median of all timed tests.
 * 4. Deterministic constant fallback.
 */
export function estimateDuration(test, { tests, timings }) {
  if (timings.has(test.id)) return timings.get(test.id);

  const packageDurations = tests
    .filter((t) => t.packageName === test.packageName && timings.has(t.id))
    .map((t) => timings.get(t.id));
  const packageMedian = median(packageDurations);
  if (packageMedian !== null) return packageMedian;

  const globalDurations = tests
    .filter((t) => timings.has(t.id))
    .map((t) => timings.get(t.id));
  const globalMedian = median(globalDurations);
  if (globalMedian !== null) return globalMedian;

  return FALLBACK_DURATION_SECONDS;
}

function pickShardCount({ tests, timings, profile, prWidenThreshold }) {
  if (timings.size === 0) return COLD_START_SHARDS;
  if (profile === 'merge-group' || profile === 'full-validation') return WIDE_DEFAULT_SHARDS;
  if (profile !== 'pull-request') {
    throw new Error(`Unknown profile: ${profile}`);
  }
  const totalDuration = tests.reduce(
    (sum, t) => sum + estimateDuration(t, { tests, timings }),
    0,
  );
  const widenByCount = tests.length >= prWidenThreshold.tests;
  const widenByDuration = totalDuration >= prWidenThreshold.duration;
  return widenByCount || widenByDuration ? PR_MAX_SHARDS : PR_DEFAULT_SHARDS;
}

function escapeFilterLiteral(value) {
  // Rust identifiers and nextest binary IDs are safe, but defensively handle
  // characters that would terminate or alter the exact-match syntax.
  if (value.includes(')') || value.includes('=')) {
    const escaped = value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    return `/^${escaped}$/`;
  }
  return `=${value}`;
}

/**
 * Build a nextest --filter-expr expression that matches exactly the tests in a
 * shard. Tests are grouped by binary ID to keep the expression compact and to
 * avoid ambiguous substring matches. Empty shards produce an expression that
 * matches no tests.
 */
export function buildFilterExpression(shard) {
  if (!shard.tests || shard.tests.length === 0) {
    return 'not (binary_id(/./))';
  }
  const byBinary = new Map();
  for (const test of shard.tests) {
    if (!byBinary.has(test.binaryId)) {
      byBinary.set(test.binaryId, []);
    }
    byBinary.get(test.binaryId).push(test.testName);
  }

  const binaryFilters = [];
  for (const [binaryId, names] of byBinary) {
    const binaryExpr = `binary_id(${escapeFilterLiteral(binaryId)})`;
    const testExprs = names
      .map((name) => `test(${escapeFilterLiteral(name)})`)
      .join(' | ');
    binaryFilters.push(
      names.length === 1 ? `${binaryExpr} & ${testExprs}` : `${binaryExpr} & (${testExprs})`,
    );
  }

  if (binaryFilters.length === 1) {
    return binaryFilters[0];
  }
  return `(${binaryFilters.join(') | (')})`;
}

/**
 * Produce a deterministic shard plan.
 *
 * Tests are sorted by estimated duration descending, then by stable id ascending,
 * and placed on the least-loaded shard (ties broken by lowest shard index).
 */
export function planTests({
  tests,
  timings = new Map(),
  profile = 'pull-request',
  event = null,
  prWidenThreshold = {
    tests: PR_WIDEN_TEST_THRESHOLD,
    duration: PR_WIDEN_DURATION_THRESHOLD_SECONDS,
  },
  generatedAt = new Date().toISOString(),
} = {}) {
  if (!Array.isArray(tests)) {
    throw new Error('tests must be an array');
  }

  const coldStart = timings.size === 0;
  const shardCount = pickShardCount({ tests, timings, profile, prWidenThreshold });

  const estimated = tests.map((t) => ({
    ...t,
    duration: estimateDuration(t, { tests, timings }),
  }));

  const sorted = estimated.slice().sort((a, b) => {
    if (a.duration !== b.duration) return b.duration - a.duration;
    return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
  });

  const shards = Array.from({ length: shardCount }, (_, index) => ({
    index,
    totalDuration: 0,
    tests: [],
  }));

  for (const test of sorted) {
    let target = 0;
    for (let i = 1; i < shards.length; i++) {
      const current = shards[target].totalDuration;
      const candidate = shards[i].totalDuration;
      if (candidate < current || (candidate === current && i < target)) {
        target = i;
      }
    }
    shards[target].tests.push(test);
    shards[target].totalDuration += test.duration;
  }

  const assignments = [];
  for (const shard of shards) {
    for (const test of shard.tests) {
      assignments.push({
        id: test.id,
        binaryId: test.binaryId,
        packageName: test.packageName,
        binaryName: test.binaryName,
        testName: test.testName,
        duration: test.duration,
        shardIndex: shard.index,
      });
    }
  }

  const assignedIds = assignments.map((a) => a.id);
  const uniqueIds = new Set(assignedIds);
  const discoveredIds = tests.map((t) => t.id);

  const proof = {
    version: PROOF_VERSION,
    generatedAt,
    event,
    profile,
    discoveredCount: tests.length,
    assignedCount: assignments.length,
    uniqueAssignedCount: uniqueIds.size,
    exactOnce: tests.length === assignments.length && assignments.length === uniqueIds.size,
    discoveredIds: discoveredIds.slice().sort(),
    assignedIds: assignedIds.slice().sort(),
    shardCount,
    coldStart,
    timingUsed: !coldStart,
  };

  const matrix = shards.map((shard) => ({
    shard: `shard-${shard.index + 1}`,
    shardIndex: shard.index,
    count: shard.tests.length,
    duration: shard.totalDuration,
    testIds: shard.tests.map((t) => t.id),
    filter: buildFilterExpression(shard),
  }));

  return {
    version: PROOF_VERSION,
    generatedAt,
    event,
    profile,
    shardCount,
    coldStart,
    assignments,
    shards,
    proof,
    matrix,
  };
}

/**
 * Throw if the plan does not assign every discovered test exactly once.
 */
export function validateExactOnce(plan) {
  if (!plan || typeof plan !== 'object') {
    throw new Error('Invalid plan object');
  }
  const ids = plan.assignments.map((a) => a.id);
  const unique = new Set(ids);
  if (ids.length !== unique.size) {
    throw new Error('Plan contains duplicate assignments');
  }
  if (ids.length !== plan.proof.discoveredCount) {
    throw new Error('Plan does not assign every discovered test exactly once');
  }
  return true;
}

async function readInput(path) {
  if (!path || path === '-') {
    const chunks = [];
    for await (const chunk of stdin) chunks.push(chunk);
    return Buffer.concat(chunks).toString('utf8');
  }
  return readFile(path, 'utf8');
}

async function atomicWriteFile(filePath, data) {
  const dir = dirname(filePath);
  await mkdir(dir, { recursive: true });
  const tmp = join(dir, `.${basename(filePath)}.${randomUUID()}.tmp`);
  await writeFile(tmp, data);
  await rename(tmp, filePath);
}

function requireArg(args, i, flag) {
  if (i >= args.length) {
    throw new Error(`Missing argument for ${flag}`);
  }
  return args[i];
}

export function parseArgs(argv) {
  const args = argv.slice(2);
  const options = {
    profile: 'pull-request',
    event: null,
    maxAgeDays: DEFAULT_MAX_AGE_DAYS,
    now: Date.now(),
    fullValidation: false,
  };

  for (let i = 0; i < args.length; i++) {
    switch (args[i]) {
      case '--discovery':
      case '-d':
        options.discoveryPath = requireArg(args, ++i, '--discovery');
        break;
      case '--timing':
      case '-t':
        options.timingPath = requireArg(args, ++i, '--timing');
        break;
      case '--profile':
      case '-p':
        options.profile = requireArg(args, ++i, '--profile');
        break;
      case '--event':
      case '-e':
        options.event = requireArg(args, ++i, '--event');
        break;
      case '--max-age-days':
        options.maxAgeDays = Number(requireArg(args, ++i, '--max-age-days'));
        break;
      case '--now':
        options.now = parseNow(requireArg(args, ++i, '--now'));
        break;
      case '--output':
      case '-o':
        options.outputPath = requireArg(args, ++i, '--output');
        break;
      case '--matrix':
      case '-m':
        options.matrixPath = requireArg(args, ++i, '--matrix');
        break;
      case '--proof':
        options.proofPath = requireArg(args, ++i, '--proof');
        break;
      case '--full-validation':
        options.fullValidation = true;
        break;
      case '--help':
      case '-h':
        options.help = true;
        break;
      default:
        throw new Error(`Unknown argument: ${args[i]}`);
    }
  }

  if (options.fullValidation) {
    options.profile = 'full-validation';
  }
  if (Number.isNaN(options.maxAgeDays)) {
    throw new Error('Invalid --max-age-days');
  }
  if (Number.isNaN(options.now)) {
    throw new Error('Invalid --now');
  }

  return options;
}

export function formatHelp() {
  return `Usage: ci-nextest-plan [options]

Options:
  -d, --discovery <path>     Path to nextest discovery JSON (default: stdin)
  -t, --timing <path>        Path to optional timing JSON (default: none)
  -p, --profile <name>       pull-request | merge-group | full-validation
      --full-validation      Shorthand for --profile full-validation
  -e, --event <name>         Event name stored in matrix/proof metadata
      --max-age-days <n>     Timing freshness window (default: 7)
      --now <timestamp>      Reference timestamp for freshness checks
  -o, --output <path>        Write full plan JSON
  -m, --matrix <path>        Write machine-readable shard matrix JSON
      --proof <path>         Write exact-once proof JSON
  -h, --help                 Show this help
`;
}

export async function main(argv = process.argv) {
  const options = parseArgs(argv);
  if (options.help) {
    stdout.write(formatHelp());
    return;
  }

  const discoveryText = await readInput(options.discoveryPath);
  const tests = parseDiscovery(discoveryText);
  const discoveredIds = new Set(tests.map((t) => t.id));

  let timingResult = { valid: false, timings: new Map() };
  if (options.timingPath) {
    const timingText = await readInput(options.timingPath);
    const maxAgeMs = options.maxAgeDays !== undefined
      ? options.maxAgeDays * 24 * 60 * 60 * 1000
      : undefined;
    timingResult = loadTiming(timingText, {
      discoveredIds,
      now: options.now,
      maxAgeMs,
    });
  }

  const generatedAt = new Date(options.now).toISOString();
  const plan = planTests({
    tests,
    timings: timingResult.timings,
    profile: options.profile,
    event: options.event,
    generatedAt,
  });
  validateExactOnce(plan);

  const planJson = JSON.stringify(plan, null, 2);
  const matrixArtifact = {
    version: PROOF_VERSION,
    generatedAt,
    event: options.event,
    profile: options.profile,
    shardCount: plan.shardCount,
    shards: plan.matrix,
  };
  const matrixJson = JSON.stringify(matrixArtifact, null, 2);
  const proofJson = JSON.stringify(plan.proof, null, 2);

  const writes = [];
  if (options.outputPath) {
    writes.push(atomicWriteFile(options.outputPath, planJson));
  }
  if (options.matrixPath) {
    writes.push(atomicWriteFile(options.matrixPath, matrixJson));
  }
  if (options.proofPath) {
    writes.push(atomicWriteFile(options.proofPath, proofJson));
  }
  await Promise.all(writes);

  if (!options.outputPath) {
    stdout.write(planJson);
  }
}

if (process.argv[1] === import.meta.url.replace('file://', '')) {
  main(process.argv).catch((err) => {
    stderr.write(`ci-nextest-plan: ${err.message}\n`);
    process.exit(1);
  });
}
