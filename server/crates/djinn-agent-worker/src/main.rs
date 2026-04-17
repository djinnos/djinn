//! `djinn-agent-worker` — the binary the `LocalDockerRuntime` launches inside
//! the per-task-run container.
//!
//! Phase 2 PR 4 of `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! ## What this binary does today (PR 4)
//!
//! 1. Parse CLI args (`--ipc-socket`).
//! 2. Read a bincode-serialized [`TaskRunSpec`] from stdin.
//! 3. Dial the launcher's Unix-domain socket so the wire is plumbed through.
//! 4. Attach to the bind-mounted `/workspace` the host handed us
//!    (`Workspace::attach_existing`) — no re-clone inside the container.
//! 5. Construct [`StubRpcServices`] wrapped in `Arc<dyn SupervisorServices>`
//!    to pin the supervisor's dispatch shape.
//! 6. Invoke `services.load_task(spec.task_id)` as a **placeholder** drive —
//!    today this hits `unimplemented!()` inside the stub, which is the
//!    correct behaviour for a scaffold PR: it proves the binary links, the
//!    stdin decode works, the socket dial works, and the supervisor surface
//!    is reachable.  PR 5 replaces this with the real
//!    `TaskRunSupervisor::new(...).run(spec).await`.
//! 7. Emit the [`TaskRunReport`] frame on stderr as length-prefixed bincode
//!    so the host-side launcher can observe it without conflating with
//!    whatever structured logs land on stdout later.
//!
//! ## What this binary does NOT do yet
//!
//! * No real RPC codec — `ipc.rs` is a placeholder length-prefixed frame
//!   helper; PR 5 introduces `ServiceRpcRequest` / `ServiceRpcResponse`.
//! * No Docker image wiring — PR 6.
//! * Nothing in the production dispatch path launches this binary — PR 6/7.
//!
//! ## Why we don't depend on `djinn-agent`
//!
//! The worker lives behind an RPC boundary; linking `djinn-agent` would drag
//! in the whole actor framework, coordinator, LSP manager, etc. — the exact
//! surface we're trying to host-side.  Only `djinn-supervisor` +
//! `djinn-runtime` + `djinn-workspace` + `djinn-core` cross the boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use djinn_runtime::{RoleKind, TaskRunOutcome, TaskRunReport, TaskRunSpec};
use djinn_supervisor::{StubRpcServices, SupervisorServices};
use djinn_workspace::Workspace;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod ipc;
mod stdio;

/// Command-line arguments for the worker binary.
#[derive(Debug, Parser)]
#[command(
    name = "djinn-agent-worker",
    about = "In-container task-run supervisor (Phase 2 PR 4 scaffold)"
)]
struct WorkerArgs {
    /// Path to the Unix-domain socket the host-side launcher is listening
    /// on.  The worker dials this socket and (in PR 5+) speaks bincode RPC
    /// over it for `SupervisorServices`.
    ///
    /// Also accepts `DJINN_IPC_SOCKET` from the environment — the launcher
    /// always sets this inside the container, but the flag is the canonical
    /// form for unit-level invocations.
    #[arg(long, env = "DJINN_IPC_SOCKET")]
    ipc_socket: PathBuf,

    /// Path the host bind-mounted `/workspace` at.  Defaults to the
    /// contractual `/workspace` — exposed as a flag so tests can run the
    /// binary outside a container against a tempdir.
    #[arg(long, env = "DJINN_WORKSPACE_PATH", default_value = "/workspace")]
    workspace_path: PathBuf,
}

#[tokio::main]
async fn main() {
    let exit = run().await;
    match exit {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            error!(error = %format!("{e:#}"), "djinn-agent-worker failed");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();

    let args = WorkerArgs::parse();
    info!(socket = %args.ipc_socket.display(), workspace = %args.workspace_path.display(), "worker starting");

    // 1. Slurp the TaskRunSpec off stdin.
    let mut stdin = tokio::io::stdin();
    let spec: TaskRunSpec = stdio::read_bincode_from_stdin(&mut stdin)
        .await
        .context("read TaskRunSpec from stdin")?;
    info!(task_id = %spec.task_id, flow = ?spec.flow, "received spec");

    // 2. Dial the launcher's Unix socket so the wire is proven end-to-end.
    //    PR 5 hands this stream to the RPC codec; PR 4 just holds it open
    //    to verify the launcher is reachable.
    let ipc = UnixStream::connect(&args.ipc_socket)
        .await
        .with_context(|| format!("dial IPC socket at {}", args.ipc_socket.display()))?;
    info!("dialed IPC socket");

    // 3. Attach to the host-materialised workspace.
    let workspace = Workspace::attach_existing(args.workspace_path.as_path(), &spec.task_branch)
        .context("attach workspace")?;
    info!(path = %workspace.path().display(), branch = %workspace.branch(), "workspace attached");

    // 4. Build the stub supervisor services.  Wrap in `Arc<dyn ...>` to mirror
    //    the dispatch shape `TaskRunSupervisor::new` will consume in PR 5.
    let services: Arc<dyn SupervisorServices> = Arc::new(StubRpcServices::new());

    // 5. Drive — today this panics through `unimplemented!()` inside
    //    `StubRpcServices::load_task`.  That's intentional for PR 4: it
    //    proves the binary reaches the supervisor surface even though the
    //    real `TaskRunSupervisor::new(...).run(spec).await` isn't wired until
    //    the RPC codec lands in PR 5.
    let report = drive_placeholder(&services, &spec)
        .await
        .context("placeholder supervisor drive")?;

    // 6. Emit the terminal report as a bincode frame on stderr.  stdout is
    //    reserved for future structured logs; PR 5 may move the report onto
    //    the IPC socket entirely.
    let mut stderr = tokio::io::stderr();
    ipc::write_frame(&mut stderr, &report)
        .await
        .context("emit TaskRunReport frame on stderr")?;
    stderr.flush().await.ok();

    drop(ipc); // keep the name in scope above so the socket stays alive through drive()
    drop(workspace);
    Ok(())
}

/// Placeholder driver — calls `services.load_task` to exercise the trait
/// surface and return a synthetic report.  Replaced by
/// `TaskRunSupervisor::new(...).run(spec).await` in PR 5.
///
/// Today this panics via `unimplemented!()` inside `StubRpcServices`; the
/// integration test in `tests/worker_smoke.rs` asserts the panic path
/// surfaces as a non-zero exit.
async fn drive_placeholder(
    services: &Arc<dyn SupervisorServices>,
    spec: &TaskRunSpec,
) -> Result<TaskRunReport> {
    let _task = services
        .load_task(spec.task_id.clone())
        .await
        .map_err(|e| anyhow::anyhow!("load_task: {e}"))?;

    // Unreachable until PR 5 replaces the stub — preserved so the function
    // signature is honest about returning a `TaskRunReport`.
    Ok(TaskRunReport {
        task_run_id: String::new(),
        outcome: TaskRunOutcome::Interrupted,
        stages_completed: Vec::<RoleKind>::new(),
    })
}

/// Compile-time sanity: the path the worker contract publishes to the
/// container image must be a valid `&Path` literal.  Catches typos in the
/// default workspace path without a runtime surprise.
#[allow(dead_code)]
const _CONTRACT_WORKSPACE: &str = "/workspace";
#[allow(dead_code)]
fn _assert_contract_workspace_path() -> &'static Path {
    Path::new(_CONTRACT_WORKSPACE)
}
