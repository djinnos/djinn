//! Kubernetes-backed `SessionRuntime` — PR 1 scaffold.
//!
//! The crate delivers `KubernetesRuntime`, a `SessionRuntime` impl that
//! dispatches per-task-run work as K8s `Job`s. PR 1 lands the module layout,
//! typed configuration, and empty trait-impl shell — real cluster wiring
//! arrives in PR 3.

pub mod config;
pub mod env_config;
pub mod graph_warmer;
pub mod graph_warmer_candidates;
pub mod graph_warmer_identity;
pub mod infra_death_log_tail;
pub mod job;
pub mod label_value;
pub mod launcher;
pub mod runtime;
pub mod secret;
pub mod sidecar;
pub mod token_review;
pub mod warm_job;
pub mod workload_inventory;

pub use config::KubernetesConfig;
pub use env_config::{
    ENV_CONFIG_KEY, ENV_CONFIG_MOUNT_DIR, ENV_CONFIG_MOUNT_FILE, VOLUME_ENV_CONFIG,
    build_env_config_config_map, env_config_config_map_name, env_config_volume,
    env_config_volume_mount,
};
pub use graph_warmer::{
    GraphWarmLease, GraphWarmLeaseError, GraphWarmLeaseGrant, GraphWarmLeaseRecovery,
    K8sGraphWarmer, KubeClientDispatcher, KubeClientJobWatcher, KubeClientWarmJobLister,
    NoopJobWatcher, NoopWarmJobLister, WarmAdmission, WarmAdmissionError, WarmAdmissionPermit,
    WarmAdmissionRequest, WarmAdmissionTransition, WarmCompletionSink, WarmJobDispatcher,
    WarmJobLister, WarmJobManifest, WarmJobWatcher, WarmTerminalOutcome,
};
pub use graph_warmer_candidates::{
    CleanupObservation, GateObservation, KubeWarmCandidateClient, WarmAnnotationValidation,
    WarmCandidate, WarmCandidateClient, WarmCandidateControl, WarmCandidateInventory,
    WarmCandidateKind, WarmCandidateObject, WarmCandidateSet, WarmCandidateSetState,
    WarmInventoryObservation,
};
pub use graph_warmer_identity::{LeasedWarmJobIdentity, warm_work_id};
pub use runtime::KubernetesRuntime;
pub use token_review::TokenReviewer;
pub use warm_job::{build_leased_warm_job, build_warm_job};
pub use workload_inventory::{
    KubeWorkloadInventory, LABEL_ADMISSION_DOMAIN, LABEL_ADMISSION_GENERATION,
    LABEL_ADMISSION_WORK_ID, UidGetResult, WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
    has_canonical_warm_signature,
};

/// Re-exported `kube::Client` type so non-owner callers can name the
/// Kubernetes client type without adding a direct `kube` dependency.
///
/// This is the standard kube-rs client — use [`try_default_client`] to
/// construct one from the ambient environment.
pub type KubeClient = kube::Client;

/// Construct a [`KubeClient`] from the ambient Kubernetes environment
/// (in-cluster service-account token or `$KUBECONFIG`).
///
/// Returns `Err` when no cluster is reachable (dev boxes, CI without a
/// kind cluster, etc.).  Callers should treat the error as a signal to
/// fall back gracefully rather than propagate.
pub async fn try_default_client() -> Result<KubeClient, kube::Error> {
    kube::Client::try_default().await
}
