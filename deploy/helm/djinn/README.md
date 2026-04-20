# djinn Helm chart

Installs djinn-server (controller), dolt (SQL state), qdrant (vector store),
and the Phase 3 image pipeline (BuildKit + Zot + image controller).

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
