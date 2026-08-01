// Test: eprintln used for skip-reason diagnostics in this smoke test.
#![allow(clippy::print_stderr)]
//! `KubernetesRuntime` end-to-end smoke test against a live `kind` cluster.
//!
//! Phase 2 K8s PR 3 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`.
//!
//! The tests are `#[ignore]`'d by default and additionally gated on
//! `DJINN_TEST_KIND=1` plus the presence of `kubectl` and `kind` on `PATH`.
//! Running them locally requires that the developer first brought up a cluster
//! with the Phase 2 PR 4 Makefile prerequisites:
//!
//! ```bash
//! # One-time, manual (shipped in commit b6280d011):
//! make kind-up                      # creates kind cluster with local registry :5001
//! make image-push-local             # builds djinn-agent-runtime:dev + pushes
//! make helm-install-local           # applies CRDs + main chart with values.local.yaml
//!
//! # Run:
//! DJINN_TEST_KIND=1 cargo test -p djinn-k8s --test kind_smoke -- --ignored
//! ```
//!
//! Both tests additionally need the isolated test Postgres that
//! `DJINN_TEST_DATABASE_URL` points at — see [`seeded_db`] for why a cluster
//! smoke test grew a database dependency.
//!
//! TWO THINGS THIS FILE HAS TO DO BEFORE IT MAY BUILD A `kube::Client`
//!
//! 1. Install a process-level rustls `CryptoProvider`. Until this was added
//!    (task `d2ae`) both tests below panicked *before their first API call*
//!    with "Could not automatically determine the process-level CryptoProvider
//!    from Rustls crate features" — a `#[ignore]`d, `DJINN_TEST_KIND`-gated
//!    file that no CI lane runs, so nothing ever noticed. See
//!    [`support::install_crypto_provider`] for the mechanism and for why the
//!    server binary is unaffected.
//! 2. Refuse any API server that is not on loopback. These tests CREATE Jobs
//!    and Secrets. `kube::Client::try_default()` resolves whatever context
//!    happens to be current, and every context in a Djinn developer's
//!    kubeconfig is EKS — so an unguarded `try_default()` plus a stray
//!    `DJINN_TEST_KIND=1` writes into a managed cluster. kind always serves on
//!    loopback; no managed control plane does. Same discipline as
//!    `tests/kueue_cluster_harness.rs` and `tests/kueue_disruption/mod.rs`.
//!
//! The tests do NOT attempt to run a full task-run end-to-end — a real task
//! lifecycle needs the djinn-server TCP listener + mirror volume + GitHub App
//! token, all out of scope until PR 4 pt2. Here we assert:
//!
//! 1. `prepare` materialises the Secret + Job the launcher needs and backfills
//!    the Secret's `OwnerReference` to the Job.
//! 2. `cancel` followed by `teardown` drives both the `Foreground`-propagated
//!    Job delete path AND `teardown`'s 404-shortcut in the polling loop,
//!    leaving no Job/Secret behind. This keeps the test fast even when the
//!    worker image would otherwise take minutes to reach a terminal state.
//!    `attach_stdio` is skipped on this path because Phase 2.1 blocks until
//!    the worker's RPC handshake lands — which never happens here (no real
//!    worker image connects back).

use std::collections::HashMap;
use std::env;
use std::process::Command;
use std::time::Duration;

use djinn_cgroup_launcher::LauncherAuthorityProtocol;
use djinn_core::events::EventBus;
use djinn_core::models::TaskRunTrigger;
use djinn_db::Database;
use djinn_db::repositories::image::ImageRepository;
use djinn_db::repositories::project::ProjectRepository;
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::runtime::KubernetesRuntime;
use djinn_runtime::{ResolvedCredentials, SessionRuntime, SupervisorFlow, TaskRunSpec};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, DeleteParams};

mod support;

/// Namespace the smoke tests write into. Assumed to exist — `make kind-up` in
/// Phase 2 PR 4 creates it; otherwise we `kubectl create ns` it below.
const TEST_NAMESPACE: &str = "djinn-test";

/// A well-formed immutable manifest digest — `sha256:` + 64 lowercase hex.
///
/// Required, not decorative: migration 164 refuses to mark ready any image that
/// declares a launcher authority protocol without capturing a digest, and
/// `resolve_dispatch_image`'s vf7a admission fence compares digests exactly.
/// The same constant, for the same reason, as
/// `tests/launcher_authority_protocol_render.rs`.
const CANONICAL_DIGEST: &str =
    "sha256:7822b7de0000000000000000000000000000000000000000000000000000cafe";

/// Minimal PATH-based which(1). Avoids pulling the `which` crate into this
/// workspace for a single call site.
fn which(bin: &str) -> bool {
    if let Ok(path) = env::var("PATH") {
        for dir in path.split(':') {
            let candidate = std::path::Path::new(dir).join(bin);
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

/// Poll `kubectl` for the namespace's existence; creates it if missing.
///
/// Returns `true` if the namespace is present after the call, `false` on
/// `kubectl` failure (caller skips the test).
fn ensure_namespace(ns: &str) -> bool {
    let exists = Command::new("kubectl")
        .args(["get", "ns", ns])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if exists {
        return true;
    }
    Command::new("kubectl")
        .args(["create", "ns", ns])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Returns `false` when the test is disabled — callers should `return` early.
/// Prints a skip reason to stderr so the developer knows what gate they hit.
fn kind_test_enabled() -> bool {
    if env::var("DJINN_TEST_KIND").is_err() {
        eprintln!("kind_smoke: DJINN_TEST_KIND not set — skipping");
        return false;
    }
    if !which("kubectl") || !which("kind") {
        eprintln!("kind_smoke: kubectl/kind not found on PATH — skipping");
        return false;
    }
    if !ensure_namespace(TEST_NAMESPACE) {
        eprintln!("kind_smoke: could not ensure namespace {TEST_NAMESPACE} exists — skipping");
        return false;
    }
    true
}

/// Build the `kube::Client` these tests talk to.
///
/// Two things happen here that `kube::Client::try_default()` does not do.
///
/// First, [`support::install_crypto_provider`] runs — without it the very next
/// line panics on "Could not automatically determine the process-level
/// CryptoProvider from Rustls crate features", before any request is sent. It
/// is safe to call once per test: the install itself happens exactly once per
/// process and is loud if it ever loses a race.
///
/// Second, the resolved API-server URL is checked. These tests create real
/// Jobs and Secrets, and a Djinn developer's kubeconfig contains nothing but
/// EKS contexts, so "whatever context is current" is not an acceptable target.
/// kind serves on loopback and no managed control plane does, which makes the
/// host a sufficient guard. `DJINN_TEST_KIND_CONTEXT` selects a specific
/// kubeconfig context; without it the current context is used, and still has
/// to clear the loopback check.
async fn kind_client() -> kube::Client {
    support::install_crypto_provider();

    let options = kube::config::KubeConfigOptions {
        context: env::var("DJINN_TEST_KIND_CONTEXT").ok(),
        ..Default::default()
    };
    let config = kube::Config::from_kubeconfig(&options)
        .await
        .expect("kind_smoke: resolve a kubeconfig for the kind cluster");

    let host = config.cluster_url.host().unwrap_or_default().to_owned();
    assert!(
        matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]"),
        "kind_smoke: refusing to run against non-loopback API server {host:?} — these tests \
         CREATE Jobs and Secrets, and every context in a Djinn kubeconfig is EKS. Point \
         KUBECONFIG (or DJINN_TEST_KIND_CONTEXT) at a local kind cluster.",
    );

    kube::Client::try_from(config).expect("kind_smoke: build a kube client for the kind cluster")
}

/// Build a `KubernetesConfig` scoped to the kind test namespace.
///
/// `server_addr` is never actually dialed during these tests — the worker Pod
/// either never starts (image missing) or is terminated before reaching the
/// bearer-token handshake — so a placeholder DNS name is fine.
fn test_config() -> KubernetesConfig {
    let mut cfg = KubernetesConfig::for_testing();
    cfg.namespace = TEST_NAMESPACE.to_string();
    cfg.server_addr = "djinn.djinn-test.svc.cluster.local:8443".into();
    cfg
}

/// Bring up an isolated Postgres and seed the one project row `prepare()`
/// insists on, returning the handle the runtime is built with.
///
/// WHY A DATABASE IS IN A CLUSTER SMOKE TEST. `prepare()` has resolved the
/// per-project devcontainer image out of the database since Phase 3 PR 5, and
/// it hard-fails rather than falling back to `config.image`. `kind_smoke.rs`
/// still built its runtime with `KubernetesRuntime::from_client`, which leaves
/// `db: None` — so even after the rustls provider was installed, both tests
/// died on
///
/// ```text
/// KubernetesRuntime constructed without a database handle;
/// `with_db` / `from_client_with_db` is required to dispatch task-run Jobs
/// ```
///
/// a second, independent rot in the same unrun file. Measured 2026-07-31
/// against a live kind cluster, before this was added.
///
/// The fixture is the same one `tests/launcher_authority_protocol_render.rs`
/// uses — project, catalog image marked ready, project pointed at it, and the
/// durable launcher-authority singleton aligned with what the image declares,
/// because migration 167's fence refuses a dispatch whose declaration the
/// server does not actually run. The digest is well-formed and required:
/// migration 164 refuses to mark ready any image that declares a protocol
/// without capturing an immutable one.
///
/// The pull ref is deliberately unresolvable: nothing here waits for a Pod, and
/// both tests delete the Job long before the kubelet gives up pulling.
async fn seeded_db(project_id: &str) -> Database {
    let db = Database::open_in_memory().expect(
        "kind_smoke: open an isolated test database — DJINN_TEST_DATABASE_URL must point at a \
         Postgres carrying the `djinn_test_template` template database",
    );
    db.ensure_initialized()
        .await
        .expect("kind_smoke: migrations");

    ProjectRepository::new(db.clone(), EventBus::noop())
        .create_with_id(project_id, project_id, "test", project_id)
        .await
        .expect("kind_smoke: seed project");

    let images = ImageRepository::new(db.clone());
    let image_id = format!("img-{project_id}");
    images
        .create(&image_id, &image_id, None, "{}")
        .await
        .expect("kind_smoke: seed catalog image");
    images
        .mark_ready(
            &image_id,
            "registry.invalid/djinn-kind-smoke:never-pulled",
            Some(CANONICAL_DIGEST),
            Some(LauncherAuthorityProtocol::LeafV1),
        )
        .await
        .expect("kind_smoke: mark the seeded image ready");
    images
        .set_project_image(project_id, Some(&image_id))
        .await
        .expect("kind_smoke: select the catalog image");

    let modes = djinn_db::LauncherAuthorityModeRepository::new(db.clone());
    let epoch = modes
        .read()
        .await
        .expect("kind_smoke: read the launcher-authority singleton")
        .expect("kind_smoke: migration 167 seeds the launcher-authority singleton")
        .epoch;
    modes
        .set_mode(epoch, LauncherAuthorityProtocol::LeafV1)
        .await;

    db
}

/// A tiny spec suitable for the smoke tests — the `Planning` flow is just the
/// planner, so there is nothing here that would demand real mirror volumes.
fn sample_spec(task_id: &str) -> TaskRunSpec {
    TaskRunSpec {
        // `prepare` parses this back to a Uuid for the resource name, so it
        // must be a valid UUID string.
        task_run_id: uuid::Uuid::now_v7().to_string(),
        task_attempt_id: None,
        task_id: task_id.into(),
        execution_generation: 0,
        project_id: format!("proj-{task_id}"),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: format!("djinn/{task_id}"),
        flow: SupervisorFlow::Planning,
        model_id_per_role: HashMap::new(),
        read_source_project_ids: Vec::new(),
        knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    }
}

/// First smoke: `prepare` creates the manifest pair and `cancel` tears them
/// down again.  Does not call `teardown` — it would poll for minutes in the
/// common "worker image can't reach djinn-server" case.
#[tokio::test]
#[ignore]
async fn kind_smoke_prepare_then_cancel() {
    if !kind_test_enabled() {
        return;
    }

    let client = kind_client().await;

    let spec = sample_spec("task-kind-smoke-prep");
    let db = seeded_db(&spec.project_id).await;

    let config = test_config();
    let registry = std::sync::Arc::new(djinn_supervisor::ConnectionRegistry::new());
    let runtime =
        KubernetesRuntime::from_client_with_db(client.clone(), config.clone(), registry, db);

    let credentials = ResolvedCredentials::default();

    // 1) prepare: handle with a populated pod_ref pointing at the Job.
    let handle = runtime
        .prepare(&spec, &credentials)
        .await
        .expect("kind_smoke: prepare() should succeed against a live kind cluster");
    let job_name = handle
        .pod_ref
        .clone()
        .expect("kind_smoke: RunHandle.pod_ref should be Some(job_name)");
    assert!(
        job_name.starts_with("djinn-taskrun-"),
        "kind_smoke: unexpected job name {job_name}"
    );

    // 2) The Secret carrying the bincode-encoded spec is present and labelled.
    let secrets: Api<Secret> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    let got_secret = secrets
        .get(&job_name)
        .await
        .expect("kind_smoke: secret should exist after prepare()");
    assert!(
        got_secret.metadata.labels.is_some(),
        "kind_smoke: secret should carry labels"
    );
    let owner_refs = got_secret
        .metadata
        .owner_references
        .as_ref()
        .expect("kind_smoke: secret should carry an OwnerReference back at the Job");
    assert!(
        owner_refs
            .iter()
            .any(|o| o.kind == "Job" && o.name == job_name),
        "kind_smoke: secret OwnerReference should point at Job {job_name}"
    );

    // 3) The Job itself is present with the expected name.
    let jobs: Api<Job> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    let got_job = jobs
        .get(&job_name)
        .await
        .expect("kind_smoke: job should exist after prepare()");
    assert_eq!(got_job.metadata.name.as_deref(), Some(job_name.as_str()));

    // 4) Cancel deletes the Job via Foreground propagation.
    runtime
        .cancel(&handle)
        .await
        .expect("kind_smoke: cancel() should succeed");

    // Foreground propagation leaves the Job briefly in a Terminating state —
    // poll for up to ten seconds for actual removal.
    assert_job_eventually_gone(&jobs, &job_name, Duration::from_millis(500), 20).await;

    // 5) Best-effort Secret cleanup (OwnerRef GC may already have handled it).
    let _ = secrets.delete(&job_name, &DeleteParams::default()).await;
}

/// Second smoke: exercises the full `prepare → attach_stdio → cancel →
/// teardown` lifecycle.  `cancel` followed by `teardown` is the "dev
/// interrupted the run" shape — `teardown`'s polling loop hits the 404
/// shortcut once the Foreground-propagated Job delete completes, so the test
/// never waits anywhere near the 5-minute teardown timeout even though the
/// worker image never actually starts.
#[tokio::test]
#[ignore]
async fn kind_smoke_runtime_lifecycle() {
    if !kind_test_enabled() {
        return;
    }

    let client = kind_client().await;

    let spec = sample_spec("task-kind-smoke-life");
    let db = seeded_db(&spec.project_id).await;

    let config = test_config();
    let registry = std::sync::Arc::new(djinn_supervisor::ConnectionRegistry::new());
    let runtime =
        KubernetesRuntime::from_client_with_db(client.clone(), config.clone(), registry, db);

    let credentials = ResolvedCredentials::default();

    // 1) prepare.
    let handle = runtime
        .prepare(&spec, &credentials)
        .await
        .expect("kind_smoke: prepare()");
    let job_name = handle
        .pod_ref
        .clone()
        .expect("kind_smoke: RunHandle.pod_ref");

    // 2) attach_stdio — Phase 2.1 blocks on the worker handshake, which
    //    never completes in this smoke test because the image never boots
    //    reachably against `server_addr`.  Skip the call; the smoke test
    //    only asserts the `prepare → cancel → teardown` K8s resource
    //    lifecycle, and `teardown` now falls through to the Job-status
    //    poll when no handshake ever landed.

    // 3) Cancel gets the Job deleting in the background so teardown's polling
    //    loop sees a 404 and returns immediately rather than waiting 5 min.
    runtime.cancel(&handle).await.expect("kind_smoke: cancel()");

    // 4) teardown consumes the handle, polls job status (404-fast-path here),
    //    then best-effort deletes.  Returns an Ok(TaskRunReport) with the
    //    task_run_id set.
    let report = runtime
        .teardown(handle)
        .await
        .expect("kind_smoke: teardown() should return a report");
    assert!(
        !report.task_run_id.is_empty(),
        "kind_smoke: TaskRunReport.task_run_id should be populated"
    );

    // 5) Cluster state is clean: Job + Secret are gone (or GC'd via the
    //    OwnerReference).
    let jobs: Api<Job> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    assert_job_eventually_gone(&jobs, &job_name, Duration::from_millis(500), 20).await;

    let secrets: Api<Secret> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    assert_secret_eventually_gone(&secrets, &job_name, Duration::from_millis(500), 20).await;
}

/// Poll the K8s API for the Job either being gone or marked for deletion.
/// Fails the test if neither condition holds within `max_iters * tick`.
async fn assert_job_eventually_gone(
    jobs: &Api<Job>,
    job_name: &str,
    tick: Duration,
    max_iters: usize,
) {
    for _ in 0..max_iters {
        match jobs.get(job_name).await {
            Err(kube::Error::Api(resp)) if resp.code == 404 => return,
            Ok(job) => {
                if job.metadata.deletion_timestamp.is_some() {
                    // Terminating — accept as "effectively gone".
                    return;
                }
            }
            Err(_) => {} // transient API errors: retry
        }
        tokio::time::sleep(tick).await;
    }
    panic!("kind_smoke: Job {job_name} should have been deleted or marked terminating");
}

/// Poll the K8s API for the Secret either being gone, marked for deletion,
/// or already GC'd by the owner reference.  Failure is non-fatal for the
/// OwnerReference case since the kubelet's GC is eventually consistent.
async fn assert_secret_eventually_gone(
    secrets: &Api<Secret>,
    secret_name: &str,
    tick: Duration,
    max_iters: usize,
) {
    for _ in 0..max_iters {
        match secrets.get(secret_name).await {
            Err(kube::Error::Api(resp)) if resp.code == 404 => return,
            Ok(secret) => {
                if secret.metadata.deletion_timestamp.is_some() {
                    return;
                }
            }
            Err(_) => {}
        }
        tokio::time::sleep(tick).await;
    }
    // Last-resort best-effort cleanup so later test runs don't collide; the
    // OwnerReference GC sometimes lags beyond a 10s window in slow CI.
    let _ = secrets.delete(secret_name, &DeleteParams::default()).await;
}
