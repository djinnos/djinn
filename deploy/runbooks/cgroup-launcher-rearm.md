# cgroup launcher re-arm runbook

## Background

Proposal d308 replaced the cgroup launcher's self-mount with a
kubelet-delegated cgroup delivered through `RuntimeClass/djinn-cgroup-writable`
(handler `runc-cgroupwritable`). Since v0.7.25, arming
`cgroupLauncher.mode: required` while `cgroupWritable.taskRuns.enabled: false`
is refused at dispatch. On 2026-07-29 that combination was live in production
and caused a total production outage: every task-run dispatch was refused
because the launcher demanded the delegated cgroup but no task-run was
permitted to receive it.

This runbook re-arms `cgroupLauncher.mode: required` safely, on the single
production node, without repeating that outage.

`CGROUP_REARM_RUNBOOK: cgroup-launcher-rearm`

## Outage declaration

`CGROUP_REARM_OUTAGE_DECLARATION: systemctl restart k3s bounces the entire single-node production cluster.`

This is a single-node production cluster. There is no second control plane
and no second node to fail over to. Restarting the `k3s` service stops the
API server, the scheduler, the kubelet, and every running Pod's supervising
kubelet-managed process tree on that one node — including all in-flight
task-run Pods and every platform service Pod (server, coordinator, admission,
ingress). Treat the restart in Step 1 as a full-cluster outage window, not a
node-local blip.

## Required dispatch pause and drain

`CGROUP_REARM_DRAIN_REQUIRED: dispatch must be paused and in-flight task-runs drained before systemctl restart k3s`

Because the Step 1 restart bounces the entire cluster (see the outage
declaration above), dispatch of new task-runs MUST be paused and all
in-flight task-runs MUST be drained (allowed to finish or be safely
terminated) before that restart executes. Do not restart `k3s` while
dispatch is active or while task-run Pods are still running.

```bash
# Required before Step 1's systemctl restart k3s:
djinn dispatch pause --reason "cgroup-launcher-rearm: node restart for conformance"
djinn dispatch pause-status   # confirm paused
# Wait for in-flight task-run Pods to drain, e.g.:
kubectl get pods -n djinn-taskruns -o wide --watch
```

Do not proceed to Step 1's restart until the pause is confirmed and the
task-run Pod list is empty.

## Step order

The re-arm proceeds through a mandatory preparation upgrade followed by six
numbered steps, strictly in this order. Do not reorder these steps.

### Preparation: install the RuntimeClass with the launcher still disarmed

`CGROUP_REARM_STEP: 0 (preparation) — install RuntimeClass, launcher disarmed`

The Step 1 conformance probe Pod sets
`runtimeClassName: djinn-cgroup-writable-probe` on itself. Kubernetes rejects
a Pod that references a `RuntimeClass` object that does not yet exist.
Therefore the `RuntimeClass` objects MUST be installed **before** conformance
runs, via a preparation Helm upgrade that leaves the launcher disarmed and
leaves task-runs off the delegated cgroup path:

```bash
helm upgrade djinn deploy/helm/djinn \
  --namespace djinn \
  --reuse-values \
  --set cgroupWritable.runtimeClass.enabled=true \
  --set cgroupWritable.taskRuns.enabled=false \
  --set cgroupLauncher.mode=disabled \
  --set imagePipeline.controller.launcherAuthorityProtocol=leaf-v1
```

The fourth flag is not optional. `imagePipeline.controller.launcherAuthorityProtocol`
now defaults to `resize-v2`, and `templates/deployment-server.yaml` refuses any
render that claims resize-v2 authority while `cgroupLauncher.mode` is not
`required` — resize-v2 declares that Kubernetes in-place Pod resize owns CPU
quota, and disarming the launcher renders no quota mechanism to own it. Without
this flag the preparation upgrade fails at render time and the whole runbook
stalls at step 0. Step 4 restores it by dropping the override.

After this preparation upgrade both classes exist in the cluster, but no
task-run is scheduled onto either (`cgroupWritable.taskRuns.enabled: false`)
and the launcher is not demanding one (`cgroupLauncher.mode` is not
`required`). This is the only state in which it is safe to run the Step 1
conformance probe.

The single value renders a pair, and the pair is not redundant:

* `djinn-cgroup-writable` is the **task-run** class. It carries
  `scheduling.nodeSelector: {djinn.io/cgroup-writable: "true"}`, which the
  RuntimeClass admission controller merges into every Pod that names it. That
  selector is what keeps ordinary task-run Pods off unconformed nodes.
* `djinn-cgroup-writable-probe` is the **conformance-probe** class. Same
  containerd handler, no `scheduling` block at all. Conformance runs against a
  node that does *not* yet carry the eligibility marker — earning it is the
  point — so a probe naming the task-run class would be rejected by the
  kubelet with `Predicate NodeAffinity failed`. `spec.nodeName` bypasses the
  scheduler, not the kubelet's own admission predicate. Before the split, that
  was a deadlock: the marker gated the probe that earns the marker, and
  conformance could never pass.

```bash
kubectl get runtimeclass djinn-cgroup-writable djinn-cgroup-writable-probe
```

Do not proceed to Step 1 until this command shows both `RuntimeClass` objects.

### Step 1: conformance install and node restart

`CGROUP_REARM_STEP: 1 — conformance install + systemctl restart k3s`

Confirm the dispatch pause and drain (above) are complete. Then install the
conformance probe and restart `k3s` on the node:

```bash
kubectl apply -f deploy/node/cgroup-writable-conformance-probe.yaml
systemctl restart k3s
```

The conformance probe Pod references
`runtimeClassName: djinn-cgroup-writable-probe`, which now exists because of
the preparation upgrade above.

After the restart, wait for the probe to complete and inspect its logs for
the explicit PASS line:

```bash
kubectl wait --for=condition=Ready pod/cgroup-writable-conformance-probe --timeout=300s
kubectl logs pod/cgroup-writable-conformance-probe | grep -F 'CONFORMANCE: PASS'
```

`CGROUP_REARM_CONFORMANCE_PASS_MARKER: CONFORMANCE: PASS`

Do not proceed past this step without that literal PASS line. If it is
absent, or a FAIL line appears instead, stop here and follow the rollback
branch below — the node is not eligible and arming must not continue.

### Step 2: eligibility marker applied by the conformance script

`CGROUP_REARM_STEP: 2 — djinn.io/cgroup-writable=true applied by the conformance script`

`deploy/node/k3s/djinn-cgroup-writable-conformance.sh` is the sole owner of
the `djinn.io/cgroup-writable` eligibility marker on the node. It sets
`djinn.io/cgroup-writable=true` on the node itself, as the last action inside
the script, and only after the conformance PASS line is printed. If
conformance fails instead, the script's own `EXIT` trap clears the marker
before the script exits.

**An operator must never set or clear this marker by hand**, on this node or
any other — that ownership belongs exclusively to the conformance script.
Do not run any command that mutates it outside that script.

Confirm the marker was set by the script — this reads the value, it does not
mutate it:

```bash
kubectl get node <node-name> -o jsonpath='{.metadata.labels.djinn\.io/cgroup-writable}'
# expect: true
```

### Step 3: enable the RuntimeClass toggle for the record

`CGROUP_REARM_STEP: 3 — cgroupWritable.runtimeClass.enabled: true`

```bash
helm upgrade djinn deploy/helm/djinn \
  --namespace djinn \
  --reuse-values \
  --set cgroupWritable.runtimeClass.enabled=true
```

This was already set to `true` by the preparation upgrade and remains `true`
here; this upgrade is idempotent and exists to make the value an explicit,
reviewed part of the release history at the point of re-arm, not only of the
preparation step.

### Step 4: enable task-runs on the delegated cgroup

`CGROUP_REARM_STEP: 4 — cgroupWritable.taskRuns.enabled: true`

```bash
helm upgrade djinn deploy/helm/djinn \
  --namespace djinn \
  --reuse-values \
  --set cgroupWritable.taskRuns.enabled=true
```

Task-run Pods scheduled after this upgrade receive
`runtimeClassName: djinn-cgroup-writable`. The launcher is still not
`required` yet, so a task-run that (for any reason) does not land on the
delegated cgroup is not refused at dispatch.

### Step 5: arm the launcher

`CGROUP_REARM_STEP: 5 — cgroupLauncher.mode: required`

```bash
helm upgrade djinn deploy/helm/djinn \
  --namespace djinn \
  --reuse-values \
  --set cgroupLauncher.mode=required
```

This is the step that reproduced the 2026-07-29 outage when it was applied
while `cgroupWritable.taskRuns.enabled` was `false`. Do not run this step
out of order relative to Step 4.

Resume dispatch only after Step 6's kernel-evidence rules (1 through 4) hold.
Step 6's rule 5 then observes live dispatch for an hour after this resume, and
rule 6 governs abort and rollback throughout:

```bash
djinn dispatch resume
```

### Step 6: verification

`CGROUP_REARM_STEP: 6 — verification`

`CGROUP_REARM_VERIFICATION_REQUIRES_KERNEL_EVIDENCE: a status field or cache-hit flag is not acceptable evidence`

`CGROUP_REARM_VERIFICATION_EVIDENCE_IS_OPERATOR_COLLECTED: the observations below are post-merge operator rollout evidence, collected on the production node by the operator performing the re-arm; no repository check and no CI job observes production state`

Verification MUST use direct kernel evidence from a live, Running task-run
Pod, not a status field, not a Helm/Kubernetes condition, and not a cached
"warmed" or "enabled" flag. Reading `cat cpu.max` by itself is also NOT
acceptable evidence, because it only shows the configured ceiling, not that
the kernel is enforcing and accounting against it.

The repository check `deploy/runbooks/tests/cgroup-launcher-rearm.sh` asserts
only that this document contains the six evidence rules below and orders
them as written. It reads this file and nothing else: it never contacts a
cluster, never reads a cgroup, and never observes production. A green check
means the rollout evidence contract is present and correctly ordered — never
that the evidence was collected. Collecting it is the operator's obligation
during the rollout, and the change record is where it is attested.

Required evidence, in this order:

1. A Running task-run Pod whose `spec.runtimeClassName` is
   `djinn-cgroup-writable`:

   ```bash
   kubectl get pod <taskrun-pod> -n djinn-taskruns -o jsonpath='{.status.phase} {.spec.runtimeClassName}{"\n"}'
   # expect: Running djinn-cgroup-writable
   ```

2. Exact `cpu.max` expectations, at birth and after the fenced lift.

   `CGROUP_REARM_VERIFY_ORDER_1_CPU_MAX: launcher leaf born at 25000 100000; after a fenced lift exactly 400000 100000`

   At birth, the launcher's cgroup leaf reports `cpu.max` of exactly
   `25000 100000`, and `cpu.stat`'s `nr_throttled` is accumulating (not
   frozen) under load:

   ```bash
   cat /sys/fs/cgroup/<launcher-leaf>/cpu.max
   # expect: 25000 100000
   cat /sys/fs/cgroup/<launcher-leaf>/cpu.stat
   # record nr_periods, nr_throttled, throttled_usec
   ```

   After the fenced lift, `cpu.max` reads exactly `400000 100000` — that
   value, not merely "some value higher than birth":

   ```bash
   cat /sys/fs/cgroup/<launcher-leaf>/cpu.max
   # expect: 400000 100000
   ```

3. Wrong-fence invariant: a lift presented with the wrong fence token is
   rejected, and the leaf is left clamped.

   `CGROUP_REARM_VERIFY_ORDER_2_WRONG_FENCE: a lift presented with the wrong fence token is rejected and leaves cpu.max clamped at 25000 100000`

   This is the non-vacuity check for the whole step. Without it, a launcher
   leaf that simply never lifts at all looks identical to a working clamp —
   both read `25000 100000`, and rule 2's birth reading alone cannot tell
   them apart. Present a lift carrying a fence token that does not match the
   leaf's current fence, confirm the lift is refused, and confirm the leaf is
   still clamped afterwards:

   ```bash
   # Present a lift whose fence token deliberately does not match the leaf's
   # current fence; the launcher must refuse it.
   cat /sys/fs/cgroup/<launcher-leaf>/cpu.max
   # expect (still clamped, refusal did not widen the leaf): 25000 100000
   ```

   A wrong-fence lift that is accepted, or that moves `cpu.max` off
   `25000 100000`, is a FAIL: abort and follow rule 6 below.

4. 100-period sampling rule, read from `cpu.stat` over a wall-clock window.

   `CGROUP_REARM_VERIFY_ORDER_3_HUNDRED_PERIODS: nr_throttled and throttled_usec unchanged across at least 100 further nr_periods, read from cpu.stat over a wall-clock window`

   Over a wall-clock window of at least 100 further `nr_periods` after the
   lift, `cpu.stat`'s `nr_throttled` and `throttled_usec` are UNCHANGED from
   their values immediately after the lift — i.e. read `cpu.stat` at the
   lift, wait, and read it again; `nr_periods` must have advanced by at
   least 100 while `nr_throttled` and `throttled_usec` stay flat:

   ```bash
   cat /sys/fs/cgroup/<launcher-leaf>/cpu.stat > /tmp/cpu-stat-at-lift
   sleep <window covering >=100 more periods>
   cat /sys/fs/cgroup/<launcher-leaf>/cpu.stat > /tmp/cpu-stat-after-window
   diff /tmp/cpu-stat-at-lift /tmp/cpu-stat-after-window
   # nr_periods must differ by >= 100; nr_throttled and throttled_usec must be identical
   ```

   `cat cpu.max` alone never satisfies this step. Measuring `cpu.stat` across
   a wall-clock window is the only acceptable evidence that the fenced lift
   is real and that the workload is no longer being throttled. A window that
   advanced fewer than 100 `nr_periods` is not a shorter version of this
   rule; it is no evidence at all.

5. One-hour observation rule: board dispatches remain greater than zero for
   the hour following the arm.

   `CGROUP_REARM_VERIFY_ORDER_4_ONE_HOUR_DISPATCH: board dispatches remain greater than zero for the hour following the arm`

   The 2026-07-29 outage was a dispatch refusal, not a kernel fault — kernel
   readings from one Pod cannot detect it. For at least one hour after
   dispatch is resumed, confirm the board keeps dispatching: task-runs
   continue to be admitted and started, and the number of dispatches observed
   across that hour is greater than zero.

   ```bash
   djinn dispatch pause-status   # expect: not paused
   # Sample across the full hour; dispatches over the window must be > 0.
   kubectl get pods -n djinn-taskruns --watch
   ```

   Zero dispatches across that hour is a FAIL even if every kernel reading
   above was perfect — that is precisely the shape the outage took.

6. Abort and rollback behavior when any rule above fails.

   `CGROUP_REARM_VERIFY_ORDER_5_ABORT_ROLLBACK: if any rule above fails, roll the launcher back to cgroupLauncher.mode: disabled and do not leave production armed on unproven delegation`

   If any of rules 2 through 5 fails — a wrong `cpu.max` at birth or after
   the lift, a wrong-fence lift that is not rejected, `nr_throttled` or
   `throttled_usec` moving across the 100-period window, or zero board
   dispatches across the hour — abort the re-arm and roll the launcher back:

   ```bash
   helm upgrade djinn deploy/helm/djinn \
     --namespace djinn \
     --reuse-values \
     --set cgroupLauncher.mode=disabled
   ```

   Production must never be left armed on unproven delegation. A partially
   collected evidence set is a failure, not a pass with a caveat: roll back,
   record which rule failed, and do not re-arm until that rule can be
   satisfied.

Rules 1 through 4 are collected while dispatch is still paused. Dispatch is
resumed (see Step 5) only once all four hold; rule 5's one-hour observation
then runs against live dispatch, and rule 6 applies throughout. The re-arm is
declared complete only after all six evidence items are collected and
attached to the change record.

## Rollback / recovery branch

`CGROUP_REARM_ROLLBACK: restore the prior containerd template byte-for-byte and restart k3s again`

If conformance fails (Step 1) — the PASS line is absent, or a FAIL line
appears — after the `systemctl restart k3s` has already occurred, the node's
containerd configuration template has already been overwritten and the
cluster is already in the outage window from that restart. Do not claim the
cluster "returns to the pre-existing config" on the strength of the
eligibility marker alone: the conformance script's own `EXIT` trap already
clears that marker on this failure path (Step 2 never runs without a PASS),
but the containerd template on disk is still the new one. A node with a
cleared eligibility marker and the new containerd template still in place is
not a recovered node.

Recovery requires an explicit restore of the prior containerd template — the
eligibility marker is not evidence of that restore, and an operator must
never set or clear it directly:

```bash
# 1. Restore the prior containerd config template byte-for-byte from the
#    retained pre-change backup taken before Step 1:
cp /var/lib/rancher/k3s/agent/etc/containerd/config.toml.tmpl.pre-rearm-backup \
   /var/lib/rancher/k3s/agent/etc/containerd/config.toml.tmpl

# 2. Confirm the restored file is byte-identical to the retained backup:
cmp /var/lib/rancher/k3s/agent/etc/containerd/config.toml.tmpl.pre-rearm-backup \
    /var/lib/rancher/k3s/agent/etc/containerd/config.toml.tmpl

# 3. Restart k3s again to load the restored template. This is a second
#    full-cluster bounce; dispatch must remain paused across it.
systemctl restart k3s

# 4. Do not set or clear djinn.io/cgroup-writable by hand. The conformance
#    script owns that marker exclusively (see its own EXIT trap in
#    deploy/node/k3s/djinn-cgroup-writable-conformance.sh) and already
#    cleared it on this failure path; an operator must never mutate it.
```

Only after `cmp` in step 2 confirms a byte-for-byte match, and the second
`systemctl restart k3s` has completed, is the node's containerd
configuration actually restored. Keep dispatch paused until this restore is
confirmed; do not resume dispatch on a node in an unknown containerd state.
Do not retry Step 1 conformance until the restore is confirmed complete.

## Summary of ordering

1. Dispatch paused and drained (required before any restart).
2. Preparation Helm upgrade: `cgroupWritable.runtimeClass.enabled=true`,
   `cgroupWritable.taskRuns.enabled=false`, launcher disarmed.
3. Step 1: conformance install, `systemctl restart k3s`, PASS line required.
4. Step 2: conformance script applies the eligibility marker — only after PASS,
   and only the script ever sets or clears it.
5. Step 3: `cgroupWritable.runtimeClass.enabled: true` (confirmed).
6. Step 4: `cgroupWritable.taskRuns.enabled: true`.
7. Step 5: `cgroupLauncher.mode: required`.
8. Step 6: verification, whose six evidence rules are ordered as written —
   the Running task-run Pod, the exact `cpu.max` values at birth and after
   the fenced lift, the wrong-fence invariant, the 100-period `cpu.stat`
   sampling window, the one-hour dispatch observation after resume, and the
   abort/rollback behavior that returns the launcher to disabled.

If Step 1 fails: restore the prior containerd template byte-for-byte (with
`cmp` confirmation) and restart `k3s` again before any retry.

All of Step 6's observations are operator rollout evidence collected after
this document merges. The repository check asserts the presence and order of
the rules only; it makes no claim about production state.
