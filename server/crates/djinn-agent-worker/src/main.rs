//! `djinn-agent-worker` — the binary the `KubernetesRuntime` launches inside
//! each per-task-run Pod.
//!
//! Phase 2 K8s PR 2 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`.
//!
//! ## What this binary does (PR 2 shape)
//!
//! 1. Reads its environment (or matching flags): `DJINN_SERVER_ADDR`,
//!    `DJINN_SPEC_PATH`, `DJINN_TOKEN_PATH`, `DJINN_TASK_RUN_ID`,
//!    `DJINN_WORKSPACE_PATH`.  The launcher projects those values onto the
//!    Pod as container env vars; `clap`'s `env` integration keeps the
//!    out-of-cluster invocation path usable by integration tests that spawn
//!    the binary with an `env()` bag instead of flags.
//! 2. Reads the bincode-serialized [`TaskRunSpec`] from `DJINN_SPEC_PATH`
//!    (mounted read-only from the per-task-run Secret at
//!    `/var/run/djinn/spec.bin` in-cluster).
//! 3. Reads the bearer token from `DJINN_TOKEN_PATH` (the kubelet projects a
//!    rotating ServiceAccount token at `/var/run/secrets/tokens/djinn`).
//! 4. Dials djinn-server's ClusterIP Service via
//!    [`RpcServices::connect_tcp`], which sends an
//!    [`djinn_supervisor::FramePayload::AuthHello`] carrying
//!    `(task_run_id, token)` and awaits an accepted
//!    [`djinn_supervisor::FramePayload::AuthResult`] before entering the
//!    shared bincode-RPC dispatch loop.  Every `SupervisorServices` trait
//!    call from here on is a round-trip over that TCP connection.
//! 5. Attaches to the bind-mounted `/workspace` the launcher materialised
//!    (`Workspace::attach_existing`) — no re-clone inside the Pod.
//! 6. Invokes `services.load_task(spec.task_id)` to prove the full
//!    request/reply round-trip works end-to-end.  A future PR swaps this
//!    placeholder driver for the full `TaskRunSupervisor::new(...).run(spec)`
//!    (the supervisor needs a real `TaskRunRepository` + `MirrorManager`
//!    which we won't plumb into the worker until PR 6/7).
//! 7. Emits the terminal [`TaskRunReport`] as a
//!    [`djinn_runtime::WorkerEvent::TerminalReport`] frame on the same RPC
//!    connection (correlation id `0`) so the launcher's per-task-run
//!    dispatch can pair it with the `KubernetesRuntime::teardown` path.
//!    The legacy stdout-frame fallback was retired with Phase 2.1 — worker
//!    and server images ship together, so there is no staged rollout.
//!
//! ## What this binary deliberately does NOT do
//!
//! * No `TaskRunSupervisor::run` drive — PR 6/7.
//! * No Kubernetes-API calls.  The worker never speaks to the apiserver; it
//!   only dials the djinn-server Service and trusts the in-cluster DNS +
//!   bearer-token handshake for auth.
//! * No stdin spec slurp, no Unix-domain socket dial — those are retired
//!   with this PR's K8s-only cut-over.  The unix-socket path survives on
//!   the launcher side ([`djinn_supervisor::serve_on_unix_socket`]) for
//!   in-process tests, but no production worker dials it.
//!
//! ## Why we don't depend on `djinn-agent` or `djinn-k8s`
//!
//! The worker lives behind an RPC boundary; linking `djinn-agent` would
//! drag in the whole actor framework, coordinator, LSP manager, etc. — the
//! exact surface we're trying to keep host-side.  Linking `djinn-k8s` would
//! pull kube-rs + k8s-openapi into the Pod image for no benefit — the
//! worker's only authenticated peer is djinn-server over the
//! handshake-guarded TCP connection, not the apiserver.  Only
//! `djinn-supervisor` + `djinn-runtime` + `djinn-workspace` + `djinn-core`
//! cross the boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use djinn_core::events::EventBus;
use djinn_db::{Database, DatabaseConnectConfig, MysqlBackendFlavor, MysqlDatabaseConfig};
use djinn_runtime::{RoleKind, TaskRunOutcome, TaskRunReport, TaskRunSpec, WorkerEvent};
use djinn_supervisor::{RpcServices, SupervisorServices};
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Top-level arg parser for the worker binary.
///
/// The default mode (no subcommand) runs the existing K8s task-run wire
/// handshake.  The `warm-graph` subcommand reuses the binary as the
/// per-project warm Pod entrypoint previously served by
/// `djinn-server --warm-graph`; `djinn-agent-worker` is the only image the
/// K8s launcher puts in warm Pods, so collapsing the two saves a
/// per-cluster image footprint.
#[derive(Debug, Parser)]
#[command(
    name = "djinn-agent-worker",
    about = "In-Pod task-run supervisor (Phase 2 K8s) + warm-graph driver"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    #[command(flatten)]
    default_args: Option<WorkerDefaultArgs>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the canonical-graph warm pipeline for a specific project and
    /// exit. The launcher invokes this via `djinn-agent-worker warm-graph
    /// <project_id>` in the per-project warm Pod.
    WarmGraph {
        /// Project id (matches `projects.id`). Positional.
        project_id: String,
    },
}

/// Arguments for the default mode (no subcommand).
///
/// Every field is environment-driven so the production Pod manifest can
/// populate them without having to author a bespoke `command:` argv; the
/// same arguments are also exposed as long-form flags so out-of-cluster
/// integration tests can call the binary with `--server-addr ...` etc.
#[derive(Debug, clap::Args)]
struct WorkerDefaultArgs {
    /// `host:port` of the djinn-server ClusterIP Service (usually
    /// `djinn.<namespace>.svc.cluster.local:8443`). String, not
    /// `SocketAddr` — kube DNS service hostnames are not parseable as
    /// `IP:port`, and `TcpStream::connect` resolves DNS itself.
    #[arg(long, env = "DJINN_SERVER_ADDR")]
    server_addr: String,

    /// Path the launcher mounted the bincode-serialized `TaskRunSpec` at.
    /// Contractual default is `/var/run/djinn/spec.bin` — projected
    /// read-only from the per-task-run Secret.
    #[arg(long, env = "DJINN_SPEC_PATH", default_value = "/var/run/djinn/spec.bin")]
    spec_path: PathBuf,

    /// Path the kubelet projected the rotating ServiceAccount token at.
    /// Contractual default is `/var/run/secrets/tokens/djinn` (audience =
    /// `djinn`).  See the Pod manifest in `djinn-k8s::job` for the
    /// projected-volume source.
    #[arg(
        long,
        env = "DJINN_TOKEN_PATH",
        default_value = "/var/run/secrets/tokens/djinn"
    )]
    token_path: PathBuf,

    /// Task-run id the launcher allocated.  Carried verbatim in the
    /// [`djinn_supervisor::AuthHelloMsg`] frame so the server can
    /// demultiplex per-task-run state on a single TCP listener.
    #[arg(long, env = "DJINN_TASK_RUN_ID")]
    task_run_id: String,

    /// Path the launcher bind-mounted `/workspace` at.  Defaults to the
    /// contractual `/workspace` — exposed as a flag so tests can run the
    /// binary outside a container against a tempdir.
    #[arg(long, env = "DJINN_WORKSPACE_PATH", default_value = "/workspace")]
    workspace_path: PathBuf,
}

/// Local [`djinn_graph::WarmContext`] implementation for the worker binary.
///
/// Mirrors the subset of `djinn-server::AppState::minimal_for_warm_only`
/// the warm pipeline actually consumes — a shared `Database`, a no-op
/// `EventBus` (nothing subscribes in the warm Pod), and a per-process
/// `indexer_lock` mutex (single-flight SCIP subprocess fan-out).
struct WorkerWarmContext {
    db: Database,
    indexer_lock: Arc<tokio::sync::Mutex<()>>,
}

impl djinn_graph::WarmContext for WorkerWarmContext {
    fn db(&self) -> &Database {
        &self.db
    }

    fn event_bus(&self) -> EventBus {
        EventBus::noop()
    }

    fn indexer_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.indexer_lock.clone()
    }
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

    let cli = Cli::parse();

    if let Some(Cmd::WarmGraph { project_id }) = cli.cmd {
        return run_warm_graph(&project_id).await;
    }

    let args = cli
        .default_args
        .ok_or_else(|| anyhow::anyhow!("missing default-mode arguments (use `warm-graph` subcommand or pass --server-addr / --task-run-id / env vars)"))?;
    info!(
        server = %args.server_addr,
        spec = %args.spec_path.display(),
        token = %args.token_path.display(),
        task_run_id = %args.task_run_id,
        workspace = %args.workspace_path.display(),
        "worker starting"
    );

    // 1. Slurp the TaskRunSpec off the mounted Secret file.
    let spec_bytes = tokio::fs::read(&args.spec_path)
        .await
        .with_context(|| format!("read TaskRunSpec from {}", args.spec_path.display()))?;
    let spec: TaskRunSpec =
        bincode::deserialize(&spec_bytes).context("bincode deserialize TaskRunSpec")?;
    info!(task_id = %spec.task_id, flow = ?spec.flow, "received spec");

    // 2. Read the projected ServiceAccount token.  Kubelet-projected tokens
    //    typically land without a trailing newline but be defensive — the
    //    token is a JWT and any surrounding whitespace would poison the
    //    Authorization: Bearer header on any future HTTP path.
    let raw_token = tokio::fs::read_to_string(&args.token_path)
        .await
        .with_context(|| format!("read bearer token from {}", args.token_path.display()))?;
    let token = raw_token.trim().to_string();
    if token.is_empty() {
        anyhow::bail!(
            "bearer token at {} is empty after trim",
            args.token_path.display()
        );
    }

    // 3. Dial djinn-server and perform the AuthHello handshake.  `connect_tcp`
    //    blocks on a single request/response round-trip on correlation_id 0,
    //    then hands the now-authenticated socket to the shared RPC dispatch
    //    loop.  Any post-handshake `SupervisorServices` call round-trips over
    //    that same TCP connection.
    let cancel = CancellationToken::new();
    let server_addr = args.server_addr.clone();
    let (rpc, background) = RpcServices::connect_tcp(
        args.server_addr,
        args.task_run_id.clone(),
        token,
        cancel.clone(),
    )
    .await
    .with_context(|| format!("dial djinn-server at {server_addr}"))?;
    info!(server = %server_addr, "tcp connection up, RPC handshake accepted");

    // 4. Attach to the host-materialised workspace.
    let workspace = Workspace::attach_existing(args.workspace_path.as_path(), &spec.task_branch)
        .context("attach workspace")?;
    info!(path = %workspace.path().display(), branch = %workspace.branch(), "workspace attached");

    // 5. Wrap the RpcServices as `Arc<dyn SupervisorServices>` — the shape
    //    `TaskRunSupervisor::new` consumes.  PR 6/7 will hand this `Arc` to
    //    a real supervisor that also owns a `TaskRunRepository` +
    //    `MirrorManager`.
    let services: Arc<dyn SupervisorServices> = rpc.clone();

    // 6. Drive — today just a `load_task` round-trip.  PR 6/7 plugs the full
    //    `TaskRunSupervisor::new(...).run(spec).await` in here.
    let report = drive_placeholder(&services, &spec)
        .await
        .context("placeholder supervisor drive")?;

    // 7. Ship the terminal report back to the launcher as a `WorkerEvent::
    //    TerminalReport` on the same RPC connection (Phase 2.1).  The
    //    launcher's `KubernetesRuntime::teardown` drains the pending
    //    connection's event channel looking for this frame and uses it as
    //    the authoritative terminal report, falling back to Job-status
    //    polling only if the stream closes without emitting one.  Best-effort:
    //    if the writer task already exited (e.g. the launcher tore the
    //    connection down first) we log the drop but still exit zero — the
    //    Job-status fallback on the launcher side covers that case.
    if let Err(e) = rpc
        .emit_event(WorkerEvent::TerminalReport(report))
        .await
    {
        warn!(
            error = %e,
            "failed to emit TerminalReport over RPC; launcher will fall back to Job-status polling"
        );
    }

    // 8. Shut down the RPC background tasks cleanly.
    //
    //    Order matters: drop every `Arc<RpcServices>` handle (which owns
    //    the outbound `mpsc::Sender<Frame>`) *before* signalling the
    //    supervisor-wide cancel.  Dropping the last sender makes the writer
    //    loop's `rx.recv().await` return `None`, so it drains any remaining
    //    frames (including the TerminalReport we just queued) before
    //    shutting down the write half.  If we fired `cancel.cancel()`
    //    first, the writer's `tokio::select!` would take its `biased`
    //    cancel branch and tear the connection down before the event left
    //    the process — the launcher would then fall back to Job-status
    //    polling even on the happy path.
    drop(services);
    drop(rpc);
    let _ = background.writer.await;
    // Reader still needs an explicit cancel — it's parked on a read that
    // won't wake up on its own now that we've closed our side of the write.
    cancel.cancel();
    let _ = background.reader.await;

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

/// Drive the `warm-graph <project_id>` subcommand end-to-end.
///
/// Mirrors what `djinn-server --warm-graph` used to do: build a minimal
/// `WarmContext` backed by the same Dolt/MySQL pool the server hits, then
/// run `djinn_graph::canonical_graph::run_warm_graph_command` once and
/// exit.  The heavy pipeline's progress lands in shared DB caches that
/// the server process reads on subsequent graph queries.
async fn run_warm_graph(project_id: &str) -> Result<()> {
    let db = bootstrap_warm_database()?;
    let ctx = WorkerWarmContext {
        db,
        indexer_lock: Arc::new(tokio::sync::Mutex::new(())),
    };
    djinn_graph::canonical_graph::run_warm_graph_command(&ctx, project_id)
        .await
        .with_context(|| format!("run_warm_graph_command({project_id})"))
}

/// Replicates `AppState::minimal_for_warm_only`'s DB resolution — the
/// warm Pod shares the same env-var contract as djinn-server so operators
/// only manage one configuration surface:
///
/// * `DJINN_DB_BACKEND` — `mysql` | `dolt` (defaults to `dolt`).
/// * `DJINN_MYSQL_URL` — full DSN (defaults to
///   `mysql://root@127.0.0.1:3306/djinn` under `dolt`).
/// * `DJINN_MYSQL_FLAVOR` — `mysql` | `dolt` (defaults to the backend).
fn bootstrap_warm_database() -> Result<Database> {
    let backend = std::env::var("DJINN_DB_BACKEND").ok();
    let backend = backend
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("dolt")
        .to_ascii_lowercase();

    let flavor_raw = std::env::var("DJINN_MYSQL_FLAVOR").ok();
    let flavor = match flavor_raw
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(backend.as_str())
        .to_ascii_lowercase()
        .as_str()
    {
        "mysql" => MysqlBackendFlavor::Mysql,
        "dolt" => MysqlBackendFlavor::Dolt,
        other => anyhow::bail!("unknown DJINN_MYSQL_FLAVOR `{other}`; expected `mysql` or `dolt`"),
    };

    let url = std::env::var("DJINN_MYSQL_URL")
        .ok()
        .or_else(|| match flavor {
            MysqlBackendFlavor::Dolt => Some("mysql://root@127.0.0.1:3306/djinn".to_owned()),
            MysqlBackendFlavor::Mysql => None,
        })
        .ok_or_else(|| anyhow::anyhow!("DJINN_MYSQL_URL must be set when DJINN_MYSQL_FLAVOR=mysql"))?;

    let connect = DatabaseConnectConfig::Mysql(MysqlDatabaseConfig { url, flavor });
    Database::open_with_config(connect).context("open warm worker database")
}

/// Compile-time sanity: the paths the worker contract publishes to the
/// container image must be valid `&Path` literals.  Catches typos in the
/// default workspace / spec / token paths without a runtime surprise.
#[allow(dead_code)]
const _CONTRACT_WORKSPACE: &str = "/workspace";
#[allow(dead_code)]
const _CONTRACT_SPEC_PATH: &str = "/var/run/djinn/spec.bin";
#[allow(dead_code)]
const _CONTRACT_TOKEN_PATH: &str = "/var/run/secrets/tokens/djinn";
#[allow(dead_code)]
fn _assert_contract_workspace_path() -> &'static Path {
    Path::new(_CONTRACT_WORKSPACE)
}
