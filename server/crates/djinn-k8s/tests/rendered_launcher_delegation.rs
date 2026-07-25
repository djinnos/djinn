//! The coverage whose absence hid task grkq's P0.
//!
//! # What went wrong, and why nothing caught it
//!
//! `launcher_cgroup_volume()` rendered the cgroup-launcher's "private delegated
//! cgroup root" as a plain `emptyDir`. An emptyDir is an ordinary directory on
//! the node filesystem — it has no `cgroup.subtree_control`, no `cpu.max`, none
//! of the cgroup2 control files the launcher opens. So on every task-run Pod the
//! sidecar died in `NativeCgroupFs::open`, the broker socket never bound, the
//! worker handshake never completed, and (because the in-process fallback the
//! comments promised was `#[cfg(test)]`-only) every shell command in every task
//! run failed.
//!
//! Three test suites existed and none could see it:
//!   * `djinn-k8s`'s own `job.rs`/`launcher.rs` unit tests assert the manifest as
//!     a *struct* — an `EmptyDirVolumeSource` is a perfectly well-formed volume,
//!     so every assertion passed;
//!   * `djinn-cgroup-launcher`'s containment suite drives a `FakeCgroup`, which
//!     is a `HashMap` and therefore satisfies any readiness the test wants;
//!   * `djinn-k8s/tests/kind_smoke.rs` is `#[ignore]`d behind `DJINN_TEST_KIND`,
//!     nothing in CI sets it, and it asserts nothing about the launcher anyway.
//!
//! The missing link was always the same: **nobody ever took the volume source
//! this crate actually renders, materialized it the way a kubelet would, and
//! handed it to the real launcher code.** That is exactly what this file does.
//!
//! # Why it is not `#[ignore]`d and needs no privileges
//!
//! It runs the REAL `djinn_cgroup_launcher::NativeCgroupFs::open` and the REAL
//! `NativeClone3::preflight` against a REAL kernel — no fakes — but every check
//! is a `statfs`/`openat`/`clone3`-argument-validation on a directory this test
//! owns. There is nothing to gate, so it runs in the ordinary
//! `cargo test -p djinn-k8s` lane where a human sees it, on every PR.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{EmptyDirVolumeSource, PodSpec, Volume};
use uuid::Uuid;

use djinn_cgroup_launcher::{CGROUP2_SUPER_MAGIC, Error as LauncherError, NativeCgroupFs};
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::launcher::{
    CgroupLauncherMode, LAUNCHER_CGROUP_ROOT, LAUNCHER_CONTAINER_NAME, LAUNCHER_UID,
    RenderValidationError, VOLUME_LAUNCHER_CGROUP, validate_enforcement_render,
};

/// Render the real task-run Job under `mode`.
fn render(mode: CgroupLauncherMode) -> Job {
    let mut config = KubernetesConfig::for_testing();
    config.cgroup_launcher_mode = mode;
    build_task_run_job(
        &config,
        &Uuid::now_v7(),
        "proj-grkq",
        "djinn-taskrun-grkq",
        "registry.example/djinn-project:grkq",
        &[],
        None,
        false,
        None,
    )
}

fn pod_of(job: &Job) -> &PodSpec {
    job.spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("rendered Job has a pod spec")
}

/// A private scratch directory under the crate's test tmpdir. Deliberately not
/// `tempfile`: this crate does not carry it as a dev-dependency, and adding one
/// for a `mkdir` would drag the whole workspace-hack regeneration along.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("grkq-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create scratch dir");
        Self(base)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every `(container_name, volume)` pair that mounts `mount_path`.
fn volumes_mounted_at<'a>(pod: &'a PodSpec, mount_path: &str) -> Vec<(&'a str, &'a Volume)> {
    let volumes = pod.volumes.as_deref().unwrap_or_default();
    pod.containers
        .iter()
        .chain(pod.init_containers.iter().flatten())
        .flat_map(|container| {
            container
                .volume_mounts
                .iter()
                .flatten()
                .filter(|mount| mount.mount_path == mount_path)
                .map(move |mount| (container.name.as_str(), mount.name.as_str()))
        })
        .map(|(container, volume_name)| {
            let volume = volumes
                .iter()
                .find(|volume| volume.name == volume_name)
                .unwrap_or_else(|| {
                    panic!(
                        "{container} mounts volume {volume_name}, which the pod does not declare"
                    )
                });
            (container, volume)
        })
        .collect()
}

/// Materialize a rendered volume source the way a kubelet would, and return the
/// directory the container would see. `None` means "this source cannot be
/// materialized locally", which for our purposes is itself an answer.
fn materialize(volume: &Volume, scratch: &Path) -> Option<PathBuf> {
    if volume.empty_dir.is_some() {
        // An emptyDir is a fresh, empty directory on the node: for a `medium:
        // Memory` volume a tmpfs, otherwise the node's own filesystem. Neither
        // is a cgroup2 hierarchy, which is the entire point.
        let dir = scratch.join(&volume.name);
        std::fs::create_dir_all(&dir).expect("materialize emptyDir");
        return Some(dir);
    }
    if let Some(host_path) = volume.host_path.as_ref() {
        return Some(PathBuf::from(&host_path.path));
    }
    None
}

/// **The contract.** For the manifest this crate actually renders: either the
/// launcher is not rendered at all, or the delegated cgroup root it mounts
/// materializes into a filesystem the real launcher accepts.
///
/// Run against BOTH modes, so arming the launcher in the future cannot quietly
/// reintroduce a root that is not a cgroup2 tree.
#[test]
fn every_rendered_delegated_cgroup_root_is_a_real_cgroup2_tree() {
    let scratch = Scratch::new("delegated-root");

    for mode in [CgroupLauncherMode::Disabled, CgroupLauncherMode::Required] {
        let job = render(mode);
        let pod = pod_of(&job);

        for (container, volume) in volumes_mounted_at(pod, LAUNCHER_CGROUP_ROOT) {
            let root = materialize(volume, scratch.path()).unwrap_or_else(|| {
                panic!(
                    "{mode:?}: container {container} mounts a delegated cgroup root at \
                     {LAUNCHER_CGROUP_ROOT} from volume {} whose source cannot be a cgroup2 \
                     hierarchy",
                    volume.name
                )
            });

            // The real launcher code, on a real kernel, against the volume this
            // crate really renders. No fake, no fixture, no ignore gate.
            if let Err(error) = NativeCgroupFs::open(&root, LAUNCHER_UID as u32) {
                panic!(
                    "{mode:?}: the rendered delegated cgroup root for container {container} \
                     (volume {}) is not usable by the launcher: {error}. A task-run Pod with \
                     this rendering CrashLoopBackOffs its cgroup-launcher sidecar on startup.",
                    volume.name
                );
            }
        }
    }
}

/// Prove the check above has teeth: the exact volume source that shipped the P0
/// is rejected, by name, by the same code path the contract test runs.
///
/// Without this, `every_rendered_delegated_cgroup_root_is_a_real_cgroup2_tree`
/// could pass vacuously (it does pass vacuously today — nothing mounts that path
/// any more) and nobody would know whether it can fail at all.
#[test]
fn the_emptydir_delegated_root_that_shipped_the_p0_is_rejected_by_name() {
    let scratch = Scratch::new("delegated-root");
    // Byte-for-byte the volume `launcher_cgroup_volume()` used to return.
    let shipped = Volume {
        name: VOLUME_LAUNCHER_CGROUP.to_string(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Volume::default()
    };
    let root = materialize(&shipped, scratch.path()).expect("emptyDir materializes");

    let error = NativeCgroupFs::open(&root, LAUNCHER_UID as u32)
        .err()
        .expect("an emptyDir can never satisfy the launcher readiness contract");

    // Named, not an opaque ENOENT from reading `cgroup.subtree_control`. The
    // opacity was half of why this took a production outage to find.
    match error {
        LauncherError::DelegatedRootIsNotCgroupFs {
            expected, actual, ..
        } => {
            assert_eq!(expected, CGROUP2_SUPER_MAGIC);
            assert_ne!(
                actual, CGROUP2_SUPER_MAGIC,
                "an emptyDir must not report itself as cgroup2"
            );
        }
        other => panic!("expected a named non-cgroup2 readiness failure, got: {other}"),
    }
}

/// The default rendering carries no launcher surface whatsoever — not the
/// sidecar, not the cgroup volume, and not a `/run/djinn-cgroup` mount.
#[test]
fn the_default_rendering_has_no_launcher_surface() {
    let job = render(CgroupLauncherMode::Disabled);
    let pod = pod_of(&job);

    let containers: BTreeSet<&str> = pod
        .containers
        .iter()
        .chain(pod.init_containers.iter().flatten())
        .map(|container| container.name.as_str())
        .collect();
    assert!(
        !containers.contains(LAUNCHER_CONTAINER_NAME),
        "the launcher sidecar must not render by default: {containers:?}"
    );

    assert!(
        volumes_mounted_at(pod, LAUNCHER_CGROUP_ROOT).is_empty(),
        "nothing may mount a delegated cgroup root by default"
    );
    assert!(
        !pod.volumes
            .iter()
            .flatten()
            .any(|volume| volume.name == VOLUME_LAUNCHER_CGROUP),
        "the delegated cgroup volume must not be declared by default"
    );
}

/// Even when the mode is armed, no `launcher-cgroup` volume is rendered.
///
/// A delegated cgroup v2 subtree is not something a *volume source* can supply
/// at all: in the goxi design the launcher establishes its own cgroup2 mount
/// inside its own cgroup namespace (which needs `CAP_SYS_ADMIN`, not a volume).
/// This is the guard against "fix" attempts that re-add an emptyDir, or reach
/// for a hostPath — which `nsdelegate` refuses across the pod's private cgroup
/// namespace anyway.
#[test]
fn arming_the_launcher_still_renders_no_bogus_cgroup_volume() {
    let pod = render(CgroupLauncherMode::Required);
    let pod = pod_of(&pod);
    assert!(
        !pod.volumes
            .iter()
            .flatten()
            .any(|volume| volume.name == VOLUME_LAUNCHER_CGROUP),
        "no volume source can supply a delegated cgroup v2 subtree in this topology"
    );
}

/// The render-time half of the readiness contract: arming the launcher fails
/// closed at dispatch, before a Job is submitted, rather than after a pod has
/// come up with a sidecar that cannot start.
#[test]
fn arming_the_launcher_fails_render_validation_before_any_job_is_submitted() {
    let mut config = KubernetesConfig::for_testing();
    assert!(
        validate_enforcement_render(&config).is_ok(),
        "the default (disabled) config must dispatch"
    );

    config.cgroup_launcher_mode = CgroupLauncherMode::Required;
    let error = validate_enforcement_render(&config)
        .expect_err("an unsatisfiable delegation must fail closed");
    assert!(
        matches!(error, RenderValidationError::CgroupDelegationUnavailable),
        "expected CgroupDelegationUnavailable, got: {error}"
    );
    // The operator has to be told what to do, not just that it failed — and the
    // message must not misrepresent an unbuilt runtime profile as a platform
    // limit, because that is how a solvable gap gets remembered as impossible.
    let message = error.to_string();
    for expected in [
        "CAP_SYS_ADMIN",
        "not a platform limit",
        "DJINN_K8S_CGROUP_LAUNCHER_MODE=disabled",
    ] {
        assert!(
            message.contains(expected),
            "the rejection must explain {expected}: {message}"
        );
    }
}

/// The launcher mode round-trips through its config string and refuses typos, so
/// a malformed `DJINN_K8S_CGROUP_LAUNCHER_MODE` can neither arm nor disarm
/// enforcement by accident.
#[test]
fn launcher_mode_parses_exactly_its_two_documented_values() {
    for mode in [CgroupLauncherMode::Disabled, CgroupLauncherMode::Required] {
        assert_eq!(CgroupLauncherMode::parse(mode.as_str()), Some(mode));
    }
    assert_eq!(CgroupLauncherMode::default(), CgroupLauncherMode::Disabled);
    assert!(!CgroupLauncherMode::Disabled.renders_sidecar());
    assert!(CgroupLauncherMode::Required.renders_sidecar());
    for typo in ["Required", "enabled", "", "requried"] {
        assert_eq!(
            CgroupLauncherMode::parse(typo),
            None,
            "{typo:?} must not parse"
        );
    }
}
