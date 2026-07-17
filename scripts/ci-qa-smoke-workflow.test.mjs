import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';

const WORKFLOW = resolve('.github/workflows/quality-gate.yml');

// qa-smoke may use only its local Postgres service and deterministic mocks. Do
// not enumerate providers here: the catalog can gain providers independently of
// this contract. Credential-shaped names catch both catalog API keys and OAuth
// tokens, while explicitly permitting names reserved for local test doubles.
const LIVE_PROVIDER_CREDENTIAL = /\$\{\{\s*(?:secrets|vars|env)\.|\b(?![A-Z0-9_]*(?:TEST|MOCK|FAKE)[A-Z0-9_]*\b)[A-Z][A-Z0-9_]*(?:_(?:API|ACCESS|SECRET|AUTH|BEARER)_?(?:KEY|TOKEN)|_ACCESS_KEY(?:_ID)?|_TOKEN|_KEY)\b/i;
const EXTERNAL_NETWORK_URL = /\bhttps?:\/\//i;
const EXTERNAL_NETWORK_CLIENT = /\b(?:curl|wget|invoke-webrequest)\b/i;

function fail(message) {
  throw new Error(`ci-qa-smoke-workflow: ${message}`);
}

function parseJobs(source) {
  const lines = source.replace(/\r\n/g, '\n').split('\n');
  const jobsAt = lines.findIndex((line) => /^jobs:\s*(?:#.*)?$/.test(line));
  if (jobsAt < 0) fail('workflow has no jobs mapping');

  const jobs = new Map();
  let current;
  for (let index = jobsAt + 1; index < lines.length; index += 1) {
    const line = lines[index];
    const match = line.match(/^ {2}([A-Za-z0-9_-]+):\s*(?:#.*)?$/);
    if (match) {
      current = { id: match[1], start: index + 1, lines: [] };
      if (jobs.has(current.id)) fail(`duplicate job declaration ${current.id}`);
      jobs.set(current.id, current);
    } else if (current) {
      current.lines.push({ text: line, number: index + 1 });
    }
  }
  return { lines, jobs };
}

function jobField(job, name) {
  const match = job.lines.find(({ text }) => new RegExp(`^ {4}${name}:\\s*(.+)$`).test(text))?.text
    .match(new RegExp(`^ {4}${name}:\\s*(.+)$`));
  return match?.[1]?.trim();
}

function jobNeeds(job) {
  const scalar = jobField(job, 'needs');
  if (scalar && !scalar.startsWith('#')) return [scalar.replace(/\s+#.*$/, '')];
  const at = job.lines.findIndex(({ text }) => /^ {4}needs:\s*(?:#.*)?$/.test(text));
  if (at < 0) return [];
  const needs = [];
  for (const { text } of job.lines.slice(at + 1)) {
    const match = text.match(/^ {6}-\s+([A-Za-z0-9_-]+)\s*(?:#.*)?$/);
    if (match) needs.push(match[1]);
    else if (/^ {4}\S/.test(text)) break;
  }
  return needs;
}

function steps(job) {
  const result = [];
  let current;
  for (const line of job.lines) {
    if (/^ {6}-\s+/.test(line.text)) {
      if (current) result.push(current);
      current = { lines: [line] };
    } else if (current) {
      current.lines.push(line);
    }
  }
  if (current) result.push(current);
  return result;
}

function stepName(step) {
  return step.lines[0].text.match(/^ {6}- name:\s*(.+?)\s*$/)?.[1];
}

function stepText(step) {
  return step.lines.map(({ text }) => text).join('\n');
}

function namedStep(job, name) {
  const step = steps(job).find((candidate) => stepName(candidate) === name);
  assert.ok(step, `${job.id} must contain the ${name} step`);
  return step;
}

function assertIncludes(text, expected, message) {
  assert.ok(text.includes(expected), message ?? `expected ${JSON.stringify(expected)}`);
}

function assertStepOrder(job, names) {
  const actual = steps(job).map(stepName);
  let previous = -1;
  for (const name of names) {
    const index = actual.indexOf(name);
    assert.ok(index >= 0, `${job.id} must contain the ${name} step`);
    assert.ok(index > previous, `${name} must follow ${names[names.indexOf(name) - 1]}`);
    previous = index;
  }
}

function assertQaJobHasNoLiveDependencies(job) {
  const source = job.lines.map(({ text }) => text).join('\n');
  assert.doesNotMatch(source, /\b(?:kubectl|kind|kubernetes|kubeconfig|helm|tilt)\b/i,
    'qa-smoke must not depend on Kubernetes or local-cluster tooling');
  assert.doesNotMatch(source, /\b(?:nightly[-_ ]live|live[-_ ](?:provider|credential|scenario|execution))\b/i,
    'qa-smoke must not run nightly/live-only work');
  assert.doesNotMatch(source, LIVE_PROVIDER_CREDENTIAL,
    'qa-smoke must not receive live provider credentials or credential expressions');
  assert.doesNotMatch(source, EXTERNAL_NETWORK_URL,
    'qa-smoke must not call external provider endpoints');
  assert.doesNotMatch(source, EXTERNAL_NETWORK_CLIENT,
    'qa-smoke must not use external-network clients');
  assert.doesNotMatch(source, /127\.0\.0\.1:543[2-9]\/(?!postgres\b|djinn_test_template\b)/,
    'qa-smoke must not target a shared or developer database');
}

test('qa-smoke live dependency guards cover catalog additions and provider URLs', () => {
  assert.match('MINIMAX_API_KEY: ${{ env.MINIMAX_API_KEY }}', LIVE_PROVIDER_CREDENTIAL,
    'provider credential names must fail closed even when sourced from env');
  assert.match('https://api.minimax.io/anthropic/v1', EXTERNAL_NETWORK_URL,
    'direct provider endpoints must fail closed');
  assert.match('curl https://api.minimax.io/anthropic/v1', EXTERNAL_NETWORK_CLIENT,
    'provider endpoint clients must fail closed');
  assert.doesNotMatch('MOCK_PROVIDER_API_KEY', LIVE_PROVIDER_CREDENTIAL,
    'deterministic mock credentials may remain local');
});

test('qa-smoke consumes the routed output and is event-safe', () => {
  const parsed = parseJobs(readFileSync(WORKFLOW, 'utf8'));
  const preflight = parsed.jobs.get('preflight');
  const qa = parsed.jobs.get('qa-smoke');
  assert.ok(preflight, 'preflight job must exist');
  assert.ok(qa, 'qa-smoke job must exist');
  assert.equal(jobField(qa, 'name'), 'qa-smoke', 'qa-smoke must remain a visible named lane');
  assert.deepEqual(jobNeeds(qa), ['preflight'], 'qa-smoke must be selected by preflight only');

  const preflightSource = preflight.lines.map(({ text }) => text).join('\n');
  assert.match(preflightSource, /^ {6}qaSmoke:\s*\$\{\{ steps\.router\.outputs\.qaSmoke \}\}\s*$/m,
    'preflight must expose the router qaSmoke output');

  const condition = jobField(qa, 'if') ?? '';
  assert.match(condition, /needs\.preflight\.outputs\.qaSmoke\s*==\s*['"]true['"]/, 'qa-smoke must consume qaSmoke');
  for (const event of ['pull_request', 'merge_group', 'workflow_dispatch']) {
    assert.match(condition, new RegExp(`github\\.event_name\\s*==\\s*['"]${event}['"]`),
      `qa-smoke must be selectable on ${event}`);
  }
  assert.doesNotMatch(condition, /github\.event_name\s*==\s*['"]push['"]/, 'qa-smoke must not run on push');
});

test('qa-smoke owns only a restore-only test cache and a disposable database', () => {
  const qa = parseJobs(readFileSync(WORKFLOW, 'utf8')).jobs.get('qa-smoke');
  assert.ok(qa, 'qa-smoke job must exist');
  const source = qa.lines.map(({ text }) => text).join('\n');

  assert.match(source, /Swatinem\/rust-cache@v2/, 'qa-smoke must restore Rust build inputs');
  assert.match(source, /^ {10}shared-key:\s*server-test\s*$/m, 'qa-smoke must use the server-test cache family');
  assert.match(source, /^ {10}save-if:\s*false\s*$/m, 'qa-smoke must remain a restore-only cache consumer');
  assert.match(source, /^ {6}DJINN_TEST_DATABASE_URL:\s*postgres:\/\/postgres:postgres@127\.0\.0\.1:5433\/postgres\s*$/m,
    'clone ownership must use the disposable maintenance database');
  assert.match(source, /^ {6}DATABASE_URL:\s*postgres:\/\/postgres:postgres@127\.0\.0\.1:5433\/djinn_test_template\s*$/m,
    'SQLx compilation must use the migrated template database');
  assert.match(source, /CREATE DATABASE djinn_test_template/, 'qa-smoke must create its template database');
  assert.match(source, /DATABASE_URL="\$DATABASE_URL" sqlx migrate run --source migrations_postgres/,
    'qa-smoke must migrate the template before compiling SQLx consumers');
  assert.match(source, /UPDATE pg_database SET datistemplate = TRUE WHERE datname = 'djinn_test_template'/,
    'qa-smoke must mark its dedicated database as a template');
  assertQaJobHasNoLiveDependencies(qa);
});

test('qa-smoke builds, precompiles, and directly executes in deterministic order', () => {
  const qa = parseJobs(readFileSync(WORKFLOW, 'utf8')).jobs.get('qa-smoke');
  assert.ok(qa, 'qa-smoke job must exist');
  assertStepOrder(qa, [
    'Build migrated Postgres template',
    'Build qa runner once',
    'Precompile selected smoke test binaries',
    'Run deterministic qa smoke and coverage gate',
    'Upload qa smoke evidence',
  ]);

  const build = stepText(namedStep(qa, 'Build qa runner once'));
  assertIncludes(build, 'cargo build -p djinn-qa', 'qa runner must be built once before execution');

  const precompile = stepText(namedStep(qa, 'Precompile selected smoke test binaries'));
  for (const packageName of ['djinn-coordinator', 'djinn-slot', 'djinn-db']) {
    assert.match(precompile, new RegExp(`cargo test -p ${packageName} --no-run`),
      `qa-smoke must precompile ${packageName}`);
  }

  const run = stepText(namedStep(qa, 'Run deterministic qa smoke and coverage gate'));
  assert.match(run, /target\/debug\/djinn-qa run --qa-profile smoke-ci --concurrency 8/,
    'qa-smoke must directly execute the built runner');
  assert.match(run, /target\/debug\/djinn-qa coverage --profile smoke-ci --format json/,
    'qa-smoke must directly execute coverage after the runner');
  assert.ok(run.indexOf('target/debug/djinn-qa run') < run.indexOf('target/debug/djinn-qa coverage'),
    'coverage must run after smoke execution');
  assert.doesNotMatch(run, /\bcargo run\b/, 'qa-smoke must reject nested cargo-run lock execution');
});

test('qa-smoke preserves rooted evidence and uploads it even after failures', () => {
  const qa = parseJobs(readFileSync(WORKFLOW, 'utf8')).jobs.get('qa-smoke');
  assert.ok(qa, 'qa-smoke job must exist');
  const run = stepText(namedStep(qa, 'Run deterministic qa smoke and coverage gate'));
  assertIncludes(run, '--repo-root .. --evidence-dir qa/evidence/smoke-ci',
    'the runner must write the repository-root smoke evidence directory');
  assertIncludes(run, '--repo-root .. --evidence ../qa/evidence/smoke-ci --output ../qa/evidence/smoke-ci/coverage.json',
    'coverage must read and write the same repository-root smoke evidence directory');
  assert.doesNotMatch(run, /(?:server\/qa\/evidence|--evidence(?:-dir)?\s+server\/qa\/evidence)/,
    'qa-smoke must not use a server-relative evidence directory');

  const upload = namedStep(qa, 'Upload qa smoke evidence');
  const uploadSource = stepText(upload);
  assert.match(uploadSource, /^ {8}if:\s*always\(\)\s*$/m, 'evidence upload must be unconditional');
  assert.match(uploadSource, /actions\/upload-artifact@v4/, 'evidence must be an artifact');
  assert.match(uploadSource, /^ {10}path:\s*qa\/evidence\/smoke-ci\s*$/m,
    'artifact path must be rooted at qa/evidence/smoke-ci');
  assert.match(uploadSource, /^ {10}if-no-files-found:\s*error\s*$/m,
    'missing smoke evidence must fail the job');
});

test('Quality Gate aggregates qa-smoke results fail-closed', () => {
  const parsed = parseJobs(readFileSync(WORKFLOW, 'utf8'));
  const gate = parsed.jobs.get('quality-gate');
  assert.ok(gate, 'Quality Gate job must exist');
  assert.ok(jobNeeds(gate).includes('qa-smoke'), 'Quality Gate must depend on qa-smoke');

  const source = gate.lines.map(({ text }) => text).join('\n');
  assert.match(source, /^ {10}QA_SMOKE:\s*\$\{\{ needs\.preflight\.outputs\.qaSmoke \}\}:\$\{\{ needs\.qa-smoke\.result \}\}\s*$/m,
    'Quality Gate must pair qa-smoke selection with its result');
  assert.match(source, /if \[ "\$selected" = true \] && \[ "\$result" = success \]; then return; fi/,
    'selected work may only pass after success');
  assert.match(source, /if \[ "\$selected" = false \] && \[ "\$result" = skipped \]; then return; fi/,
    'only unselected work may be skipped');
  assert.match(source, /check qa-smoke "\$QA_SMOKE"/, 'Quality Gate must check qa-smoke explicitly');
  assert.doesNotMatch(source, /QA_SMOKE[^\n]*\|\|\s*true/, 'qa-smoke aggregation must not be fail-open');
});
