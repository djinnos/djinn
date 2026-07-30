# Kueue prerequisite release: zero-capture gate

This runbook governs the prerequisite Kueue release only. It is deliberately
before, and is a mandatory gate for, the later Kueue cutover epic **4c9q**.
It does not label `djinn`, change task-run or warm Job rendering, or activate
Kueue admission for build Jobs.

## Two different executions

### Repository contract execution (no cluster)

Run the deterministic fake-`kubectl` contract from a repository checkout:

```sh
deploy/kueue/tests/zero-capture-gate.sh
```

This checks the harness protocol and its failure handling using captured fake
API responses. A pass proves only the repository contract; it is not evidence
that a release cluster was contacted, that Kueue was installed, or that any Pod
ran on an operator cluster.

### Required target-cluster prerequisite invocation

Before beginning any 4c9q Kueue cutover action, an operator must select a
**disposable target-cluster context**, create or identify the caller-owned
designated-operator Secret required by a fresh Djinn chart install, and run:

```sh
deploy/kueue/zero-capture-gate.sh \
  --context <disposable-target-context> \
  --designated-operator-secret <caller-owned-designated-operator-secret>
```

`<caller-owned-designated-operator-secret>` is the name (not the contents) of
the Secret that supplies the chart's required
`migration.designatedOperatorSecret` on a fresh install. Provision it in the
target `djinn` namespace using the chart's normal bootstrap procedure before
running the gate. The harness passes only this name to Helm with
`--set-string`; it does not create, print, or place Secret contents in shell
arguments. The automation equivalent is
`KUEUE_GATE_DESIGNATED_OPERATOR_SECRET=<secret-name>`.

The harness installs, in this order, the pinned Kueue prerequisite chart
`deploy/helm/djinn-prereqs` (release `djinn-prereqs`, namespace `kueue-system`)
and the inert `deploy/helm/djinn` chart with `kueue.enabled=true`. It then
verifies that namespace `djinn` does **not** have
`djinn.io/kueue-managed=true`, applies the unchanged
`deploy/kueue/tests/fixtures/precutover-task-run.yaml`, waits with a bounded
timeout for that fixture's Pod to become `Running`, and requires
`kubectl get workloads -n djinn` to return zero items.

The prerequisite arrives as a **pinned upstream chart**, not a byte-vendored
manifest: `deploy/kueue/vendor/kueue-v0.10.0.yaml` was retired, and the gate
now fails if any static Kueue manifest is applied instead. The target cluster
must be **Kubernetes >= 1.29** (Kueue 0.19); the upstream chart declares no
`kubeVersion`, so nothing enforces this for you.

Note the reduced fence, in full, before recording a pass: the per-object
`djinn.io/kueue-build-object` selector no longer exists, because the upstream
chart exposes no hook for it. `mjob`/`vjob` are namespace-fenced only. A pass
here proves zero capture with **no namespace labelled** — it does not
generalise to a labelled namespace. See `deploy/kueue/README.md`.

A nonzero exit is a failed prerequisite. Preserve its diagnostic output and do
not label the namespace or start 4c9q cutover work. A passing target-cluster
invocation is mandatory release-before-cutover evidence; record the context,
release timestamp, command output, and chart revision in the release record.

The repository has not performed an operator target-cluster invocation and
makes no claim that one occurred.

## Failure handling

The gate emits namespace, fixture Job/Pod, and Workload diagnostics for an
inertness, Pod-running, or capture failure. Resolve the prerequisite-release
configuration while keeping `djinn.io/kueue-managed` absent, then use a new
disposable context for a fresh gate invocation. Do not treat a repository
contract pass as a substitute for this target-cluster gate.
