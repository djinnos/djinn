//! Pure `Job` manifest builder for a one-shot verification "test" run.
//!
//! Mirrors [`crate::warm_job`]: the Job runs in the project's image (where the
//! toolchain lives — the server host has none), `git clone`s the default branch
//! into an emptyDir workspace, then execs `djinn-agent-worker verify-test
//! <test_id>`. The worker reads the candidate rules from the
//! `verification_test_runs` row, runs them via the real verification pipeline
//! (`verify_commit`), and writes pass/fail + per-command output back to the row.
//! The MCP layer polls that row and gates "save verification rules" on a pass —
//! so a broken rule can't be saved and disrupt live tasks.
//!
//! `backoffLimit: 0` — a failed *run* of the Job is itself a "test failed"
//! signal the worker records before exiting; we don't retry the pod.

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
use crate::warm_job::{
    MIRROR_MOUNT_DIR, VOLUME_MIRROR, VOLUME_WORKSPACE, WARM_COMMAND_BIN, WORKSPACE_MOUNT_DIR,
    sanitize_id, short_uuid,
};

/// `djinn.app/component` value written on verification-test resources.
pub const COMPONENT_VERIFICATION_TEST: &str = "verification-test";
/// Label key identifying a verification-test Job.
pub const LABEL_VERIFICATION_TEST: &str = "djinn.app/verification-test";
/// Label key for the test-run id a Job targets.
pub const LABEL_TEST_ID: &str = "djinn.app/test-id";
const LABEL_COMPONENT: &str = "djinn.app/component";
const LABEL_PROJECT_ID: &str = "djinn.app/project-id";

/// Build the Job manifest for one verification-test run.
///
/// Clones the default branch (remote HEAD, like the warm Job) of `project_id`
/// into a writable emptyDir, then runs `verify-test <test_id>` in the project
/// image. `project_image_tag` is resolved by the caller (the test needs the
/// project's toolchain image to be `ready`).
pub fn build_verification_test_job(
    config: &KubernetesConfig,
    project_id: &str,
    project_image_tag: &str,
    test_id: &str,
) -> Job {
    let suffix = Uuid::now_v7();
    let sanitized_project = sanitize_id(project_id);
    let sanitized_test = sanitize_id(test_id);
    // The Job name is copied verbatim into an auto-injected `job-name` pod
    // label, which Kubernetes caps at 63 bytes. The full test UUID plus the
    // `djinn-verify-test-` prefix and the disambiguator overruns that cap, so
    // keep only a short slice of the test id in the name; the full id is still
    // preserved in LABEL_TEST_ID below for lookups.
    let short_test: String = sanitized_test.chars().take(12).collect();
    let job_name = format!("djinn-verify-test-{short_test}-{}", short_uuid(&suffix));

    let project_root = format!("{WORKSPACE_MOUNT_DIR}/{sanitized_project}");
    let mirror_path = format!("{MIRROR_MOUNT_DIR}/{project_id}.git");

    let mut labels = BTreeMap::new();
    labels.insert(
        LABEL_COMPONENT.to_string(),
        COMPONENT_VERIFICATION_TEST.to_string(),
    );
    labels.insert(LABEL_VERIFICATION_TEST.to_string(), "true".to_string());
    labels.insert(LABEL_PROJECT_ID.to_string(), sanitized_project.clone());
    labels.insert(LABEL_TEST_ID.to_string(), sanitized_test);

    // Same clone strategy as the warm Job: the bare mirror is a blobless
    // partial clone, so pull the upstream URL (fresh installation token,
    // rotated by the mirror fetcher) and do a shallow single-branch clone of
    // the default branch straight from the origin. The verify-test worker then
    // runs the candidate commands against this working tree via verify_commit
    // (which itself runs the project's `pre_verification` setup hooks, e.g.
    // `pnpm install`, before the commands).
    let cmd = format!(
        r#"set -euo pipefail
git config --global --add safe.directory "{mirror_path}"
UPSTREAM_URL="$(git -C "{mirror_path}" config remote.origin.url)"
git clone --depth 1 --single-branch "$UPSTREAM_URL" "{project_root}"
exec {bin} verify-test "{test_id}"
"#,
        mirror_path = mirror_path,
        project_root = project_root,
        bin = WARM_COMMAND_BIN,
        test_id = test_id,
    );

    let mut env = vec![
        env_var("DJINN_MIRROR_ROOT", MIRROR_MOUNT_DIR),
        env_var("DJINN_PROJECT_ROOT", &project_root),
        env_var("RUST_LOG", "info,djinn=debug"),
    ];
    if let Some(url) = config.database_url.as_deref() {
        env.push(env_var("DJINN_DATABASE_URL", url));
    }
    // Route the Rust toolchain caches to the /cache PVC. Verification-test Pods
    // write the same shared per-project target base as warm Pods, now with
    // incremental compilation ENABLED so the base carries reusable incremental
    // state; task-run/verification Pods use private run target dirs seeded from
    // it but keep the same shared CARGO_HOME/SCCACHE settings. Single-sourced in
    // job.rs; needs the cache volume below.
    env.extend(crate::job::warm_cache_env_vars(project_id, None));

    let container = Container {
        name: "verify-test".to_string(),
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
        Volume {
            name: crate::job::VOLUME_CACHE.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: config.cache_pvc.clone(),
                read_only: Some(false),
            }),
            ..Volume::default()
        },
    ];

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
        // Run as uid 10001 like task-runs (job.rs). Both share the /cache
        // cargo target PVC; without this the verify pod runs as the image
        // default (root) and writes root-owned cargo fingerprints that the
        // worker (uid 10001) then can't overwrite — permission-denied builds
        // that hang for the whole stall window, then trip the model breaker.
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
    fn builds_verification_test_job_with_expected_shape() {
        let cfg = KubernetesConfig::for_testing();
        let job = build_verification_test_job(
            &cfg,
            "proj-xyz",
            "reg.example:5000/djinn-project-p:abc123",
            "test-123",
        );
        let name = job.metadata.name.unwrap();
        assert!(
            name.starts_with("djinn-verify-test-test-123-"),
            "got {name}"
        );
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
            cmd.contains("verify-test"),
            "command must invoke verify-test: {cmd}"
        );
        assert!(
            cmd.contains("test-123"),
            "command must pass the test id: {cmd}"
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
        // Verification keeps shared CARGO_HOME/SCCACHE routing while using the
        // warm/verification-owned base target dir with incremental disabled.
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
            "verification-test jobs must write the shared warm/verification cargo target base"
        );
        // The CARGO_INCREMENTAL / RUSTC_WRAPPER compile-strategy invariant is
        // asserted exhaustively in `crate::job::tests` (warm == verify ==
        // worker parity); not re-pinned here to avoid coupling this throwaway
        // shape test to that env-builder.
        assert_eq!(
            envs.get("SCCACHE_DIR").copied(),
            Some("/cache/sccache/proj-xyz")
        );
        assert_eq!(envs.get("SCCACHE_CACHE_SIZE").copied(), Some("20G"));
        assert_eq!(envs.get("SQLX_OFFLINE").copied(), Some("true"));
    }

    #[test]
    fn job_name_stays_within_k8s_label_budget() {
        // Kubernetes copies the Job name into an auto-injected `job-name` pod
        // label, which is rejected above 63 bytes. A full UUID test id plus the
        // `djinn-verify-test-` prefix used to overrun this and 422 the Test run.
        let cfg = KubernetesConfig::for_testing();
        let job = build_verification_test_job(
            &cfg,
            "019ea3bd-a305-73e3-806c-4edcc96ebfe2",
            "reg.example:5000/djinn-project-p:abc123",
            "019ea7ed-7db6-7252-ad45-d4180f934386",
        );
        let name = job.metadata.name.unwrap();
        assert!(
            name.len() <= 63,
            "job name must fit the 63-byte k8s label cap, got {} bytes: {name}",
            name.len()
        );
        assert!(name.starts_with("djinn-verify-test-"), "got {name}");
    }
}
