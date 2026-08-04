//! Unit tests for [`super`], the warm Job manifest builder.
//!
//! Split out of `warm_job.rs` so the production module stays inside the
//! `Server Guards` file-size budget (MAX_LINES=1500 / MAX_BYTES=51200).
//! Included with `#[path]` so it stays a child of the module under test and
//! keeps its `use super::*` access to the private builder internals.

use super::*;

#[test]
fn builds_warm_job_manifest_with_expected_shape() {
    let mut cfg = KubernetesConfig::for_testing();
    cfg.database_url = Some("postgres://djinn@djinn-postgres:5432/djinn".into());
    cfg.warm_job_termination_grace_period_seconds = 47;
    let job = build_warm_job(
        &cfg,
        "proj-xyz",
        "deadbeef",
        "reg.example:5000/djinn-project-p:abc123",
        None,
    );

    let meta = &job.metadata;
    let name = meta.name.as_deref().expect("name");
    assert!(name.starts_with("djinn-warm-proj-xyz-"), "name: {name}");
    assert_eq!(meta.namespace.as_deref(), Some(cfg.namespace.as_str()));

    let labels = meta.labels.as_ref().expect("labels");
    assert_eq!(
        labels.get(LABEL_COMPONENT).map(String::as_str),
        Some(COMPONENT_GRAPH_WARM)
    );
    assert_eq!(labels.get(LABEL_WARM).map(String::as_str), Some("true"));
    assert_eq!(
        labels.get(LABEL_PROJECT_ID).map(String::as_str),
        Some("proj-xyz")
    );

    let spec = job.spec.as_ref().expect("spec");
    assert_eq!(spec.backoff_limit, Some(0));
    assert_eq!(
        spec.ttl_seconds_after_finished,
        Some(cfg.warm_job_ttl_seconds)
    );
    assert_eq!(
        spec.active_deadline_seconds,
        Some(cfg.warm_job_timeout_seconds)
    );
    assert_eq!(
        spec.template
            .spec
            .as_ref()
            .unwrap()
            .termination_grace_period_seconds,
        Some(47),
    );

    let pod = spec.template.spec.as_ref().expect("pod");
    assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
    // See the rationale on `security_context` in `build_warm_job`.
    let psc = pod.security_context.as_ref().expect("pod security context");
    assert_eq!(
        psc.run_as_user,
        Some(i64::from(crate::launcher::CHILD_UID)),
        "warm pod must run as the cargo/build-script uid (1001): it CREATES \
         content in the shared cargo base, and chmod/chown/utimes are \
         governed by ownership alone"
    );
    assert_ne!(
        psc.run_as_user,
        Some(i64::from(crate::launcher::WORKER_UID)),
        "the 1000/1001 split is load-bearing: the broker control socket is \
         worker-owned 0600 precisely to refuse uid 1001 at connect(2)"
    );
    assert_eq!(
        psc.run_as_group,
        Some(i64::from(crate::launcher::ARTIFACT_GID)),
        "group stays 1000: still the mechanism for the lifecycle-only \
         identities that share this tree"
    );
    assert_eq!(
        psc.fs_group,
        Some(i64::from(crate::launcher::ARTIFACT_GID)),
        "warm pod must join the artifact group that owns the shared cache"
    );
    assert_eq!(
        psc.fs_group_change_policy.as_deref(),
        Some("OnRootMismatch"),
        "never Always: an unbounded recursive chown would stall warm pod start"
    );
    assert_eq!(
        pod.service_account_name.as_deref(),
        Some(cfg.service_account.as_str())
    );
    assert_eq!(pod.containers.len(), 1);

    // Default config carries no scheduling hints — manifest must be
    // byte-identical to the pre-feature shape. Mirrors the equivalent
    // assertion in job.rs.
    assert!(
        pod.node_selector.is_none(),
        "default config must not set nodeSelector"
    );
    assert!(
        pod.tolerations.is_none(),
        "default config must not set tolerations"
    );

    let container = &pod.containers[0];
    assert_eq!(container.name, "warmer");
    // Warm Pod runs on the per-project devcontainer image — that's
    // where the language indexers (rust-analyzer SCIP etc.) live.
    assert_eq!(
        container.image.as_deref(),
        Some("reg.example:5000/djinn-project-p:abc123")
    );
    // Pod command is a bash wrapper that clones the mirror before execing
    // the warm binary.
    let cmd = container.command.as_ref().expect("command");
    assert_eq!(cmd.len(), 3);
    assert_eq!(cmd[0], "/bin/bash");
    assert_eq!(cmd[1], "-c");
    assert!(cmd[2].contains("git clone"), "bash -c script: {}", cmd[2]);
    // The read-only mirror is server-owned while the warmer runs as the
    // worker uid, so the shell needs a safe.directory exception for it. It
    // must be an exported config FILE: git honours safe.directory only from
    // protected file scope in the inner child of `git clone --local` and
    // strips GIT_CONFIG_COUNT/KEY_0/VALUE_0 from that child (nurw).
    for setting in [
        "export GIT_CONFIG_SYSTEM=/workspace/.djinn-gitconfig",
        "unset GIT_CONFIG_NOSYSTEM",
        r#"printf '[safe]\n\tdirectory = *\n' > "$GIT_CONFIG_SYSTEM""#,
    ] {
        assert!(
            cmd[2].contains(setting),
            "warm shell must set up the trust file with {setting}: {}",
            cmd[2]
        );
    }
    assert!(
        !cmd[2].contains("export GIT_CONFIG_COUNT"),
        "command-scope config is stripped from the inner child of `git clone --local`, \
         so it must not be the mechanism: {}",
        cmd[2]
    );
    assert!(
        !cmd[2].contains("git config --global --add safe.directory"),
        "safe.directory must be inherited instead of persisted into $HOME/.gitconfig: {}",
        cmd[2]
    );
    // Warm clone must give the coupling index enough history to walk
    // `cursor..HEAD` without a forced unshallow on every warm. Depth
    // 1000 covers the typical case (warm cadence is <100 new commits,
    // so the saved cursor almost always lands in this window). See
    // `cases/plan-a-warm-cargo-base-reuse-validated-working-v0-6-11-0-6-12`
    // and `coupling_index::try_fetch_cursor` for the fallback path
    // when the cursor is older than the clone depth. The substring
    // match has to look for the leading space — bare `--depth 1`
    // would otherwise match the first three chars of `--depth 1000`.
    assert!(
        cmd[2].contains(" --depth 1000"),
        "warm clone must use --depth 1000 so the saved coupling cursor is \
         reachable on a fresh clone: {}",
        cmd[2]
    );
    assert!(
        !cmd[2].contains(" --depth 1 "),
        "warm clone must NOT use --depth 1 (forces an unshallow on every \
         warm): {}",
        cmd[2]
    );
    assert!(cmd[2].contains(WARM_COMMAND_BIN));
    assert!(cmd[2].contains("warm-graph \"proj-xyz\""));
    // JS deps are installed (lockfile-gated) before warming so the TS
    // indexer can resolve workspace-package tsconfig `extends`.
    assert!(
        cmd[2].contains("pnpm-lock.yaml"),
        "bash -c script: {}",
        cmd[2]
    );
    assert!(cmd[2].contains("pnpm install"));
    // The cargo target base is warmed inside `warm-graph` (the worker), where
    // mtimes are normalized to match task-run — NOT in this shell wrapper.
    // The old in-shell `cargo` step gated on a root `Cargo.toml` djinn's
    // `server/` workspace lacks, so it never ran; guard against its return.
    assert!(
        !cmd[2].contains("cargo clippy"),
        "cargo warm must live in the worker, not the warm-Job shell: {}",
        cmd[2]
    );

    let envs: BTreeMap<&str, &str> = container
        .env
        .as_ref()
        .expect("env")
        .iter()
        .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
        .collect();
    assert_eq!(
        envs.get("DJINN_MIRROR_ROOT").copied(),
        Some(MIRROR_MOUNT_DIR)
    );
    assert_eq!(envs.get("DJINN_WARM_PROJECT_ID").copied(), Some("proj-xyz"));
    // The in-Pod warm sizes its per-step cargo budgets against the Job's
    // own activeDeadlineSeconds; the two must be the same number or a step
    // bound could silently exceed the deadline that kills the Pod.
    assert_eq!(
        envs.get("DJINN_WARM_JOB_DEADLINE_SECONDS").copied(),
        Some(cfg.warm_job_timeout_seconds.to_string().as_str()),
    );
    // The bounded wait for another Pod's in-flight semantic index of the same
    // tree. Rendered here or settable nowhere: the warm Pod's environment is
    // exactly what this manifest puts in it.
    assert_eq!(
        envs.get(WARM_SCIP_CLAIM_WAIT_ENV).copied(),
        Some(cfg.scip_claim_wait_seconds.to_string().as_str()),
    );
    // DJINN_SERVER_ADDR is intentionally absent — `warm-graph` lives
    // on a disjoint subcommand whose `WorkerDefaultArgs` are not
    // parsed, so any residual envs would only be noise.
    assert!(!envs.contains_key("DJINN_SERVER_ADDR"));
    assert_eq!(
        envs.get("DJINN_PROJECT_ROOT").copied(),
        Some(format!("{WORKSPACE_MOUNT_DIR}/proj-xyz").as_str()),
    );
    // DB env forwarded from KubernetesConfig so the warm Pod shares
    // the server's Postgres target. The worker hard-requires
    // DJINN_DATABASE_URL (postgres cut-over renamed it from
    // DJINN_MYSQL_URL); regression guard for the warm-path miss.
    assert_eq!(
        envs.get("DJINN_DATABASE_URL").copied(),
        Some("postgres://djinn@djinn-postgres:5432/djinn"),
    );
    assert!(!envs.contains_key("DJINN_MYSQL_URL"));

    // Warm cache routing must keep the shared per-project target base as the
    // warm-owned seed with INCREMENTAL compilation enabled (warm == verify ==
    // worker parity) while task-run Pods use private run target dirs.
    assert_eq!(envs.get("CARGO_HOME").copied(), Some("/cache/cargo"));
    assert_eq!(
        envs.get("CARGO_TARGET_DIR").copied(),
        Some("/cache/cargo-target/proj-xyz/mold-jobs-4"),
    );
    // CARGO_INCREMENTAL=1 + RUSTC_WRAPPER="": all djinn build pods share one
    // incremental-on, sccache-off strategy so the warm seed is reusable.
    assert_eq!(envs.get("CARGO_INCREMENTAL").copied(), Some("1"));
    assert_eq!(
        envs.get("RUSTC_WRAPPER").copied(),
        Some(""),
        "warm pod must clear RUSTC_WRAPPER so incremental works"
    );
    assert_eq!(
        envs.get("SCCACHE_DIR").copied(),
        Some("/cache/sccache/proj-xyz"),
    );
    assert_eq!(envs.get("SCCACHE_CACHE_SIZE").copied(), Some("20G"));
    assert_eq!(envs.get("SQLX_OFFLINE").copied(), Some("true"));
    // Fast linker: mold is installed in the devcontainer image; wire it in
    // for the warm build so the warm base is linked (and fingerprinted)
    // identically to the task-run pods that seed from it.
    assert_eq!(
        envs.get("CARGO_BUILD_RUSTFLAGS").copied(),
        Some("-Clink-arg=-fuse-ld=mold -Clink-arg=-Wl,--threads=4"),
    );
    // Cargo/nextest parallelism is pinned to the WARM pod's OWN CPU limit
    // (warm_cpu_limit default "4"), not the host core count — the same
    // load-103 oversubscription guard the task-run pods get, derived from
    // each pod type's own limit.
    assert_eq!(envs.get("CARGO_BUILD_JOBS").copied(), Some("4"));
    assert_eq!(envs.get("NEXTEST_TEST_THREADS").copied(), Some("4"));

    let mounts = container.volume_mounts.as_ref().expect("mounts");
    assert_eq!(mounts.len(), 4, "mirror + workspace + cache + env-config");
    let by_name: BTreeMap<&str, &VolumeMount> =
        mounts.iter().map(|m| (m.name.as_str(), m)).collect();
    let mirror = by_name.get(VOLUME_MIRROR).expect("mirror mount");
    assert_eq!(mirror.mount_path, MIRROR_MOUNT_DIR);
    assert_eq!(mirror.read_only, Some(true));
    let workspace = by_name.get(VOLUME_WORKSPACE).expect("workspace mount");
    assert_eq!(workspace.mount_path, WORKSPACE_MOUNT_DIR);
    assert_eq!(workspace.read_only, Some(false));
    let cache = by_name.get(crate::job::VOLUME_CACHE).expect("cache mount");
    assert_eq!(cache.mount_path, crate::job::CACHE_MOUNT_DIR);
    assert_eq!(cache.read_only, Some(false));
    let env_config_mount = by_name
        .get(crate::env_config::VOLUME_ENV_CONFIG)
        .expect("env-config mount");
    assert_eq!(
        env_config_mount.mount_path,
        crate::env_config::ENV_CONFIG_MOUNT_DIR
    );
    assert_eq!(env_config_mount.read_only, Some(true));

    let volumes = pod.volumes.as_ref().expect("volumes");
    let by_volume_name: BTreeMap<&str, &Volume> =
        volumes.iter().map(|v| (v.name.as_str(), v)).collect();
    let mirror_v = by_volume_name.get(VOLUME_MIRROR).expect("mirror volume");
    let pvc = mirror_v.persistent_volume_claim.as_ref().expect("pvc");
    assert_eq!(pvc.claim_name, cfg.mirror_pvc);
    assert_eq!(pvc.read_only, Some(true));
    let workspace_v = by_volume_name
        .get(VOLUME_WORKSPACE)
        .expect("workspace volume");
    assert!(
        workspace_v.empty_dir.is_some(),
        "workspace must be emptyDir"
    );
    let cache_v = by_volume_name
        .get(crate::job::VOLUME_CACHE)
        .expect("cache volume");
    let cache_pvc = cache_v
        .persistent_volume_claim
        .as_ref()
        .expect("cache volume is a PVC source");
    assert_eq!(cache_pvc.claim_name, cfg.cache_pvc);
    assert_eq!(cache_pvc.read_only, Some(false));
    let env_v = by_volume_name
        .get(crate::env_config::VOLUME_ENV_CONFIG)
        .expect("env-config volume");
    let cm_src = env_v
        .config_map
        .as_ref()
        .expect("env-config volume is a ConfigMap source");
    assert_eq!(cm_src.name, "djinn-env-proj-xyz");
    assert_eq!(
        cm_src.optional,
        Some(true),
        "env-config CM must be optional so Pods start pre-P6 when the CM doesn't exist yet"
    );

    // Resource requests/limits from `warm_*` config knobs (Gap 4) —
    // without these the warm Pod runs unbounded and SCIP indexers
    // can spike CPU/memory under the kubelet's nose.
    let resources = container
        .resources
        .as_ref()
        .expect("warm container.resources set");
    let requests = resources.requests.as_ref().expect("requests set");
    assert_eq!(
        requests.get("cpu").map(|q| q.0.as_str()),
        Some(cfg.warm_cpu_request.as_str())
    );
    assert_eq!(
        requests.get("memory").map(|q| q.0.as_str()),
        Some(cfg.warm_memory_request.as_str())
    );
    let limits = resources.limits.as_ref().expect("limits set");
    assert_eq!(
        limits.get("cpu").map(|q| q.0.as_str()),
        Some(cfg.warm_cpu_limit.as_str())
    );
    assert_eq!(
        limits.get("memory").map(|q| q.0.as_str()),
        Some(cfg.warm_memory_limit.as_str())
    );
    // Defaults pin the documented values. Memory limit bumped 4Gi → 6Gi to
    // cover the added test-compile warm pass (--all-targets test codegen).
    // CPU request bumped 1 → 4 to match the limit: cgroup cpu.weight
    // derives from the REQUEST, so a `1` request starved the warm under
    // host contention and timed the Rust SCIP pass out on every run.
    assert_eq!(cfg.warm_cpu_request, "4");
    assert_eq!(cfg.warm_cpu_limit, "4");
    // The warm's CPU request must equal its limit so the kubelet gives it a
    // cgroup CFS share proportional to what it actually uses — graph
    // freshness is user-facing and must not be starved by neighbouring
    // task-run Pods on a contended node.
    assert_eq!(
        cfg.warm_cpu_request, cfg.warm_cpu_limit,
        "warm CPU request must equal limit so cpu.weight reflects real need"
    );
    assert_eq!(cfg.warm_memory_request, "2Gi");
    assert_eq!(cfg.warm_memory_limit, "6Gi");
}

/// The precondition that makes running the warm pod as `CHILD_UID` free of
/// security consequence — asserted, not assumed.
///
/// `transport::restrict_socket_to_worker` hands the broker control socket to
/// `WORKER_UID` at mode `0600` so uid 1001 is refused at `connect(2)`. That
/// only matters if a warm pod has a socket to connect to; it does not. If a
/// launcher is ever added to the warm path this test fails, and the uid
/// choice above must be revisited in the same change.
#[test]
fn durable_attempt_stamp_reaches_leased_and_unleased_pods_without_renaming_them() {
    let cfg = KubernetesConfig::for_testing();
    let mut plain = build_warm_job(&cfg, "proj-xyz", "deadbeef", "example/warm:latest", None);
    let mut leased = build_leased_warm_job(
        &cfg,
        "proj-xyz",
        "example/warm:latest",
        None,
        &LeasedWarmJobIdentity::new("proj-xyz", "req-1", "rev-1", 7),
    );

    for job in [&mut plain, &mut leased] {
        let name = job.metadata.name.clone();
        stamp_warm_attempt(
            job,
            "019fc384-c2d5-7460-aeed-5a168b112b03",
            "2026-08-02T17:30:00Z",
        );
        assert_eq!(
            job.metadata.name, name,
            "attempt data must not alter deterministic Job identity"
        );
        let env = job
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .and_then(|spec| spec.containers.first())
            .and_then(|container| container.env.as_ref())
            .expect("warmer env");
        assert!(
            env.iter()
                .any(|entry| entry.name == ENV_WARM_GRAPH_ATTEMPT_ID
                    && entry.value.as_deref() == Some("019fc384-c2d5-7460-aeed-5a168b112b03"))
        );
        assert!(
            env.iter()
                .any(|entry| entry.name == ENV_WARM_GRAPH_ATTEMPT_DEADLINE
                    && entry.value.as_deref() == Some("2026-08-02T17:30:00Z"))
        );
    }
}

#[test]
fn warm_pod_never_renders_a_launcher_sidecar() {
    let cfg = KubernetesConfig::for_testing();
    let plain = build_warm_job(&cfg, "proj-xyz", "deadbeef", "example/warm:latest", None);
    let leased = build_leased_warm_job(
        &cfg,
        "proj-xyz",
        "example/warm:latest",
        None,
        &LeasedWarmJobIdentity::new("proj-xyz", "req-1", "rev-1", 7),
    );

    for (label, job) in [("plain", &plain), ("leased", &leased)] {
        let pod = job
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .expect("pod spec");
        let names: Vec<&str> = pod
            .containers
            .iter()
            .chain(pod.init_containers.iter().flatten())
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            !names.contains(&crate::launcher::LAUNCHER_CONTAINER_NAME),
            "{label} warm job renders a launcher sidecar ({names:?}); the warm \
             pod runs as CHILD_UID and would now be on the WRONG side of the \
             worker-owned 0600 broker socket"
        );
        let volume_names: Vec<&str> = pod
            .volumes
            .iter()
            .flatten()
            .map(|v| v.name.as_str())
            .collect();
        assert!(
            !volume_names.contains(&crate::launcher::VOLUME_LAUNCHER_IPC),
            "{label} warm job mounts the launcher IPC volume ({volume_names:?})"
        );
    }
}

/// The worker contract merged by l15u/t6g0 acquires its per-project
/// advisory lock at `/cache/cargo-target/.warm-locks/<project-id>.lock`.
/// This manifest proof deliberately covers only the shared-filesystem and
/// fingerprint prerequisites for that contract; lock acquisition remains
/// wholly in `djinn-agent-worker`.
#[test]
fn warm_manifest_preserves_shared_cache_lock_prerequisites() {
    let mut cfg = KubernetesConfig::for_testing();
    cfg.cache_pvc = "warm-cache-pvc".into();
    cfg.warm_job_timeout_seconds = 1_237;
    cfg.warm_cpu_limit = "7".into();
    let job = build_warm_job(
        &cfg,
        "lock-project",
        "deadbeef",
        "example/warm:latest",
        None,
    );

    let spec = job.spec.as_ref().expect("job spec");
    // A non-default fixture proves this is configuration wiring rather
    // than an accidental assertion of the default timeout.
    assert_eq!(spec.active_deadline_seconds, Some(1_237));
    assert_eq!(spec.backoff_limit, Some(0));
    {
        // A non-default deadline must reach the in-Pod warm verbatim: the
        // per-step cargo budgets clamp against this value, so a stale or
        // absent projection would let a step outlive the Job.
        let projected: BTreeMap<&str, &str> = spec
            .template
            .spec
            .as_ref()
            .expect("pod spec")
            .containers
            .first()
            .expect("warm container")
            .env
            .as_ref()
            .expect("container environment")
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            projected.get("DJINN_WARM_JOB_DEADLINE_SECONDS").copied(),
            Some("1237"),
        );
    }

    let pod = spec.template.spec.as_ref().expect("pod spec");
    let container = pod.containers.first().expect("warm container");
    let mounts: BTreeMap<&str, &VolumeMount> = container
        .volume_mounts
        .as_ref()
        .expect("volume mounts")
        .iter()
        .map(|mount| (mount.name.as_str(), mount))
        .collect();
    let cache_mount = mounts
        .get(crate::job::VOLUME_CACHE)
        .expect("shared cache mount");
    assert_eq!(cache_mount.mount_path, crate::job::CACHE_MOUNT_DIR);
    assert_eq!(cache_mount.read_only, Some(false));

    let volumes: BTreeMap<&str, &Volume> = pod
        .volumes
        .as_ref()
        .expect("pod volumes")
        .iter()
        .map(|volume| (volume.name.as_str(), volume))
        .collect();
    let cache_pvc = volumes
        .get(crate::job::VOLUME_CACHE)
        .expect("shared cache volume")
        .persistent_volume_claim
        .as_ref()
        .expect("shared cache PVC");
    assert_eq!(cache_pvc.claim_name, "warm-cache-pvc");
    assert_eq!(cache_pvc.read_only, Some(false));

    let env: BTreeMap<&str, &str> = container
        .env
        .as_ref()
        .expect("container environment")
        .iter()
        .map(|entry| {
            (
                entry.name.as_str(),
                entry.value.as_deref().unwrap_or_default(),
            )
        })
        .collect();
    // The warm target and the worker's advisory-lock directory both resolve
    // under the writable cache mount. Keep the containment assertion in
    // addition to the exact target value: this is the filesystem handoff
    // the l15u/t6g0 worker contract requires, not merely matching strings.
    let target_dir = env
        .get("CARGO_TARGET_DIR")
        .copied()
        .expect("CARGO_TARGET_DIR");
    assert!(
        std::path::Path::new(target_dir).starts_with(&cache_mount.mount_path),
        "warm target {target_dir} must reside under writable cache mount {}",
        cache_mount.mount_path
    );
    assert_eq!(
        target_dir,
        format!(
            "{}/cargo-target/lock-project/mold-jobs-7",
            crate::job::CACHE_MOUNT_DIR
        )
    );
    assert_eq!(env.get("CARGO_INCREMENTAL").copied(), Some("1"));
    assert_eq!(env.get("CARGO_HOME").copied(), Some("/cache/cargo"));
    assert_eq!(env.get("RUSTC_WRAPPER").copied(), Some(""));
    assert_eq!(
        env.get("CARGO_BUILD_RUSTFLAGS").copied(),
        Some("-Clink-arg=-fuse-ld=mold -Clink-arg=-Wl,--threads=7")
    );
    assert_eq!(env.get("SQLX_OFFLINE").copied(), Some("true"));
    assert_eq!(env.get("CARGO_BUILD_JOBS").copied(), Some("7"));
    assert_eq!(env.get("NEXTEST_TEST_THREADS").copied(), Some("7"));
}

#[test]
fn warm_manifest_keys_subcore_limit_as_mold_jobs_one() {
    let mut cfg = KubernetesConfig::for_testing();
    cfg.warm_cpu_limit = "500m".into();
    let job = build_warm_job(&cfg, "mold-one", "deadbeef", "example/warm:latest", None);
    let container = &job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("pod spec")
        .containers[0];
    let env: BTreeMap<&str, &str> = container
        .env
        .as_ref()
        .expect("container environment")
        .iter()
        .map(|entry| {
            (
                entry.name.as_str(),
                entry.value.as_deref().unwrap_or_default(),
            )
        })
        .collect();

    assert_eq!(
        env.get("CARGO_TARGET_DIR").copied(),
        Some("/cache/cargo-target/mold-one/mold-jobs-1")
    );
    assert_eq!(env.get("CARGO_BUILD_JOBS").copied(), Some("1"));
    assert_eq!(env.get("NEXTEST_TEST_THREADS").copied(), Some("1"));
    assert_eq!(
        env.get("CARGO_BUILD_RUSTFLAGS").copied(),
        Some("-Clink-arg=-fuse-ld=mold -Clink-arg=-Wl,--threads=1")
    );
}

#[test]
fn sanitize_id_lowercases_and_maps_disallowed_chars() {
    assert_eq!(sanitize_id("Proj_ABC/xyz"), "proj-abc-xyz");
}

/// Warm Pods must inherit the same scheduling hints as task-runs —
/// otherwise they'd land on a different pool and the canonical-graph
/// cache they pre-populate wouldn't be reused by the task-run that
/// adopts it. The config struct carries one shared set of hints for
/// exactly this reason; this test guards the wiring through to the
/// warm-Job PodSpec.
#[test]
fn warm_pod_scheduling_propagates_from_config() {
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

    let job = build_warm_job(
        &cfg,
        "proj-xyz",
        "deadbeef",
        "reg.example:5000/djinn-project-p:abc123",
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
    assert_eq!(tols[0].effect.as_deref(), Some("NoSchedule"));
}

// ---------------------------------------------------------------------------
// AC3 (37yq): the warm Job name is deterministic in (project, warm generation)
// ---------------------------------------------------------------------------

/// Mirrors `scip_job::tests::job_name_is_deterministic_and_label_safe`
/// (`scip_job.rs`) — BOTH halves. Equality alone is satisfied by a constant, and
/// a constant name is worse than the `Uuid::now_v7()` it replaced: every warm
/// generation would collide with the previous one's object and the create-then-
/// observe adopt path would hand back a Job indexing the wrong revision.
#[test]
fn warm_job_name_is_deterministic_per_generation_and_label_safe() {
    let name = warm_job_name("Proj_ABC/xyz", "abc123def4567890");

    assert_eq!(
        name,
        warm_job_name("Proj_ABC/xyz", "abc123def4567890"),
        "same project + same warm generation must name the same object"
    );
    assert_ne!(
        name,
        warm_job_name("Proj_ABC/xyz", "ffffffffffffffffffff"),
        "a new warm generation must name a NEW object, or the adopt path \
         resurrects the previous revision's Job"
    );
    assert_ne!(
        name,
        warm_job_name("other-project", "abc123def4567890"),
        "two projects at the same revision must not share one Job"
    );
    assert!(
        crate::label_value::is_valid_label_value(&name),
        "the Job name is projected into the `job-name` label: {name}"
    );
}

/// The same property asserted on the manifest the dispatcher actually POSTs,
/// not just on the naming helper — the helper being deterministic proves
/// nothing if `build_warm_job` ignores it.
#[test]
fn build_warm_job_is_deterministic_in_project_and_generation() {
    let cfg = KubernetesConfig::for_testing();
    let name_of = |project: &str, generation: &str| {
        build_warm_job(&cfg, project, generation, "example/warm:latest", None)
            .metadata
            .name
            .expect("warm Job is built with a name")
    };

    assert_eq!(
        name_of("proj-xyz", "abc123"),
        name_of("proj-xyz", "abc123"),
        "two builds of one warm generation must produce one Job name"
    );
    assert_ne!(
        name_of("proj-xyz", "abc123"),
        name_of("proj-xyz", "def456"),
        "different warm generations must produce different Job names"
    );
}

/// The name `build_warm_job` renders is the name the durable warm identity has
/// always persisted. Before this slice they diverged: the builder produced a
/// `Uuid::now_v7()` name and the dispatch path silently overwrote it, so the
/// manifest a caller held was never the manifest Kubernetes saw.
#[test]
fn build_warm_job_agrees_with_the_durable_warm_identity_name() {
    let cfg = KubernetesConfig::for_testing();
    let revision = "9f1c2d3e4b5a60718293";
    let identity = LeasedWarmJobIdentity::new(
        "proj-xyz",
        crate::graph_warmer_identity::warm_work_id("proj-xyz", revision),
        revision,
        7,
    );

    assert_eq!(
        build_warm_job(&cfg, "proj-xyz", revision, "example/warm:latest", None)
            .metadata
            .name
            .as_deref(),
        Some(identity.object_name.as_str())
    );
}
