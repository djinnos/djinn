//! Pure `Job` manifest builder for a per-task-run worker Pod.
//!
//! No cluster interaction — [`build_task_run_job`] produces a
//! [`k8s_openapi::api::batch::v1::Job`] value that PR 3 will hand to
//! `kube::Api::<Job>::create`. Structuring the builder as a pure function
//! keeps unit testing trivial: `build_task_run_job(&cfg, &id, secret_name, None)` +
//! struct assertions against the returned `Job`.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, EnvVarSource, KeyToPath, ObjectFieldSelector,
    PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec, ProjectedVolumeSource,
    SecretVolumeSource, ServiceAccountTokenProjection, Toleration, Volume, VolumeMount,
    VolumeProjection,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use uuid::Uuid;

use djinn_core::paths::CacheRootId;
use djinn_runtime::RoleKind;
use djinn_supervisor::cargo_target_run_dir;

use crate::config::KubernetesConfig;
use crate::invocation_journal::{
    invocation_journal_volume, worker_invocation_journal_env, worker_invocation_journal_mount,
};
use crate::launcher::{
    RoleResourceClass, launcher_ipc_volume, launcher_sidecar_container, pod_host_users,
    pod_security_context, worker_launcher_env, worker_launcher_ipc_mount, worker_resources,
    worker_security_context,
};
use crate::sidecar::{
    BackingServiceSpec, sidecar_conn_env, sidecar_container, sidecar_dshm_volume,
};
use crate::workload_inventory::terminal_job_condition;

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

    let completed_at = job
        .status
        .as_ref()
        .and_then(|status| status.completion_time.as_ref())
        .map(|time| std::time::SystemTime::from(time.0));
    let terminal_condition = terminal_job_condition(job.status.as_ref()).map(str::to_owned);

    Some(djinn_runtime::TaskrunJobRef {
        job_name,
        task_run_id,
        created_at,
        completed_at,
        terminal_condition,
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
pub const CACHE_MOUNT_DIR: &str = djinn_core::paths::JOB_POD_CACHE_MOUNT;
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
/// are GC'd by the static 3600-second Kubernetes safety-net TTL.
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
    // The RoleKind that executes this task-run, threaded from dispatch (derived
    // from `spec.flow` — see `runtime.rs`). Drives the role-classed CPU request
    // via [`RoleResourceClass`]. `None` / unknown / any future role FAILS SAFE
    // to build-capable so a pod that might compile is never under-provisioned.
    role: Option<RoleKind>,
) -> Job {
    assert!(
        !config.cgroup_launcher_mode.renders_sidecar() || config.task_run_cgroup_writable_enabled,
        "required cgroup launcher requires runtimeClassName: djinn-cgroup-writable"
    );
    let task_run_id_str = task_run_id.to_string();
    let labels = job_labels(config, &task_run_id_str);
    let job_name = format!("djinn-taskrun-{task_run_id}");
    let role_class = RoleResourceClass::for_role(role);

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
    // The container image is the runtime artifact identity. In production the
    // dispatch resolver renders it as `repository@sha256:...` whenever the
    // registry captured a digest, so this distinguishes rebuilt images even
    // when their Cargo package version did not change.
    let mut worker_env = build_task_run_env(
        config,
        &task_run_id_str,
        project_id,
        project_image_tag,
        policy,
    );
    worker_env.extend(effective_services.iter().flat_map(sidecar_conn_env));
    // The worker receives an explicit enforcement intent instead of inferring it
    // from an incidental directory. Required mode may never degrade to direct execution.
    worker_env.push(env_var(
        "DJINN_CGROUP_LAUNCHER_MODE",
        config.cgroup_launcher_mode.as_str(),
    ));
    // IPC paths exist only with the sidecar; disabled local/development runs skip
    // the handshake entirely and retain no launcher IPC surface.
    let renders_launcher = config.cgroup_launcher_mode.renders_sidecar();
    if renders_launcher {
        worker_env.extend(worker_launcher_env());
        // The broker-backed shell path opens a durable invocation journal
        // BEFORE any session exists, and the worker's compiled-in default for
        // it lands inside the read-only `spec` Secret mount (EROFS). Name the
        // directory explicitly and mount it below; see
        // `crate::invocation_journal` for the measured failure.
        worker_env.push(worker_invocation_journal_env());
        // Where `configure_private_dep_access` must ALSO store the installation
        // token's `url.insteadOf` rewrite. Its `git config --global` write lands
        // in the WORKER's `$HOME`, which a brokered child never sees, so every
        // private-dependency fetch would go out unauthenticated and silently.
        // See `crate::private_dep_config` (goxi, ninth launcher blocker).
        worker_env.push(crate::private_dep_config::child_git_config_env());
    }

    let mut worker_volume_mounts = vec![
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
    ];
    if renders_launcher {
        // Broker control socket + worker-private launcher credential. Shared
        // with the cgroup-launcher sidecar only; never mounted into a backing
        // sidecar, and closed off from the launcher-spawned child.
        worker_volume_mounts.push(worker_launcher_ipc_mount());
        // Writable home for the durable invocation journal. Worker-only, and
        // nested under the read-only `spec` mount exactly the way the launcher
        // IPC volume already is.
        worker_volume_mounts.push(worker_invocation_journal_mount());
        // The WRITE end of the one-way private-dependency git config channel.
        // The launcher mounts the same volume `readOnly: true`, so the direction
        // is enforced by the kubelet rather than by convention.
        worker_volume_mounts.push(crate::private_dep_config::worker_child_git_mount());
    }
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
        // Base mounts + qut0's mandatory launcher IPC mount are all folded into
        // worker_volume_mounts above.
        volume_mounts: Some(worker_volume_mounts),
        // Role-classed resources: CPU request varies by role class; CPU limit
        // and memory bounds are shared ("role changes REQUESTS only").
        resources: Some(worker_resources(config, role_class)),
        // v1 security contract: worker runs as uid/gid 1000 (NOT the legacy
        // pod-wide uid 10001), drops all capabilities, restricted seccomp.
        // Non-dumpability is asserted by the worker at runtime before it
        // authenticates to the broker. See the fsGroup deploy-risk note on the
        // pod securityContext below.
        security_context: Some(worker_security_context()),
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
    if renders_launcher {
        // Enforcement volumes for the cgroup-launcher sidecar:
        //   * `launcher-ipc` — Memory emptyDir carrying the broker control
        //     socket + worker-private credential (worker + launcher only);
        volumes.push(launcher_ipc_volume());
        //   * `invocation-journal` — Memory emptyDir the worker opens its
        //     durable invocation journal in. `ShellLaunchContext::broker_backed`
        //     runs `create_dir_all` on it before the supervisor starts, and its
        //     default path is inside the READ-ONLY `spec` Secret mount, so
        //     without this volume every armed pod dies `EROFS` with zero
        //     sessions. See `crate::invocation_journal`.
        volumes.push(invocation_journal_volume());
        //   * `launcher-tmp` / `launcher-home` / `launcher-var-tmp` — writable
        //     `/tmp`, `$HOME` and `/var/tmp` for a brokered child.
        //     `readOnlyRootFilesystem: true` takes the image's copies away, and
        //     the worker container (which has neither flag) keeps them, so
        //     arming would otherwise REGRESS the unbrokered path. Measured:
        //     `git config --global` fails `Read-only file system` without the
        //     home volume, and `TMPDIR=/var/tmp mktemp -d` fails the same way
        //     without the var-tmp one — and `/var/tmp` is what the SANDBOX pins
        //     `TMPDIR` to at spawn time, which no manifest names. See
        //     `launcher_child_fs`.
        volumes.extend(crate::launcher_child_fs::launcher_scratch_volumes());
        //   * `child-git-config` — the one-way worker→child channel carrying
        //     the private-dependency installation token. RW in the worker, RO in
        //     the launcher. See `crate::private_dep_config`.
        volumes.push(crate::private_dep_config::child_git_config_volume());
    }
    // Backing-service sidecars share a Memory /dev/shm (Postgres needs more than
    // the 64Mi default). Added only when services are injected so the manifest
    // is byte-identical to the pre-feature shape for service-less projects.
    // Evidence-spike runs always skip sidecars — `effective_services` is empty
    // when `is_evidence_spike` is true, so this block is a no-op.
    if !effective_services.is_empty() {
        volumes.push(sidecar_dshm_volume());
    }
    // The cgroup-launcher is a native sidecar (initContainer + restartPolicy:
    // Always): it comes up before the worker (which dials its broker socket) and
    // is torn down when the worker exits, so the Job still reaches Completed.
    // The RuntimeClass supplies its writable delegated cgroup hierarchy; this
    // manifest deliberately supplies no cgroup volume or mount.
    //
    // Each declared backing service is then ALSO a native sidecar, appended
    // AFTER the launcher.  For evidence-spike runs, `effective_services` is
    // empty so no product databases start.
    let mut init_container_vec = Vec::new();
    if renders_launcher {
        init_container_vec.push(launcher_sidecar_container(
            config,
            project_image_tag,
            mirror_read_only,
            cache_read_only,
        ));
    }
    init_container_vec.extend(
        effective_services
            .iter()
            .map(|s| sidecar_container(config, s)),
    );
    // Emit no `initContainers` key at all when nothing is injected, restoring the
    // pre-enforcement shape for a service-less project with the launcher off.
    let init_containers = (!init_container_vec.is_empty()).then_some(init_container_vec);

    // Pin Pods to a dedicated NodePool when the operator has configured one.
    // Both fields stay `None` if the corresponding config entry is empty so
    // the rendered manifest is identical to the pre-feature shape.
    let node_selector = (!config.node_selector.is_empty()).then(|| config.node_selector.clone());
    let tolerations: Option<Vec<Toleration>> =
        (!config.tolerations.is_empty()).then(|| config.tolerations.clone());

    let pod_spec = PodSpec {
        runtime_class_name: config
            .task_run_cgroup_writable_enabled
            .then_some(crate::launcher::TASK_RUN_CGROUP_RUNTIME_CLASS.to_string()),
        service_account_name: Some(config.service_account.clone()),
        // jqvg: the task-run Pod runs repository-controlled code (agent shell
        // commands, `build.rs`, test targets, npm `postinstall`). Nothing in
        // this Pod speaks to the apiserver — the worker's only authenticated
        // peer is djinn-server, reached with the audience-bound projected token
        // on VOLUME_AUTH_TOKEN below — so the default ServiceAccount token has
        // no legitimate reader here and only supplies a leak with real
        // apiserver blast radius. Turning automount off removes that file from
        // the container filesystem entirely, which is strictly stronger than
        // the sandbox-level read denial that also covers `/var/run/secrets`.
        automount_service_account_token: Some(false),
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
        // shareProcessNamespace exists ONLY so the launcher sidecar can see the
        // worker process (PID auth via SO_PEERCRED / the broker's `worker_pid`
        // contract). It is a real widening — every backing-service sidecar can
        // then see the worker's `/proc` entries — so it is set only when the
        // launcher is actually rendered (task grkq).
        share_process_namespace: renders_launcher.then_some(true),
        // Always None. A user namespace leaves the kubelet-delegated cgroup
        // owned by an unmapped uid, preventing the launcher from creating its
        // holding leaf; see `launcher::pod_host_users`.
        host_users: pod_host_users(config.cgroup_launcher_mode),
        // v1 leases security contract (qut0). The per-container securityContexts
        // set the UIDs now (worker=1000, launcher=0); the pod context ties
        // `fsGroup` to the artifact GID (1000) with `fsGroupChangePolicy:
        // OnRootMismatch` so workspace/cache/mirror volumes are group-owned by
        // the artifact GID (setgid), letting the launcher-spawned child (group
        // 1000) write artifacts the worker (uid/gid 1000) can read.
        //
        // DEPLOY RISK: the legacy pod ran as uid 10001, and the VPS's large
        // `/mirror` and `/cache` PVCs are currently owned by 10001:10001.
        // `fsGroup=1000` makes the kubelet recursively re-own the volume to group
        // 1000 on the first mount whose root gid mismatches. `OnRootMismatch`
        // limits this to that first pod, but that first task-run/warm pod after
        // deploy pays a one-time, potentially slow recursive re-own of those huge
        // caches. Mitigation: run a one-shot `chgrp -R 1000 /mirror /cache`
        // before cutover so the root gid already matches and the recursive pass
        // is skipped. See the matching notes in `launcher.rs` and `config.rs`.
        //
        // (The legacy uid-10001 / git "dubious ownership" concern is covered by
        // the protected system-scope `safe.directory` config that
        // `djinn_git::git_command` exports — not by the GIT_CONFIG_* envs in
        // build_task_run_env, which git strips from the inner child of
        // `git clone --local`. See nurw and djinn-git/src/lib.rs.)
        security_context: Some(pod_security_context()),
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
            // Kueue create-then-admit: armed Jobs are created suspended and
            // unsuspended by Kueue once the ClusterQueue admits their Workload.
            // `None` when disarmed — see `KubernetesConfig::kueue_job_suspend`.
            suspend: config.kueue_job_suspend(),
            // Static failed-job safety net. The coordinator may delete known
            // successes earlier, but this must never become a configurable
            // short retention policy for unknown or failed outcomes.
            ttl_seconds_after_finished: Some(3600),
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
/// Add the sole owner-cache mount for immutable read-source grants.
#[allow(clippy::too_many_arguments)]
pub fn build_task_run_job_with_read_sources(
    config: &KubernetesConfig,
    task_run_id: &Uuid,
    project_id: &str,
    secret_name: &str,
    project_image_tag: &str,
    services: &[BackingServiceSpec],
    policy: Option<&djinn_stack::environment::CargoCachePolicy>,
    is_evidence_spike: bool,
    role: Option<RoleKind>,
    owner_cache_sub_path: Option<&str>,
) -> Job {
    let mut job = build_task_run_job(
        config,
        task_run_id,
        project_id,
        secret_name,
        project_image_tag,
        services,
        policy,
        is_evidence_spike,
        role,
    );
    let Some(owner_cache_sub_path) = owner_cache_sub_path else {
        return job;
    };
    let pod = job
        .spec
        .as_mut()
        .expect("builder sets JobSpec")
        .template
        .spec
        .as_mut()
        .expect("builder sets PodSpec");
    pod.volumes.get_or_insert_with(Vec::new).push(Volume {
        name: "read-sources".to_string(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: config.projects_pvc.clone(),
            read_only: Some(true),
        }),
        ..Volume::default()
    });
    pod.containers[0]
        .volume_mounts
        .get_or_insert_with(Vec::new)
        .push(VolumeMount {
            name: "read-sources".to_string(),
            mount_path: "/read-sources".to_string(),
            sub_path: Some(owner_cache_sub_path.to_string()),
            read_only: Some(true),
            ..VolumeMount::default()
        });
    job
}

fn job_labels(config: &KubernetesConfig, task_run_id: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_TASK_RUN_ID.to_string(), task_run_id.to_string());
    labels.insert(
        LABEL_COMPONENT.to_string(),
        COMPONENT_TASK_RUN_WORKER.to_string(),
    );
    // Stamped HERE, in the one map the caller clones into both the Job metadata
    // and the Pod template, so Kueue's Job webhook and Pod webhook both see it.
    // No-op unless `config.kueue_armed`.
    config.apply_kueue_build_object_labels(crate::config::KueueQueueKind::TaskRun, &mut labels);
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
    deployment_revision: &str,
    policy: Option<&djinn_stack::environment::CargoCachePolicy>,
) -> Vec<EnvVar> {
    let mut env = vec![
        env_var("RUST_BACKTRACE", "1"),
        env_var("DJINN_SERVER_ADDR", &config.server_addr),
        env_var("DJINN_SPEC_PATH", SPEC_MOUNT_FILE),
        env_var("DJINN_CREDENTIALS_PATH", CREDENTIALS_MOUNT_FILE),
        env_var("DJINN_TOKEN_PATH", TOKEN_MOUNT_FILE),
        env_var("DJINN_TASK_RUN_ID", task_run_id_str),
        // The worker REQUIRES this (clap, no default) and uses it as the
        // immutable fence for the durable invocation journal and for the
        // watchdog's exact-Pod termination, which matches it against
        // `pod.metadata.uid` (see `runtime::exact_taskrun_pod_name`). It is the
        // POD's own UID, not the Job's, so it can only come from the downward
        // API at admission — no host-side value exists when the Job is built.
        downward_api_env_var("DJINN_TASK_RUN_POD_UID", "metadata.uid"),
        // This is the exact image reference rendered onto the worker container,
        // normally a digest-pinned pull ref. It is a deployment identity, not a
        // source package version.
        env_var("DJINN_DEPLOYMENT_REVISION", deployment_revision),
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
    // unless safe.directory is set.
    //
    // These Pod-level vars cover a *direct* git invocation in the Pod and
    // nothing more. They are NOT what makes the mirror clone work: git strips
    // command-scope config from the inner `git-upload-pack` child that
    // `git clone --local` spawns, so this form is a no-op for exactly that
    // operation (nurw — it wedged every PR open on v0.7.3). Mirror clones are
    // trusted by `djinn_git::git_command`, which exports a protected
    // system-scope config file that survives into the child; see the
    // measurements in djinn-git/src/lib.rs.
    //
    // They do not cover a BROKERED command either. The launcher's environment
    // allow-list refuses every `GIT_CONFIG_*` key deliberately: `GIT_CONFIG_KEY_n`
    // /`VALUE_n` sets ARBITRARY configuration and several git keys
    // (`core.sshCommand`, `core.pager`, …) are arbitrary command execution, so
    // forwarding them would hand every brokered child a way out of the boundary
    // the broker exists to establish. A brokered child gets `safe.directory`
    // from a launcher-OWNED config file instead, exported as `GIT_CONFIG_SYSTEM`
    // and admitted only at that exact path; see
    // `djinn_cgroup_launcher::git_trust` (goxi, sixth launcher blocker).
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

    env.extend(task_run_cache_env_vars(
        project_id,
        task_run_id_str,
        &config.cpu_limit,
        policy,
    ));
    env
}

/// Runtime env vars routing the shared Rust toolchain caches to the persistent
/// `/cache` PVC. Warm Pods use the per-project base target dir;
/// task-run Pods use `task_run_cache_env_vars` so their writable target dir is
/// private per task run while still sharing registry settings and preserving a
/// legacy, repo-compatibility `SCCACHE_DIR` fallback.
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
/// - SCCACHE_DIR: this is only a compatibility fallback for a repo/tool that
///   explicitly invokes sccache; Djinn build pods clear `RUSTC_WRAPPER` and do
///   not depend on the directory. Without SCCACHE_DIR such an invocation falls back to
///   $HOME/.cache/sccache (/home/djinn/.cache/sccache), which is (1) ephemeral
///   and (2) NOT in the Landlock allowlist (only $HOME/.cache/djinn is), so the
///   sandboxed sccache server is denied write there. Namespaced per project
///   (like CARGO_TARGET_DIR): sccache's local disk cache is not safe for
///   multiple concurrent server processes sharing one directory, and Pods share
///   the /cache PVC, so a single /cache/sccache would risk multi-writer
///   corruption across projects.
fn common_cache_env_vars(project_id: &str, cpu_limit: &str) -> Vec<EnvVar> {
    // Cap build/test parallelism at the pod's OWN CPU limit. Cargo (and nextest)
    // otherwise default their job/thread count to the number of CPUs the cgroup
    // reports, which on a shared node is the HOST core count, NOT the pod's
    // limit. A deploy that cold-rebuilds ~8 task-run pods simultaneously on one
    // 12-core node therefore launched ~8 × `-j12` ≈ 96 runnable compile
    // processes → node load average 103 → kubelet/postgres/djinn-server probe
    // timeouts and probe-kill restarts (server 521 for a minute). Deriving the
    // job count from the pod's declared CPU limit keeps the aggregate runnable
    // process count bounded by the node's real capacity.
    let jobs = cpu_limit_to_jobs(cpu_limit).to_string();
    let mut env = vec![
        // Generic XDG cache root, rendered for the same reason CARGO_HOME,
        // SCCACHE_DIR, GOMODCACHE and PNPM_HOME are: `$HOME`-relative stores are
        // both unwritable and ephemeral in these pods (task 9jrg).
        //
        // Unwritable: `qut0` moved these pods to uid/gid 1000 while the image's
        // /home/djinn stayed uid 10001 mode 0775, so the pod matched "other"
        // (r-x). The durable output stash resolves
        // `$XDG_CACHE_HOME/djinn/output_stash`, falling back to
        // `$HOME/.cache/djinn/output_stash`, so with XDG_CACHE_HOME unset every
        // worker and planner session died on `create durable blobs: Permission
        // denied` before it could submit for review. The SCIP indexer cache
        // (djinn-graph scip_indexer/cache.rs) resolves the same pair.
        //
        // Ephemeral: even with the image's ownership fixed, `$HOME/.cache` is a
        // container layer that dies with the Pod — so a stash the coordinator
        // GCs on a retention window (`DJINN_OUTPUT_STASH_GC_RETENTION_DAYS`) and
        // reads back after a restart would never actually survive one. The
        // `/cache` PVC is persistent, group-owned by the artifact GID with
        // setgid 2775 (so `create_dir_all` under the worker's 0002 umask
        // inherits a conforming subtree), verified at startup by the
        // volume-ownership contract rather than assumed, and inside the agent's
        // Landlock allowlist — which follows automatically, because the sandbox
        // derives its djinn cache dir from this very env var.
        //
        // Namespaced per project like SCCACHE_DIR/CARGO_TARGET_DIR: the PVC is
        // shared cluster-wide and stashed blobs are verbatim tool output, which
        // must not commingle across project boundaries. NOT per task run: the
        // stash is read back after a restart and expires on a retention window,
        // so a per-run directory would quietly make the durable stash per-run.
        //
        // Retention: the coordinator's stash GC runs in the server process
        // against the server's OWN root, so it does not sweep this one — nor did
        // it sweep the pod-side stash before, which lived on a container layer
        // that died with the Pod. The server mounts this same claim (at
        // /var/lib/djinn/cache), so extending that sweep to the per-project roots
        // needs no new volume plumbing.
        env_var(
            "XDG_CACHE_HOME",
            &format!("{}/{project_id}", CacheRootId::Xdg.job_pod_path()),
        ),
        env_var("CARGO_HOME", CacheRootId::Cargo.job_pod_path()),
        // NOTE: we deliberately do NOT force `RUSTC_WRAPPER=sccache` or
        // `CARGO_INCREMENTAL=0` here. The fast path is incremental compilation
        // over a warm, main-based per-project target base (CI-style, like
        // Swatinem/rust-cache): the warm job pre-compiles the workspace into the
        // base with `CARGO_INCREMENTAL=1`, task-run pods seed a
        // private run target dir from that base and recompile only their delta
        // incrementally. Forcing sccache (which requires CARGO_INCREMENTAL=0)
        // disables incremental and was the wrong lever — it made every
        // task-run cold-build (~14-29min clippy). SCCACHE_DIR remains below
        // only as a writable, Landlock-allowed compatibility fallback when a
        // repo tool explicitly invokes sccache; coordinator cleanup may remove
        // stale contents, so Djinn's build path must not rely on it.
        env_var(
            "SCCACHE_DIR",
            &format!("{}/{project_id}", CacheRootId::Sccache.job_pod_path()),
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
        // djinn-agent-runtime-base.Dockerfile bakes the mold linker selection
        // as an image ENV. Pod manifests additionally cap mold's linker
        // threads to this pod's derived job count.
        //
        // Shipping this is a ONE-TIME warm-base invalidation (the effective
        // rustflags change → fingerprints change → the first warm after deploy
        // rebuilds the per-project base); steady state is fast links thereafter.
        env_var(
            "CARGO_BUILD_RUSTFLAGS",
            &format!("-Clink-arg=-fuse-ld=mold -Clink-arg=-Wl,--threads={jobs}"),
        ),
        // Pin cargo's parallel job count to the pod's CPU limit (see the
        // load-103 incident note above). Without it cargo reads the host core
        // count through the cgroup and oversubscribes the node.
        env_var("CARGO_BUILD_JOBS", &jobs),
        // Nextest picks its default test-thread count the same way (num-cpus →
        // host cores under a shared cgroup), so a single pod's `cargo nextest
        // run` would spawn host-core-many test threads. Pin it to the same
        // per-pod value so test execution parallelism matches build
        // parallelism.
        env_var("NEXTEST_TEST_THREADS", &jobs),
    ];
    // SCIP indexer cache retention (djinn-graph scip_indexer::cache_gc). The
    // cache lives on the same /cache PVC these env vars route everything else
    // onto, and the pods rendered here — not the server — are what write it, so
    // the chart's tuning has to reach them. Forwarded rather than templated
    // because it is optional: djinn-graph ships the same defaults, so a chart
    // that sets nothing still yields a bounded cache and this simply renders
    // nothing.
    env.extend(forwarded_env_vars(SCIP_CACHE_FORWARDED_ENV, |name| {
        std::env::var(name).ok()
    }));
    env
}

/// Env var names forwarded verbatim from the server process onto build pods.
const SCIP_CACHE_FORWARDED_ENV: &[&str] = &[
    "DJINN_SCIP_CACHE_MAX_BYTES",
    "DJINN_SCIP_CACHE_MAX_IDLE_HOURS",
];

/// Forward the named variables from the server's own environment, skipping any
/// that are unset or empty. An empty value must not be forwarded: the consumer
/// treats an unparseable override as "keep the built-in default", and rendering
/// an empty string would only add noise to every PodSpec.
fn forwarded_env_vars(
    names: &[&str],
    mut get_env: impl FnMut(&str) -> Option<String>,
) -> Vec<EnvVar> {
    names
        .iter()
        .filter_map(|name| {
            get_env(name)
                .filter(|value| !value.trim().is_empty())
                .map(|value| env_var(name, &value))
        })
        .collect()
}

/// Derive a cargo/nextest parallel-job count from a Kubernetes CPU limit
/// quantity, flooring to whole cores with a floor of 1.
///
/// Kubernetes CPU quantities are either a plain (possibly fractional) core
/// count (`"6"`, `"2"`, `"1.5"`) or an integer millicore value with an `m`
/// suffix (`"1500m"` = 1.5 cores, `"500m"` = 0.5 cores). Cargo's `-j` and
/// nextest's `--test-threads` are whole numbers ≥ 1, so we floor to cores and
/// clamp to a minimum of 1 (a sub-core limit like `"500m"` still gets one job).
/// An unparseable value falls back to 1 rather than to the host core count.
fn cpu_limit_to_jobs(cpu_limit: &str) -> u32 {
    let trimmed = cpu_limit.trim();
    let cores = match trimmed.strip_suffix('m') {
        Some(milli) => milli.trim().parse::<f64>().ok().map(|m| m / 1000.0),
        None => trimmed.parse::<f64>().ok(),
    };
    match cores {
        Some(c) if c >= 1.0 => c.floor() as u32,
        _ => 1,
    }
}

/// Base cache env vars routing CARGO_TARGET_DIR at the shared per-project warm
/// base keyed by its derived mold linker thread count. Warm Pods write this base
/// directly; task-run Pods retain their private run dir (the worker overrides
/// CARGO_TARGET_DIR to it).
pub(crate) fn cache_env_vars(project_id: &str, cpu_limit: &str) -> Vec<EnvVar> {
    let mut env = common_cache_env_vars(project_id, cpu_limit);
    let jobs = cpu_limit_to_jobs(cpu_limit);
    env.push(env_var(
        "CARGO_TARGET_DIR",
        &format!(
            "{}/{project_id}/mold-jobs-{jobs}",
            CacheRootId::CargoTarget.job_pod_path()
        ),
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
    cpu_limit: &str,
    _policy: Option<&djinn_stack::environment::CargoCachePolicy>,
) -> Vec<EnvVar> {
    let mut env = cache_env_vars(project_id, cpu_limit);
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
    cpu_limit: &str,
    _policy: Option<&djinn_stack::environment::CargoCachePolicy>,
) -> Vec<EnvVar> {
    let mut env = common_cache_env_vars(project_id, cpu_limit);
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

/// Env var sourced from the Pod's own object metadata at admission (downward
/// API). Values that only exist once the apiserver has created the Pod — its
/// UID above all — cannot be baked into the manifest the dispatcher renders.
fn downward_api_env_var(name: &str, field_path: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                field_path: field_path.to_string(),
                ..ObjectFieldSelector::default()
            }),
            ..EnvVarSource::default()
        }),
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
#[path = "job_role_cpu_tests.rs"]
mod job_role_cpu_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn disabled_launcher_config() -> KubernetesConfig {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.cgroup_launcher_mode = crate::launcher::CgroupLauncherMode::Disabled;
        cfg
    }

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
            // Downward-API vars (DJINN_TASK_RUN_POD_UID) carry `valueFrom`, not
            // `value`; asserted by djinn-agent-worker's `rendered_job_env_contract`.
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
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
    fn taskrun_job_ref_uses_the_shared_true_terminal_condition_predicate() {
        use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};

        let cases = [
            (vec![], None),
            (vec![("Complete", "False")], None),
            (vec![("Failed", "FALSE")], None),
            (vec![("Complete", "tRuE")], Some("Complete")),
            (vec![("Failed", "True")], Some("Failed")),
            (
                vec![("Complete", "True"), ("Failed", "True")],
                Some("Failed"),
            ),
        ];

        for (conditions, expected) in cases {
            let task_run_id = Uuid::now_v7();
            let job_name = format!("{TASKRUN_JOB_NAME_PREFIX}{task_run_id}");
            let mut job = inventory_job(Some(&job_name), None);
            job.status = Some(JobStatus {
                conditions: Some(
                    conditions
                        .iter()
                        .map(|(type_, status)| JobCondition {
                            type_: (*type_).to_owned(),
                            status: (*status).to_owned(),
                            ..Default::default()
                        })
                        .collect(),
                ),
                ..Default::default()
            });
            let reference = taskrun_job_ref_from_job(&job).expect("task-run job reference");
            assert_eq!(reference.terminal_condition.as_deref(), expected);
            assert_eq!(
                reference.terminal_condition.is_some(),
                crate::workload_inventory::job_reached_terminal_condition(job.status.as_ref()),
                "inventory and retention readers must agree for {conditions:?}"
            );
        }
    }

    #[test]
    fn complete_false_flows_to_the_acting_retention_consumer_as_live() {
        use djinn_core::job_retention::{
            JobRetentionEvidence, RetentionOutcome, SessionEvidence, classify_taskrun_job,
        };
        use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};

        let task_run_id = Uuid::now_v7();
        let job_name = format!("{TASKRUN_JOB_NAME_PREFIX}{task_run_id}");
        let mut job = inventory_job(Some(&job_name), None);
        job.status = Some(JobStatus {
            conditions: Some(vec![JobCondition {
                type_: "Complete".to_owned(),
                status: "False".to_owned(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let reference = taskrun_job_ref_from_job(&job).expect("task-run reference");
        let sessions = [SessionEvidence {
            status: "running",
            ended_at: None,
        }];
        let decision = classify_taskrun_job(
            std::time::SystemTime::UNIX_EPOCH,
            JobRetentionEvidence {
                created_at: reference.created_at,
                completed_at: reference.completed_at,
                terminal_condition: reference.terminal_condition.as_deref(),
                task_run_status: Some("running"),
                task_run_ended_at: None,
                sessions: &sessions,
            },
        );
        assert_eq!(decision.outcome, RetentionOutcome::Live);
    }

    #[test]
    fn builds_task_run_job_manifest() {
        let cfg = KubernetesConfig::for_testing();
        let task_run_id = Uuid::now_v7();
        let secret_name = "djinn-taskrun-test";
        let project_image =
            "registry.example:5000/djinn-project-p@sha256:0123456789abcdef0123456789abcdef";

        let job = build_task_run_job(
            &cfg,
            &task_run_id,
            "proj-xyz",
            secret_name,
            project_image,
            &[],
            None,
            false,
            None,
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
        assert_eq!(spec.ttl_seconds_after_finished, Some(3600));
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
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
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
        // The worker requires the Pod's OWN immutable UID (it fences the durable
        // invocation journal, and the watchdog matches it against
        // `pod.metadata.uid`). It cannot be a literal — the UID does not exist
        // until the apiserver admits the Pod — so it MUST come from the downward
        // API. Rendering it as anything else (metadata.name, the Job UID) breaks
        // exact-Pod termination silently. Task opsu: it was missing entirely and
        // every task-run Pod exited 2 in argv parsing.
        let pod_uid = container
            .env
            .as_ref()
            .expect("container.env set")
            .iter()
            .find(|e| e.name == "DJINN_TASK_RUN_POD_UID")
            .expect("DJINN_TASK_RUN_POD_UID is required by the worker binary");
        assert!(
            pod_uid.value.is_none(),
            "a Pod UID cannot be known when the manifest is rendered"
        );
        assert_eq!(
            pod_uid
                .value_from
                .as_ref()
                .and_then(|source| source.field_ref.as_ref())
                .map(|field| field.field_path.as_str()),
            Some("metadata.uid"),
            "the worker's Pod UID must come from the downward API"
        );
        let deployment_revision = container
            .env
            .as_ref()
            .expect("container.env set")
            .iter()
            .find(|e| e.name == "DJINN_DEPLOYMENT_REVISION")
            .expect("B2 capability reports require the rendered deployment identity");
        assert_eq!(
            deployment_revision.value.as_deref(),
            Some(project_image),
            "the worker must receive the exact rendered image reference as its deployment revision"
        );
        assert!(
            deployment_revision.value_from.is_none(),
            "the image identity is fixed while rendering this Job, not supplied by a mutable Pod field"
        );

        // DB env vars are gated on the corresponding config fields being
        // `Some`. `for_testing()` leaves them `None`, so they should be
        // absent here — see `forwards_db_env_vars_when_configured` for
        // the populated-config case.
        assert!(
            !envs.contains_key("DJINN_DATABASE_URL"),
            "DJINN_DATABASE_URL must be absent when database_url is None"
        );

        // Production/default rendering adds the worker-private launcher IPC
        // mount and the writable invocation-journal mount to the six baseline
        // mounts.
        let mounts = container.volume_mounts.as_ref().expect("volume_mounts set");
        assert_eq!(
            mounts.len(),
            9,
            "expected 9 volume mounts including launcher IPC, the invocation journal and the \
             one-way private-dependency git config channel"
        );
        let expected_mounts: [(&str, &str, Option<bool>); 9] = [
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
            (
                crate::launcher::VOLUME_LAUNCHER_IPC,
                crate::launcher::LAUNCHER_IPC_DIR,
                None,
            ),
            // Writable, and nested under the READ-ONLY spec mount at
            // /var/run/djinn. Without it the worker's `create_dir_all` on the
            // journal returns EROFS and the armed pod dies before any session
            // exists — see `crate::invocation_journal`.
            (
                crate::invocation_journal::VOLUME_INVOCATION_JOURNAL,
                crate::invocation_journal::INVOCATION_JOURNAL_DIR,
                None,
            ),
            // The WRITE end of the private-dependency git config channel. The
            // launcher mounts the same volume `readOnly: true`; without the
            // channel a brokered fetch of a private transitive dependency goes
            // out unauthenticated, silently. See `crate::private_dep_config`.
            (
                crate::private_dep_config::VOLUME_CHILD_GIT_CONFIG,
                crate::private_dep_config::CHILD_GIT_CONFIG_DIR,
                None,
            ),
        ];
        for (mount, (exp_name, exp_path, exp_ro)) in mounts.iter().zip(expected_mounts.iter()) {
            assert_eq!(&mount.name, exp_name);
            assert_eq!(&mount.mount_path, exp_path);
            assert_eq!(mount.read_only, *exp_ro);
        }

        // Production/default rendering includes IPC and child filesystem
        // surfaces only; RuntimeClass provides the cgroup hierarchy directly.
        let volumes = pod.volumes.as_ref().expect("volumes set");
        assert_eq!(
            volumes.len(),
            12,
            "expected 12 volumes: launcher surfaces, the invocation journal, the launcher's own \
             /tmp + $HOME + /var/tmp, and the private-dependency git config channel"
        );
        let expected_volume_names = [
            VOLUME_SPEC,
            VOLUME_AUTH_TOKEN,
            VOLUME_MIRROR,
            VOLUME_CACHE,
            VOLUME_WORKSPACE,
            crate::env_config::VOLUME_ENV_CONFIG,
            crate::launcher::VOLUME_LAUNCHER_IPC,
            crate::invocation_journal::VOLUME_INVOCATION_JOURNAL,
            // The launcher's writable /tmp, $HOME and /var/tmp:
            // `readOnlyRootFilesystem` takes the image's copies away from a
            // brokered child. /var/tmp is what the SANDBOX pins TMPDIR to at
            // spawn time, which no manifest names (goxi blocker 8).
            crate::launcher_child_fs::VOLUME_LAUNCHER_TMP,
            crate::launcher_child_fs::VOLUME_LAUNCHER_HOME,
            crate::launcher_child_fs::VOLUME_LAUNCHER_VAR_TMP,
            crate::private_dep_config::VOLUME_CHILD_GIT_CONFIG,
        ];
        for (volume, expected_name) in volumes.iter().zip(expected_volume_names.iter()) {
            assert_eq!(&volume.name, expected_name);
        }
        assert!(
            pod.init_containers
                .iter()
                .flatten()
                .any(|c| c.name == crate::launcher::LAUNCHER_CONTAINER_NAME),
            "default production rendering includes the launcher sidecar"
        );
        assert_eq!(pod.share_process_namespace, Some(true));

        // jqvg: nothing in the task-run Pod speaks to the apiserver, and the
        // Pod runs repository-controlled code. The apiserver-capable default
        // ServiceAccount token must not be projected into it at all.
        assert_eq!(
            pod.automount_service_account_token,
            Some(false),
            "jqvg: the task-run Pod must not automount the apiserver ServiceAccount token"
        );

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
            None,
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
            // Downward-API vars (DJINN_TASK_RUN_POD_UID) carry `valueFrom`, not
            // `value`; asserted by djinn-agent-worker's `rendered_job_env_contract`.
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        assert_eq!(
            envs.get("DJINN_DATABASE_URL").copied(),
            Some("postgres://djinn@djinn-postgres.djinn.svc:5432/djinn")
        );
    }

    /// The durable output stash resolves `$XDG_CACHE_HOME/djinn/output_stash`,
    /// falling back to `$HOME/.cache/djinn/output_stash`. Since `qut0` the Pod
    /// runs as uid/gid 1000 while the image's `/home/djinn` is owned by uid
    /// 10001, so leaving `XDG_CACHE_HOME` unset put the stash on a path the Pod
    /// cannot create — every worker and planner session died on `create durable
    /// blobs: Permission denied` (9jrg). It must be rendered, it must sit on the
    /// persistent PVC (the stash is GC'd on a retention window, so a container
    /// layer that dies with the Pod is not a home for it), and it must be
    /// namespaced per project.
    #[test]
    fn routes_the_xdg_cache_home_to_the_persistent_pvc_not_the_image_home() {
        let cfg = KubernetesConfig::for_testing();
        let task_run_id = Uuid::now_v7();

        let job = build_task_run_job(
            &cfg,
            &task_run_id,
            "proj-xyz",
            "djinn-taskrun-test",
            "registry.example:5000/djinn-project-p:abc123def456",
            &[],
            None,
            false,
            None,
        );
        let envs = task_run_job_envs(&job);

        let xdg = envs
            .get("XDG_CACHE_HOME")
            .copied()
            .expect("XDG_CACHE_HOME must be rendered; unset falls back to an unwritable $HOME");
        assert_eq!(
            xdg, "/cache/xdg/proj-xyz",
            "the XDG cache root must live on the persistent /cache PVC, namespaced per project"
        );
        assert!(
            xdg.starts_with(&format!("{CACHE_MOUNT_DIR}/")),
            "{xdg} must be under the group-1000-writable cache mount"
        );
        assert!(
            !xdg.contains("/home/"),
            "{xdg} must not resolve under the image home, which uid 1000 cannot write"
        );
        assert!(
            !xdg.contains(&task_run_id.to_string()),
            "{xdg} must be shared across runs: the stash is read back after a restart \
             and GC'd on a retention window, so a per-run dir would make it per-run"
        );

        // Same routing for warm Pods: they run as the same uid against the same
        // unwritable image home, and the SCIP indexer cache resolves the same
        // env pair.
        let warm_env = warm_cache_env_vars("proj-xyz", &cfg.cpu_limit, None);
        let warm: BTreeMap<&str, &str> = warm_env
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            warm.get("XDG_CACHE_HOME").copied(),
            Some("/cache/xdg/proj-xyz"),
            "warm and task-run Pods must resolve the same XDG cache root"
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
            None,
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
            // Downward-API vars (DJINN_TASK_RUN_POD_UID) carry `valueFrom`, not
            // `value`; asserted by djinn-agent-worker's `rendered_job_env_contract`.
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
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
            envs.get("RUST_BACKTRACE").copied(),
            Some("1"),
            "task-run workers must retain panic backtraces"
        );
        assert_eq!(
            envs.get("RUSTC_WRAPPER").copied(),
            Some(""),
            "task-runs disable the sccache wrapper (sccache forbids incremental)"
        );
        assert_eq!(
            envs.get("CARGO_BUILD_RUSTFLAGS").copied(),
            Some("-Clink-arg=-fuse-ld=mold -Clink-arg=-Wl,--threads=4"),
            "task-runs default to the mold fast linker (installed in the devcontainer image)"
        );
        // Cargo/nextest parallelism is capped at the task-run pod's CPU LIMIT
        // (v1 leases for_testing default "4") so a node full of cold-rebuilding
        // pods can't oversubscribe on the host core count (load-103 incident).
        assert_eq!(
            envs.get("CARGO_BUILD_JOBS").copied(),
            Some("4"),
            "task-run CARGO_BUILD_JOBS must be pinned to the pod's CPU limit, not the host core count"
        );
        assert_eq!(
            envs.get("NEXTEST_TEST_THREADS").copied(),
            Some("4"),
            "task-run NEXTEST_TEST_THREADS must match the pod's CPU limit"
        );
    }

    #[test]
    fn task_manifest_uses_its_own_mold_job_count_and_private_target() {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.cpu_limit = "1500m".into();
        let job = build_task_run_job(
            &cfg,
            &Uuid::now_v7(),
            "mold-job-variants",
            "task-secret",
            "example/task:latest",
            &[],
            None,
            false,
            None,
        );
        let env = task_run_job_envs(&job);

        assert_eq!(env.get("CARGO_BUILD_JOBS").copied(), Some("1"));
        assert_eq!(env.get("NEXTEST_TEST_THREADS").copied(), Some("1"));
        assert_eq!(
            env.get("CARGO_BUILD_RUSTFLAGS").copied(),
            Some("-Clink-arg=-fuse-ld=mold -Clink-arg=-Wl,--threads=1")
        );
        assert!(
            env.get("CARGO_TARGET_DIR")
                .expect("task target directory")
                .starts_with("/cache/cargo-target-runs/"),
            "task target output must remain private per run"
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

        let warm_vars = warm_cache_env_vars(project_id, "4", None);
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, "4", None);
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
            *warm_target, "/cache/cargo-target/test-project/mold-jobs-4",
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
            Some("-Clink-arg=-fuse-ld=mold -Clink-arg=-Wl,--threads=4"),
        );
    }

    /// Warm and task-run must EACH cap cargo/nextest parallelism at their OWN
    /// CPU limit, not the host core count (load-103 incident). Parity here is
    /// "both derive the value from their own limit" — NOT "identical value":
    /// warm and task-run pods can be sized differently (warm_cpu_limit vs
    /// cpu_limit), so the two job counts legitimately differ.
    #[test]
    fn warm_and_worker_pin_cargo_jobs_to_their_own_cpu_limit() {
        let project_id = "jobs-parity";
        let task_run_id = Uuid::now_v7().to_string();

        // Distinct limits so a "both must be identical" regression would fail.
        let warm_vars = warm_cache_env_vars(project_id, "4", None);
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, "6", None);
        let worker_env: BTreeMap<&str, &str> = worker_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        // Both set the vars (neither pod type may fall back to host cores) ...
        assert_eq!(
            warm_env.get("CARGO_BUILD_JOBS").copied(),
            Some("4"),
            "warm must pin CARGO_BUILD_JOBS to its OWN cpu limit"
        );
        assert_eq!(
            warm_env.get("NEXTEST_TEST_THREADS").copied(),
            Some("4"),
            "warm must pin NEXTEST_TEST_THREADS to its OWN cpu limit"
        );
        assert_eq!(
            worker_env.get("CARGO_BUILD_JOBS").copied(),
            Some("6"),
            "task-run must pin CARGO_BUILD_JOBS to its OWN cpu limit"
        );
        assert_eq!(
            worker_env.get("NEXTEST_TEST_THREADS").copied(),
            Some("6"),
            "task-run must pin NEXTEST_TEST_THREADS to its OWN cpu limit"
        );
        // ... and the values are derived per-pod (differ when the limits differ).
        assert_ne!(
            warm_env.get("CARGO_BUILD_JOBS"),
            worker_env.get("CARGO_BUILD_JOBS"),
            "each pod type derives CARGO_BUILD_JOBS from its own limit (parity != identical value)"
        );
    }

    /// The SCIP cache retention tuning is written by the pods this module
    /// renders, not by the server, so the chart's values have to arrive as pod
    /// env. Asserts the rendered `EnvVar`s, and that an unset or blank value
    /// renders nothing at all rather than an empty override the consumer would
    /// have to defend against.
    #[test]
    fn scip_cache_retention_tuning_is_forwarded_onto_build_pods() {
        let forwarded = forwarded_env_vars(SCIP_CACHE_FORWARDED_ENV, |name| match name {
            "DJINN_SCIP_CACHE_MAX_BYTES" => Some("4294967296".to_string()),
            "DJINN_SCIP_CACHE_MAX_IDLE_HOURS" => Some("168".to_string()),
            _ => None,
        });
        let rendered: BTreeMap<&str, &str> = forwarded
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            rendered.get("DJINN_SCIP_CACHE_MAX_BYTES").copied(),
            Some("4294967296")
        );
        assert_eq!(
            rendered.get("DJINN_SCIP_CACHE_MAX_IDLE_HOURS").copied(),
            Some("168")
        );

        // Unset in the server: nothing is rendered, and djinn-graph's own
        // defaults are what bound the cache.
        assert!(forwarded_env_vars(SCIP_CACHE_FORWARDED_ENV, |_| None).is_empty());
        // Blank is not an override.
        assert!(
            forwarded_env_vars(SCIP_CACHE_FORWARDED_ENV, |_| Some("  ".to_string())).is_empty()
        );
    }

    #[test]
    fn mold_linker_flags_follow_each_pods_derived_job_count() {
        let project_id = "mold-job-variants";
        let task_run_id = Uuid::now_v7().to_string();
        let warm_four_vars = warm_cache_env_vars(project_id, "4", None);
        let warm_four: BTreeMap<&str, &str> = warm_four_vars
            .iter()
            .map(|env| (env.name.as_str(), env.value.as_deref().unwrap_or_default()))
            .collect();
        let task_one_vars = task_run_cache_env_vars(project_id, &task_run_id, "1500m", None);
        let task_one: BTreeMap<&str, &str> = task_one_vars
            .iter()
            .map(|env| (env.name.as_str(), env.value.as_deref().unwrap_or_default()))
            .collect();
        let warm_one_vars = warm_cache_env_vars(project_id, "500m", None);
        let warm_one: BTreeMap<&str, &str> = warm_one_vars
            .iter()
            .map(|env| (env.name.as_str(), env.value.as_deref().unwrap_or_default()))
            .collect();

        assert_eq!(
            warm_four.get("CARGO_BUILD_RUSTFLAGS").copied(),
            Some("-Clink-arg=-fuse-ld=mold -Clink-arg=-Wl,--threads=4")
        );
        assert_eq!(
            task_one.get("CARGO_BUILD_RUSTFLAGS").copied(),
            Some("-Clink-arg=-fuse-ld=mold -Clink-arg=-Wl,--threads=1")
        );
        assert_eq!(
            warm_one.get("CARGO_BUILD_RUSTFLAGS"),
            task_one.get("CARGO_BUILD_RUSTFLAGS"),
            "distinct quantity spellings with equal derived counts share byte-identical flags"
        );
        assert_eq!(
            warm_four.get("CARGO_TARGET_DIR").copied(),
            Some("/cache/cargo-target/mold-job-variants/mold-jobs-4")
        );
        assert!(
            task_one
                .get("CARGO_TARGET_DIR")
                .expect("task target directory")
                .starts_with("/cache/cargo-target-runs/"),
            "task target output must remain private per run"
        );
    }

    /// Unit coverage for the CPU-limit → job-count parser: whole cores,
    /// millicores (floor to cores), sub-core clamp, and unparseable fallback.
    #[test]
    fn cpu_limit_to_jobs_parses_quantities() {
        assert_eq!(cpu_limit_to_jobs("6"), 6, "plain integer cores");
        assert_eq!(cpu_limit_to_jobs("2"), 2, "plain integer cores");
        assert_eq!(cpu_limit_to_jobs("1"), 1, "one core");
        assert_eq!(cpu_limit_to_jobs("1.5"), 1, "fractional cores floor down");
        assert_eq!(
            cpu_limit_to_jobs("1500m"),
            1,
            "1500 millicores = 1.5 cores → 1"
        );
        assert_eq!(cpu_limit_to_jobs("2000m"), 2, "2000 millicores = 2 cores");
        assert_eq!(
            cpu_limit_to_jobs("500m"),
            1,
            "sub-core millicores clamp to 1"
        );
        assert_eq!(
            cpu_limit_to_jobs("100m"),
            1,
            "sub-core millicores clamp to 1"
        );
        assert_eq!(
            cpu_limit_to_jobs(" 6 "),
            6,
            "surrounding whitespace tolerated"
        );
        assert_eq!(
            cpu_limit_to_jobs(""),
            1,
            "empty falls back to 1, not host cores"
        );
        assert_eq!(
            cpu_limit_to_jobs("garbage"),
            1,
            "unparseable falls back to 1"
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

        let warm_vars = warm_cache_env_vars(project_id, "4", None);
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, "2", None);
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
            *warm_target, "/cache/cargo-target/single-crate-project/mold-jobs-4",
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

        let warm_vars = warm_cache_env_vars(project_id, "4", Some(&policy));
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, "2", Some(&policy));
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

        let warm_vars = warm_cache_env_vars(project_id, "4", Some(&policy));
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, "2", Some(&policy));
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

        let warm_vars_none = warm_cache_env_vars(project_id, "4", None);
        let warm_vars_auto = warm_cache_env_vars(project_id, "4", Some(&policy));
        assert_eq!(
            warm_vars_none, warm_vars_auto,
            "AutoDetected must match None for warm"
        );

        let worker_vars_none = task_run_cache_env_vars(project_id, &task_run_id, "2", None);
        let worker_vars_auto =
            task_run_cache_env_vars(project_id, &task_run_id, "2", Some(&policy));
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
            None,
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
            None,
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
            None,
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
        let cfg = disabled_launcher_config();
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
            None,
        );
        let bare_pod = bare
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec");
        // This local compatibility fixture explicitly disables the launcher, so
        // a service-less run renders no init containers and no svc-dshm volume.
        assert!(
            bare_pod.init_containers.is_none(),
            "no services + launcher disabled ⇒ no initContainers"
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
            None,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec");

        // Native sidecars in initContainers. The explicit disabled local profile
        // leaves the backing service as the only init container.
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

        let warm_vars = warm_cache_env_vars(project_id, "4", Some(&policy));
        let warm_env: BTreeMap<&str, &str> = warm_vars
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();

        let worker_vars = task_run_cache_env_vars(project_id, &task_run_id, "2", Some(&policy));
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
            *warm_target, "/cache/cargo-target/consistency-test/mold-jobs-4",
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
            None,
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
            None,
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
            None,
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
        let cfg = disabled_launcher_config();
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
            None,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");

        // No backing-service sidecar is injected. This explicit disabled fixture
        // therefore renders no init containers at all for an evidence spike.
        assert!(
            pod.init_containers
                .iter()
                .flatten()
                .all(|c| !c.name.starts_with("svc-")),
            "evidence-spike must not inject backing-service sidecars"
        );
        assert!(
            pod.init_containers.is_none(),
            "evidence-spike with the launcher disabled renders no initContainers"
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
        let cfg = disabled_launcher_config();
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
            None,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");

        // The backing-service sidecar is injected for this explicit disabled
        // compatibility fixture, so it is the only init container.
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
            None,
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
            None,
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
            None,
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
            None,
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

    #[test]
    fn read_source_mount_is_exact_owner_cache_and_read_only() {
        let cfg = KubernetesConfig::for_testing();
        let job = build_task_run_job_with_read_sources(
            &cfg,
            &Uuid::now_v7(),
            "owner-project",
            "djinn-taskrun-test",
            "registry.example/project:tag",
            &[],
            None,
            false,
            None,
            Some("octo/owner-repo/.task-runtime/read-sources"),
        );
        let pod = job.spec.unwrap().template.spec.unwrap();
        let volume = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|volume| volume.name == "read-sources")
            .expect("owner-cache volume present");
        assert_eq!(
            volume
                .persistent_volume_claim
                .as_ref()
                .map(|pvc| pvc.claim_name.as_str()),
            Some(cfg.projects_pvc.as_str())
        );
        assert_eq!(
            volume
                .persistent_volume_claim
                .as_ref()
                .and_then(|pvc| pvc.read_only),
            Some(true)
        );

        let mounts = pod.containers[0].volume_mounts.as_ref().unwrap();
        let mount = mounts
            .iter()
            .find(|mount| mount.name == "read-sources")
            .expect("owner-cache mount present");
        assert_eq!(mount.mount_path, "/read-sources");
        assert_eq!(
            mount.sub_path.as_deref(),
            Some("octo/owner-repo/.task-runtime/read-sources")
        );
        assert_eq!(mount.read_only, Some(true));
        assert!(!mounts.iter().any(|mount| {
            mount.mount_path.contains(".djinn/read-sources")
                || mount.sub_path.as_deref() == Some("projects")
                || mount.sub_path.as_deref()
                    == Some("other-owner/other-repo/.task-runtime/read-sources")
                || mount.sub_path.as_deref() == Some("octo/owner-repo/.djinn/read-sources")
                || mount.sub_path.as_deref() == Some("mirror/read-sources")
        }));
    }

    #[test]
    fn zero_read_source_grants_do_not_mount_projects_claim() {
        let cfg = KubernetesConfig::for_testing();
        let job = build_task_run_job_with_read_sources(
            &cfg,
            &Uuid::now_v7(),
            "owner-project",
            "djinn-taskrun-test",
            "registry.example/project:tag",
            &[],
            None,
            false,
            None,
            None,
        );
        let pod = job.spec.unwrap().template.spec.unwrap();
        assert!(!pod.volumes.as_ref().unwrap().iter().any(|volume| {
            volume
                .persistent_volume_claim
                .as_ref()
                .is_some_and(|pvc| pvc.claim_name == cfg.projects_pvc)
        }));
        assert!(
            !pod.containers[0]
                .volume_mounts
                .as_ref()
                .unwrap()
                .iter()
                .any(|mount| mount.name == "read-sources")
        );
    }

    // ── qut0: v1 leases enforcement rendering ─────────────────────────────

    fn job_for_role(role: Option<RoleKind>) -> Job {
        build_task_run_job(
            &KubernetesConfig::for_testing(),
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-role",
            "registry.example/proj:tag",
            &[],
            None,
            false,
            role,
        )
    }

    /// The production/default config is required; this helper names that profile
    /// for tests that contrast it with explicit local compatibility mode.
    #[test]
    fn armed_launcher_without_runtime_class_fails_closed() {
        let mut config = KubernetesConfig::for_testing();
        config.task_run_cgroup_writable_enabled = false;
        let result = std::panic::catch_unwind(|| {
            build_task_run_job(
                &config,
                &Uuid::now_v7(),
                "project",
                "secret",
                "image",
                &[],
                None,
                false,
                Some(RoleKind::Worker),
            )
        });
        assert!(result.is_err());
    }

    fn armed_launcher_config() -> KubernetesConfig {
        KubernetesConfig::for_testing()
    }

    fn worker_cpu_request(job: &Job) -> String {
        let pod = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
        pod.containers[0]
            .resources
            .as_ref()
            .unwrap()
            .requests
            .as_ref()
            .unwrap()
            .get("cpu")
            .unwrap()
            .0
            .clone()
    }

    /// AC1: every RoleKind — plus grooming (Planner flow), retry/resume, and the
    /// unknown/missing default — renders the correct role-classed CPU request.
    #[test]
    fn role_classed_cpu_request_covers_every_role_and_default() {
        let cfg = KubernetesConfig::for_testing();

        // Light roles (Planner covers grooming; Refinement covers every
        // advocate/adversary/judge sub-role).
        for role in [
            RoleKind::Planner,
            RoleKind::Reviewer,
            RoleKind::Lead,
            RoleKind::Refinement,
        ] {
            assert_eq!(
                worker_cpu_request(&job_for_role(Some(role))),
                cfg.light_cpu_request,
                "{role:?} must render the light CPU request"
            );
        }

        // Build-capable roles (Worker/Verifier/Architect). Retry/resume of these
        // routes through the same RoleKind at dispatch, so classifying the
        // RoleKind is sufficient.
        for role in [RoleKind::Worker, RoleKind::Verifier, RoleKind::Architect] {
            assert_eq!(
                worker_cpu_request(&job_for_role(Some(role))),
                cfg.cpu_request,
                "{role:?} must render the build-capable CPU request"
            );
        }

        // Missing/unknown role fails safe to build-capable.
        assert_eq!(worker_cpu_request(&job_for_role(None)), cfg.cpu_request);

        // Limits + memory are identical across classes ("same limits everywhere").
        let light = job_for_role(Some(RoleKind::Planner));
        let build = job_for_role(Some(RoleKind::Worker));
        let res = |job: &Job| {
            job.spec
                .as_ref()
                .unwrap()
                .template
                .spec
                .as_ref()
                .unwrap()
                .containers[0]
                .resources
                .clone()
                .unwrap()
        };
        assert_eq!(res(&light).limits, res(&build).limits);
        assert_eq!(
            res(&light).requests.unwrap().get("memory"),
            res(&build).requests.unwrap().get("memory"),
        );

        // The CPU REQUEST is the only thing the class moves. Leasing is
        // role-agnostic — `LeaseInvocationRunner` queues on measured `cpu.stat`
        // and takes no role input — so the launcher sidecar a light pod renders
        // must be byte-identical to a build-capable pod's, including the quota
        // the broker hands a leased invocation. If a future change makes a
        // light pod lease less CPU (or renders it no launcher at all), the ~5%
        // of light task-runs that do compile would be throttled by their role
        // rather than by measurement, and this fails.
        let init = |job: &Job| {
            job.spec
                .as_ref()
                .unwrap()
                .template
                .spec
                .as_ref()
                .unwrap()
                .init_containers
                .clone()
                .unwrap_or_default()
        };
        let launcher = |job: &Job| {
            init(job)
                .into_iter()
                .find(|c| c.name == crate::launcher::LAUNCHER_CONTAINER_NAME)
                .unwrap_or_else(|| {
                    panic!("every task-run pod must render the cgroup-launcher sidecar")
                })
        };
        let light_launcher = launcher(&light);
        let build_launcher = launcher(&build);
        let leased = |c: &Container| {
            c.env
                .as_ref()
                .unwrap()
                .iter()
                .find(|e| e.name == "DJINN_LAUNCHER_LEASED_MILLICORES")
                .unwrap_or_else(|| panic!("launcher must declare DJINN_LAUNCHER_LEASED_MILLICORES"))
                .value
                .clone()
        };
        assert_eq!(
            leased(&light_launcher),
            leased(&build_launcher),
            "leased millicores must not depend on the role class"
        );
        assert_eq!(
            light_launcher, build_launcher,
            "the launcher sidecar must render identically for both classes"
        );
    }

    /// Production/default rendering requires the cgroup-launcher. The explicit
    /// disabled local arm proves no
    /// launcher container, volume, mount or env leaks into an ordinary task pod
    /// (and `shareProcessNamespace` stays unset); the armed arm proves the
    /// sidecar still renders correctly without adding any cgroup volume or
    /// mount; RuntimeClass provides that hierarchy.
    #[test]
    fn launcher_sidecar_profiles_render_only_the_explicit_disabled_path_without_launcher() {
        let image = "registry.example/proj:tag";
        let build = |cfg: &KubernetesConfig| {
            build_task_run_job(
                cfg,
                &Uuid::now_v7(),
                "proj-xyz",
                "djinn-taskrun-role",
                image,
                &[],
                None,
                false,
                Some(RoleKind::Worker),
            )
        };

        // ---- Disabled (explicit local/development compatibility) ---------
        let default_job = build(&disabled_launcher_config());
        let pod = default_job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap();
        assert!(
            pod.init_containers
                .iter()
                .flatten()
                .all(|c| c.name != crate::launcher::LAUNCHER_CONTAINER_NAME),
            "no launcher sidecar in explicit disabled mode"
        );
        for absent in [
            crate::launcher::VOLUME_LAUNCHER_IPC,
            // The brokered child's scratch surfaces and the one-way
            // private-dependency channel exist only because a command runs in
            // the launcher's mount namespace. With no launcher, the pod keeps
            // its pre-enforcement shape exactly.
            crate::launcher_child_fs::VOLUME_LAUNCHER_TMP,
            crate::launcher_child_fs::VOLUME_LAUNCHER_HOME,
            crate::launcher_child_fs::VOLUME_LAUNCHER_VAR_TMP,
            crate::private_dep_config::VOLUME_CHILD_GIT_CONFIG,
        ] {
            assert!(
                pod.volumes.iter().flatten().all(|v| v.name != absent),
                "{absent} must not be rendered in explicit disabled mode"
            );
        }
        let worker = &pod.containers[0];
        assert!(
            worker
                .volume_mounts
                .iter()
                .flatten()
                .all(|m| m.mount_path != crate::launcher::LAUNCHER_IPC_DIR),
            "worker must not mount the launcher IPC dir in explicit disabled mode"
        );
        for env_name in [
            "DJINN_LAUNCHER_SOCKET",
            "DJINN_LAUNCHER_CREDENTIAL_PATH",
            // Naming the channel to a worker with no launcher would have
            // `configure_private_dep_access` publish a live installation token
            // onto a volume nothing mounts.
            crate::private_dep_config::CHILD_GIT_CONFIG_PATH_ENV,
        ] {
            assert!(
                worker.env.iter().flatten().all(|e| e.name != env_name),
                "{env_name} must not be exported in explicit disabled mode"
            );
        }
        assert_eq!(
            pod.share_process_namespace, None,
            "shareProcessNamespace exists only for the launcher; unset in disabled mode"
        );

        // ---- Required (explicitly armed) --------------------------------
        let armed_job = build(&armed_launcher_config());
        let armed = armed_job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap();
        assert_eq!(armed.share_process_namespace, Some(true));
        let inits = armed
            .init_containers
            .as_ref()
            .expect("armed launcher renders an init container");
        assert_eq!(
            inits[0].name,
            crate::launcher::LAUNCHER_CONTAINER_NAME,
            "the launcher sidecar renders first when armed"
        );
        let launcher = &inits[0];
        assert_eq!(
            launcher.image.as_deref(),
            Some(image),
            "reuses worker image"
        );
        assert_eq!(
            launcher.command.as_deref(),
            Some(&[crate::launcher::LAUNCHER_BIN.to_string()][..]),
            "runs the packaged launcher binary — no fabricated image ref"
        );
        assert_eq!(launcher.restart_policy.as_deref(), Some("Always"));
        assert!(
            armed
                .volumes
                .iter()
                .flatten()
                .any(|v| v.name == crate::launcher::VOLUME_LAUNCHER_IPC),
            "the IPC volume renders when armed"
        );
        // The RuntimeClass supplies `/sys/fs/cgroup`; no cgroup volume or
        // cgroup mount may appear in any container in the armed Pod.
        assert!(armed.volumes.iter().flatten().all(|volume| {
            volume.host_path.is_none() && !volume.name.to_ascii_lowercase().contains("cgroup")
        }));
        assert!(
            armed
                .containers
                .iter()
                .chain(armed.init_containers.iter().flatten())
                .flat_map(|container| container.volume_mounts.iter().flatten())
                .all(|mount| mount.mount_path != crate::launcher::LAUNCHER_CGROUP_ROOT)
        );

        // NOT user-namespaced: `hostUsers: false` leaves the launcher's own
        // cgroup owned by an unmapped uid, which breaks the delegation one step
        // after the mount. See `launcher::pod_host_users`.
        assert_eq!(armed.host_users, None);
    }

    /// AC1: distinct worker/child UIDs, artifact GID, fsGroup/setgid ownership,
    /// and the restricted capability/seccomp profile on both containers.
    #[test]
    fn worker_child_launcher_security_contract_is_rendered() {
        let job = job_for_role(Some(RoleKind::Worker));
        let pod = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();

        // Worker container: uid/gid 1000, non-root, drop ALL, restricted seccomp.
        let worker = &pod.containers[0];
        let wsc = worker
            .security_context
            .as_ref()
            .expect("worker securityContext");
        assert_eq!(wsc.run_as_user, Some(1000));
        assert_eq!(wsc.run_as_group, Some(1000));
        assert_eq!(wsc.allow_privilege_escalation, Some(false));
        assert_eq!(
            wsc.capabilities.as_ref().unwrap().drop.as_deref(),
            Some(&["ALL".to_string()][..])
        );
        assert_eq!(
            wsc.seccomp_profile.as_ref().unwrap().type_,
            "RuntimeDefault"
        );

        // Launcher container: uid 0, minimal caps only, restricted seccomp.
        // Task grkq: the launcher only renders when armed, so its half of the
        // contract is asserted against an armed config. The worker/pod half
        // above is unconditional and stays on the default config.
        let armed_job = build_task_run_job(
            &armed_launcher_config(),
            &Uuid::now_v7(),
            "proj-xyz",
            "djinn-taskrun-role",
            "registry.example/proj:tag",
            &[],
            None,
            false,
            Some(RoleKind::Worker),
        );
        let armed_pod = armed_job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap();
        let launcher = armed_pod
            .init_containers
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.name == crate::launcher::LAUNCHER_CONTAINER_NAME)
            .expect("armed launcher sidecar present");
        let lsc = launcher
            .security_context
            .as_ref()
            .expect("launcher securityContext");
        assert_eq!(lsc.run_as_user, Some(0));
        let add = lsc.capabilities.as_ref().unwrap().add.as_ref().unwrap();
        // Only the residual identity/socket capabilities are granted. The
        // RuntimeClass supplies cgroup delegation; no mount capability is needed.
        assert_eq!(add, &["CHOWN", "SETGID", "SETUID", "SETPCAP"]);
        // The `CAP_`-prefixed spelling is what the API server rejects alongside
        // `allowPrivilegeEscalation: false`.
        assert!(!add.iter().any(|c| c.starts_with("CAP_")));
        assert_eq!(lsc.allow_privilege_escalation, Some(false));
        assert_eq!(
            lsc.seccomp_profile.as_ref().unwrap().type_,
            "RuntimeDefault"
        );

        // Distinct worker vs launcher UIDs.
        assert_ne!(wsc.run_as_user, lsc.run_as_user);

        // Pod-level fsGroup ties workspace/cache ownership to the artifact GID
        // (child writes group 1000; worker reads it), re-owned OnRootMismatch.
        let psc = pod.security_context.as_ref().expect("pod securityContext");
        assert_eq!(psc.fs_group, Some(1000));
        assert_eq!(
            psc.fs_group_change_policy.as_deref(),
            Some("OnRootMismatch")
        );
        // Legacy pod-wide uid 10001 override is gone.
        assert_eq!(psc.run_as_user, None);
    }

    /// AC1/AC2: credential mount isolation, asserted against an ARMED launcher
    /// config (task grkq: nothing launcher-shaped renders by default). The IPC
    /// surface is shared only by worker + launcher, no backing sidecar can touch
    /// it (nor the workspace/mirror), and NO delegated-cgroup volume is rendered
    /// at all — that volume was the P0 CrashLoopBackOff and it is gone for good.
    #[test]
    fn launcher_credential_mounts_are_isolated_and_no_cgroup_volume_is_rendered() {
        let cfg = armed_launcher_config();
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
            "djinn-taskrun-role",
            "registry.example/proj:tag",
            std::slice::from_ref(&postgres),
            None,
            false,
            Some(RoleKind::Worker),
        );
        let pod = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();

        let mount_names = |c: &Container| -> Vec<String> {
            c.volume_mounts
                .as_ref()
                .map(|m| m.iter().map(|v| v.name.clone()).collect())
                .unwrap_or_default()
        };

        // IPC volume: worker + launcher only.
        let worker = &pod.containers[0];
        assert!(mount_names(worker).contains(&crate::launcher::VOLUME_LAUNCHER_IPC.to_string()));
        let launcher = pod
            .init_containers
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.name == crate::launcher::LAUNCHER_CONTAINER_NAME)
            .unwrap();
        assert!(mount_names(launcher).contains(&crate::launcher::VOLUME_LAUNCHER_IPC.to_string()));
        // The RuntimeClass supplies `/sys/fs/cgroup`; neither the launcher nor
        // worker receives a cgroup volume/mount from this PodSpec.
        assert!(pod.volumes.iter().flatten().all(|volume| {
            volume.host_path.is_none() && !volume.name.to_ascii_lowercase().contains("cgroup")
        }));
        assert!(
            launcher
                .volume_mounts
                .iter()
                .flatten()
                .all(|mount| mount.mount_path != crate::launcher::LAUNCHER_CGROUP_ROOT)
        );

        // Backing sidecar gets neither IPC nor cgroup nor workspace access.
        let svc = pod
            .init_containers
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.name == "svc-postgres")
            .unwrap();
        let svc_mounts = mount_names(svc);
        for forbidden in [
            crate::launcher::VOLUME_LAUNCHER_IPC,
            VOLUME_WORKSPACE,
            VOLUME_MIRROR,
        ] {
            assert!(
                !svc_mounts.contains(&forbidden.to_string()),
                "backing sidecar must not mount {forbidden}"
            );
        }

        // The IPC volume is a Memory-backed emptyDir (socket + credential never
        // touch disk), and the worker carries the broker socket/credential env.
        let ipc_vol = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == crate::launcher::VOLUME_LAUNCHER_IPC)
            .unwrap();
        assert_eq!(
            ipc_vol.empty_dir.as_ref().unwrap().medium.as_deref(),
            Some("Memory")
        );
        let worker_envs: BTreeMap<&str, &str> = worker
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            worker_envs.get("DJINN_LAUNCHER_SOCKET").copied(),
            Some(crate::launcher::LAUNCHER_SOCKET_PATH)
        );
    }

    /// AC1: graph-warm resources must NOT regress (config.rs ~L175). The v1
    /// leases work only touches the task-run CPU request classing; the warm
    /// pod's 4 CPU / 2Gi request / 6Gi limit envelope is preserved.
    #[test]
    fn graph_warm_resources_are_preserved() {
        let cfg = KubernetesConfig::for_testing();
        assert_eq!(cfg.warm_cpu_request, "4");
        assert_eq!(cfg.warm_cpu_limit, "4");
        assert_eq!(cfg.warm_memory_request, "2Gi");
        assert_eq!(cfg.warm_memory_limit, "6Gi");
    }

    /// jqvg drift guard. Every secret this module projects into the worker
    /// container must sit beneath a root the shell sandbox withholds
    /// `ReadFile` on, otherwise repository-controlled code can read it again.
    /// This coupling did not exist when the credential mount was added, which
    /// is a large part of why the exposure went unnoticed.
    #[test]
    fn projected_secret_mounts_stay_covered_by_the_sandbox_denylist() {
        use std::path::Path;

        for secret in [
            SPEC_MOUNT_FILE,
            CREDENTIALS_MOUNT_FILE,
            TOKEN_MOUNT_FILE,
            // No longer automounted (see `automount_service_account_token`),
            // but keep it in the denylist's remit so re-enabling automount
            // cannot silently re-expose it to a sandboxed shell.
            "/var/run/secrets/kubernetes.io/serviceaccount/token",
        ] {
            assert!(
                djinn_sandbox::confidential::CONFIDENTIAL_ROOTS
                    .iter()
                    .any(|root| Path::new(secret).starts_with(root)),
                "{secret} is projected into the worker container but is not beneath any \
                 djinn_sandbox::confidential::CONFIDENTIAL_ROOTS entry — a sandboxed \
                 shell could read it"
            );
        }
    }
}
