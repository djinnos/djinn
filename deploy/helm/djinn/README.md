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
