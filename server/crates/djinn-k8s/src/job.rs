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
    ServiceAccountTokenProjection, Toleration, Volume, VolumeMount, VolumeProjection,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use uuid::Uuid;

use djinn_supervisor::cargo_target_run_dir;

use crate::config::KubernetesConfig;

/// Label key for the task-run id (Djinn's primary correlator).
pub const LABEL_TASK_RUN_ID: &str = "djinn.app/task-run-id";
/// Label key identifying which djinn-internal component created the resource.
pub const LABEL_COMPONENT: &str = "djinn.app/component";

/// Value written to `LABEL_COMPONENT` on Job / Pod / Secret resources
/// dispatched by the task-run runtime.
pub const COMPONENT_TASK_RUN_WORKER: &str = "task-run-worker";

/// Canonical task-run Job name prefix.
pub const TASKRUN_JOB_NAME_PREFIX: &str = "djinn-taskrun-";

/// Extract a task-run id from a canonical task-run Job name.
///
/// Returns the canonical UUID string, rejecting non-canonical prefixes and
/// malformed UUID suffixes.
pub fn task_run_id_from_job_name(job_name: &str) -> Option<String> {
    let suffix = job_name.strip_prefix(TASKRUN_JOB_NAME_PREFIX)?;
    Uuid::parse_str(suffix).ok().map(|uuid| uuid.to_string())
}

/// Extract a task-run Job inventory row from a Kubernetes Job.
///
/// Prefer the `djinn.app/task-run-id` label when it is present and valid, but
/// fall back to the canonical `djinn-taskrun-{uuid}` name so older/malformed
/// resources are still visible to the backstop reaper when the id can be
/// parsed safely. Unparseable candidates are skipped with a diagnostic log.
pub fn taskrun_job_ref_from_job(job: &Job) -> Option<djinn_runtime::TaskrunJobRef> {
    let job_name = job.metadata.name.clone().unwrap_or_default();
    if job_name.is_empty() {
        tracing::warn!("task-run Job inventory: skipping Job without metadata.name");
        return None;
    }

    let label_value = job
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(LABEL_TASK_RUN_ID));
    let task_run_id_from_label = label_value.and_then(|raw| match Uuid::parse_str(raw) {
        Ok(uuid) => Some(uuid.to_string()),
        Err(error) => {
            tracing::warn!(
                job_name = %job_name,
                task_run_id = %raw,
                error = %error,
                "task-run Job inventory: invalid task-run label; trying canonical name"
            );
            None
        }
    });

    let task_run_id_from_name = if job_name.starts_with(TASKRUN_JOB_NAME_PREFIX) {
        match task_run_id_from_job_name(&job_name) {
            Some(task_run_id) => Some(task_run_id),
            None => {
                tracing::warn!(
                    job_name = %job_name,
                    "task-run Job inventory: canonical task-run Job name has invalid UUID suffix"
                );
                None
            }
        }
    } else {
        None
    };

    let Some(task_run_id) = task_run_id_from_label.or(task_run_id_from_name) else {
        if label_value.is_some() || job_name.starts_with(TASKRUN_JOB_NAME_PREFIX) {
            tracing::warn!(
                job_name = %job_name,
                "task-run Job inventory: skipping unparseable task-run Job candidate"
            );
        }
        return None;
    };

    Some(djinn_runtime::TaskrunJobRef {
        job_name,
        task_run_id,
    })
}

/// Mount path where the spec Secret is exposed inside the worker container.
pub const SPEC_MOUNT_DIR: &str = "/var/run/djinn";
/// Full path to the bincode-encoded `TaskRunSpec` file inside the worker.
pub const SPEC_MOUNT_FILE: &str = "/var/run/djinn/spec.bin";
/// Full path to the bincode-encoded `ResolvedCredentials` file inside the
/// worker. Lives on the same Secret volume as `SPEC_MOUNT_FILE` so the
/// existing mount covers both keys (Phase 7a).
pub const CREDENTIALS_MOUNT_FILE: &str = "/var/run/djinn/credentials.bin";
/// Mount directory for the projected ServiceAccount token.
pub const TOKEN_MOUNT_DIR: &str = "/var/run/secrets/tokens";
/// Path where the projected token is read by the worker.
pub const TOKEN_MOUNT_FILE: &str = "/var/run/secrets/tokens/djinn";
/// Mount path for the mirror PVC. Mounted RW so the worker can push
/// its task_branch back to the mirror before delegating open_pr —
/// otherwise the host's `squash_merge_via_mirror` can't find the
/// worker's commits.
pub const MIRROR_MOUNT_DIR: &str = "/mirror";
/// Mount path for the writeable shared cache PVC.
pub const CACHE_MOUNT_DIR: &str = "/cache";
/// Mount path of the ephemeral workspace emptyDir.
pub const WORKSPACE_MOUNT_DIR: &str = "/workspace";
/// Audience advertised on the projected ServiceAccount token.
pub const TOKEN_AUDIENCE: &str = "djinn";
/// Token expiration requested from the kubelet, in seconds.
pub const TOKEN_EXPIRATION_SECONDS: i64 = 3600;

/// Name of the key inside the per-task-run Secret that carries the
/// bincode-encoded [`djinn_runtime::TaskRunSpec`].
pub const SPEC_SECRET_KEY: &str = "spec.bin";

/// Name of the key inside the per-task-run Secret that carries the
/// bincode-encoded [`djinn_runtime::ResolvedCredentials`] (Phase 7a).
pub const CREDENTIALS_SECRET_KEY: &str = "credentials.bin";

/// Volume name for the mounted spec Secret.
pub const VOLUME_SPEC: &str = "spec";
/// Volume name for the projected ServiceAccount token.
pub const VOLUME_AUTH_TOKEN: &str = "auth-token";
/// Volume name for the mirror PVC.
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
        env: Some(build_task_run_env(config, &task_run_id_str, project_id)),
        volume_mounts: Some(vec![
            volume_mount(VOLUME_SPEC, SPEC_MOUNT_DIR, Some(true)),
            volume_mount(VOLUME_AUTH_TOKEN, TOKEN_MOUNT_DIR, Some(true)),
            // Mirror PVC is mounted RW so the worker can push the
            // task_branch back to the mirror before delegating open_pr.
            // The mirror PVC is ReadWriteMany (deploy/helm/djinn/values.yaml)
            // so concurrent workers writing distinct, uniquely-named
            // task_branches do not conflict.
            volume_mount(VOLUME_MIRROR, MIRROR_MOUNT_DIR, Some(false)),
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
                items: Some(vec![
                    KeyToPath {
                        key: SPEC_SECRET_KEY.to_string(),
                        path: SPEC_SECRET_KEY.to_string(),
                        ..KeyToPath::default()
                    },
                    KeyToPath {
                        key: CREDENTIALS_SECRET_KEY.to_string(),
                        path: CREDENTIALS_SECRET_KEY.to_string(),
                        ..KeyToPath::default()
                    },
                ]),
                optional: Some(false),
                // 0444 (world-read) instead of 0400 so the worker process —
                // forced to runAsUser=10001 above so it can access the
                // /mirror PVC — can still read these files. They're owned
                // by root by default and 0400 means owner-only. The Pod
                // boundary is the security perimeter; within a single
                // container, world-read on the mounted Secret is fine.
                default_mode: Some(0o0444),
            }),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_AUTH_TOKEN.to_string(),
            projected: Some(ProjectedVolumeSource {
                // 0444 (world-read) instead of 0400 so the worker process —
                // forced to runAsUser=10001 above so it can access the
                // /mirror PVC — can still read these files. They're owned
                // by root by default and 0400 means owner-only. The Pod
                // boundary is the security perimeter; within a single
                // container, world-read on the mounted Secret is fine.
                default_mode: Some(0o0444),
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
                // RW so the worker can push its task_branch back to the
                // mirror before delegating open_pr. See the matching
                // VolumeMount comment above for the cross-Pod safety
                // argument.
                read_only: Some(false),
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

    // Pin Pods to a dedicated NodePool when the operator has configured one.
    // Both fields stay `None` if the corresponding config entry is empty so
    // the rendered manifest is identical to the pre-feature shape.
    let node_selector = (!config.node_selector.is_empty()).then(|| config.node_selector.clone());
    let tolerations: Option<Vec<Toleration>> =
        (!config.tolerations.is_empty()).then(|| config.tolerations.clone());

    let pod_spec = PodSpec {
        service_account_name: Some(config.service_account.clone()),
        restart_policy: Some("Never".to_string()),
        containers: vec![container],
        volumes: Some(volumes),
        node_selector,
        tolerations,
        // Give the worker enough time after SIGTERM to flush its final
        // RPC frame (TerminalReport) before SIGKILL — K8s default 30s is
        // tight when the supervisor is mid-stream over a slow link.
        termination_grace_period_seconds: Some(config.task_run_termination_grace_period_seconds),
        // Force the worker to run as uid 10001 (the djinn user baked
        // into the agent-runtime base image — see
        // server/docker/djinn-agent-runtime.Dockerfile) so it matches
        // the uid that owns the shared /mirror PVC. The per-project
        // devcontainer image layers `USER root` for apt-installs and
        // never restores USER djinn, so without this override the
        // worker runs as uid 0 and git 2.35.2+ rejects /mirror with
        // "dubious ownership". GIT_CONFIG_VALUE_0=* via env vars (set
        // in build_task_run_env) was tried first and silently failed
        // — git apparently disregards wildcard safe.directory from
        // env, only honoring it from file config. fsGroup doesn't
        // apply to PVCs (only to emptyDir / configMap volumes).
        security_context: Some(k8s_openapi::api::core::v1::PodSecurityContext {
            run_as_user: Some(10001),
            run_as_group: Some(10001),
            ..Default::default()
        }),
        ..PodSpec::default()
    };

    let template = PodTemplateSpec {
        metadata: Some(ObjectMeta {
            labels: Some(labels.clone()),
            // Protect in-flight task-run Pods from Karpenter consolidation /
            // node-drain eviction. An evicted worker loses its RPC stream to
            // the server mid-stage, so the run ends `Interrupted/[]` and no
            // stage completes (observed on staging: every run interrupted by
            // "Evicted: Underutilized" consolidation). The Pod is short-lived
            // and bounded by `active_deadline_seconds`, so opting out of
            // voluntary disruption can't pin a node indefinitely.
            annotations: Some(BTreeMap::from([(
                "karpenter.sh/do-not-disrupt".to_string(),
                "true".to_string(),
            )])),
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
            // Cap total Pod wall-clock so a stuck RPC connection or
            // runaway LLM stream can't keep the worker alive forever
            // (TTL doesn't fire until the Pod exits). `i64` per the
            // upstream `JobSpec` type, but we accept `u64` in config to
            // make negative deadlines unrepresentable.
            active_deadline_seconds: Some(config.task_run_active_deadline_seconds as i64),
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

/// Build the container env-var list for the task-run Pod.
///
/// The first block carries spec/credential paths, the task-run id, and
/// the workspace knobs (`TMPDIR`, `DJINN_MIRROR_ROOT`). The second block
/// forwards the server's DB configuration so the worker's
/// `bootstrap_warm_database()` connects to the same Dolt/MySQL instance
/// as the server — otherwise it falls back to
/// `mysql://root@127.0.0.1:3306/djinn` (single-node dev default) and the
/// connection fails silently in any real cluster. Mirrors the warm-Pod
/// pattern in `warm_job.rs`.
fn build_task_run_env(
    config: &KubernetesConfig,
    task_run_id_str: &str,
    project_id: &str,
) -> Vec<EnvVar> {
    let mut env = vec![
        env_var("DJINN_SERVER_ADDR", &config.server_addr),
        env_var("DJINN_SPEC_PATH", SPEC_MOUNT_FILE),
        env_var("DJINN_CREDENTIALS_PATH", CREDENTIALS_MOUNT_FILE),
        env_var("DJINN_TOKEN_PATH", TOKEN_MOUNT_FILE),
        env_var("DJINN_TASK_RUN_ID", task_run_id_str),
        // TMPDIR points the supervisor's TempDir::new() (used by
        // mirror.clone_ephemeral) at the writable /workspace emptyDir
        // instead of the container's tmpfs root, which has stricter
        // size limits.
        env_var("TMPDIR", WORKSPACE_MOUNT_DIR),
        // DJINN_MIRROR_ROOT is read by the in-Pod MirrorManager so the
        // worker clones from /mirror without a hard-coded path.
        env_var("DJINN_MIRROR_ROOT", MIRROR_MOUNT_DIR),
        // Forward the Job's activeDeadlineSeconds so the in-pod supervisor
        // can arm its OWN soft deadline at `deadline - margin` and wind
        // itself down gracefully (cancel + checkpoint commit/push) before
        // the kubelet hard-kills the Pod at the deadline. Without this the
        // worker is blind to its wall-clock budget and loses in-flight work
        // to the SIGTERM/SIGKILL. Same i64→string value the Job carries.
        env_var(
            "DJINN_TASK_RUN_DEADLINE_SECONDS",
            &config.task_run_active_deadline_seconds.to_string(),
        ),
    ];
    // Forward the server's DB configuration so the worker's
    // `bootstrap_warm_database()` opens the same Postgres instance the
    // server uses. Without this the worker's
    // `bootstrap_warm_database()` errors out at startup (no env-var
    // fallback any more); helpers like `resolve_role_overrides` /
    // `build_prompt_context` need a live DB to function. Mirrors warm_job.rs.
    if let Some(url) = config.database_url.as_deref() {
        env.push(env_var("DJINN_DATABASE_URL", url));
    }
    // Force git to trust the cross-UID-owned /mirror PVC. The per-project
    // image runs as root by default (USER reset by language-toolchain
    // layers), so the worker process sees the /mirror dir as
    // 10001:10001 — git 2.35.2+ rejects that with "dubious ownership"
    // unless safe.directory is set. We inject the env vars at the Pod
    // level so the worker process inherits them; the worker's Rust code
    // also sets them per-Command (run_git_command), but the Pod-level
    // env is the belt-and-suspenders that guarantees any subprocess
    // tree gets them.
    env.push(env_var("GIT_CONFIG_COUNT", "1"));
    env.push(env_var("GIT_CONFIG_KEY_0", "safe.directory"));
    env.push(env_var("GIT_CONFIG_VALUE_0", "*"));

    env.extend(task_run_cache_env_vars(project_id, task_run_id_str));
    env
}

/// Runtime env vars routing the shared Rust toolchain caches to the persistent
/// `/cache` PVC. Warm/verification Pods use the per-project base target dir;
/// task-run Pods use `task_run_cache_env_vars` so their writable target dir is
/// private per task run while still sharing registry and sccache settings.
/// The common cache routing stays single-sourced here on
/// purpose: the DB env once drifted because the task-run path was updated and
/// the warm path was missed (see the comment in warm_job.rs) — keeping shared
/// cache routing in one place makes that class of drift less likely.
///
/// Set at RUNTIME, not as image ENV, on purpose: the image must keep CARGO_HOME
/// at the baked /usr/local/cargo so install-rust.sh's cargo/rustc proxies stay
/// on PATH. Pointing CARGO_HOME at /cache in the Dockerfile would install those
/// proxies into a path the runtime PVC overlay then HIDES — the v2→v3
/// RUSTUP_HOME regression (see image-builder/src/hash.rs). Here the proxies stay
/// at /usr/local/cargo/bin (on PATH); only cargo's data dirs move to the PVC,
/// which is djinn-owned (uid 10001), persistent, and in the Landlock allowlist
/// (djinn-agent sandbox/linux.rs).
///
/// - CARGO_HOME: registry index + crate sources, content-addressed by
///   crate@version, so it is safe to SHARE across projects (like Go's module
///   cache) — common crates download once. (Image default /usr/local/cargo is
///   an image layer that loses runtime-downloaded crates when the Pod dies.)
/// - CARGO_TARGET_DIR: compiled artifacts are workspace-specific. The shared
///   warm/verification base is namespaced per project; warm jobs write it with
///   `CARGO_INCREMENTAL=0` so it does not accumulate incremental compiler state.
///   Task-runs get a deterministic private dir under
///   `/cache/cargo-target-runs/<task_run_id>` so they never write the shared base
///   directly or contend on Cargo's shared build-dir lock. The worker may seed
///   that private run dir from the warm base before cargo starts, then cargo is
///   free to mutate only the run-local target. (Default is <workspace>/target
///   inside the ephemeral clone — also lost.)
/// - SCCACHE_DIR: repos routinely pin `rustc-wrapper = "sccache"` in
///   .cargo/config.toml (e.g. the platform repo, which also sets
///   CARGO_INCREMENTAL=0 as sccache requires), so cargo invokes sccache
///   regardless of any env var. Without SCCACHE_DIR sccache falls back to
///   $HOME/.cache/sccache (/home/djinn/.cache/sccache), which is (1) ephemeral
///   and (2) NOT in the Landlock allowlist (only $HOME/.cache/djinn is), so the
///   sandboxed sccache server is denied write there. Namespaced per project
///   (like CARGO_TARGET_DIR): sccache's local disk cache is not safe for
///   multiple concurrent server processes sharing one directory, and Pods share
///   the /cache PVC, so a single /cache/sccache would risk multi-writer
///   corruption across projects.
fn common_cache_env_vars(project_id: &str) -> Vec<EnvVar> {
    vec![
        env_var("CARGO_HOME", &format!("{CACHE_MOUNT_DIR}/cargo")),
        // Route rustc through sccache so Rust compiles are cached across runs.
        // The design above assumes repos pin `rustc-wrapper = "sccache"` in
        // .cargo/config.toml, but many (incl. djinn) don't — leaving rustc
        // uncached (sccache showed 0% Rust hits), so every verification/warm/
        // task-run recompiled the workspace cold (~20min clippy). Set it in the
        // pod env (NOT .cargo/config.toml, which would break local/CI hosts
        // lacking sccache); the catalog/runtime images all ship sccache on PATH.
        // CARGO_INCREMENTAL=0 (set below / by repos) is required for sccache.
        env_var("RUSTC_WRAPPER", "sccache"),
        env_var("CARGO_INCREMENTAL", "0"),
        env_var(
            "SCCACHE_DIR",
            &format!("{CACHE_MOUNT_DIR}/sccache/{project_id}"),
        ),
        // Default is 10G, which evicts fast on a large workspace; give sccache
        // more headroom on the shared PVC.
        env_var("SCCACHE_CACHE_SIZE", "20G"),
        // Build-in-pod contexts (task-run, warm, verification) have no Postgres
        // reachable, but a repo's .cargo/config.toml may bake a DATABASE_URL for
        // local online sqlx (djinn itself bakes :5433). Force offline so the
        // compile-time sqlx macros use the committed .sqlx cache instead of
        // trying — and failing — to connect. Local dev keeps online validation
        // because it never sources cache_env_vars.
        env_var("SQLX_OFFLINE", "true"),
    ]
}

/// Cache env vars for warm/verification Pods that own the shared per-project
/// target base.
pub(crate) fn cache_env_vars(project_id: &str) -> Vec<EnvVar> {
    let mut env = common_cache_env_vars(project_id);
    env.push(env_var(
        "CARGO_TARGET_DIR",
        &format!("{CACHE_MOUNT_DIR}/cargo-target/{project_id}"),
    ));
    env
}

/// Cache env vars for warm Pods that intentionally populate the shared
/// per-project cargo target base. Incremental compilation is disabled because
/// the base is the durable single-writer cache seed, not a per-process scratch
/// directory for incremental compiler state.
pub(crate) fn warm_cache_env_vars(project_id: &str) -> Vec<EnvVar> {
    let mut env = cache_env_vars(project_id);
    env.push(env_var("CARGO_INCREMENTAL", "0"));
    env
}

/// Cache env vars for task-run Pods. The target dir is private to the canonical
/// task run id, not the generated Kubernetes resource name, so task Pods avoid
/// the shared Cargo build-dir lock while preserving the warm per-project base as
/// a read-only seed source. Shared cache settings remain identical to
/// warm/verification Pods.
fn task_run_cache_env_vars(project_id: &str, task_run_id: &str) -> Vec<EnvVar> {
    let mut env = common_cache_env_vars(project_id);
    env.push(env_var(
        "CARGO_TARGET_DIR",
        &cargo_target_run_dir(task_run_id).display().to_string(),
    ));
    env
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

    fn inventory_job(name: Option<&str>, label: Option<&str>) -> Job {
        let mut labels = BTreeMap::new();
        if let Some(label) = label {
            labels.insert(LABEL_TASK_RUN_ID.to_string(), label.to_string());
        }
        Job {
            metadata: ObjectMeta {
                name: name.map(ToString::to_string),
                labels: if labels.is_empty() {
                    None
                } else {
                    Some(labels)
                },
                ..ObjectMeta::default()
            },
            ..Job::default()
        }
    }

    fn task_run_job_envs(job: &Job) -> BTreeMap<&str, &str> {
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");
        let container = &pod.containers[0];
        container
            .env
            .as_ref()
            .expect("container.env set")
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().expect("env value")))
            .collect()
    }

    #[test]
    fn extracts_task_run_id_from_valid_label() {
        let task_run_id = Uuid::now_v7();
        let job = inventory_job(Some("unusual-job-name"), Some(&task_run_id.to_string()));

        let got = taskrun_job_ref_from_job(&job).expect("label id should be extracted");

        assert_eq!(got.job_name, "unusual-job-name");
        assert_eq!(got.task_run_id, task_run_id.to_string());
    }

    #[test]
    fn falls_back_to_canonical_name_when_label_missing_or_malformed() {
        let task_run_id = Uuid::now_v7();
        let job_name = format!("{TASKRUN_JOB_NAME_PREFIX}{task_run_id}");

        let missing_label = taskrun_job_ref_from_job(&inventory_job(Some(&job_name), None))
            .expect("canonical name should be extracted without label");
        assert_eq!(missing_label.task_run_id, task_run_id.to_string());

        let malformed_label =
            taskrun_job_ref_from_job(&inventory_job(Some(&job_name), Some("not-a-uuid")))
                .expect("canonical name should recover from malformed label");
        assert_eq!(malformed_label.task_run_id, task_run_id.to_string());
    }

    #[test]
    fn skips_unparseable_taskrun_job_candidates() {
        assert!(
            taskrun_job_ref_from_job(&inventory_job(Some("djinn-taskrun-not-a-uuid"), None))
                .is_none()
        );
        assert!(taskrun_job_ref_from_job(&inventory_job(Some("other-job"), None)).is_none());
        assert!(taskrun_job_ref_from_job(&inventory_job(None, Some("not-a-uuid"))).is_none());
        assert!(task_run_id_from_job_name("not-djinn-taskrun-id").is_none());
    }

    #[test]
    fn builds_task_run_job_manifest() {
        let cfg = KubernetesConfig::for_testing();
        let task_run_id = Uuid::now_v7();
        let secret_name = "djinn-taskrun-test";
        let project_image = "registry.example:5000/djinn-project-p:abc123def456";

        let job = build_task_run_job(&cfg, &task_run_id, "proj-xyz", secret_name, project_image);

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
        // activeDeadlineSeconds caps total Pod wall-clock — see Gap 4 in
        // the Phase 7 worker-functionality audit.
        assert_eq!(
            spec.active_deadline_seconds,
            Some(cfg.task_run_active_deadline_seconds as i64),
        );
        assert_eq!(spec.active_deadline_seconds, Some(10800));

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
        // terminationGracePeriodSeconds: the worker needs slack after
        // SIGTERM to flush a final RPC frame (TerminalReport) before the
        // kubelet escalates to SIGKILL — see Gap 4.
        assert_eq!(
            pod.termination_grace_period_seconds,
            Some(cfg.task_run_termination_grace_period_seconds),
        );
        assert_eq!(pod.termination_grace_period_seconds, Some(60));

        // Default config carries no scheduling hints — the PodSpec fields
        // must stay `None` so the manifest is byte-identical to the
        // pre-feature shape. Anything else would mean existing installs
        // started seeing nodeSelector/tolerations they didn't ask for.
        assert!(
            pod.node_selector.is_none(),
            "default config must not set nodeSelector"
        );
        assert!(
            pod.tolerations.is_none(),
            "default config must not set tolerations"
        );

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
            envs.get("DJINN_CREDENTIALS_PATH").copied(),
            Some("/var/run/djinn/credentials.bin"),
            "Phase 7a: worker reads credentials.bin from the same Secret mount"
        );
        assert_eq!(
            envs.get("DJINN_TOKEN_PATH").copied(),
            Some("/var/run/secrets/tokens/djinn")
        );
        assert_eq!(
            envs.get("DJINN_TASK_RUN_ID").copied(),
            Some(task_run_id.to_string().as_str())
        );
        assert_eq!(envs.get("TMPDIR").copied(), Some(WORKSPACE_MOUNT_DIR));
        assert_eq!(
            envs.get("DJINN_MIRROR_ROOT").copied(),
            Some(MIRROR_MOUNT_DIR)
        );
        // The in-pod soft-deadline timer reads this; it must equal the Job's
        // activeDeadlineSeconds so the supervisor's wind-down fires BEFORE the
        // kubelet hard-kills the Pod.
        assert_eq!(
            envs.get("DJINN_TASK_RUN_DEADLINE_SECONDS").copied(),
            Some(cfg.task_run_active_deadline_seconds.to_string().as_str())
        );
        // DB env vars are gated on the corresponding config fields being
        // `Some`. `for_testing()` leaves them `None`, so they should be
        // absent here — see `forwards_db_env_vars_when_configured` for
        // the populated-config case.
        assert!(
            !envs.contains_key("DJINN_DATABASE_URL"),
            "DJINN_DATABASE_URL must be absent when database_url is None"
        );

        // Volume mounts: 5 from the pre-env-config layout + the
        // environment-config mount added in P4.
        let mounts = container.volume_mounts.as_ref().expect("volume_mounts set");
        assert_eq!(mounts.len(), 6, "expected 6 volume mounts");
        let expected_mounts: [(&str, &str, Option<bool>); 6] = [
            (VOLUME_SPEC, SPEC_MOUNT_DIR, Some(true)),
            (VOLUME_AUTH_TOKEN, TOKEN_MOUNT_DIR, Some(true)),
            (VOLUME_MIRROR, MIRROR_MOUNT_DIR, Some(false)),
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
        assert_eq!(secret_src.default_mode, Some(0o0444));
        let items = secret_src.items.as_ref().expect("secret items set");
        // Phase 7a: two keys — `spec.bin` and `credentials.bin`.
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, SPEC_SECRET_KEY);
        assert_eq!(items[0].path, SPEC_SECRET_KEY);
        assert_eq!(items[1].key, CREDENTIALS_SECRET_KEY);
        assert_eq!(items[1].path, CREDENTIALS_SECRET_KEY);

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

        // mirror → PVC (read-write so the worker can push its task_branch
        // back to the mirror before delegating open_pr).
        let mirror_volume = &volumes[2];
        let mirror_pvc = mirror_volume
            .persistent_volume_claim
            .as_ref()
            .expect("mirror volume is PVC");
        assert_eq!(mirror_pvc.claim_name, cfg.mirror_pvc);
        assert_eq!(mirror_pvc.read_only, Some(false));

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

    /// When the server's `KubernetesConfig` has the DB connection vars
    /// populated, the task-run Pod must forward them so the worker's
    /// `bootstrap_warm_database()` connects to the same Postgres instance
    /// as the launcher — mirroring the warm-Pod behaviour.
    #[test]
    fn forwards_db_env_vars_when_configured() {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.database_url = Some("postgres://djinn@djinn-postgres.djinn.svc:5432/djinn".into());

        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-test",
            "registry.example:5000/djinn-project-p:abc123def456",
        );

        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");
        let container = &pod.containers[0];
        let envs: BTreeMap<&str, &str> = container
            .env
            .as_ref()
            .expect("container.env set")
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().expect("env value")))
            .collect();

        assert_eq!(
            envs.get("DJINN_DATABASE_URL").copied(),
            Some("postgres://djinn@djinn-postgres.djinn.svc:5432/djinn")
        );
    }

    /// Cargo caches must be routed to the persistent /cache PVC so Rust
    /// task-runs don't recompile the whole dependency graph cold every time.
    /// CARGO_HOME is shared (content-addressed registry); CARGO_TARGET_DIR is
    /// private per task run so task-run Pods never write the shared warm base.
    #[test]
    fn routes_cargo_caches_to_private_task_run_target_dir() {
        let cfg = KubernetesConfig::for_testing();
        let task_run_id = Uuid::now_v7();
        let expected_target_dir = format!("/cache/cargo-target-runs/{task_run_id}");

        let job = build_task_run_job(
            &cfg,
            &task_run_id,
            "proj-xyz",
            "djinn-taskrun-test",
            "registry.example:5000/djinn-project-p:abc123def456",
        );

        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");
        let container = &pod.containers[0];
        let envs: BTreeMap<&str, &str> = container
            .env
            .as_ref()
            .expect("container.env set")
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().expect("env value")))
            .collect();

        assert_eq!(envs.get("CARGO_HOME").copied(), Some("/cache/cargo"));
        assert_eq!(
            envs.get("CARGO_TARGET_DIR").copied(),
            Some(expected_target_dir.as_str()),
            "task-run target dir must be private so task-runs never write the shared warm base"
        );
        assert_ne!(
            envs.get("CARGO_TARGET_DIR").copied(),
            Some("/cache/cargo-target/proj-xyz"),
            "task-run target dir must not regress to the shared per-project warm base"
        );
        assert_ne!(
            envs.get("CARGO_TARGET_DIR").copied(),
            Some("/cache/cargo-target-runs/djinn-taskrun-test"),
            "task-run target dir must use the canonical task_run_id, not the generated Kubernetes job name"
        );
        assert_eq!(
            envs.get("SCCACHE_DIR").copied(),
            Some("/cache/sccache/proj-xyz"),
            "sccache dir must be on the PVC and namespaced per project (no multi-writer corruption)"
        );
        assert_eq!(envs.get("SCCACHE_CACHE_SIZE").copied(), Some("20G"));
        assert_eq!(
            envs.get("SQLX_OFFLINE").copied(),
            Some("true"),
            "build-in-pod has no DB; sqlx macros must use the committed .sqlx cache"
        );
    }

    #[test]
    fn same_project_task_runs_get_distinct_private_cargo_target_dirs() {
        let cfg = KubernetesConfig::for_testing();
        let first_task_run_id = Uuid::now_v7();
        let second_task_run_id = Uuid::now_v7();
        let project_id = "proj-xyz";

        let first_job = build_task_run_job(
            &cfg,
            &first_task_run_id,
            project_id,
            "djinn-taskrun-first",
            "registry.example:5000/djinn-project-p:abc123def456",
        );
        let second_job = build_task_run_job(
            &cfg,
            &second_task_run_id,
            project_id,
            "djinn-taskrun-second",
            "registry.example:5000/djinn-project-p:abc123def456",
        );

        let first_envs = task_run_job_envs(&first_job);
        let second_envs = task_run_job_envs(&second_job);
        let first_target_dir = first_envs
            .get("CARGO_TARGET_DIR")
            .copied()
            .expect("first job CARGO_TARGET_DIR set");
        let second_target_dir = second_envs
            .get("CARGO_TARGET_DIR")
            .copied()
            .expect("second job CARGO_TARGET_DIR set");

        assert_ne!(
            first_target_dir, second_target_dir,
            "same-project task-runs must not contend on one cargo target dir"
        );
        assert_eq!(
            first_target_dir,
            format!("/cache/cargo-target-runs/{first_task_run_id}")
        );
        assert_eq!(
            second_target_dir,
            format!("/cache/cargo-target-runs/{second_task_run_id}")
        );
        assert!(first_target_dir.starts_with("/cache/cargo-target-runs/"));
        assert!(second_target_dir.starts_with("/cache/cargo-target-runs/"));

        for envs in [&first_envs, &second_envs] {
            assert_eq!(envs.get("CARGO_HOME").copied(), Some("/cache/cargo"));
            assert_eq!(
                envs.get("SCCACHE_DIR").copied(),
                Some("/cache/sccache/proj-xyz")
            );
            assert_eq!(envs.get("SCCACHE_CACHE_SIZE").copied(), Some("20G"));
            assert_eq!(envs.get("SQLX_OFFLINE").copied(), Some("true"));
        }
    }

    /// When the operator has configured nodeSelector + tolerations (typical
    /// case: a dedicated NodePool tainted/labelled for djinn builds), the
    /// task-run PodSpec must carry both so the scheduler picks the right
    /// pool *and* the kubelet doesn't reject the Pod at admission.
    #[test]
    fn task_run_pod_scheduling_propagates_from_config() {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.node_selector
            .insert("workload-type".into(), "djinn".into());
        cfg.tolerations.push(Toleration {
            key: Some("workload-type".into()),
            operator: Some("Equal".into()),
            value: Some("djinn".into()),
            effect: Some("NoSchedule".into()),
            ..Toleration::default()
        });

        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-test",
            "registry.example:5000/djinn-project-p:abc123def456",
        );

        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");

        let ns = pod.node_selector.as_ref().expect("nodeSelector set");
        assert_eq!(ns.get("workload-type").map(String::as_str), Some("djinn"));

        let tols = pod.tolerations.as_ref().expect("tolerations set");
        assert_eq!(tols.len(), 1);
        assert_eq!(tols[0].key.as_deref(), Some("workload-type"));
        assert_eq!(tols[0].operator.as_deref(), Some("Equal"));
        assert_eq!(tols[0].value.as_deref(), Some("djinn"));
        assert_eq!(tols[0].effect.as_deref(), Some("NoSchedule"));
    }
}
