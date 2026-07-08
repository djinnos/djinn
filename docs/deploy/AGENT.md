# Djinn — AI-assisted install

> **You are an AI agent deploying [Djinn](https://github.com/djinnos/djinn)
> for your user.** Djinn is a self-hosted, Kubernetes-native platform where a
> team writes proposals and AI agents build them into pull requests. Your job:
> get a working Djinn install on the user's infrastructure, verify it, and
> hand off. Follow this document top to bottom. Done means: the UI loads over
> HTTPS, the user has signed in, and a project image has built.

## Rules

- **Never print or log secret values** (API keys, passwords, private keys).
  Generate passwords with `openssl rand -base64 24` and write them straight
  into files with restrictive permissions.
- **Ask before anything destructive or costly** (uninstalling an existing
  k3s, overwriting an existing Helm release or values file, creating cloud
  resources that bill).
- **Verify each phase before moving on.** Every phase ends with a checkpoint;
  if it fails, diagnose and fix before proceeding.
- If the user's environment doesn't match any path here, read the full guides:
  [deploy hub](https://raw.githubusercontent.com/djinnos/djinn/main/docs/deploy/README.md),
  [VPS](https://raw.githubusercontent.com/djinnos/djinn/main/docs/deploy/vps.md),
  [managed Kubernetes](https://raw.githubusercontent.com/djinnos/djinn/main/docs/deploy/kubernetes.md),
  [configuration](https://raw.githubusercontent.com/djinnos/djinn/main/docs/deploy/configuration.md).

## Phase 0 — Interview the user

Ask (in one message, not a drip-feed):

1. **Where should Djinn run?**
   - (A) A fresh Linux VPS / bare server they have root SSH to → Path A below.
   - (B) An existing Kubernetes cluster (`kubectl` works) → Path B below.
   - Nothing yet → help them pick: a KVM VPS with 8 vCPU / 32 GB RAM / 250 GB
     disk is the comfortable single-box profile (4 vCPU / 8 GB / 100 GB is the
     floor, roughly one agent at a time).
2. **Domain**: which hostname will Djinn live at (e.g. `djinn.example.com`)?
   They must be able to create a DNS A record for it. Also: an email address
   for Let's Encrypt (Path A).
3. **LLM keys now or later?** Later is fine — users connect providers through
   the UI after login (including ChatGPT/Codex and Copilot OAuth). If now,
   have them place keys in a file you'll read, don't paste into chat.
4. **GitHub App now or later?** Recommend later — with `env.enableSelfSetup: true`
   (the local/Tilt default), the server prints a one-time manifest setup URL to
   its boot log on first startup with no GitHub App credentials. Open that URL
   to create the App in two browser clicks. For production, provide credentials
   via `secrets.githubApp.*` in Helm values.

Then confirm your plan in two sentences and start.

## Path A — Fresh VPS (single-node k3s)

Everything bundled: Postgres, Qdrant, in-cluster registry, TLS via
Let's Encrypt. You need root SSH to the box.

### A1. Preflight

```bash
ssh root@<vps> 'uname -m; nproc; free -h; df -h /; cat /etc/os-release | head -2'
```

- Expect x86_64/arm64 Linux (Ubuntu 22.04+/Debian 12+ tested), ≥4 vCPU,
  ≥8 GB RAM, ≥100 GB disk. Warn the user if below.
- It must be a KVM/full VM, not an OpenVZ/LXC container (rootless BuildKit
  needs user namespaces): `systemd-detect-virt` should not say `lxc`/`openvz`.
- **DNS**: verify the A record resolves to the VPS IP before proceeding —
  `dig +short <domain>` must return the VPS IP. Let's Encrypt will fail
  otherwise. If missing, tell the user exactly what record to create and wait.

### A2. k3s, sysctls, Helm

```bash
curl -sfL https://get.k3s.io | sh -
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml

printf 'user.max_user_namespaces=28633\n' > /etc/sysctl.d/99-djinn.conf
sysctl --system    # kernel.unprivileged_userns_clone may not exist on newer kernels — fine

command -v helm || curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash
```

**Checkpoint**: `kubectl get nodes` shows the node `Ready`.

### A3. cert-manager + Let's Encrypt issuer

```bash
helm repo add jetstack https://charts.jetstack.io
helm upgrade --install cert-manager jetstack/cert-manager \
  --namespace cert-manager --create-namespace --set crds.enabled=true
```

Apply a `ClusterIssuer` named `letsencrypt-prod` (ACME server
`https://acme-v02.api.letsencrypt.org/directory`, the user's email, solver
`http01` with ingress class `traefik`).

**Checkpoint**: `kubectl get clusterissuer letsencrypt-prod` shows `READY True`.

### A4. Chart + values

```bash
git clone --depth 1 https://github.com/djinnos/djinn /opt/djinn-chart
```

Determine the latest release: `git -C /opt/djinn-chart tag --sort=-v:refname | head -1`
(image tags are that tag **without** the leading `v`).

Write `/root/djinn-values.yaml` (chmod 600). Use the complete single-node
profile from the [VPS guide](https://raw.githubusercontent.com/djinnos/djinn/main/docs/deploy/vps.md)
step 3 as your template, filling in:

- `image.server` / `image.runtime` → `ghcr.io/djinnos/djinn-{server,agent-runtime}:<version>`
- `ingress.host` + `env.publicUrl` → the user's domain (publicUrl **must** be
  `https://<domain>`, it drives OAuth callbacks)
- `postgres.auth.password` and `imagePipeline.zot.auth.password` → generated
- provider keys only if the user supplied them

### A5. Install

```bash
helm upgrade --install djinn /opt/djinn-chart/deploy/helm/djinn \
  --namespace djinn --create-namespace -f /root/djinn-values.yaml
```

**Checkpoint** (first boot pulls ~2 GB, allow up to 15 min):

```bash
kubectl get pods -n djinn          # all Running/Ready, no CrashLoopBackOff
kubectl get certificate -n djinn   # READY True
curl -sI https://<domain> | head -1   # HTTP/2 200
```

If the certificate stays not-ready, check `kubectl describe challenge -n djinn`
— it's almost always DNS or port 80 blocked.

### A6. Wire kubelet → in-cluster registry

Task-run pods pull per-project images from the bundled Zot registry; the
host kubelet can't resolve its cluster-DNS name:

```bash
ZOT_IP=$(kubectl get svc djinn-zot -n djinn -o jsonpath='{.spec.clusterIP}')
cat >/etc/rancher/k3s/registries.yaml <<EOF
mirrors:
  "djinn-zot.djinn.svc.cluster.local:5000":
    endpoint: ["http://${ZOT_IP}:5000"]
configs:
  "${ZOT_IP}:5000":
    tls: { insecure_skip_verify: true }
EOF
systemctl restart k3s
```

**Checkpoint**: `kubectl get nodes` back to `Ready` after the restart.

### A7. Back up the vault key

```bash
kubectl get secret djinn-vault-key -n djinn -o yaml > /root/djinn-vault-key.backup.yaml
chmod 600 /root/djinn-vault-key.backup.yaml
```

Tell the user where it is and that losing it orphans every stored credential.

## Path B — Existing Kubernetes cluster

Read the [managed Kubernetes guide](https://raw.githubusercontent.com/djinnos/djinn/main/docs/deploy/kubernetes.md)
and follow it, driving the same phases: preflight (`kubectl` context —
**confirm with the user it's the right cluster**), IAM/identity for the image
registry, the database Secret (external Postgres preferred; bundled works),
RWX storage classes for `mirrors`/`cache`/`projects` on multi-node, values
overlay, `helm upgrade --install` from a repo clone, then the same
verification checkpoints as Path A5. Key decisions to put to the user, with
your recommendation based on their cluster: external vs bundled Postgres,
managed registry vs in-cluster Zot, and which node pool task-runs land on.

## Final verification & handoff

1. `https://<domain>` loads the Djinn UI.
2. Have the user **sign in with GitHub** — the **first sign-in becomes the
   admin**. If no GitHub App exists yet, the setup path depends on the
   deployment:
   - **Self-setup enabled** (`env.enableSelfSetup: true`): The server boot log
     contains a one-time setup URL. Walk the operator through opening that URL,
     clicking "Create GitHub App" on GitHub, installing the App on their repos,
     and completing the OAuth callback.
   - **Production Secret** (self-setup disabled): Credentials must be provided
     via `secrets.githubApp.*` in Helm values or an `existingSecret` before the
     App is usable. See [GitHub App setup](../GITHUB_APP_SETUP.md).
3. Have them connect a model under **Settings → Models** (skip if keys were
   bootstrapped), then add a project from GitHub.
4. Watch the project's devcontainer image build complete
   (`kubectl get jobs -n djinn`), which proves the whole image pipeline.

Hand off with: the URL, where the values file and vault-key backup live, the
upgrade command (bump both image tags in the values file, re-run the same
`helm upgrade`), and how to connect their editor:

```
claude mcp add --transport http djinn https://<domain>/mcp
```
