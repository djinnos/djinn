// Self-test for the no-retirement gate (proposal 3i92, epic xowm, task 1j64).
//
// Every case here runs the SHELL ENTRY POINT
// `scripts/check-resize-cutover-retention.sh` and judges it by its EXIT CODE.
// That is deliberate and is the whole of AC11: making the shell script exit 0
// unconditionally has to turn this file red, so this file may never reach past
// it into the engine module for the pass/fail verdict.
//
// How a fixture is evaluated:
//
//   1. Ask the engine which paths it reads (`--list-inputs`).
//   2. Materialise exactly those paths from the REAL repository into a scratch
//      root. Copying only the gate's own inputs keeps this fast and, more
//      importantly, means a fixture cannot accidentally pass because some
//      unrelated file happened to satisfy a check.
//   3. Apply the fixture's operations.
//   4. Run the shell gate against the scratch root and assert the exit code —
//      and, for a rejection, that the failure output NAMES the asset the
//      fixture targeted. "It failed" is not evidence; "it failed for this
//      reason" is.
//
// Run it: node --test scripts/test-resize-cutover-retention.mjs

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
    copyFileSync,
    existsSync,
    mkdirSync,
    mkdtempSync,
    readFileSync,
    readdirSync,
    rmSync,
    writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import test from 'node:test';

import { ASSETS, MINIMUM_ASSETS, everyInput, stripComments } from './resize-cutover-retention-manifest.mjs';

const SCRIPT_DIR = dirname(new URL(import.meta.url).pathname);
const REPO_ROOT = resolve(SCRIPT_DIR, '..');
const GATE = join(SCRIPT_DIR, 'check-resize-cutover-retention.sh');
const ENGINE = join(SCRIPT_DIR, 'resize-cutover-retention-manifest.mjs');
const FIXTURE_DIR = join(SCRIPT_DIR, 'fixtures', 'resize-cutover-retention');

// ---------------------------------------------------------------------------
// Fixture loading.
// ---------------------------------------------------------------------------

function loadFixtures() {
    const entries = readdirSync(FIXTURE_DIR, { withFileTypes: true })
        .filter((entry) => entry.isDirectory())
        .map((entry) => entry.name)
        .sort();
    return entries.map((name) => {
        const path = join(FIXTURE_DIR, name, 'fixture.json');
        assert.ok(existsSync(path), `fixture ${name} has no fixture.json`);
        const fixture = JSON.parse(readFileSync(path, 'utf8'));
        assert.ok(
            fixture.expect === 'reject' || fixture.expect === 'accept',
            `fixture ${name}: expect must be "reject" or "accept", got ${fixture.expect}`,
        );
        assert.ok(
            typeof fixture.why === 'string' && fixture.why.length > 40,
            `fixture ${name}: "why" must explain the retirement it stands in for`,
        );
        assert.ok(Array.isArray(fixture.operations), `fixture ${name}: operations must be an array`);
        return { name, ...fixture };
    });
}

// ---------------------------------------------------------------------------
// Scratch-root materialisation.
// ---------------------------------------------------------------------------

function gateInputs() {
    const listed = spawnSync(process.execPath, [ENGINE, '--list-inputs'], { encoding: 'utf8' });
    assert.equal(listed.status, 0, `--list-inputs failed: ${listed.stderr}`);
    const paths = listed.stdout.trim().split('\n').filter(Boolean);
    assert.ok(paths.length >= 15, `the gate reads only ${paths.length} paths; that is too few to be guarding nine assets`);
    return paths;
}

function materialise(paths) {
    const root = mkdtempSync(join(tmpdir(), 'resize-retention-'));
    for (const relative of paths) {
        const source = join(REPO_ROOT, relative);
        assert.ok(
            existsSync(source),
            `the gate names ${relative} but the repository does not have it. Either the asset moved (update the manifest) or the gate is pointing at nothing.`,
        );
        const destination = join(root, relative);
        mkdirSync(dirname(destination), { recursive: true });
        copyFileSync(source, destination);
    }
    return root;
}

function apply(root, fixture) {
    for (const operation of fixture.operations) {
        const target = join(root, operation.path);
        switch (operation.op) {
            case 'delete': {
                assert.ok(
                    existsSync(target),
                    `fixture ${fixture.name}: cannot delete ${operation.path}, it is not in the materialised root. The gate does not read it, so deleting it would prove nothing.`,
                );
                rmSync(target);
                break;
            }
            case 'replace': {
                const before = readFileSync(target, 'utf8');
                const occurrences = before.split(operation.find).length - 1;
                assert.equal(
                    occurrences,
                    1,
                    `fixture ${fixture.name}: its anchor in ${operation.path} matched ${occurrences} times, not exactly once. An anchor that drifted out of the source mutates nothing and the fixture then proves nothing.`,
                );
                writeFileSync(target, before.replace(operation.find, operation.with));
                break;
            }
            case 'append': {
                writeFileSync(target, `${readFileSync(target, 'utf8')}${operation.text}`);
                break;
            }
            default:
                assert.fail(`fixture ${fixture.name}: unknown operation ${operation.op}`);
        }
    }
}

/// Run the SHELL gate. Never the engine module: AC11 makes the shell script's
/// exit code the contract.
function runGate(root) {
    const result = spawnSync('sh', [GATE], {
        encoding: 'utf8',
        env: { ...process.env, RESIZE_RETENTION_ROOT: root },
    });
    return {
        status: result.status,
        stdout: result.stdout ?? '',
        stderr: result.stderr ?? '',
    };
}

const FIXTURES = loadFixtures();

// ---------------------------------------------------------------------------
// The gate's subject must be real.
// ---------------------------------------------------------------------------

test('the gate passes against the real repository', () => {
    const result = runGate(REPO_ROOT);
    assert.equal(
        result.status,
        0,
        `the gate rejects HEAD. Nothing in this task retires anything, so this is the gate being wrong:\n${result.stdout}\n${result.stderr}`,
    );
    assert.match(result.stdout, /check-resize-cutover-retention: OK/);
});

test('the gate guards at least every asset the epic named', () => {
    assert.ok(
        ASSETS.length >= MINIMUM_ASSETS,
        `the asset table holds ${ASSETS.length}, below the ${MINIMUM_ASSETS} floor`,
    );
    const ids = new Set(ASSETS.map((asset) => asset.id));
    for (const required of [
        'cgroup-launcher-crate',
        'process-broker',
        'runtimeclass-and-node-assets',
        'credential-boundary-tests',
        'broker-mounts',
        'cgroup-kill',
        'wait-empty',
    ]) {
        assert.ok(ids.has(required), `the epic's retained boundary names ${required} and the gate does not guard it`);
    }
});

test('every asset carries its own deletion or disablement fixture', () => {
    const covered = new Set(
        FIXTURES.filter((fixture) => fixture.expect === 'reject').map((fixture) => fixture.asset),
    );
    for (const asset of ASSETS) {
        assert.ok(
            covered.has(asset.id),
            `asset ${asset.id} has no rejecting fixture. A gate that enumerates protected paths and is never PROVEN to reject their removal is a gate that can always exit 0.`,
        );
    }
});

test('the fixture set carries a required-passing refactor case', () => {
    const accepting = FIXTURES.filter(
        (fixture) => fixture.expect === 'accept' && fixture.operations.length > 0,
    );
    assert.ok(
        accepting.length >= 1,
        'AC8 requires a passing refactor case. Without one the gate could be a byte-identity check and nothing here would notice.',
    );
});

// ---------------------------------------------------------------------------
// Every fixture, through the shell entry point.
// ---------------------------------------------------------------------------

for (const fixture of FIXTURES) {
    test(`fixture ${fixture.name} is ${fixture.expect}ed`, () => {
        const paths = gateInputs();
        const root = materialise(paths);
        try {
            apply(root, fixture);
            const result = runGate(root);
            if (fixture.expect === 'accept') {
                assert.equal(
                    result.status,
                    0,
                    `${fixture.name} must PASS — ${fixture.why}\n${result.stdout}\n${result.stderr}`,
                );
                return;
            }
            assert.equal(
                result.status,
                1,
                `${fixture.name} must be REJECTED with exit 1 — ${fixture.why}\n${result.stdout}\n${result.stderr}`,
            );
            assert.ok(
                result.stderr.includes(fixture.asset),
                `${fixture.name} was rejected, but not for the asset it targets (${fixture.asset}). A rejection for an unrelated reason proves nothing about this asset:\n${result.stderr}`,
            );
        } finally {
            rmSync(root, { recursive: true, force: true });
        }
    });
}

// ---------------------------------------------------------------------------
// The two mechanisms the fixtures rest on, unit-tested directly.
// ---------------------------------------------------------------------------

test('comment stripping removes whole-line comments and leaves code alone', () => {
    const rust = [
        '    // self.fs.write_leaf(leaf.fd, "cgroup.kill", "1")',
        '    self.fs.write_leaf(leaf.fd, "cgroup.kill", "1")',
        '    /*',
        '    self.fs.write_leaf(leaf.fd, "cgroup.other", "1")',
        '    */',
    ].join('\n');
    const stripped = stripComments(rust, 'x.rs');
    assert.equal(
        stripped.split('cgroup.kill').length - 1,
        1,
        'exactly the uncommented write must survive',
    );
    assert.ok(!stripped.includes('cgroup.other'), 'a block-commented statement must not survive');
    const yaml = ['# name: djinn-cgroup-writable', 'name: djinn-cgroup-writable'].join('\n');
    assert.equal(stripComments(yaml, 'x.yaml').split('name:').length - 1, 1);
});

test('the gate refuses a root it cannot read', () => {
    const result = spawnSync('sh', [GATE], {
        encoding: 'utf8',
        env: { ...process.env, RESIZE_RETENTION_ROOT: '/nonexistent/resize-retention-root' },
    });
    assert.equal(
        result.status,
        2,
        'a gate that cannot find its subject must say so with a distinct code rather than pass',
    );
});

test('an emptied protected file reads as a deletion', () => {
    const root = materialise(gateInputs());
    try {
        writeFileSync(join(root, 'server/crates/djinn-cgroup-launcher/src/lib.rs'), '');
        const result = runGate(root);
        assert.equal(result.status, 1);
        assert.match(result.stderr, /EMPTIED|DELETED/);
    } finally {
        rmSync(root, { recursive: true, force: true });
    }
});
