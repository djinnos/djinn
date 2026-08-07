// Guard: a CANCELLED run must never be laundered into a FAILED required check.
//
// THE DEFECT THIS EXISTS TO PREVENT
//
// Run 31137003502 on PR #3064 was cancelled externally at 01:14:02. Every lane
// had already SUCCEEDED at 01:12:26 and no job failed. The `Quality Gate`
// required check nevertheless ended `failure` and blocked the PR. The chain:
//
//   1. cancellation ended qa-smoke's "Precompile selected smoke test binaries"
//      as `cancelled`;
//   2. so "Run deterministic qa smoke and coverage gate" was `skipped`, and
//      qa/evidence/smoke-ci was never created;
//   3. but "Upload qa smoke evidence" was guarded `if: always()`, and
//      `always()` fires on cancellation too, so it RAN;
//   4. `actions/upload-artifact@v4` with `if-no-files-found: error` found
//      nothing, emitted `##[error]No files were found with the provided
//      path`, and FAILED the step;
//   5. that flipped qa-smoke's job result from `cancelled` to `failure`, and
//      the fail-closed Quality Gate reported a test-shaped failure.
//
// Nothing was broken. The failure was manufactured by the guard, and it also
// destroyed the diagnostic signal: the run reads as "qa-smoke broke".
//
// THE RULE
//
// An `actions/upload-artifact` step may combine a cancellation-permissive
// guard (`always()`) with `if-no-files-found: error` only if it cannot be
// reached with its producer skipped. Since that is not decidable from text,
// the combination is banned outright. Either the guard must exclude
// cancellation (`!cancelled()`, or `success() || failure()`), or the missing
// path must be non-fatal (`warn`/`ignore`).
//
// The ban is deliberately narrow. `always()` on a step that only echoes, or
// that tests for its own inputs (`if [[ -f "$SUMMARY" ]]`), cannot manufacture
// anything and is left alone — as are `always()` teardown steps, which are the
// whole point of `always()`.

import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import test from 'node:test';

import { scriptCode } from './lib/source-text.mjs';

const WORKFLOW_DIR = resolve('.github/workflows');

function workflowFiles() {
  return readdirSync(WORKFLOW_DIR)
    .filter((name) => name.endsWith('.yml') || name.endsWith('.yaml'))
    .sort();
}

// Split a workflow into steps by the `- ` list marker at any indentation that
// introduces a mapping. Comments are stripped first so prose about `always()`
// (there is a lot of it in this repo) is never mistaken for a guard.
function uploadSteps(source) {
  const lines = scriptCode(source).split('\n');
  const found = [];
  let current = null;

  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    const marker = line.match(/^(\s*)-\s+\S/);
    if (marker) {
      if (current) found.push(current);
      current = { indent: marker[1].length, lines: [line], start: index + 1 };
      continue;
    }
    if (!current) continue;
    // A line indented no further than the step's own `-` ends the step.
    const indent = line.search(/\S/);
    if (indent >= 0 && indent <= current.indent) {
      found.push(current);
      current = null;
      continue;
    }
    current.lines.push(line);
  }
  if (current) found.push(current);

  return found
    .map((step) => ({ ...step, text: step.lines.join('\n') }))
    .filter((step) => /\bactions\/upload-artifact@/.test(step.text));
}

function stepGuard(step) {
  return step.text.match(/^\s*if:\s*(.+?)\s*$/m)?.[1];
}

function ifNoFilesFound(step) {
  return step.text.match(/^\s*if-no-files-found:\s*(\S+)\s*$/m)?.[1];
}

// `always()` is true even when the run was cancelled. Any guard that reduces
// to it — bare, or ANDed with unrelated conditions as in
// `always() && steps.shard.outputs.active == 'true'` — is permissive.
function permitsCancellation(guard) {
  if (guard === undefined) return false; // default `if` is `success()`
  return /\balways\(\)/.test(guard);
}

test('no artifact upload can turn a cancelled run into a failed step', () => {
  const offenders = [];
  for (const file of workflowFiles()) {
    const source = readFileSync(join(WORKFLOW_DIR, file), 'utf8');
    for (const step of uploadSteps(source)) {
      const guard = stepGuard(step);
      if (!permitsCancellation(guard)) continue;
      if (ifNoFilesFound(step) !== 'error') continue;
      const name = step.text.match(/name:\s*(.+)/)?.[1] ?? '(unnamed)';
      offenders.push(`${file}:${step.start} "${name}" — if: ${guard}`);
    }
  }

  assert.deepEqual(offenders, [],
    'these upload steps combine a cancellation-permissive guard with '
    + '`if-no-files-found: error`, so a cancelled run manufactures a step '
    + 'failure on a lane where nothing broke. Use `!cancelled()` (or '
    + '`success() || failure()`), or downgrade to `warn`/`ignore`:\n'
    + offenders.join('\n'));
});

// The regexes above must actually catch the shipped defect, or this file is a
// guard that asserts nothing. Replay run 31137003502's exact step text.
test('the guard catches the run-31137003502 step shape', () => {
  const regressed = [
    'jobs:',
    '  qa-smoke:',
    '    steps:',
    '      - name: Upload qa smoke evidence',
    '        if: always()',
    '        uses: actions/upload-artifact@v4',
    '        with:',
    '          name: qa-smoke-evidence',
    '          path: qa/evidence/smoke-ci',
    '          if-no-files-found: error',
    '          retention-days: 30',
  ].join('\n');

  const [step] = uploadSteps(regressed);
  assert.ok(step, 'the step splitter must find the upload step');
  assert.equal(stepGuard(step), 'always()');
  assert.equal(ifNoFilesFound(step), 'error');
  assert.equal(permitsCancellation(stepGuard(step)), true);

  // ...and must NOT flag the fix, nor the safe variants already in the tree.
  assert.equal(permitsCancellation("${{ !cancelled() && steps.qa_smoke_run.outcome != 'skipped' }}"), false);
  assert.equal(permitsCancellation('success() || failure()'), false);
  assert.equal(permitsCancellation(undefined), false);
  assert.equal(permitsCancellation("always() && steps.shard.outputs.active == 'true'"), true);

  // A comment mentioning always() is prose, not a guard.
  const commented = regressed
    .replace('        if: always()', '        # once guarded by if: always()\n        if: !cancelled()');
  const [safe] = uploadSteps(commented);
  assert.equal(permitsCancellation(stepGuard(safe)), false,
    'a stripped comment must not be read as a step guard');
});
