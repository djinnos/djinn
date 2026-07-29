//! Unit tests for [`crate::launcher`]: the rendered v1 security contract, the
//! role-classed resource envelope, the launcher's residual capability set, and
//! the fail-closed render validation.
//!
//! Split out of `launcher.rs` to keep that file inside the repository's per-file
//! size guard. It is `#[path]`-included as a private child module, so it still
//! sees the module's private items (`launcher_capabilities`,
//! `parse_cpu_millicores`) exactly as an inline `mod tests` would.

use super::*;

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
