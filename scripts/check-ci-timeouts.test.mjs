import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const scripts = dirname(fileURLToPath(import.meta.url));
const checker = join(scripts, 'check-ci-timeouts.mjs');
const fixtures = join(scripts, 'fixtures', 'ci-timeouts');
const passing = new Set(['valid-aggregate-scalar-needs', 'valid-list-needs-conditional-matrix', 'valid-expression-matrix', 'valid-local-reusable', 'valid-recursive-reusable', 'valid-structural-no-timeout', 'timeout-min', 'timeout-max']);
const expected = {
  'unbounded-transitive-qa-smoke': 'MISSING_TIMEOUT', 'unknown-need': 'UNKNOWN_NEED', 'dynamic-need': 'INVALID_NEEDS', 'missing-needs-value': 'INVALID_NEEDS', 'malformed-yaml': 'YAML_SYNTAX', 'duplicate-job': 'DUPLICATE_KEY', 'duplicate-job-field': 'DUPLICATE_KEY', 'malformed-job-value': 'MALFORMED_JOB', 'dependency-cycle': 'DEPENDENCY_CYCLE', 'reusable-call-cycle': 'WORKFLOW_CALL_CYCLE', 'remote-reusable': 'UNSUPPORTED_USES', 'expression-reusable': 'UNSUPPORTED_USES', 'unresolved-reusable-file': 'UNRESOLVED_WORKFLOW', 'not-workflow-call-target': 'UNRESOLVED_WORKFLOW', 'called-workflow-missing-jobs': 'UNRESOLVED_WORKFLOW', 'called-job-unknown-need': 'UNKNOWN_NEED', 'called-job-bound-removed': 'MISSING_TIMEOUT', 'illegal-structural-timeout': 'ILLEGAL_CALLER_TIMEOUT', 'missing-covered': 'MISSING_COVERED', 'extra-covered': 'EXTRA_COVERED', 'timeout-missing': 'MISSING_TIMEOUT', 'timeout-string': 'INVALID_TIMEOUT', 'timeout-float': 'INVALID_TIMEOUT', 'timeout-zero': 'INVALID_TIMEOUT', 'timeout-negative': 'INVALID_TIMEOUT', 'timeout-over-120': 'INVALID_TIMEOUT', 'unsupported-if-shape': 'UNSUPPORTED_IF', 'unsupported-matrix-shape': 'UNSUPPORTED_MATRIX', 'invalid-job-id': 'INVALID_JOB_ID', 'manifest-unsorted-or-duplicate': 'MANIFEST_SCHEMA', 'manifest-invalid-field-type': 'MANIFEST_SCHEMA',
};
for (const name of [...passing, ...Object.keys(expected)].sort()) test(name, () => {
  const result = spawnSync(process.execPath, [checker, '--root', join(fixtures, name), '--manifest', 'manifest.json'], { encoding: 'utf8' });
  if (passing.has(name)) assert.equal(result.status, 0, result.stderr);
  else { assert.equal(result.status, 1, result.stderr); assert.match(result.stderr, new RegExp(`: ${expected[name]}:`)); }
});
test('called-job timeout diagnostic is caller-qualified', () => {
  const root = join(fixtures, 'called-job-bound-removed');
  const result = spawnSync(process.execPath, [checker, '--root', root, '--manifest', 'manifest.json'], { encoding: 'utf8' });
  assert.match(result.stderr, /root\.yml#quality-gate=>\.github\/workflows\/called\.yml#test/);
});
test('parser diagnostics include one-based source locations', () => {
  for (const name of ['duplicate-job', 'duplicate-job-field', 'malformed-yaml']) {
    const result = spawnSync(process.execPath, [checker, '--root', join(fixtures, name), '--manifest', 'manifest.json'], { encoding: 'utf8' });
    assert.equal(result.status, 1, result.stderr);
    assert.match(result.stderr, /\.github\/workflows\/root\.yml:\d+:\d+: (?:DUPLICATE_KEY|YAML_SYNTAX):/);
  }
});
