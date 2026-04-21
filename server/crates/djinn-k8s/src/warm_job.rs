//! Pure `Job` manifest builder for a per-project canonical-graph warm run.
//!
//! Phase 3 PR 8 §6.3. The warm Job executes `djinn-server --warm-graph
//! <project_id>` inside the project's devcontainer image; the subcommand
//! owns indexer invocation, SCIP parsing, graph build, and
//! `repo_graph_cache` persistence, then exits. Job lifetime is short,
//! `backoffLimit: 0` — if the warm fails we rely on the next mirror-fetch
//! tick to trigger a fresh attempt.
//!
//! No cluster interaction — [`build_warm_job`] returns a
//! [`k8s_openapi::api::batch::v1::Job`] value that
//! [`crate::graph_warmer::K8sGraphWarmer`] hands to
//! `kube::Api::<Job>::create`.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvVar, PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use uuid::Uuid;

use crate::config::KubernetesConfig;

/// Label key identifying a graph-warm Job.
pub const LABEL_WARM: &str = "djinn.app/warm";
/// Label key for the project id a warm Job targets.
pub const LABEL_PROJECT_ID: &str = "djinn.app/project-id";
/// `djinn.app/component` value written on warm resources.
pub const COMPONENT_GRAPH_WARM: &str = "graph-warm";
/// Label key identifying which djinn-internal component created the resource.
pub const LABEL_COMPONENT: &str = "djinn.app/component";

/// Mount path for the read-only mirror PVC (mirrors the task-run Job).
pub const MIRROR_MOUNT_DIR: &str = "/mirror";
/// Volume name for the read-only mirror PVC.
pub const VOLUME_MIRROR: &str = "mirror";

/// Binary invoked inside the warm Pod — lives at this path on the per-project
/// image courtesy of the `djinn-agent-worker` Feature (Phase 3 §5.1).
pub const WARM_COMMAND_BIN: &str = "/opt/djinn/bin/djinn-server";

/// Build the Job manifest dispatched for one graph-warm run.
///
/// `project_id` becomes the resource-name suffix, label value, and the
/// positional argument to `djinn-server --warm-graph`. `project_image_tag`
/// is the per-project devcontainer tag resolved from `projects.image_tag`;
/// the warm Pod runs on that same image so SCIP indexers (bundled in the
/// `djinn-agent-worker` Feature) are reachable on `$PATH`.
///
/// The ServiceAccount (`config.service_account`) is reused from task-run
/// dispatch — the warm Pod needs the mirror PVC + the DB env, both of
/// which already work with `djinn-taskrun`.
pub fn build_warm_job(
    config: &KubernetesConfig,
    project_id: &str,
    project_image_tag: &str,
) -> Job {
    let suffix = Uuid::now_v7();
    let job_name = format!(
        "djinn-warm-{}-{}",
        sanitize_id(project_id),
        short_uuid(&suffix)
    );
    let labels = job_labels(project_id);

    let container = Container {
        name: "warmer".to_string(),
        image: Some(project_image_tag.to_string()),
        image_pull_policy: Some(config.image_pull_policy.clone()),
        command: Some(vec![
            WARM_COMMAND_BIN.to_string(),
            "--warm-graph".to_string(),
            project_id.to_string(),
        ]),
        env: Some(vec![
            env_var("DJINN_SERVER_ADDR", &config.server_addr),
            env_var("DJINN_MIRROR_ROOT", MIRROR_MOUNT_DIR),
            env_var("DJINN_WARM_PROJECT_ID", project_id),
        ]),
        volume_mounts: Some(vec![VolumeMount {
            name: VOLUME_MIRROR.to_string(),
            mount_path: MIRROR_MOUNT_DIR.to_string(),
            read_only: Some(true),
            ..VolumeMount::default()
        }]),
        ..Container::default()
    };

    let volumes = vec![Volume {
        name: VOLUME_MIRROR.to_string(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: config.mirror_pvc.clone(),
            read_only: Some(true),
        }),
        ..Volume::default()
    }];

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
            ttl_seconds_after_finished: Some(config.warm_job_ttl_seconds),
            active_deadline_seconds: Some(config.warm_job_timeout_seconds),
            ..JobSpec::default()
        }),
        ..Job::default()
    }
}

fn job_labels(project_id: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_COMPONENT.into(), COMPONENT_GRAPH_WARM.into());
    labels.insert(LABEL_WARM.into(), "true".into());
    labels.insert(LABEL_PROJECT_ID.into(), sanitize_id(project_id));
    labels
}

fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..EnvVar::default()
    }
}

/// Sanitise a project id to a DNS-label-safe form for Job names and label
/// values. Mirrors the helper in `djinn-image-controller::build_job`.
pub(crate) fn sanitize_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.len() > 48 {
        out.truncate(48);
    }
    out
}

/// Short form of a uuid v7 used as the Job-name disambiguator (full uuid
/// overruns DNS label budgets when combined with project id + prefix).
fn short_uuid(id: &Uuid) -> String {
    let full = id.simple().to_string();
    full[..12.min(full.len())].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_warm_job_manifest_with_expected_shape() {
        let cfg = KubernetesConfig::for_testing();
        let job = build_warm_job(&cfg, "proj-xyz", "reg.example:5000/djinn-project-p:abc123");

        let meta = &job.metadata;
        let name = meta.name.as_deref().expect("name");
        assert!(name.starts_with("djinn-warm-proj-xyz-"), "name: {name}");
        assert_eq!(meta.namespace.as_deref(), Some(cfg.namespace.as_str()));

        let labels = meta.labels.as_ref().expect("labels");
        assert_eq!(labels.get(LABEL_COMPONENT).map(String::as_str), Some(COMPONENT_GRAPH_WARM));
        assert_eq!(labels.get(LABEL_WARM).map(String::as_str), Some("true"));
        assert_eq!(labels.get(LABEL_PROJECT_ID).map(String::as_str), Some("proj-xyz"));

        let spec = job.spec.as_ref().expect("spec");
        assert_eq!(spec.backoff_limit, Some(0));
        assert_eq!(spec.ttl_seconds_after_finished, Some(cfg.warm_job_ttl_seconds));
        assert_eq!(spec.active_deadline_seconds, Some(cfg.warm_job_timeout_seconds));

        let pod = spec.template.spec.as_ref().expect("pod");
        assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
        assert_eq!(pod.service_account_name.as_deref(), Some(cfg.service_account.as_str()));
        assert_eq!(pod.containers.len(), 1);

        let container = &pod.containers[0];
        assert_eq!(container.name, "warmer");
        assert_eq!(
            container.image.as_deref(),
            Some("reg.example:5000/djinn-project-p:abc123")
        );
        assert_eq!(
            container.command.as_deref(),
            Some(
                [
                    WARM_COMMAND_BIN.to_string(),
                    "--warm-graph".to_string(),
                    "proj-xyz".to_string(),
                ]
                .as_slice()
            )
        );

        let envs: BTreeMap<&str, &str> = container
            .env
            .as_ref()
            .expect("env")
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(envs.get("DJINN_MIRROR_ROOT").copied(), Some(MIRROR_MOUNT_DIR));
        assert_eq!(envs.get("DJINN_WARM_PROJECT_ID").copied(), Some("proj-xyz"));
        assert_eq!(envs.get("DJINN_SERVER_ADDR").copied(), Some(cfg.server_addr.as_str()));

        let mounts = container.volume_mounts.as_ref().expect("mounts");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, VOLUME_MIRROR);
        assert_eq!(mounts[0].mount_path, MIRROR_MOUNT_DIR);
        assert_eq!(mounts[0].read_only, Some(true));

        let volumes = pod.volumes.as_ref().expect("volumes");
        assert_eq!(volumes.len(), 1);
        let mirror = &volumes[0];
        assert_eq!(mirror.name, VOLUME_MIRROR);
        let pvc = mirror.persistent_volume_claim.as_ref().expect("pvc");
        assert_eq!(pvc.claim_name, cfg.mirror_pvc);
        assert_eq!(pvc.read_only, Some(true));
    }

    #[test]
    fn sanitize_id_lowercases_and_maps_disallowed_chars() {
        assert_eq!(sanitize_id("Proj_ABC/xyz"), "proj-abc-xyz");
    }
}
