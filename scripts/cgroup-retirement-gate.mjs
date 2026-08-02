// Repository-only PREP/RETIRE gate. It deliberately has no Kubernetes, cloud, or
// credential inputs: a green result only permits *candidate review*, never rollout.
import { existsSync, readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const args = process.argv.slice(2);
const fail = (message, code = 1) => { process.stderr.write(`REJECT cgroup-retirement gate: ${message}\n`); process.exit(code); };
const usage = () => fail('usage: --prep BASE HEAD | --deploy|--release|--withdraw-node --candidate NAME --inputs FILE', 2);
const repo = resolve(process.env.CGROUP_RETIREMENT_GATE_ROOT || process.cwd());
const scriptDir = dirname(new URL(import.meta.url).pathname);

// PREP is intentionally a narrow additive safety/proof phase.  Protected paths
// are listed first so a future broad allowlist cannot silently admit retirement.
const environmentPolicyPaths = new Set([
    'server/crates/djinn-agent/src/environment.rs',
    'server/crates/djinn-agent/src/extension/handlers/workspace.rs',
    'server/crates/djinn-agent/src/extension/tests/brokered_shell_program_tests.rs',
    // This broker adapter carries the launcher-free child environment into the
    // already-armed launcher. PREP may harden that policy, never remove it.
    'server/crates/djinn-agent/src/process_broker.rs',
]);

const protectedPath = (path, status) =>
    // No PREP change belongs in rendering; retain all k8s assets rather than
    // guessing which renderer happens to name the launcher today.
    /^server\/crates\/djinn-k8s\//.test(path) ||
    /^server\/crates\/djinn-cgroup-launcher\/(?!src\/(?:env|git_trust)\.rs$)/.test(path) ||
    /^server\/(?:charts|helm|k8s|node|kind)\//.test(path) ||
    // The environment-policy change legitimately touches the agent's broker
    // adapter. It is still a preserved asset: deletion is retirement work and
    // is rejected before the narrow PREP allowlist is considered.
    (environmentPolicyPaths.has(path) && /^D/.test(status)) ||
    (!environmentPolicyPaths.has(path) &&
        /(?:^|\/)(?:runtimeclass|runtime-class|broker|cgroup\.kill|credential|launcher|render|node).*/i.test(path)) ||
    /^scripts\/check-resize-cutover-retention\.sh$/.test(path) ||
    /^scripts\/resize-cutover-retention-manifest\.mjs$/.test(path);

const prepAllowed = (path) =>
    path === 'server/crates/djinn-agent/src/process.rs' ||
    environmentPolicyPaths.has(path) ||
    path === 'server/crates/djinn-cgroup-launcher/src/env.rs' ||
    path === 'server/crates/djinn-cgroup-launcher/src/git_trust.rs' ||
    /^server\/crates\/djinn-sandbox\//.test(path) ||
    /^scripts\/(?:check-cgroup-retirement-gate\.sh|cgroup-retirement-gate\.mjs|test-cgroup-retirement-gate\.sh|verify-cgroup-retirement-evidence\.(?:sh|mjs)|test-verify-cgroup-retirement-evidence\.sh|fixtures\/cgroup-retirement\/)/.test(path);

const git = (gitArgs) => spawnSync('git', gitArgs, { cwd: repo, encoding: 'utf8' });
const changedPaths = (base, head) => {
    const result = git(['diff', '--name-status', '-M', `${base}..${head}`]);
    if (result.status !== 0) fail(`cannot inspect range ${base}..${head}: ${result.stderr.trim()}`, 2);
    const paths = [];
    for (const line of result.stdout.trim().split('\n').filter(Boolean)) {
        const fields = line.split('\t');
        // Rename/copy has old and new names. Both are relevant to retention.
        for (const path of fields.slice(1)) paths.push({ path, status: fields[0] });
    }
    return paths;
};

// Sandbox proof source is PREP work, but its credential-denial assertions are
// not optional. Keep a small textual contract instead of duplicating Landlock
// arithmetic or executing a cluster test. This makes a removal, an #[ignore],
// or a retargeted confidential-root proof fail closed even though linux.rs is
// otherwise an allowed PREP path.
const sandboxProofPaths = new Set([
    'server/crates/djinn-sandbox/src/linux.rs',
    'server/crates/djinn-sandbox/src/confidential.rs',
]);
const requiredSandboxProof = {
    'server/crates/djinn-sandbox/src/linux.rs': [
        'fn shell_sandbox_denies_reading_confidential_mount_contents()',
        'SPEC_CANARY', 'CREDENTIAL_CANARY', 'TOKEN_CANARY',
        'apply_with_confidential_roots', '!direct.status.success()',
        'cargo build', '!captured.contains(canary)',
    ],
    'server/crates/djinn-sandbox/src/confidential.rs': [
        'pub const CONFIDENTIAL_ROOTS', '"/var/run/djinn"', '"/var/run/secrets"',
        'fn confidential_roots_cover_the_pod_secret_mounts()',
    ],
};
const mandatorySandboxTest = {
    'server/crates/djinn-sandbox/src/linux.rs':
        'fn shell_sandbox_denies_reading_confidential_mount_contents()',
    'server/crates/djinn-sandbox/src/confidential.rs':
        'fn confidential_roots_cover_the_pod_secret_mounts()',
};
const showAt = (revision, path) => {
    const result = git(['show', `${revision}:${path}`]);
    return result.status === 0 ? result.stdout : null;
};
// A small, local outer-attribute scanner. It starts immediately before the
// mandatory function and only walks its attached attribute/separator suffix.
const skipQuoted = (source, start, quote) => {
    for (let at = start + 1; at < source.length; at += 1) {
        if (source[at] === '\\') { at += 1; continue; }
        if (source[at] === quote) return at + 1;
    }
    return -1;
};
const skipRawString = (source, start) => {
    const match = source.slice(start).match(/^(?:br|rb|r)(#*)"/);
    if (!match) return -1;
    const closing = `"${match[1]}`;
    const end = source.indexOf(closing, start + match[0].length);
    return end < 0 ? -1 : end + closing.length;
};
const skipBlockComment = (source, start) => {
    let depth = 1;
    for (let at = start + 2; at < source.length - 1; at += 1) {
        if (source[at] === '/' && source[at + 1] === '*') { depth += 1; at += 1; }
        else if (source[at] === '*' && source[at + 1] === '/') {
            depth -= 1;
            if (depth === 0) return at + 2;
            at += 1;
        }
    }
    return -1;
};
const parseOuterAttribute = (source, start, allowInner = false) => {
    if (source[start] !== '#') return null;
    let at = start + 1;
    while (/\s/.test(source[at] || '')) at += 1;
    if (allowInner && source[at] === '!') {
        at += 1;
        while (/\s/.test(source[at] || '')) at += 1;
    }
    if (source[at] !== '[') return null;
    const open = at;
    const stack = ['['];
    for (at += 1; at < source.length; at += 1) {
        const rawEnd = source[at] === 'r' || source[at] === 'b' ? skipRawString(source, at) : -1;
        if (rawEnd >= 0) { at = rawEnd - 1; continue; }
        if (source[at] === '"' || source[at] === '\'') {
            const quotedEnd = skipQuoted(source, at, source[at]);
            if (quotedEnd < 0) return null;
            at = quotedEnd - 1;
            continue;
        }
        if (source[at] === '/' && source[at + 1] === '/') {
            const lineEnd = source.indexOf('\n', at + 2);
            at = lineEnd < 0 ? source.length : lineEnd;
            continue;
        }
        if (source[at] === '/' && source[at + 1] === '*') {
            const commentEnd = skipBlockComment(source, at);
            if (commentEnd < 0) return null;
            at = commentEnd - 1;
            continue;
        }
        if ('[({'.includes(source[at])) stack.push(source[at]);
        else if (']})'.includes(source[at])) {
            const expected = { ']': '[', '}': '{', ')': '(' }[source[at]];
            if (stack.pop() !== expected) return null;
            if (stack.length === 0) {
                const interior = source.slice(open + 1, at);
                const name = (interior.match(/^\s*([A-Za-z_][A-Za-z0-9_]*)/) || [])[1];
                return { start, end: at + 1, name, interior };
            }
        }
    }
    return null;
};
const skipSeparatorsBackward = (source, position) => {
    let at = position;
    for (;;) {
        while (at > 0 && /\s/.test(source[at - 1])) at -= 1;
        if (source.slice(0, at).endsWith('*/')) {
            let depth = 1;
            let cursor = at - 2;
            for (; cursor > 0; cursor -= 1) {
                if (source[cursor - 1] === '*' && source[cursor] === '/') { depth += 1; cursor -= 1; }
                else if (source[cursor - 1] === '/' && source[cursor] === '*') {
                    depth -= 1;
                    if (depth === 0) { at = cursor - 1; break; }
                    cursor -= 1;
                }
            }
            if (depth !== 0) return at;
            continue;
        }
        const lineStart = source.lastIndexOf('\n', at - 1) + 1;
        if (/^\s*\/\//.test(source.slice(lineStart, at))) { at = lineStart; continue; }
        return at;
    }
};
const skipNonCode = (source, at) => {
    const rawEnd = source[at] === 'r' || source[at] === 'b' ? skipRawString(source, at) : -1;
    if (rawEnd >= 0) return rawEnd;
    if (source[at] === '"' || source[at] === '\'') return skipQuoted(source, at, source[at]);
    if (source[at] === '/' && source[at + 1] === '/') {
        const lineEnd = source.indexOf('\n', at + 2);
        return lineEnd < 0 ? source.length : lineEnd + 1;
    }
    if (source[at] === '/' && source[at + 1] === '*') return skipBlockComment(source, at);
    return at;
};
const findMandatoryFunction = (source, signature) => {
    const name = (signature.match(/^fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(/) || [])[1];
    if (!name) return -1;
    for (let at = 0; at < source.length; at += 1) {
        const nonCodeEnd = skipNonCode(source, at);
        if (nonCodeEnd < 0) return -1;
        if (nonCodeEnd !== at) { at = nonCodeEnd - 1; continue; }
        if (source.startsWith('fn', at) &&
            !/[A-Za-z0-9_]/.test(source[at - 1] || '') &&
            !/[A-Za-z0-9_]/.test(source[at + 2] || '')) {
            let nameAt = at + 2;
            while (/\s/.test(source[nameAt] || '')) nameAt += 1;
            if (source.slice(nameAt, nameAt + name.length) === name &&
                !/[A-Za-z0-9_]/.test(source[nameAt + name.length] || '') &&
                /^\s*(?:<[^<>{}()]*>)?\s*\(/.test(source.slice(nameAt + name.length))) return at;
        }
    }
    return -1;
};
// Do not search backward for a `#`: that would inspect arbitrary prior items.
// Track lexical delimiter depth instead. A hash can start an attached attribute
// only at the proof item's outer depth; hashes in macro arguments or other
// nested token trees are ordinary tokens. An incomplete `#[` is scanned
// separately from normal source delimiters, so its mismatched close cannot be
// mistaken for an item boundary.
const hasMalformedAttributePrefix = (source, end) => {
    const delimiters = [];
    const malformedAtDepth = [];
    let malformedAttribute = false;
    for (let at = 0; at < end; at += 1) {
        const nonCodeEnd = skipNonCode(source, at);
        if (nonCodeEnd < 0) return true;
        if (nonCodeEnd !== at) { at = nonCodeEnd - 1; continue; }
        if (malformedAttribute) {
            // A later attribute is a reliable lexical restart point. Everything
            // between it and the malformed opener belongs to the malformed
            // token tree, including a mismatched `}` such as `#[cfg(}`.
            if (source[at] !== '#') continue;
            malformedAttribute = false;
        }
        if (source[at] === '#') {
            const parsed = parseOuterAttribute(source, at, true);
            if (parsed && parsed.end <= end) { at = parsed.end - 1; continue; }
            let cursor = at + 1;
            while (/\s/.test(source[cursor] || '')) cursor += 1;
            if (source[cursor] === '!') {
                cursor += 1;
                while (/\s/.test(source[cursor] || '')) cursor += 1;
            }
            if (source[cursor] === '[') {
                malformedAtDepth.push(delimiters.length);
                malformedAttribute = true;
            }
        } else if ('[({'.includes(source[at])) {
            delimiters.push(source[at]);
        } else if (']})'.includes(source[at])) {
            const expected = { ']': '[', '}': '{', ')': '(' }[source[at]];
            if (delimiters[delimiters.length - 1] === expected) delimiters.pop();
        }
    }
    return malformedAtDepth.includes(delimiters.length);
};
const attachedOuterAttributes = (source, proofAt) => {
    const attributes = [];
    let end = skipSeparatorsBackward(source, proofAt);
    while (end > 0 && source[end - 1] === ']') {
        let candidate = source.lastIndexOf('#', end - 1);
        let attribute = null;
        while (candidate >= 0) {
            const parsed = parseOuterAttribute(source, candidate);
            if (parsed && parsed.end === end) { attribute = parsed; break; }
            candidate = source.lastIndexOf('#', candidate - 1);
        }
        if (!attribute) return null;
        attributes.unshift(attribute);
        end = skipSeparatorsBackward(source, attribute.start);
    }
    if (hasMalformedAttributePrefix(source, end)) return null;
    return attributes;
};
const sandboxProofRemainsArmed = (head, changes) => {
    for (const path of sandboxProofPaths) {
        if (!changes.some((change) => change.path === path)) continue;
        const source = showAt(head, path);
        if (source === null) fail(`PREP range deletes protected credential-boundary proof: ${path}`);
        for (const marker of requiredSandboxProof[path]) {
            if (!source.includes(marker)) {
                fail(`PREP range disables or retargets protected credential-boundary proof in ${path} (missing ${marker})`);
            }
        }
        const proofAt = findMandatoryFunction(source, mandatorySandboxTest[path]);
        if (proofAt < 0) {
            fail(`PREP range is missing mandatory credential-boundary proof function in ${path}`);
        } else {
            // `#[cfg(any())] #[test]` preserves every proof marker but compiles
            // the test out. Apply the same enabled-test contract to both
            // mandatory credential-boundary proofs: require an ordinary test
            // and reject every disabling modifier in its complete attached
            // outer-attribute block. This scanner stops at the preceding item.
            const attributes = attachedOuterAttributes(source, proofAt);
            if (attributes === null) {
                fail(`PREP range has malformed attached attribute on protected credential-boundary proof in ${path}`);
            }
            if (!attributes.some((attribute) => attribute.name === 'test' && attribute.interior.trim() === 'test')) {
                fail(`PREP range disables protected credential-boundary proof in ${path} (missing enabled #[test])`);
            }
            if (attributes.some((attribute) => ['cfg', 'cfg_attr', 'ignore'].includes(attribute.name))) {
                fail(`PREP range disables protected credential-boundary proof in ${path} with cfg/cfg_attr/ignore`);
            }
        }
    }
};

const runPrep = (base, head) => {
    if (!base || !head) usage();
    const changes = changedPaths(base, head);
    if (changes.length === 0) fail('PREP range is empty');
    for (const { path, status } of changes) {
        if (protectedPath(path, status)) fail(`PREP range touches protected launcher/render/RuntimeClass/node/broker/cgroup-kill/credential boundary: ${path}`);
        if (!prepAllowed(path)) fail(`PREP range contains out-of-phase change: ${path}`);
    }
    sandboxProofRemainsArmed(head, changes);
    process.stdout.write(`check-cgroup-retirement-gate: PREP OK (${changes.length} changed paths; launcher remains armed)\n`);
};

const mandatory = ['environment_policy', 'descendant_reaping', 'sandbox_proof', 'evidence_schema', 'range_guard', 'cgroup_evidence'];
const readInputs = (path) => {
    if (!path || !existsSync(path)) fail('mandatory PREP proof inputs are missing');
    let inputs;
    try { inputs = JSON.parse(readFileSync(path, 'utf8')); } catch (error) { fail(`mandatory PREP proof inputs are invalid JSON (${error.message})`); }
    if (inputs === null || typeof inputs !== 'object' || Array.isArray(inputs)) fail('mandatory PREP proof inputs must be an object');
    for (const name of mandatory) {
        if (inputs[name] !== 'green') fail(`mandatory PREP proof ${name} is ${inputs[name] === undefined ? 'missing' : inputs[name]}`);
    }
    for (const [name, status] of Object.entries(inputs)) {
        if (status !== 'green') fail(`mandatory PREP proof ${name} is ${String(status)}`);
    }
};

const runInterlock = (action, candidate, inputs) => {
    if (!candidate || !/^[A-Za-z0-9_.-]+$/.test(candidate)) usage();
    readInputs(inputs);
    const verifier = resolve(scriptDir, 'verify-cgroup-retirement-evidence.sh');
    const result = spawnSync(verifier, ['--candidate', candidate], {
        cwd: repo, encoding: 'utf8', env: process.env,
    });
    if (result.status !== 0) fail(`evidence verifier refused ${candidate}: ${(result.stderr || result.stdout).trim()}`);
    process.stdout.write(`check-cgroup-retirement-gate: ${action} is eligible for candidate review only; live rollout remains refused\n`);
};

if (args[0] === '--prep' && args.length === 3) runPrep(args[1], args[2]);
else if (['--deploy', '--release', '--withdraw-node'].includes(args[0])) {
    const candidateAt = args.indexOf('--candidate');
    const inputsAt = args.indexOf('--inputs');
    if (args.length !== 5 || candidateAt < 0 || inputsAt < 0) usage();
    runInterlock(args[0].slice(2), args[candidateAt + 1], resolve(repo, args[inputsAt + 1]));
} else usage();
