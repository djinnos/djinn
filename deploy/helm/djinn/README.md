# djinn Helm chart

Installs djinn-server (controller), Postgres 16 (SQL state, JSONB), qdrant
(vector store), and the Phase 3 image pipeline (BuildKit + Zot + image
controller).

## Incident observability

The opt-in `monitoring` and `logCollector` chart values are checked locally by
`tests/incident-observability-contract.sh`; its named output is
`helm_contract::incident_observability`. This repository verification renders
the schema and manifests, including the existing collector contract. It does
not prepare node disks, create the webhook Secret, activate an external paging
provider, or prove a real page. Those are operator-owned production gates in
[the incident observability runbook](../../../server/docs/runbooks/incident-observability.md).

## Cache cleanup mode

The shipped production value is `cacheCleanup.mode: delete`, rendered as the
literal `DJINN_CACHE_CLEANUP_MODE` environment variable on `djinn-server`.
For kind/local development, `values.local.yaml` explicitly selects `dry_run`.
An operator can make the same non-destructive override with
`--set-string cacheCleanup.mode=dry_run`. The chart accepts only the exact
`delete` and `dry_run` strings.

This Helm contract does not change direct-binary behavior: an unset or invalid
`DJINN_CACHE_CLEANUP_MODE` remains fail-safe `dry_run` there.

## Graph generation retention rollout

`graphRetention` controls the literal `DJINN_GRAPH_RETENTION_MODE` and
`DJINN_GRAPH_RETENTION_HISTORY_N` environment variables on `djinn-server`.
The production chart defaults to `mode: dry_run` and `historyN: 3`. This starts
the leader-only sweep in observation mode: it reports candidate generations but
does not delete them.

Use the rollout sequence below; no live Kubernetes cluster is required to
validate the chart because `tests/graph-retention-render.sh` renders and checks
each setting locally.

1. Leave `graphRetention.mode=dry_run` in place and observe candidate/skip/retry
   retention metrics and logs for the intended history window.
2. After that observation is acceptable, explicitly enable destructive cleanup
   with `--set-string graphRetention.mode=delete` (and, if needed,
   `--set graphRetention.historyN=<N>`).
3. Roll back immediately to the explicit escape hatch
   `--set-string graphRetention.mode=off` to stop subsequent sweeps. Returning
   to `dry_run` resumes observation without deletion.

The schema accepts only `off`, `dry_run`, and `delete`; `historyN` must be an
integer from 1 through 64. The server keeps the current generation plus the
newest N published generations. Compatibility storage bounds are independent:
each table is bounded to at most N+1 full blobs, while the two compatibility
tables together are bounded to at most 2(N+1) full blobs. Do not treat the
combined bound as a per-table allowance.

## Zot catalog retention rollout

`imagePipeline.zot.retention` bounds the in-cluster Zot registry. Without it a
catalog repo accumulates every manifest ever built — measured on the production
VPS: **117 manifests in one `djinn-image-*` repo, 83 GB, 41 days deep**. Zot's
blob GC (`gc: true`) was already working; it reclaimed 0.0 GiB because nothing
is ever untagged, so *tag* retention is the missing piece. Keeping the newest 5
tags reclaims ~78.5 GiB.

The chart ships `enabled: true` with `dryRun: true`. That pairing is the point:
a cluster **reports** the tags it would prune instead of growing silently, and
still deletes nothing until an operator opts in. `tests/zot-retention-render.sh`
validates every setting locally; no live cluster is required.

Two independent safety properties hold before anything is deleted:

1. **Scope.** The rendered catalog policy targets `repositories:
   ["djinn-image-*"]` only. BuildKit cache repos (`cache/*`) and infra repos are
   out of scope of *this* policy — cache repos get their own, differently
   shaped one (see below). Do not widen this glob.
2. **Digest pins, not tags.** Every task-run and warm Job references its project
   image *by digest*. The authoritative keep-set is therefore the database
   (`ImageRepository::list_selected_catalog_images`), not the tag list. The
   server's startup preflight (`retention_preflight.rs`) independently proves
   each selected catalog image stays pullable by a retained tag or digest pin,
   and is fail-closed: an unsafe image blocks the rollout and is reported.

`DJINN_ZOT_RETENTION_ENABLED` is rendered from the *effective* value — the
`retention.enabled` intent ANDed with `imagePipeline.enabled` and
`imagePipeline.zot.enabled`. The preflight exits the process on a Zot fetch
error, so a deployment on an external registry (the chart default
`zot.enabled: false`) must never be told retention is on; there would be no
in-cluster Zot to answer and the server would crash-loop on boot.

Rollout order:

1. Deploy the shipped default (`enabled: true`, `dryRun: true`). Zot logs the
   manifests it would remove; the server logs one
   `Zot catalog retention preflight report` line per boot.
2. Read that preflight report and confirm `outcome` is not
   `destructive_blocked` and that every selected catalog image is retained.
   This is the gate — do not skip it, because the pinned digest is currently
   the newest manifest and a tag-only view cannot prove it survives.
3. Flip destructive with `--set imagePipeline.zot.retention.dryRun=false`.
   Roll back by setting it to `true` again (or
   `--set imagePipeline.zot.retention.enabled=false` to remove the policy).

`newestTags` is the number of newest tags kept per catalog repo (default 5), and
`deleteUntagged` removes manifests left with no tags after pruning. A destructive
policy cannot render at all unless `imagePipeline.enabled` and
`imagePipeline.zot.enabled` are both true — `zot-configmap.yaml` fails the
render otherwise, rather than producing a policy with no matching preflight.

## BuildKit registry cache retention (`cache/*`)

The catalog policy is scoped to `djinn-image-*`, which is correct — and it left
a second repo family with **no retention, no TTL and no eviction of any kind**,
inside the same PVC. `build_job.rs` exports build cache with:

```
--export-cache type=registry,ref=<registry>/cache/<subject>,mode=max,image-manifest=true,oci-mediatypes=true
--import-cache type=registry,ref=<registry>/cache/<subject>
```

so every project's BuildKit cache is a `cache/<subject>` repo in the same Zot.

**The growth vector is not tag count.** `mode=max` rewrites the *same* `:latest`
ref on every build, so a cache repo has exactly one tag forever and newest-N tag
retention is a no-op against it. What accumulates is the **superseded manifest**:
each export leaves the previous cache manifest in the repo's index, untagged but
still indexed, still referencing its blobs. Because those blobs are still
*reachable*, live blob GC has nothing to collect — which is precisely why
`gc: true` / `gcDelay: 1h` / `gcInterval: 24h` measured **0.0 GiB** reclaimed on
the production VPS while the store kept growing. `gc` is the collector; it was
missing an operator to produce garbage in the first place.

`imagePipeline.zot.retention.cache.enabled` (default `true`) renders that
operator as a second policy:

```json
{
  "repositories": ["cache/*"],
  "deleteUntagged": true,
  "keepTags": [{ "patterns": [".*"] }]
}
```

`keepTags: [{patterns: [".*"]}]` is load-bearing, not decorative. It retains
every tag unconditionally, so the live `:latest` ref that in-flight builds
`--import-cache` is structurally ineligible for deletion; only already-superseded
manifests are candidates. It also makes the policy well-defined regardless of how
Zot treats a policy with an empty `keepTags` list.

### Safety: worst case is a cache miss, never a failed build

* The live ref is never a candidate — see above.
* A superseded manifest becomes a candidate only after the retention `delay`
  (`1h`), and its now-unreachable blobs are only reclaimed after `gcDelay` (`1h`)
  on the next `gcInterval` (`24h`) sweep. A build resolves and fetches its cache
  in seconds. The window is not close.
* Even a total miss is a state this code path already handles in production: the
  **first** build of every project imports a `cache/<subject>` ref that does not
  exist yet, and those builds succeed. `buildctl` treats an unresolvable
  `--import-cache` as "no cache available", not as an error. A pruned ref is
  indistinguishable from a never-created one.

### Dedupe: why no byte figure is promised

`storage.dedupe: true` stores one copy of each blob digest and shares it across
repos. A `mode=max` cache export and a `djinn-image-*` layer frequently *are* the
same blob. So:

* Deleting a cache manifest frees **zero** bytes for every blob a live catalog
  manifest still references, and vice versa.
* The reclaimable set is bounded above by the size of the *cache-only* blobs, not
  by the nominal size of the deleted manifests.
* The two policies are therefore complementary, not additive. Pruning only one
  side leaves shared blobs alive through the other side's references, and the
  catalog policy's own estimate is an upper bound for the same reason.

Report the real number from the dry-run, do not predict it.

### Rollout

The cache policy is governed by the **shared** `retention.dryRun` flag — Zot's
`dryRun` is a single retention-block-level switch and cannot be set per policy.
So it ships report-only alongside the catalog policy and goes destructive at the
same moment, on the same one operator action. To roll the catalog policy
destructive while leaving cache repos untouched, set
`imagePipeline.zot.retention.cache.enabled=false`.

The server's startup preflight (`retention_preflight.rs`) covers `djinn-image-*`
**only**, and needs nothing for `cache/*`: no database row and no kubelet pull
ever references a cache repo. Its only reader is `buildctl --import-cache`, whose
failure mode is the cache miss above. The cache policy's dry-run report comes
from Zot's own logs, not from the server preflight.

`tests/zot-cache-retention-render.sh` pins all of this.

## Build admission mode

`buildAdmission.mode` selects the literal `DJINN_BUILD_ADMISSION_MODE` emitted
on `djinn-server`: `off`, `observe` (the default), or `enforce`.
`buildAdmission.maxBuildTaskRuns` emits the literal
`DJINN_MAX_BUILD_TASKRUNS` cap and defaults to `3`; the chart accepts only
integers from `1` through `64`.

Enforce is a single-active-controller deployment mode. It requires
`server.replicas: 1` and either `server.strategy.type: Recreate`, or a
`RollingUpdate` with exactly `maxSurge: 0` and `maxUnavailable: 1`. Helm rejects
an Enforce release that does not meet this topology. Off and Observe retain the
configured server replica and rollout settings, so their normal default is the
availability-first rolling update (`maxSurge: 1`, `maxUnavailable: 0`).

## Writable cgroup preparation rollout

`cgroupWritable.runtimeClass.enabled` and `cgroupWritable.taskRuns.enabled`
both default to `false`. The preparation release may set only
`cgroupWritable.runtimeClass.enabled=true` after the node conformance process
has labeled eligible nodes. That installs `RuntimeClass/djinn-cgroup-writable`
with its existing handler and node selector, but does not assign it to task-run
Pods or change launcher privileges.

The same value also installs `RuntimeClass/djinn-cgroup-writable-probe`: the
same `runc-cgroupwritable` handler with no `scheduling` block. The two always
appear and disappear together. Node conformance uses the probe class because it
runs on a node that does not yet carry `djinn.io/cgroup-writable=true`; the
task-run class's merged `nodeSelector` becomes a NodeAffinity predicate the
kubelet enforces even for a Pod bound by `spec.nodeName`, so a probe naming it
could never be admitted, and conformance could never pass. Task-run Pods use
`djinn-cgroup-writable` only — the probe class is never assigned to them.

`cgroupWritable.taskRuns.enabled=true` requires
`cgroupWritable.runtimeClass.enabled=true`; Helm rejects the unsafe inverse
pair. Task-run assignment is reserved for the later activation release.

## Node prerequisites

The image pipeline runs BuildKit **rootless** via user namespaces. Every
node that may schedule the `buildkitd` pod must have:

```sh
sysctl -w kernel.unprivileged_userns_clone=1
sysctl -w user.max_user_namespaces=28633   # or higher
```

Persist via `/etc/sysctl.d/99-djinn-buildkit.conf` so the settings survive
reboots. k3s nodes usually ship with both flags already; bare kubeadm / kind
clusters may not.

### kind

kind inherits host sysctls. Apply the two settings on the host before
`kind create cluster`, or bake them into your kind config's
`containerdConfigPatches`.

### Quick check

```sh
kubectl debug node/<node> -it --image=busybox -- sh -c \
  'cat /proc/sys/kernel/unprivileged_userns_clone /proc/sys/user/max_user_namespaces'
```

Both values must be non-zero (`1` and `>=28633` respectively).
