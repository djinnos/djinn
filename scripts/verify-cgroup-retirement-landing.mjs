// Commit-bound, repository-only landing verifier. It composes the immutable
// candidate verifier, range interlock, rollback rehearsal, and state classifier;
// it never consults GitHub, Kubernetes, a registry, or a live rollout.
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const [root, commit] = process.argv.slice(2);
const scriptDir = dirname(new URL(import.meta.url).pathname);
const fail = (subject, message) => { process.stderr.write(`REJECT landing ${subject}: ${message}\n`); process.exit(1); };
const read = (path, subject) => { try { return JSON.parse(readFileSync(path, 'utf8')); } catch (error) { fail(subject, `invalid JSON (${error.message})`); } };
const object = (value, subject) => { if (!value || typeof value !== 'object' || Array.isArray(value)) fail(subject, 'must be an object'); return value; };
const exact = (value, keys, subject) => { object(value, subject); const got = Object.keys(value).sort(); const expected = [...keys].sort(); if (got.length !== expected.length || got.some((key, i) => key !== expected[i])) fail(subject, `has unknown or missing fields; expected exactly ${expected.join(', ')}`); };
const text = (value, subject, pattern = /^.+$/) => { if (typeof value !== 'string' || !pattern.test(value)) fail(subject, 'must be a valid non-empty string'); return value; };
const digest = (value, subject) => text(value, subject, /^sha256:[0-9a-f]{64}$/);
const sha = /^[0-9a-f]{40}$/;
if (!root || !commit || !sha.test(commit)) fail('commit', 'must be a lowercase 40-character commit identity');
const landingPath = join(root, 'landing', `${commit}.json`);
const statePath = join(root, 'landing', `${commit}.outcome.json`);
if (!existsSync(landingPath) || !existsSync(statePath)) fail('evidence', `missing landing fixture for ${commit}`);
const evidence = read(landingPath, 'evidence');
exact(evidence, ['candidate', 'commit', 'deployment', 'landing_id', 'losses', 'review', 'schema_version'], 'evidence');
if (evidence.schema_version !== 1 || evidence.commit !== commit || evidence.landing_id !== `cgroup-retirement/landing/${commit}`) fail('identity', 'does not bind the requested landing commit');
text(evidence.candidate, 'candidate', /^[A-Za-z0-9_.-]+$/);

exact(evidence.review, ['approval_state', 'configured_owners', 'effective_required_approvals', 'implementer', 'no_bypass', 'pull_request', 'reviewed_payload', 'rule_snapshot', 'reviews'], 'review');
const review = evidence.review;
text(review.implementer, 'implementer', /^[A-Za-z0-9_.-]+$/);
if (!Number.isSafeInteger(review.effective_required_approvals) || review.effective_required_approvals < 1) fail('review count', 'must be a positive effective required approving-review count');
exact(review.pull_request, ['identity', 'reviewed_head'], 'pull request');
text(review.pull_request.identity, 'pull request identity', /^[1-9][0-9]*$/);
if (review.pull_request.reviewed_head !== commit) fail('pull request', 'current reviewed head is not the landing commit');
if (review.approval_state !== 'approved') fail('approval', 'PR approval state is not approved');
if (!Array.isArray(review.configured_owners) || review.configured_owners.length === 0 || new Set(review.configured_owners).size !== review.configured_owners.length) fail('owner coverage', 'configured owners must be a unique non-empty list');
review.configured_owners.forEach((owner) => text(owner, 'configured owner', /^[A-Za-z0-9_.-]+$/));
if (!Array.isArray(review.reviews) || review.reviews.length < review.effective_required_approvals) fail('review payload', 'has fewer reviews than the effective required count');
let approvals = 0; const covered = new Set();
for (const item of review.reviews) {
  exact(item, ['actor', 'head', 'state'], 'review entry');
  text(item.actor, 'review actor', /^[A-Za-z0-9_.-]+$/);
  if (item.actor === review.implementer) fail('self-certification', 'implementer cannot certify landing evidence');
  if (item.head !== commit) fail('stale-head approval', 'review is not bound to the current landing head');
  if (item.state === 'changes_requested' || item.state === 'dismissed') fail('approval', `review state is ${item.state}`);
  if (item.state !== 'approved') fail('approval', `review state is ${item.state}`);
  approvals += 1; if (review.configured_owners.includes(item.actor)) covered.add(item.actor);
}
if (approvals < review.effective_required_approvals) fail('approval', 'effective required approving-review count is not met');
if (covered.size !== review.configured_owners.length) fail('owner coverage', 'not every configured owner approved the landing commit');
exact(review.rule_snapshot, ['commit', 'configured_owners', 'effective_required_approvals'], 'rule snapshot');
if (review.rule_snapshot.commit !== commit || review.rule_snapshot.effective_required_approvals !== review.effective_required_approvals || JSON.stringify(review.rule_snapshot.configured_owners) !== JSON.stringify(review.configured_owners)) fail('rule snapshot', 'effective rules do not match the bound landing rules');
exact(review.no_bypass, ['commit', 'direct_push', 'merged_through_pull_request'], 'no bypass');
if (review.no_bypass.commit !== commit || review.no_bypass.direct_push !== false || review.no_bypass.merged_through_pull_request !== true) fail('bypass', 'direct push or PR bypass is recorded');
exact(review.reviewed_payload, ['child_seccomp_boundary', 'launcher_uid_separation', 'second_in_worker_seccomp_installer', 'untested_replacements'], 'reviewed payload');
if (review.reviewed_payload.launcher_uid_separation !== 'lost' || review.reviewed_payload.child_seccomp_boundary !== 'lost-complete' || review.reviewed_payload.second_in_worker_seccomp_installer !== 'not-claimed' || !Array.isArray(review.reviewed_payload.untested_replacements) || review.reviewed_payload.untested_replacements.length !== 0) fail('reviewed payload', 'must honestly record uid and complete child-seccomp losses without an untested replacement');

exact(evidence.deployment, ['final_dispatch', 'image', 'node_digest', 'pod_annotation', 'render_digest', 'workload_digest'], 'deployment');
exact(evidence.deployment.image, ['oci_revision', 'digest'], 'image');
digest(evidence.deployment.image.digest, 'image digest');
if (evidence.deployment.image.oci_revision !== commit) fail('image OCI revision', 'does not bind the landing commit');
for (const key of ['render_digest', 'node_digest', 'workload_digest']) { exact(evidence.deployment[key], ['commit', 'digest'], key); digest(evidence.deployment[key].digest, key); if (evidence.deployment[key].commit !== commit) fail(key, 'does not bind the landing commit'); }
exact(evidence.deployment.pod_annotation, ['commit', 'key'], 'Pod annotation');
if (evidence.deployment.pod_annotation.commit !== commit || evidence.deployment.pod_annotation.key !== 'djinn.dev/revision') fail('Pod annotation', 'does not bind the landing commit');
exact(evidence.deployment.final_dispatch, ['commit', 'container_count', 'confirmed'], 'final dispatch');
if (evidence.deployment.final_dispatch.commit !== commit || evidence.deployment.final_dispatch.container_count !== 1 || evidence.deployment.final_dispatch.confirmed !== true) fail('final dispatch', 'is not a confirmed one-container dispatch bound to the landing commit');
exact(evidence.losses, ['child_seccomp_boundary', 'launcher_uid_separation'], 'losses');
if (evidence.losses.launcher_uid_separation !== 'lost' || evidence.losses.child_seccomp_boundary !== 'lost-complete') fail('losses', 'does not match the required reviewed loss record');

const run = (program, args, subject) => { const result = spawnSync(program, args, { encoding: 'utf8', env: { ...process.env, CGROUP_RETIREMENT_ROOT: root } }); if (result.status !== 0) fail(subject, (result.stderr || result.stdout).trim() || 'refused'); return result.stdout; };
run(resolve(scriptDir, 'verify-cgroup-retirement-evidence.sh'), ['--candidate', evidence.candidate], 'candidate proof');
run(resolve(scriptDir, 'check-cgroup-retirement-gate.sh'), ['--deploy', '--candidate', evidence.candidate, '--inputs', join(root, 'gate', 'all-green.json')], 'range gate');
run(resolve(scriptDir, 'rehearse-cgroup-retirement-rollback.sh'), [], 'rollback proof');
const state = run(process.execPath, [resolve(scriptDir, 'cgroup-retirement-outcome.mjs'), statePath], 'outcome state');
if (!state.includes('RETIRE one-container-dispatch-authorized')) fail('outcome state', 'landing preconditions did not reach RETIRE');
process.stdout.write(`verify-cgroup-retirement-evidence: LANDING OK ${commit}\n`);
