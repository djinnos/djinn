// Hermetic aggregate rollback engine. It creates a disposable Git repository;
// it never reads or changes the caller's branch or worktree.
import { chmodSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, normalize, relative, resolve, sep } from 'node:path';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';

const root = resolve(dirname(new URL(import.meta.url).pathname), '..');
const defaultPlan = resolve(root, 'scripts/fixtures/cgroup-retirement/rollback/plan.json');
const fail = (message) => { throw new Error(message); };
const sha256 = (value) => createHash('sha256').update(value).digest('hex');
const cpuMax = (value) => {
    if (typeof value !== 'string' || !/^(?:max|[1-9]\d*) [1-9]\d*$/.test(value)) return null;
    const [quota, period] = value.split(' ');
    const periodValue = Number(period);
    // cpu.max accepts a positive quota (or `max`) and a kernel-valid,
    // canonical period from one millisecond through one second.
    if (!Number.isSafeInteger(periodValue) || periodValue < 1000 || periodValue > 1_000_000) return null;
    // cgroup v2 rejects finite quotas below one millisecond too. Treating a
    // syntactically positive value such as `1 100000` as evidence would let a
    // launcher leaf attest to a setting it cannot install.
    if (quota !== 'max' && (!Number.isSafeInteger(Number(quota)) || Number(quota) < 1000)) return null;
    return value;
};
const safeAssetPath = (path) => {
    if (typeof path !== 'string' || path.length === 0 || path.includes('\0') || path.includes('\\') || isAbsolute(path) || normalize(path) !== path || path === '.' || path.split(sep).some((part) => part === '' || part === '.' || part === '..')) fail(`fixture asset path is unsafe: ${path}`);
    return path;
};
const assetTarget = (repo, path) => {
    safeAssetPath(path);
    const target = resolve(repo, path);
    const fromRepo = relative(repo, target);
    if (!fromRepo || fromRepo === '..' || fromRepo.startsWith(`..${sep}`) || isAbsolute(fromRepo)) fail(`fixture asset path escapes disposable repository: ${path}`);
    return target;
};
const git = (repo, args, allowFailure = false) => {
    const result = spawnSync('git', args, { cwd: repo, encoding: 'utf8' });
    if (!allowFailure && result.status !== 0) fail(`git ${args.join(' ')}: ${(result.stderr || result.stdout).trim()}`);
    return result;
};
const tree = (repo, revision = 'HEAD') => git(repo, ['ls-tree', '-r', revision]).stdout;
const usage = () => fail('usage: rehearse-cgroup-retirement-rollback.mjs [--fixture PLAN] [--case positive|stop-before-oldest|out-of-order|split-pair|missing-node|digest-mismatch|missing-leaf|malformed-leaf|stale-leaf|corrupt-recovery]');

const args = process.argv.slice(2);
let fixturePath = defaultPlan;
let scenario = 'positive';
for (let at = 0; at < args.length; at += 1) {
    if (args[at] === '--fixture' && args[at + 1]) fixturePath = resolve(args[++at]);
    else if (args[at] === '--case' && args[at + 1]) scenario = args[++at];
    else usage();
}
const cases = new Set(['positive', 'stop-before-oldest', 'out-of-order', 'split-pair', 'missing-node', 'digest-mismatch', 'missing-leaf', 'malformed-leaf', 'stale-leaf', 'corrupt-recovery']);
if (!cases.has(scenario)) usage();

let plan;
try { plan = JSON.parse(readFileSync(fixturePath, 'utf8')); } catch (error) { fail(`fixture is unreadable: ${error.message}`); }
if (plan?.schema !== 'djinn-cgroup-retirement-rollback-rehearsal/v1' || !Array.isArray(plan.ordered_phases) || !Array.isArray(plan.assets)) fail('fixture schema is invalid');
const manifest = JSON.parse(readFileSync(resolve(root, 'scripts/cgroup-retirement-assets.json'), 'utf8'));
const declaredPhase = new Map(manifest.candidates.flatMap(({ phase, paths }) => paths.map((path) => [path, phase])));
const assetByPath = new Map();
for (const asset of plan.assets) {
    if (!asset || typeof asset.path !== 'string' || typeof asset.content !== 'string' || assetByPath.has(asset.path)) fail('fixture assets must be unique path/content records');
    safeAssetPath(asset.path);
    assetByPath.set(asset.path, asset);
    if (asset.phase !== 'preserved' && declaredPhase.get(asset.path) !== asset.phase) fail(`fixture asset does not use manifest phase: ${asset.path}`);
}
if (plan.ordered_phases.join(',') !== manifest.candidates.map(({ phase }) => phase).join(',')) fail('fixture phase order diverges from retirement manifest');
for (const pair of new Set(plan.assets.map(({ forced_pair: pairName }) => pairName).filter(Boolean))) {
    const members = plan.assets.filter((asset) => asset.forced_pair === pair);
    if (members.length !== 2 || new Set(members.map(({ role }) => role)).size !== 2 || !members.some(({ role }) => role === 'source') || !members.some(({ role }) => role === 'test') || new Set(members.map(({ phase }) => phase)).size !== 1) fail(`forced pair ${pair} must be exactly one source and one test in one phase`);
}
const launcher = assetByPath.get(plan.launcher_leaf?.launcher_path);
if (!launcher || launcher.role !== 'launcher' || sha256(launcher.content) !== plan.launcher_leaf.source_sha256 || !cpuMax(plan.launcher_leaf.cpu_max) || plan.launcher_leaf.recorded_by !== 'deterministic-fixture-v1') fail('fixture launcher leaf is malformed or stale');

const repo = mkdtempSync(resolve(tmpdir(), 'cgroup-retirement-rollback-'));
const snapshots = {};
const capture = (name) => { snapshots[name] = { tree: tree(repo), render: readFileSync(assetTarget(repo, launcher.path), 'utf8'), node: plan.assets.filter(({ role }) => role === 'node').map(({ path }) => ({ path, content: readFileSync(assetTarget(repo, path), 'utf8') })) }; };
const verifyLeaf = (leaf) => {
    if (!leaf || typeof leaf !== 'object' || leaf.produced_by !== 'restored-launcher-exec-v1' || leaf.launcher_path !== launcher.path || !cpuMax(leaf.cpu_max) || leaf.cpu_max !== plan.launcher_leaf.cpu_max || leaf.source_sha256 !== sha256(readFileSync(assetTarget(repo, launcher.path), 'utf8'))) fail('launcher leaf proof is missing, malformed, or stale');
};
const produceLeafEvidence = () => {
    const result = spawnSync(assetTarget(repo, launcher.path), [], { cwd: repo, encoding: 'utf8' });
    const record = result.status === 0 && result.stderr === '' ? cpuMax(result.stdout.trimEnd()) : null;
    if (!record || result.stdout !== `${record}\n`) fail('restored launcher did not produce a valid cpu.max record');
    return { produced_by: 'restored-launcher-exec-v1', launcher_path: launcher.path, cpu_max: record, source_sha256: sha256(readFileSync(assetTarget(repo, launcher.path), 'utf8')) };
};
const verifyRestoration = (baseTree, leaf) => {
    for (const asset of plan.assets.filter(({ role }) => role === 'node')) if (readFileSync(assetTarget(repo, asset.path), 'utf8') !== asset.content) fail(`modeled node asset was not restored: ${asset.path}`);
    // ls-tree proves object/mode/name identity; diff --quiet ensures the
    // checkout itself has not drifted from that tracked tree.
    if (git(repo, ['diff', '--quiet'], true).status !== 0 || tree(repo) !== baseTree) fail('restored tracked tree digest does not match RETIRE_BASE');
    verifyLeaf(leaf);
};
try {
    git(repo, ['init', '-q']);
    git(repo, ['config', 'user.email', 'rollback-fixture@example.invalid']);
    git(repo, ['config', 'user.name', 'rollback-fixture']);
    for (const asset of plan.assets) { const target = assetTarget(repo, asset.path); mkdirSync(dirname(target), { recursive: true }); writeFileSync(target, asset.content); }
    chmodSync(assetTarget(repo, launcher.path), 0o755);
    git(repo, ['add', '.']); git(repo, ['commit', '-qm', 'RETIRE_BASE fixture']);
    const retireBase = git(repo, ['rev-parse', 'HEAD']).stdout.trim();
    const commits = [];
    for (const phase of plan.ordered_phases) {
        let retired = plan.assets.filter((asset) => asset.phase === phase);
        if (scenario === 'split-pair' && phase === 'RETIRE_BASE') retired = retired.filter(({ role }) => role !== 'test');
        for (const asset of retired) git(repo, ['rm', '-q', asset.path]);
        git(repo, ['commit', '-qm', `${phase} fixture candidate`]);
        commits.push({ phase, sha: git(repo, ['rev-parse', 'HEAD']).stdout.trim(), assets: retired });
    }
    const retireHead = git(repo, ['rev-parse', 'HEAD']).stdout.trim();
    if (scenario === 'split-pair') fail('forced source/test pair was split across candidate commits');
    if (commits.length !== plan.ordered_phases.length || commits.some(({ phase }, index) => phase !== plan.ordered_phases[index])) fail('candidate commits do not cover ordered manifest phases');
    for (const pair of new Set(plan.assets.map(({ forced_pair: pairName }) => pairName).filter(Boolean))) {
        const memberPaths = plan.assets.filter(({ forced_pair: pairName }) => pairName === pair).map(({ path }) => path).sort();
        const commit = commits.find(({ assets }) => assets.some(({ forced_pair: pairName }) => pairName === pair));
        if (!commit || commit.assets.filter(({ forced_pair: pairName }) => pairName === pair).map(({ path }) => path).sort().join('\0') !== memberPaths.join('\0')) fail(`forced pair ${pair} was split across candidate commits`);
    }
    if (scenario === 'out-of-order') {
        // Git can apply independent inverse patches out of order, so enforce
        // range order explicitly instead of mistaking a conflict for proof.
        if (commits[0].sha !== retireHead) fail('candidate commits must be reverted newest-to-oldest');
        fail('out-of-order fixture did not select an older candidate commit');
    }
    const toRevert = scenario === 'stop-before-oldest' ? commits.slice(1).reverse() : commits.slice().reverse();
    for (const commit of toRevert) git(repo, ['revert', '--no-edit', commit.sha]);
    if (scenario === 'stop-before-oldest') fail('rollback stopped before the oldest candidate commit');
    const baseTree = tree(repo, retireBase);
    let leaf = produceLeafEvidence();
    if (scenario === 'missing-node') writeFileSync(assetTarget(repo, plan.assets.find(({ role }) => role === 'node').path), 'missing node restore\n');
    if (scenario === 'digest-mismatch') writeFileSync(assetTarget(repo, launcher.path), 'mismatched restored launcher\n');
    if (scenario === 'missing-leaf') leaf = null;
    if (scenario === 'malformed-leaf') leaf = { launcher_path: launcher.path, cpu_max: 'not-a-cgroup-record' };
    if (scenario === 'stale-leaf') leaf.source_sha256 = '0'.repeat(64);
    if (scenario !== 'corrupt-recovery') { verifyRestoration(baseTree, leaf); process.stdout.write(`RETIRE rollback rehearsal: OK ${retireBase}..${retireHead}\n`); }
    else {
        writeFileSync(assetTarget(repo, plan.assets.find(({ role }) => role === 'node').path), 'corrupted restoration\n');
        capture('recovery-detected');
        let refusal = false;
        try { verifyRestoration(baseTree, leaf); } catch { refusal = true; }
        if (!refusal) fail('corrupted restoration did not enter RECOVERY');
        const state = { status: 'RECOVERY', dispatch: 'paused', terminal_labels: { KEEP: 'refused', RETIRE: 'refused' }, snapshots };
        const detected = state.snapshots['recovery-detected'];
        if (state.dispatch !== 'paused' || Object.values(state.terminal_labels).some((value) => value !== 'refused') || !detected || !detected.tree || !detected.render || !Array.isArray(detected.node)) fail('RECOVERY did not pause dispatch, refuse labels, and capture tree/render/node snapshots');
        const node = plan.assets.find(({ role }) => role === 'node');
        writeFileSync(assetTarget(repo, node.path), node.content);
        capture('recovery-repaired');
        verifyRestoration(baseTree, leaf);
        process.stdout.write(`RECOVERY repair remains nonterminal; dispatch paused; aggregate restoration green ${retireBase}..${retireHead}\n`);
    }
} finally { rmSync(repo, { recursive: true, force: true }); }
