# djinn-crds

Empty-but-valid Helm chart that reserves a separate install surface for
future Djinn CustomResourceDefinitions. Shipping CRDs out of the main
`djinn` chart follows the pattern recommended by Helm's docs and used by
upstream kagent: CRDs have upgrade/uninstall semantics distinct from
workloads, so keeping them in their own release avoids accidental schema
teardown when the workload chart is uninstalled.

## When to install

Always before `djinn`:

```
helm install djinn-crds deploy/helm/djinn-crds
helm install djinn     deploy/helm/djinn --namespace djinn --create-namespace
```

## When to upgrade

Independently of `djinn`. CRD schema changes are rare and are reviewed
separately; day-to-day workload upgrades do not touch this chart.

## Current contents

None. Phase 2 PR 4 ships this chart empty — Djinn's Kubernetes runtime
dispatches `batch/v1 Job` + `v1 Secret` only. A `DjinnTaskRun` CRD may be
added here in a follow-up if kubectl-native observability demand appears.
