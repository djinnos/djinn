#!/usr/bin/env node
/** Offline, fail-closed finite-timeout contract for GitHub Actions workflows. */
import { readFileSync, statSync } from 'node:fs';
import { resolve } from 'node:path';
import { parseDocument, isAlias, isMap, isScalar, isSeq } from 'yaml';

const JOB = /^[A-Za-z_][A-Za-z0-9_-]*$/;
const LOCAL = /^\.\/\.github\/workflows\/[A-Za-z0-9_.-]+\.(?:yml|yaml)$/;
const errors = [];
function fail(where, code, message, node) { errors.push({ where, code, message, pos: node?.range?.[0] ?? -1 }); }
function text(node) { return isScalar(node) && !node.tag && typeof node.value === 'string' ? node.value : null; }
function field(map, name) { return isMap(map) ? map.items.find((item) => text(item.key) === name)?.value : undefined; }
function mapField(map, name) { const node = field(map, name); return isMap(node) ? node : null; }
function safeNode(node) {
  if (!node || isAlias(node) || node.anchor || node.tag) return false;
  if (isScalar(node)) return !(typeof node.value === 'number' && !Number.isFinite(node.value));
  if (isSeq(node)) return node.items.every(safeNode);
  if (isMap(node)) return node.items.every((item) => text(item.key) !== '<<' && safeNode(item.key) && safeNode(item.value));
  return false;
}
function parseWorkflow(root, path) {
  const filename = resolve(root, path);
  try { if (!statSync(filename).isFile()) throw new Error('not file'); } catch { fail(path, 'UNRESOLVED_WORKFLOW', 'workflow file does not exist'); return null; }
  let source;
  try { source = readFileSync(filename, 'utf8'); } catch { fail(path, 'UNRESOLVED_WORKFLOW', 'workflow file cannot be read'); return null; }
  const document = parseDocument(source, { strict: true, uniqueKeys: true, prettyErrors: false, merge: false, schema: 'core' });
  for (const error of document.errors) fail(path, error.code === 'DUPLICATE_KEY' ? 'DUPLICATE_KEY' : 'YAML_SYNTAX', 'invalid YAML document', { range: error.pos });
  if (document.errors.length) return null;
  if (!isMap(document.contents) || document.contents.anchor || document.contents.tag) { fail(path, 'YAML_SYNTAX', 'workflow root must be an untagged mapping', document.contents); return null; }
  return { path, root: document.contents };
}
function loadWorkflow(root, path, cache) {
  if (cache.has(path)) return cache.get(path);
  const parsed = parseWorkflow(root, path); if (!parsed) return null;
  const jobs = mapField(parsed.root, 'jobs');
  if (!jobs || jobs.anchor || jobs.tag || !jobs.items.length) { fail(path, 'UNRESOLVED_WORKFLOW', 'workflow must contain a nonempty jobs mapping', parsed.root); return null; }
  const wf = { ...parsed, jobs: new Map() }; cache.set(path, wf);
  for (const item of jobs.items) {
    const id = text(item.key);
    if (!id || !JOB.test(id)) { fail(path, 'INVALID_JOB_ID', 'job ID must be a literal identifier', item.key); continue; }
    if (!isMap(item.value) || item.value.anchor || item.value.tag) { fail(`${path}#${id}`, 'MALFORMED_JOB', 'job must be an untagged mapping', item.value); continue; }
    wf.jobs.set(id, item.value);
  }
  return wf;
}
function readManifest(root, path) {
  let manifest;
  try { manifest = JSON.parse(readFileSync(resolve(root, path), 'utf8')); } catch { fail(path, 'MANIFEST_SCHEMA', 'manifest must be valid JSON'); return null; }
  if (!manifest || Object.getPrototypeOf(manifest) !== Object.prototype || Object.keys(manifest).sort().join(',') !== 'covered,terminalRoots,version' || manifest.version !== 1) { fail(path, 'MANIFEST_SCHEMA', 'expected exact v1 manifest shape'); return null; }
  let valid = true;
  for (const name of ['terminalRoots', 'covered']) {
    const values = manifest[name];
    if (!Array.isArray(values) || !values.length || values.some((value) => typeof value !== 'string') || new Set(values).size !== values.length || values.join('\0') !== [...values].sort().join('\0')) { fail(path, 'MANIFEST_SCHEMA', `${name} must be nonempty, unique, and sorted`); valid = false; }
  }
  if (valid && manifest.terminalRoots.some((rootId) => !manifest.covered.includes(rootId))) { fail(path, 'MANIFEST_SCHEMA', 'terminalRoots must be covered identities'); valid = false; }
  return valid ? manifest : null;
}
function validTimeout(node) { return isScalar(node) && !node.tag && typeof node.value === 'number' && Number.isInteger(node.value) && Number.isFinite(node.value) && node.value >= 1 && node.value <= 120 && /^\d+$/.test(String(node.source ?? '')); }
function validateJob(wf, node, identity, root, cache, callFiles) {
  const needsNode = field(node, 'needs'); let needs = [];
  if (needsNode !== undefined) {
    const one = text(needsNode);
    if (one !== null && JOB.test(one)) needs = [one];
    else if (isSeq(needsNode) && needsNode.items.length && needsNode.items.every((entry) => text(entry) !== null && JOB.test(text(entry)))) needs = needsNode.items.map(text);
    else fail(identity, 'INVALID_NEEDS', 'needs must be a literal job ID or nonempty list', needsNode);
    if (new Set(needs).size !== needs.length) fail(identity, 'INVALID_NEEDS', 'needs must not contain duplicates', needsNode);
    for (const dependency of needs) if (!wf.jobs.has(dependency)) fail(identity, 'UNKNOWN_NEED', `unknown need ${dependency}`, needsNode);
  }
  const ifNode = field(node, 'if'); if (ifNode !== undefined && (text(ifNode) === null || !text(ifNode).trim())) fail(identity, 'UNSUPPORTED_IF', 'if must be a nonempty string', ifNode);
  const strategy = field(node, 'strategy'); if (strategy !== undefined && (!isMap(strategy) || !safeNode(strategy))) fail(identity, 'UNSUPPORTED_MATRIX', 'strategy must be an untagged mapping', strategy);
  const matrix = isMap(strategy) ? field(strategy, 'matrix') : undefined;
  if (matrix !== undefined && !(text(matrix)?.trim() || ((isMap(matrix) || isSeq(matrix)) && safeNode(matrix)))) fail(identity, 'UNSUPPORTED_MATRIX', 'matrix has unsupported shape', matrix);
  const uses = field(node, 'uses');
  if (uses !== undefined) {
    const target = text(uses);
    if (!target || !LOCAL.test(target)) { fail(identity, 'UNSUPPORTED_USES', 'uses must be a literal local workflow path', uses); return { needs, calls: [] }; }
    for (const name of ['timeout-minutes', 'runs-on', 'steps']) if (field(node, name) !== undefined) fail(identity, 'ILLEGAL_CALLER_TIMEOUT', `structural caller must not have ${name}`, field(node, name));
    const targetPath = target.slice(2);
    if (callFiles.includes(targetPath)) { fail(identity, 'WORKFLOW_CALL_CYCLE', `reusable workflow cycle through ${targetPath}`, uses); return { needs, calls: [] }; }
    const called = loadWorkflow(root, targetPath, cache); const on = called && mapField(called.root, 'on'); const workflowCall = on && field(on, 'workflow_call');
    if (!called || !on || workflowCall === undefined || !(isMap(workflowCall) || (isScalar(workflowCall) && workflowCall.value === null))) { fail(identity, 'UNRESOLVED_WORKFLOW', 'called workflow must declare literal on.workflow_call', uses); return { needs, calls: [] }; }
    return { needs, calls: [...called.jobs.keys()].sort().map((id) => ({ wf: called, id, callFiles: [...callFiles, targetPath] })) };
  }
  const runsOn = field(node, 'runs-on'); const steps = field(node, 'steps');
  if (runsOn === undefined || !isSeq(steps) || !steps.items.length) fail(identity, 'INVALID_EXECUTABLE', 'executable job requires runs-on and nonempty steps', node);
  const timeout = field(node, 'timeout-minutes'); if (timeout === undefined) fail(identity, 'MISSING_TIMEOUT', 'executable job requires timeout-minutes', node); else if (!validTimeout(timeout)) fail(identity, 'INVALID_TIMEOUT', 'timeout-minutes must be an integer from 1 through 120', timeout);
  return { needs, calls: [] };
}
function main() {
  const args = process.argv.slice(2); let root = process.cwd(); let manifestPath = '.github/ci-timeouts.json';
  for (let index = 0; index < args.length; index += 1) { if (args[index] === '--root' && args[index + 1]) root = resolve(args[++index]); else if (args[index] === '--manifest' && args[index + 1]) manifestPath = args[++index]; else { fail('arguments', 'MANIFEST_SCHEMA', 'unsupported argument'); return finish(); } }
  const manifest = readManifest(root, manifestPath); if (!manifest) return finish();
  const cache = new Map(), seen = new Set(), active = [];
  function visit(wf, id, prefix, callFiles) {
    const identity = `${prefix}${wf.path}#${id}`; const at = active.indexOf(identity);
    if (at !== -1) {
      const cycle = active.slice(at);
      const start = cycle.reduce((best, item, index) => item < cycle[best] ? index : best, 0);
      const rotated = cycle.slice(start).concat(cycle.slice(0, start));
      fail(identity, 'DEPENDENCY_CYCLE', `dependency cycle ${rotated.concat(rotated[0]).join(' -> ')}`);
      return;
    }
    if (seen.has(identity)) return; seen.add(identity); active.push(identity);
    const node = wf.jobs.get(id);
    if (node) { const result = validateJob(wf, node, identity, root, cache, callFiles); for (const need of [...result.needs].sort()) if (wf.jobs.has(need)) visit(wf, need, prefix, callFiles); for (const call of result.calls) visit(call.wf, call.id, `${identity}=>`, call.callFiles); }
    active.pop();
  }
  for (const rootId of manifest.terminalRoots) { const marker = rootId.lastIndexOf('#'); const path = rootId.slice(0, marker); const id = rootId.slice(marker + 1); if (marker < 1 || !JOB.test(id) || !path.startsWith('.github/workflows/')) { fail(rootId, 'MANIFEST_SCHEMA', 'terminal root must be a canonical identity'); continue; } const wf = loadWorkflow(root, path, cache); if (!wf || !wf.jobs.has(id)) fail(rootId, 'UNRESOLVED_WORKFLOW', 'terminal root does not resolve'); else visit(wf, id, '', [path]); }
  for (const identity of [...seen].filter((identity) => !manifest.covered.includes(identity)).sort()) fail(identity, 'MISSING_COVERED', 'missing covered identity');
  for (const identity of manifest.covered.filter((identity) => !seen.has(identity)).sort()) fail(identity, 'EXTRA_COVERED', 'extra covered identity'); finish();
}
function finish() { if (!errors.length) console.log('ci-timeouts: OK'); else { errors.sort((left, right) => left.where.localeCompare(right.where) || left.code.localeCompare(right.code) || left.message.localeCompare(right.message)); for (const error of errors) console.error(`ci-timeouts: ${error.where}: ${error.code}: ${error.message}`); process.exitCode = 1; } }
main();
