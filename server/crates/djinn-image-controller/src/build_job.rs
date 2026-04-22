//! Per-project image build Job + its build-context ConfigMap.
//!
//! Post-P5 this path is "djinn-native": no `devcontainer build`, no Node,
//! no GitHub shallow-clone. The controller generates a Dockerfile via
//! [`djinn_image_builder::generate_dockerfile`], drops it + the install
//! scripts into a ConfigMap mounted at `/build-context`, and runs
//! `buildctl build` against it.
//!
//! The builder Pod's image (`config.builder_image`) only needs `buildctl`
//! + a POSIX shell — `moby/buildkit` is the default.

use std::collections::BTreeMap;

use djinn_image_builder::{BuildContext, ScriptFile};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, EnvVar, KeyToPath, PodSpec, PodTemplateSpec,
    SecretVolumeSource, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use crate::config::ImageControllerConfig;

/// Djinn label keys written on every build Job + its Pod template.
pub const LABEL_COMPONENT: &str = "djinn.app/component";
pub const LABEL_BUILD: &str = "djinn.app/build";
pub const LABEL_PROJECT_ID: &str = "djinn.app/project-id";
pub const LABEL_IMAGE_HASH: &str = "djinn.app/image-hash";

/// Value written to [`LABEL_COMPONENT`] on build resources.
pub const COMPONENT_IMAGE_BUILD: &str = "image-build";

/// Where the build-context ConfigMap is mounted inside the builder Pod.
/// `buildctl --local context=<here> --local dockerfile=<here>` reads the
/// generated Dockerfile + scripts from this path.
pub const BUILD_CONTEXT_MOUNT_DIR: &str = "/build-context";
/// Where the registry-auth Secret is mounted. buildctl's `docker-container`
/// auth lookup uses `DOCKER_CONFIG` / `~/.docker/config.json`.
pub const REGISTRY_AUTH_MOUNT_DIR: &str = "/etc/djinn/docker-auth";
/// Writable home dir for buildctl — keeps the auth `config.json` reachable
/// at the canonical location without needing `DOCKER_CONFIG` env var
/// plumbing. The Pod seeds this from `REGISTRY_AUTH_MOUNT_DIR` at startup.
pub const DOCKER_CONFIG_MOUNT_DIR: &str = "/root/.docker";

const VOLUME_BUILD_CONTEXT: &str = "build-context";
const VOLUME_DOCKER_CONFIG: &str = "docker-config";
const VOLUME_REGISTRY_AUTH: &str = "registry-auth";

/// How long the build script inside the builder Pod can run before the
/// kubelet kills it. Matches the previous devcontainer-cli budget.
const BUILD_ACTIVE_DEADLINE: i64 = 1800;
/// TTL applied to completed build Jobs so they self-clean (+ the
/// build-context ConfigMap via its owner-ref).
const BUILD_TTL_AFTER_FINISH: i32 = 600;

/// Short key for the generated Dockerfile inside the build-context
/// ConfigMap. Volume-items map it to the literal file name `Dockerfile`
/// inside the mount.
const DOCKERFILE_KEY: &str = "Dockerfile";

/// Returns the stable name of a build-context ConfigMap for a given
/// project + hash. Stable per (project, hash) — two concurrent builds
/// at the same hash share the same CM.
pub fn build_context_config_map_name(project_id: &str, hash_prefix: &str) -> String {
    format!(
        "djinn-build-ctx-{}-{}",
        sanitize_id(project_id),
        hash_prefix
    )
}

/// Build the ConfigMap carrying the generated Dockerfile + install
/// scripts. The Job owns it via an OwnerReference, so the CM is GC'd
/// when the Job's TTL expires.
///
/// `scripts` is the [`djinn_image_builder::BuildContext::scripts`]
/// list — each entry is `("scripts/<name>.sh", body)`.
pub fn build_image_build_context_config_map(
    config: &ImageControllerConfig,
    project_id: &str,
    hash_prefix: &str,
    build_context: &BuildContext,
) -> ConfigMap {
    let name = build_context_config_map_name(project_id, hash_prefix);

    let mut labels = BTreeMap::new();
    labels.insert(LABEL_COMPONENT.into(), COMPONENT_IMAGE_BUILD.into());
    labels.insert(LABEL_BUILD.into(), "true".into());
    labels.insert(LABEL_PROJECT_ID.into(), sanitize_id(project_id));
    labels.insert(LABEL_IMAGE_HASH.into(), hash_prefix.into());

    // Data keys cannot contain `/`; we map each script path like
    // "scripts/base-debian.sh" to a sanitised key and use `items.path`
    // in the volume to restore the subdir structure inside the mount.
    let mut data = BTreeMap::new();
    data.insert(DOCKERFILE_KEY.to_string(), build_context.dockerfile.clone());
    for (path, body) in &build_context.scripts {
        data.insert(script_key_for_path(path), body.clone());
    }

    ConfigMap {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: Some(config.namespace.clone()),
            labels: Some(labels),
            ..ObjectMeta::default()
        },
        data: Some(data),
        ..ConfigMap::default()
    }
}

/// Volume-items description that restores the `scripts/<name>.sh`
/// subdirectory layout inside the mount, given the flat ConfigMap keys
/// [`build_image_build_context_config_map`] produced.
fn build_context_key_to_paths(scripts: &[ScriptFile]) -> Vec<KeyToPath> {
    let mut items = Vec::with_capacity(scripts.len() + 1);
    items.push(KeyToPath {
        key: DOCKERFILE_KEY.to_string(),
        path: DOCKERFILE_KEY.to_string(),
        ..KeyToPath::default()
    });
    for s in scripts {
        items.push(KeyToPath {
            key: script_key_for_path(&format!("scripts/{}", s.name)),
            path: format!("scripts/{}", s.name),
            ..KeyToPath::default()
        });
    }
    items
}

fn script_key_for_path(path: &str) -> String {
    // ConfigMap data keys must match `[-._a-zA-Z0-9]+`. Replace `/` + any
    // other path char with `-` deterministically.
    path.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Build the Job manifest dispatched for one per-project image build.
///
/// `build_context` is taken by reference so the scripts list can produce
/// matching `items:` entries on the mount. The generated Dockerfile and
/// the scripts themselves are served from the per-build ConfigMap
/// [`build_image_build_context_config_map`] created first.
///
/// `image_tag` is the full content-addressable tag
/// (`<reg>/djinn-project-<id>:<hash>`); the builder writes to that tag
/// and exports cache to `<reg>/cache/<id>`.
pub fn build_image_build_job(
    config: &ImageControllerConfig,
    project_id: &str,
    hash_prefix: &str,
    image_tag: &str,
    build_context: &BuildContext,
) -> Job {
    let labels = job_labels(project_id, hash_prefix);
    let job_name = format!("djinn-build-{}-{}", sanitize_id(project_id), hash_prefix);
    let cm_name = build_context_config_map_name(project_id, hash_prefix);

    // buildctl talks to the shared in-cluster buildkitd over gRPC. The
    // `--local` flags point the build context + dockerfile at the
    // ConfigMap mount. `--output type=image,...,push=true` pushes to
    // Zot; `--export-cache` / `--import-cache` hit the same registry's
    // cache repo so subsequent builds reuse layer exports.
    let builder_script = format!(
        r#"set -euo pipefail
mkdir -p {docker_config}
cp {auth_dir}/config.json {docker_config}/config.json

exec buildctl \
    --addr "$BUILDCTL_ADDR" \
    build \
    --frontend dockerfile.v0 \
    --local context={ctx_dir} \
    --local dockerfile={ctx_dir} \
    --output type=image,name="$IMAGE_TAG",push=true \
    --export-cache type=registry,ref={registry}/cache/{project_id},mode=max \
    --import-cache type=registry,ref={registry}/cache/{project_id}
"#,
        auth_dir = REGISTRY_AUTH_MOUNT_DIR,
        docker_config = DOCKER_CONFIG_MOUNT_DIR,
        ctx_dir = BUILD_CONTEXT_MOUNT_DIR,
        registry = config.registry_host,
        project_id = project_id,
    );

    let container = Container {
        name: "builder".to_string(),
        image: Some(config.builder_image.clone()),
        env: Some(vec![
            env_var("BUILDCTL_ADDR", &config.buildkitd_host),
            env_var("REGISTRY_HOST", &config.registry_host),
            env_var("PROJECT_ID", project_id),
            env_var("IMAGE_TAG", image_tag),
        ]),
        volume_mounts: Some(vec![
            VolumeMount {
                name: VOLUME_BUILD_CONTEXT.to_string(),
                mount_path: BUILD_CONTEXT_MOUNT_DIR.to_string(),
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
        command: Some(vec!["/bin/sh".into(), "-c".into(), builder_script]),
        ..Container::default()
    };

    // Items-based mount so the flat ConfigMap keys materialise as
    // `Dockerfile` + `scripts/<name>.sh` on disk.
    let ctx_items = build_context_key_to_paths(djinn_image_builder::SCRIPTS);

    let volumes = vec![
        Volume {
            name: VOLUME_BUILD_CONTEXT.to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: cm_name.clone(),
                items: Some(ctx_items),
                optional: Some(false),
                ..ConfigMapVolumeSource::default()
            }),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_DOCKER_CONFIG.to_string(),
            empty_dir: Some(k8s_openapi::api::core::v1::EmptyDirVolumeSource::default()),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_REGISTRY_AUTH.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(config.registry_auth_secret.clone()),
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

    // Drop `build_context` reference early; `_` silences the unused-var
    // hint without having to reorder the function body.
    let _ = build_context;

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

/// Build an OwnerReference pointing at a created build Job so the
/// build-context ConfigMap cascades when the Job is GC'd.
pub fn build_job_owner_reference(job: &Job) -> Option<OwnerReference> {
    let name = job.metadata.name.clone()?;
    let uid = job.metadata.uid.clone()?;
    Some(OwnerReference {
        api_version: "batch/v1".to_string(),
        kind: "Job".to_string(),
        name,
        uid,
        controller: Some(false),
        block_owner_deletion: Some(false),
    })
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
    use djinn_image_builder::{AgentWorkerImage, generate_dockerfile};
    use djinn_stack::environment::EnvironmentConfig;

    fn test_cfg() -> ImageControllerConfig {
        ImageControllerConfig::for_testing()
    }

    fn test_build_context() -> BuildContext {
        let mut cfg = EnvironmentConfig::empty();
        cfg.schema_version = djinn_stack::environment::SCHEMA_VERSION;
        generate_dockerfile(&cfg, &AgentWorkerImage::new("djinn/agent-runtime", "dev")).unwrap()
    }

    #[test]
    fn context_config_map_name_is_deterministic() {
        assert_eq!(
            build_context_config_map_name("proj-abc", "1a2b3c4d5e6f"),
            "djinn-build-ctx-proj-abc-1a2b3c4d5e6f"
        );
    }

    #[test]
    fn context_config_map_carries_dockerfile_and_scripts() {
        let ctx = test_build_context();
        let cm = build_image_build_context_config_map(&test_cfg(), "proj-xyz", "abc123", &ctx);
        let data = cm.data.expect("data");
        assert!(data.contains_key(DOCKERFILE_KEY));
        // Every script ends up as a separate key, sanitised.
        for s in djinn_image_builder::SCRIPTS {
            let key = script_key_for_path(&format!("scripts/{}", s.name));
            assert!(data.contains_key(&key), "missing key {key}");
        }
    }

    #[test]
    fn builds_job_targets_buildctl_not_devcontainer() {
        let cfg = test_cfg();
        let ctx = test_build_context();
        let job = build_image_build_job(&cfg, "p", "abc123def456", "reg/p:abc123", &ctx);
        let script = &job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers[0]
            .command
            .as_ref()
            .unwrap()[2];
        assert!(script.contains("buildctl"), "script:\n{script}");
        assert!(
            !script.contains("devcontainer"),
            "script must not reference devcontainer-cli:\n{script}"
        );
        assert!(
            !script.contains("git clone"),
            "build Job must not clone project source — the Dockerfile generator is self-contained:\n{script}"
        );
    }

    #[test]
    fn builds_job_mounts_build_context_cm_via_items() {
        let cfg = test_cfg();
        let ctx = test_build_context();
        let job = build_image_build_job(&cfg, "proj-xyz", "abc123", "reg/p:abc123", &ctx);
        let pod = job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap();
        let volumes = pod.volumes.as_ref().unwrap();
        let ctx_vol = volumes
            .iter()
            .find(|v| v.name == VOLUME_BUILD_CONTEXT)
            .expect("build-context volume");
        let cm = ctx_vol.config_map.as_ref().expect("configmap source");
        assert_eq!(cm.name, "djinn-build-ctx-proj-xyz-abc123");
        let items = cm.items.as_ref().expect("items");
        // Dockerfile + one item per script.
        assert!(
            items.iter().any(|i| i.path == "Dockerfile" && i.key == "Dockerfile"),
            "Dockerfile item missing"
        );
        for s in djinn_image_builder::SCRIPTS {
            let path = format!("scripts/{}", s.name);
            assert!(
                items.iter().any(|i| i.path == path),
                "missing script item path: {path}"
            );
        }
    }

    #[test]
    fn builds_job_does_not_mount_mirror_pvc() {
        let cfg = test_cfg();
        let ctx = test_build_context();
        let job = build_image_build_job(&cfg, "p", "abc123", "reg/p:abc123", &ctx);
        let pod = job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap();
        let volumes = pod.volumes.as_ref().unwrap();
        assert!(
            !volumes.iter().any(|v| v.name == "mirror"),
            "mirror PVC must NOT be mounted — buildctl reads from the CM context"
        );
    }

    #[test]
    fn job_has_backoff_limit_and_ttl_set() {
        let cfg = test_cfg();
        let ctx = test_build_context();
        let job = build_image_build_job(&cfg, "p", "abc123", "reg/p:abc123", &ctx);
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

    #[test]
    fn script_key_rewrites_slashes() {
        assert_eq!(
            script_key_for_path("scripts/install-rust.sh"),
            "scripts-install-rust.sh"
        );
    }
}
