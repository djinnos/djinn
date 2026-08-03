# cgroup-launcher retirement decision runbook

`CGROUP_RETIREMENT_DECISION_SCOPE: this is an operator procedure, not an authorization to delete the launcher.`

This runbook composes the repository gates delivered by the PREP range guard,
checked asset manifest, evidence verifier, aggregate rollback rehearsal, and
commit-bound landing verifier. A green repository check proves only its
checked repository input. It never proves production-class capture, a live
cluster rehearsal, deployment observation, or human approval.

`CGROUP_RETIREMENT_OPERATOR_ONLY_BOUNDARY: production-class evidence capture, live RETIRE_CANARY rehearsal, deployment observation, and required human approval are operator actions; they are not repository-test success or worker-completed proof.`

Use `deploy/runbooks/cgroup-launcher-retirement-decision-record.md` for every
attempt. Copy it into the change record; do not replace its fields with a
free-form summary.

## State law

`CGROUP_RETIREMENT_KEEP_DEFAULT: before landing, every failed, refused, skipped, inconclusive, stale, missing-owner, rejected, or bypassed prerequisite records KEEP.`

`CGROUP_RETIREMENT_RETIRE_RULE: RETIRE is permitted only by a complete --landing M record.`

`CGROUP_RETIREMENT_RECOVERY_EXCLUSIVE: after a candidate or post-deploy fault, RECOVERY is the only state until aggregate tree byte identity, node-asset restoration, and live launcher-leaf evidence are proven.`

KEEP means no deletion commits land. The launcher render, RuntimeClass, node
assets, 3i92 retention gate, launcher leaf creation, and lifting remain armed.
A partial recovery is not KEEP and is not RETIRE. Do not relabel RECOVERY to
make a dashboard terminal: dispatch remains paused while recovery is proven.

The loss record is mandatory if RETIRE is considered: the launcher UID
separation and complete child-seccomp boundary are **lost**. Do not claim an
untested replacement; a second in-worker seccomp installer is not a
replacement in this decision.

## Ordered procedure

### 1. PREP: establish the repository range

`CGROUP_RETIREMENT_STEP: 1 PREP`

Record `PREP_BASE` and `PREP_HEAD`, then run the range guard. This is a
repository-verifiable preparation gate, not a production observation:

```sh
scripts/check-cgroup-retirement-gate.sh --prep PREP_BASE PREP_HEAD
```

Attach the command output and the checked asset manifest
`scripts/cgroup-retirement-assets.json` to the record. The manifest is the
complete candidate/preserved inventory: it prevents a candidate from deleting
or drifting launcher render, RuntimeClass, node, retention, uid/gid, fsGroup,
or required test assets. A refusal, missing asset, range violation, or stale
PREP identity is KEEP.

### 2. Evidence capture: repository candidate plus operator capture

`CGROUP_RETIREMENT_STEP: 2 EVIDENCE_CAPTURE`

First verify the immutable repository candidate and its `PREP_HEAD` digest:

```sh
scripts/verify-cgroup-retirement-evidence.sh --candidate RETIRE_HEAD
scripts/check-cgroup-retirement-gate.sh --deploy --candidate RETIRE_HEAD --inputs scripts/fixtures/cgroup-retirement/gate/all-green.json
```

The second command establishes only repository candidate-review eligibility;
it does not deploy, release, withdraw a node, or authorize RETIRE.

`CGROUP_RETIREMENT_PRODUCTION_CAPTURE_OPERATOR_ONLY: capture five production-class canaries and the final run, including zero memory.events.oom_kill delta, required headroom, reservation-aware node fit, Kueue width, and PREP_HEAD identity, as an operator action.`

Attach the operator-captured immutable evidence to the decision record. If any
capture is failed, refused, skipped, inconclusive, stale, or missing, record
KEEP. A repository fixture or green verifier is never a substitute for this
live capture.

### 3. Candidate review: preserve the default

`CGROUP_RETIREMENT_STEP: 3 CANDIDATE_REVIEW`

Review the PREP output, candidate evidence, range gate output, and manifest
against the record. Confirm every mandatory owner is present and every input
is current. Required human approval is an operator action; do not mark it
satisfied from CI, a worker, or this runbook contract.

`CGROUP_RETIREMENT_HUMAN_APPROVAL_OPERATOR_ONLY: effective approving-review count, configured owner coverage, approved current head, and PR identity are human/hosting evidence recorded by the operator.`

Any failed, refused, skipped, inconclusive, stale, missing-owner, rejected,
or bypassed review prerequisite is KEEP. Do not prepare a deletion landing
while the record says KEEP.

### 4. Rollback rehearsal: repository proof and live rehearsal

`CGROUP_RETIREMENT_STEP: 4 ROLLBACK_REHEARSAL`

Run the hermetic aggregate rehearsal before any candidate is landed:

```sh
scripts/rehearse-cgroup-retirement-rollback.sh
```

It proves on a disposable branch that candidate commits revert
newest-to-oldest to `RETIRE_BASE`, the tracked tree is byte-identical, modeled
node assets restore, and a restored launcher produces a valid `cpu.max` leaf.
It does not observe a real node or production launcher leaf.

`CGROUP_RETIREMENT_RETIRE_CANARY_OPERATOR_ONLY: rehearse RETIRE_CANARY live with the production-class operator procedure; repository rehearsal success is not evidence that RETIRE_CANARY happened.`

If the rehearsal or candidate faults, enter RECOVERY. Follow the existing
[cgroup launcher re-arm recovery procedure](cgroup-launcher-rearm.md); do not
duplicate or improvise its re-arm steps here.

### 5. RECOVERY: nonterminal fault handling

`CGROUP_RETIREMENT_STEP: 5 RECOVERY`

On any candidate or post-deploy fault, pause dispatch, capture snapshots, and
record RECOVERY. The only exit evidence is all three items below:

`CGROUP_RETIREMENT_RECOVERY_PROOFS: aggregate tree byte identity with RETIRE_BASE; node-asset restoration; live launcher-leaf evidence.`

The aggregate tree proof must be byte-identical to `RETIRE_BASE`; node assets
must be restored; and the launcher leaf must be observed live by the operator.
Until all three exist, terminal labels are refused: do not call the incomplete
state KEEP or RETIRE. Use `cgroup-launcher-rearm.md` for recovery execution.

### 6. Landing: bind the complete record to M

`CGROUP_RETIREMENT_STEP: 6 LANDING`

Only after all prior gates and operator actions are complete, set `M` to the
exact 40-hex landing commit and run:

```sh
scripts/verify-cgroup-retirement-evidence.sh --landing M
```

This repository verifier consumes the commit-bound landing record and composes
candidate evidence, the deploy range gate, aggregate rollback rehearsal, and
the deterministic outcome classifier. It validates image OCI revision M,
matching render/node/Workload digests, Pod annotation M, and confirmed final
one-container dispatch fields in the record. It cannot contact GitHub,
Kubernetes, a registry, or a live rollout; its approval/review and deployment
fields are recorded evidence, not independently observed facts.

`CGROUP_RETIREMENT_LANDING_VERIFIER: scripts/verify-cgroup-retirement-evidence.sh --landing M`

A failed or incomplete `--landing M` record is KEEP before landing. No direct
push, bypass, stale approval, absent owner, rejected review, self-certification,
or digest mismatch may be normalized into RETIRE.

### 7. RETIRE: the only deletion authorization

`CGROUP_RETIREMENT_STEP: 7 RETIRE`

RETIRE may be recorded only when the decision record is complete, the landing
verifier above succeeds for M, all human/operator fields are attested, and no
candidate/post-deploy fault is active. Land candidate deletion commits only
under that complete M-bound RETIRE record. Record the irreducible losses
honestly: `launcher_uid_separation: lost`,
`child_seccomp_boundary: lost-complete`, and
`second_in_worker_seccomp_installer: not-claimed`.

### 8. KEEP: preserve the armed baseline

`CGROUP_RETIREMENT_STEP: 8 KEEP`

For every pre-landing non-green condition, record KEEP and land no deletion
commits. Preserve the launcher render, RuntimeClass, node assets, 3i92
retention gate, leaf creation, and lifting. PREP environment/reaping hardening
may land while KEEP remains the decision.

## What the repository contract means

`deploy/runbooks/tests/cgroup-launcher-retirement-decision.sh` reads this
runbook and the decision-record template. It checks mandatory commands,
ordered terminal rules, operator-only boundaries, loss disclosures, and record
fields with negative fixtures. It does not contact a cluster, observe
RETIRE_CANARY, collect production evidence, watch deployment, or grant human
approval. A passing contract means this decision procedure remains written and
fail-closed; it never authorizes deletion by itself.
