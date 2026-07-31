#!/usr/bin/env node
// The no-retirement engine for proposal 3i92's epic `xowm` (task 1j64).
//
// WHY THIS EXISTS
// ---------------
// The in-place task-run resize cutover moves the CPU ceiling of one container.
// It is NOT a licence to retire the containment and credential boundary that
// the launcher, the broker and the writable-cgroup RuntimeClass make up. The
// epic's retained boundary is explicit: no deletion and no DISABLING of the
// launcher crate, the broker, the RuntimeClass and node assets, the
// credential-boundary tests, the broker mounts, the `cgroup.kill` write, or
// `wait_empty` belongs in this cutover.
//
// WHAT THIS IS NOT
// ----------------
// It is NOT a byte-identity check, and it must never become one. Source files
// in this epic are EXPECTED to change: they are being refactored around the
// resize path right now. A gate phrased as "file X must be byte-identical"
// combined with "pattern P must have zero hits", where P occurs in X, is
// unsatisfiable — that exact pair cost a full agent-run on 2026-07-30. So the
// contract here is:
//
//   * PRESENCE   — the asset's files are still tracked and non-empty.
//   * CONTENT    — the behaviour-bearing statement is still there, judged on
//                  source with whole-line comments stripped, so commenting a
//                  write out reads as deleting it.
//   * REACHABILITY — something in PRODUCTION still calls it. A `wait_empty`
//                  that exists and is called from nowhere is retired in every
//                  sense that matters.
//   * NO ALWAYS-FALSE GUARDS — `if false`, `#[cfg(any())]` and friends are
//                  disablement wearing the costume of a conditional.
//
// A refactor that renames a local variable inside a protected file passes.
// Deleting the `cgroup.kill` write, stubbing `wait_empty` to return
// immediately, or wedging the RuntimeClass template behind an always-false
// condition does not.
//
// Usage:
//   node scripts/resize-cutover-retention-manifest.mjs [--root <dir>]
//   node scripts/resize-cutover-retention-manifest.mjs --list-inputs
//
// Exit codes: 0 = every asset retained, 1 = a retirement was detected,
// 2 = the gate could not run (its own subject is missing).

import { readFileSync, existsSync, statSync } from 'node:fs';
import { join, extname } from 'node:path';

// ---------------------------------------------------------------------------
// Paths, spelled once.
// ---------------------------------------------------------------------------

const LAUNCHER_CRATE = 'server/crates/djinn-cgroup-launcher';
const LAUNCHER_LIB = `${LAUNCHER_CRATE}/src/lib.rs`;
const LAUNCHER_MAIN = `${LAUNCHER_CRATE}/src/main.rs`;
const LAUNCHER_BROKER = `${LAUNCHER_CRATE}/src/broker.rs`;
const LAUNCHER_TRANSPORT = `${LAUNCHER_CRATE}/src/transport.rs`;
const LAUNCHER_CARGO = `${LAUNCHER_CRATE}/Cargo.toml`;
const CONTAINMENT_TEST = `${LAUNCHER_CRATE}/tests/containment_and_escalation.rs`;
const LIFT_BOUNDARY_TEST = `${LAUNCHER_CRATE}/tests/brokered_lease_lift_boundary.rs`;

const PROCESS_BROKER = 'server/crates/djinn-agent/src/process_broker.rs';
const PROCESS = 'server/crates/djinn-agent/src/process.rs';

const CHILD_FS = 'server/crates/djinn-k8s/src/launcher_child_fs.rs';
const K8S_LAUNCHER = 'server/crates/djinn-k8s/src/launcher.rs';
const K8S_JOB = 'server/crates/djinn-k8s/src/job.rs';
const K8S_LAUNCHER_TESTS = 'server/crates/djinn-k8s/src/launcher_tests.rs';

const RUNTIMECLASS_TEMPLATE =
    'deploy/helm/djinn/templates/runtimeclass-cgroup-writable.yaml';
const NODE_CONFORMANCE = 'deploy/node/k3s/djinn-cgroup-writable-conformance.sh';
const CHART_VALUES = 'deploy/helm/djinn/values.yaml';
const RBAC_TEST = 'deploy/helm/djinn/tests/pods-resize-rbac.sh';
const CGROUP_RENDER_TEST = 'deploy/helm/djinn/tests/cgroup-writable-render.sh';
const CHART_CONTRACT_RUNNER = 'scripts/test-helm-chart-contracts.sh';

const WORKSPACE_MANIFEST = 'server/Cargo.toml';
const QUALITY_GATE = '.github/workflows/quality-gate.yml';
const RELEASE_WORKFLOW = '.github/workflows/release.yml';

// ---------------------------------------------------------------------------
// Always-false disablement signatures.
//
// None of these ever appears legitimately in the protected set. A conditional
// that can never be true is a deletion that still passes `[ -f "$path" ]`, so
// it is rejected wherever it shows up in a protected Rust or template file.
// ---------------------------------------------------------------------------

const ALWAYS_FALSE = [
    { pattern: /\bif\s+false\b/, name: 'if false' },
    { pattern: /\bif\s+!\s*true\b/, name: 'if !true' },
    { pattern: /#\[cfg\(any\(\)\)\]/, name: '#[cfg(any())]' },
    { pattern: /\bcfg!\(any\(\)\)/, name: 'cfg!(any())' },
    { pattern: /\bif\s+0\s*==\s*1\b/, name: 'if 0 == 1' },
    { pattern: /\{\{-?\s*if\s+false\s*-?\}\}/, name: '{{ if false }}' },
];

// ---------------------------------------------------------------------------
// The protected assets.
//
// `content` entries are asserted against COMMENT-STRIPPED source, so a
// commented-out write is an absent write. `reachability` entries are the same
// assertion made about a PRODUCTION call site: presence without a caller is
// retirement with extra steps.
// ---------------------------------------------------------------------------

const ASSETS = [
    {
        id: 'cgroup-launcher-crate',
        title: 'the djinn-cgroup-launcher crate',
        files: [LAUNCHER_CARGO, LAUNCHER_LIB, LAUNCHER_MAIN, LAUNCHER_BROKER, LAUNCHER_TRANSPORT],
        content: [
            {
                path: LAUNCHER_CARGO,
                pattern: /name\s*=\s*"djinn-cgroup-launcher"/,
                why: 'the crate must still declare itself; a renamed crate is a retired one under a different name',
            },
            {
                path: WORKSPACE_MANIFEST,
                pattern: /crates\/djinn-cgroup-launcher/,
                why: 'the crate must stay a workspace member, or `cargo test --workspace` stops compiling and running its containment suites',
            },
        ],
        reachability: [
            {
                path: RELEASE_WORKFLOW,
                pattern: /-p\s+djinn-cgroup-launcher/,
                why: 'the release lane must still build the launcher binary, or the shipped image has no launcher to run',
            },
        ],
        alwaysFalseScan: [LAUNCHER_LIB, LAUNCHER_BROKER, LAUNCHER_MAIN],
    },
    {
        id: 'cgroup-kill',
        title: 'the cgroup.kill write site',
        files: [LAUNCHER_LIB, PROCESS_BROKER, PROCESS],
        content: [
            {
                path: LAUNCHER_LIB,
                pattern: /write_leaf\(\s*leaf\.fd\s*,\s*"cgroup\.kill"\s*,\s*"1"\s*\)/,
                why: 'the whole-cgroup kill is what reaches EVERY descendant of the invocation leaf. A per-pid SIGKILL does not, and a commented-out write does not either',
            },
            {
                path: LAUNCHER_LIB,
                pattern: /leaf\.terminal\s*=\s*true/,
                why: 'terminal intent must be marked BEFORE the kill, so a concurrent or replayed grant can never lift a leaf that is being torn down',
            },
            {
                path: PROCESS_BROKER,
                pattern: /fn\s+kill\s*\(\s*&mut\s+self\s*\)\s*->\s*io::Result<\(\)>\s*;/,
                why: 'the broker-side handle must still expose kill',
            },
        ],
        reachability: [
            {
                path: PROCESS,
                pattern: /child\.kill\(\)/,
                why: 'the invocation teardown path must still call kill; a kill nobody calls contains nothing',
            },
        ],
        alwaysFalseScan: [LAUNCHER_LIB, PROCESS_BROKER],
    },
    {
        id: 'wait-empty',
        title: 'wait_empty, the drain observation',
        files: [LAUNCHER_LIB, LAUNCHER_BROKER, LAUNCHER_TRANSPORT, PROCESS_BROKER, PROCESS],
        content: [
            {
                path: LAUNCHER_LIB,
                pattern: /pub\s+fn\s+wait_empty\s*\(/,
                why: 'the launcher must still expose the drain observation',
            },
            {
                path: LAUNCHER_LIB,
                pattern: /read_leaf\(\s*leaf\.fd\s*,\s*"cgroup\.events"\s*\)/,
                why: 'emptiness is READ from cgroup.events. A wait_empty that returns Ok(()) without reading the kernel is the disablement this gate exists to reject',
            },
            {
                path: LAUNCHER_LIB,
                pattern: /populated_zero\s*\(/,
                why: 'the read must be interpreted: `populated 0` is the condition, not the mere fact that the file was readable',
            },
            {
                path: LAUNCHER_LIB,
                pattern: /Error::StillPopulated/,
                why: 'a still-populated leaf must be able to FAIL. A wait_empty with no failure arm cannot observe anything',
            },
            {
                path: LAUNCHER_LIB,
                pattern: /self\.wait_empty\(\s*leaf\s*\)\s*\?/,
                why: 'removal must be gated on the drain; removing a populated leaf is the leak this observation prevents',
            },
            {
                path: LAUNCHER_BROKER,
                pattern: /launcher\.wait_empty\(/,
                why: 'the privileged broker must still drive the drain on behalf of the unprivileged worker',
            },
            {
                path: LAUNCHER_TRANSPORT,
                pattern: /pub\s+fn\s+wait_empty\s*\(/,
                why: 'the control protocol must still carry the drain frame, or the worker cannot ask for it',
            },
            {
                path: PROCESS_BROKER,
                pattern: /fn\s+wait_empty\s*\(\s*&mut\s+self\s*\)\s*->\s*io::Result<\(\)>\s*;/,
                why: 'the worker-side handle must still expose the drain',
            },
        ],
        reachability: [
            {
                path: PROCESS,
                pattern: /child\.wait_empty\(\)/,
                why: 'the invocation teardown path must still WAIT for the drain. This is the one call that turns `cgroup.kill` from asynchronous into observed',
            },
        ],
        alwaysFalseScan: [LAUNCHER_LIB, LAUNCHER_BROKER, LAUNCHER_TRANSPORT, PROCESS_BROKER],
    },
    {
        id: 'process-broker',
        title: 'the worker-side process broker',
        files: [PROCESS_BROKER, PROCESS],
        content: [
            {
                path: PROCESS_BROKER,
                pattern: /trait\s+ProcessHandle\s*:/,
                why: 'the handle trait is the seam every brokered invocation is torn down through',
            },
            {
                path: PROCESS_BROKER,
                pattern: /trait\s+CgroupLauncherClient\s*:/,
                why: 'the client trait is how the worker reaches the privileged launcher at all',
            },
            {
                path: PROCESS_BROKER,
                pattern: /fn\s+fenced_lift\s*\(\s*&mut\s+self\s*\)/,
                why: 'the fenced lift is the authority boundary; an unfenced lift is goxi blocker 14',
            },
        ],
        reachability: [
            {
                path: PROCESS,
                pattern: /\.map_err\(LeaseInvocationError::Launcher\)/,
                why: 'the production invocation path must still surface launcher failures rather than swallow them',
            },
        ],
        alwaysFalseScan: [PROCESS_BROKER],
    },
    {
        id: 'broker-mounts',
        title: 'the launcher child mount set',
        files: [CHILD_FS],
        content: [
            {
                path: CHILD_FS,
                pattern: /pub\s+fn\s+launcher_volume_mounts\s*\(/,
                why: 'a brokered command runs in the LAUNCHER container mount namespace; without these mounts every child chdir is ENOENT',
            },
            {
                path: CHILD_FS,
                pattern: /pub\s+fn\s+launcher_data_mounts\s*\(/,
                why: 'the child surfaces (mirror, projects, cache, workspace) must still be re-rendered onto the sidecar',
            },
            {
                path: CHILD_FS,
                pattern: /pub\s+fn\s+launcher_scratch_volumes\s*\(/,
                why: 'the scratch volumes backing /tmp, /var/tmp and the home dir must still be declared',
            },
            {
                path: CHILD_FS,
                pattern: /worker_launcher_ipc_mount\(\)/,
                why: 'the IPC volume is the only path shared by the worker and the sidecar; the broker socket lives on it',
            },
        ],
        reachability: [
            {
                path: K8S_LAUNCHER,
                pattern: /launcher_child_fs::launcher_volume_mounts\(/,
                why: 'the renderer must still attach the mount set to the sidecar it emits',
            },
            {
                path: K8S_JOB,
                pattern: /launcher_child_fs::launcher_scratch_volumes\(\)/,
                why: 'the Job renderer must still declare the volumes those mounts resolve against',
            },
        ],
        alwaysFalseScan: [CHILD_FS],
    },
    {
        id: 'runtimeclass-and-node-assets',
        title: 'the writable-cgroup RuntimeClass and node assets',
        files: [RUNTIMECLASS_TEMPLATE, NODE_CONFORMANCE, CHART_VALUES],
        content: [
            {
                path: RUNTIMECLASS_TEMPLATE,
                pattern: /^\{\{-\s*if\s+\.Values\.cgroupWritable\.runtimeClass\.enabled\s*\}\}$/m,
                why: 'the template must be guarded by exactly the one operator-facing switch. An extra conjunct, or a literal false, renders the class unreachable while leaving the file in place',
            },
            {
                path: RUNTIMECLASS_TEMPLATE,
                pattern: /name:\s*djinn-cgroup-writable$/m,
                why: 'the task-run class is what keeps ordinary task-run Pods off unconformed nodes',
            },
            {
                path: RUNTIMECLASS_TEMPLATE,
                pattern: /name:\s*djinn-cgroup-writable-probe$/m,
                why: 'the probe class is what breaks the marker/probe deadlock; without it node conformance cannot run',
            },
            {
                path: RUNTIMECLASS_TEMPLATE,
                pattern: /handler:\s*runc-cgroupwritable/,
                why: 'the containerd handler is the whole point; a class without it grants a read-only /sys/fs/cgroup',
            },
            {
                path: RUNTIMECLASS_TEMPLATE,
                pattern: /djinn\.io\/cgroup-writable:\s*"true"/,
                why: 'the node selector is the scheduling boundary the RuntimeClass admission controller merges in',
            },
            {
                path: CHART_VALUES,
                pattern: /^cgroupWritable:$/m,
                why: 'the operator-facing switch must remain declared in values.yaml, or the template guard reads a value that does not exist',
            },
            {
                path: NODE_CONFORMANCE,
                pattern: /djinn\.io\/cgroup-writable/,
                why: 'node conformance is what earns the marker the selector demands',
            },
        ],
        reachability: [
            {
                path: K8S_LAUNCHER,
                pattern: /TASK_RUN_CGROUP_RUNTIME_CLASS/,
                why: 'the renderer must still name the class on the Pod it emits',
            },
        ],
        alwaysFalseScan: [RUNTIMECLASS_TEMPLATE],
    },
    {
        id: 'credential-boundary-tests',
        title: 'the credential-boundary chart tests',
        files: [RBAC_TEST, CGROUP_RENDER_TEST, CHART_CONTRACT_RUNNER],
        content: [
            {
                path: RBAC_TEST,
                pattern: /pods\/resize/,
                why: 'the RBAC contract is what keeps the resize verb off every identity that must not hold it',
            },
            {
                path: CGROUP_RENDER_TEST,
                pattern: /runtimeClass|runtimeClassName/,
                why: 'the render contract is what proves the class actually lands on the Pod',
            },
            {
                path: CHART_CONTRACT_RUNNER,
                pattern: /\$\{?TESTS_DIR\}?"?\/\*\.sh/,
                why: 'the runner must still glob the whole contract directory. A runner that names scripts one by one silently drops the one nobody remembered to add',
            },
        ],
        reachability: [
            {
                path: CHART_CONTRACT_RUNNER,
                pattern: /deploy\/helm\/djinn\/tests/,
                why: 'the runner must still point at the directory these tests live in',
            },
        ],
        alwaysFalseScan: [],
    },
    {
        id: 'launcher-containment-tests',
        title: "the launcher crate's own containment proofs",
        files: [CONTAINMENT_TEST, LIFT_BOUNDARY_TEST],
        content: [
            {
                path: CONTAINMENT_TEST,
                pattern: /wait_empty\(/,
                why: 'the containment proof must still exercise the drain',
            },
            {
                path: LIFT_BOUNDARY_TEST,
                pattern: /wait_empty\(/,
                why: 'the brokered lift-boundary proof must still exercise the drain through the real broker protocol',
            },
        ],
        reachability: [
            {
                path: QUALITY_GATE,
                pattern: /--test\s+brokered_lease_lift_boundary/,
                why: 'a CI lane must still RUN the privileged boundary proof. A retained test file that no lane invokes is a retired test',
            },
            {
                path: QUALITY_GATE,
                pattern: /BROKERED_LIFT_EXPECTED_PROOFS/,
                why: 'the lane must still assert how many #[ignore]d proofs it expects, or a suite that executes zero of them looks green',
            },
            {
                path: QUALITY_GATE,
                pattern: /-p\s+djinn-cgroup-launcher/,
                why: 'the lane must still build the launcher crate under test',
            },
        ],
        alwaysFalseScan: [],
    },
    {
        id: 'launcher-cpu-ceiling-stays-conditional',
        title: 'the resize-v2-only launcher CPU ceiling',
        files: [K8S_LAUNCHER, K8S_LAUNCHER_TESTS],
        content: [
            {
                path: K8S_LAUNCHER,
                pattern: /fn\s+resolve_launcher_cpu_ceiling\s*\(/,
                why: 'the ceiling must stay a decision, not a constant',
            },
            {
                path: K8S_LAUNCHER,
                pattern: /if\s+protocol\.launcher_owns_leaf_quota\(\)\s*\{\s*return\s+Ok\(None\)\s*;/,
                why: 'under leaf-v1 the launcher owns the leaf cpu.max, so a container limit here becomes an ancestor clamp over EVERY invocation leaf. 7deu measured a leaf whose cpu.max read four cores while the process burned a quarter of one, with the leaf nr_throttled at 0. Removing this arm renders the limit unconditionally and reintroduces that clamp',
            },
            {
                path: K8S_LAUNCHER_TESTS,
                pattern: /fn\s+the_launcher_cpu_ceiling_is_rendered_only_under_resize_v2\s*\(/,
                why: 'the conditionality must stay under test by name; the source comment in launcher.rs points at this test',
            },
        ],
        reachability: [
            {
                path: K8S_LAUNCHER,
                pattern: /resolve_launcher_cpu_ceiling\(/,
                why: 'the resolver must still be called from the render path',
            },
        ],
        alwaysFalseScan: [K8S_LAUNCHER],
    },
];

/// A gate whose asset table shrank is a gate that stopped guarding things.
const MINIMUM_ASSETS = 9;

// ---------------------------------------------------------------------------
// Comment stripping.
//
// Whole-line comments only, deliberately. The mutation this defends against is
// "comment the write out", which always produces a line whose first non-space
// characters are `//` (or `#` in YAML/shell). Stripping trailing comments as
// well would mean parsing string literals, and a `//` inside a URL in a string
// would corrupt the very line being asserted on.
// ---------------------------------------------------------------------------

export function stripComments(source, path) {
    const ext = extname(path);
    const hashStyle = ext === '.yaml' || ext === '.yml' || ext === '.sh' || ext === '.toml';
    const out = [];
    let inBlock = false;
    for (const line of source.split('\n')) {
        const trimmed = line.trim();
        if (inBlock) {
            if (trimmed.includes('*/')) {
                inBlock = false;
            }
            out.push('');
            continue;
        }
        if (!hashStyle && trimmed.startsWith('/*')) {
            if (!trimmed.includes('*/')) {
                inBlock = true;
            }
            out.push('');
            continue;
        }
        if (!hashStyle && (trimmed.startsWith('//') || trimmed.startsWith('*'))) {
            out.push('');
            continue;
        }
        if (hashStyle && trimmed.startsWith('#')) {
            out.push('');
            continue;
        }
        out.push(line);
    }
    return out.join('\n');
}

// ---------------------------------------------------------------------------
// Evaluation.
// ---------------------------------------------------------------------------

function everyInput() {
    const paths = new Set();
    for (const asset of ASSETS) {
        for (const path of asset.files) {
            paths.add(path);
        }
        for (const check of [...asset.content, ...asset.reachability]) {
            paths.add(check.path);
        }
        for (const path of asset.alwaysFalseScan) {
            paths.add(path);
        }
    }
    return [...paths].sort();
}

function readSource(root, path) {
    const full = join(root, path);
    if (!existsSync(full)) {
        return { missing: true };
    }
    const info = statSync(full);
    if (!info.isFile() || info.size === 0) {
        return { empty: true };
    }
    const raw = readFileSync(full, 'utf8');
    return { raw, code: stripComments(raw, path) };
}

export function evaluate(root) {
    const failures = [];
    const passed = [];

    if (ASSETS.length < MINIMUM_ASSETS) {
        return {
            fatal: `the asset table holds ${ASSETS.length} assets but at least ${MINIMUM_ASSETS} are protected. Someone deleted an asset from the gate instead of from the repository.`,
        };
    }

    for (const asset of ASSETS) {
        const problems = [];

        for (const path of asset.files) {
            const source = readSource(root, path);
            if (source.missing) {
                problems.push(`DELETED: ${path} is gone.`);
            } else if (source.empty) {
                problems.push(`EMPTIED: ${path} exists but holds no content.`);
            }
        }

        for (const check of asset.content) {
            const source = readSource(root, check.path);
            if (source.missing || source.empty) {
                problems.push(`DELETED: ${check.path} is gone, so /${check.pattern.source}/ cannot be there.`);
                continue;
            }
            if (!check.pattern.test(source.code)) {
                const commentedOut = check.pattern.test(source.raw);
                problems.push(
                    commentedOut
                        ? `DISABLED: ${check.path} still contains /${check.pattern.source}/, but only inside a comment. Commenting a statement out retires it. Why it is guarded: ${check.why}.`
                        : `RETIRED: ${check.path} no longer matches /${check.pattern.source}/. Why it is guarded: ${check.why}.`,
                );
            }
        }

        for (const check of asset.reachability) {
            const source = readSource(root, check.path);
            if (source.missing || source.empty) {
                problems.push(`UNREACHABLE: ${check.path} is gone, so nothing calls into this asset from there.`);
                continue;
            }
            if (!check.pattern.test(source.code)) {
                problems.push(
                    `UNREACHABLE: ${check.path} no longer matches /${check.pattern.source}/. Presence without a caller is retirement with extra steps. Why it is guarded: ${check.why}.`,
                );
            }
        }

        for (const path of asset.alwaysFalseScan) {
            const source = readSource(root, path);
            if (source.missing || source.empty) {
                continue; // already reported above
            }
            for (const guard of ALWAYS_FALSE) {
                if (guard.pattern.test(source.code)) {
                    problems.push(
                        `DISABLED: ${path} carries an always-false guard (${guard.name}). A conditional that can never be true is a deletion that still passes an existence check.`,
                    );
                }
            }
        }

        if (problems.length > 0) {
            failures.push({ asset, problems });
        } else {
            passed.push(asset);
        }
    }

    return { failures, passed };
}

// ---------------------------------------------------------------------------
// CLI.
// ---------------------------------------------------------------------------

function main(argv) {
    let root = process.env.RESIZE_RETENTION_ROOT ?? process.cwd();
    for (let index = 0; index < argv.length; index += 1) {
        if (argv[index] === '--root') {
            root = argv[index + 1];
            index += 1;
            if (!root) {
                process.stderr.write('FATAL: --root requires a value\n');
                return 2;
            }
        } else if (argv[index] === '--list-inputs') {
            process.stdout.write(`${everyInput().join('\n')}\n`);
            return 0;
        } else {
            process.stderr.write(`FATAL: unknown argument ${argv[index]}\n`);
            return 2;
        }
    }

    if (!existsSync(root)) {
        process.stderr.write(`FATAL: root does not exist: ${root}\n`);
        return 2;
    }

    const result = evaluate(root);
    if (result.fatal) {
        process.stderr.write(`FATAL: ${result.fatal}\n`);
        return 2;
    }

    for (const asset of result.passed) {
        process.stdout.write(`ok: ${asset.id} — ${asset.title} is retained.\n`);
    }

    if (result.failures.length === 0) {
        process.stdout.write(
            `check-resize-cutover-retention: OK (${result.passed.length} protected assets retained)\n`,
        );
        return 0;
    }

    for (const failure of result.failures) {
        process.stderr.write(`\nFAIL: ${failure.asset.id} — ${failure.asset.title}\n`);
        for (const problem of failure.problems) {
            process.stderr.write(`      ${problem}\n`);
        }
    }
    process.stderr.write(
        '\nThe in-place resize cutover retires NOTHING in the containment and\n' +
        'credential boundary. If one of these assets legitimately moved, update\n' +
        'scripts/resize-cutover-retention-manifest.mjs to point at its new home —\n' +
        'do not delete the entry.\n',
    );
    return 1;
}

export { ASSETS, everyInput, MINIMUM_ASSETS };

if (import.meta.url === `file://${process.argv[1]}`) {
    process.exit(main(process.argv.slice(2)));
}
