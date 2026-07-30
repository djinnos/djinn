# Kueue prerequisite release: zero-capture gate

This runbook governs the prerequisite Kueue release only. It is deliberately
before, and is a mandatory gate for, the later Kueue cutover epic **4c9q**.
It does not label `djinn`, change task-run or warm Job rendering, or activate
Kueue admission for build Jobs.

## Two different executions

### Repository contract execution (no cluster)

Run the deterministic stubbed-`kubectl`/`helm` contract from a repository
checkout:

```sh
deploy/kueue/tests/zero-capture-gate.sh
```

This checks the harness protocol and its failure handling using captured fake
API responses. It ends by printing an explicit `UNVERIFIED:` declaration,
because both `helm` and `kubectl` are stubs in it: nothing is installed and no
cluster is contacted. A pass proves only argument construction, argument
ordering and failure handling; it is not evidence that a release cluster was
contacted, that Kueue was installed, or that any Pod ran on an operator
cluster. Never quote it as prerequisite evidence.

### Required target-cluster prerequisite invocation

Before beginning any 4c9q Kueue cutover action, an operator must select a
**disposable target-cluster context** and run the three steps below in order.
The context must be disposable: step 1 stamps Helm ownership metadata on the
`djinn` namespace and step 3 installs a whole Djinn release into it.

**Step 1 — create the target namespace.**

```sh
kubectl --context <disposable-target-context> create namespace djinn
```

The chart renders the `djinn` Namespace itself (`namespace.create` defaults
true) and that rendered object is what the inertness assertion reads, so the
gate must not be run with `namespace.create=false`. But the Secret in step 2
has to exist *inside* that namespace before the install, because the server
Pod's bootstrap initContainer resolves it through `secretKeyRef` and never
starts otherwise. Helm normally refuses to adopt a namespace it did not create
(`exists and cannot be imported into the current release: invalid ownership
metadata`); the gate resolves that by stamping
`app.kubernetes.io/managed-by=Helm` plus the `meta.helm.sh/release-*`
annotations onto the namespace before installing. Running the gate against a
cluster with no `djinn` namespace also works — the gate creates it — but then
step 2 has nowhere to put the Secret, so do step 1 explicitly.

**Step 2 — create the caller-owned designated-operator Secret.**

```sh
kubectl --context <disposable-target-context> -n djinn \
  create secret generic <caller-owned-designated-operator-secret> \
  --from-literal=user_id=<uuid> \
  --from-literal=github_id=<numeric-github-id> \
  --from-literal=github_login=<login>
```

This is the Secret that supplies the chart's required
`migration.designatedOperatorSecret` on a fresh install. The gate takes only
its **name**, verifies the Secret exists before starting a long install wait,
and passes the name to Helm with `--set-string`; it never creates, reads or
prints the contents, and never places them in shell arguments. On a disposable
gate cluster the three values identify nothing real and placeholders are fine;
on a cluster you intend to keep, use the release's real operator identity.

**Step 3 — run the gate.**

```sh
deploy/kueue/zero-capture-gate.sh \
  --context <disposable-target-context> \
  --designated-operator-secret <caller-owned-designated-operator-secret> \
  --values deploy/kueue/tests/fixtures/single-node-values.yaml \
  --set-string image.server=ghcr.io/djinnos/djinn-server:<release-under-test> \
  --set-string image.runtime=ghcr.io/djinnos/djinn-agent-runtime:<release-under-test>
```

The automation equivalent of the Secret flag is
`KUEUE_GATE_DESIGNATED_OPERATOR_SECRET=<secret-name>`.

`--values`, `--set` and `--set-string` are forwarded verbatim to the Djinn
chart install, and supplying release values is **required**, not optional: the
chart's committed defaults are a multi-node production shape that no disposable
cluster satisfies and that production does not run either (`docs/deploy/vps.md`
overrides the same three access modes).

* **Storage.** `storage.{mirrors,cache,projects}.accessMode` defaults to
  `ReadWriteMany`. No single-node default StorageClass provisions RWX — kind's
  and k3s's `local-path` both leave the claims `Pending` and the install dies
  with `PVC is not Bound. phase: Pending` then `context deadline exceeded`.
  `deploy/kueue/tests/fixtures/single-node-values.yaml` overrides all three to
  `ReadWriteOnce`, which is correct on one node because RWO is enforced
  per-node, not per-pod.
* **Images.** `image.server` defaults to the unqualified `djinn-server:latest`,
  which resolves to `docker.io/library/djinn-server:latest` and fails with
  `pull access denied`. Name the release under test explicitly — that tag is
  the thing this gate is evidence *about*, so it belongs on the command line
  and in the release record, not pinned in a repository file.

The gate rejects caller overrides of `kueue.*` and
`migration.designatedOperatorSecret`, and emits its own `--set
kueue.enabled=true` last, so no values file or `--set` can disable the queue
topology and make "zero Workloads captured" vacuously true.

The install wait and the fixture Pod wait have **independent budgets**.
`--install-timeout-seconds` (default 900) bounds each `helm upgrade --install
--wait`; it has to cover image pulls, PVC binding, the designated-operator
bootstrap and the schema migration on a cold cluster. `--timeout-seconds`
(default 120) bounds only the fixture Pod reaching `Running`. Neither needs
overriding on a normal cluster; a pass that required raising them is worth
investigating rather than recording.

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

A harness-validation run of this procedure was executed once against a
disposable kind cluster, to prove the gate can execute at all — before that,
its `helm` install path had never run anywhere and the procedure above was
unsatisfiable on a fresh cluster. That run is evidence about the *harness*, not
about any release: the repository has performed no operator target-cluster
invocation, records none, and makes no claim about any release's prerequisite
evidence.

## Failure handling

The gate emits namespace, fixture Job/Pod, and Workload diagnostics for an
inertness, Pod-running, or capture failure. Resolve the prerequisite-release
configuration while keeping `djinn.io/kueue-managed` absent, then use a new
disposable context for a fresh gate invocation. Do not treat a repository
contract pass as a substitute for this target-cluster gate.
