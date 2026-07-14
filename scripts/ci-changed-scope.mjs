#!/usr/bin/env node
/**
 * Deterministic changed-scope planner for proposal pbv9 (see design/814k-roadmap).
 * It deliberately has no git or GitHub API dependency: callers provide the event,
 * base, head, and changed file list explicitly.
 */
import { appendFileSync, readFileSync } from 'node:fs';

const EVENTS = new Set(['pull_request', 'merge_group', 'push', 'workflow_dispatch']);
const JOB_NAMES = [
  'cargoDeny', 'clippy', 'aarch64', 'size', 'migrations', 'rawSql',
  'capability', 'boundaries', 'serverTest', 'sqlxFreshness', 'memoryEval',
  'tiltContracts', 'ui',
];
const PROTECTED_SERVER_JOBS = [
  'cargoDeny', 'clippy', 'size', 'migrations', 'rawSql', 'capability',
  'boundaries', 'serverTest', 'sqlxFreshness', 'memoryEval',
];

function fail(message) {
  throw new Error(`ci-changed-scope: ${message}`);
}

/** Normalize a repository-relative changed path, rejecting ambiguous spellings. */
export function normalizePath(value) {
  if (typeof value !== 'string' || value.length === 0) fail('every file path must be a non-empty string');
  if (value !== value.trim() || /[\0\r\n\\]/.test(value)) fail(`unsafe path ${JSON.stringify(value)}`);
  if (value.startsWith('/') || value.endsWith('/') || value.includes('//')) fail(`ambiguous path ${JSON.stringify(value)}`);
  const parts = value.split('/');
  if (parts.some((part) => part === '' || part === '.' || part === '..')) fail(`ambiguous path ${JSON.stringify(value)}`);
  return value;
}

function matches(path, prefix) {
  return path === prefix || path.startsWith(`${prefix}/`);
}

function isDocumentation(path) {
  return matches(path, 'docs') || matches(path, 'website') ||
    /(^|\/)(README|CHANGELOG|CONTRIBUTING|CODE_OF_CONDUCT)\.md$/i.test(path) ||
    path.endsWith('.md') || path.endsWith('.mdx') || path.endsWith('.rst');
}

function isMemoryRanking(path) {
  return matches(path, 'server/crates/djinn-memory-eval') || new Set([
    'server/crates/djinn-db/src/repositories/note/search.rs',
    'server/crates/djinn-db/src/repositories/note/rrf.rs',
    'server/crates/djinn-db/src/repositories/note/scoring.rs',
    'server/crates/djinn-db/src/repositories/note/context.rs',
  ]).has(path);
}

function isMigration(path) {
  return /^server\/crates\/[^/]+\/migrations_postgres\//.test(path);
}

function isTiltContract(path) {
  return path === 'Tiltfile' ||
    matches(path, 'scripts/tilt') ||
    matches(path, 'scripts/kind') ||
    new Set([
      'scripts/test-tilt-warm-restart.sh',
      'scripts/test-tiltfile-config.sh',
      'scripts/test-wrap-agent-runtime-image.sh',
    ]).has(path) ||
    matches(path, 'server/docker') ||
    path === 'deploy/helm/djinn/values.local.yaml' ||
    matches(path, 'deploy/langfuse-local');
}

// Server-side files that are INPUTS to ui/ codegen (see
// ui/scripts/generate-mcp-types.ts and ui/scripts/generate-usage-types.ts).
// Changing one without regenerating the checked-in ui golden stales main for
// the next UI-touching run (mcp:types:check / usage:types:check fail), so
// these must select the ui lane even though they live under server/.
const UI_CODEGEN_INPUTS = new Set([
  'server/src/server/tests/snapshots/djinn_server__server__tests__tool_schemas__mcp_tools_schema.snap',
  'server/schemas/usage-analytics.schema.json',
]);

function isUnclassifiedNonDocumentation(path) {
  // Documentation and the explicitly enumerated local-dev contract are the
  // only narrow lanes. Every other executable/configuration path receives
  // fail-closed full validation.
  return !isDocumentation(path) && !matches(path, 'ui') && !matches(path, 'server') &&
    !matches(path, '.github/workflows') && !isTiltContract(path);
}

/**
 * Classify normalized paths into stable pbv9 lanes. `unknown` is deliberately
 * fail-closed: it selects full validation rather than silently skipping policy.
 */
export function classifyChangedScope(files) {
  if (!Array.isArray(files)) fail('files must be an array');
  const normalizedFiles = files.map(normalizePath).sort();
  if (new Set(normalizedFiles).size !== normalizedFiles.length) fail('duplicate file paths are ambiguous');
  const lanes = {
    docs: normalizedFiles.length > 0 && normalizedFiles.every(isDocumentation),
    ui: false,
    rustCore: false,
    migrations: false,
    sqlx: false,
    sandboxAarch64: false,
    memoryRanking: false,
    tiltContracts: false,
    workflowCi: false,
    unknown: false,
  };
  for (const path of normalizedFiles) {
    if (matches(path, 'ui') || UI_CODEGEN_INPUTS.has(path)) lanes.ui = true;
    if (matches(path, 'server')) lanes.rustCore = true;
    if (isMigration(path)) lanes.migrations = true;
    if (matches(path, 'server/.sqlx')) lanes.sqlx = true;
    if (
      matches(path, 'server/crates/djinn-sandbox') ||
      matches(path, 'server/crates/workspace-hack') ||
      matches(path, 'server/.cargo') ||
      path === 'server/Cargo.lock' ||
      path === 'server/rust-toolchain.toml'
    ) lanes.sandboxAarch64 = true;
    if (isMemoryRanking(path)) lanes.memoryRanking = true;
    if (isTiltContract(path)) lanes.tiltContracts = true;
    if (matches(path, '.github/workflows')) lanes.workflowCi = true;
    if (isUnclassifiedNonDocumentation(path)) lanes.unknown = true;
  }
  // A source/configuration path that is not a recognized UI, server, or local
  // Tilt contract is unsafe to route narrowly.
  if (normalizedFiles.length === 0 || normalizedFiles.some(isUnclassifiedNonDocumentation)) lanes.unknown = true;
  return { files: normalizedFiles, lanes };
}

function validateInput(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) fail('input must be an object');
  const { event, base, head, files, fullValidation = false } = input;
  if (!EVENTS.has(event)) fail(`unsupported event ${JSON.stringify(event)}`);
  for (const [name, value] of [['base', base], ['head', head]]) {
    if (typeof value !== 'string' || value.length === 0 || value !== value.trim() || /[\0\r\n]/.test(value)) fail(`${name} must be an explicit non-empty revision`);
  }
  if (typeof fullValidation !== 'boolean') fail('fullValidation must be a boolean');
  return { event, base, head, files, fullValidation };
}

/** Return the one result object used for both manifest JSON and GitHub outputs. */
export function planChangedScope(input) {
  const { event, base, head, files, fullValidation } = validateInput(input);
  const classified = classifyChangedScope(files);
  const { lanes } = classified;
  const jobs = Object.fromEntries(JOB_NAMES.map((name) => [name, false]));
  const serverSelected = lanes.rustCore || lanes.workflowCi || lanes.unknown || fullValidation;

  if (serverSelected) for (const name of PROTECTED_SERVER_JOBS) jobs[name] = true;
  if (lanes.sandboxAarch64 || lanes.workflowCi || fullValidation || lanes.unknown) jobs.aarch64 = true;
  if (lanes.memoryRanking || lanes.workflowCi || fullValidation || lanes.unknown) jobs.memoryEval = true;
  if (lanes.tiltContracts || lanes.workflowCi || fullValidation || lanes.unknown) jobs.tiltContracts = true;
  if (lanes.ui || lanes.workflowCi || fullValidation || lanes.unknown) jobs.ui = true;

  // Merge queues must validate the server policy set even for docs-only changes;
  // UI remains intentionally independent so docs queue entries do not pay it.
  if (event === 'merge_group' && lanes.docs && !lanes.unknown) {
    for (const name of PROTECTED_SERVER_JOBS) jobs[name] = true;
  }

  const outputs = Object.fromEntries(Object.entries(jobs).map(([name, selected]) => [name, String(selected)]));
  return {
    version: 1,
    event,
    base,
    head,
    fullValidation,
    files: classified.files,
    lanes,
    jobs,
    outputs,
  };
}

// Alias retained for callers that name the primitive after its classification role.
export const classifyAndPlanChangedScope = planChangedScope;

function parseArgs(argv) {
  const input = { files: [], fullValidation: false };
  let githubOutput;
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const value = () => {
      index += 1;
      if (index >= argv.length) fail(`${arg} requires a value`);
      return argv[index];
    };
    if (arg === '--event') input.event = value();
    else if (arg === '--base') input.base = value();
    else if (arg === '--head') input.head = value();
    else if (arg === '--file') input.files.push(value());
    else if (arg === '--files-json') {
      try {
        const parsed = JSON.parse(value());
        if (!Array.isArray(parsed)) fail('--files-json must be a JSON array');
        input.files.push(...parsed);
      } catch (error) {
        if (error.message.startsWith('ci-changed-scope:')) throw error;
        fail('--files-json must be a JSON array');
      }
    } else if (arg === '--files-file') {
      const contents = readFileSync(value(), 'utf8');
      input.files.push(...contents.split(/\r?\n/).filter(Boolean));
    } else if (arg === '--full-validation') input.fullValidation = true;
    else if (arg === '--github-output') githubOutput = value();
    else if (arg === '--help') return null;
    else fail(`unknown argument ${JSON.stringify(arg)}`);
  }
  return { input, githubOutput };
}

function main() {
  const parsed = parseArgs(process.argv.slice(2));
  if (parsed === null) {
    process.stdout.write('usage: ci-changed-scope.mjs --event EVENT --base BASE --head HEAD [--file PATH | --files-json JSON | --files-file PATH] [--full-validation] [--github-output PATH]\n');
    return;
  }
  const result = planChangedScope(parsed.input);
  process.stdout.write(`${JSON.stringify(result)}\n`);
  if (parsed.githubOutput) {
    appendFileSync(parsed.githubOutput, `${Object.entries(result.outputs).map(([key, value]) => `${key}=${value}`).join('\n')}\n`);
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try { main(); } catch (error) { process.stderr.write(`${error.message}\n`); process.exitCode = 2; }
}
