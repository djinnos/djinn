//! Enforcement-pod rendering: role-classed resources, the mandatory
//! cgroup-launcher sidecar, the v1 worker/child/launcher security contract, and
//! the fail-closed render/startup validation seam.
//!
//! This module is *pure*: every function is a deterministic manifest builder or
//! a `Result`-returning validator. Nothing here talks to a cluster or mutates
//! process state — that keeps the whole v1 pod shape unit-testable via struct
//! assertions (the pattern the existing `job.rs` tests already rely on).
//!
//! The security constants are re-exported from the landed `djinn-cgroup-launcher`
//! crate (epic kh95) so the render side and the launcher's *runtime* readiness
//! validation can never drift:
//!   * worker container UID/GID = [`WORKER_UID`]/[`WORKER_GID`] (1000/1000);
//!   * the launcher-spawned child runs as [`CHILD_UID`] (1001) with the
//!     [`ARTIFACT_GID`] (1000) primary group — applied by the launcher at
//!     runtime, so the render only ties `fsGroup` to `ARTIFACT_GID`;
//!   * the delegated cgroup root is owned by [`LAUNCHER_UID`] (0), exactly the
//!     `expected_uid` the launcher's [`Readiness::validate`] checks.

use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, EnvVar, PodSecurityContext,
    ResourceRequirements, SeccompProfile, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use thiserror::Error;

use djinn_cgroup_launcher::broker::{WORKER_GID, WORKER_UID};
// Re-export the child/artifact contract constants so djinn-k8s consumers (and
// intra-doc links here) see the same UIDs the launcher enforces at runtime.
// `CHILD_UID` is applied by the launcher when it spawns the child, not in the
// pod manifest, so it is only referenced by tests/docs on the render side.
pub use djinn_cgroup_launcher::child::{ARTIFACT_GID, CHILD_UID};
use djinn_cgroup_launcher::{CgroupMode, Error as LauncherError, Readiness, UnleasedQuota};
use djinn_runtime::RoleKind;

use crate::config::KubernetesConfig;

/// UID the privileged launcher/broker container runs as. Root is required so the
/// launcher can `setresuid`/`setresgid` the child down to [`CHILD_UID`]/
/// [`ARTIFACT_GID`] and write `cpu.max` in its delegated cgroup root; it holds
/// ONLY the minimal capabilities in [`launcher_capabilities`]. This is also the
/// `expected_uid` the launcher's [`Readiness::validate`] requires the delegated
/// cgroup root to be owned by.
pub const LAUNCHER_UID: i64 = 0;

/// Container name of the mandatory cgroup-launcher sidecar.
pub const LAUNCHER_CONTAINER_NAME: &str = "cgroup-launcher";

/// Path of the packaged launcher binary inside the per-project devcontainer
/// image. Parallels [`crate::warm_job::WARM_COMMAND_BIN`]
/// (`/opt/djinn/bin/djinn-agent-worker`): both are laid down at `/opt/djinn/bin`
/// by the image-builder so the launcher rides the SAME image as the worker with
/// a different entrypoint (see `server/crates/djinn-image-builder/src/dockerfile.rs`
/// and `server/docker/djinn-agent-runtime.Dockerfile`). No fabricated image ref:
/// the launcher container reuses `project_image_tag`.
pub const LAUNCHER_BIN: &str = "/opt/djinn/bin/djinn-cgroup-launcher";

/// Unleased CPU quota (millicores) the broker pins on every child cgroup before
/// a lease lifts it. Sourced from the launcher crate's own default so the value
/// the render advertises is exactly what the crate accepts. "Same broker
/// everywhere" — this is role-independent.
pub const LAUNCHER_UNLEASED_MILLICORES: u16 = UnleasedQuota::DEFAULT_MILLICORES;

/// Supported cgroup delegation profile string (the only one v1 accepts).
pub const CGROUP_PROFILE_V2_CPU_ONLY: &str = "cgroup-v2-cpu-only";
/// Supported volume-ownership mode string (fsGroup re-owned OnRootMismatch).
pub const VOLUME_OWNERSHIP_ON_ROOT_MISMATCH: &str = "fsgroup-on-root-mismatch";

/// Launcher sidecar CPU/memory bounds — a tiny, fixed envelope; the broker does
/// almost no work of its own. Role-independent ("same broker everywhere").
pub const LAUNCHER_CPU_REQUEST: &str = "50m";
pub const LAUNCHER_CPU_LIMIT: &str = "250m";
pub const LAUNCHER_MEMORY_REQUEST: &str = "64Mi";
pub const LAUNCHER_MEMORY_LIMIT: &str = "128Mi";

/// Volume name for the worker↔launcher IPC surface (broker control socket +
/// worker-private launcher credential). Memory-backed emptyDir, mounted into the
/// worker and launcher ONLY — never into a backing-service sidecar, and closed
/// off from the launcher-spawned child (the launcher clones the child with
/// `ChildMounts::isolated()`).
pub const VOLUME_LAUNCHER_IPC: &str = "launcher-ipc";
/// Mount path of [`VOLUME_LAUNCHER_IPC`] in the worker and launcher.
pub const LAUNCHER_IPC_DIR: &str = "/var/run/djinn/launcher";
/// Broker control socket path inside [`LAUNCHER_IPC_DIR`].
pub const LAUNCHER_SOCKET_PATH: &str = "/var/run/djinn/launcher/broker.sock";
/// Worker-private launcher credential path inside [`LAUNCHER_IPC_DIR`].
pub const LAUNCHER_CREDENTIAL_PATH: &str = "/var/run/djinn/launcher/credential";

/// Volume name for the launcher's private delegated cgroup root. Mounted into
/// the launcher container ONLY.
pub const VOLUME_LAUNCHER_CGROUP: &str = "launcher-cgroup";
/// Mount path of [`VOLUME_LAUNCHER_CGROUP`] (the delegated cgroup-v2 root the
/// launcher opens; owned by [`LAUNCHER_UID`]).
pub const LAUNCHER_CGROUP_ROOT: &str = "/run/djinn-cgroup";

/// v1 resource class a task-run Pod is provisioned under, derived from the role
/// that executes the run. Only the CPU **request** differs between the two — the
/// CPU limit, both memory bounds, and the launcher/broker contract are identical
/// ("role changes REQUESTS only — same limits, same broker everywhere").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoleResourceClass {
    /// Orchestration-only roles that never run the project's compile/test
    /// toolchain: Planner, Reviewer, Lead, every Refinement sub-role, and
    /// grooming (a Planner-driven flow). Fractional-core CPU request.
    Light,
    /// Roles that may compile/build/test: Worker, Verifier, Architect, and any
    /// retry/resume of those. Full-core CPU request. Also the FAIL-SAFE default
    /// for a missing/unknown/newly-added role.
    BuildCapable,
}

impl RoleResourceClass {
    /// Classify the role that executes a task-run.
    ///
    /// Deliberately a catch-all (`_ => BuildCapable`) rather than an exhaustive
    /// match on [`RoleKind`]: an unrecognized, missing (`None`), or
    /// newly-introduced role must **fail safe to build-capable** so a pod that
    /// might compile is never under-provisioned. Only the explicitly-listed
    /// light roles are ever classed light.
    pub fn for_role(role: Option<RoleKind>) -> Self {
        match role {
            Some(
                RoleKind::Planner | RoleKind::Reviewer | RoleKind::Lead | RoleKind::Refinement,
            ) => Self::Light,
            _ => Self::BuildCapable,
        }
    }

    /// The CPU request quantity string for this class, sourced from the
    /// (env-overridable) config.
    pub fn cpu_request<'a>(&self, config: &'a KubernetesConfig) -> &'a str {
        match self {
            Self::Light => &config.light_cpu_request,
            Self::BuildCapable => &config.cpu_request,
        }
    }

    /// Stable telemetry/label string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::BuildCapable => "build-capable",
        }
    }
}

/// Worker-container resource requirements for a task-run of the given role
/// class. CPU request is role-classed; CPU limit and both memory bounds are the
/// shared, env-overridable config values.
pub fn worker_resources(
    config: &KubernetesConfig,
    class: RoleResourceClass,
) -> ResourceRequirements {
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            (
                "cpu".to_string(),
                Quantity(class.cpu_request(config).to_string()),
            ),
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
    }
}

/// `RuntimeDefault` seccomp profile shared by the worker and launcher
/// containers.
fn runtime_default_seccomp() -> SeccompProfile {
    SeccompProfile {
        type_: "RuntimeDefault".to_string(),
        ..SeccompProfile::default()
    }
}

/// Container-level security context for the worker.
///
/// v1 contract: runs as [`WORKER_UID`]/[`WORKER_GID`] (1000/1000) — NOT the
/// legacy pod-wide uid 10001 — drops all capabilities, forbids privilege
/// escalation, and pins the restricted seccomp profile. Non-dumpability is
/// asserted by the worker itself at runtime (`prepare_worker_readiness` sets
/// `PR_SET_DUMPABLE=0` and re-reads it) before it authenticates to the broker;
/// there is no Pod-manifest field for dumpability, so the render supplies the
/// restricted profile and the runtime supplies the prctl.
///
/// The root filesystem is left writable: the worker legitimately writes
/// `/workspace`, `/cache`, and `/mirror`. Isolation of secrets is by mount
/// (private credential volume) and by the Pod boundary, not by a read-only
/// rootfs.
pub fn worker_security_context() -> SecurityContext {
    SecurityContext {
        run_as_user: Some(i64::from(WORKER_UID)),
        run_as_group: Some(i64::from(WORKER_GID)),
        run_as_non_root: Some(true),
        allow_privilege_escalation: Some(false),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            ..Capabilities::default()
        }),
        seccomp_profile: Some(runtime_default_seccomp()),
        ..SecurityContext::default()
    }
}

/// Minimal capability set the launcher keeps. It runs as root but drops
/// everything except the three it needs to move the child across the credential
/// boundary before exec:
///   * `SETUID`/`SETGID` — `setresuid(1001)` / `setresgid(1000)` +
///     `setgroups(0)` to drop the child to [`CHILD_UID`]/[`ARTIFACT_GID`];
///   * `SETPCAP` — clear the child's capability bounding/ambient set.
///
/// It needs no `SYS_ADMIN`: cgroup control is confined to the *delegated* root
/// mounted at [`LAUNCHER_CGROUP_ROOT`] (owned by uid 0), and the child is born
/// there via `clone3(CLONE_INTO_CGROUP)` with no mount/namespace ops.
fn launcher_capabilities() -> Capabilities {
    Capabilities {
        drop: Some(vec!["ALL".to_string()]),
        add: Some(vec![
            "SETUID".to_string(),
            "SETGID".to_string(),
            "SETPCAP".to_string(),
        ]),
    }
}

/// Container-level security context for the launcher sidecar. Root, minimal
/// caps, restricted seccomp, read-only rootfs (it only writes the delegated
/// cgroup and the control socket, both on dedicated mounts). `no_new_privs` on
/// the *children* it spawns is applied by the launcher at runtime
/// (`child::prepare_child`); `allowPrivilegeEscalation: false` here matches that
/// intent for the launcher process itself (dropping to the child uid still works
/// under NNP).
pub fn launcher_security_context() -> SecurityContext {
    SecurityContext {
        run_as_user: Some(LAUNCHER_UID),
        run_as_group: Some(LAUNCHER_UID),
        run_as_non_root: Some(false),
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        capabilities: Some(launcher_capabilities()),
        seccomp_profile: Some(runtime_default_seccomp()),
        ..SecurityContext::default()
    }
}

/// Pod-level security context for an enforcement task-run Pod.
///
/// Sets `fsGroup = ARTIFACT_GID` (1000) with `fsGroupChangePolicy:
/// OnRootMismatch`, so the workspace/cache/mirror volumes are group-owned by the
/// artifact GID: the launcher-spawned child (primary group 1000) writes build
/// artifacts and the worker (uid 1000, also group 1000) can read them, with
/// setgid semantics preserving group ownership on new files.
///
/// DEPLOY RISK (documented per task qut0): the legacy pod ran as uid 10001 and
/// the VPS's large `/mirror` and `/cache` PVCs are currently owned by
/// 10001:10001. Switching to `fsGroup=1000` means the kubelet will, on first
/// mount where the volume ROOT's gid != 1000, recursively `chown`/setgid the
/// volume to group 1000. `OnRootMismatch` avoids re-chowning on every pod start
/// (it only acts when the top-level gid is wrong), but the FIRST pod after this
/// ships will pay a one-time recursive re-own of those huge cache volumes, which
/// can be slow and I/O-heavy. Coordinate the rollout: expect the first
/// task-run/warm pods post-deploy to start slowly while ownership converges, and
/// consider a one-shot maintenance `chgrp -R 1000` on `/mirror` and `/cache`
/// before the cutover so `OnRootMismatch` sees a matching root and skips the
/// recursive pass. See the matching note in `config.rs` and `job.rs`.
pub fn pod_security_context() -> PodSecurityContext {
    PodSecurityContext {
        fs_group: Some(i64::from(ARTIFACT_GID)),
        fs_group_change_policy: Some("OnRootMismatch".to_string()),
        ..PodSecurityContext::default()
    }
}

/// The worker↔launcher IPC volume (broker socket + worker-private credential).
pub fn launcher_ipc_volume() -> Volume {
    Volume {
        name: VOLUME_LAUNCHER_IPC.to_string(),
        empty_dir: Some(EmptyDirVolumeSource {
            // Memory-backed: the control socket + credential never touch disk.
            medium: Some("Memory".to_string()),
            size_limit: Some(Quantity("1Mi".to_string())),
        }),
        ..Volume::default()
    }
}

/// The launcher's private delegated cgroup root volume (launcher-only).
pub fn launcher_cgroup_volume() -> Volume {
    Volume {
        name: VOLUME_LAUNCHER_CGROUP.to_string(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Volume::default()
    }
}

/// Worker-side mount of the IPC volume so the worker can dial the broker socket
/// and read its private credential.
pub fn worker_launcher_ipc_mount() -> VolumeMount {
    VolumeMount {
        name: VOLUME_LAUNCHER_IPC.to_string(),
        mount_path: LAUNCHER_IPC_DIR.to_string(),
        ..VolumeMount::default()
    }
}

/// Env the worker needs to reach the broker.
pub fn worker_launcher_env() -> Vec<EnvVar> {
    vec![
        env_var("DJINN_LAUNCHER_SOCKET", LAUNCHER_SOCKET_PATH),
        env_var("DJINN_LAUNCHER_CREDENTIAL_PATH", LAUNCHER_CREDENTIAL_PATH),
    ]
}

/// Build the mandatory cgroup-launcher sidecar container.
///
/// Reuses `project_image_tag` (the SAME image the worker runs) with the packaged
/// launcher binary as its entrypoint — no fabricated image ref. It is a native
/// sidecar (`restartPolicy: Always` init container) so it is up before the
/// worker and torn down when the worker exits.
pub fn launcher_sidecar_container(config: &KubernetesConfig, project_image_tag: &str) -> Container {
    Container {
        name: LAUNCHER_CONTAINER_NAME.to_string(),
        image: Some(project_image_tag.to_string()),
        image_pull_policy: Some(config.image_pull_policy.clone()),
        // Native sidecar: init container that lives for the worker's lifetime.
        restart_policy: Some("Always".to_string()),
        command: Some(vec![LAUNCHER_BIN.to_string()]),
        args: Some(vec!["serve".to_string()]),
        env: Some(vec![
            env_var("DJINN_LAUNCHER_SOCKET", LAUNCHER_SOCKET_PATH),
            env_var("DJINN_LAUNCHER_CGROUP_ROOT", LAUNCHER_CGROUP_ROOT),
            env_var("DJINN_LAUNCHER_CREDENTIAL_PATH", LAUNCHER_CREDENTIAL_PATH),
            env_var("DJINN_LAUNCHER_EXPECTED_UID", &LAUNCHER_UID.to_string()),
            env_var(
                "DJINN_LAUNCHER_UNLEASED_MILLICORES",
                &LAUNCHER_UNLEASED_MILLICORES.to_string(),
            ),
        ]),
        volume_mounts: Some(vec![
            worker_launcher_ipc_mount(),
            VolumeMount {
                name: VOLUME_LAUNCHER_CGROUP.to_string(),
                mount_path: LAUNCHER_CGROUP_ROOT.to_string(),
                ..VolumeMount::default()
            },
        ]),
        security_context: Some(launcher_security_context()),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                (
                    "cpu".to_string(),
                    Quantity(LAUNCHER_CPU_REQUEST.to_string()),
                ),
                (
                    "memory".to_string(),
                    Quantity(LAUNCHER_MEMORY_REQUEST.to_string()),
                ),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(LAUNCHER_CPU_LIMIT.to_string())),
                (
                    "memory".to_string(),
                    Quantity(LAUNCHER_MEMORY_LIMIT.to_string()),
                ),
            ])),
            ..ResourceRequirements::default()
        }),
        ..Container::default()
    }
}

/// Fail-closed render/startup validation for an enforcement task-run.
///
/// Rejects unsupported cgroup-v2 delegation profiles, an out-of-bounds broker
/// quota, and incompatible volume-ownership modes BEFORE the Job is submitted —
/// i.e. before any user code can execute. The cgroup check is grounded in the
/// launcher crate's OWN [`Readiness::validate`]: the configured profile is
/// mapped onto the `Readiness` the launcher will re-validate at runtime, so the
/// render can never describe a delegation the launcher would reject after boot.
// No `PartialEq`/`Eq`: the `RejectedCgroupProfile { source }` variant wraps the
// launcher crate's `Error`, which is not comparable. Tests match on variants.
#[derive(Debug, Error)]
pub enum RenderValidationError {
    #[error("broker unleased quota {millicores}m is outside the launcher's accepted bounds")]
    UnsupportedBrokerQuota { millicores: u16 },
    #[error("unsupported cgroup delegation profile: {profile}")]
    UnsupportedCgroupProfile { profile: String },
    #[error(
        "configured cgroup profile {profile} is rejected by the launcher readiness contract: {source}"
    )]
    RejectedCgroupProfile {
        profile: String,
        source: LauncherError,
    },
    #[error("incompatible volume-ownership mode: {mode} (v1 requires {expected})")]
    IncompatibleVolumeOwnership {
        mode: String,
        expected: &'static str,
    },
}

/// Map a supported/unsupported cgroup-delegation profile string onto the
/// [`Readiness`] the launcher validates at runtime. `None` means the string is
/// not a recognized profile at all (→ [`RenderValidationError::UnsupportedCgroupProfile`]);
/// a recognized-but-non-conforming profile yields a `Readiness` that
/// [`Readiness::validate`] will REJECT (→ `RejectedCgroupProfile`), keeping the
/// render consistent with the crate's fail-closed runtime checks.
fn cgroup_profile_readiness(profile: &str) -> Option<Readiness> {
    let cpu_only = BTreeSet::from(["cpu".to_owned()]);
    match profile {
        CGROUP_PROFILE_V2_CPU_ONLY => Some(Readiness {
            mode: CgroupMode::V2,
            root_writable: true,
            owner_uid: LAUNCHER_UID as u32,
            delegated_controllers: cpu_only,
        }),
        // Recognized-but-unsupported delegations map to a Readiness the crate
        // rejects, so the SAME validation that runs in-pod runs at render time.
        "cgroup-v1" => Some(Readiness {
            mode: CgroupMode::V1,
            root_writable: true,
            owner_uid: LAUNCHER_UID as u32,
            delegated_controllers: BTreeSet::from(["cpu".to_owned()]),
        }),
        "cgroup-v2-hybrid" => Some(Readiness {
            mode: CgroupMode::Hybrid,
            root_writable: true,
            owner_uid: LAUNCHER_UID as u32,
            delegated_controllers: BTreeSet::from(["cpu".to_owned()]),
        }),
        "cgroup-v2-overbroad" => Some(Readiness {
            mode: CgroupMode::V2,
            root_writable: true,
            owner_uid: LAUNCHER_UID as u32,
            delegated_controllers: BTreeSet::from(["cpu".to_owned(), "memory".to_owned()]),
        }),
        "cgroup-v2-readonly" => Some(Readiness {
            mode: CgroupMode::V2,
            root_writable: false,
            owner_uid: LAUNCHER_UID as u32,
            delegated_controllers: BTreeSet::from(["cpu".to_owned()]),
        }),
        _ => None,
    }
}

/// Validate that the config describes an enforcement pod the launcher can
/// actually run. Call this at dispatch BEFORE building/submitting the Job.
pub fn validate_enforcement_render(config: &KubernetesConfig) -> Result<(), RenderValidationError> {
    // 1. Broker quota must satisfy the launcher crate's own bounds.
    UnleasedQuota::new(LAUNCHER_UNLEASED_MILLICORES).map_err(|_| {
        RenderValidationError::UnsupportedBrokerQuota {
            millicores: LAUNCHER_UNLEASED_MILLICORES,
        }
    })?;

    // 2. Cgroup delegation profile: recognized AND accepted by the launcher's
    //    runtime readiness contract.
    let readiness =
        cgroup_profile_readiness(&config.cgroup_delegation_profile).ok_or_else(|| {
            RenderValidationError::UnsupportedCgroupProfile {
                profile: config.cgroup_delegation_profile.clone(),
            }
        })?;
    readiness.validate(LAUNCHER_UID as u32).map_err(|source| {
        RenderValidationError::RejectedCgroupProfile {
            profile: config.cgroup_delegation_profile.clone(),
            source,
        }
    })?;

    // 3. Volume ownership must be the fsGroup-OnRootMismatch mode v1 renders.
    if config.volume_ownership_mode != VOLUME_OWNERSHIP_ON_ROOT_MISMATCH {
        return Err(RenderValidationError::IncompatibleVolumeOwnership {
            mode: config.volume_ownership_mode.clone(),
            expected: VOLUME_OWNERSHIP_ON_ROOT_MISMATCH,
        });
    }

    Ok(())
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
    fn every_role_kind_maps_to_a_resource_class() {
        // Light: orchestration-only roles.
        for role in [
            RoleKind::Planner,
            RoleKind::Reviewer,
            RoleKind::Lead,
            RoleKind::Refinement,
        ] {
            assert_eq!(
                RoleResourceClass::for_role(Some(role)),
                RoleResourceClass::Light,
                "{role:?} must be light"
            );
        }
        // Build-capable: roles that may compile.
        for role in [RoleKind::Worker, RoleKind::Verifier, RoleKind::Architect] {
            assert_eq!(
                RoleResourceClass::for_role(Some(role)),
                RoleResourceClass::BuildCapable,
                "{role:?} must be build-capable"
            );
        }
    }

    #[test]
    fn missing_role_fails_safe_to_build_capable() {
        assert_eq!(
            RoleResourceClass::for_role(None),
            RoleResourceClass::BuildCapable
        );
    }

    #[test]
    fn light_and_build_capable_share_limits_but_not_cpu_request() {
        let cfg = KubernetesConfig::for_testing();
        let light = worker_resources(&cfg, RoleResourceClass::Light);
        let build = worker_resources(&cfg, RoleResourceClass::BuildCapable);

        // CPU request differs by class.
        assert_eq!(
            light.requests.as_ref().unwrap().get("cpu").unwrap().0,
            cfg.light_cpu_request
        );
        assert_eq!(
            build.requests.as_ref().unwrap().get("cpu").unwrap().0,
            cfg.cpu_request
        );
        assert_ne!(
            light.requests.as_ref().unwrap().get("cpu"),
            build.requests.as_ref().unwrap().get("cpu"),
        );

        // Limits + memory identical across classes ("same limits everywhere").
        assert_eq!(light.limits, build.limits);
        assert_eq!(
            light.requests.as_ref().unwrap().get("memory"),
            build.requests.as_ref().unwrap().get("memory"),
        );
        assert_eq!(
            light.limits.as_ref().unwrap().get("cpu").unwrap().0,
            cfg.cpu_limit
        );
        assert_eq!(
            light.limits.as_ref().unwrap().get("memory").unwrap().0,
            cfg.memory_limit
        );
    }

    #[test]
    fn worker_and_launcher_have_distinct_uids_matching_the_launcher_contract() {
        let worker = worker_security_context();
        let launcher = launcher_security_context();
        assert_eq!(worker.run_as_user, Some(i64::from(WORKER_UID)));
        assert_eq!(worker.run_as_user, Some(1000));
        assert_eq!(launcher.run_as_user, Some(LAUNCHER_UID));
        assert_eq!(launcher.run_as_user, Some(0));
        assert_ne!(worker.run_as_user, launcher.run_as_user);
        // Child + artifact contract constants come straight from the crate.
        assert_eq!(CHILD_UID, 1001);
        assert_eq!(ARTIFACT_GID, 1000);
    }

    #[test]
    fn worker_security_context_is_restricted() {
        let sc = worker_security_context();
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(
            sc.capabilities.as_ref().unwrap().drop.as_deref(),
            Some(&["ALL".to_string()][..])
        );
        assert_eq!(sc.seccomp_profile.as_ref().unwrap().type_, "RuntimeDefault");
    }

    #[test]
    fn launcher_keeps_only_minimal_capabilities() {
        let caps = launcher_capabilities();
        assert_eq!(caps.drop.as_deref(), Some(&["ALL".to_string()][..]));
        let add = caps.add.expect("launcher adds minimal caps");
        assert_eq!(add, vec!["SETUID", "SETGID", "SETPCAP"]);
        // Never privileged, never SYS_ADMIN.
        assert!(!add.iter().any(|c| c == "SYS_ADMIN"));
        let sc = launcher_security_context();
        assert_eq!(sc.privileged, None);
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(sc.read_only_root_filesystem, Some(true));
    }

    #[test]
    fn pod_security_context_ties_fsgroup_to_artifact_gid_on_root_mismatch() {
        let sc = pod_security_context();
        assert_eq!(sc.fs_group, Some(i64::from(ARTIFACT_GID)));
        assert_eq!(sc.fs_group, Some(1000));
        assert_eq!(sc.fs_group_change_policy.as_deref(), Some("OnRootMismatch"));
        // The legacy pod-wide uid override is gone from the pod context.
        assert_eq!(sc.run_as_user, None);
    }

    #[test]
    fn launcher_sidecar_reuses_worker_image_with_launcher_entrypoint() {
        let cfg = KubernetesConfig::for_testing();
        let image = "registry.example/proj:tag";
        let c = launcher_sidecar_container(&cfg, image);
        assert_eq!(c.name, LAUNCHER_CONTAINER_NAME);
        // Same image as the worker, different entrypoint (real packaged binary).
        assert_eq!(c.image.as_deref(), Some(image));
        assert_eq!(c.command.as_deref(), Some(&[LAUNCHER_BIN.to_string()][..]));
        assert_eq!(c.restart_policy.as_deref(), Some("Always"));
        // Tiny fixed envelope.
        let req = c.resources.as_ref().unwrap().requests.as_ref().unwrap();
        assert_eq!(req.get("cpu").unwrap().0, LAUNCHER_CPU_REQUEST);
        assert_eq!(req.get("memory").unwrap().0, LAUNCHER_MEMORY_REQUEST);
        let lim = c.resources.as_ref().unwrap().limits.as_ref().unwrap();
        assert_eq!(lim.get("cpu").unwrap().0, LAUNCHER_CPU_LIMIT);
        assert_eq!(lim.get("memory").unwrap().0, LAUNCHER_MEMORY_LIMIT);
    }

    #[test]
    fn valid_config_passes_render_validation() {
        let cfg = KubernetesConfig::for_testing();
        assert!(validate_enforcement_render(&cfg).is_ok());
    }

    #[test]
    fn unknown_cgroup_profile_fails_closed() {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.cgroup_delegation_profile = "cgroup-v3-experimental".to_string();
        assert!(matches!(
            validate_enforcement_render(&cfg),
            Err(RenderValidationError::UnsupportedCgroupProfile { .. })
        ));
    }

    #[test]
    fn recognized_but_nonconforming_cgroup_profiles_are_rejected_by_the_launcher_contract() {
        for profile in [
            "cgroup-v1",
            "cgroup-v2-hybrid",
            "cgroup-v2-overbroad",
            "cgroup-v2-readonly",
        ] {
            let mut cfg = KubernetesConfig::for_testing();
            cfg.cgroup_delegation_profile = profile.to_string();
            assert!(
                matches!(
                    validate_enforcement_render(&cfg),
                    Err(RenderValidationError::RejectedCgroupProfile { .. })
                ),
                "{profile} must be rejected by the launcher readiness contract"
            );
        }
    }

    #[test]
    fn incompatible_volume_ownership_mode_fails_closed() {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.volume_ownership_mode = "chown-recursive-always".to_string();
        assert!(matches!(
            validate_enforcement_render(&cfg),
            Err(RenderValidationError::IncompatibleVolumeOwnership { .. })
        ));
    }
}
