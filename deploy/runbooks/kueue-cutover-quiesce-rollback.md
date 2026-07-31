# Kueue cutover: quiesce and rollback runbook

KUEUE_CUTOVER_SCOPE: this runbook does not perform a cutover and does not claim one has happened. It is the ordered contract an operator executes, and every gate in it that a repository can execute is executed by `deploy/kueue/preflight.sh` rather than remembered.

The cutover moves build admission from the Postgres `build_leases` ledger to Kueue. Two of its preconditions have already caused production outages when they were held as operator memory instead of as a check, so both are written here as laws with an executable enforcer.

## The two laws

KUEUE_CUTOVER_ORDERING_LAW: RuntimeClass `djinn-cgroup-writable` must EXIST on the target cluster before any namespace carries `djinn.io/kueue-managed`.

KUEUE_CUTOVER_WEDGE_MECHANISM: `server/crates/djinn-k8s/src/job.rs` asserts `required cgroup launcher requires runtimeClassName: djinn-cgroup-writable`. That is an `assert!` — a renderer PANIC, not a rejected Job. Label the namespace first and two fail-closed gates stack: Kueue holds the Job it captured, and the code that would have produced the next Job panics before it can. Every dispatch stops. This is the shape of the v0.7.25 outage (launcher armed without the RuntimeClass) and of the 2026-07-30 pod-permit outage. `deploy/kueue/preflight.sh --mode cutover` exits 10 and names the RuntimeClass when it is absent.

KUEUE_CUTOVER_DRAIN_LAW: the authority flip requires a quiesced fleet — zero live build-capable task-run Jobs, zero live graph-warm Jobs, zero Kueue Workloads in the namespace, and zero nonterminal `build_leases` rows. A task-run that is live across the flip is adjudicated by neither ledger: not the one it was admitted under, not the one it lands in.

KUEUE_CUTOVER_FENCE_IS_SYMMETRIC: the same four populations must be at zero before a revert. Reverting across a live fleet strands the same work in the mirror image, so `deploy/kueue/preflight.sh --mode rollback` enforces exactly the same fence and refuses in the other direction.

KUEUE_CUTOVER_PREFLIGHT_REQUIRED: `deploy/kueue/preflight.sh` must exit 0 immediately before the flip, and again immediately before any revert. A nonzero exit is a stop, never a warning to be argued with — each exit code names one population (10 RuntimeClass, 20 task-run Jobs, 21 graph-warm Jobs, 22 Workloads, 30 `build_leases` rows, 40 ledger unreadable, 41 API unreadable). Codes 40 and 41 are failures, not skips: a population that could not be read is never an empty one.

## Cutover sequence

KUEUE_CUTOVER_STEP: 0 — record the rollback pair. Write down the currently deployed djinn image reference and the chart version/values in force, byte-for-byte, before anything changes. This recorded pair is what a revert restores; a rollback that reconstructs it from memory is not a rollback.

KUEUE_CUTOVER_STEP: 1 — prove the prerequisite release is inert. `deploy/kueue/zero-capture-gate.sh` must have passed on a disposable cluster (see `deploy/runbooks/kueue-inert-release-zero-capture.md`). Kueue is installed and capturing nothing before the cutover begins.

KUEUE_CUTOVER_STEP: 2 — install the RuntimeClass. Deploy the `djinn` chart with `cgroupWritable.runtimeClass.enabled=true` and confirm `kubectl get runtimeclass djinn-cgroup-writable` returns it. This step exists on its own, before the namespace is touched, purely to satisfy KUEUE_CUTOVER_ORDERING_LAW.

KUEUE_CUTOVER_STEP: 3 — pause dispatch and drain. Stop admitting new task-runs, then wait for every live build-capable task-run Job and every graph-warm Job to reach a terminal condition and for every `build_leases` row to reach state `terminal`. Draining is waiting, not deleting: killing a live Job leaves an occupying ledger row behind.

KUEUE_CUTOVER_STEP: 4 — run the preflight. `deploy/kueue/preflight.sh --context <ctx> --mode cutover` must exit 0. Do not proceed on any other exit code.

KUEUE_CUTOVER_STEP: 5 — flip authority in ONE upgrade.
KUEUE_CUTOVER_ATOMIC_UPGRADE: the ledger-free image, the queue-named/suspended Job rendering, and the `kubectl label namespace` step that applies `djinn.io/kueue-managed=true` must land in a single `helm upgrade`. Split across two upgrades, one of the intermediate states is a cluster where Kueue gates Jobs that the renderer will not produce, or a renderer emitting queue names no controller is watching.

KUEUE_CUTOVER_STEP: 6 — resume dispatch and collect the resume evidence.

KUEUE_RESUME_EVIDENCE: resume is proven by an actually dispatched task-run reaching a running Pod under the new authority — not by a green Helm release, not by a `Workload` object existing, and not by a status field.
KUEUE_EVIDENCE_IS_OPERATOR_COLLECTED: this is an operator observation made after the change lands. No repository check in this branch observes production, and none of them can; a passing contract here means the sequence is written down and enforceable, never that a cutover happened.

## Rollback sequence

KUEUE_ROLLBACK_TRIGGER: any of — dispatch does not resume within the observation window, Jobs are admitted but never start, or the preflight's populations reappear without a matching dispatch.

KUEUE_ROLLBACK_STEP: 1 — pause dispatch and drain again, by the same definition as cutover step 3. A revert is an authority flip; it gets the same fence.

KUEUE_ROLLBACK_STEP: 2 — run the preflight in the reverse direction. `deploy/kueue/preflight.sh --context <ctx> --mode rollback` must exit 0.
KUEUE_ROLLBACK_SYMMETRIC_FENCE: this is the same fence, refusing in the other direction, and it is what stops a revert from stranding live work.

KUEUE_ROLLBACK_STEP: 3 — restore the recorded pair in ONE upgrade. The image reference and chart values recorded in cutover step 0 go back byte-for-byte, and the `djinn.io/kueue-managed` namespace label is removed in that same `helm upgrade`.
KUEUE_ROLLBACK_RUNTIME_CLASS_STAYS: RuntimeClass `djinn-cgroup-writable` is NOT removed here. The restored image still carries the `job.rs` assertion, so removing the class during a revert reproduces the wedge the revert is trying to escape.

KUEUE_ROLLBACK_STEP: 4 — clear stale occupying ledger rows before resuming.
KUEUE_ROLLBACK_CLEAR_STALE_ROWS: any `build_leases` row left in `granted`, `launching`, `bound`, `active` or `suspect` is capacity the restored ledger believes is taken by a Pod that no longer exists. Drive each to `terminal` explicitly, and record which rows were cleared. Skipping this resumes a ledger that admits against phantom occupancy and silently halves the fleet.

KUEUE_ROLLBACK_STEP: 5 — resume dispatch and collect the same resume evidence as the cutover: an actually dispatched task-run, observed by the operator.

## What a passing contract here does and does not mean

`deploy/runbooks/tests/kueue-cutover-quiesce-rollback.sh` reads this markdown and asserts that the laws are present and the steps are ordered. It observes no cluster. `deploy/kueue/tests/preflight.sh` runs the preflight against fake `kubectl` and `psql` binaries and asserts every pass and fail branch, including that each negative fixture trips the specific gate it names. Neither proves a cutover is safe on a particular day; together they prove the operator cannot start or revert one on memory alone.
