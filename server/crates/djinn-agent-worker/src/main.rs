//! `djinn-agent-worker` — the binary the `KubernetesRuntime` launches inside
//! each per-task-run Pod.
//!
//! Phase 2 K8s scaffolding (originally PR 2 of
//! `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`) plus the Phase
//! 7b cut-over from `~/.claude/plans/phase2-worker-execution-architecture.md`
//! that lights up real in-Pod supervisor drive.
//!
//! ## What this binary does
//!
//! 1. Reads its environment (or matching flags): `DJINN_SERVER_ADDR`,
//!    `DJINN_SPEC_PATH`, `DJINN_CREDENTIALS_PATH`, `DJINN_TOKEN_PATH`,
//!    `DJINN_TASK_RUN_ID`, `DJINN_WORKSPACE_PATH`. The launcher projects
//!    those onto the Pod as container env vars; `clap`'s `env` integration
//!    keeps the out-of-cluster invocation path usable by integration tests
//!    that spawn the binary with an `env()` bag instead of flags.
//! 2. Reads the bincode-serialized [`TaskRunSpec`] from `DJINN_SPEC_PATH`
//!    (mounted read-only from the per-task-run Secret at
//!    `/var/run/djinn/spec.bin` in-cluster) and the bincode-serialised
//!    [`ResolvedCredentials`] from `DJINN_CREDENTIALS_PATH`.
//! 3. Reads the bearer token from `DJINN_TOKEN_PATH` (the kubelet projects a
//!    rotating ServiceAccount token at `/var/run/secrets/tokens/djinn`).
//! 4. Dials djinn-server's ClusterIP Service via
//!    [`RpcServices::connect_tcp`], which sends an
//!    [`djinn_supervisor::FramePayload::AuthHello`] carrying
//!    `(task_run_id, token)` and awaits an accepted
//!    [`djinn_supervisor::FramePayload::AuthResult`] before entering the
//!    shared bincode-RPC dispatch loop.
//! 5. Attaches to the bind-mounted `/workspace` the launcher materialised
//!    (`Workspace::attach_existing`) — no re-clone inside the Pod.
//! 6. Constructs a [`WorkerSupervisorServices`] which delegates every
//!    host-bound trait method (DB writes, SSE publish, PR open, …) to the
//!    RPC connection and runs `execute_stage` LOCALLY against a worker-built
//!    provider per role.
//! 7. Hands the services + the in-Pod [`MirrorManager`] to
//!    `TaskRunSupervisor::new(...).run(spec)` to drive the role sequence end
//!    to end.
//! 8. Emits the terminal [`TaskRunReport`] as a
//!    [`djinn_runtime::WorkerEvent::TerminalReport`] frame on the same RPC
//!    connection so the launcher's per-task-run dispatch can pair it with
//!    the `KubernetesRuntime::teardown` path.
//!
//! ## What this binary deliberately does NOT do
//!
//! * No Kubernetes-API calls. The worker never speaks to the apiserver; it
//!   only dials the djinn-server Service and trusts the in-cluster DNS +
//!   bearer-token handshake for auth.
//! * No stdin spec slurp, no Unix-domain socket dial — those are retired
//!   with the K8s-only cut-over. The unix-socket path survives on the
//!   launcher side ([`djinn_supervisor::serve_on_unix_socket`]) for
//!   in-process tests, but no production worker dials it.
//!
//! ## Why we depend on `djinn-agent`
//!
//! Phase 7b reuses the in-tree per-stage executor
//! (`djinn_agent::supervisor::worker_execute_stage`) so the worker drives
//! real provider streams against the bind-mounted workspace without
//! duplicating the lifecycle / prompt / reply-loop bodies. `djinn-k8s` is
//! still excluded — the worker's only authenticated peer is djinn-server
//! over the handshake-guarded TCP connection, not the apiserver.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod lifecycle;
mod worker_services;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use djinn_agent::context::AgentContext;
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_core::events::EventBus;
use djinn_db::{Database, DatabaseConnectConfig, PostgresDatabaseConfig};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_runtime::{ResolvedCredentials, RoleKind, TaskRunSpec, WorkerEvent};
use djinn_supervisor::{RpcServices, SupervisorServices, TaskRunSupervisor};
use djinn_workspace::{MirrorManager, Workspace};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use worker_services::WorkerSupervisorServices;

/// Top-level arg parser for the worker binary.
///
/// Every invocation picks a subcommand. `task-run` runs the K8s task-run
/// wire handshake (default in-Pod supervisor); `warm-graph` reuses the
/// binary as the per-project warm Pod entrypoint previously served by
/// `djinn-server --warm-graph`. The two paths have disjoint required
/// args, so they live behind disjoint subcommands — a single `Cli`
/// wrapper with `#[command(flatten)] Option<Args>` doesn't work: clap
/// validates the flattened required args *before* dispatching to the
/// subcommand, so the warm Pod (which has no DJINN_SERVER_ADDR /
/// DJINN_TASK_RUN_ID) would fail argv parsing at launch.
#[derive(Debug, Parser)]
#[command(
    name = "djinn-agent-worker",
    about = "In-Pod task-run supervisor (Phase 2 K8s) + warm-graph driver"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the default K8s task-run worker loop: attach to the mounted
    /// workspace, dial djinn-server over the authenticated TCP listener,
    /// drive the supervisor to completion, and emit the terminal report.
    /// Invoked by `build_task_run_job` in `djinn-k8s`.
    TaskRun(WorkerDefaultArgs),

    /// Run the canonical-graph warm pipeline for a specific project and
    /// exit. The launcher invokes this via `djinn-agent-worker warm-graph
    /// <project_id>` in the per-project warm Pod.
    WarmGraph {
        /// Project id (matches `projects.id`). Positional.
        project_id: String,
    },
}

/// Arguments for the `task-run` subcommand.
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

    /// Path the launcher mounted the bincode-serialized
    /// [`ResolvedCredentials`] at. Contractual default is
    /// `/var/run/djinn/credentials.bin` — projected read-only from the
    /// same per-task-run Secret as `spec.bin` (Phase 7a). The worker only
    /// reads + logs the role keys today; live provider construction lands
    /// in Phase 7b.
    #[arg(
        long,
        env = "DJINN_CREDENTIALS_PATH",
        default_value = "/var/run/djinn/credentials.bin"
    )]
    credentials_path: PathBuf,

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

    match cli.cmd {
        Cmd::TaskRun(args) => run_task_run(args).await,
        Cmd::WarmGraph { project_id } => run_warm_graph(&project_id).await,
    }
}

/// Configure git + Go in the worker Pod so the agent's build/test commands can
/// fetch the project org's PRIVATE transitive deps. The workspace remote points
/// at the local `/mirror` (no github auth), so transitive `github.com/<owner>/*`
/// fetches would otherwise hit GitHub unauthenticated. Using the per-project
/// installation token (minted host-side, carried on the spec — never a
/// hardcoded org), we:
///   - write a global git `url.insteadOf` rewrite so any git fetch of
///     `https://github.com/<owner>/…` carries the token (covers Go modules,
///     cargo git deps with `git-fetch-with-cli`, and pnpm/npm git deps), and
///   - set `GOPRIVATE=github.com/<owner>/*` (via `go env -w`) so `go` fetches
///     those modules directly via git instead of the public proxy/sumdb.
/// Best-effort: failures are logged (never the token) and never fatal — public
/// deps still resolve. The token is short-lived (~1h) and lives only in the
/// Pod's HOME/go env for the run.
async fn configure_private_dep_access(spec: &TaskRunSpec) {
    let (Some(owner), Some(token)) = (
        spec.github_owner.as_deref(),
        spec.github_install_token.as_deref(),
    ) else {
        return;
    };

    // Global git credential rewrite. NOTE: never log `key` — it embeds the token.
    let key = format!("url.https://x-access-token:{token}@github.com/{owner}/.insteadOf");
    let value = format!("https://github.com/{owner}/");
    match tokio::process::Command::new("git")
        .args(["config", "--global", &key, &value])
        .status()
        .await
    {
        Ok(s) if s.success() => {
            info!(owner, "configure_private_dep_access: git insteadOf set for private deps")
        }
        Ok(s) => warn!(
            owner,
            code = ?s.code(),
            "configure_private_dep_access: git config failed; private deps may be inaccessible"
        ),
        Err(e) => {
            warn!(owner, error = %e, "configure_private_dep_access: git config errored")
        }
    }

    // GOPRIVATE for Go projects (no-op when `go` isn't installed — best-effort).
    let goprivate = format!("GOPRIVATE=github.com/{owner}/*");
    match tokio::process::Command::new("go")
        .args(["env", "-w", &goprivate])
        .status()
        .await
    {
        Ok(s) if s.success() => info!(owner, "configure_private_dep_access: GOPRIVATE set"),
        // Non-Go projects: `go` absent → expected; debug, not warn.
        _ => tracing::debug!(owner, "configure_private_dep_access: `go env -w` skipped (go absent?)"),
    }
}

async fn run_task_run(args: WorkerDefaultArgs) -> Result<()> {
    info!(
        server = %args.server_addr,
        spec = %args.spec_path.display(),
        credentials = %args.credentials_path.display(),
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

    // Configure git + Go so the agent's build/test commands can fetch the
    // org's PRIVATE transitive deps using the per-project installation token.
    configure_private_dep_access(&spec).await;

    // 1b. Slurp the per-role credentials bundle off the same Secret mount
    //     (Phase 7a). Phase 7b hands these to `WorkerSupervisorServices` so
    //     `execute_stage` can build providers locally without round-tripping
    //     vault keys through the host.
    let credentials_bytes = tokio::fs::read(&args.credentials_path)
        .await
        .with_context(|| {
            format!(
                "read ResolvedCredentials from {}",
                args.credentials_path.display()
            )
        })?;
    let credentials: ResolvedCredentials = bincode::deserialize(&credentials_bytes)
        .context("bincode deserialize ResolvedCredentials")?;
    let role_keys: Vec<&'static str> = credentials
        .roles()
        .copied()
        .map(RoleKind::as_str)
        .collect();
    info!(
        roles = ?role_keys,
        bytes = credentials_bytes.len(),
        "received per-role credentials bundle"
    );

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

    // Wire SIGTERM / SIGINT into the supervisor's cancel token so the existing
    // `finalize_interrupted` path runs and flushes a terminal
    // `update_task_run_status(Interrupted)` RPC back to the host before the
    // Pod exits. Without this, K8s' `activeDeadlineSeconds` / eviction /
    // graceful-drain SIGTERM kills the runtime mid-flight and the host's
    // task_runs row stays `running` forever.
    //
    // The K8s Job sets `terminationGracePeriodSeconds=60`, which is the
    // window we have to drain the RPC before SIGKILL hits.
    install_termination_handlers(cancel.clone(), args.task_run_id.clone());

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

    // 5. Build the in-Pod `WorkerSupervisorServices` around the RPC connection
    //    + resolved credentials. `execute_stage` runs locally in the Pod;
    //    every other trait method delegates to djinn-server over the same
    //    TCP connection so the worker never opens its own DB / vault / event
    //    bus.
    //
    //    The `AgentContext` carries the in-Pod database connection the
    //    per-stage executor threads through helpers that still touch the DB
    //    directly (`resolve_role_overrides`, `build_prompt_context`,
    //    `spawn_post_session_work`). Phase 7-followup: route those reads
    //    through `SupervisorServices` too so the worker can run without a
    //    local Database connection.
    let in_pod_db = bootstrap_warm_database()
        .await
        .context("bootstrap in-Pod database for WorkerSupervisorServices")?;
    let agent_context = build_worker_agent_context(in_pod_db, rpc.clone(), spec.project_id.clone());
    let worker_services: Arc<dyn SupervisorServices> = Arc::new(WorkerSupervisorServices::new(
        rpc.clone(),
        credentials,
        cancel.clone(),
        agent_context,
    ));

    // 6. Construct the in-Pod `MirrorManager`. `clone_ephemeral` resolves
    //    against `DJINN_MIRROR_ROOT` (the launcher sets this to `/mirror`,
    //    the PVC-backed RW bind mount the host populated; the worker also
    //    pushes its task_branch back here before delegating open_pr).
    let mirror_root = std::env::var("DJINN_MIRROR_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/mirror"));
    let mirror = Arc::new(MirrorManager::new(mirror_root));

    // 7. Drive the real supervisor end-to-end. Stage execution is local;
    //    every host-bound trait call (DB writes, SSE publish, PR open)
    //    round-trips over RPC.
    let supervisor = TaskRunSupervisor::new(mirror, worker_services.clone());
    let report = supervisor
        .run(spec.clone())
        .await
        .context("task-run supervisor drive")?;

    // 8. Ship the terminal report back to the launcher as a `WorkerEvent::
    //    TerminalReport` on the same RPC connection. The launcher's
    //    `KubernetesRuntime::teardown` drains the pending connection's event
    //    channel looking for this frame and uses it as the authoritative
    //    terminal report, falling back to Job-status polling only if the
    //    stream closes without emitting one. Best-effort: if the writer
    //    task already exited (e.g. the launcher tore the connection down
    //    first) we log the drop but still exit zero — the Job-status
    //    fallback on the launcher side covers that case.
    if let Err(e) = rpc
        .emit_event(WorkerEvent::TerminalReport(report))
        .await
    {
        warn!(
            error = %e,
            "failed to emit TerminalReport over RPC; launcher will fall back to Job-status polling"
        );
    }

    // 9. Shut down the RPC background tasks cleanly.
    //
    //    Order matters: drop every `Arc<RpcServices>` handle (which owns
    //    the outbound `mpsc::Sender<Frame>`) *before* awaiting the writer.
    //    Dropping the last sender makes the writer loop's
    //    `rx.recv().await` return `None`, so it shuts down the write half
    //    cleanly.  If any `Arc<dyn SupervisorServices>` still pointed at
    //    the inner `WorkerSupervisorServices` (which carries a
    //    `rpc: Arc<RpcServices>` field), the sender would stay alive and
    //    the writer would wait forever.
    //
    //    `supervisor` holds the second `Arc<dyn SupervisorServices>`
    //    clone (taken at line construction); drop it first so the chain
    //    `supervisor → worker_services_clone → rpc → Sender` releases.
    drop(supervisor);
    drop(worker_services);
    drop(rpc);
    let _ = background.writer.await;
    // Reader still needs an explicit cancel — it's parked on a read that
    // won't wake up on its own now that we've closed our side of the write.
    cancel.cancel();
    let _ = background.reader.await;

    drop(workspace);
    Ok(())
}

/// Spawn background listeners for SIGTERM and SIGINT that flip `cancel` when
/// the kubelet (or operator) signals shutdown. The supervisor body checks
/// `cancel` between stages, exits its for-loop with `Interrupted`, and runs
/// `finalize_interrupted` — which calls `update_task_run_status(Interrupted)`
/// over the still-live RPC channel. The Pod's
/// `terminationGracePeriodSeconds=60` gives that RPC time to land before
/// SIGKILL.
fn install_termination_handlers(cancel: CancellationToken, task_run_id: String) {
    for (kind, label) in [
        (SignalKind::terminate(), "SIGTERM"),
        (SignalKind::interrupt(), "SIGINT"),
    ] {
        let mut stream = match signal(kind) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    signal = label,
                    error = %e,
                    "failed to install signal handler; supervisor will not see kubelet shutdowns"
                );
                continue;
            }
        };
        let cancel = cancel.clone();
        let task_run_id = task_run_id.clone();
        tokio::spawn(async move {
            if stream.recv().await.is_some() {
                info!(
                    signal = label,
                    task_run_id = %task_run_id,
                    "received termination signal; cancelling supervisor so the terminal RPC flushes"
                );
                cancel.cancel();
            }
        });
    }
}

/// Build the in-Pod `AgentContext` the per-stage executor threads through
/// helpers that still touch the DB directly. Most fields are no-ops on the
/// worker; `db` carries the in-Pod connection bootstrapped via
/// `bootstrap_warm_database`, and `event_bus` bridges every emitted
/// envelope onto the existing RPC connection so the host's SSE subscribers
/// (web UI session live-feed) see worker-side activity in real time.
/// Gap-2 of the Phase 7-followup: before the bridge landed, the worker
/// installed `EventBus::noop()` here and every
/// `event_bus.send(..)` call in `actors::slot::reply_loop` / `streaming`
/// silently vanished.
fn build_worker_agent_context(
    db: Database,
    rpc: Arc<RpcServices>,
    project_id: String,
) -> AgentContext {
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_supervisor::services::SerializableDjinnEvent;
    let rpc_for_bus = rpc.clone();
    let event_bus = EventBus::spawning(move |envelope: DjinnEventEnvelope| {
        let rpc = rpc_for_bus.clone();
        async move {
            let wire = SerializableDjinnEvent::from_envelope(&envelope);
            if let Err(e) = rpc.emit_djinn_event(wire).await {
                tracing::warn!(
                    entity_type = %envelope.entity_type,
                    action = %envelope.action,
                    error = %e,
                    "worker EventBus bridge: emit_djinn_event RPC failed"
                );
            }
        }
    });
    AgentContext {
        db,
        event_bus,
        git_actors: Arc::new(Mutex::new(HashMap::new())),
        verifying_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
        role_registry: Arc::new(RoleRegistry::new()),
        health_tracker: HealthTracker::new(),
        file_time: Arc::new(FileTime::new()),
        lsp: LspManager::new(),
        catalog: CatalogService::new(),
        coordinator: Arc::new(tokio::sync::Mutex::new(None)),
        active_tasks: Default::default(),
        task_ops_project_path_override: None,
        working_root: None,
        graph_warmer: None,
        repo_graph_ops: None,
        mirror: None,
        rpc_registry: None,
        // The K8s worker only ever serves one project per Pod, so default
        // every multi-project-aware tool call (epic_show, epic_tasks,
        // task_*, …) to spec.project_id. Without this the LLM has to
        // remember to pass `project` to every call or burn tokens
        // retrying past the "project is required when multiple projects
        // are configured" error from helpers::resolve_project_id_for_agent_tools.
        default_project_id: Some(project_id),
    }
}

/// Drive the `warm-graph <project_id>` subcommand end-to-end.
///
/// Mirrors what `djinn-server --warm-graph` used to do: build a minimal
/// `WarmContext` backed by the same Dolt/MySQL pool the server hits, then
/// run `djinn_graph::canonical_graph::run_warm_graph_command` once and
/// exit.  The heavy pipeline's progress lands in shared DB caches that
/// the server process reads on subsequent graph queries.
async fn run_warm_graph(project_id: &str) -> Result<()> {
    let db = bootstrap_warm_database().await?;
    let ctx = WorkerWarmContext {
        db,
        indexer_lock: Arc::new(tokio::sync::Mutex::new(())),
    };

    // Run the customer's `.devcontainer/devcontainer.json` lifecycle hooks
    // (onCreateCommand, postCreateCommand, updateContentCommand,
    // postStartCommand) before invoking the canonical-graph pipeline. These
    // hooks carry per-project setup — e.g. `rustup component add
    // rust-analyzer` for a pinned toolchain — without which the SCIP
    // indexers can fail with "Unknown binary" inside the warm Pod. Resolved
    // against DJINN_PROJECT_ROOT (set by build_warm_job).
    //
    // Non-fatal on purpose: the status quo before this runner existed was
    // zero hook execution, so any partial success is strictly additive; a
    // broken hook shouldn't wedge the graph build. Errors are logged with
    // full context so they surface in kubectl logs.
    let lifecycle_root = std::env::var("DJINN_PROJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/workspace"));

    // Load the project's EnvironmentConfig from the ConfigMap mount and
    // run `pre_anything` hooks (which run in every Pod djinn starts)
    // before the canonical-graph pipeline. An absent mount is fine — the
    // warm Pod runs without hooks, matching pre-cut-over behaviour.
    let env_config_path = PathBuf::from(lifecycle::ENV_CONFIG_MOUNT_FILE);
    match lifecycle::load_environment_config(&env_config_path).await {
        Ok(Some(cfg)) => {
            tracing::info!(
                project_id,
                schema_version = cfg.schema_version,
                workspace_count = cfg.workspaces.len(),
                pre_anything_hooks = cfg.lifecycle.pre_anything.len(),
                "environment_config loaded from {}",
                env_config_path.display()
            );
            if let Err(e) = lifecycle::run_phase(
                &lifecycle_root,
                "pre_anything",
                &cfg.lifecycle.pre_anything,
            )
            .await
            {
                warn!(
                    project_id,
                    project_root = %lifecycle_root.display(),
                    error = %format!("{e:#}"),
                    "pre_anything hook failed; continuing with warm-graph anyway"
                );
            }
        }
        Ok(None) => tracing::debug!(
            project_id,
            "no environment_config mounted at {} — continuing without hooks",
            env_config_path.display()
        ),
        Err(e) => warn!(
            project_id,
            error = %format!("{e:#}"),
            "environment_config present but failed to load; ignoring"
        ),
    }

    // Architect-only warm path: this subcommand binary is dispatched
    // exclusively by `K8sGraphWarmer`, which is wired into the
    // architect-only `GraphWarmerService::trigger` pipeline.  Minting the
    // capability token here is the sanctioned "this is the warm Pod
    // runner" claim — see `djinn_graph::architect` for the invariant.
    djinn_graph::canonical_graph::run_warm_graph_command(
        &ctx,
        project_id,
        djinn_graph::architect::ArchitectWarmToken::new(),
    )
    .await
    .with_context(|| format!("run_warm_graph_command({project_id})"))
}

/// Replicates `AppState::minimal_for_warm_only`'s DB resolution — the
/// warm Pod shares the same env-var contract as djinn-server so operators
/// only manage one configuration surface:
///
/// * `DJINN_DATABASE_URL` — full DSN (required).
async fn bootstrap_warm_database() -> Result<Database> {
    let url = std::env::var("DJINN_DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("DJINN_DATABASE_URL must be set for the warm worker pod"))?;

    let connect = DatabaseConnectConfig::Postgres(PostgresDatabaseConfig { url });
    let db = Database::open_with_config(connect).context("open warm worker database")?;
    // Worker pods must never run the migrator: it grabs a global advisory
    // lock that contends with the migrate-Job / peer pods, and our 10s
    // `lock_timeout` then cancels the statement ("canceling statement due to
    // lock timeout"), wedging stage setup — the worker failures seen during
    // deploys. Verify the schema lock-free and mark the DB initialized so
    // every later repository call short-circuits `ensure_initialized()`.
    db.verify_and_mark_initialized()
        .await
        .context("verify worker database schema is current")?;
    Ok(db)
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
