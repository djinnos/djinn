// Contract tests for the retired Rust file-size gate.
//
// scripts/test-check-file-size.sh proves the SCRIPT cannot fail on an
// oversized file. It cannot see the workflow. These tests cover the other
// half: that the step wiring in quality-gate.yml cannot turn a size
// measurement back into a red build, and that retiring the veto did not
// leave the `quality-gate` rollup expecting a result nobody produces.
//
// Run with:
//   node --test scripts/ci-file-size-report.test.mjs

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';

const WORKFLOW = resolve('.github/workflows/quality-gate.yml');
const REPORT_STEP = 'Report changed Rust file sizes';
const SELFTEST_STEP = 'Self-test Rust file-size report';

function fail(message) {
  throw new Error(`ci-file-size-report: ${message}`);
}

const source = readFileSync(WORKFLOW, 'utf8').replace(/\r\n/g, '\n');
const lines = source.split('\n');

/** Slice the YAML block of the step whose `- name:` matches `name`. */
function stepBlock(name) {
  const startsAt = lines.findIndex((line) => line.trim() === `- name: ${name}`);
  if (startsAt < 0) fail(`workflow has no step named ${JSON.stringify(name)}`);
  const indent = lines[startsAt].indexOf('-');
  const block = [lines[startsAt]];
  for (let i = startsAt + 1; i < lines.length; i += 1) {
    const line = lines[i];
    // A blank line belongs to the step; a `- ` or dedent at or above the
    // step's own indent starts the next step or the next job key.
    if (line.trim() === '') { block.push(line); continue; }
    const lineIndent = line.search(/\S/);
    if (lineIndent <= indent) break;
    block.push(line);
  }
  return block.join('\n');
}

test('the file-size report step is routed, but cannot fail the run', () => {
  const block = stepBlock(REPORT_STEP);

  assert.match(
    block,
    /if:\s*needs\.preflight\.outputs\.size == 'true'/,
    'the report must stay routed by the `size` preflight output — an unrouted report is a deleted one',
  );

  // The retirement itself. Three independent reasons the step exits 0:
  // gating shell options are off, every script invocation is `||`-guarded,
  // and the block ends on an unconditional `exit 0`.
  assert.match(
    block,
    /set \+e \+o pipefail/,
    "the step must clear bash's inherited `-e`/`pipefail`, or a non-zero script exit fails the run",
  );
  assert.match(
    block,
    /check-file-size\.sh --files-from-stdin\s*\\?\s*\n?\s*\|\| echo "::warning/,
    'the script invocation must be `||`-guarded so a crash degrades to a warning',
  );
  assert.match(
    block.trimEnd(),
    /\n\s*exit 0$/,
    'the run block must end on an unconditional `exit 0`',
  );

  // A size finding must never be raised at error severity again.
  assert.doesNotMatch(
    block,
    /exit 1\b/,
    'the report step must contain no failing exit path',
  );
  assert.doesNotMatch(
    block,
    /::error/,
    'a large file is a notice, not an error annotation',
  );

  // `continue-on-error` would render the step as a failure GitHub then
  // ignores. That reads as "the size gate is broken" to every human
  // scanning the run; a report that succeeds is a different artifact from
  // a failure that is tolerated.
  assert.doesNotMatch(
    block,
    /continue-on-error/,
    'the report must genuinely succeed, not be a tolerated failure',
  );
});

test('the report script keeps its own self-test wired into the same lane', () => {
  const block = stepBlock(SELFTEST_STEP);
  assert.match(block, /if:\s*needs\.preflight\.outputs\.size == 'true'/);
  assert.match(block, /run:\s*\.\/scripts\/test-check-file-size\.sh/);

  // The self-test IS still a gate, deliberately: it fails when the reporter
  // is broken, never when a file is large. Without it, "report-only" would
  // be indistinguishable from "silently does nothing".
  const step = stepBlock(SELFTEST_STEP);
  assert.doesNotMatch(step, /continue-on-error/);
  assert.doesNotMatch(step, /\|\| true/);
});

test('the quality-gate rollup still expects a server-guards result on size work', () => {
  // Retiring the veto must not strand the rollup. `size` selects
  // `server-guards`, so the job still RUNS and still reports `success` — the
  // rollup's `selected=true result=success` arm. If the routing had been
  // dropped instead, `size` work would leave `server-guards` skipped while
  // some other output still selected it, and the rollup would fail closed on
  // a lane nobody meant to remove.
  const guardsJob = source.slice(source.indexOf('\n  server-guards:'));
  const guardsIf = guardsJob.slice(0, guardsJob.indexOf('runs-on:'));
  assert.match(
    guardsIf,
    /needs\.preflight\.outputs\.size == 'true'/,
    'server-guards must still be selected by the `size` output',
  );

  const rollup = source.slice(source.indexOf('\n  quality-gate:'));
  assert.match(
    rollup,
    /\|\| needs\.preflight\.outputs\.size == 'true'[\s\S]*?\}\}:\$\{\{ needs\.server-guards\.result \}\}/,
    'the rollup must keep pairing the `size` selector with the server-guards result',
  );
  assert.match(
    rollup,
    /check server-guards "\$GUARDS"/,
    'the rollup must still assert the server-guards lane',
  );
});
