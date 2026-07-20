import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';

const workflow = readFileSync(resolve('.github/workflows/quality-gate.yml'), 'utf8');
const smoke = readFileSync(resolve('scripts/image-ci/run-mold-trixie-apt-smoke.sh'), 'utf8');
const installer = readFileSync(resolve('server/crates/djinn-image-builder/scripts/install-rust.sh'), 'utf8');
const runtimeDockerfile = readFileSync(resolve('server/docker/djinn-agent-runtime-base.Dockerfile'), 'utf8');

function preflightBlock(source) {
  const start = source.indexOf('\n  preflight:\n');
  assert.ok(start >= 0, 'Quality Gate must retain an always-run preflight job');
  // Match the next top-level job key: a newline followed by exactly two
  // spaces and a non-whitespace character. This avoids matching the
  // four-/six-space indented lines inside the preflight job body, which
  // a bare '\n  ' substring search would incorrectly latch onto.
  const rest = source.slice(start + 15);
  const nextRel = rest.search(/\n  \S/);
  assert.ok(nextRel >= 0, 'Quality Gate must retain a job following preflight');
  const end = start + 15 + nextRel;
  return source.slice(start, end);
}

function assertSmokeContract(source) {
  assert.match(source, /docker run --rm[\s\\]+(?:.|\n)*debian:trixie-slim/s,
    'smoke must execute Docker from current debian:trixie-slim');
  assert.match(source, /source_files\(\)/,
    'smoke must retain and compare the base-distribution apt sources');
  assert.match(source, /! -path \/etc\/apt\/sources\.list\.d\/mold-snapshot\.list/,
    'only the added mold source may be excluded from the base-source comparison');
  assert.match(source, /> \/etc\/apt\/sources\.list\.d\/mold-snapshot\.list/,
    'smoke must add the snapshot as a separate apt source');
  assert.doesNotMatch(source, /> \/etc\/apt\/sources\.list(?:\s|$)/,
    'smoke must not replace /etc/apt/sources.list');
  assert.match(source, /apt-get update/,
    'smoke must resolve apt metadata rather than linting shell text');
  assert.match(source, /apt-get install -y --no-install-recommends "mold=\$MOLD_VERSION"/,
    'smoke must install the exact canonical mold package');
  assert.match(source, /probe-mold-compatibility\.sh --evidence-dir \/evidence/,
    'smoke must retain raw version/help evidence through the fail-closed probe');
  assert.match(source, /RUST_INSTALLER=.*install-rust\.sh/);
  assert.match(source, /RUNTIME_DOCKERFILE=.*djinn-agent-runtime-base\.Dockerfile/);
  assert.match(source, /runtime-base mold snapshot or package version differs/,
    'smoke must reject cross-path pin drift before Docker runs');
}

test('Quality Gate executes the trixie apt smoke for pull requests and merge groups', () => {
  assert.match(workflow, /^  pull_request:/m);
  assert.match(workflow, /^  merge_group:/m);
  const preflight = preflightBlock(workflow);
  assert.doesNotMatch(preflight, /^    if:/m,
    'preflight must not conditionally bypass the required smoke');
  assert.match(preflight, /name: Exercise pinned mold apt stanza on current trixie/);
  assert.match(preflight, /run: \.\/scripts\/image-ci\/run-mold-trixie-apt-smoke\.sh/);
});

test('trixie apt smoke uses canonical pins, preserves sources, installs, and probes', () => {
  assertSmokeContract(smoke);
  assert.match(installer, /readonly DEBIAN_SNAPSHOT_URL=/);
  assert.match(installer, /readonly MOLD_VERSION=/);
  assert.match(runtimeDockerfile, /ARG DEBIAN_SNAPSHOT_URL=/);
  assert.match(runtimeDockerfile, /ARG MOLD_VERSION=/);
});

test('contract rejects mutations that turn the smoke into lint or bypass evidence', () => {
  for (const [label, mutated] of [
    ['missing apt update', smoke.replace('apt-get update', 'true')],
    ['missing package install', smoke.replace('apt-get install -y --no-install-recommends "mold=$MOLD_VERSION"', 'true')],
    ['replaced base source', smoke.replace('mold-snapshot.list', 'sources.list')],
    ['missing compatibility probe', smoke.replace('probe-mold-compatibility.sh', 'skipped-probe.sh')],
  ]) {
    assert.throws(() => assertSmokeContract(mutated), label);
  }

  assert.throws(
    () => assert.match(preflightBlock(workflow.replace('run: ./scripts/image-ci/run-mold-trixie-apt-smoke.sh', 'run: true')), /run: \.\/scripts\/image-ci\/run-mold-trixie-apt-smoke\.sh/),
    'workflow contract must reject a non-executing smoke step',
  );
  assert.throws(
    () => assert.match(workflow.replace(/^  pull_request:\n/m, ''), /^  pull_request:/m),
    'workflow contract must reject omission from pull_request runs',
  );
  assert.throws(
    () => assert.match(workflow.replace(/^  merge_group: null\n/m, ''), /^  merge_group:/m),
    'workflow contract must reject omission from merge_group runs',
  );
});
