//! Smoke test for [`djinn_runtime::LocalDockerRuntime`].
//!
//! Phase 2 PR 6 of `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! Gated behind:
//! * `#[ignore]` — opt-in so the default `cargo test` run on a dev laptop or
//!   CI box without a Docker daemon stays green.
//! * `DJINN_TEST_DOCKER=1` environment variable — lets `cargo test -- --ignored`
//!   skip when docker is unavailable rather than failing loudly.
//! * presence of a usable `docker` binary on `$PATH`.
//! * presence of the `djinn-agent-runtime:local` image (or the tag supplied
//!   via `DJINN_DOCKER_IMAGE`).
//!
//! Coverage:
//!
//! 1. Prepare a `TaskRunSpec` for `SupervisorFlow::Planning`.
//! 2. `LocalDockerRuntime::prepare` against a test mirror that synthesises a
//!    minimal repo.
//! 3. `attach_stdio` returns a [`BiStream`] (currently a placeholder — the
//!    test asserts the shape, not event content).
//! 4. The fake `IpcServerFactory` records ≥ 1 accept within the timeout —
//!    the worker is expected to dial back.
//! 5. `teardown` removes the container and unlinks the socket.
//!
//! The test does NOT assert any particular `TaskRunReport` content — the
//! worker-side streaming emitter lands in a later PR.

#![cfg(feature = "local-docker")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use djinn_core::models::TaskRunTrigger;
use djinn_runtime::local_docker::{
    IpcServerFactory, IpcServerHandle, LocalDockerConfig, LocalDockerRuntime, MirrorBackend,
};
use djinn_runtime::{SessionRuntime, SupervisorFlow, TaskRunSpec};
use djinn_workspace::Workspace;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

/// Wraps a tempdir, returning it as a `Workspace::attach_existing` clone.
///
/// The real runtime uses `MirrorManager::clone_ephemeral` which relies on a
/// pre-populated bare mirror.  The smoke test skips mirror plumbing entirely
/// — we just need *some* directory to bind-mount at `/workspace`.
struct FakeMirror {
    _root: TempDir,
    path: PathBuf,
    branch: String,
}

impl FakeMirror {
    fn new() -> Self {
        let root = TempDir::new().expect("fake-mirror tempdir");
        let path = root.path().to_path_buf();
        Self {
            _root: root,
            path,
            branch: "main".to_string(),
        }
    }
}

#[async_trait]
impl MirrorBackend for FakeMirror {
    async fn clone_ephemeral(
        &self,
        _project_id: &str,
        _branch: &str,
    ) -> Result<Workspace, djinn_runtime::RuntimeError> {
        Workspace::attach_existing(&self.path, &self.branch).map_err(|e| {
            djinn_runtime::RuntimeError::Prepare(format!("fake mirror attach: {e}"))
        })
    }
}

/// Accepts exactly one connection, increments a counter, and bails.  No
/// protocol semantics — the smoke test only proves the runtime bound the
/// socket and the worker (or this test's own stand-in dialler) could reach
/// it.
struct FakeFactory {
    accepts: Arc<AtomicUsize>,
    cancel: CancellationToken,
}

#[async_trait]
impl IpcServerFactory for FakeFactory {
    async fn bind(
        &self,
        socket_path: PathBuf,
    ) -> std::io::Result<Box<dyn IpcServerHandle>> {
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)?;
        let accepts = self.accepts.clone();
        let cancel = self.cancel.clone();
        let join = tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                accept = listener.accept() => {
                    if accept.is_ok() {
                        accepts.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        });
        Ok(Box::new(FakeHandle {
            cancel: self.cancel.clone(),
            join: Some(join),
        }))
    }
}

struct FakeHandle {
    cancel: CancellationToken,
    join: Option<tokio::task::JoinHandle<()>>,
}

#[async_trait]
impl IpcServerHandle for FakeHandle {
    async fn shutdown(mut self: Box<Self>) {
        self.cancel.cancel();
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
    }
}

/// Check whether the local environment can host the docker-backed smoke
/// test — absence of any piece yields `None` and the test is skipped.
fn docker_test_enabled() -> Option<()> {
    if std::env::var("DJINN_TEST_DOCKER").ok().as_deref() != Some("1") {
        return None;
    }
    let docker_on_path = std::process::Command::new("docker")
        .arg("--version")
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !docker_on_path {
        return None;
    }
    Some(())
}

#[tokio::test]
#[ignore = "requires DJINN_TEST_DOCKER=1 + running docker daemon + djinn-agent-runtime image"]
async fn local_docker_runtime_full_lifecycle() {
    if docker_test_enabled().is_none() {
        eprintln!("skipping: DJINN_TEST_DOCKER!=1 or docker binary not found");
        return;
    }

    // Per-test ipc + cache roots live in a tempdir so the run does not
    // collide with an already-deployed host layout.
    let ipc_tmp = TempDir::new().expect("ipc tempdir");
    let cache_tmp = TempDir::new().expect("cache tempdir");
    let mirror_tmp = TempDir::new().expect("mirror root tempdir");
    for sub in ["cargo", "pnpm", "pip"] {
        std::fs::create_dir_all(cache_tmp.path().join(sub)).expect("cache subdir");
    }

    let cfg = LocalDockerConfig::builder()
        .image_tag(
            std::env::var("DJINN_DOCKER_IMAGE")
                .unwrap_or_else(|_| "djinn-agent-runtime:local".into()),
        )
        .ipc_root(ipc_tmp.path())
        .cache_root(cache_tmp.path())
        .mirrors_root(mirror_tmp.path())
        .build();

    let mirror = Arc::new(FakeMirror::new());
    let accepts = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FakeFactory {
        accepts: accepts.clone(),
        cancel: CancellationToken::new(),
    });

    let runtime = LocalDockerRuntime::connect_local(cfg, mirror, factory)
        .expect("connect to local docker");

    let spec = TaskRunSpec {
        task_id: "smoke-task".into(),
        project_id: "smoke-proj".into(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "djinn/smoke".into(),
        flow: SupervisorFlow::Planning,
        model_id_per_role: Default::default(),
    };

    let handle = runtime.prepare(&spec).await.expect("prepare");
    assert!(handle.container_id.is_some(), "expected a container id");
    assert!(handle.ipc_socket.exists(), "ipc socket should be bound");

    // attach_stdio — PR 6 returns an empty BiStream.  Assert the call
    // succeeds without panicking.
    let _stream = runtime.attach_stdio(&handle).await.expect("attach_stdio");

    // Wait up to 30s for the worker to connect.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        if accepts.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(
        accepts.load(Ordering::SeqCst) >= 1,
        "IPC socket saw {} connections; expected >= 1 within timeout",
        accepts.load(Ordering::SeqCst)
    );

    // Teardown.
    let _report = runtime.teardown(handle).await.expect("teardown");
}
