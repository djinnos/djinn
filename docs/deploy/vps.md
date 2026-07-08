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
