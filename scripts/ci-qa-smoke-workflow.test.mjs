import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';

const WORKFLOW = resolve('.github/workflows/quality-gate.yml');
const BUILD_COMMAND = 'cargo build -p djinn-qa';
const PRECOMPILE_COMMAND = 'cargo test --no-run -p djinn-coordinator -p djinn-slot -p djinn-db';
const RUN_COMMAND = 'target/debug/djinn-qa run --qa-profile smoke-ci --concurrency 8 --evidence-dir qa/evidence/smoke-ci';
const MAINTENANCE_DATABASE_URL = 'postgres://postgres:postgres@127.0.0.1:5433/postgres';
const TEMPLATE_DATABASE_URL = 'postgres://postgres:postgres@127.0.0.1:5433/djinn_test_template';
// qa-smoke runs from server, while the runner emits evidence at the repository
// root. Coverage must use that emitted directory, not server/qa/evidence.
const COVERAGE_COMMAND = 'target/debug/djinn-qa coverage --profile smoke-ci --format json --evidence ../qa/evidence/smoke-ci --output ../qa/evidence/smoke-ci/coverage.json';

function job(source, id) {
  const match = source.match(new RegExp(`^  ${id}:\\n([\\s\\S]*?)(?=^  [A-Za-z0-9_-]+:|$(?![\\s\\S]))`, 'm'));
  assert.ok(match, `workflow must declare ${id}`);
  return match[1];
}

test('qa-smoke workflow contract is deterministic, routed, and fail closed', () => {
  const source = readFileSync(WORKFLOW, 'utf8');
  const preflight = job(source, 'preflight');
  const smoke = job(source, 'qa-smoke');
  const aggregate = job(source, 'quality-gate');

  assert.match(preflight, /qaSmoke: \$\{\{ steps\.router\.outputs\.qaSmoke \}\}/,
    'preflight must expose the router qaSmoke output');
  assert.match(preflight, /node --test scripts\/ci-qa-smoke-workflow\.test\.mjs/,
    'preflight must execute this workflow contract');

  assert.match(smoke, /^    name: qa-smoke$/m, 'the smoke check must be visible as qa-smoke');
  assert.match(smoke, /^    needs: preflight$/m);
  assert.match(smoke, /^    timeout-minutes: 90$/m,
    'qa-smoke must have a finite job deadline in addition to subprocess deadlines');
  assert.match(smoke, /needs\.preflight\.outputs\.qaSmoke == 'true'/);
  for (const event of ['pull_request', 'merge_group', 'workflow_dispatch']) {
    assert.match(smoke, new RegExp(`github\\.event_name == '${event}'`), `${event} must select qa-smoke`);
  }
  assert.match(smoke, /working-directory: server/,
    'commands must execute from the Rust workspace while preserving repository-relative evidence paths');

  assert.match(smoke, /uses: Swatinem\/rust-cache@v2[\s\S]*?workspaces: server[\s\S]*?shared-key: server-quality[\s\S]*?save-if: false/,
    'qa-smoke must be a restore-only server-quality cache consumer');
  assert.match(smoke, /^    services:\n      postgres:\n        image: postgres:16$/m,
    'qa-smoke must provision a disposable local Postgres service');
  const maintenanceDatabaseMatch = smoke.match(/^      DJINN_TEST_DATABASE_URL: (\S+)$/m);
  assert.ok(maintenanceDatabaseMatch,
    'qa-smoke must expose a runtime database URL for isolated clone acquisition');
  assert.equal(maintenanceDatabaseMatch[1], MAINTENANCE_DATABASE_URL,
    'the runtime clone lifecycle must be owned by the postgres maintenance database');
  assert.match(smoke, /uses: taiki-e\/install-action@v2[\s\S]*?tool: sqlx-cli[\s\S]*?name: Build disposable Postgres test template[\s\S]*?CREATE DATABASE djinn_test_template[\s\S]*?DATABASE_URL=postgres:\/\/postgres:postgres@127\.0\.0\.1:5433\/djinn_test_template sqlx migrate run --source migrations_postgres[\s\S]*?UPDATE pg_database SET datistemplate = TRUE WHERE datname = 'djinn_test_template'/,
    'qa-smoke must migrate and mark the disposable clone template before scenarios run');
  assert.doesNotMatch(smoke, /\b(?:KUBECONFIG|kubectl|kubernetes|helm|tilt|kind|credentials|live[-_ ]?(?:provider|credential|scenario)|external[-_ ]?network)\b/i,
    'qa-smoke must not configure live, provider-network, or Kubernetes dependencies');

  const templateAt = smoke.indexOf('Build disposable Postgres test template');
  const migrationAt = smoke.indexOf('sqlx migrate run --source migrations_postgres', templateAt);
  const templateReadyAt = smoke.indexOf("UPDATE pg_database SET datistemplate = TRUE WHERE datname = 'djinn_test_template'", migrationAt);
  const smokeStepAt = smoke.indexOf('name: Run deterministic qa smoke evidence');
  const uploadStepAt = smoke.indexOf('name: Upload qa-smoke evidence', smokeStepAt);
  const executionStep = smoke.slice(smokeStepAt, uploadStepAt);
  const compileDatabaseMatches = [...executionStep.matchAll(/^          DATABASE_URL: (\S+)$/gm)];
  const buildAt = smoke.indexOf(BUILD_COMMAND);
  const precompileAt = smoke.indexOf(PRECOMPILE_COMMAND);
  const runAt = smoke.indexOf(RUN_COMMAND);
  const coverageAt = smoke.indexOf(COVERAGE_COMMAND);
  assert.doesNotMatch(executionStep, /cargo run -p djinn-qa/,
    'the runner must not retain the Cargo build lock while it launches cargo test children');
  assert.equal(executionStep.split(BUILD_COMMAND).length - 1, 1,
    'qa-smoke must build djinn-qa exactly once before execution');
  assert.ok(buildAt >= 0, 'the djinn-qa build must be present');
  assert.equal(executionStep.split(PRECOMPILE_COMMAND).length - 1, 1,
    'scenario packages must be precompiled exactly once against the template');
  assert.ok(precompileAt >= 0, 'the exact scenario package precompile must be present');
  assert.ok(runAt >= 0, 'qa-smoke must run the exact deterministic smoke command');
  assert.ok(templateAt >= 0 && migrationAt > templateAt && templateReadyAt > migrationAt && templateReadyAt < smokeStepAt && smokeStepAt < buildAt && buildAt < precompileAt && precompileAt < runAt,
    'template bootstrap, djinn-qa build, package precompile, and direct runner must remain ordered');
  assert.equal(compileDatabaseMatches.length, 1,
    'the smoke and coverage execution step must own exactly one SQLx compile-time database URL');
  assert.equal(compileDatabaseMatches[0][1], TEMPLATE_DATABASE_URL,
    'the build and package precompile must share the migrated template compile URL');
  assert.notEqual(maintenanceDatabaseMatch[1], compileDatabaseMatches[0][1],
    'isolated clone acquisition and SQLx compile-time checks must use distinct databases');
  assert.ok(coverageAt > runAt, 'coverage must run after smoke evidence is emitted');
  assert.match(smoke, /--evidence \.\.\/qa\/evidence\/smoke-ci --output \.\.\/qa\/evidence\/smoke-ci\/coverage\.json/,
    'coverage must read and write alongside the repository-root evidence emitted from server');
  assert.match(smoke, /set \+e[\s\S]*?run_status=\$\?[\s\S]*?coverage_status=\$\?/,
    'coverage must still run after scenario failure so evidence remains diagnosable');

  assert.match(smoke, /if: always\(\)[\s\S]*?uses: actions\/upload-artifact@v4[\s\S]*?name: qa-smoke-evidence[\s\S]*?path: qa\/evidence\/smoke-ci[\s\S]*?if-no-files-found: error/,
    'qa-smoke evidence must always upload and reject absent evidence');

  assert.match(aggregate, /^      - qa-smoke$/m,
    'quality-gate must wait for qa-smoke');
  assert.match(aggregate, /QA_SMOKE: \$\{\{ needs\.preflight\.outputs\.qaSmoke \}\}:\$\{\{ needs\.qa-smoke\.result \}\}/,
    'aggregate must pair qaSmoke selection with the lane result');
  assert.match(aggregate, /check qa-smoke "\$QA_SMOKE"/,
    'aggregate must fail closed on selected qa-smoke failures and unselected non-skips');
});
