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

None, and none are planned. Djinn's Phase 2 Kubernetes design
deliberately builds on stock `batch/v1 Job` + `v1 Secret` primitives and
exposes task-run observability through a label selector
(`djinn.app/task-run-id`) rather than a custom resource. That keeps
`kubectl get jobs -l djinn.app/task-run-id=…` working out of the box and
avoids the schema-migration burden that comes with a dedicated CRD.

This chart is kept intentionally empty so the `helm install djinn-crds`
lifecycle primitive exists up front (mirroring kagent's split-chart
pattern); if a future design ever needs a real custom resource, there is
a valid release to upgrade into without reshaping the install story.
