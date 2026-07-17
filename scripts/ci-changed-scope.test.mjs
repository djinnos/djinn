import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { classifyChangedScope, planChangedScope } from './ci-changed-scope.mjs';

const protectedJobs = [
  'cargoDeny', 'clippy', 'size', 'migrations', 'rawSql', 'capability',
  'boundaries', 'serverTest', 'sqlxFreshness', 'memoryEval',
];

function plan(files, overrides = {}) {
  return planChangedScope({
    event: 'pull_request', base: 'base-sha', head: 'head-sha', files,
    ...overrides,
  });
}

function assertOnlyJobs(result, names) {
  const expected = new Set(names);
  for (const [name, selected] of Object.entries(result.jobs)) assert.equal(selected, expected.has(name), name);
}

test('docs-only is a narrow lane outside the merge queue', () => {
  const result = plan(['README.md', 'docs/guide.md']);
  assert.equal(result.lanes.docs, true);
  assertOnlyJobs(result, []);
});

test('UI-only selects only UI', () => {
  const result = plan(['ui/src/App.tsx']);
  assert.equal(result.lanes.ui, true);
  assertOnlyJobs(result, ['ui']);
});

test('QA taxonomy, scenarios, and runner inputs select qa-smoke', () => {
  for (const path of [
    'qa/taxonomy.yaml',
    'qa/scenarios/dispatch/exhaustion-terminal-side-effects.yaml',
    'server/crates/djinn-qa/Cargo.toml',
    'server/crates/djinn-qa/src/harness.rs',
    'server/crates/djinn-qa/src/database_harness.rs',
    'server/crates/djinn-qa/src/run.rs',
    'server/crates/djinn-qa/src/evidence.rs',
    'server/crates/djinn-qa/tests/coverage_cli.rs',
  ]) {
    const result = plan([path]);
    assert.equal(result.lanes.qaSmoke, true, path);
    assert.equal(result.jobs.qaSmoke, true, path);
    assert.equal(result.lanes.unknown, false, path);
  }
});

test('qa-smoke stays unselected for unrelated narrow lanes', () => {
  for (const path of ['README.md', 'ui/src/App.tsx', 'scripts/tilt/build-ui.sh']) {
    const result = plan([path]);
    assert.equal(result.lanes.qaSmoke, false, path);
    assert.equal(result.jobs.qaSmoke, false, path);
  }
});

test('local Tilt inputs select the dedicated contract lane', () => {
  for (const path of [
    'Tiltfile',
    'scripts/tilt/build-ui.sh',
    'scripts/kind/setup-kind.sh',
    'scripts/test-tiltfile-config.sh',
    'deploy/helm/djinn/values.local.yaml',
    'deploy/langfuse-local/langfuse.yaml',
  ]) {
    const result = plan([path]);
    assert.equal(result.lanes.tiltContracts, true, path);
    assertOnlyJobs(result, ['tiltContracts']);
  }
});

test('server Docker changes select server policy and Tilt contracts', () => {
  const result = plan(['server/docker/djinn-image-builder.Dockerfile']);
  assert.equal(result.lanes.rustCore, true);
  assert.equal(result.lanes.tiltContracts, true);
  assertOnlyJobs(result, [...protectedJobs, 'tiltContracts']);
});

test('Rust core selects protected server policy jobs but not UI or aarch64', () => {
  const result = plan(['server/crates/djinn-agent/src/lib.rs']);
  assert.equal(result.lanes.rustCore, true);
  assertOnlyJobs(result, protectedJobs);
});

test('UI codegen inputs under server/ select the UI lane too', () => {
  for (const path of [
    'server/src/server/tests/snapshots/djinn_server__server__tests__tool_schemas__mcp_tools_schema.snap',
    'server/schemas/usage-analytics.schema.json',
  ]) {
    const result = plan([path]);
    assert.equal(result.lanes.ui, true, path);
    assert.equal(result.lanes.rustCore, true, path);
    assert.equal(result.jobs.ui, true, path);
  }
});

test('migrations and SQLx cache classify their lanes and select protected jobs', () => {
  const migration = plan(['server/crates/djinn-db/migrations_postgres/20260101_init.sql']);
  assert.equal(migration.lanes.migrations, true);
  assertOnlyJobs(migration, protectedJobs);
  const sqlx = plan(['server/.sqlx/query-abc.json']);
  assert.equal(sqlx.lanes.sqlx, true);
  assertOnlyJobs(sqlx, protectedJobs);
});

test('sandbox selects the aarch64 job as well as server policy', () => {
  const result = plan(['server/crates/djinn-sandbox/src/lib.rs']);
  assert.equal(result.lanes.sandboxAarch64, true);
  assertOnlyJobs(result, [...protectedJobs, 'aarch64']);
});

test('memory ranking selects memory evaluation and server policy', () => {
  const result = plan(['server/crates/djinn-db/src/repositories/note/rrf.rs']);
  assert.equal(result.lanes.memoryRanking, true);
  assertOnlyJobs(result, protectedJobs);
});

test('workflow CI changes route every job', () => {
  const result = plan(['.github/workflows/quality-gate.yml']);
  assert.equal(result.lanes.workflowCi, true);
  assert.equal(result.jobs.qaSmoke, true);
  assertOnlyJobs(result, Object.keys(result.jobs));
});

test('full validation selects every job regardless of a docs-only diff', () => {
  const result = plan(['docs/guide.md'], { fullValidation: true });
  assert.equal(result.jobs.qaSmoke, true);
  assertOnlyJobs(result, Object.keys(result.jobs));
});

test('docs-only merge_group selects every protected server policy job, not UI', () => {
  const result = plan(['docs/guide.md'], { event: 'merge_group' });
  assert.equal(result.lanes.docs, true);
  for (const name of protectedJobs) assert.equal(result.jobs[name], true, name);
  assert.equal(result.jobs.ui, false);
  assert.equal(result.jobs.aarch64, false);
  assert.equal(result.jobs.qaSmoke, false);
});

test('unknown executable and configuration paths fail closed', () => {
  for (const path of [
    'scripts/new-check.sh', '.github/actions/check/action.yml', 'package.json',
    '.cargo/config.toml', 'config/quality-gate.yml', 'tools/verify-ci.sh',
    'deploy/helm/djinn/templates/deployment-server.yaml',
  ]) {
    const result = plan([path]);
    assert.equal(result.lanes.unknown, true, path);
    assert.equal(result.jobs.qaSmoke, true, path);
    assertOnlyJobs(result, Object.keys(result.jobs));
  }
});

test('normalization rejects unsafe and ambiguous file lists', () => {
  for (const files of [['docs/../secret'], ['/docs/a.md'], ['docs/a.md', 'docs/a.md']]) {
    assert.throws(() => classifyChangedScope(files), /ci-changed-scope/);
  }
});

test('CLI JSON and GitHub output are derived from the same deterministic result', () => {
  const outputFile = join(mkdtempSync(join(tmpdir(), 'changed-scope-')), 'github-output');
  const invocation = spawnSync(process.execPath, [
    'scripts/ci-changed-scope.mjs', '--event', 'merge_group', '--base', 'base', '--head', 'head',
    '--file', 'qa/scenarios/dispatch/exhaustion-terminal-side-effects.yaml', '--github-output', outputFile,
  ], { encoding: 'utf8' });
  assert.equal(invocation.status, 0, invocation.stderr);
  const result = JSON.parse(invocation.stdout);
  assert.deepEqual(result, plan(['qa/scenarios/dispatch/exhaustion-terminal-side-effects.yaml'], { event: 'merge_group', base: 'base', head: 'head' }));
  assert.equal(result.outputs.qaSmoke, 'true');
  const githubOutputs = Object.fromEntries(readFileSync(outputFile, 'utf8').trim().split('\n').map((line) => line.split('=')));
  assert.deepEqual(githubOutputs, result.outputs);
});

test('CLI reads NUL-delimited git paths without core.quotePath corruption', () => {
  const directory = mkdtempSync(join(tmpdir(), 'changed-scope-nul-'));
  const filesFile = join(directory, 'changed-files.z');
  const unicodePath = '.djinn/decisions/agent-harness-—-library.md';
  writeFileSync(filesFile, `${unicodePath}\0scripts/check.sh\0`);

  const invocation = spawnSync(process.execPath, [
    'scripts/ci-changed-scope.mjs', '--event', 'pull_request', '--base', 'base', '--head', 'head',
    '--files0-file', filesFile,
  ], { encoding: 'utf8' });

  assert.equal(invocation.status, 0, invocation.stderr);
  const result = JSON.parse(invocation.stdout);
  assert.deepEqual(result.files, [unicodePath, 'scripts/check.sh'].sort());
  assert.equal(result.lanes.unknown, true);
});
