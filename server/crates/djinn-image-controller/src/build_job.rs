//! Pure `Job` manifest builder for a per-project devcontainer build.
//!
//! Matches the YAML shape in `phase3-devcontainer-and-warming.md` §5.5.
//! No cluster interaction — [`build_image_build_job`] returns the object
//! and the controller calls `kube::Api::<Job>::create`. Keeping the
//! builder pure lets the unit tests assert container env, volumes, and
//! labels without a live apiserver.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, KeyToPath, PodSpec, PodTemplateSpec,
    SecretVolumeSource, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use crate::config::ImageControllerConfig;

/// Djinn label keys written on every build Job + its Pod template.
pub const LABEL_COMPONENT: &str = "djinn.app/component";
pub const LABEL_BUILD: &str = "djinn.app/build";
pub const LABEL_PROJECT_ID: &str = "djinn.app/project-id";
pub const LABEL_IMAGE_HASH: &str = "djinn.app/image-hash";

/// Value written to [`LABEL_COMPONENT`] on build resources.
pub const COMPONENT_IMAGE_BUILD: &str = "image-build";

/// Mount path for the per-Job writable workspace (emptyDir). `devcontainer
/// build` requires a filesystem with the repo checked out — the Job does a
/// fresh shallow clone from GitHub into this directory at startup.
pub const WORKSPACE_MOUNT_DIR: &str = "/workspace";
/// Mount path for the per-project Secret that carries the clone URL with a
/// fresh GitHub installation token. The controller upserts this Secret on
/// every enqueue; the Job reads the URL from `<mount>/url` at startup.
pub const BUILD_TOKEN_MOUNT_DIR: &str = "/etc/djinn/build-token";
/// Key inside the build-token Secret that holds the clone URL.
pub const BUILD_TOKEN_SECRET_KEY_URL: &str = "url";
/// Writable docker-config directory (emptyDir). We keep this at the
/// standard `$HOME/.docker` location so `DOCKER_CONFIG` doesn't need to
/// be set — docker CLI and `docker buildx create` find both auth
/// (`config.json`) and buildx state (`buildx/`) here without env overrides.
pub const DOCKER_CONFIG_MOUNT_DIR: &str = "/root/.docker";
/// Mount path for the read-only registry-auth Secret. Kept separate from
/// `DOCKER_CONFIG_MOUNT_DIR` so the auth file can be seeded into a
/// writable location at startup (buildx writes state alongside the auth
/// file, so the directory must be writable as a whole).
pub const REGISTRY_AUTH_MOUNT_DIR: &str = "/etc/djinn/docker-auth";

const VOLUME_WORKSPACE: &str = "workspace";
const VOLUME_BUILD_TOKEN: &str = "build-token";
const VOLUME_DOCKER_CONFIG: &str = "docker-config";
const VOLUME_REGISTRY_AUTH: &str = "registry-auth";

/// K8s Secret name carrying the clone URL (with installation token) for a
/// given project's build Jobs. Stable per project — upserted on every
/// enqueue, referenced by every build Job Pod. Exposed so the controller
/// can name the Secret consistently with the Job's mount.
pub fn build_token_secret_name(project_id: &str) -> String {
    format!("djinn-build-token-{}", sanitize_id(project_id))
}

/// How long the build script inside the builder Pod can run before the
/// kubelet kills it. Matches the plan's recommended `activeDeadlineSeconds`.
const BUILD_ACTIVE_DEADLINE: i64 = 1800;
/// TTL applied to completed build Jobs so they self-clean.
const BUILD_TTL_AFTER_FINISH: i32 = 600;

/// Build the Job manifest dispatched for one per-project image build.
///
/// `project_id` becomes part of the Job name, the label set, and the
/// `IMAGE_TAG` env the builder script reads.
/// `hash_prefix` (12 chars of the full devcontainer hash) is appended to the
/// Job name so per-hash builds can coexist if old Jobs haven't yet cleaned.
/// `image_tag` is the full content-addressable tag (`<reg>/<repo>:<hash12>`).
pub fn build_image_build_job(
    config: &ImageControllerConfig,
    project_id: &str,
    hash_prefix: &str,
    image_tag: &str,
) -> Job {
    let labels = job_labels(project_id, hash_prefix);
    let job_name = format!("djinn-build-{}-{}", sanitize_id(project_id), hash_prefix);

    // Build Jobs are self-contained: read a fresh clone URL (with
    // installation token) out of a per-project Secret the controller
    // upserts on every enqueue, shallow-clone directly from GitHub into
    // the writable emptyDir workspace, then hand the tree to
    // devcontainer-cli. No mirror PVC mount.
    //
    // Docker config shape: `$HOME/.docker` is a writable emptyDir and
    // we `cp` the `config.json` from the read-only registry-auth Secret
    // into it at startup. Keeping auth + buildx state together under the
    // default `$HOME/.docker` means no `DOCKER_CONFIG` or `BUILDX_CONFIG`
    // overrides are needed — docker-cli and buildx both find what they
    // expect where they expect it, and the "writable config dir" is an
    // explicit Pod volume rather than an implicit `/tmp` write.
    let builder_script = format!(
        r#"set -euo pipefail
cp {auth_dir}/config.json {docker_config}/config.json

UPSTREAM_URL="$(cat {token_dir}/{token_key})"
git clone --depth 1 --single-branch "$UPSTREAM_URL" "$WORKSPACE_PATH"

docker buildx create --name djinn-builder --use \
    --driver remote "$DOCKER_HOST"
devcontainer build \
    --workspace-folder "$WORKSPACE_PATH" \
    --image-name "$IMAGE_TAG" \
    --push true \
    --cache-from type=registry,ref={registry}/cache/{project_id} \
    --cache-to type=registry,ref={registry}/cache/{project_id},mode=max
"#,
        auth_dir = REGISTRY_AUTH_MOUNT_DIR,
        docker_config = DOCKER_CONFIG_MOUNT_DIR,
        token_dir = BUILD_TOKEN_MOUNT_DIR,
        token_key = BUILD_TOKEN_SECRET_KEY_URL,
        registry = config.registry_host,
        project_id = project_id,
    );

    let container = Container {
        name: "builder".to_string(),
        image: Some(config.builder_image.clone()),
        env: Some(vec![
            env_var("DOCKER_HOST", &config.buildkitd_host),
            env_var("REGISTRY_HOST", &config.registry_host),
            env_var("PROJECT_ID", project_id),
            env_var(
                "WORKSPACE_PATH",
                &format!("{WORKSPACE_MOUNT_DIR}/{project_id}"),
            ),
            env_var("IMAGE_TAG", image_tag),
        ]),
        volume_mounts: Some(vec![
            VolumeMount {
                name: VOLUME_WORKSPACE.to_string(),
                mount_path: WORKSPACE_MOUNT_DIR.to_string(),
                read_only: Some(false),
                ..VolumeMount::default()
            },
            VolumeMount {
                name: VOLUME_BUILD_TOKEN.to_string(),
                mount_path: BUILD_TOKEN_MOUNT_DIR.to_string(),
                read_only: Some(true),
                ..VolumeMount::default()
            },
            VolumeMount {
                name: VOLUME_DOCKER_CONFIG.to_string(),
                mount_path: DOCKER_CONFIG_MOUNT_DIR.to_string(),
                read_only: Some(false),
                ..VolumeMount::default()
            },
            VolumeMount {
                name: VOLUME_REGISTRY_AUTH.to_string(),
                mount_path: REGISTRY_AUTH_MOUNT_DIR.to_string(),
                read_only: Some(true),
                ..VolumeMount::default()
            },
        ]),
        command: Some(vec!["/bin/bash".into(), "-c".into(), builder_script]),
        ..Container::default()
    };

    let volumes = vec![
        Volume {
            name: VOLUME_WORKSPACE.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_BUILD_TOKEN.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(build_token_secret_name(project_id)),
                optional: Some(false),
                default_mode: Some(0o0400),
                ..SecretVolumeSource::default()
            }),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_DOCKER_CONFIG.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_REGISTRY_AUTH.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(config.registry_auth_secret.clone()),
                // The chart's `registry-auth-secret.yaml` stores the docker
                // auth file under key `config.json` (matching what buildkitd
                // already consumes), not the dockerconfigjson-typed key.
                // Keep both names the same here so the mount lands at
                // `/root/.docker/config.json` inside the builder Pod.
                items: Some(vec![KeyToPath {
                    key: "config.json".to_string(),
                    path: "config.json".to_string(),
                    ..KeyToPath::default()
                }]),
                optional: Some(false),
                default_mode: Some(0o0400),
            }),
            ..Volume::default()
        },
    ];

    let pod_spec = PodSpec {
        // Build Jobs don't talk to the apiserver — they dial buildkitd over
        // gRPC and push to Zot over HTTP. The namespace default SA is
        // sufficient (no verbs required).  Leaving SA name unset falls back
        // to `default`, which is the standard pattern for offline builder
        // Pods.
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
            backoff_limit: Some(1),
            ttl_seconds_after_finished: Some(BUILD_TTL_AFTER_FINISH),
            active_deadline_seconds: Some(BUILD_ACTIVE_DEADLINE),
            ..JobSpec::default()
        }),
        ..Job::default()
    }
}

fn job_labels(project_id: &str, hash_prefix: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_COMPONENT.into(), COMPONENT_IMAGE_BUILD.into());
    labels.insert(LABEL_BUILD.into(), "true".into());
    labels.insert(LABEL_PROJECT_ID.into(), sanitize_id(project_id));
    labels.insert(LABEL_IMAGE_HASH.into(), hash_prefix.to_string());
    labels
}

fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..EnvVar::default()
    }
}

/// Kubernetes label values + resource names must match `[a-z0-9.-]`; we
/// downcase, keep word chars, and swap everything else for `-`. Length-cap
/// at 63 so names stay valid DNS labels.
pub(crate) fn sanitize_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.len() > 63 {
        out.truncate(63);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> ImageControllerConfig {
        ImageControllerConfig::for_testing()
    }

    #[test]
    fn builds_job_with_expected_metadata() {
        let cfg = test_cfg();
        let job = build_image_build_job(&cfg, "project-abc", "1a2b3c4d5e6f", "reg/p:abc");
        let meta = &job.metadata;
        let name = meta.name.as_deref().unwrap();
        assert!(name.starts_with("djinn-build-"), "name was {name}");
        assert!(name.ends_with("-1a2b3c4d5e6f"));
        let labels = meta.labels.as_ref().unwrap();
        assert_eq!(labels.get(LABEL_COMPONENT).unwrap(), COMPONENT_IMAGE_BUILD);
        assert_eq!(labels.get(LABEL_BUILD).unwrap(), "true");
        assert_eq!(labels.get(LABEL_PROJECT_ID).unwrap(), "project-abc");
        assert_eq!(labels.get(LABEL_IMAGE_HASH).unwrap(), "1a2b3c4d5e6f");
        assert_eq!(meta.namespace.as_deref(), Some(cfg.namespace.as_str()));
    }

    #[test]
    fn container_carries_docker_host_image_tag_and_workspace_path() {
        let cfg = test_cfg();
        let job = build_image_build_job(&cfg, "proj-xyz", "deadbeefcafe", "reg/p:deadbeef");
        let pod = job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap();
        let container = &pod.containers[0];
        let env: BTreeMap<&str, &str> = container
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(env.get("DOCKER_HOST").copied(), Some(cfg.buildkitd_host.as_str()));
        assert_eq!(env.get("REGISTRY_HOST").copied(), Some(cfg.registry_host.as_str()));
        assert_eq!(env.get("PROJECT_ID").copied(), Some("proj-xyz"));
        assert_eq!(env.get("IMAGE_TAG").copied(), Some("reg/p:deadbeef"));
        assert_eq!(
            env.get("WORKSPACE_PATH").copied(),
            Some(format!("{WORKSPACE_MOUNT_DIR}/proj-xyz").as_str())
        );
        assert!(
            !env.contains_key("MIRROR_PATH"),
            "MIRROR_PATH must be absent — build Jobs no longer mount the mirror PVC"
        );
        // DOCKER_CONFIG / BUILDX_CONFIG are intentionally unset: the Pod
        // mounts a writable emptyDir at the default $HOME/.docker so
        // docker-cli and buildx find everything under the standard path.
        assert!(
            !env.contains_key("DOCKER_CONFIG"),
            "DOCKER_CONFIG must be unset — writable config lives at default $HOME/.docker"
        );
        assert!(
            !env.contains_key("BUILDX_CONFIG"),
            "BUILDX_CONFIG must be unset — buildx writes state alongside config.json"
        );
    }

    #[test]
    fn volumes_include_workspace_build_token_docker_config_and_registry_auth() {
        let cfg = test_cfg();
        let job = build_image_build_job(&cfg, "p", "hhhhhhhhhhhh", "reg/p:h");
        let pod = job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap();
        let volumes = pod.volumes.as_ref().unwrap();
        let by_name: BTreeMap<&str, &Volume> =
            volumes.iter().map(|v| (v.name.as_str(), v)).collect();

        assert!(
            !by_name.contains_key("mirror"),
            "mirror PVC must NOT be mounted — Jobs clone straight from GitHub now"
        );

        let workspace = by_name
            .get(VOLUME_WORKSPACE)
            .expect("workspace emptyDir volume present");
        assert!(workspace.empty_dir.is_some(), "workspace must be emptyDir");

        let docker_cfg = by_name
            .get(VOLUME_DOCKER_CONFIG)
            .expect("docker-config emptyDir volume present");
        assert!(
            docker_cfg.empty_dir.is_some(),
            "docker-config must be writable emptyDir (buildx writes state under it)"
        );

        let token = by_name
            .get(VOLUME_BUILD_TOKEN)
            .expect("build-token Secret volume present");
        let token_src = token.secret.as_ref().unwrap();
        assert_eq!(
            token_src.secret_name.as_deref(),
            Some(build_token_secret_name("p").as_str())
        );
        assert_eq!(token_src.optional, Some(false));

        let auth = by_name
            .get(VOLUME_REGISTRY_AUTH)
            .expect("registry-auth volume present");
        let secret_src = auth.secret.as_ref().unwrap();
        assert_eq!(secret_src.secret_name.as_deref(), Some(cfg.registry_auth_secret.as_str()));
        let items = secret_src.items.as_ref().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "config.json");
    }

    #[test]
    fn docker_config_mount_is_writable_and_at_default_home_docker() {
        let cfg = test_cfg();
        let job = build_image_build_job(&cfg, "p", "hhhhhhhhhhhh", "reg/p:h");
        let container = &job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers[0];
        let mounts = container.volume_mounts.as_ref().unwrap();
        let docker_cfg = mounts
            .iter()
            .find(|m| m.name == VOLUME_DOCKER_CONFIG)
            .expect("docker-config mount present");
        assert_eq!(docker_cfg.mount_path, "/root/.docker");
        assert_eq!(
            docker_cfg.read_only,
            Some(false),
            "docker-config must be writable — buildx writes state + we cp auth into it"
        );
    }

    #[test]
    fn build_token_secret_name_is_stable_per_project() {
        assert_eq!(
            build_token_secret_name("project-abc"),
            "djinn-build-token-project-abc"
        );
        // Sanitization propagates so the Secret name is a valid DNS-1123 name.
        assert_eq!(
            build_token_secret_name("Weird/ID_1"),
            "djinn-build-token-weird-id-1"
        );
    }

    #[test]
    fn container_mounts_workspace_writable() {
        let cfg = test_cfg();
        let job = build_image_build_job(&cfg, "p", "hhhhhhhhhhhh", "reg/p:h");
        let container = &job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers[0];
        let mounts = container.volume_mounts.as_ref().unwrap();
        let workspace = mounts
            .iter()
            .find(|m| m.name == VOLUME_WORKSPACE)
            .expect("workspace mount present");
        assert_eq!(workspace.mount_path, WORKSPACE_MOUNT_DIR);
        assert_eq!(
            workspace.read_only,
            Some(false),
            "workspace must be writable so `git clone` + devcontainer can populate it"
        );
    }

    #[test]
    fn job_has_backoff_limit_and_ttl_set() {
        let cfg = test_cfg();
        let job = build_image_build_job(&cfg, "p", "hhhhhhhhhhhh", "reg/p:h");
        let spec = job.spec.as_ref().unwrap();
        assert_eq!(spec.backoff_limit, Some(1));
        assert_eq!(spec.ttl_seconds_after_finished, Some(BUILD_TTL_AFTER_FINISH));
        assert_eq!(spec.active_deadline_seconds, Some(BUILD_ACTIVE_DEADLINE));
    }

    #[test]
    fn sanitize_id_swaps_bad_chars_and_truncates() {
        assert_eq!(sanitize_id("Project_ID/42"), "project-id-42");
        let long = "a".repeat(80);
        assert_eq!(sanitize_id(&long).len(), 63);
    }
}
