//! Enforcement-pod rendering: role-classed resources, the (mode-gated)
//! cgroup-launcher sidecar, the v1 worker/child/launcher security contract, and
//! the fail-closed render/startup validation seam.
//!
//! The launcher sidecar is rendered by the production default
//! ([`CgroupLauncherMode::Required]). An explicit disabled mode remains for
//! local/development profiles. The armed path is real and
//! measured — the launcher establishes its own delegated cgroup v2 root, throttles
//! an unleased invocation to [`LAUNCHER_UNLEASED_MILLICORES`], and lifts it to the
//! pod's declared CPU budget on a fenced lease. `djinn-cgroup-launcher`'s
//! `tests/delegated_cpu_lease_lifecycle.rs` proves that end to end on a real
//! kernel by measuring `cpu.stat`, not by reading `cpu.max` back.
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
use serde::{Deserialize, Serialize};
use thiserror::Error;

// Re-exported so every pod render that shares the cache volumes (task-run in
// `job.rs`, warm in `warm_job.rs`) pins the SAME worker identity from one place.
pub use djinn_cgroup_launcher::broker::{WORKER_GID, WORKER_UID};
// Re-export the child/artifact contract constants so djinn-k8s consumers (and
// intra-doc links here) see the same UIDs the launcher enforces at runtime.
// `CHILD_UID` is applied by the launcher when it spawns the child, not in the
// pod manifest, so it is only referenced by tests/docs on the render side.
pub use djinn_cgroup_launcher::child::{ARTIFACT_GID, CHILD_UID};
// The capability lists the runtime actually `capset`s to. The render derives the
// container's `capabilities.add` from these so the manifest cannot grant a
// capability the runtime discards, nor omit one the runtime needs.
use djinn_cgroup_launcher::bootstrap::RETAINED_CAPABILITY_NAMES;
use djinn_cgroup_launcher::{
    CgroupMode, Error as LauncherError, LauncherAuthorityProtocol, LeasedQuota, Readiness,
    UnleasedQuota,
};
pub use djinn_runtime::RoleResourceClass;

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

/// Launcher sidecar CPU **request**: the broker's own steady footprint, which is
/// genuinely tiny — it brokers, it does not compute.
///
/// This stays small on purpose even though every launched command now runs in
/// this container's cgroup. A CPU request is a `cpu.weight`, and `cpu.weight` is
/// a *floor* under contention, not a ceiling: a container only loses share to a
/// sibling that is continuously runnable, and the worker is not — while a build
/// runs the worker is streaming LLM tokens and reading the broker socket in
/// short bursts, so the launcher receives everything the worker does not use.
/// Node-level fairness is set by the pod's total request, which is unchanged.
pub const LAUNCHER_CPU_REQUEST: &str = "50m";
/// [`LAUNCHER_CPU_REQUEST`] in millicores, for the numeric comparison in
/// [`RenderValidationError::LauncherCpuCeilingBelowRequest`]. The two are
/// asserted to agree by `launcher_tests::the_launcher_cpu_request_constants_agree`,
/// so this is a spelling of the same value, not a second source of truth.
pub const LAUNCHER_CPU_REQUEST_MILLICORES: u32 = 50;
/// Launcher sidecar memory **request**: also the broker's steady footprint.
/// The build's peak lives in the limit, not the request — the same
/// request-is-steady/limit-is-peak shape the worker container already uses.
pub const LAUNCHER_MEMORY_REQUEST: &str = "64Mi";

// NOTE (task 7deu, defect 1): there is deliberately NO unconditional
// `LAUNCHER_CPU_LIMIT`, and under `leaf-v1` there is no launcher CPU limit at
// all. Task 4wx3 added one for `resize-v2` ONLY; the history below is why that
// conditionality is the whole point.
//
// This constant used to be "250m", matching goxi's normative resource matrix.
// It was the single reason the whole feature was a no-op. Under `nsdelegate` the
// delegated cgroup root IS the launcher container's own cgroup, whose `cpu.max`
// the kubelet had already set from that limit — so a bigger quota written on an
// invocation leaf changed nothing, the ancestor still clamped. Measured with the
// leaf set to a full 4 cores: two spinners over a 5s wall window should burn 10s
// of CPU; `cpu.stat` reported `usage_usec 1252296`, i.e. 1.25s — exactly 0.25
// core. Worse, `nr_throttled` read 0 in the leaf because the throttling happened
// at the parent, which made goxi's throttle-based heavy detection structurally
// blind. With the limit removed, the same pod reported `nr_throttled 40/40` on
// the unleased leaf and 1.995 cores of measured post-lift throughput.
//
// Under `leaf-v1` the ceiling did not disappear, it moved to where the work is:
// the invocation leaf's own `cpu.max`, unleased at
// [`LAUNCHER_UNLEASED_MILLICORES`] and lifted to [`launcher_leased_millicores`].
// Removing a container CPU limit also makes the kubelet leave the POD cgroup's
// `cpu.max` unset, which is required — a pod ceiling would reintroduce exactly
// the ancestor clamp this removes.
//
// Under `resize-v2` the arithmetic inverts, because
// [`LauncherAuthorityProtocol::launcher_owns_leaf_quota`] is FALSE: the launcher
// never writes a leaf `cpu.max`, not even `max`. There is therefore no leaf
// ceiling for a container limit to clamp *below* — the container limit IS the
// ceiling, and pod resize is what moves it. Rendering it unconditionally would
// reinstate the measurement above verbatim, which is why
// [`apply_launcher_cpu_ceiling`] gates on the protocol and why the leaf-v1 arm
// of `launcher_tests::the_launcher_cpu_ceiling_is_rendered_only_under_resize_v2`
// asserts the ABSENCE of the key rather than a value.

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

/// Kubelet-delegated writable cgroup root supplied by the RuntimeClass.
pub const LAUNCHER_CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// RuntimeClass that delegates the writable cgroup hierarchy to task-run Pods.
pub const TASK_RUN_CGROUP_RUNTIME_CLASS: &str = "djinn-cgroup-writable";

/// Environment variable carrying the quota-authority protocol into the pod.
///
/// Read by the launcher sidecar's `main.rs`
/// (`djinn-cgroup-launcher::AUTHORITY_PROTOCOL_ENV`) and by the worker's
/// `launcher_handshake`, which asserts it to the broker at READY. The two
/// containers run the same image, so setting it on one and not the other is the
/// one disagreement that can actually occur — which is why
/// [`apply_launcher_authority_protocol`] writes both, and why the broker refuses
/// a READY that disagrees.
///
/// Before this constant existed, `DJINN_LAUNCHER_AUTHORITY_PROTOCOL` was read by
/// the shipped binary (#2823) and emitted by **nothing**: every production
/// launcher fell through to `leaf-v1` no matter what the image declared.
pub const AUTHORITY_PROTOCOL_ENV: &str = "DJINN_LAUNCHER_AUTHORITY_PROTOCOL";

/// Whether an enforcement task-run Pod renders the cgroup-launcher sidecar.
///
/// The armed path relies on `runtimeClassName: djinn-cgroup-writable`: kubelet
/// delegates the pod's writable `/sys/fs/cgroup` hierarchy. The launcher vacates
/// that root into `init/`, enables only `cpu`, and creates per-invocation leaves
/// for the CPU lease. It never mounts cgroup2 and the PodSpec contains neither a
/// cgroup volume nor a hostPath. The sidecar retains only identity/socket
/// capabilities, `RuntimeDefault` seccomp, and the default AppArmor profile.
///
/// Production defaults to [`Required`](CgroupLauncherMode::Required). An
/// operator may set `DJINN_K8S_CGROUP_LAUNCHER_MODE=disabled` only for a local
/// or development profile that deliberately uses direct worker execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CgroupLauncherMode {
    /// No sidecar, no launcher volumes, no launcher IPC env. Shell commands run
    /// in-process in the worker, unleased. This is for explicit local/development
    /// profiles only.
    Disabled,
    /// Arm the sidecar: RuntimeClass delegates its cgroup root and every shell
    /// command runs in a per-invocation leaf under a CPU lease.
    #[default]
    Required,
}

impl CgroupLauncherMode {
    /// Stable config/telemetry string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Required => "required",
        }
    }

    /// Parse the `DJINN_K8S_CGROUP_LAUNCHER_MODE` value. Unknown values are
    /// rejected rather than silently defaulting, so a typo can neither arm nor
    /// disarm enforcement by accident.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "disabled" => Some(Self::Disabled),
            "required" => Some(Self::Required),
            _ => None,
        }
    }

    /// Does this mode render the sidecar and its volumes?
    pub fn renders_sidecar(&self) -> bool {
        matches!(self, Self::Required)
    }
}

/// The CPU request quantity string for `class`, sourced from the
/// (env-overridable) config.
///
/// The classifier itself is [`RoleResourceClass`], which lives in
/// `djinn-runtime` so `djinn-coordinator`'s build-admission cap and this crate's
/// pod sizing can never disagree about which roles compile. Only the mapping
/// onto *this* crate's config lives here.
pub fn class_cpu_request(class: RoleResourceClass, config: &KubernetesConfig) -> &str {
    match class {
        RoleResourceClass::Light => &config.light_cpu_request,
        RoleResourceClass::BuildCapable => &config.cpu_request,
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
                Quantity(class_cpu_request(class, config).to_string()),
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

/// Capability set required for the launcher's broker and child identity boundary.
/// The kubelet RuntimeClass delegates the cgroup root, so no mount or bootstrap
/// capability is granted.
fn launcher_capabilities() -> Capabilities {
    Capabilities {
        drop: Some(vec!["ALL".to_string()]),
        add: Some(
            RETAINED_CAPABILITY_NAMES
                .iter()
                .map(|capability| (*capability).to_string())
                .collect(),
        ),
    }
}

/// Container-level security context for the launcher sidecar. The delegated
/// cgroup root is supplied by the RuntimeClass; the sidecar keeps only the
/// identity capabilities required to broker children.
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

/// Whether the Pod runs in a user namespace. Always [`None`]: a user namespace
/// can leave the kubelet-delegated cgroup owned by an unmapped uid, preventing
/// the launcher from creating its holding leaf.
pub fn pod_host_users(_mode: CgroupLauncherMode) -> Option<bool> {
    None
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
///
/// `mirror_read_only`/`cache_read_only` mirror the worker's own flags for the
/// same run: a brokered command runs in THIS container's mount namespace, so
/// the two must agree or the isolation contract holds only for commands that
/// happen not to be brokered.
pub fn launcher_sidecar_container(
    config: &KubernetesConfig,
    project_image_tag: &str,
    mirror_read_only: bool,
    cache_read_only: bool,
) -> Container {
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
            // The quota a granted lease lifts an invocation leaf to, and the
            // number the launcher derives the child's `CARGO_BUILD_JOBS` /
            // `NEXTEST_TEST_THREADS` / `MAKEFLAGS` / `GOMAXPROCS` pins from.
            // Sourced from the pod's own declared CPU limit so a brokered build
            // picks exactly the parallelism an unbrokered one would.
            env_var(
                LEASED_MILLICORES_ENV,
                &launcher_leased_millicores(config).to_string(),
            ),
            // The worker-written, launcher-read-only git config the trust anchor
            // `[include]`s, so a brokered cargo/go/pnpm fetch of a private
            // dependency carries the installation token. Named by the render
            // rather than compiled in, exactly like the journal directory; see
            // `crate::private_dep_config` (goxi, ninth launcher blocker).
            crate::private_dep_config::child_git_config_env(),
        ]),
        // IPC plus the data mounts a brokered child needs; the RuntimeClass
        // provides `/sys/fs/cgroup` directly, without a Pod volume.
        volume_mounts: Some(crate::launcher_child_fs::launcher_volume_mounts(
            mirror_read_only,
            cache_read_only,
        )),
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
            // NO CPU LIMIT HERE — see the note where `LAUNCHER_CPU_LIMIT` used
            // to be. Under leaf-v1 a limit here becomes an ancestor clamp on
            // every invocation leaf and silently caps every build at the
            // launcher's own quota.
            //
            // This builder is a pure function of the config and cannot know the
            // authority protocol, which is a property of the resolved catalog
            // image. The resize-v2 ceiling is therefore applied at the same
            // post-render seam that writes [`AUTHORITY_PROTOCOL_ENV`] — see
            // [`apply_launcher_cpu_ceiling`]. leaf-v1 keeps this shape exactly.
            //
            // The MEMORY limit is the worker's, not a sidecar's: when the
            // launcher is armed every command runs in this container's cgroup,
            // so the build's memory peak lands here. The old 128Mi would have
            // OOM-killed the first `cargo build`. Memory has no equivalent of the
            // CPU problem — a memory limit is a ceiling, not a rate, and the
            // build needs a real one.
            limits: Some(BTreeMap::from([(
                "memory".to_string(),
                Quantity(config.memory_limit.clone()),
            )])),
            ..ResourceRequirements::default()
        }),
        ..Container::default()
    }
}

// Rendered CPU quantities live beside this module and are re-exported here,
// so `crate::launcher::*` remains the single import path.
pub use crate::launcher_cpu::{
    LEASED_MILLICORES_ENV, launcher_leased_millicores, retune_launcher_lease, warm_job_millicores,
};
use crate::launcher_cpu::{parse_cpu_millicores, rendered_lease_millicores};

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
    #[error(
        "cgroup launcher mode is `required`, but the launcher container this build renders cannot \
         establish a delegated cgroup v2 subtree: {reason}. Arming enforcement with this render \
         would submit a Pod whose sidecar fails startup readiness, so it is refused before the Job \
         exists. Correct the rendered runtime profile; `disabled` is reserved for explicit local/\
         development compatibility. See djinn_k8s::launcher::CgroupLauncherMode."
    )]
    ArmedRenderCannotDelegate { reason: &'static str },
    #[error(
        "cgroup launcher mode is `required`, but the pod CPU limit {limit} does not map onto a \
         usable lease quota ({min}m..={max}m). The lease is what bounds a lifted build now that \
         the launcher container carries no CPU limit; without a valid one a single build would be \
         bounded only by the node."
    )]
    UnsupportedLeaseQuota { limit: String, min: u32, max: u32 },
    #[error("cgroup launcher mode is `required`, but task-run RuntimeClass assignment is disabled")]
    MissingDelegatedRuntimeClass,
    #[error(
        "the resolved image declares no launcher authority protocol and carries no immutable \
         registry digest, so the server cannot know which component owns invocation CPU quota in \
         it. Rebuild the catalog image: builds since the protocol declaration landed record both. \
         Refused before the Job exists rather than dispatching under a guess."
    )]
    UndeclarableAuthorityProtocol,
    #[error(
        "cgroup launcher mode is `required`, but the rendered Job has no `{container}` container \
         to carry {env}. The launcher would start with no protocol selection and silently fall \
         back to leaf-v1, which is the exact silence this render exists to remove."
    )]
    MissingLauncherSidecar {
        container: &'static str,
        env: &'static str,
    },
    #[error(
        "the resolved authority protocol is {protocol}, but the rendered `{container}` sidecar \
         carries no usable {env} value (found: {found}). Under resize-v2 the launcher never writes \
         a leaf `cpu.max`, so the sidecar's own CPU limit is the ONLY ceiling a brokered build has \
         and there is nothing left to derive it from. Refused rather than rendering a sidecar \
         whose builds are bounded only by the node."
    )]
    UnresolvableLauncherCpuCeiling {
        protocol: LauncherAuthorityProtocol,
        container: &'static str,
        env: &'static str,
        found: String,
    },
    #[error(
        "the resize-v2 launcher CPU ceiling {ceiling}m is below the sidecar's own CPU request \
         {request}. Kubernetes rejects a container whose limit is under its request, so this \
         renders a Pod that never admits at all rather than a build that merely runs slowly — \
         refused here, while the number is still a number."
    )]
    LauncherCpuCeilingBelowRequest { ceiling: u32, request: &'static str },
}

/// Decide the protocol a Job must render, from what the resolved catalog image
/// declared and whether it is digest-pinned.
///
/// * **Declared** → that protocol. Migration 166 guarantees a declaring image is
///   also digest-pinned, so the value names a specific artifact.
/// * **Undeclared but digest-pinned** → [`LauncherAuthorityProtocol::LeafV1`].
///   These are the images built before the declaration existed. `leaf-v1` is
///   what they have always run, and the digest is what makes "these bytes"
///   meaningful — so the render states it explicitly rather than relying on the
///   launcher's absent-variable default.
/// * **Neither** → refused. Nothing identifies the artifact and nothing states
///   its quota authority; the alternative is dispatching on a guess about who
///   writes `cpu.max`, in a system where guessing wrong is either an unbounded
///   build or a 16x-throttled one.
///
/// A per-deployment allowlist for undeclared, undigested images is task vf7a's
/// job and deliberately not here: an allowlist that arrives with the fence it is
/// supposed to open is not a fence.
pub fn render_authority_protocol(
    declared: Option<LauncherAuthorityProtocol>,
    digest: Option<&str>,
) -> Result<LauncherAuthorityProtocol, RenderValidationError> {
    if let Some(declared) = declared {
        return Ok(declared);
    }
    match digest.map(str::trim).filter(|digest| !digest.is_empty()) {
        Some(_) => Ok(LauncherAuthorityProtocol::LeafV1),
        None => Err(RenderValidationError::UndeclarableAuthorityProtocol),
    }
}

/// Write [`AUTHORITY_PROTOCOL_ENV`] onto the rendered Job's launcher sidecar and
/// worker containers.
///
/// This runs after `build_task_run_job` rather than inside it because the
/// protocol is a property of the resolved *catalog image*, which only the
/// dispatch path has (`djinn_k8s::runtime`), while the Job builder is a pure
/// function of the config — the same split `build_resources::apply_resolved_resources`
/// already uses for per-project resource overrides.
///
/// It also applies the protocol-conditional launcher CPU ceiling
/// ([`apply_launcher_cpu_ceiling`]), because that is a second thing the
/// resolved protocol — and only the resolved protocol — decides about the
/// sidecar. Keeping the two in one function means a render can never carry the
/// `resize-v2` env with a `leaf-v1` resource shape, or the reverse.
///
/// Fail-closed: in `required` mode a Job with no `cgroup-launcher` container is
/// an error, not a silent skip. Under `disabled` mode there is no sidecar and no
/// broker, so there is nothing to agree about and this is a no-op.
pub fn apply_launcher_authority_protocol(
    job: &mut k8s_openapi::api::batch::v1::Job,
    mode: CgroupLauncherMode,
    protocol: LauncherAuthorityProtocol,
) -> Result<(), RenderValidationError> {
    if !mode.renders_sidecar() {
        return Ok(());
    }
    let missing = || RenderValidationError::MissingLauncherSidecar {
        container: LAUNCHER_CONTAINER_NAME,
        env: AUTHORITY_PROTOCOL_ENV,
    };
    let pod = job
        .spec
        .as_mut()
        .and_then(|spec| spec.template.spec.as_mut())
        .ok_or_else(missing)?;

    let sidecar = pod
        .init_containers
        .as_mut()
        .and_then(|containers| {
            containers
                .iter_mut()
                .find(|container| container.name == LAUNCHER_CONTAINER_NAME)
        })
        .ok_or_else(missing)?;
    // Resolve the ceiling BEFORE mutating anything, so a refusal leaves the
    // caller's Job exactly as it found it.
    let ceiling = resolve_launcher_cpu_ceiling(sidecar, protocol)?;
    set_env(sidecar, AUTHORITY_PROTOCOL_ENV, protocol.as_wire());
    apply_launcher_cpu_ceiling(sidecar, ceiling);

    // The worker asserts the SAME value to the broker at READY. Setting it on
    // the sidecar alone would make every armed pod fail its own handshake as
    // soon as a resize-v2 image exists.
    for worker in pod
        .containers
        .iter_mut()
        .filter(|container| container.name == "worker")
    {
        set_env(worker, AUTHORITY_PROTOCOL_ENV, protocol.as_wire());
    }
    Ok(())
}

/// Set (or replace) one environment entry on a container, so re-applying is
/// idempotent rather than appending a second, shadowed definition.
fn set_env(container: &mut Container, name: &str, value: &str) {
    let env = container.env.get_or_insert_with(Vec::new);
    match env.iter_mut().find(|entry| entry.name == name) {
        Some(existing) => {
            existing.value = Some(value.to_string());
            existing.value_from = None;
        }
        None => env.push(env_var(name, value)),
    }
}

/// The launcher sidecar's `limits.cpu`, in millicores, for `protocol` — or
/// [`None`] when the protocol must render no CPU limit at all.
///
/// * **leaf-v1 → `None`.** The launcher owns the leaf's `cpu.max`
///   ([`LauncherAuthorityProtocol::launcher_owns_leaf_quota`]), so a container
///   limit here is an ancestor clamp that silently caps every build at the
///   launcher's own quota. That is task 7deu's defect 1, measured; see the note
///   where `LAUNCHER_CPU_LIMIT` used to be. This arm exists so that regression
///   cannot return through the resize-v2 door.
/// * **resize-v2 → the lease ceiling.** The launcher writes no leaf quota under
///   this protocol, so nothing else bounds a brokered build; pod resize moves
///   this limit, and this limit is what it moves.
///
/// The value is read back off the rendered sidecar rather than recomputed from
/// the config, because `build_resources::apply_resolved_resources` runs first
/// and may have re-pointed the lease at a per-project `cpu_limit` override — see
/// [`crate::launcher_cpu::rendered_lease_millicores`].
fn resolve_launcher_cpu_ceiling(
    sidecar: &Container,
    protocol: LauncherAuthorityProtocol,
) -> Result<Option<u32>, RenderValidationError> {
    if protocol.launcher_owns_leaf_quota() {
        return Ok(None);
    }
    let ceiling = rendered_lease_millicores(sidecar).map_err(|found| {
        RenderValidationError::UnresolvableLauncherCpuCeiling {
            protocol,
            container: LAUNCHER_CONTAINER_NAME,
            env: LEASED_MILLICORES_ENV,
            found,
        }
    })?;
    // A limit under the container's own request is not a slow pod, it is a Pod
    // the apiserver rejects. Catch it here, where the number is still a number
    // and the failure names the ceiling.
    if ceiling < LAUNCHER_CPU_REQUEST_MILLICORES {
        return Err(RenderValidationError::LauncherCpuCeilingBelowRequest {
            ceiling,
            request: LAUNCHER_CPU_REQUEST,
        });
    }
    Ok(Some(ceiling))
}

/// Write (or clear) the launcher sidecar's `limits.cpu`.
///
/// Clearing on the [`None`] arm keeps re-application total rather than additive:
/// applying leaf-v1 over a previously resize-v2 render must leave no stale
/// clamp behind. The clear never *creates* an empty `resources`/`limits` map, so
/// the leaf-v1 manifest is byte-identical to one this function never touched.
/// `requests` and `limits.memory` are not addressed on either arm.
fn apply_launcher_cpu_ceiling(sidecar: &mut Container, ceiling: Option<u32>) {
    match ceiling {
        Some(millicores) => {
            sidecar
                .resources
                .get_or_insert_with(ResourceRequirements::default)
                .limits
                .get_or_insert_with(BTreeMap::new)
                .insert("cpu".to_string(), Quantity(format!("{millicores}m")));
        }
        None => {
            if let Some(limits) = sidecar
                .resources
                .as_mut()
                .and_then(|resources| resources.limits.as_mut())
            {
                limits.remove("cpu");
            }
        }
    }
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

    // 4. An armed launcher requires the RuntimeClass assignment. The builder
    // writes its exact constant name, while this dispatch-time guard rejects the
    // invalid armed-without-assignment cross-product before Job submission.
    if config.cgroup_launcher_mode.renders_sidecar() {
        if !config.task_run_cgroup_writable_enabled {
            return Err(RenderValidationError::MissingDelegatedRuntimeClass);
        }
        // RuntimeClass assignment is the atomic privilege boundary. The exact
        // class name is written by `build_task_run_job`; no capability or LSM
        // bootstrap contract is valid for an armed launcher.
        // 5. The lease must map onto a quota the launcher crate accepts, or a
        //    lifted build has no ceiling at all.
        if parse_cpu_millicores(&config.cpu_limit).is_none() {
            return Err(RenderValidationError::UnsupportedLeaseQuota {
                limit: config.cpu_limit.clone(),
                min: LeasedQuota::MIN_MILLICORES,
                max: LeasedQuota::MAX_MILLICORES,
            });
        }
        LeasedQuota::new(launcher_leased_millicores(config)).map_err(|_| {
            RenderValidationError::UnsupportedLeaseQuota {
                limit: config.cpu_limit.clone(),
                min: LeasedQuota::MIN_MILLICORES,
                max: LeasedQuota::MAX_MILLICORES,
            }
        })?;
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
#[path = "launcher_tests.rs"]
mod tests;
