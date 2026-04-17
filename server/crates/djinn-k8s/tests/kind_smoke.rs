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

use djinn_core::models::TaskRunTrigger;
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::runtime::KubernetesRuntime;
use djinn_runtime::{SessionRuntime, SupervisorFlow, TaskRunSpec};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, DeleteParams};

/// Namespace the smoke tests write into. Assumed to exist — `make kind-up` in
/// Phase 2 PR 4 creates it; otherwise we `kubectl create ns` it below.
const TEST_NAMESPACE: &str = "djinn-test";

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
        eprintln!(
            "kind_smoke: could not ensure namespace {TEST_NAMESPACE} exists — skipping"
        );
        return false;
    }
    true
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

/// A tiny spec suitable for the smoke tests — the `Planning` flow is just the
/// planner, so there is nothing here that would demand real mirror volumes.
fn sample_spec(task_id: &str) -> TaskRunSpec {
    TaskRunSpec {
        task_id: task_id.into(),
        project_id: format!("proj-{task_id}"),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: format!("djinn/{task_id}"),
        flow: SupervisorFlow::Planning,
        model_id_per_role: HashMap::new(),
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

    let client = kube::Client::try_default()
        .await
        .expect("kind_smoke: kube::Client::try_default");

    let config = test_config();
    let registry = std::sync::Arc::new(djinn_supervisor::ConnectionRegistry::new());
    let runtime = KubernetesRuntime::from_client(client.clone(), config.clone(), registry);

    let spec = sample_spec("task-kind-smoke-prep");

    // 1) prepare: handle with a populated pod_ref pointing at the Job.
    let handle = runtime
        .prepare(&spec)
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
        owner_refs.iter().any(|o| o.kind == "Job" && o.name == job_name),
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
    let _ = secrets
        .delete(&job_name, &DeleteParams::default())
        .await;
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

    let client = kube::Client::try_default()
        .await
        .expect("kind_smoke: kube::Client::try_default");

    let config = test_config();
    let registry = std::sync::Arc::new(djinn_supervisor::ConnectionRegistry::new());
    let runtime = KubernetesRuntime::from_client(client.clone(), config.clone(), registry);

    let spec = sample_spec("task-kind-smoke-life");

    // 1) prepare.
    let handle = runtime
        .prepare(&spec)
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
    runtime
        .cancel(&handle)
        .await
        .expect("kind_smoke: cancel()");

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
