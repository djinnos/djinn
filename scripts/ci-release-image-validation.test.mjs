import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';

const workflow = readFileSync(resolve('.github/workflows/release.yml'), 'utf8');
const generatedDockerfile = readFileSync(resolve('scripts/image-ci/generated-rust.Dockerfile'), 'utf8');

function runtimeBaseBlock() {
  const start = workflow.indexOf('\n  runtime-base:\n');
  const end = workflow.indexOf('\n  ui-dist:\n', start);
  assert.ok(start >= 0 && end > start, 'release workflow must retain a runtime-base job');
  return workflow.slice(start, end);
}

test('runtime-base publication is ordered after local image validation', () => {
  const job = runtimeBaseBlock();
  const runtimeBuild = job.indexOf('Build runtime-base locally');
  const generatedBuild = job.indexOf('Build representative generated Rust image');
  const compatibility = job.indexOf('Validate mold compatibility for both installation paths');
  const smoke = job.indexOf('Run runtime-base mold thread-cap smoke');
  const evidence = job.indexOf('Upload mold image validation evidence');
  const publish = job.indexOf('Publish validated runtime-base image');
  for (const [name, index] of Object.entries({ runtimeBuild, generatedBuild, compatibility, smoke, evidence, publish })) {
    assert.ok(index >= 0, `runtime-base must include ${name}`);
  }
  assert.ok(runtimeBuild < compatibility && generatedBuild < compatibility,
    'both installation paths must be built before compatibility validation');
  assert.ok(compatibility < smoke && smoke < evidence && evidence < publish,
    'compatibility, smoke, and evidence retention must precede publication');
  assert.equal(job.slice(0, publish).match(/push:\s*true/g), null,
    'publication must not occur before validation');
  assert.match(job.slice(publish), /push:\s*true/,
    'validated runtime-base publication must use an explicit push');
  assert.match(job, /if:\s*always\(\)/,
    'evidence upload must survive failed validation');
});

test('generated Rust validation uses the real installer and runtime smoke has explicit N', () => {
  assert.match(generatedDockerfile, /COPY server\/crates\/djinn-image-builder\/scripts\//,
    'representative image must use the generated-image script bundle');
  assert.match(generatedDockerfile, /install-rust\.sh/,
    'representative image must exercise install-rust.sh');
  const job = runtimeBaseBlock();
  assert.match(job, /probe-mold-compatibility\.sh --evidence-dir evidence\/runtime-base/);
  assert.match(job, /probe-mold-compatibility\.sh --evidence-dir evidence\/generated-rust/);
  assert.match(job, /run-mold-thread-smoke\.sh --threads 4 --evidence-dir evidence\/runtime-base/,
    'runtime smoke must specify a deterministic explicit thread count');
});
