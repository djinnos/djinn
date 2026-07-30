//! Runtime configuration shared by every `djinn-k8s` helper.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Toleration;
use serde::{Deserialize, Serialize};

use crate::launcher::CgroupLauncherMode;

/// The measured end-to-end cost of one complete production warm Job
/// (2026-07-27): 5442s — 1798s of cargo and 3644s of graph phase, of which the
/// SCIP index was 3523s.
///
/// Public because it is the floor two independent decisions are checked
/// against: [`KubernetesConfig::warm_job_timeout_seconds`] must exceed it, and
/// any time the warm path is allowed to *wait* (see
/// `djinn_graph::semantic_index_claim::DEFAULT_WAIT_SECONDS`) has to fit in
/// what is left over, because a wait that does not pay off still has to be
/// followed by the whole warm.
pub const MEASURED_FULL_WARM_SECONDS: i64 = 5_442;

/// Configuration for `KubernetesRuntime`.
///
/// Loaded once at djinn-server boot and cloned into the runtime. Fields
/// intentionally mirror what the Helm chart surfaces as values so operators
/// can tune them without touching code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KubernetesConfig {
    /// Kubernetes namespace for Jobs, Secrets, and the worker ServiceAccount.
    pub namespace: String,
    /// Fully-qualified image reference for `djinn-agent-runtime`
    /// (e.g. `ghcr.io/djinn/djinn-agent-runtime:0.1.0`).
    pub image: String,
    /// `imagePullPolicy` for the worker container. Defaults to `IfNotPresent`.
    pub image_pull_policy: String,
    /// ServiceAccount mounted into each worker Pod. Provides the projected
    /// token authenticating back to djinn-server.
    pub service_account: String,
    /// CPU **request** for a *build-capable* task-run Pod (Worker / Verifier /
    /// Architect and their retry/resume flows, plus the fail-safe default for
    /// any unknown/new role — see [`crate::launcher::RoleResourceClass`]).
    ///
    /// v1 leases default: `"1"`. The `cpu.weight` (CFS share) a container gets
    /// derives from its REQUEST, so a build-capable pod that will actually
    /// compile needs a full core of guaranteed share. Prod overrides this to
    /// `4` via `DJINN_K8S_CPU_REQUEST` (see the taskrun-pod-cpu-4 benchmark
    /// note) — the env override is preserved, so this default only sets the
    /// out-of-the-box value.
    pub cpu_request: String,
    /// CPU **request** for a *light* task-run Pod (Planner / Reviewer / Lead /
    /// every Refinement sub-role / grooming). These pods orchestrate an agent
    /// session and are *unlikely* to run the project's compile/test toolchain
    /// (measured at 5.5% of light sessions), so they get a fractional-core
    /// request. They are not incapable of compiling — the minority that do are
    /// governed by the measured, role-agnostic invocation lease, which is why
    /// the CPU **limit** below is deliberately not role-classed. v1 leases
    /// default: `"300m"`. Overridable via `DJINN_K8S_LIGHT_CPU_REQUEST`.
    ///
    /// Only the CPU REQUEST is role-classed: the CPU **limit**
    /// ([`Self::cpu_limit`]) and both memory bounds are identical for light and
    /// build-capable pods ("same limits everywhere"), and the launcher/broker
    /// contract is identical regardless of role.
    pub light_cpu_request: String,
    /// CPU limit shared by both role classes (light and build-capable). v1
    /// leases default: `"4"`. The limit is deliberately NOT role-classed — only
    /// the guaranteed request differs by role. Overridable via
    /// `DJINN_K8S_CPU_LIMIT`.
    pub cpu_limit: String,
    /// Memory request shared by both role classes. v1 leases default: `"2Gi"`.
    /// Overridable via `DJINN_K8S_MEMORY_REQUEST` (prod projects a larger
    /// Burstable value — see the taskrun-memory-burstable note).
    pub memory_request: String,
    /// Memory limit shared by both role classes. v1 leases default: `"4Gi"`.
    /// Overridable via `DJINN_K8S_MEMORY_LIMIT`.
    pub memory_limit: String,
    /// RWX PVC backing the task-run mirror (mounted read-only at `/mirror`).
    pub mirror_pvc: String,
    /// RWX PVC holding canonical project roots. Task-run Pods mount only an
    /// owning project read-source subPath from this claim, never the namespace.
    pub projects_pvc: String,
    /// RWX PVC backing shared caches (cargo / pnpm / pip). Mounted writeable
    /// at `/cache` — the Job manifest mounts this PVC once; the worker carves
    /// per-tool subdirectories (`/cache/cargo`, `/cache/pnpm`, `/cache/pip`)
    /// itself.
    pub cache_pvc: String,
    /// DNS address of the djinn-server RPC listener
    /// (e.g. `djinn.djinn-system.svc.cluster.local:8443`). Worker dials this.
    pub server_addr: String,
    /// TTL (seconds) applied to completed graph-warm Jobs for auto-GC.
    /// Shorter than task-run Jobs because warm Jobs are disposable the
    /// moment they've populated `repo_graph_cache`.
    pub warm_job_ttl_seconds: i32,
    /// Maximum wall-clock seconds a warm Job may run before the kubelet
    /// terminates it (`activeDeadlineSeconds`). Keeps a wedged indexer
    /// subprocess from pinning a Pod indefinitely.
    ///
    /// Default 7200s (120 minutes). The warm Pod runs a single default-features
    /// cargo pass (clippy + build + test-compile) matching the worker's feature
    /// set, and then — inside the SAME Pod and against the SAME deadline — the
    /// whole SCIP indexing and graph-publication phase.
    ///
    /// The old 3600s default only ever accounted for the cargo half. MEASURED on
    /// a complete production warm (2026-07-27, 22:12:30Z → 23:43:12Z): the graph
    /// phase alone took 3 644 498 ms (60m44s), of which the rust-analyzer SCIP
    /// pass was 3 522 197 ms, and the whole Job needed ~5442s. **On the shipped
    /// default every warm of that workspace would have been SIGKILLed at 60
    /// minutes, forever** — only the deployed
    /// `DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS=7200` override kept it alive, so the
    /// chart default was silently a broken configuration for any workspace of
    /// that size. 7200s covers the measured worst case with ~32% margin.
    ///
    /// Warm Jobs run in the background and don't affect worker latency, so a
    /// generous deadline is safe. Tunable per deployment via
    /// `DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS`; raise it if a larger workspace
    /// consistently hits the deadline. This sets the Job's
    /// `activeDeadlineSeconds`; the in-process watcher deadline follows it
    /// (plus a small grace) so a long warm run is not declared failed while the
    /// Job is still legitimately running.
    pub warm_job_timeout_seconds: i64,
    /// MySQL DSN forwarded to the warm Pod so `djinn-agent-worker
    /// warm-graph` can reuse the server's backing MySQL instance.
    /// `None` leaves the warm binary to fall back to its built-in default
    /// (`mysql://root@127.0.0.1:3306/djinn`), which only works in single-
    /// process local test setups.
    pub database_url: Option<String>,
    /// Maximum wall-clock seconds a task-run Pod may live before the
    /// kubelet terminates it (`activeDeadlineSeconds` on the Job). Without
    /// this, a stuck RPC connection or runaway LLM stream can keep the Pod
    /// alive indefinitely — the Job's `ttl_seconds_after_finished` only
    /// fires after the Pod exits, so a hung worker leaks compute forever.
    /// Default 10800s (3 hours): this is an infra BACKSTOP, not a run
    /// scheduler. The in-pod supervisor arms its own soft deadline at
    /// `this - margin` and winds itself down gracefully (cancel + checkpoint
    /// commit/push) well before the kubelet hard-kills the Pod, so a slow
    /// model never loses work to the deadline in the healthy case. A 1-hour
    /// default starved slow providers — a 50-60 min worker stage left no room
    /// for the reviewer stage and the Pod was deadline-killed mid-review,
    /// losing every commit — so the backstop is set generously.
    pub task_run_active_deadline_seconds: u64,
    /// `terminationGracePeriodSeconds` on the task-run Pod. K8s default 30s
    /// is tight: when SIGTERM fires (deadline / eviction / drain) the worker
    /// must both flush its terminal `TerminalReport` RPC AND run a checkpoint
    /// commit/push so in-flight work survives the kill. Default 60s gives both
    /// room before SIGTERM is escalated to SIGKILL.
    pub task_run_termination_grace_period_seconds: i64,
    /// CPU request on the warm Pod container. The canonical-graph pipeline
    /// spawns SCIP indexer subprocesses (rust-analyzer, scip-go, etc.) that
    /// can spike CPU; without a request the scheduler has no fairness
    /// signal and the Pod can be evicted under contention.
    ///
    /// Default `4` — deliberately equal to `warm_cpu_limit`. Kubernetes
    /// derives the container's cgroup `cpu.weight` (CFS share) from the
    /// REQUEST, not the limit. A `1` request left the warm Pod with a tiny
    /// share under host contention: on the single-node k3s VPS, six task-run
    /// Pods (each requesting ~2 cores) drove host load ~20, and the warm
    /// Pod's `rust-analyzer scip` pass — which finishes the `server`
    /// workspace in ~257s on an idle 16-core box — was throttled to ~2
    /// effective cores and blew past its 1202s → 1800s adaptive wall-clock
    /// caps on every run. Graph freshness is user-facing (task-runs adopt
    /// the warm's canonical graph), so the warm's cgroup weight must reflect
    /// its actual need: matching the request to the limit gives it a fair
    /// CFS share proportional to what it will actually use. Tunable via
    /// `DJINN_K8S_WARM_CPU_REQUEST`.
    pub warm_cpu_request: String,
    /// CPU limit on the warm Pod container. Caps the indexer subprocesses
    /// so they don't starve neighbours on the same node. Default `4`.
    ///
    /// Bumped `2` → `4`: `rust-analyzer scip` derives `CARGO_BUILD_JOBS` from
    /// the cgroup-visible CPU quota (see djinn-graph `cargo_build_jobs`), and
    /// runs concurrently with the scip-typescript indexers. A `2` limit
    /// throttled the Rust workspace so hard it hit its wall-clock cap on every
    /// warm; `4` gives the derived job count room to match without letting a
    /// single warm Pod monopolise a node.
    pub warm_cpu_limit: String,
    /// Memory request on the warm Pod container. SCIP index buffers for a
    /// medium-sized Rust workspace already crest 1 GiB; reserving 2 GiB
    /// keeps the kubelet from killing the Pod under memory pressure.
    pub warm_memory_request: String,
    /// Memory limit on the warm Pod container. Hard ceiling so a runaway
    /// indexer can't OOM the node. Default `4Gi`.
    pub warm_memory_limit: String,
    /// CPU request on the standalone SCIP-index Pod ([`crate::scip_job`]).
    ///
    /// Default `1`. Unlike every other build-tier request this one is **not**
    /// sized for parallelism — the SCIP phase is 94% serial (rust-analyzer's
    /// `StaticIndex::compute` is a bare loop with no rayon, upstream issue
    /// #18140) so a second core buys almost nothing. It is sized instead by the
    /// capacity budget: the SCIP Job takes no build lease, so under proposal
    /// `8ixk` its request folds into `protected_mcpu` and is subtracted from the
    /// CPU build slots are derived from. The ceiling before the derived cap
    /// drops from 2 to 1 on the current 12-core node is **2200m**; see
    /// [`crate::scip_job::SCIP_PROTECTED_REQUEST_CEILING_MILLICORES`], which is
    /// enforced by test.
    pub scip_cpu_request: String,
    /// CPU limit on the standalone SCIP-index Pod. Default `2` — headroom for
    /// the brief parallel window (measured 20s against a 368s serial window)
    /// and for the non-Rust indexers, without changing the request the capacity
    /// derivation actually reads.
    pub scip_cpu_limit: String,
    /// Memory request on the standalone SCIP-index Pod. Default `4Gi`.
    pub scip_memory_request: String,
    /// Memory limit on the standalone SCIP-index Pod. Default `16Gi`.
    ///
    /// This is a **measured** figure, not a default to be trimmed: peak RSS for
    /// the SCIP phase is 10.0 GB. The warm Pod's `warm_memory_limit` still
    /// defaults to `6Gi` and survives production only because an out-of-band
    /// override raises it to 16Gi — this field states the real number in the
    /// manifest instead, and
    /// `crate::scip_job::tests::scip_memory_limit_covers_the_measured_peak`
    /// fails the build if it is ever lowered below the measured peak.
    pub scip_memory_limit: String,
    /// Cadence floor for the standalone SCIP index, in seconds. Default
    /// `10800` (3 hours).
    ///
    /// This is a *rate limit*, not a timer: a tick still dispatches nothing
    /// unless the repository head has advanced since the last successful index
    /// (see [`crate::scip_schedule`]). It exists so a continuously-advancing
    /// `main` cannot turn a leaseless 16Gi Job into a permanent resident.
    pub scip_index_interval_seconds: u64,
    /// `ttlSecondsAfterFinished` on the SCIP-index Job. Default `14400` (4h).
    ///
    /// Deliberately longer than [`Self::scip_index_interval_seconds`]: the
    /// retained *succeeded* Job set — keyed by its
    /// `djinn.app/scip-revision` annotation — is the durable ledger the
    /// change-detection gate reads. A TTL shorter than the interval would make
    /// every tick look like "never indexed" and re-dispatch unconditionally.
    pub scip_job_ttl_seconds: i32,
    /// `activeDeadlineSeconds` on the SCIP-index Job. Default `7200` (2h)
    /// against a measured 3523s (58m43s) SCIP phase.
    pub scip_job_timeout_seconds: i64,
    /// Master switch for standalone SCIP-index dispatch. **Defaults to
    /// `false`.**
    ///
    /// The composition root wires the real
    /// [`crate::scip_schedule::ScipIndexScheduler`] unconditionally — this flag
    /// is checked inside the tick, not at wiring time, so arming the feature is
    /// a config flip rather than a redeploy, and the code path is never
    /// unreachable. Off means the watcher ticks, logs its decisions at debug,
    /// and creates nothing.
    pub scip_index_enabled: bool,
    /// How long the repository head must have stood still before a SCIP index
    /// is worth producing, in seconds. Default `3523` — the measured duration
    /// of the SCIP phase itself.
    ///
    /// The SCIP cache key folds in `source_hashes`, so an index produced at
    /// head `H0` only serves a warm running against `H0`'s sources. If `main`
    /// advances during the ~59-minute run, the following warm misses and
    /// re-indexes inline. Requiring the tree to have been stable for at least
    /// as long as the run takes is the cheap, stateless way to bias toward the
    /// case where the artifact is still current when it lands. `0` disables the
    /// gate. See [`crate::scip_schedule::decide`].
    pub scip_quiescence_seconds: u64,
    /// How long the **warm** Pod waits for another Pod's in-flight semantic
    /// index of the same tree before indexing inline itself, in seconds.
    /// Default `900`.
    ///
    /// Projected into the warm Job under
    /// [`crate::warm_job::WARM_SCIP_CLAIM_WAIT_ENV`], which is the key
    /// `djinn_graph::semantic_index_claim` reads. Rendering it is what makes
    /// this an operator lever at all: the warm Pod's environment is exactly
    /// what this manifest puts in it, so a value that is never rendered is a
    /// value that can never be set.
    ///
    /// The bound exists because the wait can be a total loss — the holder may
    /// outlast it — and the warm still has to run every phase itself
    /// afterwards, inside the same `activeDeadlineSeconds`. `0` disables
    /// waiting; the warm then indexes immediately, duplicating the in-flight
    /// run exactly as it did before the claim existed.
    pub scip_claim_wait_seconds: u64,
    /// `spec.nodeSelector` applied to both task-run and warm Pods. Empty map
    /// leaves the field unset (any node tolerating the Pod's other constraints
    /// is eligible). Operators typically use this together with `tolerations`
    /// to pin builds onto a dedicated NodePool — e.g. nodeSelector
    /// `workload-type: djinn` plus a matching toleration for a
    /// `workload-type=djinn:NoSchedule` taint. Surfaced in the chart as
    /// `resources.taskrun.nodeSelector`.
    pub node_selector: BTreeMap<String, String>,
    /// `spec.tolerations` applied to both task-run and warm Pods. Empty vec
    /// leaves the field unset. Same NodePool-pinning use case as
    /// `node_selector` above. Surfaced in the chart as
    /// `resources.taskrun.tolerations`.
    pub tolerations: Vec<Toleration>,
    /// Cgroup-v2 delegation profile the enforcement launcher runs under. The
    /// only profile v1 supports is `"cgroup-v2-cpu-only"`
    /// ([`crate::launcher::CGROUP_PROFILE_V2_CPU_ONLY`]): a cgroup-v2 mount with
    /// a delegated root owned by uid 0 and exactly the `cpu` controller enabled
    /// for children. [`crate::launcher::validate_enforcement_render`] maps this
    /// string onto the SAME `Readiness::validate` the launcher runs in-pod, so a
    /// misconfigured node profile fails closed at dispatch BEFORE any user code
    /// executes. Overridable via `DJINN_K8S_CGROUP_DELEGATION_PROFILE`.
    pub cgroup_delegation_profile: String,
    /// Administrator-configured hard bounds for per-project `build_resources`
    /// overrides on the **task-run** Pod. Empty (the default) leaves every axis
    /// unbounded — a per-project override is only bounded by request ≤ limit.
    /// A resolved request below `cpu_min`/`memory_min` or a resolved limit above
    /// `cpu_max`/`memory_max` fails closed at resolution (no Job created).
    /// Overridable via `DJINN_K8S_TASK_{CPU,MEMORY}_{MIN,MAX}`.
    #[serde(default)]
    pub task_resource_bounds: crate::build_resources::ResourceBounds,
    /// Administrator-configured hard bounds for per-project `build_resources`
    /// overrides on the **warm** Pod. Same semantics as
    /// [`Self::task_resource_bounds`]; overridable via
    /// `DJINN_K8S_WARM_{CPU,MEMORY}_{MIN,MAX}`.
    #[serde(default)]
    pub warm_resource_bounds: crate::build_resources::ResourceBounds,
    /// Volume-ownership mode used for the workspace/cache/mirror surfaces. v1
    /// requires `"fsgroup-on-root-mismatch"`
    /// ([`crate::launcher::VOLUME_OWNERSHIP_ON_ROOT_MISMATCH`]): `fsGroup =
    /// ARTIFACT_GID (1000)` re-owned only when the volume root gid mismatches.
    /// Any other mode fails render validation. Overridable via
    /// `DJINN_K8S_VOLUME_OWNERSHIP_MODE`.
    pub volume_ownership_mode: String,
    /// Whether a task-run Pod renders the cgroup-launcher sidecar.
    ///
    /// Defaults to [`CgroupLauncherMode::Required`]: the launcher sidecar,
    /// private IPC/cgroup surfaces, and worker enforcement signal are rendered
    /// together. `disabled` remains an explicit local/development compatibility
    /// profile.
    /// Overridable via `DJINN_K8S_CGROUP_LAUNCHER_MODE`; an unrecognized value is
    /// ignored with a warning rather than silently flipping enforcement.
    #[serde(default)]
    pub cgroup_launcher_mode: CgroupLauncherMode,
    /// Activates the kubelet-delegated cgroup RuntimeClass for new task-run Pods.
    #[serde(default)]
    pub task_run_cgroup_writable_enabled: bool,
    /// Opts task-run, warm and standalone-SCIP Jobs into Kueue admission.
    ///
    /// When true the renderers stamp `suspend: true`, the
    /// `kueue.x-k8s.io/queue-name` LocalQueue selector and the
    /// `djinn.io/kueue-build-object` marker onto BOTH the Job metadata and the
    /// Pod template, so Kueue's Job webhook and Pod webhook both see them.
    ///
    /// DANGER: `suspend: true` is not inert. Kueue only captures Jobs in a
    /// namespace labelled `djinn.io/kueue-managed`, so a suspended Job in an
    /// unlabelled namespace is never admitted and hangs forever. The Helm chart
    /// drives this field and that namespace label from the SAME value
    /// (`kueue.armed`) for exactly that reason — never plumb them apart.
    ///
    /// Overridable via `DJINN_KUEUE_ARMED`; defaults to false.
    #[serde(default)]
    pub kueue_armed: bool,
    /// Name prefix of the per-kind LocalQueues rendered by
    /// `deploy/helm/djinn/templates/kueue-topology.yaml`.
    ///
    /// The chart names them `<djinn.fullname>-task-run` / `-warm` / `-scip`, and
    /// `djinn.fullname` depends on the Helm release name — so the value cannot be
    /// hard-coded here without risking a `queue-name` that resolves to no
    /// LocalQueue, which (once armed) means the Job is never admitted.
    ///
    /// Overridable via `DJINN_KUEUE_LOCAL_QUEUE_PREFIX`. Only read when
    /// [`Self::kueue_armed`] is true.
    #[serde(default = "default_kueue_local_queue_prefix")]
    pub kueue_local_queue_prefix: String,
}

fn default_kueue_local_queue_prefix() -> String {
    "djinn".into()
}

/// Kueue's LocalQueue selector. Kueue's Job webhook reads it from the Job, and
/// its Pod webhook reads it from the Pod — hence both label locations.
pub const LABEL_KUEUE_QUEUE_NAME: &str = "kueue.x-k8s.io/queue-name";

/// Djinn's own marker for "this object is admitted through Kueue". Used by the
/// chart contracts and by `tests/kueue_build_object_labels.rs` to tell an armed
/// build object apart from a control-plane workload.
pub const LABEL_KUEUE_BUILD_OBJECT: &str = "djinn.io/kueue-build-object";

/// The Job kinds that participate in Kueue admission.
///
/// There is deliberately NO image-build variant. An image build is an upstream
/// dependency of a task-run, so sharing one ClusterQueue would let a task-run
/// hold slots while waiting for the image build queued behind it — a
/// priority inversion the current ledger cannot produce. Keeping the variant
/// absent makes that mistake a compile error rather than a review catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KueueQueueKind {
    /// Per-task-run worker Job.
    TaskRun,
    /// Graph-warm Job.
    Warm,
    /// Standalone SCIP index Job.
    Scip,
}

impl KueueQueueKind {
    /// Suffix of the LocalQueue this kind is admitted through. Must match the
    /// LocalQueue names in `deploy/helm/djinn/templates/kueue-topology.yaml`.
    #[must_use]
    pub const fn local_queue_suffix(self) -> &'static str {
        match self {
            Self::TaskRun => "task-run",
            Self::Warm => "warm",
            Self::Scip => "scip",
        }
    }
}

impl KubernetesConfig {
    /// Name of the LocalQueue `kind` is admitted through.
    #[must_use]
    pub fn kueue_local_queue_name(&self, kind: KueueQueueKind) -> String {
        format!(
            "{}-{}",
            self.kueue_local_queue_prefix,
            kind.local_queue_suffix()
        )
    }

    /// `JobSpec::suspend` for a build Job of `kind`.
    ///
    /// `None` when disarmed so the rendered Job is byte-identical to the
    /// pre-cutover shape — an explicit `Some(false)` would serialize a
    /// `suspend: false` key that was never there before.
    #[must_use]
    pub fn kueue_job_suspend(&self) -> Option<bool> {
        self.kueue_armed.then_some(true)
    }

    /// Stamp the Kueue admission labels for `kind` into `labels` when armed, and
    /// do nothing at all when disarmed.
    ///
    /// Call this from each renderer's `job_labels()` — the single map that is
    /// cloned into BOTH `Job.metadata.labels` and
    /// `Job.spec.template.metadata.labels`. Stamping after the fact (the
    /// `stamp_admission_identity` pattern in `graph_warmer_identity.rs`) reaches
    /// only the Job metadata and silently misses Kueue's Pod webhook.
    pub fn apply_kueue_build_object_labels(
        &self,
        kind: KueueQueueKind,
        labels: &mut BTreeMap<String, String>,
    ) {
        if !self.kueue_armed {
            return;
        }
        labels.insert(LABEL_KUEUE_BUILD_OBJECT.to_string(), "true".to_string());
        labels.insert(
            LABEL_KUEUE_QUEUE_NAME.to_string(),
            self.kueue_local_queue_name(kind),
        );
    }
}

impl KubernetesConfig {
    /// Minimal default used by unit tests; production deployments load
    /// their Kubernetes settings from the `DJINN_*` environment projected by
    /// `deploy/helm/djinn/templates/deployment-server.yaml` via
    /// [`KubernetesConfig::from_env`].
    pub fn for_testing() -> Self {
        Self {
            namespace: "djinn".into(),
            image: "djinn-agent-runtime:dev".into(),
            image_pull_policy: "IfNotPresent".into(),
            service_account: "djinn-taskrun".into(),
            // v1 leases role-classed CPU requests: build-capable pods request a
            // full core; light (orchestration-only) pods request 300m. The CPU
            // LIMIT and both memory bounds are shared across roles ("same limits
            // everywhere"). Prod overrides cpu_request/cpu_limit/memory_* via
            // the DJINN_K8S_* envs, which are all still honored in from_env().
            cpu_request: "1".into(),
            light_cpu_request: "300m".into(),
            cpu_limit: "4".into(),
            memory_request: "2Gi".into(),
            memory_limit: "4Gi".into(),
            mirror_pvc: "djinn-mirror".into(),
            projects_pvc: "djinn-projects".into(),
            cache_pvc: "djinn-cache".into(),
            server_addr: "djinn.djinn.svc.cluster.local:8443".into(),
            warm_job_ttl_seconds: 300,
            warm_job_timeout_seconds: 7200,
            database_url: None,
            task_run_active_deadline_seconds: 10800,
            task_run_termination_grace_period_seconds: 60,
            // Request == limit: cgroup cpu.weight derives from the REQUEST, so
            // a `1` request starved the warm to ~2 effective cores under host
            // contention even with a `4` limit, timing the Rust SCIP pass out
            // on every run. Matching request to limit gives the warm a CFS
            // share proportional to its real need (graph freshness is
            // user-facing). See the field doc for the full mechanics.
            warm_cpu_request: "4".into(),
            // Bumped limit 2 → 4 so the cgroup-aware `CARGO_BUILD_JOBS` in the
            // Rust SCIP indexer has real CPU to use; a 2-CPU cap starved the
            // warm and timed the Rust workspace out on every run.
            warm_cpu_limit: "4".into(),
            // Bumped limit 4Gi → 6Gi: compiling test binaries with --all-targets
            // links more codegen units at once than clippy alone.
            warm_memory_request: "2Gi".into(),
            warm_memory_limit: "6Gi".into(),
            // The SCIP phase is 94% serial, so this request buys capacity
            // accounting (8ixk `protected_mcpu`), not throughput. 1000m sits
            // well inside the 2200m ceiling that would cost a build slot.
            scip_cpu_request: "1".into(),
            scip_cpu_limit: "2".into(),
            scip_memory_request: "4Gi".into(),
            // MEASURED peak is 10.0 GB. Stated explicitly here rather than
            // inherited from a 6Gi default plus a production override.
            scip_memory_limit: "16Gi".into(),
            scip_index_interval_seconds: 10_800,
            scip_job_ttl_seconds: 14_400,
            scip_job_timeout_seconds: 7_200,
            // OFF by default. The scheduler is wired for real regardless; this
            // is the only thing that lets it create a Job.
            scip_index_enabled: false,
            scip_quiescence_seconds: crate::scip_job::MEASURED_SCIP_PHASE_SECONDS as u64,
            // 900s. Must equal `djinn_graph::semantic_index_claim`'s own
            // default, which applies wherever this manifest is not what builds
            // the environment; `djinn_server::scip_index_watcher` asserts the
            // two agree, from the crate that can see both.
            scip_claim_wait_seconds: 900,
            node_selector: BTreeMap::new(),
            tolerations: Vec::new(),
            // v1 leases enforcement contract. Both fail render validation if set
            // to anything the launcher's runtime readiness check would reject.
            cgroup_delegation_profile: crate::launcher::CGROUP_PROFILE_V2_CPU_ONLY.into(),
            volume_ownership_mode: crate::launcher::VOLUME_OWNERSHIP_ON_ROOT_MISMATCH.into(),
            // Production task-runs require the launcher and use fresh invocation leaves.
            cgroup_launcher_mode: CgroupLauncherMode::Required,
            task_run_cgroup_writable_enabled: true,
            // Kueue arming is OFF until the cutover epic 4c9q flips it. See the
            // field doc: arming without the `djinn.io/kueue-managed` namespace
            // label hangs every build Job.
            kueue_armed: false,
            kueue_local_queue_prefix: default_kueue_local_queue_prefix(),
            // Unbounded by default: per-project build_resources overrides are
            // gated only by request <= limit until an operator configures the
            // per-kind DJINN_K8S_{TASK,WARM}_{CPU,MEMORY}_{MIN,MAX} envs.
            task_resource_bounds: crate::build_resources::ResourceBounds::default(),
            warm_resource_bounds: crate::build_resources::ResourceBounds::default(),
        }
    }

    /// Load a [`KubernetesConfig`] from environment variables, falling back
    /// to [`Self::for_testing`] values for anything unset.
    ///
    /// This is the production path: the Helm chart sets these env vars on
    /// the djinn-server Deployment (see `charts/djinn/templates/deployment.yaml`
    /// and `values.yaml`), so every field a real operator would tune is
    /// overridable without a TOML/YAML rewrite.
    ///
    /// | Env var | Field | Default |
    /// |---|---|---|
    /// | `DJINN_K8S_NAMESPACE` | `namespace` | `djinn` |
    /// | `DJINN_K8S_IMAGE` | `image` | `djinn-agent-runtime:dev` |
    /// | `DJINN_K8S_IMAGE_PULL_POLICY` | `image_pull_policy` | `IfNotPresent` |
    /// | `DJINN_K8S_SERVICE_ACCOUNT` | `service_account` | `djinn-taskrun` |
    /// | `DJINN_K8S_CPU_REQUEST` | `cpu_request` (build-capable) | `1` |
    /// | `DJINN_K8S_LIGHT_CPU_REQUEST` | `light_cpu_request` | `300m` |
    /// | `DJINN_K8S_CPU_LIMIT` | `cpu_limit` (shared) | `4` |
    /// | `DJINN_K8S_MEMORY_REQUEST` | `memory_request` (shared) | `2Gi` |
    /// | `DJINN_K8S_MEMORY_LIMIT` | `memory_limit` (shared) | `4Gi` |
    /// | `DJINN_K8S_MIRROR_PVC` | `mirror_pvc` | `djinn-mirror` |
    /// | `DJINN_K8S_CACHE_PVC` | `cache_pvc` | `djinn-cache` |
    /// | `DJINN_K8S_SERVER_ADDR` | `server_addr` | `djinn.djinn.svc.cluster.local:8443` |
    /// | `DJINN_K8S_WARM_JOB_TTL_SECONDS` | `warm_job_ttl_seconds` | `300` (parsed as `i32`) |
    /// | `DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS` | `warm_job_timeout_seconds` | `7200` (parsed as `i64`) |
    /// | `DJINN_DATABASE_URL` | `database_url` | _(unset → warm Pod has no fallback; helm chart projects this via the `djinn-server` ConfigMap)_ |
    /// | `DJINN_K8S_TASK_RUN_ACTIVE_DEADLINE_SECONDS` | `task_run_active_deadline_seconds` | `10800` (parsed as `u64`) |
    /// | `DJINN_K8S_TASK_RUN_TERMINATION_GRACE_PERIOD_SECONDS` | `task_run_termination_grace_period_seconds` | `60` (parsed as `i64`) |
    /// | `DJINN_K8S_WARM_CPU_REQUEST` | `warm_cpu_request` | `4` (== limit; cgroup cpu.weight derives from the request) |
    /// | `DJINN_K8S_WARM_CPU_LIMIT` | `warm_cpu_limit` | `4` |
    /// | `DJINN_K8S_WARM_MEMORY_REQUEST` | `warm_memory_request` | `2Gi` |
    /// | `DJINN_K8S_WARM_MEMORY_LIMIT` | `warm_memory_limit` | `6Gi` |
    /// | `DJINN_K8S_SCIP_CPU_REQUEST` | `scip_cpu_request` | `1` (≤ 2200m or the derived build-slot cap drops) |
    /// | `DJINN_K8S_SCIP_CPU_LIMIT` | `scip_cpu_limit` | `2` |
    /// | `DJINN_K8S_SCIP_MEMORY_REQUEST` | `scip_memory_request` | `4Gi` |
    /// | `DJINN_K8S_SCIP_MEMORY_LIMIT` | `scip_memory_limit` | `16Gi` (measured peak 10.0 GB) |
    /// | `DJINN_K8S_SCIP_INDEX_INTERVAL_SECONDS` | `scip_index_interval_seconds` | `10800` (parsed as `u64`) |
    /// | `DJINN_K8S_SCIP_JOB_TTL_SECONDS` | `scip_job_ttl_seconds` | `14400` (parsed as `i32`) |
    /// | `DJINN_K8S_SCIP_JOB_TIMEOUT_SECONDS` | `scip_job_timeout_seconds` | `7200` (parsed as `i64`) |
    /// | `DJINN_K8S_SCIP_INDEX_ENABLED` | `scip_index_enabled` | `false` — the arming switch |
    /// | `DJINN_K8S_SCIP_QUIESCENCE_SECONDS` | `scip_quiescence_seconds` | `3523` (the measured phase cost; `0` disables) |
    /// | `DJINN_K8S_SCIP_CLAIM_WAIT_SECONDS` | `scip_claim_wait_seconds` | `900` (warm's bounded wait for an in-flight index of the same tree; `0` disables waiting) |
    /// | `DJINN_K8S_NODE_SELECTOR` | `node_selector` | `{}` (parsed as a JSON object of string→string) |
    /// | `DJINN_K8S_TOLERATIONS` | `tolerations` | `[]` (parsed as a JSON array of k8s `Toleration` objects) |
    /// | `DJINN_K8S_CGROUP_DELEGATION_PROFILE` | `cgroup_delegation_profile` | `cgroup-v2-cpu-only` |
    /// | `DJINN_K8S_VOLUME_OWNERSHIP_MODE` | `volume_ownership_mode` | `fsgroup-on-root-mismatch` |
    /// | `DJINN_K8S_CGROUP_LAUNCHER_MODE` | `cgroup_launcher_mode` | `required` |
    /// | `DJINN_KUEUE_ARMED` | `kueue_armed` | `false` — the Kueue arming switch |
    /// | `DJINN_KUEUE_LOCAL_QUEUE_PREFIX` | `kueue_local_queue_prefix` | `djinn` |
    ///
    /// `DJINN_DATABASE_URL` is read from djinn-server's own environment (the
    /// Helm chart projects it via `envFrom: configMap djinn-config`) and
    /// is forwarded onto both the warm Pod container (so `warm-graph`
    /// talks to the same backing store) and the task-run Pod container
    /// (so the worker's `bootstrap_warm_database()` opens the same
    /// Postgres instance and helpers like `resolve_role_overrides` /
    /// `build_prompt_context` succeed mid-run).
    ///
    pub fn from_env() -> Self {
        let mut cfg = Self::for_testing();
        if let Ok(v) = std::env::var("DJINN_K8S_NAMESPACE") {
            cfg.namespace = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_IMAGE") {
            cfg.image = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_IMAGE_PULL_POLICY") {
            cfg.image_pull_policy = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SERVICE_ACCOUNT") {
            cfg.service_account = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_CPU_REQUEST") {
            cfg.cpu_request = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_LIGHT_CPU_REQUEST") {
            cfg.light_cpu_request = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_CPU_LIMIT") {
            cfg.cpu_limit = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_MEMORY_REQUEST") {
            cfg.memory_request = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_MEMORY_LIMIT") {
            cfg.memory_limit = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_MIRROR_PVC") {
            cfg.mirror_pvc = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_PROJECTS_PVC") {
            cfg.projects_pvc = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_CACHE_PVC") {
            cfg.cache_pvc = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SERVER_ADDR") {
            cfg.server_addr = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_WARM_JOB_TTL_SECONDS") {
            match v.parse::<i32>() {
                Ok(n) => cfg.warm_job_ttl_seconds = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_WARM_JOB_TTL_SECONDS not a valid i32 — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS") {
            match v.parse::<i64>() {
                Ok(n) => cfg.warm_job_timeout_seconds = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS not a valid i64 — keeping default"
                ),
            }
        }
        cfg.database_url = std::env::var("DJINN_DATABASE_URL")
            .ok()
            .filter(|v| !v.is_empty());
        if let Ok(v) = std::env::var("DJINN_K8S_TASK_RUN_ACTIVE_DEADLINE_SECONDS") {
            match v.parse::<u64>() {
                Ok(n) => cfg.task_run_active_deadline_seconds = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_TASK_RUN_ACTIVE_DEADLINE_SECONDS not a valid u64 — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_TASK_RUN_TERMINATION_GRACE_PERIOD_SECONDS") {
            match v.parse::<i64>() {
                Ok(n) => cfg.task_run_termination_grace_period_seconds = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_TASK_RUN_TERMINATION_GRACE_PERIOD_SECONDS not a valid i64 — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_WARM_CPU_REQUEST") {
            cfg.warm_cpu_request = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_WARM_CPU_LIMIT") {
            cfg.warm_cpu_limit = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_WARM_MEMORY_REQUEST") {
            cfg.warm_memory_request = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_WARM_MEMORY_LIMIT") {
            cfg.warm_memory_limit = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SCIP_CPU_REQUEST") {
            cfg.scip_cpu_request = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SCIP_CPU_LIMIT") {
            cfg.scip_cpu_limit = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SCIP_MEMORY_REQUEST") {
            cfg.scip_memory_request = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SCIP_MEMORY_LIMIT") {
            cfg.scip_memory_limit = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SCIP_INDEX_INTERVAL_SECONDS") {
            match v.parse::<u64>() {
                Ok(n) => cfg.scip_index_interval_seconds = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_SCIP_INDEX_INTERVAL_SECONDS not a valid u64 — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SCIP_JOB_TTL_SECONDS") {
            match v.parse::<i32>() {
                Ok(n) => cfg.scip_job_ttl_seconds = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_SCIP_JOB_TTL_SECONDS not a valid i32 — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SCIP_INDEX_ENABLED") {
            cfg.scip_index_enabled = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SCIP_QUIESCENCE_SECONDS") {
            match v.parse::<u64>() {
                Ok(n) => cfg.scip_quiescence_seconds = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_SCIP_QUIESCENCE_SECONDS not a valid u64 — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SCIP_CLAIM_WAIT_SECONDS") {
            match v.parse::<u64>() {
                Ok(n) => cfg.scip_claim_wait_seconds = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_SCIP_CLAIM_WAIT_SECONDS not a valid u64 — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_SCIP_JOB_TIMEOUT_SECONDS") {
            match v.parse::<i64>() {
                Ok(n) => cfg.scip_job_timeout_seconds = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_SCIP_JOB_TIMEOUT_SECONDS not a valid i64 — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_NODE_SELECTOR")
            && !v.is_empty()
        {
            match serde_json::from_str::<BTreeMap<String, String>>(&v) {
                Ok(map) => cfg.node_selector = map,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_NODE_SELECTOR not valid JSON (expected object of string→string) — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_TOLERATIONS")
            && !v.is_empty()
        {
            match serde_json::from_str::<Vec<Toleration>>(&v) {
                Ok(t) => cfg.tolerations = t,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_TOLERATIONS not valid JSON (expected array of Toleration objects) — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_CGROUP_DELEGATION_PROFILE") {
            cfg.cgroup_delegation_profile = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_VOLUME_OWNERSHIP_MODE") {
            cfg.volume_ownership_mode = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_CGROUP_LAUNCHER_MODE") {
            match CgroupLauncherMode::parse(&v) {
                Some(mode) => cfg.cgroup_launcher_mode = mode,
                // Keep the safe default rather than guessing: a typo must never
                // arm (or silently disarm) the enforcement sidecar.
                None => tracing::warn!(
                    value = %v,
                    "DJINN_K8S_CGROUP_LAUNCHER_MODE is not `disabled` or `required` — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_TASK_RUN_CGROUP_WRITABLE_ENABLED") {
            match v.parse::<bool>() {
                Ok(enabled) => cfg.task_run_cgroup_writable_enabled = enabled,
                Err(error) => {
                    tracing::warn!(value = %v, %error, "DJINN_K8S_TASK_RUN_CGROUP_WRITABLE_ENABLED is not a boolean — keeping disabled")
                }
            }
        }
        // Kueue arming. Deliberately NOT `DJINN_K8S_*`-prefixed: it is one half
        // of a chart-level contract with the `djinn.io/kueue-managed` namespace
        // label, both driven by `kueue.armed` in values.yaml.
        //
        // Env parsing AND the startup preflight latch both live in
        // `crate::kueue_preflight::kueue_armed_from_env`, because the renderers
        // are no longer the only consumer: the coordinator's build-admission
        // gate reads the same state to decide whether the in-process ledger
        // stands down (37yq). Two readers that each parsed the env for
        // themselves could disagree, and the disagreement that matters is
        // exactly the dangerous one — a ledger that stood down while the
        // preflight had disarmed the renderers, leaving nothing owning
        // capacity. One function, one answer.
        cfg.kueue_armed = crate::kueue_preflight::kueue_armed_from_env();
        if let Ok(v) = std::env::var("DJINN_KUEUE_LOCAL_QUEUE_PREFIX")
            && !v.is_empty()
        {
            cfg.kueue_local_queue_prefix = v;
        }
        // Per-project build_resources hard bounds (per Pod kind, per resource).
        // Unset leaves the axis unbounded. Empty strings are ignored so an
        // operator can clear a bound by exporting the var empty.
        let bound = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        cfg.task_resource_bounds.cpu_min = bound("DJINN_K8S_TASK_CPU_MIN");
        cfg.task_resource_bounds.cpu_max = bound("DJINN_K8S_TASK_CPU_MAX");
        cfg.task_resource_bounds.memory_min = bound("DJINN_K8S_TASK_MEMORY_MIN");
        cfg.task_resource_bounds.memory_max = bound("DJINN_K8S_TASK_MEMORY_MAX");
        cfg.warm_resource_bounds.cpu_min = bound("DJINN_K8S_WARM_CPU_MIN");
        cfg.warm_resource_bounds.cpu_max = bound("DJINN_K8S_WARM_CPU_MAX");
        cfg.warm_resource_bounds.memory_min = bound("DJINN_K8S_WARM_MEMORY_MIN");
        cfg.warm_resource_bounds.memory_max = bound("DJINN_K8S_WARM_MEMORY_MAX");
        cfg
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Serializes every test that mutates process env around `from_env()`.
    /// `pub(crate)` because `kueue_preflight`'s latch test drives `from_env()`
    /// too and has to take the SAME lock — a second mutex would serialize
    /// nothing.
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `from_env()` honors the env vars it documents.  This is a sanity
    /// check on the env-var names (regressions would silently fall back to
    /// defaults on the production path without any compile-time signal).
    #[test]
    fn from_env_reads_documented_vars() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: serialized against sibling test via ENV_LOCK; no other
        // threads in the test process read these env keys.
        unsafe {
            std::env::set_var("DJINN_K8S_NAMESPACE", "test-ns");
            std::env::set_var("DJINN_K8S_IMAGE", "repo/img:tag");
            std::env::set_var("DJINN_K8S_SERVER_ADDR", "djinn:9000");
            std::env::set_var("DJINN_K8S_PROJECTS_PVC", "owner-cache-projects-pvc");
            std::env::set_var(
                "DJINN_DATABASE_URL",
                "postgres://djinn:djinn@djinn-postgres:5432/djinn",
            );
        }
        let cfg = KubernetesConfig::from_env();
        assert_eq!(cfg.namespace, "test-ns");
        assert_eq!(cfg.image, "repo/img:tag");
        assert_eq!(cfg.server_addr, "djinn:9000");
        assert_eq!(cfg.projects_pvc, "owner-cache-projects-pvc");
        // Unset vars fall back to `for_testing` defaults.
        assert_eq!(cfg.service_account, "djinn-taskrun");
        // DB URL forwarded as-is for warm Pod env projection.
        assert_eq!(
            cfg.database_url.as_deref(),
            Some("postgres://djinn:djinn@djinn-postgres:5432/djinn")
        );

        // Reset so we don't leak into other tests that might touch
        // overlapping env keys via `from_env()`.
        unsafe {
            std::env::remove_var("DJINN_K8S_NAMESPACE");
            std::env::remove_var("DJINN_K8S_IMAGE");
            std::env::remove_var("DJINN_K8S_SERVER_ADDR");
            std::env::remove_var("DJINN_K8S_PROJECTS_PVC");
            std::env::remove_var("DJINN_DATABASE_URL");
        }
    }

    /// `from_env()` parses the JSON-encoded scheduling env vars (operators
    /// set these to pin task-run + warm Pods to a dedicated NodePool).
    #[test]
    fn from_env_parses_pod_scheduling_json() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved_ns = std::env::var("DJINN_K8S_NODE_SELECTOR").ok();
        let saved_tol = std::env::var("DJINN_K8S_TOLERATIONS").ok();
        // SAFETY: serialized against sibling tests via ENV_LOCK.
        unsafe {
            std::env::set_var("DJINN_K8S_NODE_SELECTOR", r#"{"workload-type":"djinn"}"#);
            std::env::set_var(
                "DJINN_K8S_TOLERATIONS",
                r#"[{"key":"workload-type","operator":"Equal","value":"djinn","effect":"NoSchedule"}]"#,
            );
        }
        let cfg = KubernetesConfig::from_env();
        assert_eq!(
            cfg.node_selector.get("workload-type").map(String::as_str),
            Some("djinn"),
        );
        assert_eq!(cfg.tolerations.len(), 1);
        let t = &cfg.tolerations[0];
        assert_eq!(t.key.as_deref(), Some("workload-type"));
        assert_eq!(t.operator.as_deref(), Some("Equal"));
        assert_eq!(t.value.as_deref(), Some("djinn"));
        assert_eq!(t.effect.as_deref(), Some("NoSchedule"));
        unsafe {
            match saved_ns {
                Some(prev) => std::env::set_var("DJINN_K8S_NODE_SELECTOR", prev),
                None => std::env::remove_var("DJINN_K8S_NODE_SELECTOR"),
            }
            match saved_tol {
                Some(prev) => std::env::set_var("DJINN_K8S_TOLERATIONS", prev),
                None => std::env::remove_var("DJINN_K8S_TOLERATIONS"),
            }
        }
    }

    /// The default `warm_job_timeout_seconds` must accommodate the WHOLE warm
    /// Job, not just its cargo half: the same Pod, against the same
    /// `activeDeadlineSeconds`, then runs SCIP indexing and graph publication.
    ///
    /// The old guard asserted `>= 3600` on the strength of the cargo phase
    /// alone (~25 min cold for a ~12-crate workspace). A complete production
    /// warm measured on 2026-07-27 needed **5442s end to end** — 1798s of cargo
    /// and 3644s of graph phase — so every warm of that workspace on the
    /// shipped default was SIGKILLed at 60 minutes with no graph published.
    /// The floor is therefore the measured requirement, not the cargo estimate.
    #[test]
    fn warm_job_timeout_default_accommodates_the_whole_warm_job() {
        let cfg = KubernetesConfig::for_testing();
        assert!(
            cfg.warm_job_timeout_seconds >= MEASURED_FULL_WARM_SECONDS,
            "default warm_job_timeout_seconds is {} but must be >= {} — the \
             measured cargo + SCIP + publish cost of one real warm Job. Below \
             that the Pod is SIGKILLed mid-SCIP and publishes no server index.",
            cfg.warm_job_timeout_seconds,
            MEASURED_FULL_WARM_SECONDS,
        );
    }
}
