//! Pure `Job` manifest builder for a per-project canonical-graph warm run.
//!
//! The warm Job runs `djinn-agent-worker warm-graph <project_id>` inside
//! the project's devcontainer image. The devcontainer carries language
//! indexers (rust-analyzer for SCIP, etc.) so `warm-graph` can drive the
//! SCIP pipeline natively there. The `djinn-agent-worker` binary gains
//! a `warm-graph` subcommand that delegates into `djinn-graph` (the
//! extracted canonical-graph crate both server and worker depend on).
//!
//! The Pod's command is a shell wrapper that first `git clone`s the bare
//! mirror into an emptyDir workspace, then execs `djinn-agent-worker
//! warm-graph`. `DJINN_PROJECT_ROOT` tells the binary to treat the clone
//! as the project's working tree (bypassing the DB's stored
//! `projects.path` which points at a server-local dir not available in
//! the warm Pod). `backoffLimit: 0` — if the warm fails we rely on the
//! next graph_warmer tick to trigger a fresh attempt.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, PersistentVolumeClaimVolumeSource, PodSpec,
    PodTemplateSpec, Volume, VolumeMount,
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
/// Mount path for the writable workspace emptyDir. The warm Pod clones
/// the bare mirror here before running `warm-graph`.
pub const WORKSPACE_MOUNT_DIR: &str = "/workspace";
/// Volume name for the workspace emptyDir.
pub const VOLUME_WORKSPACE: &str = "workspace";

/// Binary path inside the devcontainer image. The `djinn-agent-worker`
/// Feature installs the worker binary at `/opt/djinn/bin/djinn-agent-worker`;
/// the binary exposes a `warm-graph <project-id>` subcommand.
pub const WARM_COMMAND_BIN: &str = "/opt/djinn/bin/djinn-agent-worker";

/// Build the Job manifest dispatched for one graph-warm run.
///
/// `project_id` becomes the resource-name suffix + label value. The
/// Pod's command is a shell wrapper that clones the bare mirror into a
/// writable emptyDir, then invokes `djinn-server warm-graph <project_id>`.
///
/// The ServiceAccount (`config.service_account`) is reused from task-run
/// dispatch — the warm Pod needs the mirror PVC + the DB env, both of
/// which already work with the task-run SA.
pub fn build_warm_job(
    config: &KubernetesConfig,
    project_id: &str,
    project_image_tag: &str,
) -> Job {
    let suffix = Uuid::now_v7();
    let sanitized_project = sanitize_id(project_id);
    let job_name = format!("djinn-warm-{}-{}", sanitized_project, short_uuid(&suffix));
    let labels = job_labels(project_id);

    let project_root = format!("{WORKSPACE_MOUNT_DIR}/{sanitized_project}");
    let mirror_path = format!("{MIRROR_MOUNT_DIR}/{project_id}.git");

    // Shell wrapper: the bare mirror on the PVC is `--filter=blob:none`,
    // so cloning it with `--local --shared` gives a partial clone where
    // `git checkout` fails on every missing blob (`unable to read sha1
    // file of <path>`). We avoid the filter entirely by pulling the
    // upstream URL (with fresh installation token, rotated every 60s by
    // the mirror fetcher) out of the mirror config and doing a
    // `--depth 1 --single-branch` clone straight from GitHub. Same
    // pattern the per-project build Job uses. Once we do it, the
    // workspace is a full working tree.
    let cmd = format!(
        r#"set -euo pipefail
git config --global --add safe.directory "{mirror_path}"
UPSTREAM_URL="$(git -C "{mirror_path}" config remote.origin.url)"
git clone --depth 1 --single-branch "$UPSTREAM_URL" "{project_root}"
exec {bin} warm-graph "{project_id}"
"#,
        mirror_path = mirror_path,
        project_root = project_root,
        bin = WARM_COMMAND_BIN,
        project_id = project_id,
    );

    let container = Container {
        name: "warmer".to_string(),
        image: Some(project_image_tag.to_string()),
        image_pull_policy: Some(config.image_pull_policy.clone()),
        command: Some(vec!["/bin/bash".to_string(), "-c".to_string(), cmd]),
        env: Some(vec![
            env_var("DJINN_SERVER_ADDR", &config.server_addr),
            env_var("DJINN_MIRROR_ROOT", MIRROR_MOUNT_DIR),
            env_var("DJINN_WARM_PROJECT_ID", project_id),
            // run_warm_graph_command picks this up when set and uses it
            // as the canonical project root, bypassing the DB's
            // server-local `projects.path`.
            env_var("DJINN_PROJECT_ROOT", &project_root),
        ]),
        volume_mounts: Some(vec![
            VolumeMount {
                name: VOLUME_MIRROR.to_string(),
                mount_path: MIRROR_MOUNT_DIR.to_string(),
                read_only: Some(true),
                ..VolumeMount::default()
            },
            VolumeMount {
                name: VOLUME_WORKSPACE.to_string(),
                mount_path: WORKSPACE_MOUNT_DIR.to_string(),
                read_only: Some(false),
                ..VolumeMount::default()
            },
        ]),
        ..Container::default()
    };

    let volumes = vec![
        Volume {
            name: VOLUME_MIRROR.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: config.mirror_pvc.clone(),
                read_only: Some(true),
            }),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_WORKSPACE.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Volume::default()
        },
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
        // Warm Pod runs on the per-project devcontainer image — that's
        // where the language indexers (rust-analyzer SCIP etc.) live.
        assert_eq!(
            container.image.as_deref(),
            Some("reg.example:5000/djinn-project-p:abc123")
        );
        // Pod command is a bash wrapper that clones the mirror before execing
        // the warm binary.
        let cmd = container.command.as_ref().expect("command");
        assert_eq!(cmd.len(), 3);
        assert_eq!(cmd[0], "/bin/bash");
        assert_eq!(cmd[1], "-c");
        assert!(cmd[2].contains("git clone"), "bash -c script: {}", cmd[2]);
        assert!(cmd[2].contains(WARM_COMMAND_BIN));
        assert!(cmd[2].contains("warm-graph \"proj-xyz\""));

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
        assert_eq!(
            envs.get("DJINN_PROJECT_ROOT").copied(),
            Some(format!("{WORKSPACE_MOUNT_DIR}/proj-xyz").as_str()),
        );

        let mounts = container.volume_mounts.as_ref().expect("mounts");
        assert_eq!(mounts.len(), 2);
        let by_name: BTreeMap<&str, &VolumeMount> =
            mounts.iter().map(|m| (m.name.as_str(), m)).collect();
        let mirror = by_name.get(VOLUME_MIRROR).expect("mirror mount");
        assert_eq!(mirror.mount_path, MIRROR_MOUNT_DIR);
        assert_eq!(mirror.read_only, Some(true));
        let workspace = by_name.get(VOLUME_WORKSPACE).expect("workspace mount");
        assert_eq!(workspace.mount_path, WORKSPACE_MOUNT_DIR);
        assert_eq!(workspace.read_only, Some(false));

        let volumes = pod.volumes.as_ref().expect("volumes");
        let by_volume_name: BTreeMap<&str, &Volume> =
            volumes.iter().map(|v| (v.name.as_str(), v)).collect();
        let mirror_v = by_volume_name.get(VOLUME_MIRROR).expect("mirror volume");
        let pvc = mirror_v.persistent_volume_claim.as_ref().expect("pvc");
        assert_eq!(pvc.claim_name, cfg.mirror_pvc);
        assert_eq!(pvc.read_only, Some(true));
        let workspace_v = by_volume_name
            .get(VOLUME_WORKSPACE)
            .expect("workspace volume");
        assert!(workspace_v.empty_dir.is_some(), "workspace must be emptyDir");
    }

    #[test]
    fn sanitize_id_lowercases_and_maps_disallowed_chars() {
        assert_eq!(sanitize_id("Proj_ABC/xyz"), "proj-abc-xyz");
    }
}
