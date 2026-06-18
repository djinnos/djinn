//! Pure `Job` manifest builder for a one-shot pre-PR verification run.
//!
//! Verification commands (`cargo clippy`, `cargo test`, `pnpm test`, …) need
//! the project's toolchain + the shared `/cache` PVC, neither of which exists on
//! the djinn-server host — running them inline there exits 127 ("cargo: not
//! found") and false-fails every task. So, like [`crate::warm_job`] and
//! [`crate::verification_test_job`], verification runs in the project's image:
//! the Job clones the target branch from the upstream (fresh installation token,
//! rotated by the mirror fetcher), fetches + checks out the task branch so the
//! pod builds the same tree the worker pushed, then execs `djinn-agent-worker
//! verify-task <run_id>`. The worker normalizes mtimes, resolves the scoped
//! verification commands, runs the real pipeline (`verify_commit`), and writes
//! per-command results + pass/fail back to the `verification_runs` row. The
//! server polls that row, then runs the existing pass/fail transitions.
//!
//! `backoffLimit: 0` — a failed *run* of the Job is itself recorded by the
//! worker (or surfaced to the server as a poll timeout); we don't retry the pod.
//!
//! Verification reuses the warm per-project cargo target base on `/cache` as a
//! read-only SEED, exactly like task-run pods: the worker seeds a private run
//! target dir from the warm base and recompiles only the task's delta
//! incrementally (`CARGO_INCREMENTAL=1`), instead of cold-building or churning a
//! shared mutable base — see [`crate::job::verify_cache_env_vars`].

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, PersistentVolumeClaimVolumeSource, PodSpec,
    PodTemplateSpec, ResourceRequirements, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use uuid::Uuid;

use crate::config::KubernetesConfig;
use crate::sidecar::{BackingServiceSpec, sidecar_container, sidecar_conn_env, sidecar_dshm_volume};
use crate::warm_job::{
    MIRROR_MOUNT_DIR, VOLUME_MIRROR, VOLUME_WORKSPACE, WARM_COMMAND_BIN, WORKSPACE_MOUNT_DIR,
    sanitize_id, short_uuid,
};

/// `djinn.app/component` value written on verification resources.
pub const COMPONENT_VERIFICATION: &str = "verification";
/// Label key identifying a verification Job.
pub const LABEL_VERIFICATION: &str = "djinn.app/verification";
/// Label key for the verification-run id a Job targets.
pub const LABEL_RUN_ID: &str = "djinn.app/verification-run-id";
const LABEL_COMPONENT: &str = "djinn.app/component";
const LABEL_PROJECT_ID: &str = "djinn.app/project-id";

/// Build the Job manifest for one pre-PR verification run.
///
/// Clones `target_branch` from the upstream, fetches + checks out `task_branch`,
/// then runs `verify-task <run_id>` in the project's image (`project_image_tag`
/// is resolved by the caller — verification needs the project's toolchain image
/// to be `ready`).
/// `services` are the backing services declared on the project's image
/// (resolved via [`crate::sidecar::resolve_image_services`]); each is injected
/// as a native sidecar so verification runs the same tests against the same
/// services the worker had. Pass an empty slice for none.
pub fn build_verification_job(
    config: &KubernetesConfig,
    project_id: &str,
    project_image_tag: &str,
    run_id: &str,
    task_branch: &str,
    target_branch: &str,
    services: &[BackingServiceSpec],
) -> Job {
    let suffix = Uuid::now_v7();
    let sanitized_project = sanitize_id(project_id);
    let sanitized_run = sanitize_id(run_id);
    // The Job name is copied verbatim into an auto-injected `job-name` pod
    // label, which Kubernetes caps at 63 bytes. Keep only a short slice of the
    // run id in the name; the full id is preserved in LABEL_RUN_ID for lookups.
    let short_run: String = sanitized_run.chars().take(12).collect();
    let job_name = format!("djinn-verify-{short_run}-{}", short_uuid(&suffix));

    let project_root = format!("{WORKSPACE_MOUNT_DIR}/{sanitized_project}");
    let mirror_path = format!("{MIRROR_MOUNT_DIR}/{project_id}.git");

    let mut labels = BTreeMap::new();
    labels.insert(
        LABEL_COMPONENT.to_string(),
        COMPONENT_VERIFICATION.to_string(),
    );
    labels.insert(LABEL_VERIFICATION.to_string(), "true".to_string());
    labels.insert(LABEL_PROJECT_ID.to_string(), sanitized_project.clone());
    labels.insert(LABEL_RUN_ID.to_string(), sanitized_run);

    // Clone the target branch from the upstream URL (fresh installation token,
    // rotated by the mirror fetcher) for a full, buildable main tree. The TASK
    // branch, however, is pushed by workers to the local bare MIRROR — not to
    // upstream (it only reaches GitHub later, at PR time) — so fetch it from the
    // mirror, which holds the worker's commits + their blobs. Fetching it from
    // `origin` (upstream) fails with "couldn't find remote ref" for any task
    // not yet on GitHub. main's blobs come from the upstream clone; the task
    // delta's blobs come from the mirror, so the checkout tree is complete with
    // no promisor fetch. Install JS deps lockfile-gated, then run verify-task.
    let cmd = format!(
        r#"set -euo pipefail
git config --global --add safe.directory "{mirror_path}"
UPSTREAM_URL="$(git -C "{mirror_path}" config remote.origin.url)"
git clone --single-branch --branch "{target_branch}" "$UPSTREAM_URL" "{project_root}"
cd "{project_root}"
git fetch "{mirror_path}" "{task_branch}:refs/remotes/origin/{task_branch}"
git checkout -B "{task_branch}" "origin/{task_branch}"
if [ -f pnpm-lock.yaml ]; then
  ( corepack enable >/dev/null 2>&1 || true; \
    corepack pnpm install --frozen-lockfile || pnpm install --frozen-lockfile || pnpm install ) || true
elif [ -f yarn.lock ]; then
  ( corepack enable >/dev/null 2>&1 || true; \
    corepack yarn install --frozen-lockfile || yarn install ) || true
elif [ -f package-lock.json ]; then
  ( npm ci || npm install ) || true
fi
exec {bin} verify-task "{run_id}"
"#,
        mirror_path = mirror_path,
        project_root = project_root,
        target_branch = target_branch,
        task_branch = task_branch,
        bin = WARM_COMMAND_BIN,
        run_id = run_id,
    );

    let mut env = vec![
        env_var("DJINN_MIRROR_ROOT", MIRROR_MOUNT_DIR),
        env_var("DJINN_PROJECT_ROOT", &project_root),
        env_var("RUST_LOG", "info,djinn=debug"),
    ];
    if let Some(url) = config.database_url.as_deref() {
        env.push(env_var("DJINN_DATABASE_URL", url));
    }
    // Route the Rust toolchain caches to the /cache PVC. Verification reuses the
    // warm per-project cargo target base as a read-only SEED (like task-runs):
    // the worker seeds a private run target dir from it and recompiles only the
    // task's delta incrementally (CARGO_INCREMENTAL=1) — no shared-base writes,
    // no Cargo build-dir lock contention. Single-sourced in job.rs; needs the
    // cache volume below.
    env.extend(crate::job::verify_cache_env_vars(project_id));
    // One connection env var per injected backing service (e.g.
    // TEST_POSTGRES_URL → 127.0.0.1:5432), matching the task-run pod.
    env.extend(services.iter().map(sidecar_conn_env));

    let container = Container {
        name: "verify".to_string(),
        image: Some(project_image_tag.to_string()),
        image_pull_policy: Some(config.image_pull_policy.clone()),
        command: Some(vec!["/bin/bash".to_string(), "-c".to_string(), cmd]),
        env: Some(env),
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
            VolumeMount {
                name: crate::job::VOLUME_CACHE.to_string(),
                mount_path: crate::job::CACHE_MOUNT_DIR.to_string(),
                read_only: Some(false),
                ..VolumeMount::default()
            },
            crate::env_config::env_config_volume_mount(),
        ]),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(config.warm_cpu_request.clone())),
                (
                    "memory".to_string(),
                    Quantity(config.warm_memory_request.clone()),
                ),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(config.warm_cpu_limit.clone())),
                (
                    "memory".to_string(),
                    Quantity(config.warm_memory_limit.clone()),
                ),
            ])),
            ..ResourceRequirements::default()
        }),
        ..Container::default()
    };

    let mut volumes = vec![
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
        Volume {
            name: crate::job::VOLUME_CACHE.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: config.cache_pvc.clone(),
                read_only: Some(false),
            }),
            ..Volume::default()
        },
        crate::env_config::env_config_volume(project_id),
    ];
    if !services.is_empty() {
        volumes.push(sidecar_dshm_volume());
    }

    let init_containers = (!services.is_empty()).then(|| {
        services
            .iter()
            .map(|s| sidecar_container(config, s))
            .collect::<Vec<_>>()
    });

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
        // Run as uid 10001 like task-runs (job.rs). The verification pod shares
        // the /cache cargo target PVC with workers; without this it runs as the
        // image default (root) and writes root-owned cargo fingerprints the
        // worker (uid 10001) then can't overwrite — permission-denied builds.
        security_context: Some(k8s_openapi::api::core::v1::PodSecurityContext {
            run_as_user: Some(10001),
            run_as_group: Some(10001),
            ..Default::default()
        }),
        ..PodSpec::default()
    };

    Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(config.namespace.clone()),
            labels: Some(labels.clone()),
            ..ObjectMeta::default()
        },
        spec: Some(JobSpec {
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..ObjectMeta::default()
                }),
                spec: Some(pod_spec),
            },
            backoff_limit: Some(0),
            ttl_seconds_after_finished: Some(config.warm_job_ttl_seconds),
            active_deadline_seconds: Some(config.warm_job_timeout_seconds),
            ..JobSpec::default()
        }),
        ..Job::default()
    }
}

fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..EnvVar::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_verification_job_with_expected_shape() {
        let cfg = KubernetesConfig::for_testing();
        let job = build_verification_job(
            &cfg,
            "proj-xyz",
            "reg.example:5000/djinn-project-p:abc123",
            "run-123",
            "task/ab12",
            "main",
            &[],
        );
        let name = job.metadata.name.unwrap();
        assert!(name.starts_with("djinn-verify-run-123-"), "got {name}");
        let spec = job.spec.unwrap();
        assert_eq!(spec.backoff_limit, Some(0));
        let pod = spec.template.spec.unwrap();
        // Must run as uid 10001 like task-runs, or it writes root-owned cargo
        // artifacts into the shared /cache target and breaks worker builds.
        assert_eq!(
            pod.security_context.as_ref().and_then(|s| s.run_as_user),
            Some(10001),
            "verify pod must run as the worker uid to share the cargo cache safely"
        );
        let c = &pod.containers[0];
        assert_eq!(
            c.image.as_deref(),
            Some("reg.example:5000/djinn-project-p:abc123")
        );
        let cmd = c.command.as_ref().unwrap().join(" ");
        assert!(
            cmd.contains("verify-task"),
            "command must invoke verify-task: {cmd}"
        );
        assert!(
            cmd.contains("run-123"),
            "command must pass the run id: {cmd}"
        );
        // Pod must build the task branch's tree (clone target, fetch+checkout task).
        assert!(
            cmd.contains("--branch \"main\""),
            "clones target branch: {cmd}"
        );
        assert!(
            cmd.contains("checkout -B \"task/ab12\""),
            "checks out task branch: {cmd}"
        );
        // Mirror (ro) + workspace (rw) + cache (rw) mounts present.
        let mounts = c.volume_mounts.as_ref().unwrap();
        assert!(
            mounts
                .iter()
                .any(|m| m.name == VOLUME_MIRROR && m.read_only == Some(true))
        );
        assert!(mounts.iter().any(|m| m.name == VOLUME_WORKSPACE));
        assert!(
            mounts
                .iter()
                .any(|m| m.name == crate::job::VOLUME_CACHE && m.read_only == Some(false))
        );
        // Verification keeps shared CARGO_HOME/SCCACHE routing. CARGO_TARGET_DIR
        // points at the warm base as the seed source + fallback; the worker
        // overrides it to a private run dir at runtime. Incremental is ENABLED so
        // the run dir recompiles only the task delta over main's warm artifacts.
        let envs: BTreeMap<&str, &str> = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(envs.get("CARGO_HOME").copied(), Some("/cache/cargo"));
        assert_eq!(
            envs.get("CARGO_TARGET_DIR").copied(),
            Some("/cache/cargo-target/proj-xyz"),
            "verification seeds from the shared warm cargo target base"
        );
        assert_eq!(
            envs.get("CARGO_INCREMENTAL").copied(),
            Some("0"),
            "verification reuses the warm base via cargo freshness + sccache (incremental=0; sccache forbids incremental)"
        );
        assert!(
            !envs.contains_key("RUSTC_WRAPPER"),
            "verification must not force sccache (it disables incremental)"
        );
        assert_eq!(
            envs.get("SCCACHE_DIR").copied(),
            Some("/cache/sccache/proj-xyz")
        );
        assert_eq!(envs.get("SQLX_OFFLINE").copied(), Some("true"));
        assert_eq!(
            envs.get("DJINN_PROJECT_ROOT").copied(),
            Some("/workspace/proj-xyz")
        );
    }

    #[test]
    fn job_name_stays_within_k8s_label_budget() {
        // Kubernetes copies the Job name into an auto-injected `job-name` pod
        // label, rejected above 63 bytes. A full UUID run id plus the
        // `djinn-verify-` prefix used to overrun this and 422 the run.
        let cfg = KubernetesConfig::for_testing();
        let job = build_verification_job(
            &cfg,
            "019ea3bd-a305-73e3-806c-4edcc96ebfe2",
            "reg.example:5000/djinn-project-p:abc123",
            "019ea7ed-7db6-7252-ad45-d4180f934386",
            "task/019ea7ed",
            "main",
            &[],
        );
        let name = job.metadata.name.unwrap();
        assert!(
            name.len() <= 63,
            "job name must fit the 63-byte k8s label cap, got {} bytes: {name}",
            name.len()
        );
        assert!(name.starts_with("djinn-verify-"), "got {name}");
    }
}
