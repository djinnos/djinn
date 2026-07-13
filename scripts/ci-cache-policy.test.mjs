import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';

const WORKFLOW = resolve('.github/workflows/quality-gate.yml');

// Each family has exactly one job permitted to save it. Adding a family requires
// adding its owner here before it can be used in the quality-gate workflow.
const CACHE_OWNERS = new Map([
  ['server-quality', 'cache-warm-x86_64-quality'],
  ['server-test', 'cache-warm-x86_64-test'],
  ['server-aarch64-check', 'cache-warm-aarch64'],
]);

function fail(message) {
  throw new Error(`ci-cache-policy: ${message}`);
}

function scalar(value) {
  return value.trim().replace(/\s+#.*$/, '').replace(/^['"]|['"]$/g, '');
}

/**
 * Extract job blocks without a YAML dependency. Job identifiers are structural
 * YAML keys at jobs' indentation; step contents stay opaque except for cache
 * action fields. This intentionally tolerates comments and field reordering.
 */
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
      continue;
    }
    if (current) current.lines.push({ text: line, number: index + 1 });
  }
  return { lines, jobs };
}

function cacheSteps(job) {
  const steps = [];
  let current;
  for (const line of job.lines) {
    if (/^ {6}-\s+/.test(line.text)) {
      if (current) steps.push(current);
      current = { lines: [line], uses: null };
    } else if (current) {
      current.lines.push(line);
    }
    if (current) {
      const uses = line.text.match(/^(?: {6}-\s+| {8})uses:\s*(\S+)/);
      if (uses) current.uses = scalar(uses[1]);
    }
  }
  if (current) steps.push(current);
  return steps.filter((step) => /^(?:Swatinem\/rust-cache@|actions\/cache(?:@|\/[^/]+@))/i.test(step.uses ?? ''));
}

function field(step, name) {
  const expression = new RegExp(`^ {10}${name}:\\s*(.+)$`);
  const line = step.lines.find(({ text }) => expression.test(text));
  return line ? scalar(line.text.match(expression)[1]) : undefined;
}

function isTrue(value) {
  return value !== undefined && /^true$/i.test(value);
}

function isFalse(value) {
  return value !== undefined && /^false$/i.test(value);
}

function workflowEvents(parsed) {
  const trigger = parsed.lines.slice(0, parsed.lines.findIndex((line) => /^jobs:/.test(line)));
  return trigger
    .map((line) => line.match(/^ {2}([A-Za-z_][A-Za-z0-9_]*):\s*(?:null)?\s*(?:#.*)?$/)?.[1])
    .filter((event) => event && event !== 'workflow_call');
}

/**
 * Evaluate the event-name portion of a job condition structurally. Unknown
 * predicates (such as preflight outputs) are treated as satisfiable, while
 * boolean literals and mutually-exclusive event tests are evaluated. This
 * catches `if: false` and contradictory event routes without pretending to
 * execute GitHub expressions.
 */
function conditionAllowsEvent(condition, event) {
  if (!condition) return true;
  const expression = condition
    .replace(/^\s*\$\{\{\s*|\s*\}\}\s*$/g, '')
    .replace(/github\.event_name\s*(==|!=)\s*(['"])([^'"]+)\2/g,
      (_, operator, _quote, candidate) => String(operator === '==' ? event === candidate : event !== candidate));
  const tokens = [];
  let atom = '';
  const pushAtom = () => {
    if (atom.trim()) tokens.push({ type: 'atom', value: atom.trim() });
    atom = '';
  };
  for (let index = 0; index < expression.length; index += 1) {
    const character = expression[index];
    if (character === '&' && expression[index + 1] === '&') {
      pushAtom(); tokens.push({ type: 'and' }); index += 1;
    } else if (character === '|' && expression[index + 1] === '|') {
      pushAtom(); tokens.push({ type: 'or' }); index += 1;
    } else if (character === '!' && expression[index + 1] !== '=') {
      pushAtom(); tokens.push({ type: 'not' });
    } else if (character === '(' || character === ')') {
      pushAtom(); tokens.push({ type: character });
    } else {
      atom += character;
    }
  }
  pushAtom();

  let cursor = 0;
  const primary = () => {
    const token = tokens[cursor++];
    if (!token) fail(`cannot parse owner condition ${condition}`);
    if (token.type === 'not') return !primary();
    if (token.type === '(') {
      const value = disjunction();
      if (tokens[cursor++]?.type !== ')') fail(`cannot parse owner condition ${condition}`);
      return value;
    }
    if (token.type !== 'atom') fail(`cannot parse owner condition ${condition}`);
    return !/^false$/i.test(token.value);
  };
  const conjunction = () => {
    let value = primary();
    while (tokens[cursor]?.type === 'and') { cursor += 1; value = primary() && value; }
    return value;
  };
  const disjunction = () => {
    let value = conjunction();
    while (tokens[cursor]?.type === 'or') { cursor += 1; value = conjunction() || value; }
    return value;
  };
  const allowed = disjunction();
  if (cursor !== tokens.length) fail(`cannot parse owner condition ${condition}`);
  return allowed;
}

function jobNeeds(job) {
  const scalarNeed = job.lines.find(({ text }) => /^ {4}needs:\s*\S/.test(text));
  if (scalarNeed) return [scalar(scalarNeed.text.replace(/^ {4}needs:\s*/, ''))];
  const needsAt = job.lines.findIndex(({ text }) => /^ {4}needs:\s*(?:#.*)?$/.test(text));
  if (needsAt < 0) return [];
  const needs = [];
  for (const { text } of job.lines.slice(needsAt + 1)) {
    const match = text.match(/^ {6}-\s+([A-Za-z0-9_-]+)\s*(?:#.*)?$/);
    if (match) needs.push(match[1]);
    else if (/^ {4}\S/.test(text)) break;
  }
  return needs;
}

function jobCanRun(parsed, job, event, visiting = new Set()) {
  if (!job || visiting.has(job.id)) return false;
  const condition = job.lines.find(({ text }) => /^ {4}if:\s*/.test(text))?.text.replace(/^ {4}if:\s*/, '');
  if (!conditionAllowsEvent(condition, event)) return false;
  const nextVisiting = new Set(visiting).add(job.id);
  return jobNeeds(job).every((dependency) => jobCanRun(parsed, parsed.jobs.get(dependency), event, nextVisiting));
}

function assertOwnerReachable(parsed, family, owner) {
  const job = parsed.jobs.get(owner);
  assert.ok(job, `declared owner ${owner} for ${family} is missing`);
  const reachableEvents = workflowEvents(parsed).filter((event) => jobCanRun(parsed, job, event));
  assert.ok(reachableEvents.length > 0,
    `declared owner ${owner} for ${family} is unreachable from every workflow trigger`);
}

function assertMainAndDispatchReachable(parsed, job) {
  const trigger = parsed.lines.slice(0, parsed.lines.findIndex((line) => /^jobs:/.test(line))).join('\n');
  assert.match(trigger, /^ {2}push:\s*\n(?:.*\n)*?^ {4}branches:\s*\n(?:.*\n)*?^ {6}- main\s*$/m,
    'main push must trigger the workflow');
  assert.match(trigger, /^ {2}workflow_dispatch:\s*(?:null)?\s*$/m,
    'workflow_dispatch must trigger the workflow');
  const condition = job.lines.filter(({ text }) => /^ {4}if:/.test(text)).map(({ text }) => text).join(' ');
  assert.match(condition, /github\.event_name\s*==\s*['"]push['"]/, 'cache-warm-aarch64 must be reachable from main');
  assert.match(condition, /github\.event_name\s*==\s*['"]workflow_dispatch['"]/, 'cache-warm-aarch64 must be reachable from workflow_dispatch');
}

test('quality-gate has one saving owner and restore-only consumers per cache family', () => {
  const parsed = parseJobs(readFileSync(WORKFLOW, 'utf8'));
  const saves = new Map([...CACHE_OWNERS.keys()].map((family) => [family, []]));

  for (const job of parsed.jobs.values()) {
    for (const step of cacheSteps(job)) {
      const action = step.uses.toLowerCase();
      const rustCache = action.startsWith('swatinem/rust-cache@');
      const family = rustCache ? field(step, 'shared-key') : field(step, 'key');
      if (!family) fail(`${job.id}:${step.lines[0].number} cache action lacks a declared cache family`);
      if (!CACHE_OWNERS.has(family)) fail(`${job.id}:${step.lines[0].number} uses undeclared cache family ${family}`);

      // rust-cache saves unless save-if is explicitly false. actions/cache and
      // actions/cache/save can save; only actions/cache/restore is restore-only.
      const savesCache = rustCache ? !isFalse(field(step, 'save-if')) : !action.startsWith('actions/cache/restore@');
      if (savesCache) saves.get(family).push(job.id);

      const owner = CACHE_OWNERS.get(family);
      if (job.id !== owner) {
        assert.equal(savesCache, false,
          `${job.id} is a restore-only consumer of ${family}; set save-if: false or use actions/cache/restore`);
      } else if (rustCache) {
        assert.ok(isTrue(field(step, 'save-if')), `${owner} must explicitly set save-if: true for ${family}`);
      }
    }
  }

  for (const [family, owner] of CACHE_OWNERS) {
    assertOwnerReachable(parsed, family, owner);
    assert.deepEqual(saves.get(family), [owner], `${family} must have exactly one saving owner: ${owner}`);
  }

  assertMainAndDispatchReachable(parsed, parsed.jobs.get('cache-warm-aarch64'));
});
