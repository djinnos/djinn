//! Per-task-run `Secret` manifest builder + owner-reference helper.
//!
//! The Secret carries exactly one key, `spec.bin`, holding a bincode-encoded
//! [`djinn_runtime::TaskRunSpec`]. The worker container reads it from a
//! read-only mount (see `job.rs`). PR 3 cross-links the Secret's
//! `ownerReferences` back at the Job so kubernetes GCs the Secret together
//! with its Job.
//!
//! See `plans/phase2-k8s-scaffolding.md` ("Spec delivery swap: stdin →
//! mounted file") for why we stopped piping the spec over stdin.

use std::collections::BTreeMap;

use djinn_runtime::TaskRunSpec;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use thiserror::Error;
use uuid::Uuid;

/// Failure modes surfaced by [`build_task_run_secret`].
#[derive(Debug, Error)]
pub enum SecretError {
    /// The `TaskRunSpec` could not be encoded via bincode — almost always a
    /// serde schema mismatch rather than an IO condition.
    #[error("bincode serialize failed: {0}")]
    Serialize(#[from] bincode::Error),
}

/// Label key for the task-run id (Djinn's primary correlator on the Secret).
pub const LABEL_TASK_RUN_ID: &str = "djinn.app/task-run-id";
/// Label key identifying which djinn-internal component created the Secret.
pub const LABEL_COMPONENT: &str = "djinn.app/component";
/// Key under which the bincode-encoded `TaskRunSpec` is stored inside
/// `Secret.data`. Must match the filename the Job mounts at
/// `/var/run/djinn/spec.bin`.
pub const SPEC_DATA_KEY: &str = "spec.bin";

/// Build the per-task-run `Secret` that carries the bincode-serialized
/// [`TaskRunSpec`] on its `spec.bin` key.
///
/// The Secret name mirrors the Job name (`djinn-taskrun-{task_run_id}`) so
/// the Job manifest can reference it by construction without a round-trip.
/// Returned manifest is not yet applied to the cluster — callers pass it to
/// `kube::Api::<Secret>::create` (or equivalent) in PR 3.
pub fn build_task_run_secret(
    namespace: &str,
    task_run_id: &Uuid,
    spec: &TaskRunSpec,
) -> Result<Secret, SecretError> {
    let encoded = bincode::serialize(spec)?;

    let mut data = BTreeMap::new();
    data.insert(SPEC_DATA_KEY.to_string(), ByteString(encoded));

    let mut labels = BTreeMap::new();
    labels.insert(LABEL_TASK_RUN_ID.to_string(), task_run_id.to_string());
    labels.insert(LABEL_COMPONENT.to_string(), "task-run-spec".to_string());

    Ok(Secret {
        metadata: ObjectMeta {
            name: Some(task_run_resource_name(task_run_id)),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        type_: Some("Opaque".to_string()),
        data: Some(data),
        ..Default::default()
    })
}

/// Helper: name used for both the `Secret` and the `Job` in this PR.
///
/// Both resources share the name so the Job manifest can reference the
/// Secret without a round-trip to the API server.
pub fn task_run_resource_name(task_run_id: &Uuid) -> String {
    format!("djinn-taskrun-{task_run_id}")
}

/// Helper that produces an `OwnerReference` pointing at a parent `Job` so
/// the Secret GCs when the Job is deleted. Called by PR 3 once the Job UID
/// is known — we can't build this inside [`build_task_run_secret`] because
/// the Job doesn't exist yet at Secret-build time.
pub fn job_owner_reference(job_name: &str, job_uid: &str) -> OwnerReference {
    OwnerReference {
        api_version: "batch/v1".to_string(),
        kind: "Job".to_string(),
        name: job_name.to_string(),
        uid: job_uid.to_string(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use djinn_core::models::TaskRunTrigger;
    use djinn_runtime::{SupervisorFlow, TaskRunSpec};

    #[test]
    fn task_run_secret_roundtrips_bincoded_spec() {
        // Shape mirrors djinn_runtime::spec::tests::task_run_spec_bincode_roundtrip.
        let spec = TaskRunSpec {
            task_id: "task-abc".to_string(),
            project_id: "proj-xyz".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-abc".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
        };

        let task_run_id = Uuid::now_v7();
        let secret = build_task_run_secret("djinn", &task_run_id, &spec)
            .expect("build per-task-run Secret");

        // Name: matches task_run_resource_name() and starts with the prefix.
        let name = secret
            .metadata
            .name
            .as_deref()
            .expect("metadata.name present");
        assert!(
            name.starts_with("djinn-taskrun-"),
            "unexpected Secret name: {name}"
        );
        assert_eq!(name, task_run_resource_name(&task_run_id));

        assert_eq!(secret.metadata.namespace, Some("djinn".to_string()));
        assert_eq!(secret.type_.as_deref(), Some("Opaque"));

        // Labels: task-run-id and component are present.
        let labels = secret.metadata.labels.as_ref().expect("labels present");
        assert_eq!(
            labels.get(LABEL_TASK_RUN_ID).map(String::as_str),
            Some(task_run_id.to_string().as_str())
        );
        assert_eq!(
            labels.get(LABEL_COMPONENT).map(String::as_str),
            Some("task-run-spec")
        );

        // Payload: `spec.bin` key carries a bincode-encoded TaskRunSpec.
        let data = secret.data.as_ref().expect("data present");
        assert!(data.contains_key(SPEC_DATA_KEY));

        let payload_bytes = &data.get(SPEC_DATA_KEY).expect("spec.bin entry").0;
        let round_trip: TaskRunSpec =
            bincode::deserialize(payload_bytes).expect("deserialize TaskRunSpec");
        assert_eq!(round_trip.task_id, spec.task_id);
        assert_eq!(round_trip.project_id, spec.project_id);
        assert_eq!(round_trip.trigger, spec.trigger);
        assert_eq!(round_trip.base_branch, spec.base_branch);
        assert_eq!(round_trip.task_branch, spec.task_branch);
        assert_eq!(round_trip.flow, spec.flow);
        assert_eq!(round_trip.model_id_per_role, spec.model_id_per_role);

        // Owner reference helper produces the right parent pointer.
        let owner = job_owner_reference("djinn-taskrun-test", "uid-123");
        assert_eq!(owner.kind, "Job");
        assert_eq!(owner.api_version, "batch/v1");
        assert_eq!(owner.name, "djinn-taskrun-test");
        assert_eq!(owner.uid, "uid-123");
        assert_eq!(owner.controller, Some(true));
        assert_eq!(owner.block_owner_deletion, Some(true));
    }
}
