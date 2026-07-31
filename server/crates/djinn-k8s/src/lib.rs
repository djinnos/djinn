//! Kubernetes-backed `SessionRuntime` — PR 1 scaffold.
//!
//! The crate delivers `KubernetesRuntime`, a `SessionRuntime` impl that
//! dispatches per-task-run work as K8s `Job`s. PR 1 lands the module layout,
//! typed configuration, and empty trait-impl shell — real cluster wiring
//! arrives in PR 3.

pub mod build_resources;
pub mod config;
pub mod env_config;
pub mod graph_warmer;
pub mod graph_warmer_candidates;
pub mod graph_warmer_identity;
pub mod infra_death_log_tail;
pub mod invocation_journal;
pub mod job;
pub mod kueue_preflight;
pub mod label_value;
pub mod launcher;
pub mod launcher_child_fs;
mod launcher_cpu;
pub mod pod_resize;
pub mod private_dep_config;
pub mod runtime;
mod runtime_eviction;
pub mod scip_job;
pub mod scip_schedule;
pub mod secret;
pub mod sidecar;
pub mod token_review;
pub mod warm_job;
pub mod workload_inventory;

pub use build_resources::{
    ResolveError, ResourceBounds, apply_resolved_resources, resolve_task_run_resources,
    resolve_warm_resources,
};
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
    api_error_is_already_exists,
};
pub use graph_warmer_candidates::{
    CleanupObservation, GateObservation, KubeWarmCandidateClient, WarmAnnotationValidation,
    WarmCandidate, WarmCandidateClient, WarmCandidateControl, WarmCandidateInventory,
    WarmCandidateKind, WarmCandidateObject, WarmCandidateSet, WarmCandidateSetState,
    WarmInventoryObservation, WarmObjectLifecycle,
};
pub use graph_warmer_identity::{LeasedWarmJobIdentity, deterministic_warm_job_name, warm_work_id};
pub use kueue_preflight::{
    KueuePreflightOutcome, LABEL_KUEUE_MANAGED, NamespaceKueueStatus,
    classify_labels as classify_kueue_namespace_labels, decide as decide_kueue_preflight,
    disarm_kueue_globally, kueue_armed_from_env, kueue_disarmed_by_preflight,
    observe_namespace as observe_kueue_namespace, run as run_kueue_preflight,
};
pub use runtime::{KubernetesRuntime, taskrun_job_name};
pub use scip_job::{
    ANNOTATION_SCIP_REVISION, COMPONENT_SCIP_INDEX, LABEL_CAPACITY_RESERVED, LABEL_SCIP_INDEX,
    MEASURED_SCIP_PEAK_MEMORY_BYTES, SCIP_PROTECTED_REQUEST_CEILING_MILLICORES,
    build_scip_index_job, scip_index_job_name,
};
pub use scip_schedule::{
    KubeClientScipJobInventory, ScipIndexDecision, ScipIndexScheduler, ScipJobInventory,
    ScipJobObservation, decide as decide_scip_index, observe_from_jobs,
};
pub use token_review::TokenReviewer;
pub use warm_job::{build_leased_warm_job, build_warm_job, warm_job_name};
pub use workload_inventory::{
    KubeWorkloadInventory, LABEL_ADMISSION_DOMAIN, LABEL_ADMISSION_GENERATION,
    LABEL_ADMISSION_WORK_ID, ObjectPresence, UidGetResult, WorkloadInventory, WorkloadObjectKind,
    WorkloadRecord, has_canonical_warm_signature,
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
