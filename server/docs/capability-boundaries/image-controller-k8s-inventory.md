# Image-controller Kubernetes capability-boundary inventory

**Owner:** team/image-controller  
**Tracked by:** epic/fztz (Wave 2 item 5)  
**Scope:** `server/crates/djinn-image-controller/src/{build_job.rs,controller.rs,watcher.rs}` and `server/crates/djinn-image-controller/tests/unit.rs`.  

## Decision: document and tighten, not migrate in this PR

The image-controller crate owns a large, coherent swath of Kubernetes interaction:

- `build_job.rs` — constructs the image-build `Job` manifest, its build-context `ConfigMap`, and the builder `PodSpec` (`k8s_openapi` types only; no client calls).
- `controller.rs` — drives the cluster lifecycle: `Api<Job>::list/get_opt/create/delete`, `Api<ConfigMap>::patch` (runtime and build-context), `OwnerReference` patching, and concurrency-cap counting.
- `watcher.rs` — runs a `kube::runtime::watcher` over build `Job`s, lists `Pod`s for log tails, and emits DB/events on terminal Job status.
- `tests/unit.rs` — constructs synthetic `Job`/`ObjectMeta`/`JobStatus` fixtures and feeds `kube::runtime::watcher::Event` values into the watcher transition logic.

A full migration behind `djinn-k8s` in one PR would require designing a new, stable owner-crate API that exposes:

1. image-build Job + ConfigMap builders (batch/core OpenAPI types),
2. a higher-level "apply-or-create Job, replace Failed Jobs" reconcile helper,
3. a streaming Job watcher with per-project/image terminal-state callbacks and optional Pod log fetching,
4. a concurrency-cap counting helper over labeled Jobs, and
5. unit-testable fixtures/types without callers importing `kube` or `k8s_openapi`.

That surface is too large and too domain-specific to be safely mechanical in a single session. It is better treated as a dedicated adapter epic with its own design spike. The current task therefore **documents the remaining direct usage** and **tightens the allowlist entries** so every exception is precise, owner-attributed, and points to this inventory and the cleanup epic.

## Pattern-by-pattern inventory

### 1. `build_job.rs` — manifest construction only

- Direct `k8s_openapi` imports for `Job`, `JobSpec`, `ConfigMap`, `Container`, `EnvVar`, `PodSpec`, `PodTemplateSpec`, `SecretVolumeSource`, `Volume`, `VolumeMount`, `OwnerReference`, `ObjectMeta`, `EmptyDirVolumeSource`, `ConfigMapVolumeSource`, `KeyToPath`.
- No `kube::` client usage; this file is the pure-function "builder" half of the image-controller's K8s seam.
- **Intended adapter path:** move the Job/ConfigMap value builders into `djinn-k8s::image_build_job` (or similar) as pure functions, parameterized by the existing `ImageControllerConfig` and `BuildContext` data. The owner crate would re-export the `k8s_openapi` value types so image-controller no longer needs its own `k8s-openapi` dependency for this path.
- **Why temporarily allowed:** the controller and watcher must keep their current `kube::` client types for now; splitting the builder out independently is valuable but only removes one file's imports. Keeping the builder in image-controller avoids a half-migrated seam.

### 2. `controller.rs` — Kubernetes client control plane

- `kube::Client` as `ImageController` state and `ImageControllerError::Kube`.
- `kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams}` for Job and ConfigMap operations.
- `k8s_openapi` for `Job`, `ConfigMap`, and `OwnerReference`.
- `set_context_cm_owner` currently emits a JSON-merge patch for `ownerReferences`; the rest uses `Patch::Apply`.
- **Intended adapter path:** introduce a `djinn-k8s::image_build::JobManager` (or reuse the existing warm-job/runtime helpers) that owns the `Api<Job>`/Api<ConfigMap>` operations behind a small trait, returning the same `Job`/`ConfigMap` values. Image-controller would hold the manager and pass it the manifest builders above. This matches the `K8sGraphWarmer` pattern already present in `djinn-k8s::graph_warmer`.
- **Why temporarily allowed:** this is the core reconciler for image builds; replacing it with a generic helper risks changing concurrency, retry, or owner-reference behavior. A dedicated adapter task can preserve semantics by test-driving the migration against the existing unit/integration tests.

### 3. `watcher.rs` — streaming Job watcher + Pod log fetcher

- `kube::Client` for log fetching and `Api<Job>`/`Api<Pod>`.
- `kube::runtime::watcher` and `kube::runtime::watcher::Event` for the streaming watch.
- `kube::api::{Api, ListParams, LogParams}` for Pod log tail retrieval.
- `k8s_openapi` for `Job`, `Pod`, `JobStatus`, and `ObjectMeta`.
- `__test_handle_event` is exposed for unit tests, taking `watcher::Event<Job>` directly.
- **Intended adapter path:** add a `djinn-k8s::job_watcher` abstraction that takes a label selector and callbacks for `Succeeded`/`Failed`/`Running` transitions, plus a `PodLogFetcher` for log tails. This would let image-controller register a handler that updates the DB and emits events, while the `kube::runtime::watcher` stays inside `djinn-k8s`. The `__test_handle_event` test seam would be replaced by a test double in the new abstraction.
- **Why temporarily allowed:** the watcher is tightly coupled to the `kube::runtime::watcher` stream shape and to the Pod-log fallback logic used in failure diagnostics. Extracting both without losing test coverage is larger than a single PR.

### 4. `tests/unit.rs` — test fixture construction

- Imports `k8s_openapi::api::batch::v1::{Job, JobStatus}`, `k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta`, and `kube::runtime::watcher` to construct `watcher::Event::Apply`/`InitApply` values.
- No production behavior; this is purely test scaffolding.
- **Intended adapter path:** once `djinn-k8s` exposes a job-watcher abstraction with a test double, the unit tests can feed synthetic transition events through the double without naming `kube::runtime::watcher::Event` or `k8s_openapi` types.
- **Why temporarily allowed:** it cannot move until the production watcher seam is defined, and the test-only usage is distinct from production direct usage.

## Allowlist treatment

The allowlist entries for the image-controller should be:

- One entry per file + matcher combination, not a directory glob.
- `owner = "team/image-controller"`.
- `rationale` that distinguishes production vs. test usage and cites this inventory document.
- `cleanup_issue = "epic/fztz"` (or a future child issue of the image-controller adapter epic).
- No broad `server/crates/djinn-image-controller/**` glob.

Any matcher that disappears because of this or a future task must be removed, and the remaining entries must stay narrow.

## Verification

After this cleanup, the per-capability K8s check should still pass when the allowlist is supplied:

```sh
find server -name '*.rs' | sh scripts/check-k8s-boundary.sh --files-from-stdin
```

The detector should not be weakened; the entries are explicit exceptions, not detector holes.
