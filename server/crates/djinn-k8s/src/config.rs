//! Runtime configuration shared by every `djinn-k8s` helper.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Toleration;
use serde::{Deserialize, Serialize};

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
    /// CPU request (e.g. `"2"`).
    pub cpu_request: String,
    /// CPU limit (e.g. `"2"`).
    pub cpu_limit: String,
    /// Memory request (e.g. `"4Gi"`).
    pub memory_request: String,
    /// Memory limit (e.g. `"4Gi"`).
    pub memory_limit: String,
    /// TTL (seconds) applied to completed Jobs for auto-GC.
    pub ttl_seconds_after_finished: i32,
    /// RWX PVC backing the task-run mirror (mounted read-only at `/mirror`).
    pub mirror_pvc: String,
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
    /// Default 3600s (60 minutes). The warm Pod runs a single default-features
    /// cargo pass (clippy + build + test-compile) matching the worker's feature
    /// set. From Phase 0 data: a default-features re-warm took ~10 minutes,
    /// and a cold first warm for a ~12-crate workspace took ~20-25 minutes.
    /// 3600s leaves ample margin for the single-pass worst case. Warm Jobs
    /// run in the background and don't affect worker latency, so a generous
    /// deadline is safe. Tunable per deployment via
    /// `DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS`; raise it if a larger workspace
    /// consistently hits the deadline.
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
}

impl KubernetesConfig {
    /// Minimal default used by unit tests; production deployments load
    /// from the djinn-server config file.
    pub fn for_testing() -> Self {
        Self {
            namespace: "djinn".into(),
            image: "djinn-agent-runtime:dev".into(),
            image_pull_policy: "IfNotPresent".into(),
            service_account: "djinn-taskrun".into(),
            cpu_request: "2".into(),
            cpu_limit: "2".into(),
            memory_request: "4Gi".into(),
            memory_limit: "4Gi".into(),
            ttl_seconds_after_finished: 300,
            mirror_pvc: "djinn-mirror".into(),
            cache_pvc: "djinn-cache".into(),
            server_addr: "djinn.djinn.svc.cluster.local:8443".into(),
            warm_job_ttl_seconds: 300,
            warm_job_timeout_seconds: 3600,
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
            node_selector: BTreeMap::new(),
            tolerations: Vec::new(),
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
    /// | `DJINN_K8S_CPU_REQUEST` | `cpu_request` | `2` |
    /// | `DJINN_K8S_CPU_LIMIT` | `cpu_limit` | `2` |
    /// | `DJINN_K8S_MEMORY_REQUEST` | `memory_request` | `4Gi` |
    /// | `DJINN_K8S_MEMORY_LIMIT` | `memory_limit` | `4Gi` |
    /// | `DJINN_K8S_TTL_SECONDS` | `ttl_seconds_after_finished` | `300` (parsed as `i32`) |
    /// | `DJINN_K8S_MIRROR_PVC` | `mirror_pvc` | `djinn-mirror` |
    /// | `DJINN_K8S_CACHE_PVC` | `cache_pvc` | `djinn-cache` |
    /// | `DJINN_K8S_SERVER_ADDR` | `server_addr` | `djinn.djinn.svc.cluster.local:8443` |
    /// | `DJINN_K8S_WARM_JOB_TTL_SECONDS` | `warm_job_ttl_seconds` | `300` (parsed as `i32`) |
    /// | `DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS` | `warm_job_timeout_seconds` | `3600` (parsed as `i64`) |
    /// | `DJINN_DATABASE_URL` | `database_url` | _(unset → warm Pod has no fallback; helm chart projects this via the `djinn-server` ConfigMap)_ |
    /// | `DJINN_K8S_TASK_RUN_ACTIVE_DEADLINE_SECONDS` | `task_run_active_deadline_seconds` | `10800` (parsed as `u64`) |
    /// | `DJINN_K8S_TASK_RUN_TERMINATION_GRACE_PERIOD_SECONDS` | `task_run_termination_grace_period_seconds` | `60` (parsed as `i64`) |
    /// | `DJINN_K8S_WARM_CPU_REQUEST` | `warm_cpu_request` | `4` (== limit; cgroup cpu.weight derives from the request) |
    /// | `DJINN_K8S_WARM_CPU_LIMIT` | `warm_cpu_limit` | `4` |
    /// | `DJINN_K8S_WARM_MEMORY_REQUEST` | `warm_memory_request` | `2Gi` |
    /// | `DJINN_K8S_WARM_MEMORY_LIMIT` | `warm_memory_limit` | `6Gi` |
    /// | `DJINN_K8S_NODE_SELECTOR` | `node_selector` | `{}` (parsed as a JSON object of string→string) |
    /// | `DJINN_K8S_TOLERATIONS` | `tolerations` | `[]` (parsed as a JSON array of k8s `Toleration` objects) |
    ///
    /// `DJINN_DATABASE_URL` is read from djinn-server's own environment (the
    /// Helm chart projects it via `envFrom: configMap djinn-config`) and
    /// is forwarded onto both the warm Pod container (so `warm-graph`
    /// talks to the same backing store) and the task-run Pod container
    /// (so the worker's `bootstrap_warm_database()` opens the same
    /// Postgres instance and helpers like `resolve_role_overrides` /
    /// `build_prompt_context` succeed mid-run).
    ///
    /// A malformed `DJINN_K8S_TTL_SECONDS` is logged at `warn` and falls
    /// back to the default — the runtime still boots.
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
        if let Ok(v) = std::env::var("DJINN_K8S_CPU_LIMIT") {
            cfg.cpu_limit = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_MEMORY_REQUEST") {
            cfg.memory_request = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_MEMORY_LIMIT") {
            cfg.memory_limit = v;
        }
        if let Ok(v) = std::env::var("DJINN_K8S_TTL_SECONDS") {
            match v.parse::<i32>() {
                Ok(n) => cfg.ttl_seconds_after_finished = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_K8S_TTL_SECONDS not a valid i32 — keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var("DJINN_K8S_MIRROR_PVC") {
            cfg.mirror_pvc = v;
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
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both tests in this module mutate the same `DJINN_K8S_TTL_SECONDS`
    // env var. `cargo test` runs tests in parallel threads within one
    // process, so without a lock the two races: one test's set_var/
    // remove_var can clobber the other's between set and from_env().
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
            std::env::set_var("DJINN_K8S_TTL_SECONDS", "600");
            std::env::set_var(
                "DJINN_DATABASE_URL",
                "postgres://djinn:djinn@djinn-postgres:5432/djinn",
            );
        }
        let cfg = KubernetesConfig::from_env();
        assert_eq!(cfg.namespace, "test-ns");
        assert_eq!(cfg.image, "repo/img:tag");
        assert_eq!(cfg.server_addr, "djinn:9000");
        assert_eq!(cfg.ttl_seconds_after_finished, 600);
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
            std::env::remove_var("DJINN_K8S_TTL_SECONDS");
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

    /// A malformed `DJINN_K8S_TTL_SECONDS` falls back to the default —
    /// the runtime should still boot instead of crashing the Helm rollout
    /// if an operator typos the value.
    #[test]
    fn from_env_ttl_parse_error_falls_back_to_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: serialized against sibling test via ENV_LOCK; we save +
        // restore the key so a concurrent `cargo test` run can't observe
        // the transient `not-a-number` state.
        let saved = std::env::var("DJINN_K8S_TTL_SECONDS").ok();
        unsafe {
            std::env::set_var("DJINN_K8S_TTL_SECONDS", "not-a-number");
        }
        let cfg = KubernetesConfig::from_env();
        assert_eq!(
            cfg.ttl_seconds_after_finished,
            KubernetesConfig::for_testing().ttl_seconds_after_finished
        );
        unsafe {
            match saved {
                Some(prev) => std::env::set_var("DJINN_K8S_TTL_SECONDS", prev),
                None => std::env::remove_var("DJINN_K8S_TTL_SECONDS"),
            }
        }
    }

    /// The default `warm_job_timeout_seconds` must accommodate a single
    /// default-features warm pass: clippy + build fallback + test-compile
    /// (`nextest run --no-run` / `cargo test --no-run`). The worst cold case
    /// for a ~12-crate workspace is ~25 minutes; 3600s (60 min) leaves ample
    /// margin. This regression guard fails loudly if the default is reduced
    /// below 3600.
    #[test]
    fn warm_job_timeout_default_accommodates_single_pass_warm() {
        let cfg = KubernetesConfig::for_testing();
        assert!(
            cfg.warm_job_timeout_seconds >= 3600,
            "default warm_job_timeout_seconds is {} but must be >= 3600 \
             (60 min) to cover a cold single-pass + test-compile warm with margin",
            cfg.warm_job_timeout_seconds,
        );
    }
}
