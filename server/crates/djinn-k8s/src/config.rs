//! Runtime configuration shared by every `djinn-k8s` helper.

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
    pub warm_job_timeout_seconds: i64,
    /// Database DSN forwarded to the warm Pod so `djinn-agent-worker
    /// warm-graph` can reuse the server's backing Dolt/MySQL instance.
    /// `None` leaves the warm binary to fall back to its built-in default
    /// (`mysql://root@127.0.0.1:3306/djinn`), which only works in single-
    /// process local test setups.
    pub database_url: Option<String>,
    /// Matches `DJINN_DB_BACKEND` on djinn-server (`mysql` | `dolt`).
    /// Forwarded so the warm Pod bootstraps an identical `Database` pool.
    pub database_backend: Option<String>,
    /// Matches `DJINN_MYSQL_FLAVOR` on djinn-server. Usually equal to
    /// `database_backend`; kept distinct because the warm binary accepts
    /// independent values (e.g. talking MySQL protocol to a Dolt server).
    pub database_flavor: Option<String>,
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
            warm_job_timeout_seconds: 1800,
            database_url: None,
            database_backend: None,
            database_flavor: None,
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
    /// | `DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS` | `warm_job_timeout_seconds` | `1800` (parsed as `i64`) |
    /// | `DJINN_MYSQL_URL` | `database_url` | _(unset → warm Pod uses default `mysql://root@127.0.0.1:3306/djinn`)_ |
    /// | `DJINN_DB_BACKEND` | `database_backend` | _(unset)_ |
    /// | `DJINN_MYSQL_FLAVOR` | `database_flavor` | _(unset)_ |
    ///
    /// The three DB vars are read from djinn-server's own environment (the
    /// Helm chart projects them via `envFrom: configMap djinn-config`) and
    /// are forwarded onto the warm Pod container so `warm-graph` talks to
    /// the same backing store. Task-run Pods don't need them — they speak
    /// to djinn-server over RPC, not the DB directly.
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
        cfg.database_url = std::env::var("DJINN_MYSQL_URL")
            .ok()
            .filter(|v| !v.is_empty());
        cfg.database_backend = std::env::var("DJINN_DB_BACKEND")
            .ok()
            .filter(|v| !v.is_empty());
        cfg.database_flavor = std::env::var("DJINN_MYSQL_FLAVOR")
            .ok()
            .filter(|v| !v.is_empty());
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
                "DJINN_MYSQL_URL",
                "mysql://root@djinn-dolt:3306/djinn",
            );
            std::env::set_var("DJINN_DB_BACKEND", "dolt");
            std::env::set_var("DJINN_MYSQL_FLAVOR", "dolt");
        }
        let cfg = KubernetesConfig::from_env();
        assert_eq!(cfg.namespace, "test-ns");
        assert_eq!(cfg.image, "repo/img:tag");
        assert_eq!(cfg.server_addr, "djinn:9000");
        assert_eq!(cfg.ttl_seconds_after_finished, 600);
        // Unset vars fall back to `for_testing` defaults.
        assert_eq!(cfg.service_account, "djinn-taskrun");
        // DB vars forwarded as-is for warm Pod env projection.
        assert_eq!(
            cfg.database_url.as_deref(),
            Some("mysql://root@djinn-dolt:3306/djinn")
        );
        assert_eq!(cfg.database_backend.as_deref(), Some("dolt"));
        assert_eq!(cfg.database_flavor.as_deref(), Some("dolt"));

        // Reset so we don't leak into other tests that might touch
        // overlapping env keys via `from_env()`.
        unsafe {
            std::env::remove_var("DJINN_K8S_NAMESPACE");
            std::env::remove_var("DJINN_K8S_IMAGE");
            std::env::remove_var("DJINN_K8S_SERVER_ADDR");
            std::env::remove_var("DJINN_K8S_TTL_SECONDS");
            std::env::remove_var("DJINN_MYSQL_URL");
            std::env::remove_var("DJINN_DB_BACKEND");
            std::env::remove_var("DJINN_MYSQL_FLAVOR");
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
}
