//! Pure `Job` manifest builder for a per-task-run worker Pod.
//!
//! No cluster interaction — [`build_task_run_job`] produces a
//! [`k8s_openapi::api::batch::v1::Job`] value that PR 3 will hand to
//! `kube::Api::<Job>::create`. Structuring the builder as a pure function
//! keeps unit testing trivial: `build_task_run_job(&cfg, &id, secret_name)` +
//! struct assertions against the returned `Job`.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, KeyToPath, PersistentVolumeClaimVolumeSource, PodSpec,
    PodTemplateSpec, ProjectedVolumeSource, ResourceRequirements, SecretVolumeSource,
    ServiceAccountTokenProjection, Volume, VolumeMount, VolumeProjection,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use uuid::Uuid;

use crate::config::KubernetesConfig;

/// Label key for the task-run id (Djinn's primary correlator).
pub const LABEL_TASK_RUN_ID: &str = "djinn.app/task-run-id";
/// Label key identifying which djinn-internal component created the resource.
pub const LABEL_COMPONENT: &str = "djinn.app/component";

/// Value written to `LABEL_COMPONENT` on Job / Pod / Secret resources
/// dispatched by the task-run runtime.
pub const COMPONENT_TASK_RUN_WORKER: &str = "task-run-worker";

/// Mount path where the spec Secret is exposed inside the worker container.
pub const SPEC_MOUNT_DIR: &str = "/var/run/djinn";
/// Full path to the bincode-encoded `TaskRunSpec` file inside the worker.
pub const SPEC_MOUNT_FILE: &str = "/var/run/djinn/spec.bin";
/// Mount directory for the projected ServiceAccount token.
pub const TOKEN_MOUNT_DIR: &str = "/var/run/secrets/tokens";
/// Path where the projected token is read by the worker.
pub const TOKEN_MOUNT_FILE: &str = "/var/run/secrets/tokens/djinn";
/// Mount path for the read-only mirror PVC.
pub const MIRROR_MOUNT_DIR: &str = "/mirror";
/// Mount path for the writeable shared cache PVC.
pub const CACHE_MOUNT_DIR: &str = "/cache";
/// Mount path of the ephemeral workspace emptyDir.
pub const WORKSPACE_MOUNT_DIR: &str = "/workspace";
/// Audience advertised on the projected ServiceAccount token.
pub const TOKEN_AUDIENCE: &str = "djinn";
/// Token expiration requested from the kubelet, in seconds.
pub const TOKEN_EXPIRATION_SECONDS: i64 = 3600;

/// Name of the single key inside the per-task-run Secret that carries the
/// bincode-encoded [`djinn_runtime::TaskRunSpec`].
pub const SPEC_SECRET_KEY: &str = "spec.bin";

/// Volume name for the mounted spec Secret.
pub const VOLUME_SPEC: &str = "spec";
/// Volume name for the projected ServiceAccount token.
pub const VOLUME_AUTH_TOKEN: &str = "auth-token";
/// Volume name for the read-only mirror PVC.
pub const VOLUME_MIRROR: &str = "mirror";
/// Volume name for the writeable shared cache PVC.
pub const VOLUME_CACHE: &str = "cache";
/// Volume name for the ephemeral workspace emptyDir.
pub const VOLUME_WORKSPACE: &str = "workspace";

/// Build the `Job` manifest dispatched for one task-run.
///
/// The Job runs exactly one Pod (`restartPolicy: Never`, `backoffLimit: 0`);
/// Djinn's supervisor owns retry policy at the task level. Completed Jobs
/// are GC'd after `config.ttl_seconds_after_finished`.
///
/// `task_run_id` supplies both the resource name suffix and the label value;
/// `secret_name` is the name of the Secret produced by
/// [`crate::secret::build_taskrun_secret`] whose `spec.bin` key is mounted at
/// [`SPEC_MOUNT_FILE`]. The caller is responsible for having created that
/// Secret before the Job is submitted to the cluster.
///
/// `project_image_tag` is the per-project devcontainer image tag resolved
/// from `projects.image_tag` (Phase 3 PR 5). The caller MUST verify the
/// project's `image_status == ready` before reaching this builder —
/// there is no fallback to `config.image`; the per-task-run Pod always
/// runs the project-specific image. `config.image` is retained only for
/// legacy call sites we no longer expect to reach at runtime.
pub fn build_task_run_job(
    config: &KubernetesConfig,
    task_run_id: &Uuid,
    project_id: &str,
    secret_name: &str,
    project_image_tag: &str,
) -> Job {
    let task_run_id_str = task_run_id.to_string();
    let labels = job_labels(&task_run_id_str);
    let job_name = format!("djinn-taskrun-{task_run_id}");

    let container = Container {
        name: "worker".to_string(),
        image: Some(project_image_tag.to_string()),
        image_pull_policy: Some(config.image_pull_policy.clone()),
        // The per-project devcontainer image inherits its ENTRYPOINT from
        // the devcontainer base (typically `/bin/sh`), not from the
        // `djinn-agent-worker` Feature (which only installs the binary
        // and does not set ENTRYPOINT). We invoke the worker explicitly
        // so the task-run path is independent of base-image conventions;
        // `task-run` is the subcommand that consumes the DJINN_SERVER_ADDR /
        // DJINN_TASK_RUN_ID envs below.
        command: Some(vec![
            crate::warm_job::WARM_COMMAND_BIN.to_string(),
            "task-run".to_string(),
        ]),
        env: Some(vec![
            env_var("DJINN_SERVER_ADDR", &config.server_addr),
            env_var("DJINN_SPEC_PATH", SPEC_MOUNT_FILE),
            env_var("DJINN_TOKEN_PATH", TOKEN_MOUNT_FILE),
            env_var("DJINN_TASK_RUN_ID", &task_run_id_str),
        ]),
        volume_mounts: Some(vec![
            volume_mount(VOLUME_SPEC, SPEC_MOUNT_DIR, Some(true)),
            volume_mount(VOLUME_AUTH_TOKEN, TOKEN_MOUNT_DIR, Some(true)),
            volume_mount(VOLUME_MIRROR, MIRROR_MOUNT_DIR, Some(true)),
            volume_mount(VOLUME_CACHE, CACHE_MOUNT_DIR, None),
            volume_mount(VOLUME_WORKSPACE, WORKSPACE_MOUNT_DIR, None),
            crate::env_config::env_config_volume_mount(),
        ]),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(config.cpu_request.clone())),
                (
                    "memory".to_string(),
                    Quantity(config.memory_request.clone()),
                ),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(config.cpu_limit.clone())),
                ("memory".to_string(), Quantity(config.memory_limit.clone())),
            ])),
            ..ResourceRequirements::default()
        }),
        ..Container::default()
    };

    let volumes = vec![
        Volume {
            name: VOLUME_SPEC.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret_name.to_string()),
                items: Some(vec![KeyToPath {
                    key: SPEC_SECRET_KEY.to_string(),
                    path: SPEC_SECRET_KEY.to_string(),
                    ..KeyToPath::default()
                }]),
                optional: Some(false),
                default_mode: Some(0o0400),
            }),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_AUTH_TOKEN.to_string(),
            projected: Some(ProjectedVolumeSource {
                default_mode: Some(0o0400),
                sources: Some(vec![VolumeProjection {
                    service_account_token: Some(ServiceAccountTokenProjection {
                        audience: Some(TOKEN_AUDIENCE.to_string()),
                        expiration_seconds: Some(TOKEN_EXPIRATION_SECONDS),
                        path: "djinn".to_string(),
                    }),
                    ..VolumeProjection::default()
                }]),
            }),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_MIRROR.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: config.mirror_pvc.clone(),
                read_only: Some(true),
            }),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_CACHE.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: config.cache_pvc.clone(),
                read_only: Some(false),
            }),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_WORKSPACE.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Volume::default()
        },
        crate::env_config::env_config_volume(project_id),
    ];

    let pod_spec = PodSpec {
        service_account_name: Some(config.service_account.clone()),
        restart_policy: Some("Never".to_string()),
        containers: vec![container],
        volumes: Some(volumes),
        ..PodSpec::default()
    };

    let template = PodTemplateSpec {
        metadata: Some(ObjectMeta {
            labels: Some(labels.clone()),
            ..ObjectMeta::default()
        }),
        spec: Some(pod_spec),
    };

    Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(config.namespace.clone()),
            labels: Some(labels),
            ..ObjectMeta::default()
        },
        spec: Some(JobSpec {
            template,
            backoff_limit: Some(0),
            ttl_seconds_after_finished: Some(config.ttl_seconds_after_finished),
            ..JobSpec::default()
        }),
        ..Job::default()
    }
}

/// Build the label set attached to the Job and its Pod template.
///
/// Labels are intentionally minimal: the task-run id is the primary
/// correlator and the component marker lets controllers find task-run
/// resources with a single selector.
fn job_labels(task_run_id: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_TASK_RUN_ID.to_string(), task_run_id.to_string());
    labels.insert(
        LABEL_COMPONENT.to_string(),
        COMPONENT_TASK_RUN_WORKER.to_string(),
    );
    labels
}

fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..EnvVar::default()
    }
}

fn volume_mount(name: &str, mount_path: &str, read_only: Option<bool>) -> VolumeMount {
    VolumeMount {
        name: name.to_string(),
        mount_path: mount_path.to_string(),
        read_only,
        ..VolumeMount::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_task_run_job_manifest() {
        let cfg = KubernetesConfig::for_testing();
        let task_run_id = Uuid::now_v7();
        let secret_name = "djinn-taskrun-test";
        let project_image = "registry.example:5000/djinn-project-p:abc123def456";

        let job = build_task_run_job(
            &cfg,
            &task_run_id,
            "proj-xyz",
            secret_name,
            project_image,
        );

        // Metadata.
        let meta = &job.metadata;
        let name = meta.name.as_deref().expect("metadata.name set");
        assert!(
            name.starts_with("djinn-taskrun-"),
            "unexpected job name: {name}"
        );
        assert_eq!(meta.namespace.as_deref(), Some("djinn"));
        let labels = meta.labels.as_ref().expect("metadata.labels set");
        assert_eq!(
            labels.get(LABEL_TASK_RUN_ID).map(String::as_str),
            Some(task_run_id.to_string().as_str())
        );
        assert_eq!(
            labels.get(LABEL_COMPONENT).map(String::as_str),
            Some(COMPONENT_TASK_RUN_WORKER)
        );

        // Job-level knobs.
        let spec = job.spec.as_ref().expect("job.spec set");
        assert_eq!(spec.backoff_limit, Some(0));
        assert_eq!(spec.ttl_seconds_after_finished, Some(300));

        // Pod template mirrors labels.
        let template_labels = spec
            .template
            .metadata
            .as_ref()
            .and_then(|m| m.labels.as_ref())
            .expect("template.metadata.labels set");
        assert_eq!(
            template_labels.get(LABEL_TASK_RUN_ID).map(String::as_str),
            Some(task_run_id.to_string().as_str())
        );
        assert_eq!(
            template_labels.get(LABEL_COMPONENT).map(String::as_str),
            Some(COMPONENT_TASK_RUN_WORKER)
        );

        // Pod spec basics.
        let pod = spec.template.spec.as_ref().expect("template.spec set");
        assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
        assert_eq!(pod.service_account_name.as_deref(), Some("djinn-taskrun"));

        // Exactly one container named "worker".
        assert_eq!(pod.containers.len(), 1);
        let container = &pod.containers[0];
        assert_eq!(container.name, "worker");
        assert_eq!(container.image.as_deref(), Some(project_image));

        // The task-run Pod must invoke the worker binary + `task-run`
        // subcommand explicitly — the per-project devcontainer image has
        // no relevant ENTRYPOINT (the Feature only installs the binary).
        let cmd = container.command.as_ref().expect("container.command set");
        assert_eq!(
            cmd.as_slice(),
            &[
                crate::warm_job::WARM_COMMAND_BIN.to_string(),
                "task-run".to_string(),
            ]
        );

        // Env vars — require the two load-bearing ones, and confirm the
        // task-run id made it through.
        let envs: BTreeMap<&str, &str> = container
            .env
            .as_ref()
            .expect("container.env set")
            .iter()
            .map(|e| {
                (
                    e.name.as_str(),
                    e.value.as_deref().expect("env value present"),
                )
            })
            .collect();
        assert_eq!(
            envs.get("DJINN_SERVER_ADDR").copied(),
            Some(cfg.server_addr.as_str())
        );
        assert_eq!(
            envs.get("DJINN_SPEC_PATH").copied(),
            Some("/var/run/djinn/spec.bin")
        );
        assert_eq!(
            envs.get("DJINN_TOKEN_PATH").copied(),
            Some("/var/run/secrets/tokens/djinn")
        );
        assert_eq!(
            envs.get("DJINN_TASK_RUN_ID").copied(),
            Some(task_run_id.to_string().as_str())
        );

        // Volume mounts: 5 from the pre-env-config layout + the
        // environment-config mount added in P4.
        let mounts = container
            .volume_mounts
            .as_ref()
            .expect("volume_mounts set");
        assert_eq!(mounts.len(), 6, "expected 6 volume mounts");
        let expected_mounts: [(&str, &str, Option<bool>); 6] = [
            (VOLUME_SPEC, SPEC_MOUNT_DIR, Some(true)),
            (VOLUME_AUTH_TOKEN, TOKEN_MOUNT_DIR, Some(true)),
            (VOLUME_MIRROR, MIRROR_MOUNT_DIR, Some(true)),
            (VOLUME_CACHE, CACHE_MOUNT_DIR, None),
            (VOLUME_WORKSPACE, WORKSPACE_MOUNT_DIR, None),
            (
                crate::env_config::VOLUME_ENV_CONFIG,
                crate::env_config::ENV_CONFIG_MOUNT_DIR,
                Some(true),
            ),
        ];
        for (mount, (exp_name, exp_path, exp_ro)) in mounts.iter().zip(expected_mounts.iter()) {
            assert_eq!(&mount.name, exp_name);
            assert_eq!(&mount.mount_path, exp_path);
            assert_eq!(mount.read_only, *exp_ro);
        }

        // Volumes mirror the mount list.
        let volumes = pod.volumes.as_ref().expect("volumes set");
        assert_eq!(volumes.len(), 6, "expected 6 volumes");
        let expected_volume_names = [
            VOLUME_SPEC,
            VOLUME_AUTH_TOKEN,
            VOLUME_MIRROR,
            VOLUME_CACHE,
            VOLUME_WORKSPACE,
            crate::env_config::VOLUME_ENV_CONFIG,
        ];
        for (volume, expected_name) in volumes.iter().zip(expected_volume_names.iter()) {
            assert_eq!(&volume.name, expected_name);
        }

        // spec → Secret volume with the right name + key-to-path mapping.
        let spec_volume = &volumes[0];
        let secret_src = spec_volume.secret.as_ref().expect("spec volume is Secret");
        assert_eq!(secret_src.secret_name.as_deref(), Some(secret_name));
        assert_eq!(secret_src.optional, Some(false));
        assert_eq!(secret_src.default_mode, Some(0o0400));
        let items = secret_src.items.as_ref().expect("secret items set");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, SPEC_SECRET_KEY);
        assert_eq!(items[0].path, SPEC_SECRET_KEY);

        // auth-token → projected with a ServiceAccountToken source.
        let token_volume = &volumes[1];
        let projected = token_volume
            .projected
            .as_ref()
            .expect("auth-token volume is projected");
        let sources = projected.sources.as_ref().expect("projected sources set");
        assert_eq!(sources.len(), 1);
        let sa_token = sources[0]
            .service_account_token
            .as_ref()
            .expect("ServiceAccountToken source present");
        assert_eq!(sa_token.audience.as_deref(), Some(TOKEN_AUDIENCE));
        assert_eq!(sa_token.expiration_seconds, Some(TOKEN_EXPIRATION_SECONDS));
        assert_eq!(sa_token.path, "djinn");

        // mirror → PVC (read-only).
        let mirror_volume = &volumes[2];
        let mirror_pvc = mirror_volume
            .persistent_volume_claim
            .as_ref()
            .expect("mirror volume is PVC");
        assert_eq!(mirror_pvc.claim_name, cfg.mirror_pvc);
        assert_eq!(mirror_pvc.read_only, Some(true));

        // cache → PVC (writeable).
        let cache_volume = &volumes[3];
        let cache_pvc = cache_volume
            .persistent_volume_claim
            .as_ref()
            .expect("cache volume is PVC");
        assert_eq!(cache_pvc.claim_name, cfg.cache_pvc);
        assert_eq!(cache_pvc.read_only, Some(false));

        // workspace → emptyDir.
        let workspace_volume = &volumes[4];
        assert!(
            workspace_volume.empty_dir.is_some(),
            "workspace volume should be emptyDir"
        );

        // Resource requests/limits from config.
        let resources = container
            .resources
            .as_ref()
            .expect("container.resources set");
        let requests = resources.requests.as_ref().expect("requests set");
        assert_eq!(
            requests.get("cpu").map(|q| q.0.as_str()),
            Some(cfg.cpu_request.as_str())
        );
        assert_eq!(
            requests.get("memory").map(|q| q.0.as_str()),
            Some(cfg.memory_request.as_str())
        );
        let limits = resources.limits.as_ref().expect("limits set");
        assert_eq!(
            limits.get("cpu").map(|q| q.0.as_str()),
            Some(cfg.cpu_limit.as_str())
        );
        assert_eq!(
            limits.get("memory").map(|q| q.0.as_str()),
            Some(cfg.memory_limit.as_str())
        );
    }
}
