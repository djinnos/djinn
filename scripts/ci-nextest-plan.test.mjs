// scripts/ci-nextest-plan.test.mjs
// Invariant tests for the timing-fed nextest planning engine.

import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { readFile, writeFile, mkdtemp, rm, access } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  canonicalId,
  parseDiscovery,
  loadTiming,
  estimateDuration,
  planTests,
  validateExactOnce,
  buildFilterExpression,
  parseArgs,
  TIMING_VERSION,
  PROOF_VERSION,
  FALLBACK_DURATION_SECONDS,
  DEFAULT_MAX_AGE_DAYS,
  PR_DEFAULT_SHARDS,
  PR_MAX_SHARDS,
  WIDE_DEFAULT_SHARDS,
  COLD_START_SHARDS,
  PR_WIDEN_TEST_THRESHOLD,
  PR_WIDEN_DURATION_THRESHOLD_SECONDS,
} from './ci-nextest-plan.mjs';

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const SCRIPT = join(__dirname, 'ci-nextest-plan.mjs');
const NEXTEST_TOML = join(__dirname, '..', 'server', '.config', 'nextest.toml');

function makeSummary(overrides = {}) {
  return {
    'test-count': 0,
    'rust-suites': {},
    ...overrides,
  };
}

function suite(packageName, binaryName, testCases, { binaryId = 'pkg::bin', status = 'listed' } = {}) {
  return {
    'package-name': packageName,
    'binary-name': binaryName,
    'binary-id': binaryId,
    status,
    testcases: testCases,
  };
}

function testCase({ matches = true, ignored = false } = {}) {
  return {
    'filter-match': matches ? { status: 'matches' } : { status: 'mismatch', reason: 'string' },
    ignored,
  };
}

function makeTiming(overrides = {}) {
  return {
    version: TIMING_VERSION,
    generated_at: Date.now(),
    tests: {},
    ...overrides,
  };
}

function ids(plan) {
  return plan.assignments.map((a) => a.id);
}

function durations(plan) {
  return plan.assignments.map((a) => a.duration);
}

function timingMap(tests, duration = 10) {
  const map = new Map();
  for (const t of tests) map.set(t.id, duration);
  return map;
}

async function makeTempDir() {
  return mkdtemp('/var/tmp/ci-nextest-plan-');
}

async function pathExists(path) {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

function runCli(args) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [SCRIPT, ...args], {
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (d) => { stdout += d; });
    child.stderr.on('data', (d) => { stderr += d; });
    child.on('error', reject);
    child.on('close', (code) => {
      resolve({ code, stdout, stderr });
    });
  });
}

async function runPlan(args) {
  const result = await runCli(args);
  if (result.code !== 0) {
    throw new Error(`CLI exited ${result.code}: ${result.stderr}`);
  }
  return JSON.parse(result.stdout);
}

function parseToml(text) {
  const sections = {};
  let current = null;
  for (let line of text.split('\n')) {
    const comment = line.indexOf('#');
    if (comment >= 0) line = line.slice(0, comment);
    line = line.trim();
    if (!line) continue;
    const sectionMatch = line.match(/^\[([^\]]+)\]$/);
    if (sectionMatch) {
      current = sectionMatch[1];
      sections[current] = {};
      continue;
    }
    if (current) {
      const eq = line.indexOf('=');
      if (eq >= 0) {
        const key = line.slice(0, eq).trim();
        const value = line.slice(eq + 1).trim();
        sections[current][key] = value;
      }
    }
  }
  return sections;
}

const SUMMARY_ONE = makeSummary({
  'rust-suites': {
    'pkg::bin': suite('pkg', 'bin', {
      'test_one': testCase(),
      'test_two': testCase(),
    }),
  },
});

const SUMMARY_MANY = makeSummary({
  'rust-suites': {
    'pkg::bin': suite('pkg', 'bin', {
      'fast': testCase(),
      'slow': testCase(),
    }),
    'other::it': suite('other', 'it', {
      'alpha': testCase(),
      'beta': testCase(),
    }),
  },
});

describe('canonicalId', () => {
  it('is stable and reversible', () => {
    const id = canonicalId('pkg', 'bin', 'test_one');
    assert.equal(id, 'pkg|bin|test_one');
    const [p, b, t] = id.split('|');
    assert.deepEqual([p, b, t], ['pkg', 'bin', 'test_one']);
  });
});

describe('parseDiscovery', () => {
  it('extracts stable identities from a nextest summary', () => {
    const tests = parseDiscovery(JSON.stringify(SUMMARY_ONE));
    assert.equal(tests.length, 2);
    assert.equal(tests[0].id, 'pkg|bin|test_one');
    assert.equal(tests[1].id, 'pkg|bin|test_two');
    assert.equal(tests[0].binaryId, 'pkg::bin');
    assert.equal(tests[0].packageName, 'pkg');
  });

  it('ignores tests that do not match the discovery filter', () => {
    const summary = makeSummary({
      'rust-suites': {
        'pkg::bin': suite('pkg', 'bin', {
          'included': testCase(),
          'excluded': testCase({ matches: false }),
        }),
      },
    });
    const tests = parseDiscovery(JSON.stringify(summary));
    assert.equal(tests.length, 1);
    assert.equal(tests[0].testName, 'included');
  });

  it('ignores non-listed suites', () => {
    const summary = makeSummary({
      'rust-suites': {
        'pkg::bin': suite('pkg', 'bin', { only: testCase() }, { status: 'skipped' }),
      },
    });
    const tests = parseDiscovery(JSON.stringify(summary));
    assert.equal(tests.length, 0);
  });

  it('fails safely on malformed discovery identities', () => {
    const summary = makeSummary({
      'rust-suites': {
        'pkg::bin': {
          'package-name': '',
          'binary-name': 'bin',
          testcases: { only: testCase() },
        },
      },
    });
    assert.throws(() => parseDiscovery(JSON.stringify(summary)), /missing or invalid package-name/);
  });

  it('fails safely on duplicate discovered identities', () => {
    const summary = makeSummary({
      'rust-suites': {
        'pkg::bin': suite('pkg', 'bin', { dup: testCase() }),
        'pkg::bin2': suite('pkg', 'bin', { dup: testCase() }, { binaryId: 'pkg::bin2' }),
      },
    });
    assert.throws(() => parseDiscovery(JSON.stringify(summary)), /Duplicate discovered test identity/);
  });

  it('returns a deterministic order independent of input ordering', () => {
    const a = parseDiscovery(JSON.stringify(SUMMARY_MANY));
    const summary = makeSummary({
      'rust-suites': {
        'other::it': suite('other', 'it', {
          'beta': testCase(),
          'alpha': testCase(),
        }),
        'pkg::bin': suite('pkg', 'bin', {
          'slow': testCase(),
          'fast': testCase(),
        }),
      },
    });
    const b = parseDiscovery(JSON.stringify(summary));
    assert.deepEqual(a.map((t) => t.id), b.map((t) => t.id));
  });
});

describe('loadTiming', () => {
  const tests = [
    { id: 'pkg|bin|fast', packageName: 'pkg' },
    { id: 'other|it|alpha', packageName: 'other' },
  ];
  const discoveredIds = new Set(tests.map((t) => t.id));
  const now = Date.now();

  it('accepts fresh, compatible timing', () => {
    const timing = makeTiming({
      generated_at: now,
      tests: { 'pkg|bin|fast': 10, 'other|it|alpha': 20 },
    });
    const result = loadTiming(JSON.stringify(timing), { discoveredIds, now });
    assert.equal(result.valid, true);
    assert.equal(result.timings.get('pkg|bin|fast'), 10);
  });

  it('discards deleted timing entries', () => {
    const timing = makeTiming({
      generated_at: now,
      tests: {
        'pkg|bin|fast': 10,
        'deleted|bin|gone': 99,
      },
    });
    const result = loadTiming(JSON.stringify(timing), { discoveredIds, now });
    assert.equal(result.valid, true);
    assert.equal(result.timings.has('deleted|bin|gone'), false);
    assert.equal(result.timings.size, 1);
  });

  it('rejects stale timing data', () => {
    const timing = makeTiming({
      generated_at: now - 10 * 24 * 60 * 60 * 1000,
      tests: { 'pkg|bin|fast': 10 },
    });
    const result = loadTiming(JSON.stringify(timing), { discoveredIds, now });
    assert.equal(result.valid, false);
    assert.equal(result.reason, 'Timing data is stale');
  });

  it('rejects incompatible timing version', () => {
    const timing = makeTiming({ version: 'legacy/v0', tests: { 'pkg|bin|fast': 10 } });
    const result = loadTiming(JSON.stringify(timing), { discoveredIds, now });
    assert.equal(result.valid, false);
    assert.match(result.reason, /version mismatch/);
  });

  it('rejects missing generated_at', () => {
    const timing = makeTiming({ generated_at: undefined, tests: { 'pkg|bin|fast': 10 } });
    const result = loadTiming(JSON.stringify(timing), { discoveredIds, now });
    assert.equal(result.valid, false);
    assert.match(result.reason, /generated_at/);
  });
});

describe('estimateDuration', () => {
  it('uses exact timing when available', () => {
    const tests = [{ id: 'pkg|bin|a', packageName: 'pkg' }];
    const timings = new Map([['pkg|bin|a', 42]]);
    assert.equal(estimateDuration(tests[0], { tests, timings }), 42);
  });

  it('falls back to package median for new tests', () => {
    const tests = [
      { id: 'pkg|bin|a', packageName: 'pkg' },
      { id: 'pkg|bin|b', packageName: 'pkg' },
      { id: 'pkg|bin|c', packageName: 'pkg' },
    ];
    const timings = new Map([
      ['pkg|bin|a', 10],
      ['pkg|bin|b', 20],
    ]);
    assert.equal(estimateDuration(tests[2], { tests, timings }), 15);
  });

  it('falls back to global median when package median unavailable', () => {
    const tests = [
      { id: 'pkg|bin|a', packageName: 'pkg' },
      { id: 'other|bin|b', packageName: 'other' },
    ];
    const timings = new Map([['pkg|bin|a', 10]]);
    assert.equal(estimateDuration(tests[1], { tests, timings }), 10);
  });

  it('uses deterministic constant fallback when no timings exist', () => {
    const tests = [{ id: 'pkg|bin|a', packageName: 'pkg' }];
    const timings = new Map();
    assert.equal(estimateDuration(tests[0], { tests, timings }), FALLBACK_DURATION_SECONDS);
  });
});

describe('buildFilterExpression', () => {
  it('produces an empty match for a shard with no tests', () => {
    const expr = buildFilterExpression({ tests: [] });
    assert.equal(expr, 'not (binary_id(/./))');
  });

  it('matches a single test exactly', () => {
    const expr = buildFilterExpression({
      tests: [{ binaryId: 'pkg::bin', testName: 'test_one' }],
    });
    assert.equal(expr, 'binary_id(=pkg::bin) & test(=test_one)');
  });

  it('groups multiple tests by binary ID', () => {
    const expr = buildFilterExpression({
      tests: [
        { binaryId: 'pkg::bin', testName: 'test_one' },
        { binaryId: 'pkg::bin', testName: 'test_two' },
      ],
    });
    assert.equal(expr, 'binary_id(=pkg::bin) & (test(=test_one) | test(=test_two))');
  });

  it('joins multiple binaries with union', () => {
    const expr = buildFilterExpression({
      tests: [
        { binaryId: 'pkg::bin', testName: 'a' },
        { binaryId: 'other::it', testName: 'b' },
      ],
    });
    assert.equal(expr, '(binary_id(=pkg::bin) & test(=a)) | (binary_id(=other::it) & test(=b))');
  });
});

describe('parseArgs', () => {
  it('uses defaults for an empty argument list', () => {
    const opts = parseArgs(['node', 'script']);
    assert.equal(opts.profile, 'pull-request');
    assert.equal(opts.event, null);
    assert.equal(opts.maxAgeDays, DEFAULT_MAX_AGE_DAYS);
    assert.equal(typeof opts.now, 'number');
    assert.equal(opts.fullValidation, false);
    assert.equal(opts.help, undefined);
  });

  it('accepts discovery, timing, matrix, proof, and event paths', () => {
    const opts = parseArgs([
      'node', 'script',
      '--discovery', 'd.json',
      '--timing', 't.json',
      '--matrix', 'm.json',
      '--proof', 'p.json',
      '--event', 'pull_request',
    ]);
    assert.equal(opts.discoveryPath, 'd.json');
    assert.equal(opts.timingPath, 't.json');
    assert.equal(opts.matrixPath, 'm.json');
    assert.equal(opts.proofPath, 'p.json');
    assert.equal(opts.event, 'pull_request');
  });

  it('overrides profile with --full-validation', () => {
    const opts = parseArgs(['node', 'script', '--profile', 'pull-request', '--full-validation']);
    assert.equal(opts.profile, 'full-validation');
  });

  it('accepts a numeric or ISO --now', () => {
    const numeric = parseArgs(['node', 'script', '--now', '1700000000000']);
    assert.equal(numeric.now, 1700000000000);
    const iso = parseArgs(['node', 'script', '--now', '2026-01-01T00:00:00.000Z']);
    assert.equal(iso.now, Date.parse('2026-01-01T00:00:00.000Z'));
  });

  it('rejects unknown arguments', () => {
    assert.throws(() => parseArgs(['node', 'script', '--bogus']), /Unknown argument: --bogus/);
  });

  it('rejects missing option values', () => {
    assert.throws(() => parseArgs(['node', 'script', '--discovery']), /Missing argument for --discovery/);
  });
});

describe('planTests', () => {
  it('assigns every discovered test exactly once', () => {
    const tests = parseDiscovery(JSON.stringify(SUMMARY_MANY));
    const plan = planTests({ tests, timings: new Map() });
    validateExactOnce(plan);
    assert.equal(plan.proof.exactOnce, true);
    assert.equal(plan.shardCount, COLD_START_SHARDS);
  });

  it('enumerates discovered and assigned IDs in the proof', () => {
    const tests = parseDiscovery(JSON.stringify(SUMMARY_MANY));
    const plan = planTests({ tests, timings: new Map() });
    const expectedIds = tests.map((t) => t.id).sort();
    assert.deepEqual(plan.proof.discoveredIds, expectedIds);
    assert.deepEqual(plan.proof.assignedIds, expectedIds);
    assert.equal(plan.proof.version, PROOF_VERSION);
  });

  it('plans compact PR with two shards when timing is fresh', () => {
    const tests = parseDiscovery(JSON.stringify(SUMMARY_ONE));
    const timings = timingMap(tests, 30);
    const plan = planTests({ tests, timings, profile: 'pull-request' });
    assert.equal(plan.shardCount, PR_DEFAULT_SHARDS);
    assert.equal(plan.coldStart, false);
  });

  it('widens PR to four shards at the test-count threshold with fresh timing', () => {
    const testCases = {};
    for (let i = 0; i < PR_WIDEN_TEST_THRESHOLD; i++) {
      testCases[`t_${i}`] = testCase();
    }
    const summary = makeSummary({ 'rust-suites': { 'pkg::bin': suite('pkg', 'bin', testCases) } });
    const tests = parseDiscovery(JSON.stringify(summary));
    const timings = timingMap(tests, 10);
    const plan = planTests({ tests, timings, profile: 'pull-request' });
    assert.equal(plan.shardCount, PR_MAX_SHARDS);
    assert.equal(plan.coldStart, false);
  });

  it('widens PR to four shards at the duration threshold with fresh timing', () => {
    const testCases = {};
    const target = Math.ceil(PR_WIDEN_DURATION_THRESHOLD_SECONDS / 100) + 1;
    for (let i = 0; i < target; i++) {
      testCases[`t_${i}`] = testCase();
    }
    const summary = makeSummary({ 'rust-suites': { 'pkg::bin': suite('pkg', 'bin', testCases) } });
    const tests = parseDiscovery(JSON.stringify(summary));
    const timings = timingMap(tests, 100); // 100s per test, so total exceeds threshold
    const plan = planTests({ tests, timings, profile: 'pull-request' });
    assert.equal(plan.shardCount, PR_MAX_SHARDS);
    assert.equal(plan.coldStart, false);
  });

  it('keeps merge-group and full-validation wide at four shards', () => {
    const tests = parseDiscovery(JSON.stringify(SUMMARY_ONE));
    const timings = timingMap(tests, 10);
    const mg = planTests({ tests, timings, profile: 'merge-group' });
    const fv = planTests({ tests, timings, profile: 'full-validation' });
    assert.equal(mg.shardCount, WIDE_DEFAULT_SHARDS);
    assert.equal(fv.shardCount, WIDE_DEFAULT_SHARDS);
    assert.equal(mg.coldStart, false);
    assert.equal(fv.coldStart, false);
  });

  it('uses timing for balancing and produces a non-cold-start plan', () => {
    const summary = makeSummary({
      'rust-suites': {
        'pkg::bin': suite('pkg', 'bin', {
          'fast': testCase(),
          'slow': testCase(),
        }),
      },
    });
    const tests = parseDiscovery(JSON.stringify(summary));
    const timing = makeTiming({
      tests: { 'pkg|bin|fast': 10, 'pkg|bin|slow': 100 },
    });
    const discoveredIds = new Set(tests.map((t) => t.id));
    const { timings } = loadTiming(JSON.stringify(timing), { discoveredIds, now: Date.now() });
    const plan = planTests({ tests, timings, profile: 'pull-request' });
    assert.equal(plan.coldStart, false);
    assert.equal(plan.shardCount, PR_DEFAULT_SHARDS);
    const slow = plan.assignments.find((a) => a.testName === 'slow');
    const fast = plan.assignments.find((a) => a.testName === 'fast');
    assert.equal(slow.duration, 100);
    assert.equal(fast.duration, 10);
    validateExactOnce(plan);
  });

  it('balances deterministically independent of input order', () => {
    const summaryA = makeSummary({
      'rust-suites': {
        'a::bin': suite('a', 'bin', { x: testCase(), y: testCase() }),
        'b::bin': suite('b', 'bin', { z: testCase() }),
      },
    });
    const summaryB = makeSummary({
      'rust-suites': {
        'b::bin': suite('b', 'bin', { z: testCase() }),
        'a::bin': suite('a', 'bin', { y: testCase(), x: testCase() }),
      },
    });
    const testsA = parseDiscovery(JSON.stringify(summaryA));
    const testsB = parseDiscovery(JSON.stringify(summaryB));
    const timingsA = timingMap(testsA, 10);
    const timingsB = timingMap(testsB, 10);
    const planA = planTests({ tests: testsA, timings: timingsA, profile: 'pull-request' });
    const planB = planTests({ tests: testsB, timings: timingsB, profile: 'pull-request' });
    assert.deepEqual(ids(planA), ids(planB));
    assert.deepEqual(durations(planA), durations(planB));
    assert.deepEqual(planA.assignments.map((a) => a.shardIndex), planB.assignments.map((a) => a.shardIndex));
  });

  it('never drops a test when timing is absent, stale, or incompatible', () => {
    const summary = makeSummary({
      'rust-suites': {
        'pkg::bin': suite('pkg', 'bin', {
          a: testCase(),
          b: testCase(),
        }),
      },
    });
    const tests = parseDiscovery(JSON.stringify(summary));
    const discoveredIds = new Set(tests.map((t) => t.id));
    const now = Date.now();

    const absent = new Map();

    const staleTiming = makeTiming({
      generated_at: now - 30 * 24 * 60 * 60 * 1000,
      tests: { 'pkg|bin|a': 10 },
    });
    const stale = loadTiming(JSON.stringify(staleTiming), { discoveredIds, now }).timings;

    const incompatibleTiming = makeTiming({
      version: 'legacy/v0',
      tests: { 'pkg|bin|a': 10 },
    });
    const incompatible = loadTiming(JSON.stringify(incompatibleTiming), { discoveredIds, now }).timings;

    const cases = [
      { name: 'absent', timings: absent },
      { name: 'stale', timings: stale },
      { name: 'incompatible', timings: incompatible },
    ];
    for (const c of cases) {
      const plan = planTests({ tests, timings: c.timings, profile: 'pull-request' });
      validateExactOnce(plan);
      assert.equal(plan.proof.discoveredCount, 2);
      assert.equal(plan.proof.assignedCount, 2);
      assert.equal(plan.coldStart, true);
      assert.equal(plan.shardCount, COLD_START_SHARDS);
    }
  });
});

describe('CLI end-to-end', () => {
  it('writes matrix and proof artifacts', async () => {
    const dir = await makeTempDir();
    try {
      const discovery = join(dir, 'discovery.json');
      const timing = join(dir, 'timing.json');
      const matrixPath = join(dir, 'matrix.json');
      const proofPath = join(dir, 'proof.json');
      await writeFile(discovery, JSON.stringify(SUMMARY_MANY));
      await writeFile(timing, JSON.stringify(makeTiming({
        generated_at: Date.now(),
        tests: { 'pkg|bin|fast': 10, 'pkg|bin|slow': 100, 'other|it|alpha': 20, 'other|it|beta': 20 },
      })));

      const plan = await runPlan([
        '--discovery', discovery,
        '--timing', timing,
        '--profile', 'pull-request',
        '--event', 'pull_request',
        '--matrix', matrixPath,
        '--proof', proofPath,
        '--now', Date.now().toString(),
      ]);
      assert.equal(plan.profile, 'pull-request');
      assert.equal(plan.event, 'pull_request');

      const matrix = JSON.parse(await readFile(matrixPath, 'utf8'));
      assert.equal(matrix.version, PROOF_VERSION);
      assert.equal(matrix.profile, 'pull-request');
      assert.equal(matrix.event, 'pull_request');
      assert.equal(matrix.shardCount, plan.shardCount);
      assert.equal(Array.isArray(matrix.shards), true);
      assert.equal(matrix.shards.length, plan.shardCount);

      const proof = JSON.parse(await readFile(proofPath, 'utf8'));
      assert.equal(proof.version, PROOF_VERSION);
      assert.equal(proof.profile, 'pull-request');
      assert.equal(proof.event, 'pull_request');
      assert.equal(proof.exactOnce, true);
      assert.deepEqual(proof.discoveredIds, plan.proof.discoveredIds);
      assert.deepEqual(proof.assignedIds, plan.proof.assignedIds);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it('exits nonzero on malformed discovery without writing artifacts', async () => {
    const dir = await makeTempDir();
    try {
      const discovery = join(dir, 'discovery.json');
      const matrixPath = join(dir, 'matrix.json');
      const proofPath = join(dir, 'proof.json');
      await writeFile(discovery, 'not valid json');

      const result = await runCli([
        '--discovery', discovery,
        '--matrix', matrixPath,
        '--proof', proofPath,
      ]);
      assert.notEqual(result.code, 0);
      assert.match(result.stderr, /Invalid discovery JSON/);
      assert.equal(await pathExists(matrixPath), false);
      assert.equal(await pathExists(proofPath), false);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it('does not drop tests when timing is stale, incompatible, or missing', async () => {
    const dir = await makeTempDir();
    try {
      const discovery = join(dir, 'discovery.json');
      await writeFile(discovery, JSON.stringify(SUMMARY_MANY));
      const now = Date.now();

      const stale = makeTiming({
        generated_at: now - 30 * 24 * 60 * 60 * 1000,
        tests: { 'pkg|bin|fast': 10 },
      });
      const incompatible = makeTiming({ version: 'legacy/v0', tests: { 'pkg|bin|fast': 10 } });
      const cases = [
        { name: 'missing', timing: null },
        { name: 'stale', timing: stale },
        { name: 'incompatible', timing: incompatible },
      ];

      for (const c of cases) {
        const args = ['--discovery', discovery, '--profile', 'pull-request', '--now', now.toString()];
        if (c.timing) {
          const timingPath = join(dir, `${c.name}-timing.json`);
          await writeFile(timingPath, JSON.stringify(c.timing));
          args.push('--timing', timingPath);
        }
        const plan = await runPlan(args);
        assert.equal(plan.proof.discoveredCount, 4, `${c.name}: discoveredCount`);
        assert.equal(plan.proof.assignedCount, 4, `${c.name}: assignedCount`);
        assert.equal(plan.proof.exactOnce, true, `${c.name}: exactOnce`);
        assert.equal(plan.coldStart, true, `${c.name}: coldStart`);
      }
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it('sets full-validation profile via flag', async () => {
    const dir = await makeTempDir();
    try {
      const discovery = join(dir, 'discovery.json');
      await writeFile(discovery, JSON.stringify(SUMMARY_ONE));
      const plan = await runPlan(['--discovery', discovery, '--full-validation']);
      assert.equal(plan.profile, 'full-validation');
      assert.equal(plan.shardCount, WIDE_DEFAULT_SHARDS);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it('matrix filters partition discovered tests exactly once', async () => {
    const dir = await makeTempDir();
    try {
      const discovery = join(dir, 'discovery.json');
      await writeFile(discovery, JSON.stringify(SUMMARY_MANY));
      const matrixPath = join(dir, 'matrix.json');
      await runPlan([
        '--discovery', discovery,
        '--matrix', matrixPath,
        '--profile', 'pull-request',
      ]);
      const matrix = JSON.parse(await readFile(matrixPath, 'utf8'));
      const seen = new Set();
      const discovered = new Set(parseDiscovery(JSON.stringify(SUMMARY_MANY)).map((t) => t.id));
      for (const row of matrix.shards) {
        assert.equal(row.shard, `shard-${row.shardIndex + 1}`);
        assert.equal(typeof row.filter, 'string');
        assert.equal(row.count, row.testIds.length);
        for (const id of row.testIds) {
          assert.equal(seen.has(id), false, `duplicate assignment: ${id}`);
          assert.equal(discovered.has(id), true, `unknown id: ${id}`);
          seen.add(id);
        }
      }
      assert.equal(seen.size, discovered.size);
      assert.deepEqual([...seen].sort(), [...discovered].sort());
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe('nextest.toml compatibility', () => {
  it('contains planner-emit profiles that inherit from ci and preserve default/ci', async () => {
    const toml = await readFile(NEXTEST_TOML, 'utf8');
    const sections = parseToml(toml);

    assert.equal('profile.default' in sections, true);
    assert.equal('profile.ci' in sections, true);
    assert.equal('profile.pull-request' in sections, true);
    assert.equal('profile.merge-group' in sections, true);
    assert.equal('profile.full-validation' in sections, true);

    assert.equal(sections['profile.pull-request'].inherits, '"ci"');
    assert.equal(sections['profile.pull-request']['fail-fast'], 'true');
    assert.equal(sections['profile.pull-request']['final-status-level'], '"fail"');

    assert.equal(sections['profile.merge-group'].inherits, '"ci"');
    assert.equal(sections['profile.full-validation'].inherits, '"ci"');

    // Existing CI profile is unchanged.
    assert.equal(sections['profile.ci']['fail-fast'], 'false');
    assert.equal(sections['profile.ci']['final-status-level'], '"flaky"');
    assert.equal(sections['profile.default']['fail-fast'], 'true');
    assert.equal(sections['profile.default'].retries, '0');
  });
});
