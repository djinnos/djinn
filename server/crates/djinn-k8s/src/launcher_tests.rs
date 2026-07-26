//! Unit tests for [`crate::launcher`]: the rendered v1 security contract, the
//! role-classed resource envelope, the launcher's bootstrap capability set, and
//! the fail-closed render validation.
//!
//! Split out of `launcher.rs` to keep that file inside the repository's per-file
//! size guard. It is `#[path]`-included as a private child module, so it still
//! sees the module's private items (`launcher_capabilities`,
//! `parse_cpu_millicores`) exactly as an inline `mod tests` would.

use super::*;

use djinn_cgroup_launcher::bootstrap::{
    BOOTSTRAP_ONLY_CAPABILITY_NAMES, RETAINED_CAPABILITY_NAMES,
};
use djinn_runtime::RoleKind;

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

/// The launcher is granted exactly the six capabilities it needs, spelled
/// the way the API server accepts, and is never `privileged`.
///
/// `CHOWN` is retained past bootstrap because the broker socket is handed to the
/// worker with `chown(2)`, which requires the capability even at euid 0.
#[test]
fn launcher_capabilities_are_exactly_the_designed_set() {
    let caps = launcher_capabilities();
    assert_eq!(caps.drop.as_deref(), Some(&["ALL".to_string()][..]));
    let add = caps.add.expect("launcher adds capabilities");
    assert_eq!(
        add,
        vec![
            "CHOWN",
            "SETGID",
            "SETUID",
            "SETPCAP",
            "SYS_ADMIN",
            "SYS_RESOURCE"
        ]
    );
    // `privileged: true` would grant everything and defeat the bootstrap
    // drop entirely; it must never appear.
    let sc = launcher_security_context();
    assert_eq!(sc.privileged, None);
    assert_eq!(sc.read_only_root_filesystem, Some(true));
}

/// The Kubernetes API server rejects the literal string `CAP_SYS_ADMIN` in
/// `capabilities.add` when `allowPrivilegeEscalation` is false. goxi's
/// manifest as written uses that spelling and is therefore API-invalid; the
/// conventional bare name is accepted. A regression here is a 422 at Job
/// submission, which is a much worse place to find out.
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

/// The bootstrap capabilities are named in one place and are exactly the
/// ones the launcher crate drops at runtime. If the render adds a
/// bootstrap-only capability the runtime does not shed, a task-run pod ships
/// holding it — which for SYS_ADMIN is a node-wide escape primitive.
#[test]
fn every_bootstrap_only_capability_is_one_the_launcher_drops_at_runtime() {
    let add = launcher_capabilities().add.unwrap();
    for capability in LAUNCHER_BOOTSTRAP_ONLY_CAPABILITIES {
        assert!(
            add.iter().any(|granted| granted == capability),
            "{capability} is declared bootstrap-only but is not granted"
        );
    }
    // The permanent set is what `child::prepare_child` needs and nothing
    // more; anything else granted must be on the bootstrap-only list.
    let permanent: Vec<&String> = add
        .iter()
        .filter(|granted| !LAUNCHER_BOOTSTRAP_ONLY_CAPABILITIES.contains(&granted.as_str()))
        .collect();
    assert_eq!(permanent, vec!["CHOWN", "SETGID", "SETUID", "SETPCAP"]);
}

/// The rendered grant and the runtime `capset` must be the same set.
///
/// This is the guard for the third v0.7.x rollback. The launcher retains exactly
/// `bootstrap::RETAINED_CAPABILITIES` and destroys the rest, so a capability the
/// manifest grants but the runtime does not retain is gone microseconds later,
/// and one the runtime retains but the manifest never grants never existed. Both
/// directions are silent until a real kernel refuses a syscall — which is how
/// `CAP_CHOWN` came to be missing while `UnixBrokerServer::bind` needed it.
#[test]
fn the_rendered_grant_is_exactly_what_the_runtime_retains_plus_bootstrap_only() {
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
        .chain(BOOTSTRAP_ONLY_CAPABILITY_NAMES)
        .map(|capability| (*capability).to_string())
        .collect();
    assert_eq!(
        add, expected,
        "the Pod grant must be the runtime's retained set plus its bootstrap-only set"
    );

    for retained in RETAINED_CAPABILITY_NAMES {
        assert!(
            !LAUNCHER_BOOTSTRAP_ONLY_CAPABILITIES.contains(retained),
            "{retained} cannot be both retained and dropped"
        );
    }
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

/// The armed sidecar mounts a writable directory at the cgroup root path.
/// Without it `readOnlyRootFilesystem: true` makes the launcher's own
/// `mount(2)` fail at the mountpoint, before it ever reaches the kernel.
#[test]
fn the_launcher_mounts_a_writable_mountpoint_at_the_cgroup_root() {
    let cfg = KubernetesConfig::for_testing();
    let c = launcher_sidecar_container(&cfg, "registry.example/proj:tag", false, false);
    let mount = c
        .volume_mounts
        .unwrap()
        .into_iter()
        .find(|mount| mount.mount_path == LAUNCHER_CGROUP_ROOT)
        .expect("the launcher must mount a writable cgroup mountpoint");
    assert_eq!(mount.name, VOLUME_LAUNCHER_CGROUP);
    assert_ne!(
        mount.read_only,
        Some(true),
        "a read-only mountpoint cannot be mounted over"
    );
    // And the volume that backs it must be declared, or the API server
    // rejects the manifest for a dangling volumeMount.
    let volume = launcher_cgroup_mountpoint_volume();
    assert_eq!(volume.name, VOLUME_LAUNCHER_CGROUP);
    assert!(volume.empty_dir.is_some());
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
/// silently proving a boundary nobody ships.
#[test]
fn rendered_security_context_matches_the_adversarial_proof_fixture() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../djinn-cgroup-launcher/tests/fixtures/rendered-security-context.env");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read rendered security-context fixture {path:?}: {e}"));
    let fixture: BTreeMap<&str, &str> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (key, value) = line.split_once('=').expect("fixture line is key=value");
            (key.trim(), value.trim())
        })
        .collect();

    let config = KubernetesConfig::for_testing();
    // Use the production/default Job builder rather than a parallel security
    // context fixture. The privileged cpu.stat lane consumes this file, so a
    // manifest change cannot leave it measuring a stale approximation.
    let job = crate::job::build_task_run_job(
        &config,
        &uuid::Uuid::now_v7(),
        "rendered-contract-project",
        "djinn-taskrun-rendered-contract",
        "registry.example/djinn-project:contract",
        &[],
        None,
        false,
        Some(RoleKind::Worker),
    );
    let pod = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("production Job has a pod spec");
    let worker = pod
        .containers
        .iter()
        .find(|container| container.name == "worker")
        .and_then(|container| container.security_context.as_ref())
        .expect("production Job worker security context");
    let launcher_container = pod
        .init_containers
        .iter()
        .flatten()
        .find(|container| container.name == LAUNCHER_CONTAINER_NAME)
        .expect("production required Job launcher container");
    let launcher = launcher_container
        .security_context
        .as_ref()
        .expect("production Job launcher security context");
    let pod_security = pod
        .security_context
        .as_ref()
        .expect("production Job pod security context");
    let capabilities = |sc: &SecurityContext, take_added: bool| {
        let caps = sc.capabilities.as_ref().expect("capabilities");
        let list = if take_added {
            caps.add.clone().unwrap_or_default()
        } else {
            caps.drop.clone().unwrap_or_default()
        };
        list.join(",")
    };
    let expected: BTreeMap<&str, String> = BTreeMap::from([
        (
            "worker_run_as_user",
            worker.run_as_user.unwrap().to_string(),
        ),
        (
            "worker_run_as_group",
            worker.run_as_group.unwrap().to_string(),
        ),
        (
            "worker_run_as_non_root",
            worker.run_as_non_root.unwrap().to_string(),
        ),
        (
            "worker_allow_privilege_escalation",
            worker.allow_privilege_escalation.unwrap().to_string(),
        ),
        ("worker_capabilities_drop", capabilities(worker, false)),
        (
            "launcher_run_as_user",
            launcher.run_as_user.unwrap().to_string(),
        ),
        (
            "launcher_run_as_group",
            launcher.run_as_group.unwrap().to_string(),
        ),
        (
            "launcher_allow_privilege_escalation",
            launcher.allow_privilege_escalation.unwrap().to_string(),
        ),
        (
            "launcher_read_only_root_filesystem",
            launcher.read_only_root_filesystem.unwrap().to_string(),
        ),
        ("launcher_capabilities_drop", capabilities(launcher, false)),
        ("launcher_capabilities_add", capabilities(launcher, true)),
        (
            "launcher_bootstrap_only_capabilities",
            LAUNCHER_BOOTSTRAP_ONLY_CAPABILITIES.join(","),
        ),
        (
            "launcher_apparmor_profile",
            launcher
                .app_armor_profile
                .as_ref()
                .expect("the required render pins the launcher's AppArmor profile")
                .type_
                .clone(),
        ),
        ("child_run_as_user", CHILD_UID.to_string()),
        ("child_run_as_group", ARTIFACT_GID.to_string()),
        ("child_umask", "0002".to_string()),
        ("pod_fs_group", pod_security.fs_group.unwrap().to_string()),
        (
            "pod_fs_group_change_policy",
            pod_security.fs_group_change_policy.clone().unwrap(),
        ),
        (
            "seccomp_profile",
            worker.seccomp_profile.as_ref().unwrap().type_.clone(),
        ),
        ("launcher_expected_uid", LAUNCHER_UID.to_string()),
        (
            "unleased_millicores",
            LAUNCHER_UNLEASED_MILLICORES.to_string(),
        ),
        (
            "leased_millicores",
            launcher_leased_millicores(&config).to_string(),
        ),
        (
            "cgroup_delegation_profile",
            config.cgroup_delegation_profile.clone(),
        ),
        (
            "volume_ownership_mode",
            config.volume_ownership_mode.clone(),
        ),
        (
            "launcher_cpu_limit",
            launcher_container
                .resources
                .as_ref()
                .and_then(|resources| resources.limits.as_ref())
                .and_then(|limits| limits.get("cpu"))
                .map_or_else(|| "none".to_owned(), |quantity| quantity.0.clone()),
        ),
        (
            "launcher_cpu_request",
            launcher_container
                .resources
                .as_ref()
                .and_then(|resources| resources.requests.as_ref())
                .and_then(|requests| requests.get("cpu"))
                .expect("production launcher CPU request")
                .0
                .clone(),
        ),
        (
            "launcher_memory_request",
            launcher_container
                .resources
                .as_ref()
                .and_then(|resources| resources.requests.as_ref())
                .and_then(|requests| requests.get("memory"))
                .expect("production launcher memory request")
                .0
                .clone(),
        ),
        (
            "launcher_memory_limit",
            launcher_container
                .resources
                .as_ref()
                .and_then(|resources| resources.limits.as_ref())
                .and_then(|limits| limits.get("memory"))
                .expect("production launcher memory limit")
                .0
                .clone(),
        ),
        (
            "launcher_lease_quota_millicores",
            launcher_container
                .env
                .as_ref()
                .and_then(|env| {
                    env.iter()
                        .find(|entry| entry.name == "DJINN_LAUNCHER_LEASED_MILLICORES")
                })
                .and_then(|entry| entry.value.clone())
                .expect("production launcher explicit lease quota"),
        ),
    ]);

    for (key, value) in &expected {
        assert_eq!(
            fixture.get(key).copied(),
            Some(value.as_str()),
            "the adversarial proof fixture is stale for `{key}`; update \
             crates/djinn-cgroup-launcher/tests/fixtures/rendered-security-context.env"
        );
    }
    let extra: Vec<&&str> = fixture
        .keys()
        .filter(|k| !expected.contains_key(*k))
        .collect();
    assert!(
        extra.is_empty(),
        "the fixture declares keys the render does not produce: {extra:?}"
    );
    // The launcher and worker share one seccomp profile; assert it rather
    // than let the fixture describe only half the contract.
    assert_eq!(
        launcher.seccomp_profile.as_ref().unwrap().type_,
        fixture["seccomp_profile"]
    );
    // The render must also actually accept the profile pair it advertises.
    assert!(validate_enforcement_render(&config).is_ok());
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
