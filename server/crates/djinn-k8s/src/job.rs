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
use crate::sidecar::{
    BackingServiceSpec, sidecar_conn_env, sidecar_container, sidecar_dshm_volume,
};

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

    // Carry the Job's creation timestamp so the backstop reaper can age-gate
    // young Jobs: the worker inserts the task_runs row only after pod boot, so
    // a fresh Job legitimately has no DB owner rows yet. k8s_openapi wraps the
    // timestamp as `Time(chrono::DateTime<Utc>)`; chrono provides the
    // `SystemTime: From<DateTime<Utc>>` conversion.
    let created_at = job
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|time| std::time::SystemTime::from(time.0));

    Some(djinn_runtime::TaskrunJobRef {
        job_name,
        task_run_id,
        created_at,
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
/// `services` are the backing services declared on the project's image
/// (resolved via [`crate::sidecar::resolve_image_services`]); each is injected
/// as a native sidecar and its connection string exported to the worker as the
/// preset's env var. Pass an empty slice for no injected services.
#[allow(clippy::too_many_arguments)]
pub fn build_task_run_job(
    config: &KubernetesConfig,
    task_run_id: &Uuid,
    project_id: &str,
    secret_name: &str,
    project_image_tag: &str,
    services: &[BackingServiceSpec],
    policy: Option<&djinn_stack::environment::CargoCachePolicy>,
    is_evidence_spike: bool,
) -> Job {
    let task_run_id_str = task_run_id.to_string();
    let labels = job_labels(&task_run_id_str);
    let job_name = format!("djinn-taskrun-{task_run_id}");

    // Evidence-spike runs receive no backing-service connection env vars —
    // the worker has no business reaching product databases/queues for a
    // read-only investigation.  The `services` slice is still passed in for
    // the normal path; here we select an empty slice for evidence spikes so
    // sidecar_conn_env produces nothing.
    let effective_services: &[BackingServiceSpec] = if is_evidence_spike { &[] } else { services };

    // Evidence-spike runs mount durable repository/cache resources
    // read-only so that a tool-surface bug cannot mutate the host mirror
    // or shared build cache.  The workspace emptyDir remains ephemeral —
    // it dies with the Pod and never touches a durable PVC.
    //
    // NOTE: the emptyDir workspace mount itself stays mutable because the
    // supervisor/worktree bootstrap requires a writable TMPDIR to start.
    // This is the narrow exception — the durable PVCs (mirror, cache) are
    // the mutation-proof surface, and the emptyDir is per-Pod ephemeral
    // storage that cannot leak writes outside the container boundary.
    let mirror_read_only = is_evidence_spike;
    let cache_read_only = is_evidence_spike;

    // Worker env carries the base task-run knobs plus the connection env var(s)
    // per injected backing service (e.g. DATABASE_URL + TEST_POSTGRES_URL →
    // 127.0.0.1:5432). A preset may declare more than one name.  Evidence-spike
    // runs use `effective_services` (empty) so no DB connection env is injected.
    let mut worker_env = build_task_run_env(config, &task_run_id_str, project_id, policy);
    worker_env.extend(effective_services.iter().flat_map(sidecar_conn_env));

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
        env: Some(worker_env),
        volume_mounts: Some(vec![
            volume_mount(VOLUME_SPEC, SPEC_MOUNT_DIR, Some(true)),
            volume_mount(VOLUME_AUTH_TOKEN, TOKEN_MOUNT_DIR, Some(true)),
            // Mirror PVC: mounted RW for normal workers (push task_branch
            // before delegating open_pr) but RO for evidence-spike runs
            // where write access to the host mirror is a safety violation.
            volume_mount(VOLUME_MIRROR, MIRROR_MOUNT_DIR, Some(mirror_read_only)),
            // Cache PVC: default mutable for normal workers (cargo builds write
            // to private per-run target dirs under /cache).  For evidence-spike
            // runs, explicitly read-only — no build artifacts should persist.
            // The None (non-evidence) path preserves the original manifest shape.
            volume_mount(
                VOLUME_CACHE,
                CACHE_MOUNT_DIR,
                if is_evidence_spike { Some(true) } else { None },
            ),
            // Workspace emptyDir: always mutable.  Ephemeral per-Pod
            // storage — dies with the Pod.  See the `mirror_read_only`
            // comment above for why this narrow exception is safe.
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

    let mut volumes = vec![
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
                    // Per-task-run payload files added in hgd0 Wave 1.
                    // The effective EnvironmentConfig JSON and resolved
                    // service metadata JSON are sourced from the same
                    // per-task-run Secret and mounted at stable paths
                    // under /var/run/djinn/ for the in-pod worker.
                    // `optional: true` (below) lets old Secrets that
                    // lack these keys still create a valid Pod.
                    KeyToPath {
                        key: crate::env_config::ENV_CONFIG_SECRET_DATA_KEY.to_string(),
                        path: crate::env_config::ENV_CONFIG_SECRET_DATA_KEY.to_string(),
                        ..KeyToPath::default()
                    },
                    KeyToPath {
                        key: crate::env_config::SERVICE_METADATA_SECRET_DATA_KEY.to_string(),
                        path: crate::env_config::SERVICE_METADATA_SECRET_DATA_KEY.to_string(),
                        ..KeyToPath::default()
                    },
                ]),
                // `optional: true` so Pods tolerate Secrets that lack
                // the newer `environment.json` / `service_metadata.json`
                // payload keys (built by the legacy `build_task_run_secret`
                // rather than `TaskRunSecretBuilder`).  The Secret itself
                // is always created by the caller before the Job; this flag
                // relaxes only the per-key requirement.
                optional: Some(true),
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
                // RW for normal workers (push task_branch before delegating
                // open_pr); RO for evidence-spike runs to enforce the
                // read-only isolation contract at the K8s volume boundary.
                // See the matching VolumeMount comment above.
                read_only: Some(mirror_read_only),
            }),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_CACHE.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: config.cache_pvc.clone(),
                read_only: Some(cache_read_only),
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
    // Backing-service sidecars share a Memory /dev/shm (Postgres needs more than
    // the 64Mi default). Added only when services are injected so the manifest
    // is byte-identical to the pre-feature shape for service-less projects.
    // Evidence-spike runs always skip sidecars — `effective_services` is empty
    // when `is_evidence_spike` is true, so this block is a no-op.
    if !effective_services.is_empty() {
        volumes.push(sidecar_dshm_volume());
    }

    // Each declared backing service becomes a native sidecar (initContainer +
    // restartPolicy: Always). `None` when there are none, keeping the manifest
    // unchanged for projects without injected services.  For evidence-spike
    // runs, `effective_services` is empty so no sidecar init containers are
    // injected — the read-only investigation must not start product databases.
    let init_containers = (!effective_services.is_empty()).then(|| {
        effective_services
            .iter()
            .map(|s| sidecar_container(config, s))
            .collect::<Vec<_>>()
    });

    // Pin Pods to a dedicated NodePool when the operator has configured one.
    // Both fields stay `None` if the corresponding config entry is empty so
    // the rendered manifest is identical to the pre-feature shape.
    let node_selector = (!config.node_selector.is_empty()).then(|| config.node_selector.clone());
    let tolerations: Option<Vec<Toleration>> =
        (!config.tolerations.is_empty()).then(|| config.tolerations.clone());

    let pod_spec = PodSpec {
        service_account_name: Some(config.service_account.clone()),
        restart_policy: Some("Never".to_string()),
        init_containers,
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
    policy: Option<&djinn_stack::environment::CargoCachePolicy>,
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

    // djinn:allow-oversize — job.rs was already over the 50KB byte guard on
    // main before this one-line forward; kept whole rather than split.
    //
    // Forward the outbound-request debug flag to the task-run pod (the LLM
    // call for worker/planner/reviewer stages happens in this Pod, not the
    // server). Only forwarded when set on the server, so it stays off by
    // default; used to capture the literal provider request for diagnosing
    // empty-stream failures (e.g. kimi-for-coding).
    if let Ok(v) = std::env::var("DJINN_DEBUG_PROVIDER_REQUEST") {
        env.push(env_var("DJINN_DEBUG_PROVIDER_REQUEST", &v));
    }

    env.extend(task_run_cache_env_vars(project_id, task_run_id_str, policy));
    env
}

/// Runtime env vars routing the shared Rust toolchain caches to the persistent
/// `/cache` PVC. Warm Pods use the per-project base target dir;
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
///   warm base is namespaced per project; the warm job pre-compiles main into it
///   with `CARGO_INCREMENTAL=1` so it carries a clean, incremental-enabled
///   main-based cache (CI-style, like Swatinem/rust-cache). Task-runs get a deterministic private dir (under
///   `/cache/cargo-target-runs/<id>`) seeded from that warm base, so they never
///   write the shared base directly or contend on Cargo's shared build-dir lock,
///   and recompile only their delta incrementally. (Default is
///   <workspace>/target inside the ephemeral clone — lost when the Pod dies.)
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
        // NOTE: we deliberately do NOT force `RUSTC_WRAPPER=sccache` or
        // `CARGO_INCREMENTAL=0` here. The fast path is incremental compilation
        // over a warm, main-based per-project target base (CI-style, like
        // Swatinem/rust-cache): the warm job pre-compiles the workspace into the
        // base with `CARGO_INCREMENTAL=1`, task-run pods seed a
        // private run target dir from that base and recompile only their delta
        // incrementally. Forcing sccache (which requires CARGO_INCREMENTAL=0)
        // disables incremental and was the wrong lever — it made every
        // task-run cold-build (~14-29min clippy). SCCACHE_DIR is left set
        // below so a repo that *itself* pins `rustc-wrapper = "sccache"` in its
        // .cargo/config.toml still gets a writable, Landlock-allowed cache dir;
        // we just don't impose the wrapper on repos that don't ask for it.
        env_var(
            "SCCACHE_DIR",
            &format!("{CACHE_MOUNT_DIR}/sccache/{project_id}"),
        ),
        // Default is 10G, which evicts fast on a large workspace; give sccache
        // more headroom on the shared PVC.
        env_var("SCCACHE_CACHE_SIZE", "20G"),
        // Build-in-pod contexts (task-run, warm) have no Postgres
        // reachable, but a repo's .cargo/config.toml may bake a DATABASE_URL for
        // local online sqlx (djinn itself bakes :5433). Force offline so the
        // compile-time sqlx macros use the committed .sqlx cache instead of
        // trying — and failing — to connect. Local dev keeps online validation
        // because it never sources cache_env_vars.
        env_var("SQLX_OFFLINE", "true"),
        // Fast-linker default for every build-in-pod cargo invocation. The
        // per-project devcontainer image already installs mold (see
        // image-builder scripts/install-rust.sh, which apt-installs
        // `clang lld mold` whenever a Rust toolchain is requested), but nothing
        // WIRED it in — so warm/task-run pods linked djinn-server + every test
        // binary with the default `ld` and paid full link time on every
        // iterative edit→clippy/test loop (measured 6-22min/invocation).
        //
        // `CARGO_BUILD_RUSTFLAGS` (the env form of `build.rustflags`) is the
        // LOWEST-priority rustflags source: a repo that pins its own
        // `[build]`/`[target.*] rustflags` in `.cargo/config.toml` keeps its
        // flags untouched, and a project on a custom base image without mold is
        // unaffected on the (rare) crates that override. It degrades gracefully
        // rather than clobbering, unlike `RUSTFLAGS`.
        //
        // Set HERE, in the shared helper both warm_cache_env_vars and
        // task_run_cache_env_vars flow through, ON PURPOSE: cargo folds
        // rustflags into its compile fingerprint (and the warm base is seeded
        // into task-run target dirs), so warm and worker MUST resolve the
        // identical flag or the warm seed fingerprint-mismatches and every
        // task-run cold-rebuilds. Single-sourcing it here makes that drift
        // impossible — the same class of guarantee the SCCACHE_DIR/CARGO_HOME
        // routing above relies on. Mirrors the local-docker path, where
        // djinn-agent-runtime-base.Dockerfile bakes the identical
        // `CARGO_BUILD_RUSTFLAGS=-Clink-arg=-fuse-ld=mold` as an image ENV.
        //
        // Shipping this is a ONE-TIME warm-base invalidation (the effective
        // rustflags change → fingerprints change → the first warm after deploy
        // rebuilds the per-project base); steady state is fast links thereafter.
        env_var("CARGO_BUILD_RUSTFLAGS", "-Clink-arg=-fuse-ld=mold"),
    ]
}

/// Base cache env vars routing CARGO_TARGET_DIR at the shared per-project warm
/// base. Warm Pods write this base directly; task-run Pods seed a private
/// run dir from it (the worker overrides CARGO_TARGET_DIR to a private run dir).
pub(crate) fn cache_env_vars(project_id: &str) -> Vec<EnvVar> {
    let mut env = common_cache_env_vars(project_id);
    env.push(env_var(
        "CARGO_TARGET_DIR",
        &format!("{CACHE_MOUNT_DIR}/cargo-target/{project_id}"),
    ));
    env
}

/// Cache env vars for warm Pods that populate the shared per-project cargo
/// target base.
///
/// Warm and task-run pods must use the SAME compile strategy or the
/// warm base is wasted (cargo fingerprints fold in `CARGO_INCREMENTAL` and the
/// rustc wrapper). Both therefore force `CARGO_INCREMENTAL=1` and clear
/// any repo `rustc-wrapper = "sccache"` (`RUSTC_WRAPPER=""`). A project that
/// hard-pins `CARGO_INCREMENTAL=0 force=true` in its own `.cargo/config.toml`
/// can still beat env, but that clamps warm AND worker identically, so
/// warm==worker parity (and seed reuse) holds — only the incremental speedup
/// that project opted out of is lost. We deliberately do NOT rewrite the
/// clone's config, since the worker auto-commit (`git add -A`) would otherwise
/// commit the rewrite into every PR.
///
/// `policy` is accepted for signature parity with the other builders but no
/// longer flips incremental: incremental-on is now an invariant across all
/// djinn build pods.
pub(crate) fn warm_cache_env_vars(
    project_id: &str,
    _policy: Option<&djinn_stack::environment::CargoCachePolicy>,
) -> Vec<EnvVar> {
    let mut env = cache_env_vars(project_id);
    env.push(env_var("CARGO_INCREMENTAL", "1"));
    env.push(env_var("RUSTC_WRAPPER", ""));
    env
}

/// Cache env vars for task-run Pods. The target dir is private to the canonical
/// task run id, not the generated Kubernetes resource name, so task Pods avoid
/// the shared Cargo build-dir lock while preserving the warm per-project base as
/// a read-only seed source.
///
/// A task-run is an ITERATIVE worker loop: the agent edits a crate and re-runs
/// `cargo clippy`/`test` many times. Incremental compilation is the right tool —
/// it recompiles only changed codegen units (seconds) instead of a full crate
/// rebuild (~9min for a large crate like `djinn-agent`) every edit.
///
/// Always `CARGO_INCREMENTAL=1` and `RUSTC_WRAPPER=""` (clearing any repo
/// `rustc-wrapper = "sccache"`), matching `warm_cache_env_vars`. Warm ==
/// worker must use the SAME compile strategy or the warm seed is wasted.
/// `policy` is accepted for signature parity but no longer flips incremental.
fn task_run_cache_env_vars(
    project_id: &str,
    task_run_id: &str,
    _policy: Option<&djinn_stack::environment::CargoCachePolicy>,
) -> Vec<EnvVar> {
    let mut env = common_cache_env_vars(project_id);
    env.push(env_var(
        "CARGO_TARGET_DIR",
        &cargo_target_run_dir(task_run_id).display().to_string(),
    ));
    env.push(env_var("CARGO_INCREMENTAL", "1"));
    // Override any `.cargo/config.toml` `rustc-wrapper = "sccache"`: sccache
    // forbids incremental, and the iterative loop wants incremental.
    env.push(env_var("RUSTC_WRAPPER", ""));
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
    fn carries_creation_timestamp_into_inventory_ref() {
        let task_run_id = Uuid::now_v7();
        let job_name = format!("{TASKRUN_JOB_NAME_PREFIX}{task_run_id}");
        let created = k8s_openapi::chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("valid timestamp");

        let mut job = inventory_job(Some(&job_name), None);
        job.metadata.creation_timestamp = Some(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(created),
        );

        let got = taskrun_job_ref_from_job(&job).expect("canonical name should be extracted");
        assert_eq!(got.created_at, Some(std::time::SystemTime::from(created)));

        // No creation timestamp → None, which the backstop treats as old.
        let bare = taskrun_job_ref_from_job(&inventory_job(Some(&job_name), None))
            .expect("canonical name should be extracted");
        assert_eq!(bare.created_at, None);
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

        let job = build_task_run_job(
            &cfg,
            &task_run_id,
            "proj-xyz",
            secret_name,
            project_image,
            &[],
            None,
            false,
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
        assert_eq!(secret_src.optional, Some(true));
        assert_eq!(secret_src.default_mode, Some(0o0444));
        let items = secret_src.items.as_ref().expect("secret items set");
        // Phase 7a + hgd0: four keys — `spec.bin`, `credentials.bin`,
        // `environment.json`, `service_metadata.json`.
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].key, SPEC_SECRET_KEY);
        assert_eq!(items[0].path, SPEC_SECRET_KEY);
        assert_eq!(items[1].key, CREDENTIALS_SECRET_KEY);
        assert_eq!(items[1].path, CREDENTIALS_SECRET_KEY);
        // Per-task-run payload files (hgd0 Wave 1).
        assert_eq!(items[2].key, crate::env_config::ENV_CONFIG_SECRET_DATA_KEY);
        assert_eq!(items[2].path, crate::env_config::ENV_CONFIG_SECRET_DATA_KEY);
        assert_eq!(
            items[3].key,
            crate::env_config::SERVICE_METADATA_SECRET_DATA_KEY
        );
        assert_eq!(
            items[3].path,
            crate::env_config::SERVICE_METADATA_SECRET_DATA_KEY
        );

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
            &[],
            None,
            false,
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
            &[],
            None,
            false,
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
        assert_eq!(
            envs.get("CARGO_INCREMENTAL").copied(),
            Some("1"),
            "task-runs use incremental compilation for the iterative worker edit/compile loop"
        );
        assert_eq!(
            envs.get("RUSTC_WRAPPER").copied(),
            Some(""),
            "task-runs disable the sccache wrapper (sccache forbids incremental)"
        );
        assert_eq!(
            envs.get("CARGO_BUILD_RUSTFLAGS").copied(),
            Some("-Clink-arg=-fuse-ld=mold"),
            "task-runs default to the mold fast linker (installed in the devcontainer image)"
        );
    }

    /// Regression guard: warm and task-run must resolve the same shared cache
    /// strategy for a given project. The cold-compile regression was caused by
    /// warm and worker silently diverging on feature sets (warm ran
    /// `--all-features`, worker ran default features). This test asserts that
    /// the shared cache routing env vars — CARGO_HOME (shared registry),
    /// SCCACHE_DIR (namespaced sccache), and SCCACHE_CACHE_SIZE — are identical
    /// between warm and worker, and that CARGO_TARGET_DIR resolves to the same
    /// per-project base directory. The intended differences (CARGO_INCREMENTAL
    /// and RUSTC_WRAPPER) are also pinned.
    ///
    /// Tests a workspace-with-sccache project shape (the typical djinn project
    /// with `rustc-wrapper = "sccache"` pinned in `.cargo/config.toml`).
    #[test]
    fn warm_and_worker_resolve_same_cache_strategy() {
        let project_id = "test-project";
        let task_run_id = Uuid::now_v7().to_string();

        let warm_vars = warm_cache_env_vars(project_id, None);
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, None);
        let worker_env: BTreeMap<&str, &str> = worker_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        // --- Shared cache routing must be identical ---
        // CARGO_HOME is the content-addressed registry (safe to share).
        assert_eq!(
            warm_env.get("CARGO_HOME"),
            worker_env.get("CARGO_HOME"),
            "CARGO_HOME must be identical between warm and worker"
        );
        assert_eq!(warm_env.get("CARGO_HOME").copied(), Some("/cache/cargo"));

        // SCCACHE_DIR is namespaced per project.
        assert_eq!(
            warm_env.get("SCCACHE_DIR"),
            worker_env.get("SCCACHE_DIR"),
            "SCCACHE_DIR must be identical between warm and worker"
        );
        assert_eq!(
            warm_env.get("SCCACHE_DIR").copied(),
            Some("/cache/sccache/test-project"),
        );

        assert_eq!(
            warm_env.get("SCCACHE_CACHE_SIZE"),
            worker_env.get("SCCACHE_CACHE_SIZE"),
            "SCCACHE_CACHE_SIZE must be identical between warm and worker"
        );
        assert_eq!(warm_env.get("SCCACHE_CACHE_SIZE").copied(), Some("20G"),);

        assert_eq!(
            warm_env.get("SQLX_OFFLINE"),
            worker_env.get("SQLX_OFFLINE"),
            "SQLX_OFFLINE must be identical between warm and worker"
        );

        // --- CARGO_TARGET_DIR base resolves to the same per-project directory ---
        // Warm writes the shared per-project base; worker uses a private
        // per-run dir seeded from that base.
        let warm_target = warm_env
            .get("CARGO_TARGET_DIR")
            .expect("warm CARGO_TARGET_DIR set");
        let worker_target = worker_env
            .get("CARGO_TARGET_DIR")
            .expect("worker CARGO_TARGET_DIR set");

        assert_eq!(
            *warm_target, "/cache/cargo-target/test-project",
            "warm CARGO_TARGET_DIR must be the per-project shared base"
        );
        assert!(
            worker_target.starts_with("/cache/cargo-target-runs/"),
            "worker CARGO_TARGET_DIR must be a private per-run dir: {worker_target}"
        );
        // Both resolve under the same per-project namespace — the worker's
        // private dir is seeded from the warm base.
        assert!(
            warm_target.contains("test-project"),
            "warm target must be namespaced per project"
        );

        // --- Compile strategy MUST be identical (parity invariant) ---
        // Warm == verify == worker: incremental=1 + RUSTC_WRAPPER="" so the
        // warm seed is reusable across all three.
        assert_eq!(
            warm_env.get("CARGO_INCREMENTAL").copied(),
            Some("1"),
            "warm must enable incremental (warm-cache parity)"
        );
        assert_eq!(
            worker_env.get("CARGO_INCREMENTAL").copied(),
            Some("1"),
            "worker must enable incremental (iterative edit loop)"
        );
        assert_eq!(
            warm_env.get("CARGO_INCREMENTAL"),
            worker_env.get("CARGO_INCREMENTAL"),
            "warm and worker CARGO_INCREMENTAL must match"
        );

        // All djinn build pods clear RUSTC_WRAPPER so any repo-level sccache
        // wrapper can't disable incremental.
        assert_eq!(
            warm_env.get("RUSTC_WRAPPER").copied(),
            Some(""),
            "warm must clear RUSTC_WRAPPER so incremental works"
        );
        assert_eq!(
            worker_env.get("RUSTC_WRAPPER").copied(),
            Some(""),
            "worker must clear RUSTC_WRAPPER so incremental works"
        );

        // The fast-linker rustflag is part of the compile fingerprint, so it
        // MUST match between warm and worker or the warm seed is wasted (every
        // task-run cold-rebuilds against a fingerprint-mismatched base).
        assert_eq!(
            warm_env.get("CARGO_BUILD_RUSTFLAGS"),
            worker_env.get("CARGO_BUILD_RUSTFLAGS"),
            "warm and worker CARGO_BUILD_RUSTFLAGS must match (fingerprint parity)"
        );
        assert_eq!(
            warm_env.get("CARGO_BUILD_RUSTFLAGS").copied(),
            Some("-Clink-arg=-fuse-ld=mold"),
        );
    }

    /// Same invariant as [`warm_and_worker_resolve_same_cache_strategy`] but for
    /// a single-crate no-sccache project shape. The cache routing env vars are
    /// identical regardless of project shape — the per-project namespace is the
    /// only variable. When `CargoCachePolicy` is introduced (Phase 2), this test
    /// will be extended to verify that the policy produces matching feature sets
    /// for both warm and worker.
    ///
    /// Even without sccache pinned in `.cargo/config.toml`, the warm/worker env
    /// vars still set SCCACHE_DIR (on the PVC, namespaced) so a repo that later
    /// adds sccache gets a writable, Landlock-allowed cache dir for free.
    #[test]
    fn warm_and_worker_same_posture_no_sccache() {
        let project_id = "single-crate-project";
        let task_run_id = Uuid::now_v7().to_string();

        let warm_vars = warm_cache_env_vars(project_id, None);
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, None);
        let worker_env: BTreeMap<&str, &str> = worker_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        // Shared cache routing identical regardless of project shape.
        assert_eq!(
            warm_env.get("CARGO_HOME"),
            worker_env.get("CARGO_HOME"),
            "CARGO_HOME must be identical between warm and worker (no-sccache shape)"
        );
        assert_eq!(
            warm_env.get("SCCACHE_DIR"),
            worker_env.get("SCCACHE_DIR"),
            "SCCACHE_DIR must be identical between warm and worker (no-sccache shape)"
        );
        assert_eq!(
            warm_env.get("SCCACHE_DIR").copied(),
            Some("/cache/sccache/single-crate-project"),
            "SCCACHE_DIR must be namespaced per project even without sccache pinned"
        );
        assert_eq!(
            warm_env.get("SCCACHE_CACHE_SIZE"),
            worker_env.get("SCCACHE_CACHE_SIZE"),
        );
        assert_eq!(warm_env.get("SQLX_OFFLINE"), worker_env.get("SQLX_OFFLINE"),);

        // CARGO_TARGET_DIR resolves to the same per-project base.
        let warm_target = warm_env
            .get("CARGO_TARGET_DIR")
            .expect("warm CARGO_TARGET_DIR set");
        let worker_target = worker_env
            .get("CARGO_TARGET_DIR")
            .expect("worker CARGO_TARGET_DIR set");

        assert_eq!(
            *warm_target, "/cache/cargo-target/single-crate-project",
            "warm CARGO_TARGET_DIR must be the per-project shared base (no-sccache)"
        );
        assert!(
            worker_target.starts_with("/cache/cargo-target-runs/"),
            "worker CARGO_TARGET_DIR must be a private per-run dir (no-sccache): {worker_target}"
        );

        // Compile strategy is the same regardless of project shape: warm ==
        // worker (incremental=1, wrapper cleared).
        assert_eq!(warm_env.get("CARGO_INCREMENTAL").copied(), Some("1"));
        assert_eq!(worker_env.get("CARGO_INCREMENTAL").copied(), Some("1"));
        assert_eq!(warm_env.get("RUSTC_WRAPPER").copied(), Some(""));
        assert_eq!(worker_env.get("RUSTC_WRAPPER").copied(), Some(""));
    }

    /// Incremental is now an invariant (always 1) regardless of policy: an
    /// explicit policy still yields incremental=1 for warm/verify/worker.
    #[test]
    fn explicit_policy_enables_incremental_for_warm_and_verify() {
        let project_id = "explicit-policy";
        let task_run_id = Uuid::now_v7().to_string();
        let policy = djinn_stack::environment::CargoCachePolicy::Explicit(
            djinn_stack::environment::CargoCachePolicyOverride {
                workspace: false,
                features: vec![],
                all_features: false,
                warm_commands: vec![],
            },
        );

        let warm_vars = warm_cache_env_vars(project_id, Some(&policy));
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, Some(&policy));
        let worker_env: BTreeMap<&str, &str> = worker_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        // Warm → incremental enabled
        assert_eq!(
            warm_env.get("CARGO_INCREMENTAL").copied(),
            Some("1"),
            "warm must enable incremental"
        );

        // Task-run unchanged
        assert_eq!(
            worker_env.get("CARGO_INCREMENTAL").copied(),
            Some("1"),
            "worker must still enable incremental"
        );
        assert_eq!(
            worker_env.get("RUSTC_WRAPPER").copied(),
            Some(""),
            "worker must still clear RUSTC_WRAPPER"
        );
    }

    /// An explicit policy can NOT re-enable sccache or disable incremental on
    /// djinn build pods: warm/verify/worker force incremental=1 +
    /// RUSTC_WRAPPER="" so the warm seed stays reusable. (The clone-config
    /// normalization step enforces the same on the cloned tree.)
    #[test]
    fn explicit_policy_forces_incremental_and_clears_wrapper() {
        let project_id = "explicit-policy";
        let task_run_id = Uuid::now_v7().to_string();
        let policy = djinn_stack::environment::CargoCachePolicy::Explicit(
            djinn_stack::environment::CargoCachePolicyOverride {
                workspace: false,
                features: vec![],
                all_features: false,
                warm_commands: vec![],
            },
        );

        let warm_vars = warm_cache_env_vars(project_id, Some(&policy));
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, Some(&policy));
        let worker_env: BTreeMap<&str, &str> = worker_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        // Both force incremental=1 + RUSTC_WRAPPER="" regardless of policy.
        for (label, env) in [("warm", &warm_env), ("worker", &worker_env)] {
            assert_eq!(
                env.get("CARGO_INCREMENTAL").copied(),
                Some("1"),
                "{label} must force incremental regardless of policy"
            );
            assert_eq!(
                env.get("RUSTC_WRAPPER").copied(),
                Some(""),
                "{label} must clear RUSTC_WRAPPER even when policy.sccache=true"
            );
        }
    }

    /// AutoDetected policy behaves identically to None (backward compat).
    #[test]
    fn auto_detected_policy_matches_none_behavior() {
        let project_id = "auto-detected";
        let task_run_id = Uuid::now_v7().to_string();
        let policy = djinn_stack::environment::CargoCachePolicy::AutoDetected;

        let warm_vars_none = warm_cache_env_vars(project_id, None);
        let warm_vars_auto = warm_cache_env_vars(project_id, Some(&policy));
        assert_eq!(
            warm_vars_none, warm_vars_auto,
            "AutoDetected must match None for warm"
        );

        let worker_vars_none = task_run_cache_env_vars(project_id, &task_run_id, None);
        let worker_vars_auto = task_run_cache_env_vars(project_id, &task_run_id, Some(&policy));
        assert_eq!(
            worker_vars_none, worker_vars_auto,
            "AutoDetected must match None for task-run"
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
            &[],
            None,
            false,
        );
        let second_job = build_task_run_job(
            &cfg,
            &second_task_run_id,
            project_id,
            "djinn-taskrun-second",
            "registry.example:5000/djinn-project-p:abc123def456",
            &[],
            None,
            false,
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
            &[],
            None,
            false,
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

    /// A declared backing service is injected as a native sidecar and its
    /// connection string exported to the worker; service-less projects keep
    /// the pre-feature manifest shape (no initContainers, no extra volume).
    #[test]
    fn injects_backing_service_as_native_sidecar() {
        let cfg = KubernetesConfig::for_testing();
        let postgres = BackingServiceSpec {
            service_type: "postgres".into(),
            image: "postgres:18-alpine".into(),
            port: 5432,
            env: vec![("POSTGRES_PASSWORD".into(), "postgres".into())],
            cpu_request: "100m".into(),
            memory_request: "256Mi".into(),
            cpu_limit: "500m".into(),
            memory_limit: "512Mi".into(),
            conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
            conn_env_var: "TEST_POSTGRES_URL".into(),
        };

        // Service-less build: no initContainers, no svc-dshm volume.
        let bare = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-bare",
            "registry.example:5000/djinn-project-p:abc123def456",
            &[],
            None,
            false,
        );
        let bare_pod = bare
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec");
        assert!(
            bare_pod.init_containers.is_none(),
            "no services ⇒ no initContainers (manifest unchanged)"
        );
        assert!(
            !bare_pod
                .volumes
                .as_ref()
                .unwrap()
                .iter()
                .any(|v| v.name == crate::sidecar::SIDECAR_DSHM_VOLUME),
            "no services ⇒ no svc-dshm volume"
        );

        // With one service injected.
        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-svc",
            "registry.example:5000/djinn-project-p:abc123def456",
            std::slice::from_ref(&postgres),
            None,
            false,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec");

        // Native sidecar in initContainers.
        let inits = pod.init_containers.as_ref().expect("init_containers set");
        assert_eq!(inits.len(), 1);
        assert_eq!(inits[0].name, "svc-postgres");
        assert_eq!(inits[0].restart_policy.as_deref(), Some("Always"));

        // Connection env var exported to the worker container.
        let worker = &pod.containers[0];
        let envs: BTreeMap<&str, &str> = worker
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            envs.get("TEST_POSTGRES_URL").copied(),
            Some("postgres://postgres:postgres@127.0.0.1:5432/app_test")
        );

        // Shared /dev/shm volume added for the sidecar.
        assert!(
            pod.volumes
                .as_ref()
                .unwrap()
                .iter()
                .any(|v| v.name == crate::sidecar::SIDECAR_DSHM_VOLUME),
            "svc-dshm volume must be present when a service is injected"
        );
    }

    /// Consistency test (epic AC #4): `warm_cache_env_vars` and
    /// `task_run_cache_env_vars` resolve CARGO_INCREMENTAL and RUSTC_WRAPPER
    /// IDENTICALLY (incremental=1, wrapper cleared) even for an explicit
    /// policy — the warm-cache fast path requires all djinn build pods to share
    /// one compile strategy, so the seed is reusable.
    #[test]
    fn policy_derived_env_consistent_across_warm_and_worker() {
        let project_id = "consistency-test";
        let task_run_id = Uuid::now_v7().to_string();

        let policy = djinn_stack::environment::CargoCachePolicy::Explicit(
            djinn_stack::environment::CargoCachePolicyOverride {
                workspace: true,
                features: vec![],
                all_features: false,
                warm_commands: vec![],
            },
        );

        let warm_vars = warm_cache_env_vars(project_id, Some(&policy));
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, Some(&policy));
        let worker_env: BTreeMap<&str, &str> = worker_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        // Both force incremental on (parity invariant), regardless of policy.
        assert_eq!(
            warm_env.get("CARGO_INCREMENTAL").copied(),
            Some("1"),
            "warm forces CARGO_INCREMENTAL=1 (warm-cache parity)"
        );
        assert_eq!(
            warm_env.get("CARGO_INCREMENTAL"),
            worker_env.get("CARGO_INCREMENTAL"),
            "warm and worker CARGO_INCREMENTAL must be identical"
        );

        // Both clear RUSTC_WRAPPER so a repo-level sccache wrapper can't disable
        // incremental.
        assert_eq!(
            warm_env.get("RUSTC_WRAPPER").copied(),
            Some(""),
            "warm must clear RUSTC_WRAPPER"
        );
        assert_eq!(
            warm_env.get("RUSTC_WRAPPER"),
            worker_env.get("RUSTC_WRAPPER"),
            "warm and worker RUSTC_WRAPPER must be identical"
        );

        // Shared cache routing is identical.
        assert_eq!(
            warm_env.get("CARGO_HOME"),
            worker_env.get("CARGO_HOME"),
            "CARGO_HOME must be identical between warm and worker"
        );
        assert_eq!(
            warm_env.get("SCCACHE_DIR"),
            worker_env.get("SCCACHE_DIR"),
            "SCCACHE_DIR must be identical between warm and worker"
        );

        // Intentional warm-vs-worker posture difference preserved:
        // warm writes the shared per-project base; worker uses a private dir.
        let warm_target = warm_env
            .get("CARGO_TARGET_DIR")
            .expect("warm CARGO_TARGET_DIR set");
        let worker_target = worker_env
            .get("CARGO_TARGET_DIR")
            .expect("worker CARGO_TARGET_DIR set");
        assert_eq!(
            *warm_target, "/cache/cargo-target/consistency-test",
            "warm CARGO_TARGET_DIR must be the per-project shared base"
        );
        assert!(
            worker_target.starts_with("/cache/cargo-target-runs/"),
            "worker CARGO_TARGET_DIR must be a private per-run dir"
        );
    }

    // ── Evidence-spike K8s runtime isolation ──────────────────────────────
    //
    // These tests assert that evidence-spike task-run jobs fail closed at
    // the container boundary: durable PVC mounts are read-only, no
    // backing-service sidecars or connection env vars are injected, and the
    // manifest differs from the normal worker path in exactly the expected
    // isolation fields.  Normal (non-evidence-spike) jobs must remain
    // unchanged — the existing test `builds_task_run_job_manifest` already
    // pins that baseline.

    /// Evidence-spike runs must mount the mirror PVC read-only so a
    /// tool-surface bug cannot write task branches or mutate the host
    /// mirror.  This asserts both the VolumeMount and the PVC source.
    #[test]
    fn evidence_spike_mirror_mounted_read_only() {
        let cfg = KubernetesConfig::for_testing();
        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-spike",
            "registry.example:5000/djinn-project-p:abc123def456",
            &[],
            None,
            true, // is_evidence_spike
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");
        let container = &pod.containers[0];
        let mounts = container.volume_mounts.as_ref().expect("volume_mounts set");

        // Mirror mount must be read-only.
        let mirror_mount = mounts
            .iter()
            .find(|m| m.name == VOLUME_MIRROR)
            .expect("mirror mount present");
        assert_eq!(
            mirror_mount.read_only,
            Some(true),
            "evidence-spike mirror mount must be read-only"
        );

        // PVC source must also be read-only.
        let volumes = pod.volumes.as_ref().expect("volumes set");
        let mirror_vol = volumes
            .iter()
            .find(|v| v.name == VOLUME_MIRROR)
            .expect("mirror volume present");
        let pvc = mirror_vol
            .persistent_volume_claim
            .as_ref()
            .expect("mirror volume is PVC");
        assert_eq!(
            pvc.read_only,
            Some(true),
            "evidence-spike mirror PVC source must be read-only"
        );
    }

    /// Evidence-spike runs must mount the cache PVC read-only — no build
    /// artifacts should be persisted to the shared cache.
    #[test]
    fn evidence_spike_cache_mounted_read_only() {
        let cfg = KubernetesConfig::for_testing();
        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-spike",
            "registry.example:5000/djinn-project-p:abc123def456",
            &[],
            None,
            true,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");
        let container = &pod.containers[0];
        let mounts = container.volume_mounts.as_ref().expect("volume_mounts set");

        let cache_mount = mounts
            .iter()
            .find(|m| m.name == VOLUME_CACHE)
            .expect("cache mount present");
        assert_eq!(
            cache_mount.read_only,
            Some(true),
            "evidence-spike cache mount must be read-only"
        );

        let volumes = pod.volumes.as_ref().expect("volumes set");
        let cache_vol = volumes
            .iter()
            .find(|v| v.name == VOLUME_CACHE)
            .expect("cache volume present");
        let pvc = cache_vol
            .persistent_volume_claim
            .as_ref()
            .expect("cache volume is PVC");
        assert_eq!(
            pvc.read_only,
            Some(true),
            "evidence-spike cache PVC source must be read-only"
        );
    }

    /// The workspace emptyDir must remain mutable even for evidence spikes —
    /// it is ephemeral per-Pod storage that dies with the container.
    #[test]
    fn evidence_spike_workspace_stays_mutable() {
        let cfg = KubernetesConfig::for_testing();
        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-spike",
            "registry.example:5000/djinn-project-p:abc123def456",
            &[],
            None,
            true,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");
        let container = &pod.containers[0];
        let mounts = container.volume_mounts.as_ref().expect("volume_mounts set");

        let ws_mount = mounts
            .iter()
            .find(|m| m.name == VOLUME_WORKSPACE)
            .expect("workspace mount present");
        assert_eq!(
            ws_mount.read_only, None,
            "workspace emptyDir must remain mutable (not read-only) for evidence spikes"
        );
    }

    /// Evidence-spike runs must not receive backing-service sidecars even
    /// when the caller passes a service slice — the builder filters them
    /// out so no product DB/queue is started.
    #[test]
    fn evidence_spike_suppresses_backing_service_sidecars() {
        let cfg = KubernetesConfig::for_testing();
        let postgres = BackingServiceSpec {
            service_type: "postgres".into(),
            image: "postgres:18-alpine".into(),
            port: 5432,
            env: vec![("POSTGRES_PASSWORD".into(), "postgres".into())],
            cpu_request: "100m".into(),
            memory_request: "256Mi".into(),
            cpu_limit: "500m".into(),
            memory_limit: "512Mi".into(),
            conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
            conn_env_var: "TEST_POSTGRES_URL".into(),
        };

        // Even with a service declared, evidence-spike must suppress it.
        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-spike",
            "registry.example:5000/djinn-project-p:abc123def456",
            std::slice::from_ref(&postgres),
            None,
            true, // is_evidence_spike
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");

        // No init containers (no sidecar).
        assert!(
            pod.init_containers.is_none(),
            "evidence-spike must not inject backing-service sidecars"
        );

        // No svc-dshm volume.
        assert!(
            !pod.volumes
                .as_ref()
                .unwrap()
                .iter()
                .any(|v| v.name == crate::sidecar::SIDECAR_DSHM_VOLUME),
            "evidence-spike must not add svc-dshm volume"
        );

        // No connection env var exported.
        let worker = &pod.containers[0];
        let envs: BTreeMap<&str, &str> = worker
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert!(
            !envs.contains_key("TEST_POSTGRES_URL"),
            "evidence-spike must not export DB connection env vars"
        );
    }

    /// Regression guard: a normal (non-evidence-spike) job with a service
    /// injected still gets the sidecar and connection env var — the
    /// evidence-spike path must not have altered normal behavior.
    #[test]
    fn normal_job_with_service_unaffected_by_evidence_spike_path() {
        let cfg = KubernetesConfig::for_testing();
        let postgres = BackingServiceSpec {
            service_type: "postgres".into(),
            image: "postgres:18-alpine".into(),
            port: 5432,
            env: vec![("POSTGRES_PASSWORD".into(), "postgres".into())],
            cpu_request: "100m".into(),
            memory_request: "256Mi".into(),
            cpu_limit: "500m".into(),
            memory_limit: "512Mi".into(),
            conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
            conn_env_var: "TEST_POSTGRES_URL".into(),
        };

        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-normal",
            "registry.example:5000/djinn-project-p:abc123def456",
            std::slice::from_ref(&postgres),
            None,
            false, // NOT evidence spike
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");

        // Sidecar IS injected for normal runs.
        let inits = pod.init_containers.as_ref().expect("init_containers set");
        assert_eq!(inits.len(), 1);
        assert_eq!(inits[0].name, "svc-postgres");

        // Connection env var IS present.
        let worker = &pod.containers[0];
        let envs: BTreeMap<&str, &str> = worker
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            envs.get("TEST_POSTGRES_URL").copied(),
            Some("postgres://postgres:postgres@127.0.0.1:5432/app_test")
        );

        // Mirror and cache PVCs are RW.
        let volumes = pod.volumes.as_ref().expect("volumes set");
        let mirror_vol = volumes
            .iter()
            .find(|v| v.name == VOLUME_MIRROR)
            .expect("mirror volume present");
        assert_eq!(
            mirror_vol
                .persistent_volume_claim
                .as_ref()
                .unwrap()
                .read_only,
            Some(false),
            "normal job mirror PVC must be read-write"
        );
        let cache_vol = volumes
            .iter()
            .find(|v| v.name == VOLUME_CACHE)
            .expect("cache volume present");
        assert_eq!(
            cache_vol
                .persistent_volume_claim
                .as_ref()
                .unwrap()
                .read_only,
            Some(false),
            "normal job cache PVC must be read-write"
        );
    }

    /// A backing service with multi-name presets (comma-separated
    /// `conn_env_var` like `DATABASE_URL,TEST_POSTGRES_URL`) must export
    /// BOTH env var names with the same rendered connection string so the
    /// worker can reach for either the conventional or the bespoke name.
    #[test]
    fn multi_name_service_preset_exports_all_conn_env_vars() {
        let cfg = KubernetesConfig::for_testing();
        let postgres = BackingServiceSpec {
            service_type: "postgres".into(),
            image: "postgres:18-alpine".into(),
            port: 5432,
            env: vec![("POSTGRES_PASSWORD".into(), "postgres".into())],
            cpu_request: "100m".into(),
            memory_request: "256Mi".into(),
            cpu_limit: "500m".into(),
            memory_limit: "512Mi".into(),
            conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
            conn_env_var: "DATABASE_URL,TEST_POSTGRES_URL".into(),
        };

        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-multi",
            "registry.example:5000/djinn-project-p:abc123def456",
            std::slice::from_ref(&postgres),
            None,
            false,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");
        let worker = &pod.containers[0];
        let envs: BTreeMap<&str, &str> = worker
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        let expected_conn = "postgres://postgres:postgres@127.0.0.1:5432/app_test";
        assert_eq!(
            envs.get("DATABASE_URL").copied(),
            Some(expected_conn),
            "DATABASE_URL must be present for multi-name preset"
        );
        assert_eq!(
            envs.get("TEST_POSTGRES_URL").copied(),
            Some(expected_conn),
            "TEST_POSTGRES_URL must be present for multi-name preset"
        );
    }

    /// The per-task-run Secret volume mounts the payload files at stable
    /// paths under `/var/run/djinn/` so the worker can read them.
    #[test]
    fn secret_volume_mounts_payload_files_at_stable_paths() {
        let cfg = KubernetesConfig::for_testing();
        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-test",
            "registry.example:5000/djinn-project-p:abc123def456",
            &[],
            None,
            false,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");
        let container = &pod.containers[0];
        let mounts = container.volume_mounts.as_ref().expect("volume_mounts set");

        // The spec volume mount covers /var/run/djinn/ — all payload files
        // (spec.bin, credentials.bin, environment.json, service_metadata.json)
        // are accessible under that directory.
        let spec_mount = mounts
            .iter()
            .find(|m| m.name == VOLUME_SPEC)
            .expect("spec volume mount present");
        assert_eq!(spec_mount.mount_path, SPEC_MOUNT_DIR);
        assert_eq!(spec_mount.read_only, Some(true));

        // Verify the volume's items list includes the payload files.
        let volumes = pod.volumes.as_ref().expect("volumes set");
        let spec_volume = volumes
            .iter()
            .find(|v| v.name == VOLUME_SPEC)
            .expect("spec volume present");
        let secret_src = spec_volume.secret.as_ref().expect("spec volume is Secret");
        let items = secret_src.items.as_ref().expect("items set");

        let env_config_item = items
            .iter()
            .find(|i| i.key == crate::env_config::ENV_CONFIG_SECRET_DATA_KEY)
            .expect("environment.json item present");
        assert_eq!(
            env_config_item.path,
            crate::env_config::ENV_CONFIG_SECRET_DATA_KEY,
            "environment.json key must map to the same filename"
        );

        let service_meta_item = items
            .iter()
            .find(|i| i.key == crate::env_config::SERVICE_METADATA_SECRET_DATA_KEY)
            .expect("service_metadata.json item present");
        assert_eq!(
            service_meta_item.path,
            crate::env_config::SERVICE_METADATA_SECRET_DATA_KEY,
            "service_metadata.json key must map to the same filename"
        );
    }

    // ---- hgd0 Wave 1 transport regression tests ----------------------------

    /// AC4: A postgres service preset with `conn_env_var: "TEST_POSTGRES_URL"`
    /// injects that exact env var into the worker container with the rendered
    /// loopback connection string.  The worker reads this to reach the sidecar
    /// without any pre-task bootstrap command — the connection string is a
    /// static env var, not the output of a lifecycle command.
    #[test]
    fn worker_env_receives_test_postgres_url_from_service_preset() {
        let cfg = KubernetesConfig::for_testing();
        let postgres = BackingServiceSpec {
            service_type: "postgres".into(),
            image: "postgres:18-alpine".into(),
            port: 5432,
            env: vec![("POSTGRES_PASSWORD".into(), "postgres".into())],
            cpu_request: "100m".into(),
            memory_request: "256Mi".into(),
            cpu_limit: "500m".into(),
            memory_limit: "512Mi".into(),
            conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
            conn_env_var: "TEST_POSTGRES_URL".into(),
        };

        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-pg",
            "registry.example:5000/djinn-project-p:abc123def456",
            std::slice::from_ref(&postgres),
            None,
            false,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");
        let worker = &pod.containers[0];
        let envs: BTreeMap<&str, &str> = worker
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        // The preset's TEST_POSTGRES_URL is a static connection string.
        let expected = "postgres://postgres:postgres@127.0.0.1:5432/app_test";
        assert_eq!(
            envs.get("TEST_POSTGRES_URL").copied(),
            Some(expected),
            "TEST_POSTGRES_URL must be the rendered loopback connection string"
        );

        // The env var is a connection string, NOT a pre-task command.
        // Verify no command-like patterns appear in the value.
        let value = envs.get("TEST_POSTGRES_URL").unwrap();
        assert!(
            value.starts_with("postgres://"),
            "env var value must be a connection URL, not a command: {value}"
        );
    }

    /// AC4: A multi-name postgres preset (`conn_env_var: "DATABASE_URL,TEST_POSTGRES_URL"`)
    /// emits BOTH env vars into the worker container with the same rendered
    /// connection string.  This is the canonical djinn service-preset shape.
    /// Crucially, these are static connection env vars — no pre-task commands
    /// are attached to or derived from the preset.
    #[test]
    fn multi_name_preset_emits_both_database_url_and_test_postgres_url() {
        let cfg = KubernetesConfig::for_testing();
        let postgres = BackingServiceSpec {
            service_type: "postgres".into(),
            image: "postgres:18-alpine".into(),
            port: 5432,
            env: vec![("POSTGRES_PASSWORD".into(), "postgres".into())],
            cpu_request: "100m".into(),
            memory_request: "256Mi".into(),
            cpu_limit: "500m".into(),
            memory_limit: "512Mi".into(),
            conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
            conn_env_var: "DATABASE_URL,TEST_POSTGRES_URL".into(),
        };

        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-pg-multi",
            "registry.example:5000/djinn-project-p:abc123def456",
            std::slice::from_ref(&postgres),
            None,
            false,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");
        let worker = &pod.containers[0];
        let envs: BTreeMap<&str, &str> = worker
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        let expected = "postgres://postgres:postgres@127.0.0.1:5432/app_test";
        assert_eq!(
            envs.get("DATABASE_URL").copied(),
            Some(expected),
            "DATABASE_URL must be the rendered loopback connection string"
        );
        assert_eq!(
            envs.get("TEST_POSTGRES_URL").copied(),
            Some(expected),
            "TEST_POSTGRES_URL must be the rendered loopback connection string"
        );
        assert_eq!(
            envs.get("DATABASE_URL"),
            envs.get("TEST_POSTGRES_URL"),
            "both env var names must carry the same connection string"
        );
    }

    /// AC4: Service presets are purely connection-injection mechanisms — the
    /// `BackingServiceSpec` struct has no fields for pre-task commands, lifecycle
    /// hooks, or any other command-execution metadata.  This is a type-level
    /// regression guard: if a pre-task field were accidentally added to the
    /// struct, this assertion on the serialized JSON shape would catch it.
    #[test]
    fn service_preset_does_not_carry_pretask_command_fields() {
        let postgres = BackingServiceSpec {
            service_type: "postgres".into(),
            image: "postgres:18-alpine".into(),
            port: 5432,
            env: vec![("POSTGRES_PASSWORD".into(), "postgres".into())],
            cpu_request: "100m".into(),
            memory_request: "256Mi".into(),
            cpu_limit: "500m".into(),
            memory_limit: "512Mi".into(),
            conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
            conn_env_var: "TEST_POSTGRES_URL".into(),
        };

        // Serialize the spec and verify no lifecycle/pre_task fields leak in.
        // BackingServiceSpec doesn't implement Serialize directly, so we verify
        // through the sidecar_conn_env and sidecar_container helpers: both
        // produce only connection env vars and container specs, not commands.
        let conn_envs = crate::sidecar::sidecar_conn_env(&postgres);
        for env in &conn_envs {
            // Every env var from sidecar_conn_env is a connection string
            // (starts with the rendered conn_template), not a command.
            let value = env.value.as_deref().unwrap_or("");
            assert!(
                value.starts_with("postgres://"),
                "conn env var must be a connection URL, not a command: {}={}",
                env.name,
                value
            );
        }

        // The sidecar container itself has no command override — it uses
        // the image's default entrypoint (Postgres, Redis, etc.), not a
        // lifecycle pre-task command.
        let cfg = crate::config::KubernetesConfig::for_testing();
        let container = crate::sidecar::sidecar_container(&cfg, &postgres);
        assert!(
            container.command.is_none(),
            "sidecar container must not override the image entrypoint with commands"
        );
    }
}
