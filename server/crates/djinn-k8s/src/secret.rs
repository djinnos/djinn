//! Per-task-run `Secret` manifest builder + owner-reference helper.
//!
//! The Secret carries up to four keys:
//!
//! - `spec.bin` — bincode-encoded [`djinn_runtime::TaskRunSpec`].
//! - `credentials.bin` — bincode-encoded
//!   [`djinn_runtime::ResolvedCredentials`] (per-role LLM provider
//!   credentials resolved host-side at dispatch).
//! - `environment.json` — UTF-8 JSON effective `EnvironmentConfig` (see
//!   [`ENV_CONFIG_SECRET_DATA_KEY`]).  Absent when no config is provided;
//!   defaults to [`EnvironmentConfig::empty()`].
//! - `service_metadata.json` — UTF-8 JSON resolved service metadata
//!   (see [`SERVICE_METADATA_SECRET_DATA_KEY`]).  Absent when no
//!   services are resolved.
//!
//! The worker container reads both from the same read-only mount (see
//! `job.rs`). PR 3 cross-links the Secret's `ownerReferences` back at the
//! Job so kubernetes GCs the Secret together with its Job.
//!
//! See `plans/phase2-k8s-scaffolding.md` ("Spec delivery swap: stdin →
//! mounted file") for why we stopped piping the spec over stdin, and
//! `plans/phase7a-design-secret-mount-creds.md` for the credentials hop.

use std::collections::BTreeMap;

use djinn_runtime::{ResolvedCredentials, TaskRunSpec};
use djinn_stack::environment::EnvironmentConfig;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use thiserror::Error;
use uuid::Uuid;

use crate::env_config::{
    ENV_CONFIG_SECRET_DATA_KEY, SERVICE_METADATA_SECRET_DATA_KEY,
    default_environment_config_json_bytes,
};

/// Failure modes surfaced by [`build_task_run_secret`] and
/// [`TaskRunSecretBuilder::build`].
#[derive(Debug, Error)]
pub enum SecretError {
    /// The `TaskRunSpec` could not be encoded via bincode — almost always a
    /// serde schema mismatch rather than an IO condition.
    #[error("bincode serialize failed: {0}")]
    Serialize(#[from] bincode::Error),
    /// The service metadata JSON could not be serialized.
    #[error("service metadata JSON serialize failed: {0}")]
    ServiceMetadataJson(#[from] serde_json::Error),
}

/// Label key for the task-run id (Djinn's primary correlator on the Secret).
pub const LABEL_TASK_RUN_ID: &str = "djinn.app/task-run-id";
/// Label key identifying which djinn-internal component created the Secret.
pub const LABEL_COMPONENT: &str = "djinn.app/component";
/// Key under which the bincode-encoded `TaskRunSpec` is stored inside
/// `Secret.data`. Must match the filename the Job mounts at
/// `/var/run/djinn/spec.bin`.
pub const SPEC_DATA_KEY: &str = "spec.bin";

/// Key under which the bincode-encoded [`ResolvedCredentials`] are stored
/// inside `Secret.data`. Must match the filename the Job mounts at
/// `/var/run/djinn/credentials.bin` (see `job.rs::CREDENTIALS_MOUNT_FILE`).
/// Phase 7a — worker reads + logs the role keys at startup; full provider
/// construction lands in Phase 7b.
pub const CREDENTIALS_DATA_KEY: &str = "credentials.bin";

/// Builder for a per-task-run `Secret` manifest.
///
/// Carries the mandatory `spec.bin` + `credentials.bin` payloads (Phase 7a)
/// and optionally includes the effective `EnvironmentConfig` JSON and
/// resolved service metadata JSON (hgd0 Wave 1).
///
/// ```ignore
/// let secret = TaskRunSecretBuilder::new("djinn", &task_run_id, &spec, &credentials)
///     .environment_config(&cfg)
///     .service_metadata(&resolution)?
///     .build()?;
/// ```
pub struct TaskRunSecretBuilder<'a> {
    namespace: &'a str,
    task_run_id: &'a Uuid,
    spec: &'a TaskRunSpec,
    credentials: &'a ResolvedCredentials,
    environment_config_json: Option<Vec<u8>>,
    service_metadata_json: Option<Vec<u8>>,
}

impl<'a> TaskRunSecretBuilder<'a> {
    /// Create a new builder with the mandatory spec and credentials.
    pub fn new(
        namespace: &'a str,
        task_run_id: &'a Uuid,
        spec: &'a TaskRunSpec,
        credentials: &'a ResolvedCredentials,
    ) -> Self {
        Self {
            namespace,
            task_run_id,
            spec,
            credentials,
            environment_config_json: None,
            service_metadata_json: None,
        }
    }

    /// Include the effective `EnvironmentConfig` as a JSON payload.
    ///
    /// The JSON is serialized immediately and stored as UTF-8 bytes.
    /// Callers typically pass a project-specific config; to use the
    /// default (empty pre-task list), call
    /// [`with_default_environment_config`](Self::with_default_environment_config).
    pub fn environment_config(mut self, config: &EnvironmentConfig) -> Self {
        // Serialization of a valid EnvironmentConfig is infallible in
        // practice; serde_json::to_vec only fails on non-UTF-8 map keys
        // or similar pathologies that EnvironmentConfig never exhibits.
        self.environment_config_json =
            Some(serde_json::to_vec(config).expect("EnvironmentConfig serialization"));
        self
    }

    /// Include the default `EnvironmentConfig::empty()` as the JSON
    /// payload.  Used when no project-level config exists.
    pub fn with_default_environment_config(mut self) -> Self {
        self.environment_config_json = Some(default_environment_config_json_bytes());
        self
    }

    /// Include the resolved service metadata as a JSON payload.
    ///
    /// The value must implement [`Serialize`](serde::Serialize); the
    /// canonical type is
    /// [`ImageServiceResolution`](crate::sidecar::ImageServiceResolution).
    pub fn service_metadata<T: serde::Serialize>(
        mut self,
        metadata: &T,
    ) -> Result<Self, SecretError> {
        self.service_metadata_json = Some(serde_json::to_vec(metadata)?);
        Ok(self)
    }

    /// Build the final `Secret` manifest.
    pub fn build(self) -> Result<Secret, SecretError> {
        let encoded_spec = bincode::serialize(self.spec)?;
        let encoded_credentials = bincode::serialize(self.credentials)?;

        let mut data = BTreeMap::new();
        data.insert(SPEC_DATA_KEY.to_string(), ByteString(encoded_spec));
        data.insert(
            CREDENTIALS_DATA_KEY.to_string(),
            ByteString(encoded_credentials),
        );

        if let Some(env_json) = self.environment_config_json {
            data.insert(ENV_CONFIG_SECRET_DATA_KEY.to_string(), ByteString(env_json));
        }

        if let Some(meta_json) = self.service_metadata_json {
            data.insert(
                SERVICE_METADATA_SECRET_DATA_KEY.to_string(),
                ByteString(meta_json),
            );
        }

        let mut labels = BTreeMap::new();
        labels.insert(LABEL_TASK_RUN_ID.to_string(), self.task_run_id.to_string());
        labels.insert(LABEL_COMPONENT.to_string(), "task-run-spec".to_string());

        Ok(Secret {
            metadata: ObjectMeta {
                name: Some(task_run_resource_name(self.task_run_id)),
                namespace: Some(self.namespace.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
            type_: Some("Opaque".to_string()),
            data: Some(data),
            ..Default::default()
        })
    }
}

/// Build the per-task-run `Secret` that carries the bincode-serialized
/// [`TaskRunSpec`] on its `spec.bin` key plus the bincode-serialized
/// [`ResolvedCredentials`] on its `credentials.bin` key.
///
/// The Secret name mirrors the Job name (`djinn-taskrun-{task_run_id}`) so
/// the Job manifest can reference it by construction without a round-trip.
/// Returned manifest is not yet applied to the cluster — callers pass it to
/// `kube::Api::<Secret>::create` (or equivalent) in PR 3.
///
/// This legacy entry-point does **not** include the `environment.json` or
/// `service_metadata.json` payload keys.  Use [`TaskRunSecretBuilder`] to
/// include those payloads.
pub fn build_task_run_secret(
    namespace: &str,
    task_run_id: &Uuid,
    spec: &TaskRunSpec,
    credentials: &ResolvedCredentials,
) -> Result<Secret, SecretError> {
    let encoded_spec = bincode::serialize(spec)?;
    let encoded_credentials = bincode::serialize(credentials)?;

    let mut data = BTreeMap::new();
    data.insert(SPEC_DATA_KEY.to_string(), ByteString(encoded_spec));
    data.insert(
        CREDENTIALS_DATA_KEY.to_string(),
        ByteString(encoded_credentials),
    );

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
    use djinn_runtime::{
        ResolvedCredentials, RoleKind, SerializableCredential, SupervisorFlow, TaskRunSpec,
    };

    /// Shared test fixture: a minimal valid TaskRunSpec.
    fn test_spec() -> TaskRunSpec {
        TaskRunSpec {
            task_run_id: "019e6a03-8aef-7201-9c9d-d7ba17613a0b".to_string(),
            task_attempt_id: None,
            task_id: "task-abc".to_string(),
            project_id: "proj-xyz".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-abc".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        }
    }

    /// Shared test fixture: minimal credentials with one worker key.
    fn test_credentials() -> ResolvedCredentials {
        let mut credentials = ResolvedCredentials::default();
        credentials.insert(
            RoleKind::Worker,
            SerializableCredential::ApiKey {
                key_name: "ANTHROPIC_API_KEY".into(),
                api_key: "sk-ant-fake".into(),
            },
        );
        credentials
    }

    #[test]
    fn task_run_secret_roundtrips_bincoded_spec() {
        // Shape mirrors djinn_runtime::spec::tests::task_run_spec_bincode_roundtrip.
        let spec = test_spec();
        let credentials = test_credentials();

        let task_run_id = Uuid::now_v7();
        let secret = build_task_run_secret("djinn", &task_run_id, &spec, &credentials)
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
        assert!(
            data.contains_key(CREDENTIALS_DATA_KEY),
            "credentials.bin must be present (Phase 7a)"
        );

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

        // credentials.bin roundtrips back to the same `ResolvedCredentials`.
        let cred_bytes = &data
            .get(CREDENTIALS_DATA_KEY)
            .expect("credentials.bin entry")
            .0;
        let round_trip_creds: ResolvedCredentials =
            bincode::deserialize(cred_bytes).expect("deserialize ResolvedCredentials");
        assert_eq!(round_trip_creds, credentials);

        // Owner reference helper produces the right parent pointer.
        let owner = job_owner_reference("djinn-taskrun-test", "uid-123");
        assert_eq!(owner.kind, "Job");
        assert_eq!(owner.api_version, "batch/v1");
        assert_eq!(owner.name, "djinn-taskrun-test");
        assert_eq!(owner.uid, "uid-123");
        assert_eq!(owner.controller, Some(true));
        assert_eq!(owner.block_owner_deletion, Some(true));
    }

    // ---- TaskRunSecretBuilder tests ------------------------------------

    #[test]
    fn builder_with_no_optional_payloads_matches_legacy() {
        // The builder without any optional payload should produce a Secret
        // identical in data keys to the legacy build_task_run_secret.
        let spec = test_spec();
        let credentials = test_credentials();
        let task_run_id = Uuid::now_v7();

        let legacy = build_task_run_secret("djinn", &task_run_id, &spec, &credentials).unwrap();
        let built = TaskRunSecretBuilder::new("djinn", &task_run_id, &spec, &credentials)
            .build()
            .unwrap();

        let legacy_data = legacy.data.as_ref().unwrap();
        let built_data = built.data.as_ref().unwrap();

        assert_eq!(legacy_data.len(), built_data.len());
        assert!(built_data.contains_key(SPEC_DATA_KEY));
        assert!(built_data.contains_key(CREDENTIALS_DATA_KEY));
        // No optional keys when none are set.
        assert!(!built_data.contains_key(ENV_CONFIG_SECRET_DATA_KEY));
        assert!(!built_data.contains_key(SERVICE_METADATA_SECRET_DATA_KEY));
    }

    #[test]
    fn builder_with_environment_config_includes_json_key() {
        let spec = test_spec();
        let credentials = test_credentials();
        let task_run_id = Uuid::now_v7();

        let cfg = EnvironmentConfig::empty();
        let secret = TaskRunSecretBuilder::new("djinn", &task_run_id, &spec, &credentials)
            .environment_config(&cfg)
            .build()
            .unwrap();

        let data = secret.data.as_ref().unwrap();
        // Legacy keys still present.
        assert!(data.contains_key(SPEC_DATA_KEY));
        assert!(data.contains_key(CREDENTIALS_DATA_KEY));
        // New environment.json key present.
        assert!(
            data.contains_key(ENV_CONFIG_SECRET_DATA_KEY),
            "environment.json must be present when environment_config is set"
        );

        // Round-trip: the bytes are valid UTF-8 JSON that decodes back to EnvironmentConfig.
        let env_bytes = &data.get(ENV_CONFIG_SECRET_DATA_KEY).unwrap().0;
        let json_str =
            std::str::from_utf8(env_bytes).expect("environment.json must be valid UTF-8");
        let round_tripped: EnvironmentConfig = serde_json::from_str(json_str)
            .expect("environment.json must be valid EnvironmentConfig");
        assert_eq!(round_tripped, cfg);
    }

    #[test]
    fn builder_with_default_environment_config_has_empty_pretask() {
        let spec = test_spec();
        let credentials = test_credentials();
        let task_run_id = Uuid::now_v7();

        let secret = TaskRunSecretBuilder::new("djinn", &task_run_id, &spec, &credentials)
            .with_default_environment_config()
            .build()
            .unwrap();

        let data = secret.data.as_ref().unwrap();
        assert!(data.contains_key(ENV_CONFIG_SECRET_DATA_KEY));

        let env_bytes = &data.get(ENV_CONFIG_SECRET_DATA_KEY).unwrap().0;
        let json_str = std::str::from_utf8(env_bytes).expect("valid UTF-8");
        let cfg: EnvironmentConfig =
            serde_json::from_str(json_str).expect("valid EnvironmentConfig");

        // Default config has schema_version 1 and empty pre_task.
        assert_eq!(cfg.schema_version, 1);
        assert!(
            cfg.lifecycle.pre_task.is_empty(),
            "default config must have empty lifecycle.pre_task, got: {:?}",
            cfg.lifecycle.pre_task
        );
    }

    #[test]
    fn builder_with_service_metadata_includes_json_key() {
        use crate::sidecar::ImageServiceResolution;

        let spec = test_spec();
        let credentials = test_credentials();
        let task_run_id = Uuid::now_v7();

        let resolution = ImageServiceResolution::default();
        let secret = TaskRunSecretBuilder::new("djinn", &task_run_id, &spec, &credentials)
            .service_metadata(&resolution)
            .unwrap()
            .build()
            .unwrap();

        let data = secret.data.as_ref().unwrap();
        assert!(data.contains_key(SPEC_DATA_KEY));
        assert!(data.contains_key(CREDENTIALS_DATA_KEY));
        assert!(
            data.contains_key(SERVICE_METADATA_SECRET_DATA_KEY),
            "service_metadata.json must be present when service_metadata is set"
        );

        // Round-trip: valid UTF-8 JSON that decodes back to ImageServiceResolution.
        let meta_bytes = &data.get(SERVICE_METADATA_SECRET_DATA_KEY).unwrap().0;
        let json_str =
            std::str::from_utf8(meta_bytes).expect("service_metadata.json must be valid UTF-8");
        let round_tripped: ImageServiceResolution =
            serde_json::from_str(json_str).expect("valid ImageServiceResolution");
        assert_eq!(round_tripped.injected, resolution.injected);
        assert_eq!(round_tripped.skipped, resolution.skipped);
    }

    #[test]
    fn builder_with_all_payloads_present() {
        use crate::sidecar::{
            ImageServiceResolution, InjectedServiceMetadata, ResolvedImageMetadata,
        };

        let spec = test_spec();
        let credentials = test_credentials();
        let task_run_id = Uuid::now_v7();

        let cfg = EnvironmentConfig::empty();
        let resolution = ImageServiceResolution {
            image: Some(ResolvedImageMetadata {
                id: "img-1".into(),
                name: "test-image".into(),
                tag: Some("latest".into()),
            }),
            requested_preset_ids: vec!["preset-pg".into()],
            injected: vec![InjectedServiceMetadata {
                preset_id: "preset-pg".into(),
                service_type: "postgres".into(),
                port: 5432,
                conn_env_var: "TEST_POSTGRES_URL".into(),
            }],
            skipped: Vec::new(),
            lookup_error: None,
            services: Vec::new(),
        };

        let secret = TaskRunSecretBuilder::new("djinn", &task_run_id, &spec, &credentials)
            .environment_config(&cfg)
            .service_metadata(&resolution)
            .unwrap()
            .build()
            .unwrap();

        let data = secret.data.as_ref().unwrap();
        // All four keys present.
        assert_eq!(
            data.len(),
            4,
            "expected 4 keys: spec.bin, credentials.bin, environment.json, service_metadata.json"
        );
        assert!(data.contains_key(SPEC_DATA_KEY));
        assert!(data.contains_key(CREDENTIALS_DATA_KEY));
        assert!(data.contains_key(ENV_CONFIG_SECRET_DATA_KEY));
        assert!(data.contains_key(SERVICE_METADATA_SECRET_DATA_KEY));

        // Verify spec and credentials still round-trip.
        let spec_bytes = &data.get(SPEC_DATA_KEY).unwrap().0;
        let rt_spec: TaskRunSpec = bincode::deserialize(spec_bytes).unwrap();
        assert_eq!(rt_spec.task_id, spec.task_id);

        let cred_bytes = &data.get(CREDENTIALS_DATA_KEY).unwrap().0;
        let rt_creds: ResolvedCredentials = bincode::deserialize(cred_bytes).unwrap();
        assert_eq!(rt_creds, credentials);

        // Verify environment.json is valid UTF-8 JSON.
        let env_bytes = &data.get(ENV_CONFIG_SECRET_DATA_KEY).unwrap().0;
        let env_str = std::str::from_utf8(env_bytes).expect("UTF-8");
        let rt_cfg: EnvironmentConfig = serde_json::from_str(env_str).unwrap();
        assert_eq!(rt_cfg, cfg);

        // Verify service_metadata.json is valid UTF-8 JSON.
        let meta_bytes = &data.get(SERVICE_METADATA_SECRET_DATA_KEY).unwrap().0;
        let meta_str = std::str::from_utf8(meta_bytes).expect("UTF-8");
        let rt_meta: ImageServiceResolution = serde_json::from_str(meta_str).unwrap();
        assert_eq!(rt_meta.injected.len(), 1);
        assert_eq!(rt_meta.injected[0].service_type, "postgres");
        assert_eq!(rt_meta.injected[0].port, 5432);
    }

    #[test]
    fn default_environment_config_json_bytes_is_valid_and_empty_pretask() {
        // Directly verify the helper function produces valid JSON.
        let bytes = default_environment_config_json_bytes();
        let json_str = std::str::from_utf8(&bytes).expect("valid UTF-8");
        let cfg: EnvironmentConfig =
            serde_json::from_str(json_str).expect("valid EnvironmentConfig");
        assert_eq!(cfg.schema_version, 1);
        assert!(cfg.lifecycle.pre_task.is_empty());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn env_config_secret_data_key_matches_filename() {
        // The key used in the Secret data map must match the filename the
        // Job mounts at TASK_RUN_ENV_CONFIG_MOUNT_FILE.
        assert_eq!(
            ENV_CONFIG_SECRET_DATA_KEY, "environment.json",
            "data key must match mount filename"
        );
    }

    #[test]
    fn service_metadata_secret_data_key_matches_filename() {
        assert_eq!(
            SERVICE_METADATA_SECRET_DATA_KEY, "service_metadata.json",
            "data key must match mount filename"
        );
    }

    // ---- hgd0 Wave 1 transport regression tests ----------------------------

    /// AC1: The legacy `build_task_run_secret` entry-point does NOT include
    /// `environment.json` or `service_metadata.json` keys.  Old Secrets that
    /// predate hgd0 Wave 1 are therefore backward-compatible: the Job's
    /// `optional: true` on the Secret volume lets the Pod start without these
    /// keys, and the worker defaults to `EnvironmentConfig::empty()` (empty
    /// `lifecycle.pre_task`).
    #[test]
    fn legacy_secret_omits_environment_json_key() {
        let spec = test_spec();
        let credentials = test_credentials();
        let task_run_id = Uuid::now_v7();

        let secret = build_task_run_secret("djinn", &task_run_id, &spec, &credentials).unwrap();

        let data = secret.data.as_ref().unwrap();
        // Legacy path: only spec.bin + credentials.bin.
        assert_eq!(data.len(), 2);
        assert!(data.contains_key(SPEC_DATA_KEY));
        assert!(data.contains_key(CREDENTIALS_DATA_KEY));
        // No environment.json — the worker defaults to EnvironmentConfig::empty()
        // which has schema_version 1 and empty lifecycle.pre_task.
        assert!(
            !data.contains_key(ENV_CONFIG_SECRET_DATA_KEY),
            "legacy secret must not include environment.json"
        );
        assert!(
            !data.contains_key(SERVICE_METADATA_SECRET_DATA_KEY),
            "legacy secret must not include service_metadata.json"
        );
    }

    /// AC1: The builder with `with_default_environment_config()` produces a
    /// Secret whose `environment.json` key decodes to an `EnvironmentConfig`
    /// with `schema_version: 1` and an empty `lifecycle.pre_task`.  This is
    /// the same config the worker mounts for projects with no explicit
    /// environment config, proving the "no config" path defaults cleanly.
    #[test]
    fn builder_default_config_mounts_schema_version_one_and_empty_pretask() {
        let spec = test_spec();
        let credentials = test_credentials();
        let task_run_id = Uuid::now_v7();

        let secret = TaskRunSecretBuilder::new("djinn", &task_run_id, &spec, &credentials)
            .with_default_environment_config()
            .build()
            .unwrap();

        let data = secret.data.as_ref().unwrap();
        assert!(data.contains_key(ENV_CONFIG_SECRET_DATA_KEY));

        let env_bytes = &data.get(ENV_CONFIG_SECRET_DATA_KEY).unwrap().0;
        let json_str = std::str::from_utf8(env_bytes).expect("valid UTF-8");
        let cfg: EnvironmentConfig =
            serde_json::from_str(json_str).expect("valid EnvironmentConfig");

        // The mounted effective config is schema_version 1 with empty pre_task —
        // the worker treats this as "no pre-task lifecycle".
        assert_eq!(cfg.schema_version, 1);
        assert!(
            cfg.lifecycle.pre_task.is_empty(),
            "default config must have empty lifecycle.pre_task, got: {:?}",
            cfg.lifecycle.pre_task
        );
        assert!(cfg.validate().is_ok());
    }

    /// AC2: A non-empty `lifecycle.pre_task` config round-trips exactly through
    /// the Secret transport as JSON.  The worker reads the same bytes from
    /// `/var/run/djinn/environment.json` and deserializes the exact commands.
    #[test]
    fn builder_with_nonempty_pretask_preserves_exact_commands_in_json() {
        use djinn_stack::environment::{LifecycleHooks, PreTaskCommand, PreTaskFailurePolicy};

        let spec = test_spec();
        let credentials = test_credentials();
        let task_run_id = Uuid::now_v7();

        let cfg = EnvironmentConfig {
            schema_version: 1,
            lifecycle: LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("install-deps".into()),
                        command: "pip install -e .".into(),
                        timeout_seconds: 120,
                        failure_policy: PreTaskFailurePolicy::default(),
                    },
                    PreTaskCommand {
                        name: None,
                        command: "npm ci".into(),
                        timeout_seconds: 300,
                        failure_policy: PreTaskFailurePolicy::default(),
                    },
                ],
                ..LifecycleHooks::default()
            },
            ..EnvironmentConfig::empty()
        };

        let secret = TaskRunSecretBuilder::new("djinn", &task_run_id, &spec, &credentials)
            .environment_config(&cfg)
            .build()
            .unwrap();

        let data = secret.data.as_ref().unwrap();
        let env_bytes = &data.get(ENV_CONFIG_SECRET_DATA_KEY).unwrap().0;
        let json_str = std::str::from_utf8(env_bytes).expect("valid UTF-8");
        let round_tripped: EnvironmentConfig =
            serde_json::from_str(json_str).expect("valid EnvironmentConfig");

        // Exact preservation of the pre_task commands.
        assert_eq!(round_tripped.lifecycle.pre_task.len(), 2);
        assert_eq!(
            round_tripped.lifecycle.pre_task[0].name.as_deref(),
            Some("install-deps")
        );
        assert_eq!(
            round_tripped.lifecycle.pre_task[0].command,
            "pip install -e ."
        );
        assert_eq!(round_tripped.lifecycle.pre_task[0].timeout_seconds, 120);
        assert_eq!(round_tripped.lifecycle.pre_task[1].name, None);
        assert_eq!(round_tripped.lifecycle.pre_task[1].command, "npm ci");
        assert_eq!(round_tripped.lifecycle.pre_task[1].timeout_seconds, 300);

        // The config still validates.
        assert!(round_tripped.validate().is_ok());
    }

    /// AC3: The full `ImageServiceResolution` round-trips through the Secret
    /// builder preserving `requested_preset_ids`, `injected`, and `skipped`
    /// — the same semantics the runtime logs as `task_run_services_resolved`.
    #[test]
    fn builder_service_metadata_round_trips_requested_injected_skipped() {
        use crate::sidecar::{
            ImageServiceResolution, InjectedServiceMetadata, ResolvedImageMetadata,
            SkippedServicePreset,
        };

        let spec = test_spec();
        let credentials = test_credentials();
        let task_run_id = Uuid::now_v7();

        let resolution = ImageServiceResolution {
            image: Some(ResolvedImageMetadata {
                id: "img-pg".into(),
                name: "postgres-image".into(),
                tag: Some("v1".into()),
            }),
            requested_preset_ids: vec!["preset-postgres-18".into(), "preset-redis-7".into()],
            injected: vec![InjectedServiceMetadata {
                preset_id: "preset-postgres-18".into(),
                service_type: "postgres".into(),
                port: 5432,
                conn_env_var: "DATABASE_URL,TEST_POSTGRES_URL".into(),
            }],
            skipped: vec![SkippedServicePreset {
                preset_id: "preset-redis-7".into(),
                reason: "unknown service preset".into(),
            }],
            lookup_error: None,
            services: Vec::new(), // serde(skip)
        };

        let secret = TaskRunSecretBuilder::new("djinn", &task_run_id, &spec, &credentials)
            .service_metadata(&resolution)
            .unwrap()
            .build()
            .unwrap();

        let data = secret.data.as_ref().unwrap();
        let meta_bytes = &data.get(SERVICE_METADATA_SECRET_DATA_KEY).unwrap().0;
        let json_str = std::str::from_utf8(meta_bytes).expect("valid UTF-8");
        let round_tripped: ImageServiceResolution =
            serde_json::from_str(json_str).expect("valid ImageServiceResolution");

        // requested_preset_ids preserved.
        assert_eq!(
            round_tripped.requested_preset_ids,
            vec!["preset-postgres-18", "preset-redis-7"]
        );
        // injected preserved.
        assert_eq!(round_tripped.injected.len(), 1);
        assert_eq!(round_tripped.injected[0].preset_id, "preset-postgres-18");
        assert_eq!(round_tripped.injected[0].service_type, "postgres");
        assert_eq!(round_tripped.injected[0].port, 5432);
        assert_eq!(
            round_tripped.injected[0].conn_env_var,
            "DATABASE_URL,TEST_POSTGRES_URL"
        );
        // skipped preserved.
        assert_eq!(round_tripped.skipped.len(), 1);
        assert_eq!(round_tripped.skipped[0].preset_id, "preset-redis-7");
        assert!(round_tripped.skipped[0].reason.contains("unknown"));
        // lookup_error absent.
        assert!(round_tripped.lookup_error.is_none());
        // image metadata preserved.
        let img = round_tripped.image.as_ref().expect("image present");
        assert_eq!(img.id, "img-pg");
        assert_eq!(img.name, "postgres-image");
        assert_eq!(img.tag.as_deref(), Some("v1"));
    }
}
