// Engine for verify-cgroup-retirement-evidence.sh. Kept repository-only: all
// inputs are immutable JSON fixtures, and every byte quantity is parsed as BigInt.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const [root, candidateName] = process.argv.slice(2);
const fail = (subject, message) => {
    process.stderr.write(`REJECT ${subject}: ${message}\n`);
    process.exit(1);
};
const readJson = (path, subject) => {
    try { return JSON.parse(readFileSync(path, 'utf8')); }
    catch (error) { fail(subject, `invalid JSON (${error.message})`); }
};
const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const exactKeys = (value, keys, subject) => {
    if (!isObject(value)) fail(subject, 'must be an object');
    const actual = Object.keys(value).sort();
    const expected = [...keys].sort();
    if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
        fail(subject, `has unknown or missing fields; expected exactly ${expected.join(', ')}`);
    }
};
const string = (value, subject, pattern = /^.+$/) => {
    if (typeof value !== 'string' || !pattern.test(value)) fail(subject, 'must be a valid non-empty string');
    return value;
};
const bytes = (value, subject) => {
    // Decimal integer strings only: JSON numbers can silently lose byte precision.
    if (typeof value !== 'string' || !/^(0|[1-9][0-9]*)$/.test(value)) fail(subject, 'must be a canonical non-negative integer byte string');
    return BigInt(value);
};
const digest = (value, subject) => string(value, subject, /^sha256:[0-9a-f]{64}$/);

const schema = readJson(join(root, 'schema.json'), 'schema');
exactKeys(schema, ['schema_name', 'schema_version'], 'schema');
if (schema.schema_name !== 'djinn-cgroup-retirement-evidence' || schema.schema_version !== 1) {
    fail('schema', 'unsupported immutable schema version');
}
const prep = readJson(join(root, 'PREP_HEAD.json'), 'PREP_HEAD');
exactKeys(prep, ['identity_digests', 'prep_head', 'schema_version'], 'PREP_HEAD');
if (prep.schema_version !== 1 || prep.prep_head !== 'PREP_HEAD') fail('PREP_HEAD', 'invalid PREP_HEAD identity');
exactKeys(prep.identity_digests, ['evidence', 'kueue_width', 'quota', 'reservation'], 'PREP_HEAD identity_digests');
for (const key of Object.keys(prep.identity_digests)) digest(prep.identity_digests[key], `PREP_HEAD ${key} digest`);

const evidence = readJson(join(root, 'candidates', `${candidateName}.json`), `candidate ${candidateName}`);
exactKeys(evidence, ['evidence_id', 'identity_digests', 'prep_head', 'runs', 'schema_version', 'subject', 'node_fit'], 'evidence');
if (evidence.schema_version !== 1) fail('evidence', 'schema_version must be 1');
if (evidence.prep_head !== prep.prep_head) fail('evidence', 'stale PREP_HEAD identity');
if (evidence.evidence_id !== `cgroup-retirement/${candidateName}`) fail('evidence', 'evidence_id must bind the requested candidate');
exactKeys(evidence.identity_digests, ['evidence', 'kueue_width', 'quota', 'reservation'], 'identity_digests');
for (const key of Object.keys(prep.identity_digests)) {
    const actual = digest(evidence.identity_digests[key], `${key} digest`);
    if (actual !== prep.identity_digests[key]) fail(key, 'digest does not match PREP_HEAD');
}
exactKeys(evidence.subject, ['image_digest', 'node_name', 'pod_name', 'pod_uid', 'run_id'], 'subject');
string(evidence.subject.run_id, 'subject run_id');
string(evidence.subject.pod_name, 'subject pod_name');
string(evidence.subject.pod_uid, 'subject pod_uid', /^[0-9a-f-]{36}$/);
digest(evidence.subject.image_digest, 'subject image_digest');
string(evidence.subject.node_name, 'subject node_name');

if (!Array.isArray(evidence.runs) || evidence.runs.length !== 6) fail('runs', 'requires exactly five canaries and one final run');
const wantedRoles = ['canary-1', 'canary-2', 'canary-3', 'canary-4', 'canary-5', 'final'];
const seen = new Set();
let finalRun;
for (const run of evidence.runs) {
    exactKeys(run, ['cgroup_path', 'ceiling_bytes', 'image_digest', 'memory_events_oom_kill_after', 'memory_events_oom_kill_before', 'node_name', 'pod_name', 'pod_uid', 'role', 'run_id', 'sum_bytes'], 'run');
    const role = string(run.role, 'run role');
    if (!wantedRoles.includes(role) || seen.has(role)) fail('runs', `requires each role once; invalid role ${role}`);
    seen.add(role);
    string(run.run_id, `${role} run_id`);
    string(run.pod_name, `${role} pod_name`);
    string(run.pod_uid, `${role} pod_uid`, /^[0-9a-f-]{36}$/);
    digest(run.image_digest, `${role} image_digest`);
    string(run.node_name, `${role} node_name`);
    string(run.cgroup_path, `${role} cgroup_path`, /^\/kubepods(?:\/|$)/);
    if (run.image_digest !== evidence.subject.image_digest || run.node_name !== evidence.subject.node_name) {
        fail(role, 'image or node identity does not match the declared production subject');
    }
    const before = bytes(run.memory_events_oom_kill_before, `${role} memory.events.oom_kill before`);
    const after = bytes(run.memory_events_oom_kill_after, `${role} memory.events.oom_kill after`);
    if (after !== before) fail(role, 'memory.events.oom_kill delta is not zero');
    const sum = bytes(run.sum_bytes, `${role} sum_bytes`);
    const ceiling = bytes(run.ceiling_bytes, `${role} ceiling_bytes`);
    // ceil(20% * sum) without floating point: ceil(sum / 5).
    const margin = [512n * 1024n * 1024n, (sum + 4n) / 5n].reduce((a, b) => a > b ? a : b);
    if (ceiling < sum + margin) fail(role, 'ceiling is below sum + max(512Mi, ceil(20% * sum))');
    if (role === 'final') finalRun = run;
}
if (seen.size !== wantedRoles.length) fail('runs', 'missing a required canary or final run');
if (finalRun.run_id !== evidence.subject.run_id || finalRun.pod_name !== evidence.subject.pod_name || finalRun.pod_uid !== evidence.subject.pod_uid) {
    fail('final', 'run and Pod identity does not match the declared production subject');
}

exactKeys(evidence.node_fit, ['allocatable_bytes', 'candidate_request_bytes', 'eviction_reservation_bytes', 'kube_reservation_bytes', 'other_pod_requests_bytes', 'system_reservation_bytes'], 'node_fit');
const allocatable = bytes(evidence.node_fit.allocatable_bytes, 'node_fit allocatable_bytes');
const system = bytes(evidence.node_fit.system_reservation_bytes, 'node_fit system_reservation_bytes');
const kube = bytes(evidence.node_fit.kube_reservation_bytes, 'node_fit kube_reservation_bytes');
const eviction = bytes(evidence.node_fit.eviction_reservation_bytes, 'node_fit eviction_reservation_bytes');
const other = bytes(evidence.node_fit.other_pod_requests_bytes, 'node_fit other_pod_requests_bytes');
const candidate = bytes(evidence.node_fit.candidate_request_bytes, 'node_fit candidate_request_bytes');
if (system + kube + eviction + other + candidate > allocatable) {
    fail('node_fit', 'allocatable memory does not fit reservations, other Pod requests, and candidate request');
}
process.stdout.write(`verify-cgroup-retirement-evidence: OK ${candidateName}\n`);
