import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';

const workflow = readFileSync(resolve('.github/workflows/quality-gate.yml'), 'utf8');
const smoke = readFileSync(resolve('scripts/image-ci/run-mold-trixie-apt-smoke.sh'), 'utf8');
const installer = readFileSync(resolve('server/crates/djinn-image-builder/scripts/install-rust.sh'), 'utf8');
const runtimeDockerfile = readFileSync(resolve('server/docker/djinn-agent-runtime-base.Dockerfile'), 'utf8');

// The smoke lives in `ci-contracts`, not `preflight`: it is path-independent,
// so it runs beside the routed lanes instead of blocking them (every other job
// `needs: preflight`). What this contract pins is the always-run property, not
// the job name — the block must exist and must carry no `if:` guard.
const CONTRACTS_JOB = 'ci-contracts';

function jobBlock(source, job = CONTRACTS_JOB) {
  const marker = `\n  ${job}:\n`;
  const start = source.indexOf(marker);
  assert.ok(start >= 0, `Quality Gate must retain an always-run ${job} job`);
  // Match the next top-level job key: a newline followed by exactly two
  // spaces and a non-whitespace character. This avoids matching the
  // four-/six-space indented lines inside the job body, which a bare
  // '\n  ' substring search would incorrectly latch onto.
  const rest = source.slice(start + marker.length);
  const nextRel = rest.search(/\n  \S/);
  assert.ok(nextRel >= 0, `Quality Gate must retain a job following ${job}`);
  const end = start + marker.length + nextRel;
  return source.slice(start, end);
}

function assertSmokeContract(source) {
  assert.match(source, /image="debian:trixie-slim"/,
    'smoke must name the current trixie image once for retryable pulling');
  assert.match(source, /for attempt in 1 2 3; do[\s\S]*docker pull "\$image"[\s\S]*sleep "\$attempt"/,
    'smoke must retry transient registry failures before running the probe');
  assert.match(source, /docker run --rm[\s\\]+--pull=never/,
    'smoke must run the image it successfully pulled rather than issuing a second pull');
  assert.match(source, /docker run --rm[\s\\]+(?:.|\n)*"\$image"/s,
    'smoke must execute Docker from the current trixie image');
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
  const contracts = jobBlock(workflow);
  assert.doesNotMatch(contracts, /^    if:/m,
    'ci-contracts must not conditionally bypass the required smoke');
  assert.match(contracts, /name: Exercise pinned mold apt stanza on current trixie/);
  assert.match(contracts, /run: \.\/scripts\/image-ci\/run-mold-trixie-apt-smoke\.sh/);
  // The split is only safe while the quality gate still fails closed on this
  // job — otherwise moving the smoke out of preflight would make it advisory.
  assert.match(workflow, /^      - ci-contracts$/m,
    'quality-gate must depend on ci-contracts');
  assert.match(workflow, /\[ "\$CONTRACTS" = success \]/,
    'quality-gate must fail closed on the ci-contracts result');
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
    () => assert.match(jobBlock(workflow.replace('run: ./scripts/image-ci/run-mold-trixie-apt-smoke.sh', 'run: true')), /run: \.\/scripts\/image-ci\/run-mold-trixie-apt-smoke\.sh/),
    'workflow contract must reject a non-executing smoke step',
  );
  assert.throws(
    () => assert.match(jobBlock(workflow.replace(/^      - ci-contracts\n/m, '')), /^      - ci-contracts$/m),
    'workflow contract must reject dropping ci-contracts from the quality gate',
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
