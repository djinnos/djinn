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

test('CI cluster is tuned for throwaway template-clone workloads', () => {
  // `CREATE DATABASE … STRATEGY = FILE_COPY` in
  // server/crates/djinn-db/src/database.rs trades per-block WAL for a
  // checkpoint pair around every clone. Measured, that is ~5x SLOWER than the
  // WAL_LOG default on a cluster with fsync=on. These settings are therefore
  // a hard precondition of the clone strategy, not an independent tweak — if
  // one is dropped without the other, CI gets slower, not faster.
  for (const setting of [
    "ALTER SYSTEM SET fsync = 'off'",
    "ALTER SYSTEM SET synchronous_commit = 'off'",
    "ALTER SYSTEM SET full_page_writes = 'off'",
    "ALTER SYSTEM SET wal_level = 'minimal'",
    "ALTER SYSTEM SET max_wal_senders = '0'",
    "ALTER SYSTEM SET autovacuum = 'off'",
  ]) {
    assert.ok(setupScript.includes(setting), `missing: ${setting}`);
  }

  // wal_level=minimal and max_wal_senders=0 must land in the SAME restart:
  // Postgres refuses to start with minimal WAL and a non-zero walsender
  // budget. Both live in the single heredoc before the single restart.
  const heredoc = setupScript.slice(
    setupScript.indexOf("ALTER USER postgres WITH PASSWORD"),
    setupScript.indexOf('\nSQL'),
  );
  assert.match(heredoc, /wal_level = 'minimal'/);
  assert.match(heredoc, /max_wal_senders = '0'/);

  // The settings must be asserted against the RUNNING server, not just
  // written to postgresql.auto.conf and assumed.
  assert.match(setupScript, /SHOW \$\{setting\}/);
  assert.match(setupScript, /PostgreSQL tuning not applied/);
});

test('the FILE_COPY clone strategy and its preconditions ship together', () => {
  const clone = readFileSync('server/crates/djinn-db/src/database.rs', 'utf8');
  assert.match(clone, /STRATEGY = FILE_COPY/,
    'template clone must be able to select the FILE_COPY strategy');
  // FILE_COPY must stay gated on the precondition it needs. Measured, it is
  // ~5.4x SLOWER than the WAL_LOG default against an fsync=on cluster, so a
  // developer or worker pod pointed at a durable Postgres must not get it.
  assert.match(clone, /SHOW fsync/,
    'the clone strategy must be probed, not assumed');
  const compose = readFileSync('docker-compose.yml', 'utf8');
  for (const setting of ['fsync=off', 'wal_level=minimal', 'max_wal_senders=0']) {
    assert.ok(compose.includes(setting),
      `docker-compose postgres-test must set ${setting} for FILE_COPY to pay off`);
  }
});
