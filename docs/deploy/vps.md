# Single VPS with k3s

One box, everything bundled: Postgres, Qdrant, and an in-cluster
[Zot](https://zotregistry.dev) registry all run inside a single-node
[k3s](https://k3s.io) cluster, with TLS from Let's Encrypt through k3s's
built-in Traefik. This is the cheapest way to run Djinn for a team.

Prefer to delegate? Paste the [AI-assisted install prompt](AGENT.md) into an
agent with SSH access to the box and it will walk these exact steps with you.

## Sizing

Agents run as pods *on the same box*, so concurrency is bounded by the
hardware, not by Djinn:

| | vCPU | RAM | Disk |
|---|------|-----|------|
| Minimum (1 agent at a time) | 4 | 8 GB | 100 GB |
| Comfortable (2–3 concurrent agents) | 8 | 32–48 GB | 250 GB |

Disk is the thing to watch long-term: the Zot registry keeps per-project image
history and the shared cache PVC accumulates cargo/pnpm/pip caches. Use a KVM
VPS (rootless BuildKit needs user namespaces; container-based VPSes like
OpenVZ won't work). Ubuntu 22.04+/Debian 12+ are the tested paths.

## Prerequisites

- Root SSH access to the box.
- **DNS first**: an A record `djinn.example.com → <your-vps-ip>`, in place
  *before* you start — Let's Encrypt HTTP-01 needs it to resolve.
- An email address for Let's Encrypt.

## 1. Install k3s

```bash
curl -sfL https://get.k3s.io | sh -
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
kubectl get nodes    # wait for Ready
```

k3s ships everything we need: Traefik ingress and the `local-path` storage
provisioner (single-node `ReadWriteOnce` PVCs are correct here — RWO is
enforced per-node, not per-pod).

Set the sysctls rootless BuildKit needs (most k3s-capable kernels already have
them, but persist them anyway):

```bash
cat >/etc/sysctl.d/99-djinn.conf <<'EOF'
user.max_user_namespaces=28633
EOF
# On older kernels also add: kernel.unprivileged_userns_clone=1
sysctl --system
```

### Bound the containerd image store (kubelet image GC)

**This repo does not manage kubelet configuration declaratively — there is no
ansible/terraform tree and nothing writes `/etc/rancher/k3s/config.yaml`. The
step below is operator-applied, once, per node.**

Every project-image build pushes a new devcontainer image, and the kubelet pulls
it. Nothing evicts the old ones. On a long-lived node the containerd image store
becomes the single largest consumer on the root filesystem (measured on the
production VPS: **129 GB across 433 image refs, of which only 16 were in use**).

kubelet's default image GC is *disk-level* only: it starts evicting when the
filesystem crosses `imageGCHighThresholdPercent` (default 85%). That threshold
is the reason the node keeps rediscovering DiskPressure — GC does nothing at
all until the disk is already nearly full, then evicts under pressure. The
durable fix is the *age-based* bound `imageMaximumGCAge`, which reclaims unused
images on a timer regardless of how full the disk is.

`imageMaximumGCAge` has **no kubelet command-line flag** — it exists only in the
`KubeletConfiguration` file — so it cannot be passed via a bare `kubelet-arg`.
Point kubelet at a config file instead:

```bash
cat >/etc/rancher/k3s/kubelet.conf <<'EOF'
apiVersion: kubelet.config.k8s.io/v1beta1
kind: KubeletConfiguration
# Reclaim images unused for 7 days, on a timer, independent of disk level.
# Must be strictly greater than the image GC scan period.
imageMaximumGCAge: "168h"
# Disk-level backstop retained at the kubelet defaults.
imageGCHighThresholdPercent: 85
imageGCLowThresholdPercent: 80
EOF

cat >/etc/rancher/k3s/config.yaml <<'EOF'
kubelet-arg:
  - "config=/etc/rancher/k3s/kubelet.conf"
EOF

systemctl restart k3s     # running pods are unaffected
```

Requires Kubernetes ≥ 1.30 (the `ImageMaximumGCAge` feature gate is beta/on by
default there, GA from 1.32). Check with `k3s --version`.

Safety: kubelet only garbage-collects images **not referenced by any container
it currently manages**, so images backing Running/Pending pods — including the
digest-pinned project image every task-run and warm Job references — are never
candidates. Age is measured from last use, not from pull.

Verify after the restart:

```bash
# The setting is live if it appears in the kubelet's effective config.
kubectl get --raw "/api/v1/nodes/$(hostname)/proxy/configz" | grep -o '"imageMaximumGCAge":"[^"]*"'

# Watch the image count fall over the following days.
k3s crictl images | wc -l
```

If `/etc/rancher/k3s/config.yaml` already exists, merge the `kubelet-arg` key
into it rather than overwriting the file.

### PVC sizes are requests, not quotas

Read this before you "fix" a full volume by raising a number.

Measured on this production VPS: the `djinn-zot` and `djinn-cache` PVCs are each
declared **40Gi** and each holds **83G**. Nothing errored, nothing alerted,
nothing was evicted. That is not a bug in the chart's numbers — it is what
`spec.resources.requests.storage` *means* under the k3s `local-path`
provisioner, which mkdirs a directory on the node filesystem and applies no
quota of any kind. The declared capacity is advisory. There is no backstop at
any layer below it.

(Both 40Gi values are install-time overrides. The chart's own defaults are
100Gi for `imagePipeline.zot.storage.size` and 20Gi for `storage.cache.size`.
Raising them would not have changed anything here — see below.)

**Why the chart does not simply ship bigger numbers.** On `local-path` a bigger
number enforces nothing, so it buys nothing. On a StorageClass that *does*
enforce (EBS, PD, Ceph RBD, Longhorn, TopoLVM) a bigger number is also a
one-way door: `local-path` sets `allowVolumeExpansion: false`, and raising a
PVC request against a non-expandable StorageClass is **rejected by the API
server**, which wedges `helm upgrade` for every existing install. Growing a
number that means nothing is not worth breaking upgrades for.

There are exactly three things that actually bound this node, in order of how
much they matter:

1. **Bound the producers.** This is the real fix and it is where the chart's
   defaults do the work: `imagePipeline.zot.retention` (catalog *and* BuildKit
   cache repos), `cacheCleanup.mode`, `graphRetention`, and the kubelet image
   GC configured in the previous section. A volume with no producer bound will
   fill any number you give it; a volume with bounded producers does not need
   one.
2. **Use a StorageClass that can enforce, if you need enforcement.** `local-path`
   cannot. XFS project quotas on the backing filesystem, or a real CSI driver
   (TopoLVM, Longhorn), can. This is a node/infrastructure decision, not a chart
   value — the chart honours `storageClassName` and takes no position beyond
   that. Expect to size deliberately at that point, because on those drivers
   `size:` becomes a hard ENOSPC boundary rather than a suggestion.
3. **Watch the node filesystem, not the PVCs.** Under `local-path` every PVC on
   this box shares one filesystem, so per-PVC accounting is a fiction anyway and
   node-level free space is the only real signal:

   ```bash
   df -h /var/lib/rancher/k3s/storage
   du -sh /var/lib/rancher/k3s/storage/* | sort -h | tail
   ```

**On alerting.** The chart bundles Prometheus + Alertmanager
(`monitoring.enabled`, default off), and it deliberately does **not** ship a
"PVC is N% full" rule. The metrics that would carry that signal
(`kubelet_volume_stats_*`) come from the kubelet, and the bundled Prometheus
scrapes only `djinn-server` and `djinn-log-rotator` — it runs with
`automountServiceAccountToken: false` and has no RBAC for `nodes/metrics`. A
rule written against those metrics today would be permanently inert: an alert
that never fires, which is worse than no alert because it reads like coverage.
If you want that alert, add the kubelet scrape job, ServiceAccount and
ClusterRole first, then the rule — and verify the target is `up` before
believing it.

Install Helm if the box doesn't have it:

```bash
curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash
```

## 2. Install cert-manager + a Let's Encrypt issuer

```bash
helm repo add jetstack https://charts.jetstack.io
helm upgrade --install cert-manager jetstack/cert-manager \
  --namespace cert-manager --create-namespace --set crds.enabled=true

kubectl apply -f - <<'EOF'
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: letsencrypt-prod
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: you@example.com          # <-- your email
    privateKeySecretRef:
      name: letsencrypt-prod-key
    solvers:
      - http01:
          ingress:
            class: traefik
EOF
```

> Testing the pipeline first? Create a second issuer pointed at
> `https://acme-staging-v02.api.letsencrypt.org/directory` named
> `letsencrypt-staging` and reference that in the values below — you'll get an
> untrusted-but-working cert without burning prod rate limits.

## 3. Get the chart and write your values

```bash
git clone https://github.com/djinnos/djinn
cd djinn
```

Create `my-values.yaml` (a complete, working single-node profile — see
[Configuration](configuration.md) for every knob):

```yaml
image:
  server: ghcr.io/djinnos/djinn-server:0.6.69      # pin both to the latest release
  runtime: ghcr.io/djinnos/djinn-agent-runtime:0.6.69

# Single node: RWO PVCs are correct; k3s local-path provides them.
storage:
  mirrors:  { accessMode: ReadWriteOnce, size: 30Gi }
  cache:    { accessMode: ReadWriteOnce, size: 30Gi }
  projects: { accessMode: ReadWriteOnce, size: 20Gi }

ingress:
  enabled: true
  className: traefik
  host: djinn.example.com                          # <-- your domain
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt-prod
  tls:
    enabled: true
    secretName: djinn-tls

env:
  publicUrl: "https://djinn.example.com"           # <-- required: OAuth callbacks

# Bundled backing services.
postgres:
  enabled: true
  auth:
    password: "<generate-a-real-password>"
qdrant:
  enabled: true

# In-cluster image pipeline: BuildKit builds per-project devcontainer images
# and pushes them to the bundled Zot registry.
imagePipeline:
  enabled: true
  buildkitd:
    zotPlaintext: true          # Zot is plain HTTP on a ClusterIP
  zot:
    enabled: true
    anonymousPull: true         # kubelet pulls project images without a pull-secret
    auth:
      username: djinn
      password: "<generate-a-real-password>"
    storage:
      size: 60Gi

# Optional: bootstrap provider keys so models work before anyone opens Settings.
# Leave blank to connect providers through the UI (incl. ChatGPT/Codex OAuth).
secrets:
  providers:
    anthropicApiKey: ""
    openaiApiKey: ""
```

The credential-vault AES key (`secrets.vaultKey.key`) self-provisions on first
install and is preserved across upgrades — **back it up** (`kubectl get secret
djinn-vault-key -n djinn -o yaml`); losing it means re-entering every stored
credential.

## 4. Install

```bash
helm upgrade --install djinn deploy/helm/djinn \
  --namespace djinn --create-namespace \
  -f my-values.yaml

kubectl get pods -n djinn -w          # server, postgres, qdrant, zot, buildkitd → Running
kubectl get certificate -n djinn      # READY: True once Let's Encrypt issues
```

First boot pulls ~2 GB of images; give it a few minutes. Migrations run
automatically in the server pod's initContainer.

## 5. Wire the kubelet to the in-cluster registry

Task-run pods run per-project images pulled from Zot — but the host-network
kubelet can't resolve Zot's cluster-DNS name. Mirror the Service hostname to
its (stable) ClusterIP in k3s's registry config:

```bash
ZOT_IP=$(kubectl get svc djinn-zot -n djinn -o jsonpath='{.spec.clusterIP}')

cat >/etc/rancher/k3s/registries.yaml <<EOF
mirrors:
  "djinn-zot.djinn.svc.cluster.local:5000":
    endpoint:
      - "http://${ZOT_IP}:5000"
configs:
  "${ZOT_IP}:5000":
    tls:
      insecure_skip_verify: true
EOF

systemctl restart k3s     # picks up registries.yaml; running pods are unaffected
```

## 6. First login

Open `https://djinn.example.com`:

1. **Sign in with GitHub** — the first user to sign in becomes the admin.
2. **GitHub App setup:** With `env.enableSelfSetup: true` (set in
   `values.local.yaml`), the server prints a one-time manifest setup URL to
   its boot log on first startup with no credentials present. Open that URL in
   your browser to create the App, install it on your repos, and complete the
   OAuth callback — two GitHub clicks, zero manual credential entry. For
   production deployments (self-setup disabled), provide credentials via
   `secrets.githubApp.*` before deploying
   ([details](../GITHUB_APP_SETUP.md)).
3. Connect a model under **Settings → Models** if you didn't bootstrap keys.
4. Add a project from GitHub, watch its devcontainer image build, and write
   your first proposal.

## Upgrades

```bash
git -C djinn pull
# bump image.server + image.runtime together in my-values.yaml, then:
helm upgrade djinn deploy/helm/djinn -n djinn -f my-values.yaml
```

The migrate initContainer applies any new migrations before the new pod serves
traffic. The vault key and generated secrets are preserved.

## Gotchas

- **Single node = no HA.** A runaway compile can starve the box; each user's
  per-model concurrency cap is the throttle. Keep it low on small boxes.
- **Disk fills slowly, then all at once** — Zot image history plus build
  caches. Watch `df` and size PVCs generously.
- **`kernel.unprivileged_userns_clone` missing** on newer kernels is fine —
  user namespaces are enabled by default there.
- **Firewall**: only 22/80/443 need to be open inbound. If you run UFW, also
  allow k3s's pod/service CIDRs (`10.42.0.0/16`, `10.43.0.0/16`) and the
  `cni0` interface, or pod networking breaks.
- **Tear down**: `/usr/local/bin/k3s-uninstall.sh` removes k3s and every
  workload, PVC data included.
