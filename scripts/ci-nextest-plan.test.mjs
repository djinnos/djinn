// scripts/ci-nextest-plan.test.mjs
// Invariant tests for the timing-fed nextest planning engine.

import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
  canonicalId,
  parseDiscovery,
  loadTiming,
  estimateDuration,
  planTests,
  validateExactOnce,
  TIMING_VERSION,
  FALLBACK_DURATION_SECONDS,
  PR_DEFAULT_SHARDS,
  PR_MAX_SHARDS,
  WIDE_DEFAULT_SHARDS,
  COLD_START_SHARDS,
  PR_WIDEN_TEST_THRESHOLD,
  PR_WIDEN_DURATION_THRESHOLD_SECONDS,
} from './ci-nextest-plan.mjs';

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

describe('planTests', () => {
  it('assigns every discovered test exactly once', () => {
    const tests = parseDiscovery(JSON.stringify(SUMMARY_MANY));
    const plan = planTests({ tests, timings: new Map() });
    validateExactOnce(plan);
    assert.equal(plan.proof.exactOnce, true);
    assert.equal(plan.shardCount, COLD_START_SHARDS);
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
