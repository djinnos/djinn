# djinn Helm chart

Installs djinn-server (controller), Postgres 16 (SQL state, JSONB), qdrant
(vector store), and the Phase 3 image pipeline (BuildKit + Zot + image
controller).

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
