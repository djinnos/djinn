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
//! It runs the REAL `djinn_cgroup_launcher::NativeCgroupFs::open` against a REAL
//! kernel — no fakes — but every check is a `statfs`/`openat` on a directory
//! this test owns. There is nothing to gate, so it runs in the ordinary
//! `cargo test -p djinn-k8s` lane where a human sees it, on every PR.
//!
//! # What changed for task 7deu
//!
//! The volume rendered at `/run/djinn-cgroup` is back — but as a writable
//! MOUNTPOINT, not as a delegated root. That distinction is the whole lesson of
//! the P0, so it is asserted in both directions:
//! [`the_rendered_cgroup_root_is_a_mountpoint_and_never_a_delegated_root`]
//! requires the volume to materialize AND requires `NativeCgroupFs::open` on it
//! to FAIL. The delegation is established by the launcher's own `mount(2)`; that
//! end-to-end chain — mount, delegate, throttle, lift, measured on `cpu.stat` —
//! is proven in `djinn-cgroup-launcher`'s privileged lane, which is the only
//! place a real cgroup2 hierarchy exists.

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
    RenderValidationError, VOLUME_LAUNCHER_CGROUP, VOLUME_LAUNCHER_IPC,
    validate_enforcement_render,
};

/// Render the production/default required profile.
fn render() -> Job {
    let config = KubernetesConfig::for_testing();
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

/// **The contract.** The volume rendered at [`LAUNCHER_CGROUP_ROOT`] is a
/// writable MOUNTPOINT, and it must never be mistaken for the delegation.
///
/// Both halves matter, and both are asserted against the real launcher code on
/// a real kernel:
///
/// * it must materialize into a directory at all (a source that cannot be
///   materialized locally cannot be mounted onto in a pod either), and
/// * `NativeCgroupFs::open` on it must **FAIL**, by name. That failure is the
///   proof that the launcher's own `mount(2)` is load-bearing. If this ever
///   started passing, someone would have handed the launcher a "delegated root"
///   that is not one — which is exactly the P0 that CrashLoopBackOffed the
///   sidecar on every task-run Pod.
#[test]
fn the_rendered_cgroup_root_is_a_mountpoint_and_never_a_delegated_root() {
    let scratch = Scratch::new("delegated-root");
    let job = render();
    let pod = pod_of(&job);

    let mounted = volumes_mounted_at(pod, LAUNCHER_CGROUP_ROOT);
    assert_eq!(
        mounted.len(),
        1,
        "exactly one container mounts the cgroup root, and it is the launcher"
    );
    let (container, volume) = mounted[0];
    assert_eq!(container, LAUNCHER_CONTAINER_NAME);

    let root = materialize(volume, scratch.path()).unwrap_or_else(|| {
        panic!(
            "the cgroup mountpoint is rendered from volume {}, whose source cannot be \
             materialized into a directory the launcher could mount onto",
            volume.name
        )
    });

    let error = NativeCgroupFs::open(&root, LAUNCHER_UID as u32)
        .err()
        .expect(
            "the rendered volume must NOT satisfy the delegated-root contract on its own; if it \
         does, the launcher's own mount(2) is not what establishes the delegation and the \
         readiness check proves nothing",
        );
    match error {
        LauncherError::DelegatedRootIsNotCgroupFs {
            expected, actual, ..
        } => {
            assert_eq!(expected, CGROUP2_SUPER_MAGIC);
            assert_ne!(actual, CGROUP2_SUPER_MAGIC);
        }
        other => panic!("expected a named non-cgroup2 readiness failure, got: {other}"),
    }
}

/// Prove the check above has teeth: the exact volume source that shipped the P0
/// is rejected, by name, by the same code path.
#[test]
fn the_emptydir_delegated_root_that_shipped_the_p0_is_rejected_by_name() {
    let scratch = Scratch::new("shipped-p0");
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

/// The production default is the required enforcement profile.
#[test]
fn the_default_rendering_has_the_mandatory_launcher_surface() {
    let job = render();
    let pod = pod_of(&job);

    let containers: BTreeSet<&str> = pod
        .containers
        .iter()
        .chain(pod.init_containers.iter().flatten())
        .map(|container| container.name.as_str())
        .collect();
    assert!(
        containers.contains(LAUNCHER_CONTAINER_NAME),
        "the production default must render the mandatory launcher: {containers:?}"
    );

    assert!(
        volumes_mounted_at(pod, LAUNCHER_CGROUP_ROOT)
            .iter()
            .any(|(container, _)| *container == LAUNCHER_CONTAINER_NAME),
        "the mandatory launcher must mount its private cgroup mountpoint"
    );
    assert!(
        pod.volumes
            .iter()
            .flatten()
            .any(|volume| volume.name == VOLUME_LAUNCHER_CGROUP),
        "the mandatory launcher cgroup mountpoint volume must be declared"
    );
    assert_eq!(
        pod.host_users,
        Some(false),
        "the mandatory launcher bootstrap must be user-namespaced"
    );
}

/// A `hostPath` is still not the route, armed or not.
///
/// The pod's private cgroup namespace is mounted `nsdelegate`, so a target
/// outside that namespace root is unreachable no matter what the manifest says.
/// This is the guard against "fix" attempts that reach for one.
#[test]
fn no_host_path_is_ever_rendered_for_the_cgroup_root() {
    let job = render();
    let pod = pod_of(&job);
    for (container, volume) in volumes_mounted_at(pod, LAUNCHER_CGROUP_ROOT) {
        assert!(
            volume.host_path.is_none(),
            "container {container} mounts a hostPath at the cgroup root; \
             nsdelegate refuses a target outside the pod's cgroup namespace"
        );
    }
}

/// The armed render is dispatchable — that is the deliverable — and the
/// preconditions that make it dispatchable are individually load-bearing.
///
/// This replaces a gate that unconditionally refused the armed mode while the
/// runtime profile did not exist. The refusal has not been deleted, it has been
/// made specific: each check rejects a render that would produce a sidecar which
/// cannot start, and does so BEFORE the Job is submitted.
#[test]
fn the_armed_render_is_dispatchable_and_every_precondition_still_fails_closed() {
    let mut config = KubernetesConfig::for_testing();
    assert!(
        validate_enforcement_render(&config).is_ok(),
        "the production required config must dispatch"
    );

    // The lease is the only ceiling left once the launcher container carries no
    // CPU limit, so a pod CPU limit that cannot become one is refused.
    let mut unusable = config.clone();
    unusable.cpu_limit = "500m".to_string();
    let error = validate_enforcement_render(&unusable)
        .expect_err("a lease quota below the launcher crate's floor must fail closed");
    assert!(
        matches!(error, RenderValidationError::UnsupportedLeaseQuota { .. }),
        "expected UnsupportedLeaseQuota, got: {error}"
    );
    let message = error.to_string();
    assert!(
        message.contains("bounded only by the node"),
        "the operator must be told what is at stake: {message}"
    );
}

/// Defect 1, on the manifest the API server would receive: the launcher
/// container must declare NO CPU limit.
///
/// Under `nsdelegate` the delegated root IS the launcher's container cgroup, so
/// a limit here is an ancestor clamp on every invocation leaf. Measured with the
/// leaf at four cores and a 250m container limit: `usage_usec 1252296` over a 5s
/// window — 0.25 core — with `nr_throttled` reading 0 in the leaf because the
/// throttling happened at the parent. Removing it took the same pod to
/// `nr_throttled 40/40` unleased and 1.995 measured cores after the lift.
#[test]
fn the_armed_launcher_container_has_no_cpu_limit_and_a_real_memory_limit() {
    let config = KubernetesConfig::for_testing();
    let job = render();
    let pod = pod_of(&job);
    let launcher = pod
        .init_containers
        .iter()
        .flatten()
        .find(|container| container.name == LAUNCHER_CONTAINER_NAME)
        .expect("the armed render includes the launcher sidecar");

    let limits = launcher
        .resources
        .as_ref()
        .and_then(|resources| resources.limits.as_ref())
        .expect("the launcher declares limits");
    assert!(
        !limits.contains_key("cpu"),
        "a CPU limit on the launcher clamps every invocation leaf to it: {limits:?}"
    );
    // Every command now runs in this container's cgroup, so the build's memory
    // peak lands here — a sidecar-sized memory limit would OOM-kill the first
    // `cargo build`.
    assert_eq!(limits.get("memory").unwrap().0, config.memory_limit);
    let requests = launcher
        .resources
        .as_ref()
        .and_then(|resources| resources.requests.as_ref())
        .expect("launcher requests");
    assert_eq!(requests.get("cpu").unwrap().0, "50m");
    assert_eq!(requests.get("memory").unwrap().0, "64Mi");
    let leased = launcher
        .env
        .as_ref()
        .and_then(|env| {
            env.iter()
                .find(|var| var.name == "DJINN_LAUNCHER_LEASED_MILLICORES")
        })
        .and_then(|var| var.value.as_deref());
    assert_eq!(
        leased,
        Some("4000"),
        "the lifted quota must be explicit, never max"
    );
}

/// The armed pod runs in a user namespace, and the launcher's capabilities are
/// spelled the way the API server accepts.
#[test]
fn the_armed_pod_confines_the_bootstrap_capability() {
    let job = render();
    let pod = pod_of(&job);
    assert_eq!(
        pod.host_users,
        Some(false),
        "hostUsers: false maps the launcher's bootstrap CAP_SYS_ADMIN into a user namespace, \
         where the non-namespaced sysctls that make it an escape primitive are unreachable"
    );

    let launcher = pod
        .init_containers
        .iter()
        .flatten()
        .find(|container| container.name == LAUNCHER_CONTAINER_NAME)
        .expect("the armed render includes the launcher sidecar");
    let security = launcher
        .security_context
        .as_ref()
        .expect("launcher securityContext");
    assert_eq!(security.allow_privilege_escalation, Some(false));
    assert_eq!(security.privileged, None);
    assert_eq!(
        security
            .seccomp_profile
            .as_ref()
            .map(|profile| profile.type_.as_str()),
        Some("RuntimeDefault")
    );
    let added = security
        .capabilities
        .as_ref()
        .and_then(|caps| caps.add.as_deref())
        .unwrap_or_default();
    assert!(
        added.iter().any(|capability| capability == "SYS_ADMIN"),
        "the launcher cannot mount its delegated root without SYS_ADMIN: {added:?}"
    );
    for capability in added {
        assert!(
            !capability.starts_with("CAP_"),
            "{capability} uses the spelling the API server rejects alongside \
             allowPrivilegeEscalation: false"
        );
    }
    for sidecar in pod.init_containers.iter().flatten() {
        if sidecar.name == LAUNCHER_CONTAINER_NAME {
            continue;
        }
        let mounts = sidecar.volume_mounts.as_deref().unwrap_or_default();
        for forbidden in [VOLUME_LAUNCHER_IPC, VOLUME_LAUNCHER_CGROUP, "workspace"] {
            assert!(
                !mounts.iter().any(|mount| mount.name == forbidden),
                "optional sidecar {} must not mount {forbidden}",
                sidecar.name
            );
        }
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
    assert_eq!(CgroupLauncherMode::default(), CgroupLauncherMode::Required);
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
