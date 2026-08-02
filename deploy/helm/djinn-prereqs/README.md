# djinn-prereqs

Cluster-scoped third-party prerequisites for Djinn. Install it as its own
release, before the `djinn` chart — the same shape as cert-manager.

Today it contains one pinned dependency: **Kueue `0.19.0`**, taken unmodified
from `oci://registry.k8s.io/kueue/charts/kueue`.

```bash
helm install djinn-prereqs deploy/helm/djinn-prereqs \
  --namespace kueue-system --create-namespace --wait
```

Requires **Kubernetes >= 1.30**, measured by installing this chart rather than
read off a release note. The upstream `Chart.yaml` declares no `kubeVersion`,
so Helm will *not* stop you on an older cluster — check `kubectl version`
yourself.

| Cluster | Result |
| --- | --- |
| 1.29.14 | **fails** — Kueue 0.19's CRDs use `spec.versions[].selectableFields`, which does not exist before 1.30, so the `workloads.kueue.x-k8s.io` apply is rejected |
| 1.30.13 | installs, controller Ready |
| 1.31.0 (the `scripts/kind/setup-kind.sh` pin) | installs, controller Ready |
| k3s 1.35.5 (production VPS) | above the floor |

The floor is 1.30 **only because `values.yaml` disables Kueue's DRA feature
gates**. Kueue 0.19 ships `KueueDRAIntegration` on, which makes the manager
index `resource.k8s.io/v1` ResourceSlices — an API group that is GA only in
Kubernetes **1.34**. Left at the upstream default the controller exits with
`could not setup ResourceSlice indexer`, `helm --wait` never returns, and you
are left with Established CRDs and registered webhooks behind a dead
controller. Do not remove those gates without moving this number to 1.34;
`tests/feature-gates.sh` fails if you try.

## What this release does and does not do

Djinn's values set a *positive* `managedJobsNamespaceSelector` requiring
`djinn.io/kueue-managed: "true"`, so this release captures nothing on its own —
a namespace must be labelled first. At stock values the `djinn` chart applies
that label: `kueue.armed: true` renders it onto the Namespace
(`djinn/templates/namespace.yaml`) and stamps `suspend: true` plus a queue name
onto every task-run, warm and standalone-SCIP Job, so Workloads are created and
Kueue's quota is what bounds build concurrency. This release is inert only
against a `djinn` deployment that explicitly sets `kueue.armed: false`.

That distinction matters when you reach for stock upstream Kueue instead of
this chart: at upstream defaults the Pod/Deployment/StatefulSet webhooks are
`failurePolicy: Fail` with a selector that covers `djinn`, so an unavailable
Kueue controller stops `djinn-server`, Postgres, Qdrant and task-run Pods
alike. This chart's values are what prevent that.

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

The `djinn` chart renders its Kueue queue topology at stock values
(`kueue.enabled: true`) and arms admission (`kueue.armed: true`), so this
release is a prerequisite rather than an option — nothing extra to request:

```bash
helm install djinn deploy/helm/djinn \
  --namespace djinn --create-namespace
```

Install this release **first**. `djinn/templates/prereq-guard.yaml` consults
live API discovery during a real install and refuses the release, naming this
chart, when `kueue.x-k8s.io/v1beta1` is not served — otherwise the operator
gets the API server's bare `no matches for kind "ResourceFlavor"`, which names
no remedy. A cluster that will never run Kueue installs `djinn` with
`--set kueue.enabled=false --set kueue.armed=false` (see
`djinn/values.local.yaml` for the full local-dev opt-out, which also disables
the cgroup launcher stack).
