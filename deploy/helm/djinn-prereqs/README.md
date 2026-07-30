# djinn-prereqs

Cluster-scoped third-party prerequisites for Djinn. Install it as its own
release, before the `djinn` chart — the same shape as cert-manager.

Today it contains one pinned dependency: **Kueue `0.19.0`**, taken unmodified
from `oci://registry.k8s.io/kueue/charts/kueue`.

```bash
helm install djinn-prereqs deploy/helm/djinn-prereqs \
  --namespace kueue-system --create-namespace --wait
```

Requires **Kubernetes >= 1.29** (a Kueue 0.19 requirement). The upstream
`Chart.yaml` declares no `kubeVersion`, so Helm will *not* stop you on an older
cluster — check `kubectl version` yourself.

## What this release does and does not do

It is **inert**. Djinn's values set a *positive*
`managedJobsNamespaceSelector` requiring `djinn.io/kueue-managed: "true"`, and
nothing in this repository applies that label. Kueue therefore selects no
namespace and creates no Workload. Arming it belongs to cutover epic 4c9q.

`values.yaml` is the whole repository-owned policy; the subchart is untouched.
It replaces `deploy/kueue/vendor/kueue-v0.10.0.yaml`, a 13,175-line
byte-vendored fork of upstream's `manifests.yaml`, now deleted.

Read **[deploy/kueue/README.md](../../kueue/README.md)** before changing
anything here. It documents the pin, the forced `0.10.0` → `0.19.0` move, the
`objectSelector` scope reduction and its residual risk, and the contracts.

## Pinning

`Chart.lock` and `charts/kueue-0.19.0.tgz` are both committed. The tarball is
vendored so the contracts can render without registry access and so operators
install the exact reviewed bytes.

To bump:

```bash
# edit dependencies[0].version in Chart.yaml, then:
helm dependency update deploy/helm/djinn-prereqs
git add deploy/helm/djinn-prereqs/Chart.lock deploy/helm/djinn-prereqs/charts
git rm deploy/helm/djinn-prereqs/charts/kueue-<old>.tgz
bash deploy/kueue/tests/webhook-selectors.sh
```

`values.yaml` restates upstream's `managerConfig.controllerManagerConfigYaml`
in full (the chart exposes the two knobs Djinn needs only through that one
string). `deploy/kueue/tests/check-manager-config-drift.py` compares it against
the pinned tarball's own default and fails on any difference beyond the two
sanctioned edits — so a bump tells you exactly what upstream changed.

## Values

| Key | Default | Meaning |
| --- | --- | --- |
| `kueue.enabled` | `true` | Gates the whole Kueue dependency (`condition` in `Chart.yaml`). Set false if you manage Kueue elsewhere. |
| `kueue.managerConfig.controllerManagerConfigYaml` | see `values.yaml` | Upstream default + the namespace fence + `frameworks` minus `pod`/`deployment`/`statefulset`. |

Everything else under `kueue.` passes straight through to the upstream
subchart; see its own `values.yaml` inside `charts/kueue-0.19.0.tgz`.

## Relationship to `djinn-crds`

Unrelated. `djinn-crds` is reserved for **Djinn's own** future
CustomResourceDefinitions and is intentionally empty. Kueue is a third-party
operator and belongs here.

## Then install Djinn

The `djinn` chart's Kueue queue topology is off by default and must be
requested explicitly once this prerequisite exists:

```bash
helm install djinn deploy/helm/djinn \
  --namespace djinn --create-namespace --set kueue.enabled=true
```
