//! `djinn-agent-worker` — the binary the `LocalDockerRuntime` launches inside
//! the per-task-run container.
//!
//! Phase 2 PR 5 of `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! ## What this binary does (PR 5)
//!
//! 1. Parse CLI args (`--ipc-socket`).
//! 2. Read a bincode-serialized [`TaskRunSpec`] from stdin.
//! 3. Dial the launcher's Unix-domain socket and spin up a real
//!    [`RpcServices`] — reader/writer tasks + correlation-id tracking —
//!    so every `SupervisorServices` call the in-container supervisor makes
//!    goes over the wire as a bincode frame.
//! 4. Attach to the bind-mounted `/workspace` the host handed us
//!    (`Workspace::attach_existing`) — no re-clone inside the container.
//! 5. Invoke `services.load_task(spec.task_id)` to prove the full
//!    request/reply round-trip works end-to-end.  A future PR swaps this
//!    placeholder driver for the full `TaskRunSupervisor::new(...).run(spec)`
//!    (the supervisor needs a real `TaskRunRepository` + `MirrorManager`
//!    which we won't plumb into the worker until PR 6/7).
//! 6. Emit the [`TaskRunReport`] frame on stderr as length-prefixed bincode
//!    so the host-side launcher can observe it without conflating with
//!    whatever structured logs land on stdout later.
//!
//! ## What this binary does NOT do yet
//!
//! * No `TaskRunSupervisor::run` drive — that needs `TaskRunRepository` and
//!   `MirrorManager`, which PR 6/7 wires up through `SessionRuntime`.
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
use djinn_supervisor::{RpcServices, SupervisorServices};
use djinn_workspace::Workspace;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod ipc;
mod stdio;

/// Command-line arguments for the worker binary.
#[derive(Debug, Parser)]
#[command(
    name = "djinn-agent-worker",
    about = "In-container task-run supervisor (Phase 2 PR 5 — real RPC wire)"
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

    // 2. Dial the launcher's Unix socket and build the real RpcServices.
    //    `RpcServices::connect` spawns the reader / writer tasks that drive
    //    the bincode frame codec; every SupervisorServices trait call from
    //    here on is a round-trip over that socket.
    let cancel = CancellationToken::new();
    let (rpc, background) = RpcServices::connect(&args.ipc_socket, cancel.clone())
        .await
        .with_context(|| format!("dial IPC socket at {}", args.ipc_socket.display()))?;
    info!("dialed IPC socket");

    // 3. Attach to the host-materialised workspace.
    let workspace = Workspace::attach_existing(args.workspace_path.as_path(), &spec.task_branch)
        .context("attach workspace")?;
    info!(path = %workspace.path().display(), branch = %workspace.branch(), "workspace attached");

    // 4. Wrap the RpcServices as `Arc<dyn SupervisorServices>` — the shape
    //    `TaskRunSupervisor::new` consumes.  PR 6/7 will hand this `Arc` to
    //    a real supervisor that also owns a `TaskRunRepository` + `MirrorManager`.
    let services: Arc<dyn SupervisorServices> = rpc.clone();

    // 5. Drive — today just a `load_task` round-trip.  The full
    //    `TaskRunSupervisor::new(...).run(spec).await` requires DB / mirror
    //    handles that live host-side; PR 6/7 wires those in.
    let report = drive_placeholder(&services, &spec)
        .await
        .context("placeholder supervisor drive")?;

    // 6. Emit the terminal report as a bincode frame on stdout.  Tracing
    //    logs stay on stderr so test harnesses can decode the report byte
    //    stream without stripping log lines first.  A later PR may move the
    //    report onto the IPC socket entirely.
    let mut stdout = tokio::io::stdout();
    ipc::write_frame(&mut stdout, &report)
        .await
        .context("emit TaskRunReport frame on stdout")?;
    stdout.flush().await.ok();

    // 7. Shut down the RPC background tasks cleanly.
    cancel.cancel();
    let _ = background.reader.await;
    let _ = background.writer.await;

    drop(workspace);
    Ok(())
}

/// Placeholder driver — calls `services.load_task` through the real RPC
/// wire to prove the round-trip works, then synthesises an `Interrupted`
/// report.  Replaced by `TaskRunSupervisor::new(...).run(spec).await` in
/// PR 6/7 once the supervisor can be instantiated with a `TaskRunRepository`
/// and `MirrorManager` that reach the host side over the same RPC.
async fn drive_placeholder(
    services: &Arc<dyn SupervisorServices>,
    spec: &TaskRunSpec,
) -> Result<TaskRunReport> {
    let task = services
        .load_task(spec.task_id.clone())
        .await
        .map_err(|e| anyhow::anyhow!("load_task: {e}"))?;
    info!(task_id = %task.id, title = %task.title, "round-tripped load_task");

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
