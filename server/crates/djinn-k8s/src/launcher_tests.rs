//! Unit tests for [`crate::launcher`]: the rendered v1 security contract, the
//! role-classed resource envelope, the launcher's residual capability set, and
//! the fail-closed render validation.
//!
//! Split out of `launcher.rs` to keep that file inside the repository's per-file
//! size guard. It is `#[path]`-included as a private child module, so it still
//! sees the module's private items (`launcher_capabilities`,
//! `parse_cpu_millicores`) exactly as an inline `mod tests` would.

use super::*;

#[test]
fn launcher_sidecar_declares_cpu_resize_without_restart() {
    let launcher = launcher_sidecar_container(
        &KubernetesConfig::for_testing(),
        "registry.example/djinn:test",
        false,
        false,
    );
    let policies = launcher.resize_policy.expect("explicit resize policy");
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0].resource_name, "cpu");
    assert_eq!(policies[0].restart_policy, "NotRequired");
}

use std::collections::BTreeMap;

use djinn_cgroup_launcher::bootstrap::RETAINED_CAPABILITY_NAMES;
use djinn_runtime::RoleKind;
use uuid::Uuid;

/// The classifier itself is proven exhaustively in `djinn-runtime` (one
/// home, shared with the coordinator's build-admission cap). What this crate
/// owns is the mapping onto ITS config, so that is what is asserted here.
#[test]
fn the_resource_class_maps_onto_this_crates_cpu_requests() {
    let cfg = KubernetesConfig::for_testing();
    assert_eq!(
        class_cpu_request(RoleResourceClass::Light, &cfg),
        cfg.light_cpu_request
    );
    assert_eq!(
        class_cpu_request(RoleResourceClass::BuildCapable, &cfg),
        cfg.cpu_request
    );
    // The re-export is the same type the coordinator admits against; a
    // second local copy is exactly the drift this move exists to prevent.
    assert_eq!(
        RoleResourceClass::for_role(Some(RoleKind::Planner)),
        RoleResourceClass::Light
    );
    assert_eq!(
        RoleResourceClass::for_role(Some(RoleKind::Worker)),
        RoleResourceClass::BuildCapable
    );
}

#[test]
fn missing_role_fails_safe_to_build_capable() {
    assert_eq!(
        RoleResourceClass::for_role(None),
        RoleResourceClass::BuildCapable
    );
}

#[test]
fn worker_resources_role_classing() {
    let cfg = KubernetesConfig::for_testing();
    let light = worker_resources(&cfg, RoleResourceClass::Light);
    let build_capable = worker_resources(&cfg, RoleResourceClass::BuildCapable);

    let light_request = parse_cpu_millicores(
        &light
            .requests
            .as_ref()
            .expect("Light worker resources must contain requests")
            .get("cpu")
            .expect("Light worker resources requests must contain cpu")
            .0,
    )
    .expect("Light worker resources requests.cpu must be a parseable CPU quantity");
    let build_capable_request = parse_cpu_millicores(
        &build_capable
            .requests
            .as_ref()
            .expect("BuildCapable worker resources must contain requests")
            .get("cpu")
            .expect("BuildCapable worker resources requests must contain cpu")
            .0,
    )
    .expect("BuildCapable worker resources requests.cpu must be a parseable CPU quantity");
    let light_limit = parse_cpu_millicores(
        &light
            .limits
            .as_ref()
            .expect("Light worker resources must contain limits")
            .get("cpu")
            .expect("Light worker resources limits must contain cpu")
            .0,
    )
    .expect("Light worker resources limits.cpu must be a parseable CPU quantity");
    let build_capable_limit = parse_cpu_millicores(
        &build_capable
            .limits
            .as_ref()
            .expect("BuildCapable worker resources must contain limits")
            .get("cpu")
            .expect("BuildCapable worker resources limits must contain cpu")
            .0,
    )
    .expect("BuildCapable worker resources limits.cpu must be a parseable CPU quantity");

    assert_eq!(
        light_limit, build_capable_limit,
        "Light limits.cpu parsed as {light_limit}m; BuildCapable limits.cpu parsed as {build_capable_limit}m"
    );
    assert!(
        light_request < build_capable_request,
        "Light requests.cpu parsed as {light_request}m; BuildCapable requests.cpu parsed as {build_capable_request}m"
    );

    // Memory stays shared across the role classes.
    assert_eq!(
        light.requests.as_ref().unwrap().get("memory"),
        build_capable.requests.as_ref().unwrap().get("memory"),
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

/// The launcher is granted exactly the residual identity/socket capabilities
/// it needs, spelled the way the API server accepts, and is never `privileged`.
#[test]
fn launcher_capabilities_are_exactly_the_designed_set() {
    let caps = launcher_capabilities();
    assert_eq!(caps.drop.as_deref(), Some(&["ALL".to_string()][..]));
    let add = caps.add.expect("launcher adds capabilities");
    assert_eq!(add, vec!["CHOWN", "SETGID", "SETUID", "SETPCAP"]);
    // `privileged: true` would grant everything; it must never appear.
    let sc = launcher_security_context();
    assert_eq!(sc.privileged, None);
    assert_eq!(sc.read_only_root_filesystem, Some(true));
}

/// Kubernetes capability names use the API's bare spelling. A prefixed name
/// alongside `allowPrivilegeEscalation: false` is rejected at Job submission.
#[test]
fn capabilities_use_the_api_accepted_spelling_alongside_no_privilege_escalation() {
    let sc = launcher_security_context();
    assert_eq!(sc.allow_privilege_escalation, Some(false));
    for capability in sc.capabilities.unwrap().add.unwrap() {
        assert!(
            !capability.starts_with("CAP_"),
            "{capability} uses the CAP_-prefixed spelling the API server rejects \
             alongside allowPrivilegeEscalation: false"
        );
    }
}

/// The render grants exactly the identity/socket capabilities the launcher uses.
#[test]
fn the_rendered_grant_is_exactly_the_runtime_residual_set() {
    let cfg = KubernetesConfig::for_testing();
    let container = launcher_sidecar_container(&cfg, "registry.example/proj:tag", false, false);
    let add = container
        .security_context
        .unwrap()
        .capabilities
        .unwrap()
        .add
        .unwrap();
    let expected: Vec<String> = RETAINED_CAPABILITY_NAMES
        .iter()
        .map(|capability| (*capability).to_string())
        .collect();
    assert_eq!(add, expected);
}

/// Defect 1, asserted on the manifest: a CPU limit on the launcher container
/// is an ancestor clamp on every invocation leaf. Measured at 0.25 core
/// against a leaf set to four, with `nr_throttled` reading 0 in the leaf
/// because the throttling happened at the parent.
#[test]
fn the_launcher_container_declares_no_cpu_limit() {
    let cfg = KubernetesConfig::for_testing();
    let c = launcher_sidecar_container(&cfg, "registry.example/proj:tag", false, false);
    let limits = c.resources.unwrap().limits.unwrap();
    assert!(
        !limits.contains_key("cpu"),
        "a CPU limit here silently caps every build at it, whatever the leaf says: {limits:?}"
    );
    // The memory limit is the pod's build budget, not a sidecar's: every
    // command now runs in this container's cgroup, so the build's memory
    // peak lands here. The old 128Mi would OOM-kill the first cargo build.
    assert_eq!(limits.get("memory").unwrap().0, cfg.memory_limit);
}

/// The lease ceiling comes from the pod's own declared CPU limit, so the
/// bound a build runs under is the bound the manifest advertises.
#[test]
fn the_lease_quota_is_derived_from_the_declared_pod_cpu_limit() {
    let mut cfg = KubernetesConfig::for_testing();
    assert_eq!(cfg.cpu_limit, "4");
    assert_eq!(launcher_leased_millicores(&cfg), 4_000);
    cfg.cpu_limit = "12000m".to_string();
    assert_eq!(launcher_leased_millicores(&cfg), 12_000);
    // Out of range or unparseable falls back to the crate default rather
    // than to anything unbounded.
    cfg.cpu_limit = "not-a-quantity".to_string();
    assert_eq!(
        launcher_leased_millicores(&cfg),
        LeasedQuota::DEFAULT_MILLICORES
    );
    cfg.cpu_limit = "500m".to_string();
    assert_eq!(
        launcher_leased_millicores(&cfg),
        LeasedQuota::DEFAULT_MILLICORES
    );
}

/// A user namespace is never requested, armed or not.
///
/// `hostUsers: false` leaves the launcher's own cgroup owned by a uid that is
/// unmapped inside the namespace, so the `init` holding leaf cannot be created
/// and the `+cpu` delegation never happens — measured on the production node,
/// see [`pod_host_users`]. This is the guard against restoring it for the
/// confinement argument, which is sound but unimplementable here.
#[test]
fn user_namespaces_are_never_requested_because_they_break_the_delegation() {
    assert_eq!(pod_host_users(CgroupLauncherMode::Disabled), None);
    assert_eq!(pod_host_users(CgroupLauncherMode::Required), None);
}

/// The RuntimeClass supplies the writable cgroup hierarchy; a rendered Pod must
/// not attempt to replace it with an emptyDir or hostPath mount.
#[test]
fn launcher_has_no_rendered_cgroup_mount() {
    let cfg = KubernetesConfig::for_testing();
    let container = launcher_sidecar_container(&cfg, "registry.example/proj:tag", false, false);
    assert!(
        container
            .volume_mounts
            .iter()
            .flatten()
            .all(|mount| mount.mount_path != LAUNCHER_CGROUP_ROOT)
    );
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
    let c = launcher_sidecar_container(&cfg, image, false, false);
    assert_eq!(c.name, LAUNCHER_CONTAINER_NAME);
    // Same image as the worker, different entrypoint (real packaged binary).
    assert_eq!(c.image.as_deref(), Some(image));
    assert_eq!(c.command.as_deref(), Some(&[LAUNCHER_BIN.to_string()][..]));
    assert_eq!(c.restart_policy.as_deref(), Some("Always"));
    // Requests stay the broker's steady footprint (see the constants);
    // limits are asserted by `the_launcher_container_declares_no_cpu_limit`.
    let req = c.resources.as_ref().unwrap().requests.as_ref().unwrap();
    assert_eq!(req.get("cpu").unwrap().0, LAUNCHER_CPU_REQUEST);
    assert_eq!(req.get("memory").unwrap().0, LAUNCHER_MEMORY_REQUEST);
    // The lease ceiling reaches the launcher as env, or it would fall back
    // to a default that has nothing to do with this pod's budget.
    let env = c.env.as_ref().unwrap();
    let value = |name: &str| {
        env.iter()
            .find(|var| var.name == name)
            .and_then(|var| var.value.clone())
            .unwrap_or_else(|| panic!("{name} must be rendered on the launcher"))
    };
    assert_eq!(value("DJINN_LAUNCHER_UNLEASED_MILLICORES"), "250");
    assert_eq!(value("DJINN_LAUNCHER_LEASED_MILLICORES"), "4000");
    assert_eq!(value("DJINN_LAUNCHER_CGROUP_ROOT"), LAUNCHER_CGROUP_ROOT);
}

#[test]
fn valid_config_passes_render_validation() {
    let cfg = KubernetesConfig::for_testing();
    assert!(validate_enforcement_render(&cfg).is_ok());
}

/// The armed render now PASSES validation — that is the point of this task.
/// It is not a weakened gate: the individual preconditions below still fail
/// closed, and the launcher's own startup readiness is unchanged.
#[test]
fn the_armed_render_passes_validation() {
    let cfg = KubernetesConfig::for_testing();
    validate_enforcement_render(&cfg)
        .expect("the armed render must be dispatchable; it is the shipped enforcement path");
}

/// A pod CPU limit that cannot become a lease quota is refused while armed,
/// because the lease is the only ceiling left once the launcher container
/// has no CPU limit of its own.
#[test]
fn an_armed_render_with_an_unusable_lease_quota_fails_closed() {
    for limit in ["500m", "not-a-quantity", "128"] {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.cpu_limit = limit.to_string();
        assert!(
            matches!(
                validate_enforcement_render(&cfg),
                Err(RenderValidationError::UnsupportedLeaseQuota { .. })
            ),
            "cpu_limit {limit} must not arm enforcement"
        );
        // Disarmed, the same config still dispatches: the lease ceiling only
        // matters when something is actually leasing.
        cfg.cgroup_launcher_mode = CgroupLauncherMode::Disabled;
        assert!(validate_enforcement_render(&cfg).is_ok());
    }
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

/// The adversarial kernel-boundary proof (djinn-cgroup-launcher's
/// `tests/kernel_boundary_under_rendered_context.rs`, task zf13/goxi AC2)
/// cannot depend on this crate — that would be a dependency cycle — so it
/// drives its real UID-1000/1001 processes from a checked-in fixture of the
/// rendered security context. This test is what keeps that fixture honest:
/// it rebuilds the REAL manifest values and asserts every fixture line, so
/// changing the render without updating the fixture fails here instead of
/// silently proving a boundary nobody ships. It renders the complete Job rather
/// than only a standalone SecurityContext, so the fixture's RuntimeClass claim
/// cannot survive a missing `runtimeClassName`.
#[test]
fn rendered_security_context_matches_the_adversarial_proof_fixture() {
    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../djinn-cgroup-launcher/tests/fixtures/rendered-security-context.env");
    let fixture_contents = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|error| panic!("read fixture {fixture_path:?}: {error}"));
    let fixture: BTreeMap<_, _> = fixture_contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split_once('=').expect("fixture lines are key=value"))
        .collect();
    let value = |key| {
        *fixture
            .get(key)
            .unwrap_or_else(|| panic!("fixture contains {key}"))
    };

    let config = KubernetesConfig::for_testing();
    let job = crate::job::build_task_run_job(
        &config,
        &Uuid::nil(),
        "fixture-project",
        "fixture-secret",
        "registry.example/djinn:fixture",
        &[],
        None,
        false,
        None,
    );
    let pod = job
        .spec
        .and_then(|spec| spec.template.spec)
        .expect("rendered Job has PodSpec");
    let worker = pod
        .containers
        .iter()
        .find(|container| container.name == "worker")
        .expect("rendered Job has worker");
    let launcher = pod
        .init_containers
        .iter()
        .flatten()
        .find(|container| container.name == LAUNCHER_CONTAINER_NAME)
        .expect("armed Job has launcher");
    let launcher_security = launcher
        .security_context
        .as_ref()
        .expect("launcher security");
    let launcher_env = launcher.env.as_ref().expect("launcher environment");
    let env = |name| {
        launcher_env
            .iter()
            .find(|variable| variable.name == name)
            .and_then(|variable| variable.value.as_deref())
            .unwrap_or_else(|| panic!("launcher renders {name}"))
    };

    assert_eq!(
        pod.runtime_class_name.as_deref(),
        Some(value("task_run_runtime_class"))
    );
    assert_eq!(value("launcher_cgroup_source"), "runtimeclass");
    assert_eq!(value("launcher_cgroup_volume"), "none");
    assert_eq!(
        env("DJINN_LAUNCHER_CGROUP_ROOT"),
        value("launcher_cgroup_root")
    );
    assert_eq!(
        env("DJINN_LAUNCHER_EXPECTED_UID"),
        value("launcher_expected_uid")
    );
    assert_eq!(
        env("DJINN_LAUNCHER_UNLEASED_MILLICORES"),
        value("unleased_millicores")
    );
    assert_eq!(
        env("DJINN_LAUNCHER_LEASED_MILLICORES"),
        value("launcher_lease_quota_millicores")
    );
    assert_eq!(
        launcher_security.app_armor_profile, None,
        "default AppArmor has no override"
    );
    assert_eq!(value("launcher_apparmor_profile"), "RuntimeDefault");
    assert_eq!(
        launcher_security.seccomp_profile.as_ref().unwrap().type_,
        value("seccomp_profile")
    );
    assert_eq!(
        launcher_security.capabilities.as_ref().unwrap().add,
        Some(
            value("launcher_capabilities_add")
                .split(',')
                .map(str::to_string)
                .collect()
        )
    );
    assert_eq!(
        worker
            .security_context
            .as_ref()
            .unwrap()
            .seccomp_profile
            .as_ref()
            .unwrap()
            .type_,
        value("seccomp_profile")
    );
    assert_eq!(
        launcher
            .resources
            .as_ref()
            .unwrap()
            .requests
            .as_ref()
            .unwrap()["cpu"]
            .0,
        value("launcher_cpu_request")
    );
    assert!(
        launcher
            .resources
            .as_ref()
            .unwrap()
            .limits
            .as_ref()
            .unwrap()
            .get("cpu")
            .is_none(),
        "fixture's launcher_cpu_limit=none permits no ancestor clamp"
    );
    assert!(
        pod.init_containers
            .iter()
            .flatten()
            .chain(pod.containers.iter())
            .flat_map(|container| container.volume_mounts.iter().flatten())
            .all(|mount| mount.mount_path != value("launcher_cgroup_root")),
        "the RuntimeClass cgroup is never supplied through a container mount"
    );
    assert!(
        pod.volumes
            .iter()
            .flatten()
            .all(|volume| volume.host_path.is_none()),
        "the rendered Job contains no hostPath volume"
    );
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

/// A per-project CPU-limit override must move the lease with it. The lease is
/// the only ceiling that governs a build once the launcher container has no CPU
/// limit of its own, so a worker limit of 8 with a lease of 4 would silently
/// halve every build on that project.
#[test]
fn a_resolved_cpu_limit_override_retunes_the_lease() {
    use k8s_openapi::api::core::v1::PodSpec;

    let cfg = KubernetesConfig::for_testing();
    let mut pod = PodSpec {
        init_containers: Some(vec![launcher_sidecar_container(
            &cfg,
            "registry.example/proj:tag",
            false,
            false,
        )]),
        ..PodSpec::default()
    };
    let leased = |pod: &PodSpec| {
        pod.init_containers
            .iter()
            .flatten()
            .find(|c| c.name == LAUNCHER_CONTAINER_NAME)
            .and_then(|c| c.env.as_ref())
            .and_then(|env| {
                env.iter()
                    .find(|var| var.name == "DJINN_LAUNCHER_LEASED_MILLICORES")
            })
            .and_then(|var| var.value.clone())
            .expect("the launcher carries a lease ceiling")
    };

    assert_eq!(leased(&pod), "4000");
    retune_launcher_lease(&mut pod, "8");
    assert_eq!(leased(&pod), "8000");
    retune_launcher_lease(&mut pod, "6500m");
    assert_eq!(leased(&pod), "6500");
    // A limit that cannot become a lease quota leaves the rendered value alone
    // rather than writing something unbounded.
    retune_launcher_lease(&mut pod, "250m");
    assert_eq!(leased(&pod), "6500");
    retune_launcher_lease(&mut pod, "not-a-quantity");
    assert_eq!(leased(&pod), "6500");
}

// ---------------------------------------------------------------------------
// Task 4wx3: the resize-v2-only launcher CPU ceiling.
//
// The 7deu measurement (see the note in `launcher.rs` where `LAUNCHER_CPU_LIMIT`
// used to be) is the whole reason these tests assert an ABSENCE on the leaf-v1
// arm. A container CPU limit on the launcher is an ancestor clamp over every
// invocation leaf; under leaf-v1 the launcher writes those leaves' `cpu.max`, so
// the clamp silently caps every build. Under resize-v2 the launcher writes no
// leaf quota at all, so the container limit is the only ceiling there is.
// ---------------------------------------------------------------------------

/// Build the real task-run Job, so these assertions run against the manifest
/// dispatch actually submits rather than a hand-assembled `Container`.
fn rendered_task_run_job(config: &KubernetesConfig) -> k8s_openapi::api::batch::v1::Job {
    crate::job::build_task_run_job(
        config,
        &Uuid::nil(),
        "project-1",
        "djinn-task-run-secret",
        "registry.example/proj:tag",
        &[],
        None,
        false,
        None,
    )
}

fn launcher_of(job: &k8s_openapi::api::batch::v1::Job) -> &Container {
    job.spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .and_then(|pod| pod.init_containers.as_ref())
        .and_then(|containers| {
            containers
                .iter()
                .find(|container| container.name == LAUNCHER_CONTAINER_NAME)
        })
        .expect("the armed render carries the launcher sidecar")
}

fn quantities(
    map: Option<&BTreeMap<String, k8s_openapi::apimachinery::pkg::api::resource::Quantity>>,
) -> BTreeMap<String, String> {
    map.map(|map| {
        map.iter()
            .map(|(key, value)| (key.clone(), value.0.clone()))
            .collect()
    })
    .unwrap_or_default()
}

fn launcher_limits(job: &k8s_openapi::api::batch::v1::Job) -> BTreeMap<String, String> {
    quantities(
        launcher_of(job)
            .resources
            .as_ref()
            .and_then(|resources| resources.limits.as_ref()),
    )
}

fn launcher_requests(job: &k8s_openapi::api::batch::v1::Job) -> BTreeMap<String, String> {
    quantities(
        launcher_of(job)
            .resources
            .as_ref()
            .and_then(|resources| resources.requests.as_ref()),
    )
}

/// Read one env value off a container, or `None`.
fn env_value(container: &Container, name: &str) -> Option<String> {
    container
        .env
        .iter()
        .flatten()
        .find(|entry| entry.name == name)
        .and_then(|entry| entry.value.clone())
}

fn apply(
    job: &mut k8s_openapi::api::batch::v1::Job,
    protocol: LauncherAuthorityProtocol,
) -> Result<(), RenderValidationError> {
    apply_launcher_authority_protocol(job, CgroupLauncherMode::Required, protocol)
}

/// Overwrite (or remove) the rendered lease-ceiling env on the launcher sidecar,
/// so the ceiling guards can be driven to states the two current writers of that
/// variable cannot themselves produce.
fn set_lease_env(job: &mut k8s_openapi::api::batch::v1::Job, value: Option<&str>) {
    let pod = job
        .spec
        .as_mut()
        .and_then(|spec| spec.template.spec.as_mut())
        .expect("rendered pod spec");
    for container in pod.init_containers.iter_mut().flatten() {
        if container.name != LAUNCHER_CONTAINER_NAME {
            continue;
        }
        let env = container.env.get_or_insert_with(Vec::new);
        match value {
            Some(value) => {
                for entry in env.iter_mut() {
                    if entry.name == LEASED_MILLICORES_ENV {
                        entry.value = Some(value.to_string());
                    }
                }
            }
            None => env.retain(|entry| entry.name != LEASED_MILLICORES_ENV),
        }
    }
}

/// The millicore spelling of [`LAUNCHER_CPU_REQUEST`] used by the below-request
/// guard is the same number as the string the manifest carries.
#[test]
fn the_launcher_cpu_request_constants_agree() {
    assert_eq!(
        LAUNCHER_CPU_REQUEST,
        format!("{LAUNCHER_CPU_REQUEST_MILLICORES}m")
    );
}

/// **AC1.** A leaf-v1 render has NO `cpu` key in the launcher sidecar's limits;
/// a resize-v2 render has exactly the configured lease ceiling.
///
/// Non-vacuity, exactly as the AC words it: delete the
/// `protocol.launcher_owns_leaf_quota()` early return in
/// `resolve_launcher_cpu_ceiling` — i.e. render the ceiling unconditionally —
/// and the leaf-v1 assertion below fails. That mutation is the literal 7deu
/// defect-1 regression: an ancestor clamp on every invocation leaf, measured at
/// 0.25 of a core against a leaf set to 4.
#[test]
fn the_launcher_cpu_ceiling_is_rendered_only_under_resize_v2() {
    let cfg = KubernetesConfig::for_testing();

    let mut leaf_v1 = rendered_task_run_job(&cfg);
    apply(&mut leaf_v1, LauncherAuthorityProtocol::LeafV1).expect("leaf-v1 renders");
    let limits = launcher_limits(&leaf_v1);
    assert!(
        !limits.contains_key("cpu"),
        "leaf-v1 must render no launcher CPU limit; a limit here clamps every \
         invocation leaf the launcher lifts (7deu defect 1). Got: {limits:?}"
    );

    let mut resize_v2 = rendered_task_run_job(&cfg);
    apply(&mut resize_v2, LauncherAuthorityProtocol::ResizeV2).expect("resize-v2 renders");
    assert_eq!(
        launcher_limits(&resize_v2).get("cpu").map(String::as_str),
        Some(format!("{}m", launcher_leased_millicores(&cfg)).as_str()),
    );
    // Stated absolutely once as well, so a change to the default is visible.
    assert_eq!(
        launcher_limits(&resize_v2).get("cpu").map(String::as_str),
        Some("4000m")
    );
}

/// The ceiling is the lease the launcher will actually grant, including a
/// per-project `build_resources.task.cpu_limit` override — which
/// `apply_resolved_resources` applies BEFORE the protocol seam runs. Deriving
/// the ceiling from the deployment default instead would clamp such a pod below
/// its own lease: the 7deu ancestor clamp, re-entered through the override path.
#[test]
fn the_resize_v2_ceiling_tracks_a_per_project_cpu_limit_override() {
    let cfg = KubernetesConfig::for_testing();
    let mut job = rendered_task_run_job(&cfg);
    let pod = job
        .spec
        .as_mut()
        .and_then(|spec| spec.template.spec.as_mut())
        .expect("rendered pod spec");
    retune_launcher_lease(pod, "8");
    apply(&mut job, LauncherAuthorityProtocol::ResizeV2).expect("resize-v2 renders");
    assert_eq!(
        launcher_limits(&job).get("cpu").map(String::as_str),
        Some("8000m"),
        "the container ceiling must equal the lease ceiling, not the deployment default"
    );
}

/// Re-applying leaf-v1 over a resize-v2 render must leave no stale clamp: the
/// rendered resource shape is a total statement of the protocol, not an
/// additive one.
#[test]
fn re_rendering_leaf_v1_clears_a_resize_v2_ceiling() {
    let cfg = KubernetesConfig::for_testing();
    let mut job = rendered_task_run_job(&cfg);
    apply(&mut job, LauncherAuthorityProtocol::ResizeV2).expect("resize-v2 renders");
    assert!(launcher_limits(&job).contains_key("cpu"));
    apply(&mut job, LauncherAuthorityProtocol::LeafV1).expect("leaf-v1 renders");
    assert!(
        !launcher_limits(&job).contains_key("cpu"),
        "a leaf-v1 re-render must not inherit the resize-v2 clamp"
    );
}

/// **AC2.** The sidecar's requests are byte-identical across both renders — and
/// to the untouched builder output — and the memory limit is unchanged.
///
/// Non-vacuity: change either request value in one arm (or have
/// `apply_launcher_cpu_ceiling` write into `requests` rather than `limits`) and
/// the byte-equality below fails.
#[test]
fn the_launcher_requests_and_memory_limit_are_identical_across_protocols() {
    let cfg = KubernetesConfig::for_testing();
    let baseline_requests = launcher_requests(&rendered_task_run_job(&cfg));
    assert_eq!(
        baseline_requests,
        BTreeMap::from([
            ("cpu".to_string(), LAUNCHER_CPU_REQUEST.to_string()),
            ("memory".to_string(), LAUNCHER_MEMORY_REQUEST.to_string()),
        ]),
        "the request is the broker's steady footprint; it is not the build's"
    );

    for protocol in LauncherAuthorityProtocol::ALL {
        let mut job = rendered_task_run_job(&cfg);
        apply(&mut job, protocol).expect("both protocols render");
        assert_eq!(
            launcher_requests(&job),
            baseline_requests,
            "{protocol} must not disturb the launcher's requests"
        );
        assert_eq!(
            launcher_limits(&job).get("memory").map(String::as_str),
            Some(cfg.memory_limit.as_str()),
            "{protocol} must not disturb the launcher's memory limit — every \
             brokered command's memory peak lands in this container's cgroup"
        );
    }
}

/// **AC3.** A resize-v2 ceiling below the sidecar's own CPU request is a render
/// error, not a Pod the apiserver rejects at admission.
///
/// Non-vacuity: delete the `ceiling < LAUNCHER_CPU_REQUEST_MILLICORES` arm in
/// `resolve_launcher_cpu_ceiling` and this renders `limits.cpu: 10m` against
/// `requests.cpu: 50m` — a structurally invalid Pod — instead of failing.
#[test]
fn a_resize_v2_ceiling_below_the_launcher_cpu_request_is_refused() {
    let cfg = KubernetesConfig::for_testing();
    let mut job = rendered_task_run_job(&cfg);
    set_lease_env(&mut job, Some("10"));
    let error = apply(&mut job, LauncherAuthorityProtocol::ResizeV2)
        .expect_err("a 10m ceiling under a 50m request must not render");
    assert!(
        matches!(
            error,
            RenderValidationError::LauncherCpuCeilingBelowRequest { ceiling: 10, .. }
        ),
        "unexpected error: {error:?}"
    );
    // A refusal leaves the Job as it found it rather than half-applied.
    assert!(!launcher_limits(&job).contains_key("cpu"));
    assert_eq!(
        env_value(launcher_of(&job), AUTHORITY_PROTOCOL_ENV),
        None,
        "a refused render must not have already written the protocol"
    );
    // leaf-v1 is unaffected: its launcher container carries no ceiling at all,
    // so there is nothing that could sit below the request.
    let mut leaf_v1 = rendered_task_run_job(&cfg);
    set_lease_env(&mut leaf_v1, Some("10"));
    apply(&mut leaf_v1, LauncherAuthorityProtocol::LeafV1)
        .expect("leaf-v1 has no ceiling to check");
}

/// **AC3, the config-level door.** The same 10m stated as the pod CPU limit
/// never reaches a render at all: `validate_enforcement_render` refuses it
/// before the Job is built, because 10m maps onto no usable lease quota.
#[test]
fn a_ten_millicore_pod_cpu_limit_never_arms() {
    let mut cfg = KubernetesConfig::for_testing();
    cfg.cpu_limit = "10m".to_string();
    let error = validate_enforcement_render(&cfg)
        .expect_err("10m must not arm enforcement under either protocol");
    assert!(
        matches!(error, RenderValidationError::UnsupportedLeaseQuota { .. }),
        "unexpected error: {error:?}"
    );
}

/// A resize-v2 render whose ceiling cannot be resolved at all is refused rather
/// than rendered without one. Under resize-v2 the launcher never writes leaf
/// `cpu.max`, so "no container limit" does not mean "the leaf bounds it" — it
/// means the build is bounded only by the node.
#[test]
fn an_unresolvable_resize_v2_ceiling_is_refused() {
    let cfg = KubernetesConfig::for_testing();
    for lease in [None, Some(""), Some("   "), Some("4000m"), Some("0")] {
        let mut job = rendered_task_run_job(&cfg);
        set_lease_env(&mut job, lease);
        let error = apply(&mut job, LauncherAuthorityProtocol::ResizeV2)
            .expect_err("an unresolvable lease ceiling must not render");
        assert!(
            matches!(
                error,
                RenderValidationError::UnresolvableLauncherCpuCeiling { .. }
            ),
            "lease {lease:?}: unexpected error {error:?}"
        );
        assert!(!launcher_limits(&job).contains_key("cpu"));
    }
}

/// **AC4.** No non-launcher container's resources change under either protocol.
///
/// Asserted structurally over the WHOLE rendered manifest: every container in
/// `initContainers` and `containers` is discovered from the serialized Job and
/// its `resources` compared, so a resource change on the worker — or on a
/// backing-service sidecar, or on a container that does not exist yet — is
/// caught without this test naming it. The container SET is compared too, so
/// dropping a container is not silently "nothing's resources changed".
#[test]
fn no_non_launcher_container_resources_change_under_either_protocol() {
    let cfg = KubernetesConfig::for_testing();

    fn container_resources(
        job: &k8s_openapi::api::batch::v1::Job,
    ) -> BTreeMap<String, serde_json::Value> {
        let document = serde_json::to_value(job).expect("the Job serializes");
        let pod = &document["spec"]["template"]["spec"];
        ["initContainers", "containers"]
            .into_iter()
            .flat_map(|field| {
                pod[field]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |container| {
                        (
                            format!(
                                "{field}/{}",
                                container["name"].as_str().unwrap_or("<unnamed>")
                            ),
                            container
                                .get("resources")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        )
                    })
            })
            .collect()
    }

    let launcher_key = format!("initContainers/{LAUNCHER_CONTAINER_NAME}");
    let baseline = container_resources(&rendered_task_run_job(&cfg));
    assert!(
        baseline.contains_key(&launcher_key) && baseline.len() > 1,
        "the manifest must carry the launcher AND something else, or this test \
         proves nothing: {baseline:?}"
    );

    for protocol in LauncherAuthorityProtocol::ALL {
        let mut job = rendered_task_run_job(&cfg);
        apply(&mut job, protocol).expect("both protocols render");
        let after = container_resources(&job);
        assert_eq!(
            baseline.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>(),
            "{protocol} changed the rendered container set"
        );
        for (key, before) in &baseline {
            if key == &launcher_key {
                continue;
            }
            assert_eq!(
                Some(before),
                after.get(key),
                "{protocol} changed the resources of container {key}"
            );
        }
    }
}

/// A `disabled` render carries no sidecar to hold a ceiling, and asking for one
/// is a no-op rather than an error — the same shape the protocol env has.
#[test]
fn a_disabled_render_gets_no_launcher_ceiling() {
    let mut cfg = KubernetesConfig::for_testing();
    cfg.cgroup_launcher_mode = CgroupLauncherMode::Disabled;
    let mut job = rendered_task_run_job(&cfg);
    apply_launcher_authority_protocol(
        &mut job,
        CgroupLauncherMode::Disabled,
        LauncherAuthorityProtocol::ResizeV2,
    )
    .expect("disabled is a no-op");
    let pod = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("rendered pod spec");
    assert!(
        !pod.init_containers
            .iter()
            .flatten()
            .any(|container| container.name == LAUNCHER_CONTAINER_NAME),
        "a disabled render carries no launcher sidecar at all"
    );
}
