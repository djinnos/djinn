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
    /// RWX PVC backing the cargo registry/git caches, mounted at `/cache/cargo`.
    pub cache_cargo_pvc: String,
    /// RWX PVC backing the pnpm store, mounted at `/cache/pnpm`.
    pub cache_pnpm_pvc: String,
    /// RWX PVC backing the pip wheel cache, mounted at `/cache/pip`.
    pub cache_pip_pvc: String,
    /// DNS address of the djinn-server RPC listener
    /// (e.g. `djinn.djinn-system.svc.cluster.local:8443`). Worker dials this.
    pub server_addr: String,
    /// Value forwarded into the worker container as `RUST_LOG`.
    pub rust_log: String,
}

impl KubernetesConfig {
    /// Name of the per-task-run Secret holding the bincode-encoded
    /// [`djinn_runtime::TaskRunSpec`] at key `spec.bin`.
    pub fn secret_name_for(&self, task_run_id: &str) -> String {
        format!("djinn-taskrun-{task_run_id}")
    }

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
            cache_cargo_pvc: "djinn-cache-cargo".into(),
            cache_pnpm_pvc: "djinn-cache-pnpm".into(),
            cache_pip_pvc: "djinn-cache-pip".into(),
            server_addr: "djinn.djinn.svc.cluster.local:8443".into(),
            rust_log: "info,djinn=debug".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_name_for_prefixes_task_run_id() {
        let cfg = KubernetesConfig::for_testing();
        assert_eq!(
            cfg.secret_name_for("01HF0VZ2NKEXAMPLE"),
            "djinn-taskrun-01HF0VZ2NKEXAMPLE"
        );
    }
}
