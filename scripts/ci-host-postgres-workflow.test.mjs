import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const workflowPaths = [
  '.github/workflows/quality-gate.yml',
  '.github/workflows/resize-matrix.yml',
  '.github/workflows/memory-qa-nightly.yml',
];
const workflows = workflowPaths.map((path) => readFileSync(path, 'utf8')).join('\n');
const setupScript = readFileSync('scripts/ci-start-postgres.sh', 'utf8');

test('database jobs use runner PostgreSQL instead of registry service containers', () => {
  assert.doesNotMatch(workflows, /image:\s*(?:public\.ecr\.aws\/docker\/library\/)?postgres:16/,
    'PostgreSQL CI jobs must not pull a registry image before checkout');
  assert.equal(workflows.match(/name: Start PostgreSQL 16/g)?.length, 7,
    'all seven PostgreSQL jobs must start the runner installation');
  assert.equal(workflows.match(/runs-on: ubuntu-24\.04/g)?.length, 7,
    'the seven jobs must pin the runner image that provides PostgreSQL 16');
});

test('host setup preserves the existing PostgreSQL contract and fails on runner drift', () => {
  assert.match(setupScript, /readonly expected_major=16/);
  assert.match(setupScript, /readonly port=5433/);
  assert.match(setupScript, /readonly database=djinn/);
  assert.match(setupScript, /Unexpected PostgreSQL version/);
  assert.match(setupScript, /PostgreSQL cluster missing/);
  assert.match(setupScript, /ALTER USER postgres WITH PASSWORD 'postgres'/);
  assert.match(setupScript, /ALTER SYSTEM SET port = '5433'/);
});
