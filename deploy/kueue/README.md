# Kueue prerequisite installation

Kueue is a **cluster-scoped third-party prerequisite** for Djinn, in exactly
the sense cert-manager already is: you install it as its own release, before
the `djinn` chart, and Djinn does not own its lifecycle. It is installed from
`deploy/helm/djinn-prereqs`.

It is deliberately **install and scope only**: nothing here labels the `djinn`
namespace, changes any Djinn workload, task-run, warm job, or control-plane
object, or activates Kueue admission for build Jobs. Arming it is the Kueue
cutover epic **4c9q**.

## Provenance

| Field | Value |
| --- | --- |
| Upstream project | `kubernetes-sigs/kueue` |
| Distribution | Upstream **OCI Helm chart**, unmodified |
| Repository | `oci://registry.k8s.io/kueue/charts/kueue` |
| Pinned chart version | `0.19.0` (appVersion `v0.19.0`) |
| Pin recorded in | `deploy/helm/djinn-prereqs/Chart.yaml` + `Chart.lock` |
| Chart digest | `sha256:2fbaa5b15b54c1149185d7f399c573838c630c10eb35b363763cbbd3327e4333` |
| Vendored dependency | `deploy/helm/djinn-prereqs/charts/kueue-0.19.0.tgz` |
| Minimum Kubernetes | **1.29** (Kueue 0.19 requirement; the chart does **not** declare `kubeVersion`, so Helm will not enforce it) |

### There is no fork any more

This directory used to hold `vendor/kueue-v0.10.0.yaml`: 13,175 lines of the
upstream `manifests.yaml` release asset, copied byte-for-byte except for four
hand-edited webhook entries. That file is **deleted**. Djinn's scoping now
lives as *values* over an untouched upstream chart, in
`deploy/helm/djinn-prereqs/values.yaml`.

### Why the pin moved 0.10.0 → 0.19.0

It was forced, not opportunistic. Kueue **never published a `0.10.0` chart** —
the OCI repository goes `0.9.5`, `0.10.3`, `0.11.0` … `0.19.0`. The old pin was
taken from a GitHub release *asset*, which has no chart equivalent, so "pin the
same version as a chart" was not an available option. `0.19.0` is the latest
published chart. Kueue 0.19 requires Kubernetes >= 1.29; the production VPS
(k3s v1.35.5) and the local kind cluster (1.31.0) both clear it.

### Offline / hermetic story

`charts/kueue-0.19.0.tgz` is **committed** (~200 KB) rather than resolved by
`helm dependency build` at deploy time. Two reasons:

1. The contracts in `tests/` render the chart with `helm template` and assert on
   the output. They run in a CI lane with no guarantee of registry access, and a
   contract that can only run when `registry.k8s.io` is reachable is a contract
   that gets skipped.
2. An operator installs the exact reviewed bytes. A resolve-at-install-time
   dependency could hand a cluster something the contracts never saw.

`Chart.lock` records the digest, so `helm dependency update` reproduces the
tarball and any substitution is detectable. To bump: edit the `version:` in
`Chart.yaml`, run `helm dependency update deploy/helm/djinn-prereqs`, commit
`Chart.lock` + the new tarball, delete the old one, and run the contracts — the
drift checker will print exactly what upstream changed underneath the override.

## The scoping policy (what replaced the fork)

`deploy/helm/djinn-prereqs/values.yaml` restates upstream's
`managerConfig.controllerManagerConfigYaml` with exactly two edits. The upstream
chart exposes both knobs only through that one opaque string, so a partial
override is not possible.

**1. Positive namespace fence.** Upstream ships a *negative* selector
(everything except `kube-system` / the release namespace). Djinn inverts it:

```yaml
managedJobsNamespaceSelector:
  matchLabels:
    djinn.io/kueue-managed: "true"
```

A namespace must be explicitly labelled before any Kueue admission webhook can
select an object in it. **No asset in this repository applies that label.** That
is what makes the release inert.

> `managedJobsNamespaceSelector` is **not** merely controller config. The 0.19.0
> chart templates each webhook's own `namespaceSelector` from it:
> `templates/webhook/manifests.yaml` renders
> `{{- if (hasKey $managerConfig "managedJobsNamespaceSelector") }}` and emits
> the value verbatim, falling back to the negative `kubernetes.io/metadata.name
> NotIn [kube-system, <release-ns>]` expression otherwise. Setting it therefore
> **does** change webhook registration scope — including `mjob`/`vjob`. This is
> the values hook that positively scopes the Job webhooks, and it is why the
> availability regression below is avoided rather than merely documented.

**2. `integrations.frameworks` minus `pod`, `deployment`, `statefulset`.**
Upstream 0.19.0 ships all three **enabled** (they were commented out as
recently as 0.10.3, so an assumption carried over from the old pin is wrong
here). At stock defaults that means every Pod, Deployment and StatefulSet CREATE
outside `kube-system`/`kueue-system` traverses a `failurePolicy: Fail` Kueue
webhook — on a single-node cluster, an unavailable Kueue controller then blocks
`djinn-server`, Postgres, Qdrant and task-run Pods alike. Djinn must never rely
on the upstream default here.

**The three webhooks cannot be unregistered through values.** Upstream renders
`mpod`/`vpod`, `mdeployment`/`vdeployment` and `mstatefulset`/`vstatefulset`
unconditionally and uses `integrations.frameworks` only to switch their
`failurePolicy`:

```gotemplate
{{- if has "pod" $integrationsConfig.frameworks }}
failurePolicy: Fail
{{- else }}
failurePolicy: Ignore
{{- end }}
```

Removing the three names therefore yields `failurePolicy: Ignore`, which *is*
the availability guarantee: an unreachable Kueue webhook is skipped rather than
fatal for those creations. The contract asserts that policy on the rendered
output, and would fail if anyone put `pod` back.

## Scope reduction: `objectSelector` is gone

**Read this before treating the new contract as equivalent to the old one.**

The retired fork carried a *second* fence on the Job/Pod webhooks:

```yaml
objectSelector:
  matchLabels:
    djinn.io/kueue-build-object: "true"
```

**The upstream chart has no `objectSelector` hook at any version.** Keeping that
assertion would require re-forking the manifest or post-processing the rendered
output — i.e. recreating the exact problem this change removes. So it is
**removed, not weakened**. `tests/check-webhook-selectors.py` no longer asserts
it, says so in its output, and its docstring records why.

What that costs, precisely:

| Webhook | Before (fork) | Now (pinned chart) |
| --- | --- | --- |
| `mpod` / `vpod` | namespace + object fence, `failurePolicy: Fail` | namespace fence, **`failurePolicy: Ignore`** (framework disabled) |
| `mdeployment` / `vdeployment`, `mstatefulset` / `vstatefulset` | not scoped by the fork | namespace fence, **`failurePolicy: Ignore`** |
| `mjob` / `vjob` | namespace + object fence | **namespace fence only**, `failurePolicy: Fail` |

For comparison, at **stock upstream defaults** all six would be
`failurePolicy: Fail` with a selector that *does* select `djinn`. Both the
namespace fence and the `Ignore` policy are Djinn values doing work; the
contract proves it by rejecting the stock render.

### Residual risk (input to 4c9q)

`mjob`/`vjob` keep upstream's `failurePolicy: Fail` and are now fenced by
**namespace only**. In a namespace labelled `djinn.io/kueue-managed=true`, every
`batch/v1` Job CREATE routes through Kueue's webhook, and a Kueue control-plane
outage blocks all of them — not just Djinn build Jobs. The fork's per-object
label used to bound that set.

Today the blast radius is **zero**, because no namespace carries the label —
and that is *asserted*, not assumed: `tests/webhook-selectors.sh` extracts the
labels the `djinn` chart actually renders onto its Namespace and evaluates every
relevant webhook's `namespaceSelector` against them the way the API server
would. The same test also runs the inverse case, so the cost of labelling
`djinn` is printed as a failure rather than discovered in production.

The cutover epic **4c9q** must choose where that label goes with this in mind:

- A **dedicated build-Job namespace** keeps the fenced set to build Jobs. This
  is the option the residual risk argues for. It is deliberately *not*
  implemented here — it belongs to 4c9q.
- Labelling **`djinn` itself** would put every Job in the control-plane
  namespace behind a `Fail`-policy webhook. Do not do this without deciding the
  outage story first.

Do **not** "fix" this by post-processing chart output to re-add an
`objectSelector`. That is a fork with extra steps.

## Contracts

Both run with no cluster and no registry:

```sh
bash deploy/kueue/tests/webhook-selectors.sh
bash deploy/kueue/tests/zero-capture-gate.sh
```

`webhook-selectors.sh` renders `deploy/helm/djinn-prereqs` — the artifact a
cluster actually receives — and asserts, on every core
`pods`/`jobs`/`deployments`/`statefulsets` CREATE webhook:

1. that its `namespaceSelector`, evaluated as the API server would against the
   labels the `djinn` chart really renders onto its Namespace, does **not**
   select `djinn` (the availability assertion);
2. that the selector is exactly the positive `djinn.io/kueue-managed` fence;
3. that the `pods`/`deployments`/`statefulsets` webhooks are
   `failurePolicy: Ignore`.

It then proves it is non-vacuous against **real mis-scoped renders** of the same
pinned subchart: upstream defaults, a `djinn` namespace carrying the managed
label, `pod` re-added to the frameworks, the namespace fence deleted, and an
empty (`kueue.enabled=false`) render. A separate drift checker
(`check-manager-config-drift.py`) reads the subchart's own default out of the
pinned tarball and fails if the override differs from it in any way other than
the two sanctioned edits.

The old contract hard-wired `vendor/kueue-v0.10.0.yaml`. Under a chart-based
design that path is not what any cluster installs, so the test would have stayed
green while validating a file nobody deploys. It now renders the chart, and
fails loudly if `deploy/kueue/vendor/` ever reappears.

Both scripts need `helm` and python3 with PyYAML. They **fail** rather than skip
if either is absent.

## Zero-capture prerequisite gate

The structural contract proves the selectors are *shaped* correctly. It does not
prove that installing the prerequisite alongside the inert chart captures
nothing. `zero-capture-gate.sh` proves that on a real disposable cluster, and
`deploy/runbooks/kueue-inert-release-zero-capture.md` makes a passing invocation
a mandatory prerequisite for 4c9q. It installs `deploy/helm/djinn-prereqs`
followed by the `djinn` chart with `kueue.enabled=true`, then requires
`kubectl get workloads -n djinn` to return zero items.

`deploy/kueue/tests/zero-capture-gate.sh` is its hermetic fake-`kubectl`
contract; it needs no cluster credentials.
