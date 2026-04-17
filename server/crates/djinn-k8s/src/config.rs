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
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `from_env()` honors the env vars it documents.  This is a sanity
    /// check on the env-var names (regressions would silently fall back to
    /// defaults on the production path without any compile-time signal).
    #[test]
    fn from_env_reads_documented_vars() {
        // SAFETY: single-threaded unit test, no other threads read env.
        unsafe {
            std::env::set_var("DJINN_K8S_NAMESPACE", "test-ns");
            std::env::set_var("DJINN_K8S_IMAGE", "repo/img:tag");
            std::env::set_var("DJINN_K8S_SERVER_ADDR", "djinn:9000");
            std::env::set_var("DJINN_K8S_TTL_SECONDS", "600");
        }
        let cfg = KubernetesConfig::from_env();
        assert_eq!(cfg.namespace, "test-ns");
        assert_eq!(cfg.image, "repo/img:tag");
        assert_eq!(cfg.server_addr, "djinn:9000");
        assert_eq!(cfg.ttl_seconds_after_finished, 600);
        // Unset vars fall back to `for_testing` defaults.
        assert_eq!(cfg.service_account, "djinn-taskrun");

        // Reset so we don't leak into other tests that might touch
        // overlapping env keys via `from_env()`.
        unsafe {
            std::env::remove_var("DJINN_K8S_NAMESPACE");
            std::env::remove_var("DJINN_K8S_IMAGE");
            std::env::remove_var("DJINN_K8S_SERVER_ADDR");
            std::env::remove_var("DJINN_K8S_TTL_SECONDS");
        }
    }

    /// A malformed `DJINN_K8S_TTL_SECONDS` falls back to the default —
    /// the runtime should still boot instead of crashing the Helm rollout
    /// if an operator typos the value.
    #[test]
    fn from_env_ttl_parse_error_falls_back_to_default() {
        // SAFETY: single-threaded unit test; we save + restore the key.
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
