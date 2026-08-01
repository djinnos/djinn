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

use djinn_image_builder::{BuildContext, DeclarationError, ScriptFile};
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
/// Present on catalog-image build Jobs (migration 46). Mutually exclusive
/// with [`LABEL_PROJECT_ID`]: the watcher branches on which one is set to
/// decide whether to reconcile the `projects` row or the `images` row.
pub const LABEL_IMAGE_ID: &str = "djinn.app/image-id";
pub const LABEL_IMAGE_HASH: &str = "djinn.app/image-hash";
/// Full, collision-free build intent. Labels retain the short hash because of
/// Kubernetes' 63-character label-value limit; terminal CAS uses this annotation.
pub const ANNOTATION_IMAGE_CONFIG_HASH: &str = "djinn.app/image-config-hash";

/// Value written to [`LABEL_COMPONENT`] on build resources.
pub const COMPONENT_IMAGE_BUILD: &str = "image-build";

/// Name of the builder container's environment variable carrying the launcher
/// authority protocol the artifact declares.
///
/// One name, two consumers that must agree: the container env
/// [`build_image_build_job`] sets, and the `echo` in
/// [`render_builder_script`] that expands it into the sentinel the watcher
/// parses into `images.launcher_authority_protocol`. Renaming it on one side
/// only would print a literal `${…}` the watcher refuses — so both sides read
/// this constant and the drift is not expressible.
pub const LAUNCHER_PROTOCOL_JOB_ENV: &str = "LAUNCHER_AUTHORITY_PROTOCOL";

/// What a build Job builds: a project's bespoke per-project image, or a
/// shared catalog image (migration 46). The build mechanics are identical
/// — same Dockerfile generator, same buildctl Job — only the identity
/// (image tag, resource names, correlator label, cache repo) differs.
#[derive(Clone, Debug)]
pub enum BuildSubject {
    /// A project's own per-project image, keyed by project id.
    Project(String),
    /// A shared catalog image, keyed by image id.
    Image(String),
}

impl BuildSubject {
    pub fn project(id: &str) -> Self {
        Self::Project(id.to_string())
    }

    pub fn image(id: &str) -> Self {
        Self::Image(id.to_string())
    }

    /// The raw id (project id or image id). This is what the correlator
    /// label carries so the watcher round-trips it back to the DB key.
    pub fn id(&self) -> &str {
        match self {
            Self::Project(id) | Self::Image(id) => id,
        }
    }

    /// The correlator label `(key, value)` written on the Job. Value is the
    /// sanitized raw id (sanitize is a no-op for the uuid keys we use, so it
    /// round-trips losslessly to the DB primary key).
    pub fn label(&self) -> (&'static str, String) {
        match self {
            Self::Project(id) => (LABEL_PROJECT_ID, sanitize_id(id)),
            Self::Image(id) => (LABEL_IMAGE_ID, sanitize_id(id)),
        }
    }

    /// DNS-safe segment used in k8s resource names (Job + ConfigMap) and the
    /// registry cache repo. The raw uuid only — NO kind prefix: a Job name
    /// (`djinn-build-<id>-<hash>`) becomes the auto-injected `job-name` pod
    /// label, which is capped at 63 chars, and `djinn-build-` + a 36-char
    /// uuid + `-` + a 12-char hash already lands at 61. An `img-`/`proj-`
    /// prefix would overflow it. Project and image ids are both distinct
    /// uuidv7s, so the bare id never collides across the two namespaces; the
    /// correlator label ([`Self::label`]) is what tells them apart.
    pub fn resource_segment(&self) -> String {
        sanitize_id(self.id())
    }

    /// The image-tag repository segment: `djinn-project-<id>` for projects,
    /// `djinn-image-<id>` for catalog images.
    pub fn tag_repo_segment(&self) -> String {
        match self {
            Self::Project(id) => format!("djinn-project-{}", sanitize_id(id)),
            Self::Image(id) => format!("djinn-image-{}", sanitize_id(id)),
        }
    }
}

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
/// subject + hash. Stable per (subject, hash) — two concurrent builds
/// at the same hash share the same CM.
pub fn build_context_config_map_name_for(subject: &BuildSubject, hash_prefix: &str) -> String {
    format!(
        "djinn-build-ctx-{}-{}",
        subject.resource_segment(),
        hash_prefix
    )
}

/// Project-keyed convenience wrapper over [`build_context_config_map_name_for`].
pub fn build_context_config_map_name(project_id: &str, hash_prefix: &str) -> String {
    build_context_config_map_name_for(&BuildSubject::project(project_id), hash_prefix)
}

/// Name of the build Job for `(subject, hash_prefix)`.
///
/// Single source of the formula: the controller has to name the *same* Job
/// [`build_image_build_job`] created in order to read its build metadata back
/// out of the Pod logs when the Job finished before the watcher saw it.
pub fn build_job_name_for(subject: &BuildSubject, hash_prefix: &str) -> String {
    format!("djinn-build-{}-{}", subject.resource_segment(), hash_prefix)
}

/// Build the ConfigMap carrying the generated Dockerfile + install
/// scripts. The Job owns it via an OwnerReference, so the CM is GC'd
/// when the Job's TTL expires.
///
/// `scripts` is the [`djinn_image_builder::BuildContext::scripts`]
/// list — each entry is `("scripts/<name>.sh", body)`.
pub fn build_image_build_context_config_map(
    config: &ImageControllerConfig,
    subject: &BuildSubject,
    hash_prefix: &str,
    build_context: &BuildContext,
) -> ConfigMap {
    let name = build_context_config_map_name_for(subject, hash_prefix);

    let mut labels = BTreeMap::new();
    labels.insert(LABEL_COMPONENT.into(), COMPONENT_IMAGE_BUILD.into());
    labels.insert(LABEL_BUILD.into(), "true".into());
    let (subject_label_key, subject_label_value) = subject.label();
    labels.insert(subject_label_key.into(), subject_label_value);
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

/// The shell the builder container runs, rendered from the deployment config
/// alone.
///
/// Extracted from [`build_image_build_job`] so the script's own text is
/// reachable without materialising a Kubernetes object: the sentinel this
/// script prints is what the catalog row is written from, and a test of that
/// chain should read the real script rather than restate it.
pub(crate) fn render_builder_script(
    config: &ImageControllerConfig,
    subject_segment: &str,
) -> String {
    // buildctl talks to the shared in-cluster buildkitd over gRPC. The
    // `--local` flags point the build context + dockerfile at the
    // ConfigMap mount. `--output type=image,...,push=true` pushes to
    // Zot; `--export-cache` / `--import-cache` hit the same registry's
    // cache repo so subsequent builds reuse layer exports.
    // `image-manifest=true,oci-mediatypes=true` on --export-cache is
    // required for ECR (and other OCI-strict registries): buildkit's
    // default cache export writes a non-OCI cache-manifest media type
    // that ECR rejects with `PUT .../manifests/latest: 400 Bad Request`.
    // Writing the cache as an OCI image manifest instead makes ECR accept
    // it. The `--output` already carries oci-mediatypes=true; the cache
    // export needs both flags. Registries that auto-accept the default
    // media type (Zot, GHCR) are unaffected by the stricter encoding.
    format!(
        r#"set -euo pipefail
mkdir -p {docker_config}
cp {auth_dir}/config.json {docker_config}/config.json

# Wait for buildkitd to accept gRPC connections before building. buildctl
# has no built-in connect retry, so a build dispatched while buildkitd is
# mid-rollout (e.g. during a chart upgrade) dies with
# `failed to list workers: ... connect: connection refused` and burns a
# Job backoff attempt. `buildctl debug workers` is the same call that was
# failing, so it's the right liveness probe. ~2min ceiling covers a pod
# reschedule; if buildkitd is genuinely down the Job still fails, just
# with a clearer signal after the wait.
echo "waiting for buildkitd at $BUILDCTL_ADDR ..."
for i in $(seq 1 60); do
    if buildctl --addr "$BUILDCTL_ADDR" debug workers >/dev/null 2>&1; then
        echo "buildkitd reachable after $((i)) attempt(s)"
        break
    fi
    if [ "$i" -eq 60 ]; then
        echo "buildkitd still unreachable after 60 attempts; failing" >&2
        exit 1
    fi
    sleep 2
done

buildctl \
    --addr "$BUILDCTL_ADDR" \
    build \
    --frontend dockerfile.v0 \
    --local context={ctx_dir} \
    --local dockerfile={ctx_dir} \
    --output type=image,name="$IMAGE_TAG",push=true,oci-mediatypes=true \
    --metadata-file /tmp/buildmeta.json \
    --export-cache type=registry,ref={registry}/cache/{cache_segment},mode=max,image-manifest=true,oci-mediatypes=true \
    --import-cache type=registry,ref={registry}/cache/{cache_segment}

# Emit the pushed image's immutable manifest digest on a deterministic
# sentinel line so the build watcher can capture images.registry_digest
# from the Pod logs (--metadata-file writes the containerimage.digest field).
DIGEST=$(grep -o '"containerimage.digest"[^,}}]*' /tmp/buildmeta.json | grep -o 'sha256:[a-f0-9]\+' || true)
echo "DJINN_IMAGE_DIGEST=${{DIGEST}}"

# Report the launcher authority protocol this artifact declares. The value is
# whatever the BuildContext put in the Dockerfile's
# `djinn.app/launcher-authority-protocol` LABEL — it arrives as an env var on
# this container and is echoed verbatim. It is deliberately NOT derived from
# "$IMAGE_TAG": a tag is mutable naming and can be made to claim anything,
# while the protocol decides whether the launcher or Pod resize owns CPU quota.
echo "{protocol_sentinel}${{{protocol_env}}}"
"#,
        auth_dir = REGISTRY_AUTH_MOUNT_DIR,
        docker_config = DOCKER_CONFIG_MOUNT_DIR,
        ctx_dir = BUILD_CONTEXT_MOUNT_DIR,
        registry = config.registry_host,
        cache_segment = subject_segment,
        protocol_sentinel = crate::watcher::PROTOCOL_SENTINEL,
        protocol_env = LAUNCHER_PROTOCOL_JOB_ENV,
    )
}

/// Build the Job manifest dispatched for one per-project image build.
///
/// `build_context` is taken by reference so the scripts list can produce
/// matching `items:` entries on the mount. The generated Dockerfile and
/// the scripts themselves are served from the per-build ConfigMap
/// [`build_image_build_context_config_map`] created first.
///
/// `image_tag` is the full content-addressable tag
/// (`<reg>/djinn-project-<id>:<hash>` or `<reg>/djinn-image-<id>:<hash>`);
/// the builder writes to that tag and exports cache to
/// `<reg>/cache/<subject-segment>`.
///
/// # Fail-closed on a disagreeing declaration
///
/// This Job renders two things that must say the same word: the Dockerfile
/// (via the ConfigMap) whose `LABEL` becomes the artifact's own metadata, and
/// the `LAUNCHER_AUTHORITY_PROTOCOL` env the builder echoes as the sentinel the
/// watcher writes into `images.launcher_authority_protocol`. So this is the
/// last place both are still in one hand, and it refuses to render a Job whose
/// [`BuildContext`] reports a protocol its Dockerfile does not declare. An
/// image whose label and catalog row disagree cannot be built, rather than
/// being built and then detected.
pub fn build_image_build_job(
    config: &ImageControllerConfig,
    subject: &BuildSubject,
    hash_prefix: &str,
    image_tag: &str,
    build_context: &BuildContext,
) -> Result<Job, DeclarationError> {
    let declared = build_context.verify_declaration()?;
    let labels = job_labels(subject, hash_prefix);
    let subject_segment = subject.resource_segment();
    let job_name = build_job_name_for(subject, hash_prefix);
    let cm_name = build_context_config_map_name_for(subject, hash_prefix);
    let builder_script = render_builder_script(config, &subject_segment);

    let container = Container {
        name: "builder".to_string(),
        image: Some(config.builder_image.clone()),
        env: Some(vec![
            env_var("BUILDCTL_ADDR", &config.buildkitd_host),
            env_var("REGISTRY_HOST", &config.registry_host),
            env_var("SUBJECT_ID", subject.id()),
            env_var("IMAGE_TAG", image_tag),
            // The value `verify_declaration` just read back out of the
            // Dockerfile this Job builds — so the sentinel the catalog is
            // written from and the artifact's own LABEL are the same string,
            // checked rather than assumed.
            env_var(LAUNCHER_PROTOCOL_JOB_ENV, declared.as_wire()),
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
        // /bin/sh on node:22-slim (Debian) is dash, which doesn't support
        // `set -o pipefail`. The image-builder Dockerfile installs bash
        // explicitly for this reason.
        command: Some(vec!["/bin/bash".into(), "-c".into(), builder_script]),
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

    // The build Pod has to authenticate to a managed registry (ECR/GCR/
    // ACR) via the docker credential helper baked into djinn-image-builder.
    // Helpers walk the AWS/GCP/Azure SDK env that's injected by IRSA /
    // Workload-Identity webhooks based on the Pod's ServiceAccount. With
    // no SA set, K8s falls back to `default` (no annotation, no token) and
    // every push ends in `401 Unauthorized`.
    let pod_spec = PodSpec {
        restart_policy: Some("Never".to_string()),
        service_account_name: Some(config.build_service_account.clone()),
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

    Ok(Job {
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
    })
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

fn job_labels(subject: &BuildSubject, hash_prefix: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_COMPONENT.into(), COMPONENT_IMAGE_BUILD.into());
    labels.insert(LABEL_BUILD.into(), "true".into());
    let (subject_label_key, subject_label_value) = subject.label();
    labels.insert(subject_label_key.into(), subject_label_value);
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
    use djinn_image_builder::{AgentWorkerImage, DEFAULT_LAUNCHER_PROTOCOL, generate_dockerfile};
    use djinn_stack::environment::EnvironmentConfig;

    fn test_cfg() -> ImageControllerConfig {
        ImageControllerConfig::for_testing()
    }

    fn test_build_context() -> BuildContext {
        let mut cfg = EnvironmentConfig::empty();
        cfg.schema_version = djinn_stack::environment::SCHEMA_VERSION;
        generate_dockerfile(
            &cfg,
            &AgentWorkerImage::new("djinn/agent-runtime", "dev"),
            DEFAULT_LAUNCHER_PROTOCOL,
        )
        .unwrap()
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
        let cm = build_image_build_context_config_map(
            &test_cfg(),
            &BuildSubject::project("proj-xyz"),
            "abc123",
            &ctx,
        );
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
        let job = build_image_build_job(
            &cfg,
            &BuildSubject::project("p"),
            "abc123def456",
            "reg/p:abc123",
            &ctx,
        )
        .unwrap();
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
        // ECR rejects buildkit's default cache-manifest media type with a
        // 400 on PUT .../manifests/latest; the export must request an OCI
        // image manifest. Regression guard for the 2026-05-25 fix.
        assert!(
            script.contains("--export-cache")
                && script.contains("image-manifest=true")
                && script.contains("oci-mediatypes=true"),
            "cache export must use OCI image-manifest media types for ECR:\n{script}"
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
        let job = build_image_build_job(
            &cfg,
            &BuildSubject::project("proj-xyz"),
            "abc123",
            "reg/p:abc123",
            &ctx,
        )
        .unwrap();
        let pod = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
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
            items
                .iter()
                .any(|i| i.path == "Dockerfile" && i.key == "Dockerfile"),
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
    fn builds_job_uses_configured_service_account() {
        // Default SA carries no IRSA annotation, so without an explicit
        // serviceAccountName the build Pod can't authenticate to managed
        // registries (ECR/GCR/ACR) and every push 401s.
        let mut cfg = test_cfg();
        cfg.build_service_account = "custom-build-sa".into();
        let ctx = test_build_context();
        let job = build_image_build_job(
            &cfg,
            &BuildSubject::project("p"),
            "abc123",
            "reg/p:abc123",
            &ctx,
        )
        .unwrap();
        let pod = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
        assert_eq!(pod.service_account_name.as_deref(), Some("custom-build-sa"));
    }

    #[test]
    fn builds_job_does_not_mount_mirror_pvc() {
        let cfg = test_cfg();
        let ctx = test_build_context();
        let job = build_image_build_job(
            &cfg,
            &BuildSubject::project("p"),
            "abc123",
            "reg/p:abc123",
            &ctx,
        )
        .unwrap();
        let pod = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
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
        let job = build_image_build_job(
            &cfg,
            &BuildSubject::project("p"),
            "abc123",
            "reg/p:abc123",
            &ctx,
        )
        .unwrap();
        let spec = job.spec.as_ref().unwrap();
        assert_eq!(spec.backoff_limit, Some(1));
        assert_eq!(
            spec.ttl_seconds_after_finished,
            Some(BUILD_TTL_AFTER_FINISH)
        );
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

    #[test]
    fn build_subject_distinguishes_project_and_image_namespaces() {
        let proj = BuildSubject::project("019e51db-proj");
        let img = BuildSubject::image("019e9907-img");
        // Correlator label keys differ so the watcher can branch.
        assert_eq!(proj.label().0, LABEL_PROJECT_ID);
        assert_eq!(img.label().0, LABEL_IMAGE_ID);
        // The label VALUE round-trips the raw id (DB key) losslessly.
        assert_eq!(proj.label().1, "019e51db-proj");
        assert_eq!(img.label().1, "019e9907-img");
        // Resource segment is the bare id (no kind prefix) to fit the 63-char
        // job-name label budget; the tag REPO segment is kind-prefixed so the
        // registry namespaces stay distinct.
        assert_eq!(img.resource_segment(), "019e9907-img");
        assert_eq!(proj.resource_segment(), "019e51db-proj");
        assert_eq!(proj.tag_repo_segment(), "djinn-project-019e51db-proj");
        assert_eq!(img.tag_repo_segment(), "djinn-image-019e9907-img");
    }

    #[test]
    fn catalog_build_job_name_fits_dns_label_budget() {
        // Regression: an `img-`-prefixed segment overflowed the auto-injected
        // `job-name` pod label (>63 chars) and every catalog build 422'd.
        let cfg = test_cfg();
        let ctx = test_build_context();
        let job = build_image_build_job(
            &cfg,
            &BuildSubject::image("019e9907-3685-7041-a7b7-246adf24c2d0"),
            "6812838f6587",
            "reg/djinn-image-019e9907-3685-7041-a7b7-246adf24c2d0:6812838f6587",
            &ctx,
        )
        .unwrap();
        let name = job.metadata.name.as_deref().unwrap();
        assert!(
            name.len() <= 63,
            "job name {} is {} chars",
            name,
            name.len()
        );
    }

    #[test]
    fn catalog_build_job_carries_image_label_not_project_label() {
        let cfg = test_cfg();
        let ctx = test_build_context();
        let job = build_image_build_job(
            &cfg,
            &BuildSubject::image("img-1"),
            "abc123",
            "reg/djinn-image-img-1:abc123",
            &ctx,
        )
        .unwrap();
        let labels = job.metadata.labels.as_ref().unwrap();
        assert_eq!(
            labels.get(LABEL_IMAGE_ID).map(String::as_str),
            Some("img-1")
        );
        assert!(!labels.contains_key(LABEL_PROJECT_ID));
        assert_eq!(
            job.metadata.name.as_deref(),
            Some("djinn-build-img-1-abc123")
        );
    }

    #[test]
    fn catalog_build_script_emits_digest_sentinel() {
        let cfg = test_cfg();
        let ctx = test_build_context();
        let job = build_image_build_job(
            &cfg,
            &BuildSubject::image("img-1"),
            "abc123",
            "reg/djinn-image-img-1:abc123",
            &ctx,
        )
        .unwrap();
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
        assert!(script.contains("--metadata-file"), "script:\n{script}");
        assert!(script.contains("DJINN_IMAGE_DIGEST="), "script:\n{script}");
        // Cache is keyed by the (bare) subject id.
        assert!(script.contains("/cache/img-1"), "script:\n{script}");
    }

    /// The protocol sentinel is fed by an env var carrying the *BuildContext's*
    /// declaration — the same value the Dockerfile's LABEL got. The script must
    /// never read it out of `$IMAGE_TAG`.
    #[test]
    fn catalog_build_script_declares_the_protocol_from_the_build_context_not_the_tag() {
        let cfg = test_cfg();
        let ctx = test_build_context();
        let job = build_image_build_job(
            &cfg,
            &BuildSubject::image("img-1"),
            "abc123",
            // A tag that lies about the protocol in every segment.
            "reg/djinn-image-resize-v2:resize-v2",
            &ctx,
        )
        .unwrap();
        let container = &job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers[0];
        let script = &container.command.as_ref().unwrap()[2];

        assert!(
            script.contains("echo \"DJINN_LAUNCHER_PROTOCOL=${LAUNCHER_AUTHORITY_PROTOCOL}\""),
            "script:\n{script}"
        );
        let declared = container
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "LAUNCHER_AUTHORITY_PROTOCOL")
            .expect("the build container must carry the declaration")
            .value
            .clone()
            .unwrap();
        assert_eq!(declared, ctx.launcher_protocol.as_wire());
        assert_eq!(
            declared,
            djinn_image_builder::DEFAULT_LAUNCHER_PROTOCOL.as_wire()
        );
        assert_ne!(
            declared, "resize-v2",
            "the misleading tag must not reach the declaration"
        );
    }

    /// The **configured** declaration — not just the default — is what the
    /// build container's env carries, for every protocol the type admits.
    ///
    /// This assertion lives here rather than in the end-to-end module because
    /// it is the one step of that chain which genuinely needs the rendered
    /// Kubernetes object, and this file is already inside the image-controller's
    /// k8s capability-boundary inventory (`epic/fztz`). Reading the Job from a
    /// new file would have grown that inventory to buy what one test needs.
    ///
    /// MUTATION: render the env from a literal (`env_var(…, "leaf-v1")`)
    /// instead of the verified declaration. The `resize-v2` iteration fails,
    /// naming the value the catalog would have been told.
    #[test]
    fn the_build_container_env_carries_the_configured_declaration() {
        let cfg = test_cfg();
        for protocol in djinn_launcher_protocol::LauncherAuthorityProtocol::ALL {
            let mut env_cfg = EnvironmentConfig::empty();
            env_cfg.schema_version = djinn_stack::environment::SCHEMA_VERSION;
            let ctx = generate_dockerfile(
                &env_cfg,
                &AgentWorkerImage::new("djinn/agent-runtime", "dev"),
                protocol,
            )
            .unwrap();

            let job = build_image_build_job(
                &cfg,
                &BuildSubject::image("img-1"),
                "abc123",
                // Still a tag that lies, in the opposite direction each time.
                "reg/djinn-image-leaf-v1:leaf-v1",
                &ctx,
            )
            .unwrap();
            let declared = job
                .spec
                .as_ref()
                .unwrap()
                .template
                .spec
                .as_ref()
                .unwrap()
                .containers[0]
                .env
                .as_ref()
                .unwrap()
                .iter()
                .find(|e| e.name == LAUNCHER_PROTOCOL_JOB_ENV)
                .expect("the build container must carry the declaration")
                .value
                .clone()
                .unwrap();

            assert_eq!(
                declared,
                protocol.as_wire(),
                "{protocol}: the catalog is written from this value, so it must be the \
                 declaration the artifact was built with"
            );
            // And it is the same string the Dockerfile's LABEL carries — the
            // artifact's metadata and the catalog's row, compared directly.
            assert!(
                ctx.dockerfile.contains(&format!(
                    "LABEL {}=\"{declared}\"",
                    djinn_image_builder::LAUNCHER_PROTOCOL_LABEL
                )),
                "{protocol}: the Job env and the artifact's LABEL must agree:\n{}",
                ctx.dockerfile
            );
        }
    }

    #[test]
    fn build_job_name_matches_the_job_the_builder_renders() {
        let cfg = test_cfg();
        let ctx = test_build_context();
        let subject = BuildSubject::image("img-1");
        let job = build_image_build_job(
            &cfg,
            &subject,
            "abc123",
            "reg/djinn-image-img-1:abc123",
            &ctx,
        )
        .unwrap();
        assert_eq!(
            job.metadata.name.as_deref(),
            Some(build_job_name_for(&subject, "abc123").as_str())
        );
    }
}
