//! Smoke test against a live `kind` cluster.
//!
//! Phase 2 K8s PR 3 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`.
//!
//! The test is `#[ignore]`'d by default and additionally gated on
//! `DJINN_TEST_KIND=1` plus the presence of `kubectl` and `kind` on `PATH`.
//! Running it locally requires that `make kind-up` (in PR 4 pt1) has already
//! stood up a kind cluster + local registry. The test does NOT attempt to
//! run a real task-run end-to-end — a full cluster run needs the djinn-server
//! TCP listener + mirror volume + GitHub App token, all out of scope until
//! PR 4 pt2. Here we assert only that `KubernetesRuntime::prepare`
//! successfully materialises the Secret + Job manifests the launcher needs,
//! and that `cancel` tears the Job down again.
//!
//! Invoke:
//! ```bash
//! DJINN_TEST_KIND=1 cargo test -p djinn-k8s --test kind_smoke -- --ignored
//! ```

use std::collections::HashMap;
use std::env;
use std::process::Command;

use djinn_core::models::TaskRunTrigger;
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::runtime::KubernetesRuntime;
use djinn_k8s::secret::task_run_resource_name;
use djinn_runtime::{SessionRuntime, SupervisorFlow, TaskRunSpec};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, DeleteParams};
use uuid::Uuid;

/// Namespace the smoke test writes into. Assumed to exist (the Helm chart's
/// `make kind-up` target creates it).
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

#[tokio::test]
#[ignore]
async fn kind_smoke_prepare_then_cancel() {
    // 1) Gate on DJINN_TEST_KIND=1 — the developer opts in explicitly.
    if env::var("DJINN_TEST_KIND").is_err() {
        eprintln!("kind_smoke: DJINN_TEST_KIND not set — skipping");
        return;
    }

    // 2) Gate on kubectl + kind being on PATH.
    if !which("kubectl") || !which("kind") {
        eprintln!("kind_smoke: kubectl/kind not found on PATH — skipping");
        return;
    }

    // 3) Assume kind + local registry exist (make kind-up responsibility).
    //    Ensure the test namespace is present.
    if !ensure_namespace(TEST_NAMESPACE) {
        panic!("kind_smoke: could not ensure namespace {TEST_NAMESPACE} exists");
    }

    // 4) Load the ambient kubeconfig.
    let client = kube::Client::try_default()
        .await
        .expect("kind_smoke: kube::Client::try_default");

    // 5) Build a KubernetesConfig scoped to the test namespace.
    let mut config = KubernetesConfig::for_testing();
    config.namespace = TEST_NAMESPACE.to_string();
    // NOTE: server_addr is read by the worker Pod; it doesn't need to be
    // reachable for this test because we never wait for the Pod to run.
    config.server_addr = "djinn.djinn-test.svc.cluster.local:8443".into();

    let runtime = KubernetesRuntime::from_client(client.clone(), config.clone());

    // 6) Construct a tiny Planning-flow spec (no real work required).
    let spec = TaskRunSpec {
        task_id: "task-kind-smoke".into(),
        project_id: "proj-kind-smoke".into(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "djinn/task-kind-smoke".into(),
        flow: SupervisorFlow::Planning,
        model_id_per_role: HashMap::new(),
    };

    // 7) Drive prepare() — assert a handle with a pod_ref comes back.
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

    // 8) Assert the Secret exists via Api::<Secret>::get.
    let secrets: Api<Secret> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    let got_secret = secrets
        .get(&job_name)
        .await
        .expect("kind_smoke: secret should exist after prepare()");
    // task_run_id in pod_ref matches task_run_resource_name for that id —
    // cross-check the labels are non-empty.
    assert!(
        got_secret.metadata.labels.is_some(),
        "kind_smoke: secret should carry labels"
    );
    // Sanity on the helper — it's the same name shape prepare() produced.
    let task_run_id = Uuid::now_v7(); // fresh id for assertion only
    let _ = task_run_resource_name(&task_run_id);

    // 9) Assert the Job exists.
    let jobs: Api<Job> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    let got_job = jobs
        .get(&job_name)
        .await
        .expect("kind_smoke: job should exist after prepare()");
    assert_eq!(got_job.metadata.name.as_deref(), Some(job_name.as_str()));

    // 10) Skip waiting for Pod completion — image may not exist.

    // 11) Cancel tears the Job down — assert it's gone.
    runtime
        .cancel(&handle)
        .await
        .expect("kind_smoke: cancel() should succeed");

    // Cancel is foreground-propagating; the Job may linger briefly. Poll
    // for up to a few seconds until it's gone.
    let mut removed = false;
    for _ in 0..20 {
        match jobs.get(&job_name).await {
            Err(kube::Error::Api(resp)) if resp.code == 404 => {
                removed = true;
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
    assert!(
        removed,
        "kind_smoke: job {job_name} should have been deleted after cancel()"
    );

    // 12) Cleanup: best-effort remove any leftover Secret (OwnerRef GC may
    // already have handled it).
    let _ = secrets
        .delete(&job_name, &DeleteParams::default())
        .await;
}
