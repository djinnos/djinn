# Shared-volume ownership contract — runbook

The `mirrors`, `cache`, and `projects` volumes are written by **three
identities**:

| Identity | Who | Where |
|----------|-----|-------|
| uid `10001` | `djinn-server` | `/var/lib/djinn/{mirrors,cache,projects}` |
| uid/gid `1000` | task-run + warm Job worker | `/workspace`, `/cache`, `/mirror` |
| uid `1001`, primary group `1000` | the launcher-spawned child (compiles, tests) | `/workspace`, `/cache` |

They can only share those volumes under one contract:

* **group ownership = `1000`** (`ARTIFACT_GID`, `djinn_cgroup_launcher::child`)
  on every directory and file;
* **group-write (`g+w`)** on every directory and file — so any of the three can
  create, overwrite, and unlink what another produced;
* **setgid (`g+s`) on every directory** — so files created there *inherit* the
  artifact group instead of the creating process's primary group.

Violating it does not produce a crash. It produces a **silent freeze**: the warm
Job's cargo phase is best-effort (lock-unavailable, step failure and timeout only
`WARN`) while the Job reports success from the graph phase, so a cache the warm
pod cannot write shows up as *green warm Jobs over a permanently frozen build
base*. This happened while preparing the v0.7.0 deploy — 13,512 files owned by
`10001` with dirs `755` / files `644` — and was caught by hand.

## What enforces it now

| Layer | Mechanism |
|-------|-----------|
| Fresh install | The chart's `fix-volume-perms` initContainer normalizes the three volume **roots** to `ownerUid:artifactGid` with `chmod 2775` (setgid + `g+w`). O(1), idempotent, **never recursive**. |
| Server access | The server pod gets `securityContext.supplementalGroups: [1000]` — membership in the artifact group with zero filesystem work. |
| Worker/warm pods | The Pod render pins `fsGroup: 1000` + `fsGroupChangePolicy: OnRootMismatch` and runs as uid/gid `1000` (task-run **and** warm — they share `/cache`). |
| Runtime | `djinn-agent-worker` validates the **actually mounted** roots at startup (`volume_contract`) and fails readiness with a typed error naming the path and observed-versus-required ownership/mode. |

The startup check stats the workspace, the git metadata (`<workspace>/.git` and
the mirror root), and every configured cache root — `/cache`,
`/cache/cargo-target`, `/cache/cargo-target-runs`, `CARGO_HOME`, `SCCACHE_DIR`,
`CARGO_TARGET_DIR`, plus the warm Job's own Cargo warm-base variant. It also
asserts the process is a member of gid `1000`; a conforming volume is useless to
a pod that is not in the group.

It is **bounded**: depth 3, 32 entries per directory, 512 `lstat`s total. It
checks a *sample of the subtree*, not just the root, because the exact
production near-miss had a hand-fixed root over a broken subtree. It **never
repairs** — a recursive `chown` over a 300G cache is a multi-minute stall on pod
start, which is the whole reason `fsGroupChangePolicy` is `OnRootMismatch`.

## Symptom → diagnosis

A pod that fails this check logs one line and exits:

```
volume permission contract VIOLATED  kind=group_write path=/cache/cargo-target/<project>/mold-jobs-8 ...
```

```bash
kubectl -n djinn logs job/<warm-job> | grep 'volume permission contract'
kubectl -n djinn get pods --field-selector=status.phase=Failed
```

Confirm from the volume side (`stat` on the offending path):

```bash
kubectl -n djinn exec deploy/djinn -- \
  stat -c '%n uid=%u gid=%g mode=%a' \
  /var/lib/djinn/cache /var/lib/djinn/cache/cargo-target
```

Conforming output is `gid=1000` and a mode beginning with `2` (setgid) and
carrying group-write: `2775` for directories, `664` for files.

## One-time remediation for pre-existing volumes

Volumes provisioned before this contract (or restored from a backup taken before
it) carry the old ownership throughout their **contents**. The chart only
normalizes the roots, so those need a single operator pass. Run it once, with the
server scaled down and no task-run or warm Jobs in flight:

```bash
NS=djinn

# 1. Quiesce: stop new Jobs and drain the ones running.
kubectl -n "$NS" scale deploy/djinn --replicas=0
kubectl -n "$NS" delete jobs -l app.kubernetes.io/component=graph-warm --ignore-not-found
kubectl -n "$NS" delete jobs -l app.kubernetes.io/component=task-run  --ignore-not-found

# 2. One-shot root pod that mounts the same claims and normalizes them.
kubectl -n "$NS" run volume-ownership-fix --rm -i --restart=Never \
  --image=busybox:1.36 \
  --overrides='{"spec":{"securityContext":{"runAsUser":0},"containers":[{"name":"fix","image":"busybox:1.36","command":["sh","-c","set -eu; for d in /mnt/mirrors /mnt/cache /mnt/projects; do chgrp -R 1000 \"$d\"; chmod -R g+w \"$d\"; find \"$d\" -type d -exec chmod g+s {} +; done; echo done"],"volumeMounts":[{"name":"mirrors","mountPath":"/mnt/mirrors"},{"name":"cache","mountPath":"/mnt/cache"},{"name":"projects","mountPath":"/mnt/projects"}]}],"volumes":[{"name":"mirrors","persistentVolumeClaim":{"claimName":"djinn-mirrors"}},{"name":"cache","persistentVolumeClaim":{"claimName":"djinn-cache"}},{"name":"projects","persistentVolumeClaim":{"claimName":"djinn-projects"}}]}}'

# 3. Bring the server back; the startup check now passes.
kubectl -n "$NS" scale deploy/djinn --replicas=1
```

Substitute the real claim names (`kubectl -n "$NS" get pvc`) — they differ when
`storage.*.existingClaim` is set. On a large cache this takes minutes; that is
exactly why it is an operator action and not something a pod does at startup.

Cheaper alternative when only the cache is broken: delete the Cargo warm base
(`/cache/cargo-target`, `/cache/cargo-target-runs`) and let the next warm Job
rebuild it under the correct ownership. It costs one cold warm cycle and no
`chown` walk.

### Break-glass

`DJINN_VOLUME_CONTRACT_MODE=audit` in the worker's environment downgrades the
failure to a loud `ERROR` log and lets the pod proceed over a known-broken
volume. It is a rollout-window escape hatch, **not** a fallback — nothing
reverts to the legacy uid, and the underlying breakage is still there. Outside
Kubernetes (no `KUBERNETES_SERVICE_HOST`) the check is `off`: there are no
pod-mounted volumes to validate.

## Why an explicit initContainer instead of relying on `fsGroup`

`fsGroup` + `fsGroupChangePolicy: OnRootMismatch` looks like it already
guarantees this. It does not, for four reasons:

1. **The trigger is a heuristic on the volume root.** The kubelet skips the
   recursive pass when the root's gid already matches the fsGroup *and* the root
   carries the expected setgid/permission bits. A root that was fixed by hand —
   precisely what happened here — makes the kubelet skip while the entire
   subtree stays wrong. The failure mode is exactly the silent one.
2. **When it does trigger, it is unbounded.** A root mismatch on a 300G cache
   means a full recursive `chown` at mount time, before any container starts:
   minutes of stalled pod start on every affected volume.
3. **It does not apply to every volume type.** `fsGroup` is a no-op for
   `hostPath` (what a single-node k3s install uses) and its behaviour varies
   across CSI drivers and `RunAsAny`-style policies.
4. **It couples membership to filesystem work.** All the server pod needs is to
   be *in* the group; `supplementalGroups` grants that with no ownership pass at
   all.

So the chart uses the explicit path: a root initContainer that normalizes only
the three volume **roots** (deterministic, O(1), driver-independent), plus
setgid so everything created below them inherits the group, plus
`supplementalGroups` for the server. `fsGroup: 1000` stays on the task-run and
warm Pod renders — with the roots already conforming, `OnRootMismatch` is a
guaranteed no-op there rather than a surprise recursive pass — and it is never
set to `Always`. The startup validator is the backstop that makes any remaining
gap loud instead of silent.

## Related

- `server/crates/djinn-agent-worker/src/volume_contract.rs` — the contract and
  the bounded check.
- `server/crates/djinn-agent-worker/tests/volume_contract.rs` — deterministic
  proof that each violation fails readiness and a conforming layout passes.
- `deploy/helm/djinn/tests/volume-ownership-render.sh` — the render contract.
- `server/crates/djinn-k8s/src/launcher.rs` — `pod_security_context()` and the
  worker/child/launcher uid contract.
