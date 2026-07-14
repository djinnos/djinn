// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
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
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod cargo_cache_policy;
#[allow(dead_code)] // Consumed by the later warm-ordering integration task.
pub mod cargo_incremental_prune;
pub mod cargo_metrics;
mod cargo_target_seed;
mod checkpoint;
mod checkpoint_safety;
mod lifecycle;
mod worker_services;

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use cargo_target_seed::{
    CargoTargetSeedFallback, CargoTargetSeedResult, run_target_dir, seed_cargo_target_dir,
    teardown_run_dir, warm_base_dir,
};
use clap::{Parser, Subcommand};
use djinn_agent::context::{AgentContext, ReconciliationSweepConfig};
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_core::events::EventBus;
use djinn_db::{Database, DatabaseConnectConfig, PostgresDatabaseConfig};
use djinn_graph::graph_parity::{GraphArtifactBlobParityError, assert_graph_artifact_blob_parity};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_runtime::{
    ResolvedCredentials, RoleKind, TaskRunOutcome, TaskRunReport, TaskRunSpec, WorkerEvent,
};
use djinn_supervisor::{RpcServices, SupervisorServices, TaskRunSupervisor};
use djinn_workspace::{MirrorManager, Workspace};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Wall-clock margin subtracted from the Job's `activeDeadlineSeconds` to set
/// the in-pod soft deadline. The supervisor winds itself down (cancel +
/// checkpoint commit/push) this far ahead of the kubelet's hard kill so a
/// slow model never loses work to the deadline. ~10 min covers a checkpoint
/// commit/push plus the terminal RPC flush with comfortable slack.
const SOFT_DEADLINE_MARGIN: Duration = Duration::from_secs(600);

/// Floor for the armed soft-deadline interval. For small configured deadlines
/// (tests, tuned-down installs) `deadline - margin` can underflow or land
/// implausibly early; clamp so the timer never fires immediately at startup.
const SOFT_DEADLINE_MIN: Duration = Duration::from_secs(60);

/// Upper bound on the checkpoint commit+push. The Pod's
/// `terminationGracePeriodSeconds` (default 60s) is the hard window between
/// SIGTERM and SIGKILL; bound the checkpoint well inside it so a wedged git
/// operation (locked index, mid-merge) can't eat the whole grace period and
/// starve the terminal RPC flush.
const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the periodic push loop wakes to push `task_branch` to the mirror.
/// ~3 min is a deliberate middle ground: short enough that an OOM SIGKILL
/// (which gets NO signal, so the SIGTERM/soft-deadline checkpoint never runs)
/// strands at most ~3 min of already-committed work, long enough that the
/// per-tick `git rev-parse` + occasional push add negligible load. The push is
/// read-only on the working tree (it reads refs/objects only — no add, no
/// commit), so it never contends with the live agent or cargo.
const PERIODIC_PUSH_INTERVAL: Duration = Duration::from_secs(180);

const CARGO_TARGET_DIR_ENV: &str = "CARGO_TARGET_DIR";

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

    /// Compare two cached repo graph artifacts for a project and exit.
    ///
    /// Loads blobs exclusively through `RepoGraphCacheRepository` and delegates
    /// artifact deserialization/comparison to `djinn_graph::graph_parity` so
    /// cache compatibility shims stay centralized in the graph crate.
    CompareGraphArtifacts {
        /// Project id (matches `projects.id`).
        #[arg(long)]
        project_id: String,

        /// Old commit SHA to compare, or `latest` for the newest cached row.
        #[arg(long)]
        old_commit: String,

        /// New commit SHA to compare, or `latest` for the newest cached row.
        #[arg(long)]
        new_commit: String,
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
    #[arg(
        long,
        env = "DJINN_SPEC_PATH",
        default_value = "/var/run/djinn/spec.bin"
    )]
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
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    if let Err(error) = djinn_telemetry::init() {
        warn!(%error, "failed to initialize worker telemetry recorder");
    }

    let cli = Cli::parse();

    match cli.cmd {
        Cmd::TaskRun(args) => run_task_run(args).await,
        Cmd::WarmGraph { project_id } => run_warm_graph(&project_id).await,
        Cmd::CompareGraphArtifacts {
            project_id,
            old_commit,
            new_commit,
        } => run_compare_graph_artifacts(&project_id, &old_commit, &new_commit).await,
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
///
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
    match djinn_git::run_git_command_in(
        std::path::Path::new("/"),
        vec!["config".into(), "--global".into(), key, value],
    )
    .await
    {
        Ok(_) => {
            info!(
                owner,
                "configure_private_dep_access: git insteadOf set for private deps"
            )
        }
        Err(e) => {
            warn!(owner, error = %e, "configure_private_dep_access: git config failed; private deps may be inaccessible")
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
        _ => tracing::debug!(
            owner,
            "configure_private_dep_access: `go env -w` skipped (go absent?)"
        ),
    }
}

fn set_cargo_target_dir_for_children(destination: &Path) {
    // SAFETY: this worker mutates the process environment only during
    // task-run startup, before installing its own signal/deadline/push
    // background tasks or starting supervisor-driven command execution.
    unsafe { std::env::set_var(CARGO_TARGET_DIR_ENV, destination) };
}

fn record_cargo_target_seed_result(seed_context: &'static str, result: &CargoTargetSeedResult) {
    let seed_outcome = if result.cold_started() {
        "fallback"
    } else {
        "hit"
    };
    let fallback_reason = if result.cold_started() {
        cargo_target_seed_fallback_reason(result.fallback_reason.as_ref())
    } else {
        ""
    };

    if !result.cold_started() {
        djinn_telemetry::cargo_target_seed::increment_seed_hit();
    }

    info!(
        seed_context,
        seed_outcome,
        fallback_reason,
        linked_file_count = result.linked_file_count,
        copied_file_count = result.copied_file_count,
        skipped_file_count = result.skipped_file_count,
        elapsed_ms = result.elapsed.as_millis(),
        "cargo target seed result"
    );
}

fn cargo_target_seed_fallback_reason(reason: Option<&CargoTargetSeedFallback>) -> &'static str {
    match reason {
        Some(CargoTargetSeedFallback::BaseMissing) => {
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_MISSING
        }
        Some(CargoTargetSeedFallback::BaseNotDirectory) => {
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_NOT_DIRECTORY
        }
        Some(CargoTargetSeedFallback::BaseUnusable(_)) => {
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_UNUSABLE
        }
        Some(CargoTargetSeedFallback::ScanFailed(_)) => {
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_SCAN_FAILED
        }
        Some(CargoTargetSeedFallback::CloneFailed(_)) => {
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_CLONE_FAILED
        }
        None => djinn_telemetry::cargo_target_seed::FALLBACK_REASON_UNKNOWN,
    }
}

/// Classify the cargo-target seed attempt outcome for the `workspace_seed_seconds`
/// histogram (proposal zp5t).
///
/// - `Ok(Ok(_))` → `ok` (seed succeeded, including cold-start fallback which
///   is still a completed seed attempt).
/// - `Ok(Err(_))` → `error` (the seed helper returned a setup/IO error).
/// - `Err(join_err)` → `cancelled` when the `spawn_blocking` task was
///   cancelled (runtime shutdown), `error` otherwise (panic).
///
/// This is a pure function so tests can assert classification without running
/// the full async seed path.
fn classify_seed_outcome(
    result: &Result<std::io::Result<CargoTargetSeedResult>, tokio::task::JoinError>,
) -> &'static str {
    match result {
        Ok(Ok(_)) => djinn_telemetry::workspace_seed::OUTCOME_OK,
        Ok(Err(_)) => djinn_telemetry::workspace_seed::OUTCOME_ERROR,
        Err(join_err) if join_err.is_cancelled() => {
            djinn_telemetry::workspace_seed::OUTCOME_CANCELLED
        }
        Err(_) => djinn_telemetry::workspace_seed::OUTCOME_ERROR,
    }
}

/// Terminal state of a started cargo-target seed attempt.
///
/// `Cancelled` is separate from the join result so the terminal recording
/// boundary can be failure-injected without aborting a started
/// `spawn_blocking` task, which Tokio cannot reliably abort.
enum SeedAttemptTerminal<'a> {
    Join(&'a Result<std::io::Result<CargoTargetSeedResult>, tokio::task::JoinError>),
    // Deterministic failure injection for telemetry tests. Production
    // cancellation is represented by a cancelled JoinError above.
    #[cfg(test)]
    Cancelled,
}

fn classify_seed_terminal(terminal: SeedAttemptTerminal<'_>) -> &'static str {
    match terminal {
        SeedAttemptTerminal::Join(result) => classify_seed_outcome(result),
        #[cfg(test)]
        SeedAttemptTerminal::Cancelled => djinn_telemetry::workspace_seed::OUTCOME_CANCELLED,
    }
}

/// Record one `workspace_seed_seconds` sample for the cargo-target seed
/// attempt, classifying its terminal state. Called exactly once per started
/// seed attempt.
///
/// Accepts `elapsed` directly so tests can assert deterministic `_sum`
/// deltas without a wall-clock dependency.
fn record_seed_terminal_seconds(elapsed: Duration, terminal: SeedAttemptTerminal<'_>) {
    let outcome = classify_seed_terminal(terminal);
    djinn_telemetry::workspace_seed::record_seconds(outcome, elapsed);
}

/// Build a deterministic successful join boundary for telemetry tests without
/// starting an unabortable blocking task.
#[cfg(test)]
fn completed_seed_join(
    result: std::io::Result<CargoTargetSeedResult>,
) -> Result<std::io::Result<CargoTargetSeedResult>, tokio::task::JoinError> {
    Ok(result)
}

async fn prepare_cargo_target_dir(spec: &TaskRunSpec, workspace_path: &Path) -> PathBuf {
    // Canonicalize once so every structured event for this task-run shares the
    // same absolute workspace_dir. The cargo warm path uses the SAME canonical
    // form (see `warm_cargo_target_base`), so emitting both absolute paths
    // lets the coordinator health sweep confirm the seed outcome for the
    // workspace task-run will compile in.
    let workspace_dir = match std::fs::canonicalize(workspace_path) {
        Ok(canonical) => canonical,
        Err(err) => {
            warn!(
                task_run_id = %spec.task_run_id,
                project_id = %spec.project_id,
                workspace_path = %workspace_path.display(),
                error = %err,
                "cargo target seed: failed to canonicalize workspace path; \
                 emitting events with the unresolved path"
            );
            workspace_path.to_path_buf()
        }
    };
    let workspace_dir_display = workspace_dir.display().to_string();

    // Surface the resolved workspace dir as a low-cardinality metric so the
    // coordinator health sweep can correlate the task-run seed outcome with
    // the warm-step outcome for the same workspace.
    cargo_metrics::record_resolved_workspace_dir(&spec.project_id, workspace_dir_display.as_str());

    let source_base = warm_base_dir(&spec.project_id);
    let fallback_run_dir = run_target_dir(&spec.task_run_id);
    let (destination_run_dir, env_was_present) = match std::env::var_os(CARGO_TARGET_DIR_ENV) {
        Some(raw) if !raw.is_empty() => {
            let configured = PathBuf::from(raw);
            if configured == source_base {
                warn!(
                    task_run_id = %spec.task_run_id,
                    project_id = %spec.project_id,
                    configured_target_dir = %configured.display(),
                    fallback_target_dir = %fallback_run_dir.display(),
                    "cargo target seed: ignoring shared warm base CARGO_TARGET_DIR for task run"
                );
                set_cargo_target_dir_for_children(&fallback_run_dir);
                (fallback_run_dir.clone(), false)
            } else {
                (configured, true)
            }
        }
        _ => {
            set_cargo_target_dir_for_children(&fallback_run_dir);
            (fallback_run_dir.clone(), false)
        }
    };

    // Resolve the destination to its absolute form so the structured event
    // carries a path the coordinator health sweep can compare against the
    // warm base path. CARGO_TARGET_DIR env values may be relative or contain
    // symlinks; canonicalize once and reuse.
    let cargo_target_dir_display = match std::fs::canonicalize(&destination_run_dir) {
        Ok(canonical) => canonical.display().to_string(),
        Err(_) => destination_run_dir.display().to_string(),
    };

    info!(
        task_run_id = %spec.task_run_id,
        project_id = %spec.project_id,
        source_base = %source_base.display(),
        destination_run_dir = %destination_run_dir.display(),
        env_was_present,
        "cargo target seed: preparing private run target dir"
    );

    let seed_source_base = source_base.clone();
    let seed_destination_run_dir = destination_run_dir.clone();
    let seed_start = djinn_core::clock::Clock::now_instant(&djinn_core::clock::SystemClock::new());
    let seed_join_result = tokio::task::spawn_blocking(move || {
        seed_cargo_target_dir(seed_source_base, seed_destination_run_dir)
    })
    .await;
    let seed_elapsed = seed_start.elapsed();
    // Record exactly one workspace_seed_seconds sample per started seed
    // attempt, regardless of outcome (ok / error / cancelled).
    record_seed_terminal_seconds(seed_elapsed, SeedAttemptTerminal::Join(&seed_join_result));
    match seed_join_result {
        Ok(Ok(result)) => {
            record_cargo_target_seed_result("task_run", &result);
            let fallback_reason = result
                .fallback_reason
                .as_ref()
                .map(std::string::ToString::to_string);
            let seed_outcome = if result.cold_started() {
                "fallback"
            } else {
                "hit"
            };
            // Single structured event carrying the absolute workspace and
            // cargo_target_dir plus the seed outcome — this is the surface the
            // coordinator health sweep consumes to correlate seed outcomes
            // with workspace paths. Existing rich log lines below carry the
            // full link/copy/elapsed context unchanged.
            info!(
                task_run_id = %spec.task_run_id,
                project_id = %spec.project_id,
                workspace_dir = %workspace_dir_display,
                cargo_target_dir = %cargo_target_dir_display,
                seed_outcome,
                fallback_reason = fallback_reason.as_deref().unwrap_or(""),
                "cargo target seed: workspace and outcome summary"
            );
            if result.cold_started() {
                let fallback_reason = fallback_reason.as_deref().unwrap_or("unknown");
                warn!(
                    task_run_id = %spec.task_run_id,
                    project_id = %spec.project_id,
                    source_base = %source_base.display(),
                    destination_run_dir = %destination_run_dir.display(),
                    clone_duration_ms = result.elapsed.as_millis(),
                    seed_duration_ms = result.elapsed.as_millis(),
                    linked_file_count = result.linked_file_count,
                    copied_file_count = result.copied_file_count,
                    skipped_file_count = result.skipped_file_count,
                    linked_bytes = result.linked_bytes,
                    copied_bytes = result.copied_bytes,
                    fallback_reason,
                    "cargo target seed: falling back to cold private target dir"
                );
            } else {
                info!(
                    task_run_id = %spec.task_run_id,
                    project_id = %spec.project_id,
                    source_base = %source_base.display(),
                    destination_run_dir = %destination_run_dir.display(),
                    clone_duration_ms = result.elapsed.as_millis(),
                    seed_duration_ms = result.elapsed.as_millis(),
                    linked_file_count = result.linked_file_count,
                    copied_file_count = result.copied_file_count,
                    skipped_file_count = result.skipped_file_count,
                    linked_bytes = result.linked_bytes,
                    copied_bytes = result.copied_bytes,
                    "cargo target seed: seeded private run target dir"
                );
            }
        }
        Ok(Err(err)) => {
            djinn_telemetry::cargo_target_seed::increment_seed_fallback(
                djinn_telemetry::cargo_target_seed::FALLBACK_REASON_UNKNOWN,
            );
            let fallback_reason = format!("seed helper failed: {err}");
            info!(
                task_run_id = %spec.task_run_id,
                project_id = %spec.project_id,
                workspace_dir = %workspace_dir_display,
                cargo_target_dir = %cargo_target_dir_display,
                seed_outcome = "fallback",
                fallback_reason = %fallback_reason,
                "cargo target seed: workspace and outcome summary"
            );
            warn!(
                task_run_id = %spec.task_run_id,
                project_id = %spec.project_id,
                source_base = %source_base.display(),
                destination_run_dir = %destination_run_dir.display(),
                clone_duration_ms = 0_u128,
                seed_duration_ms = 0_u128,
                linked_file_count = 0_u64,
                copied_file_count = 0_u64,
                skipped_file_count = 0_u64,
                fallback_reason = %fallback_reason,
                "cargo target seed: proceeding with cold private target dir after setup error"
            );
        }
        Err(err) => {
            djinn_telemetry::cargo_target_seed::increment_seed_fallback(
                djinn_telemetry::cargo_target_seed::FALLBACK_REASON_UNKNOWN,
            );
            let fallback_reason = format!("seed task join failed: {err}");
            info!(
                task_run_id = %spec.task_run_id,
                project_id = %spec.project_id,
                workspace_dir = %workspace_dir_display,
                cargo_target_dir = %cargo_target_dir_display,
                seed_outcome = "fallback",
                fallback_reason = %fallback_reason,
                "cargo target seed: workspace and outcome summary"
            );
            warn!(
                task_run_id = %spec.task_run_id,
                project_id = %spec.project_id,
                source_base = %source_base.display(),
                destination_run_dir = %destination_run_dir.display(),
                clone_duration_ms = 0_u128,
                seed_duration_ms = 0_u128,
                linked_file_count = 0_u64,
                copied_file_count = 0_u64,
                skipped_file_count = 0_u64,
                fallback_reason = %fallback_reason,
                "cargo target seed: proceeding with cold private target dir after setup task failure"
            );
        }
    }

    destination_run_dir
}

/// Resolve the cargo workspace directory under `project_root`.
///
/// Prefers the project's `EnvironmentConfig` Rust workspace `root` (so a repo
/// whose cargo workspace lives in a subdir — djinn's `server/` — is found
/// without hardcoding). Falls back to a `Cargo.toml` at the project root or any
/// single first-level subdir. Returns `None` when no cargo workspace exists
/// (non-Rust repo) so the caller can skip the warm cleanly.
///
/// Must resolve to the SAME absolute dir task-run compiles in. Task-run
/// runs its scoped commands (`cd server && cargo …`) from `DJINN_PROJECT_ROOT`,
/// and both warm and task-run clone to `/workspace/<sanitize_id(project)>`, so a
/// dir like `<project_root>/server` lines up byte-for-byte across the two pods —
/// a prerequisite for cargo fingerprints (which embed absolute source paths) to
/// match and reuse to hit.
fn resolve_cargo_workspace_dir(
    project_root: &Path,
    env_config: Option<&djinn_stack::environment::EnvironmentConfig>,
) -> Option<PathBuf> {
    // 1. EnvironmentConfig Rust workspace root (authoritative when present).
    if let Some(cfg) = env_config {
        for ws in &cfg.workspaces {
            if ws.language.eq_ignore_ascii_case("rust") {
                let dir = project_root.join(&ws.root);
                if dir.join("Cargo.toml").is_file() {
                    return Some(dir);
                }
            }
        }
    }

    // 2. Cargo.toml at the project root.
    if project_root.join("Cargo.toml").is_file() {
        return Some(project_root.to_path_buf());
    }

    // 3. Cargo.toml one level down (djinn's `server/`).
    if let Ok(entries) = std::fs::read_dir(project_root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if dir.is_dir() && dir.join("Cargo.toml").is_file() {
                return Some(dir);
            }
        }
    }

    None
}

/// Pre-compile the cargo workspace from `main` into the warm per-project target
/// base so task-run pods seed it and recompile only their delta incrementally
/// instead of cold-building.
///
/// Runs the SAME work task-run compiles (`cargo clippy --workspace
/// --all-targets --all-features`, falling back to `cargo build`, then `cargo
/// test --workspace --all-targets --all-features --no-run`) so the artifacts +
/// fingerprints in the base actually match what task-run produces. The warm
/// pod's env already routes `CARGO_TARGET_DIR=/cache/cargo-target/<project>`
/// (the warm base) with `CARGO_INCREMENTAL=1`, so these compiles write straight
/// into the base.
///
/// Caller MUST have normalized tracked-file mtimes (`normalize_mtimes_at`)
/// first: cargo freshness keys on file mtimes, and task-run normalizes the
/// same way before it compiles — without matching mtimes the base's fingerprints
/// won't match task-run's fresh clone and reuse never hits.
///
/// Best-effort throughout: a missing cargo workspace (non-Rust repo) or any
/// compile failure logs and returns — it never fails the graph warm.
async fn warm_cargo_target_base(
    project_id: &str,
    project_root: &Path,
    policy: &cargo_cache_policy::CargoCachePolicy,
) {
    let Some(workspace_dir) = resolve_cargo_workspace_dir(project_root, None) else {
        info!(
            project_id,
            project_root = %project_root.display(),
            "cargo warm: no cargo workspace found; skipping (non-Rust repo?)"
        );
        return;
    };

    // Canonicalize once so every structured event and metric for this warm
    // shares the same absolute workspace_dir. Cargo fingerprints embed
    // absolute source paths, so the canonical form is what task-run will
    // also resolve to at task-run time — emitting it here is what lets the
    // coordinator health sweep correlate the warm path with the run path.
    let workspace_dir = match std::fs::canonicalize(&workspace_dir) {
        Ok(canonical) => canonical,
        Err(err) => {
            warn!(
                project_id,
                workspace_dir = %workspace_dir.display(),
                error = %err,
                "cargo warm: failed to canonicalize resolved workspace dir; \
                 emitting events with the unresolved path"
            );
            workspace_dir
        }
    };

    let target_dir = std::env::var(CARGO_TARGET_DIR_ENV).unwrap_or_default();
    info!(
        project_id,
        workspace_dir = %workspace_dir.display(),
        cargo_target_dir = %target_dir,
        "cargo warm: compiling main into the warm per-project target base"
    );
    // Surface the resolved workspace dir as a low-cardinality metric the
    // coordinator health sweep can read alongside the warm-step counter.
    cargo_metrics::record_resolved_workspace_dir(
        project_id,
        workspace_dir.to_string_lossy().as_ref(),
    );

    // Stamp the warm base BEFORE compiling. The compile refreshes the mtime of
    // every artifact it actually uses, so a post-compile `cargo sweep --file`
    // can safely delete everything older than this stamp — stale crate versions
    // cargo accumulates in `deps/` (it never GCs a target dir) plus orphaned
    // `incremental/` sessions. Best-effort: on images without cargo-sweep the
    // stamp fails, `sweep_stamped` stays false, and the whole prune no-ops.
    let sweep_stamped =
        run_cargo_sweep_step(project_id, &workspace_dir, &["--stamp"], "sweep-stamp").await;

    let started = djinn_core::clock::Clock::now_instant(&djinn_core::clock::SystemClock::new());

    let commands = &policy.warm_commands;
    if commands.is_empty() {
        info!(
            project_id,
            "cargo warm: no warm commands in policy; skipping"
        );
        return;
    }

    // clippy is the heavier of the two passes and produces the same
    // check artifacts; fall back to a plain build if clippy is unavailable.
    // Each command carries its own feature_args — no policy.features()
    // chaining needed (and omitted to avoid double-adding features in the
    // dual-pass warm design).
    let clippy_args: Vec<String> = commands[0]
        .args
        .iter()
        .cloned()
        .chain(commands[0].feature_args.iter().cloned())
        .collect();
    let clippy_ok = run_cargo_warm_step(
        project_id,
        &workspace_dir,
        &clippy_args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        commands[0].label,
    )
    .await;
    // Track whether ANY warm step compiled successfully. The post-compile
    // `--file` sweep only runs when at least one step succeeded, so a fully-red
    // branch (nothing compiles → nothing refreshed) never prunes a still-good
    // base down to a cold rebuild.
    let mut any_step_ok = clippy_ok;

    // Run remaining warm commands (all-features clippy, default-features
    // clippy, build fallback, test --no-run, etc.), skipping the first
    // (already ran). The build fallback is skipped when clippy succeeded.
    for cmd in commands.iter().skip(1) {
        // Skip the build fallback if clippy already succeeded — it's only
        // needed when clippy fails.
        if !clippy_ok
            && cmd.label == "build (clippy fallback)"
            && commands[0].label.starts_with("clippy")
        {
            // Already ran clippy (which failed); fall through to build.
        } else if clippy_ok && cmd.label == "build (clippy fallback)" {
            // clippy succeeded; build fallback is not needed.
            continue;
        }

        let args: Vec<String> = cmd
            .args
            .iter()
            .cloned()
            .chain(cmd.feature_args.iter().cloned())
            .collect();
        any_step_ok |= run_cargo_warm_step(
            project_id,
            &workspace_dir,
            &args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            cmd.label,
        )
        .await;
    }

    let elapsed = started.elapsed();
    cargo_metrics::record_warm_base_freshness(project_id, elapsed.as_millis() as u64);

    // Prune the warm base of everything the compile above did not touch. Safe by
    // construction: cargo-sweep only removes whole artifact files older than the
    // stamp, and cargo transparently rebuilds anything genuinely needed on the
    // next warm — it can never leave a corrupt/half cache. Gated on a successful
    // stamp AND at least one green step so a broken branch keeps its warm base.
    if sweep_stamped && any_step_ok {
        run_cargo_sweep_step(project_id, &workspace_dir, &["--file"], "sweep-file").await;
    } else {
        info!(
            project_id,
            sweep_stamped,
            any_step_ok,
            "cargo warm: skipping warm-base sweep (no stamp or no successful compile step)"
        );
    }

    info!(
        project_id,
        workspace_dir = %workspace_dir.display(),
        elapsed_ms = elapsed.as_millis() as u64,
        "cargo warm: warm target base compile complete"
    );
}

/// Run one `cargo <args>` step inside `workspace_dir` for the warm base.
/// Returns `true` on success. Never panics; logs failures as warnings so a
/// compile error can't abort the graph warm.
///
/// Emits a structured tracing event with the resolved absolute
/// `workspace_dir`, the full `cargo_command` argv, the `step_label`, and the
/// terminal `seed_outcome` ("ok" / "failed" / "spawn_error") so the
/// coordinator health sweep can correlate warm-step outcomes with workspace
/// paths and command shapes. The metric `djinn_cargo_warm_step_total` is
/// incremented with bounded `project_id`, `step`, and `outcome` labels.
async fn run_cargo_warm_step(
    project_id: &str,
    workspace_dir: &Path,
    args: &[&str],
    label: &str,
) -> bool {
    run_cargo_warm_step_with_cargo("cargo", project_id, workspace_dir, args, label).await
}

/// Run `cargo sweep <args>` inside `workspace_dir` to prune the warm target
/// base. Returns `true` on success. Best-effort and non-fatal like the warm
/// steps: a non-zero exit (e.g. cargo-sweep not installed on an older image →
/// "no such subcommand: sweep") or a spawn error logs and returns `false`, so
/// the warm proceeds and simply skips pruning. cargo-sweep resolves the target
/// dir via `cargo metadata`, which honors the inherited `CARGO_TARGET_DIR`, so
/// it operates on the same warm base the compile wrote to.
async fn run_cargo_sweep_step(
    project_id: &str,
    workspace_dir: &Path,
    args: &[&str],
    label: &str,
) -> bool {
    run_cargo_sweep_step_with_cargo("cargo", project_id, workspace_dir, args, label).await
}

async fn run_cargo_sweep_step_with_cargo(
    cargo_bin: impl AsRef<OsStr>,
    project_id: &str,
    workspace_dir: &Path,
    args: &[&str],
    label: &str,
) -> bool {
    let sweep_command = format!("cargo sweep {}", args.join(" "));
    let workspace_dir_display = workspace_dir.display().to_string();
    match tokio::process::Command::new(cargo_bin.as_ref())
        .arg("sweep")
        .args(args)
        .current_dir(workspace_dir)
        .status()
        .await
    {
        Ok(status) if status.success() => {
            info!(
                project_id,
                workspace_dir = %workspace_dir_display,
                sweep_command = %sweep_command,
                step_label = label,
                sweep_outcome = "ok",
                "cargo warm: sweep step succeeded"
            );
            true
        }
        Ok(status) => {
            warn!(
                project_id,
                workspace_dir = %workspace_dir_display,
                sweep_command = %sweep_command,
                step_label = label,
                sweep_outcome = "failed",
                code = ?status.code(),
                "cargo warm: sweep step failed (non-fatal; warm base left unpruned)"
            );
            false
        }
        Err(e) => {
            warn!(
                project_id,
                workspace_dir = %workspace_dir_display,
                sweep_command = %sweep_command,
                step_label = label,
                sweep_outcome = "spawn_error",
                error = %e,
                "cargo warm: could not spawn cargo-sweep (absent on this image?); \
                 warm base left unpruned"
            );
            false
        }
    }
}

async fn run_cargo_warm_step_with_cargo(
    cargo_bin: impl AsRef<OsStr>,
    project_id: &str,
    workspace_dir: &Path,
    args: &[&str],
    label: &str,
) -> bool {
    let cargo_instrumented = cargo_instrument_enabled();
    let plan = cargo_warm_execution_plan(args, cargo_instrumented);
    let cargo_command = format!("cargo {}", plan.args.join(" "));
    let workspace_dir_display = workspace_dir.display().to_string();

    if !cargo_instrumented {
        return match tokio::process::Command::new(cargo_bin.as_ref())
            .args(&plan.args)
            .current_dir(workspace_dir)
            .status()
            .await
        {
            Ok(status) if status.success() => {
                info!(
                    project_id,
                    workspace_dir = %workspace_dir_display,
                    cargo_command = %cargo_command,
                    step_label = label,
                    seed_outcome = "ok",
                    "cargo warm: step succeeded"
                );
                cargo_metrics::record_warm_step(
                    project_id,
                    label,
                    djinn_telemetry::cargo_warm_step::OUTCOME_OK,
                );
                true
            }
            Ok(status) => {
                warn!(
                    project_id,
                    workspace_dir = %workspace_dir_display,
                    cargo_command = %cargo_command,
                    step_label = label,
                    seed_outcome = "failed",
                    code = ?status.code(),
                    "cargo warm: step failed (non-fatal; continuing warm)"
                );
                cargo_metrics::record_warm_step(
                    project_id,
                    label,
                    djinn_telemetry::cargo_warm_step::OUTCOME_FAILED,
                );
                false
            }
            Err(err) => {
                warn!(
                    project_id,
                    workspace_dir = %workspace_dir_display,
                    cargo_command = %cargo_command,
                    step_label = label,
                    seed_outcome = "spawn_error",
                    error = %err,
                    "cargo warm: failed to spawn `cargo` (non-fatal; continuing warm)"
                );
                cargo_metrics::record_warm_step(
                    project_id,
                    label,
                    djinn_telemetry::cargo_warm_step::OUTCOME_SPAWN_ERROR,
                );
                false
            }
        };
    }

    match tokio::process::Command::new(cargo_bin.as_ref())
        .args(&plan.args)
        .current_dir(workspace_dir)
        .output()
        .await
    {
        Ok(output) => {
            if cargo_instrumented {
                let (stdout_fresh_count, stdout_compiling_count) =
                    cargo_fresh_compiling_counts(&output.stdout);
                let (stderr_fresh_count, stderr_compiling_count) =
                    cargo_fresh_compiling_counts(&output.stderr);
                let fresh_count = stdout_fresh_count + stderr_fresh_count;
                let compiling_count = stdout_compiling_count + stderr_compiling_count;

                info!(
                    project_id,
                    step = label,
                    step_label = label,
                    workspace_dir = %workspace_dir_display,
                    fresh_count,
                    compiling_count,
                    "cargo warm: instrumented Fresh/Compiling counts"
                );
                djinn_telemetry::cargo_cache::record_warm_step_fresh_count(
                    project_id,
                    label,
                    fresh_count,
                );
                djinn_telemetry::cargo_cache::record_warm_step_compiling_count(
                    project_id,
                    label,
                    compiling_count,
                );
            }

            if output.status.success() {
                info!(
                    project_id,
                    workspace_dir = %workspace_dir_display,
                    cargo_command = %cargo_command,
                    step_label = label,
                    seed_outcome = "ok",
                    "cargo warm: step succeeded"
                );
                cargo_metrics::record_warm_step(
                    project_id,
                    label,
                    djinn_telemetry::cargo_warm_step::OUTCOME_OK,
                );
                true
            } else {
                warn!(
                    project_id,
                    workspace_dir = %workspace_dir_display,
                    cargo_command = %cargo_command,
                    step_label = label,
                    seed_outcome = "failed",
                    code = ?output.status.code(),
                    "cargo warm: step failed (non-fatal; continuing warm)"
                );
                cargo_metrics::record_warm_step(
                    project_id,
                    label,
                    djinn_telemetry::cargo_warm_step::OUTCOME_FAILED,
                );
                false
            }
        }
        Err(err) => {
            warn!(
                project_id,
                workspace_dir = %workspace_dir_display,
                cargo_command = %cargo_command,
                step_label = label,
                seed_outcome = "spawn_error",
                error = %err,
                "cargo warm: failed to spawn `cargo` (non-fatal; continuing warm)"
            );
            cargo_metrics::record_warm_step(
                project_id,
                label,
                djinn_telemetry::cargo_warm_step::OUTCOME_SPAWN_ERROR,
            );
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CargoWarmOutputMode {
    InheritStatusOnly,
    CaptureForInstrumentation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoWarmExecutionPlan {
    args: Vec<String>,
    output_mode: CargoWarmOutputMode,
}

fn cargo_warm_execution_plan(args: &[&str], instrumented: bool) -> CargoWarmExecutionPlan {
    let mut planned_args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let output_mode = if instrumented {
        planned_args.push("-v".to_owned());
        CargoWarmOutputMode::CaptureForInstrumentation
    } else {
        CargoWarmOutputMode::InheritStatusOnly
    };

    CargoWarmExecutionPlan {
        args: planned_args,
        output_mode,
    }
}

fn cargo_fresh_compiling_counts(output: &[u8]) -> (usize, usize) {
    let (f, c) = parse_cargo_fresh_compiling(&String::from_utf8_lossy(output));
    (f as usize, c as usize)
}

/// Parse cargo `-v` stdout/stderr for `Fresh` and `Compiling` line prefixes.
///
/// This is intentionally a pure `&str → (u64, u64)` function so it can be
/// unit-tested with static strings without spawning any processes.
fn parse_cargo_fresh_compiling(output: &str) -> (u64, u64) {
    let mut fresh_count: u64 = 0;
    let mut compiling_count: u64 = 0;

    for line in output.lines() {
        match line.split_whitespace().next() {
            Some("Fresh") => fresh_count += 1,
            Some("Compiling") => compiling_count += 1,
            _ => {}
        }
    }

    (fresh_count, compiling_count)
}

/// Returns `true` when `DJINN_CARGO_INSTRUMENT` is set in the environment,
/// indicating that cargo warm steps should capture stdout/stderr for
/// Fresh/Compiling line parsing.  Extracted so tests can verify the toggle
/// without spawning a real cargo process.
fn cargo_instrument_enabled() -> bool {
    std::env::var("DJINN_CARGO_INSTRUMENT").is_ok()
}

struct CargoTargetRunDirGuard {
    task_run_id: String,
    project_id: String,
    run_dir: PathBuf,
}

impl CargoTargetRunDirGuard {
    fn new(task_run_id: String, project_id: String, run_dir: PathBuf) -> Self {
        Self {
            task_run_id,
            project_id,
            run_dir,
        }
    }
}

impl Drop for CargoTargetRunDirGuard {
    fn drop(&mut self) {
        match teardown_run_dir(&self.run_dir) {
            Ok(result) => info!(
                task_run_id = %self.task_run_id,
                project_id = %self.project_id,
                destination_run_dir = %self.run_dir.display(),
                cleanup_outcome = result.outcome(),
                removed_count = result.removed_count(),
                error_count = 0_u64,
                "cargo target teardown: private run target dir cleanup completed"
            ),
            Err(err) => warn!(
                task_run_id = %self.task_run_id,
                project_id = %self.project_id,
                destination_run_dir = %self.run_dir.display(),
                cleanup_outcome = "failed",
                removed_count = 0_u64,
                error_count = 1_u64,
                error = %err,
                "cargo target teardown: failed to remove private run target dir"
            ),
        }
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

    let cargo_target_run_dir = prepare_cargo_target_dir(&spec, &args.workspace_path).await;
    let _cargo_target_guard = CargoTargetRunDirGuard::new(
        spec.task_run_id.clone(),
        spec.project_id.clone(),
        cargo_target_run_dir,
    );

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
    let role_keys: Vec<&'static str> = credentials.roles().copied().map(RoleKind::as_str).collect();
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

    // The live ephemeral stage clone's path, populated by
    // `WorkerSupervisorServices::execute_stage` on its first call and read
    // lazily by the SIGTERM / soft-deadline checkpoint. Created here so the
    // checkpoint handlers (wired below, BEFORE the RPC connect / services
    // construction so cancel-on-SIGTERM is armed as early as possible) and the
    // services impl (constructed after the RPC connect) share the same slot.
    let captured_workspace_path: Arc<std::sync::Mutex<Option<PathBuf>>> =
        Arc::new(std::sync::Mutex::new(None));

    // Identity used by the SIGTERM / soft-deadline checkpoint, mirroring the
    // supervisor's post-stage auto-commit: attribute to the task creator,
    // falling back to the bot for system/patrol tasks (or host/worker skew).
    let checkpoint_identity = CheckpointIdentity {
        name: spec
            .commit_author_name
            .clone()
            .unwrap_or_else(|| "djinn-bot".to_string()),
        email: spec
            .commit_author_email
            .clone()
            .unwrap_or_else(|| "bot@djinn.local".to_string()),
    };

    // Wire SIGTERM / SIGINT into the supervisor's cancel token so the existing
    // `finalize_interrupted` path runs and flushes a terminal
    // `update_task_run_status(Interrupted)` RPC back to the host before the
    // Pod exits. Without this, K8s' `activeDeadlineSeconds` / eviction /
    // graceful-drain SIGTERM kills the runtime mid-flight and the host's
    // task_runs row stays `running` forever.
    //
    // Each signal ALSO triggers a best-effort checkpoint (commit + push of
    // task_branch) so a mid-stage kill doesn't strand the worker's in-flight
    // commits in the ephemeral clone — the supervisor's own post-stage push
    // only runs at a stage boundary, which a cancelled mid-stage run never
    // reaches. The K8s Job sets `terminationGracePeriodSeconds=60`, the window
    // we have to checkpoint + drain the RPC before SIGKILL hits.
    install_termination_handlers(
        cancel.clone(),
        args.task_run_id.clone(),
        captured_workspace_path.clone(),
        spec.task_branch.clone(),
        checkpoint_identity.clone(),
    );

    // Clones reserved for the terminal "save before teardown" checkpoint fired
    // after the supervisor returns (see below). `checkpoint_identity` is moved
    // into the soft-deadline handler and `captured_workspace_path` into the
    // supervisor services, so capture our copies before those moves.
    let terminal_checkpoint_identity = checkpoint_identity.clone();
    let terminal_captured_workspace_path = captured_workspace_path.clone();

    // Arm the in-pod soft deadline. The kubelet's `activeDeadlineSeconds` is a
    // hard backstop that SIGKILLs the Pod with no chance to save work; the soft
    // deadline fires `margin` ahead of it and drives the SAME graceful path as
    // SIGTERM (cancel + checkpoint), so the healthy slow-model case winds itself
    // down rather than being hard-killed. `DJINN_TASK_RUN_DEADLINE_SECONDS` is
    // set by `build_task_run_job` from the same config value the Job carries;
    // absent/unparseable → no soft deadline (the kubelet backstop still applies).
    install_soft_deadline(
        cancel.clone(),
        args.task_run_id.clone(),
        captured_workspace_path.clone(),
        spec.task_branch.clone(),
        checkpoint_identity,
    );

    // OOM kills are SIGKILL: no signal, no soft-deadline fire, no checkpoint —
    // the pod just vanishes with its ephemeral clone. Production task pods were
    // repeatedly OOM-killed mid-stage (rust-analyzer + rustc + rust-lld blowing
    // the cgroup limit), losing 30-60 min of in-flight work. The periodic push
    // is the OOM-proof backstop: every ~3 min it pushes whatever the worker has
    // ALREADY committed (via its own shell) to the mirror, so an unsignalled
    // SIGKILL strands at most one interval of committed work. It is PUSH-ONLY
    // (no commit) so it never mutates HEAD/index under the live agent.
    install_periodic_push(
        cancel.clone(),
        args.task_run_id.clone(),
        captured_workspace_path.clone(),
        spec.task_branch.clone(),
    );

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

    // 4a. Bootstrap the in-Pod database.  The pre-task boundary emits one
    //     `task_run_pretask_ran` activity event per started command into the
    //     same `activity_log` table the host-side runtime writes to, so the
    //     DB handle must be ready BEFORE the boundary runs.  The handle is
    //     reused below for `WorkerSupervisorServices` (via `AgentContext`).
    let in_pod_db = bootstrap_warm_database()
        .await
        .context("bootstrap in-Pod database for pre-task activity")?;
    let pretask_activity_sink =
        lifecycle::TaskRepositoryActivitySink::from_database(in_pod_db.clone());

    // 4b. Pre-task startup boundary: load the effective EnvironmentConfig
    //     and resolved service metadata from the hgd0 Secret-backed mounts,
    //     then check service readiness and run pre-task commands.  This must
    //     complete before supervisor dispatch so the workspace is fully
    //     prepared when the agent session starts.  A blocking pre-task
    //     failure surfaces as an error for environmental non-attempt
    //     classification (c9l4).  One `task_run_pretask_ran` activity event
    //     is emitted per started command; if readiness fails the runner is
    //     never reached and no event is recorded (the explicit readiness
    //     error path wins).
    let boundary_result = lifecycle::execute_task_run_startup_boundary(
        workspace.path(),
        &cancel,
        Some(&spec.task_id),
        &pretask_activity_sink,
    )
    .await;

    // 4c. Classify blocking pre-task and service readiness failures as
    //     environmental non-attempts.  When the startup boundary fails,
    //     no `TaskRunSupervisor::run` is invoked — no agent session or
    //     work attempt is created, and no quality/arbiter/park penalties
    //     are applied.  The worker emits a `TerminalReport` with an
    //     `EnvironmentalNonAttempt` outcome and exits cleanly so the host
    //     can classify the run accordingly.
    let (pre_task_inputs, pretask_result) = match boundary_result {
        Ok(ok) => ok,
        Err(e) => {
            let reason = classify_environmental_failure(&e);
            warn!(
                error = %e,
                classification = %reason,
                "pre-task startup boundary failed; emitting environmental non-attempt report"
            );
            let report = TaskRunReport {
                task_run_id: spec.task_run_id.clone(),
                outcome: TaskRunOutcome::EnvironmentalNonAttempt {
                    reason: reason.to_string(),
                },
                stages_completed: Vec::new(),
            };
            if let Err(emit_err) = rpc.emit_event(WorkerEvent::TerminalReport(report)).await {
                warn!(
                    error = %emit_err,
                    "failed to emit EnvironmentalNonAttempt TerminalReport; \
                     launcher will fall back to Job-status polling"
                );
            }
            // Shut down RPC cleanly before exiting.
            drop(rpc);
            let _ = background.writer.await;
            cancel.cancel();
            let _ = background.reader.await;
            drop(workspace);
            return Ok(());
        }
    };
    info!(
        pre_task_count = pre_task_inputs.environment_config.lifecycle.pre_task.len(),
        injected_services = pre_task_inputs.service_metadata.injected.len(),
        pretask_all_succeeded = pretask_result.all_succeeded(),
        "pre-task startup boundary complete"
    );

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
    let agent_context = build_worker_agent_context(in_pod_db, rpc.clone(), spec.project_id.clone());
    let worker_services: Arc<dyn SupervisorServices> = Arc::new(WorkerSupervisorServices::new(
        rpc.clone(),
        credentials,
        cancel.clone(),
        agent_context,
        captured_workspace_path,
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
    let report_result = supervisor
        .run_for_orderly_shutdown(spec.clone())
        .await
        .context("task-run supervisor drive");

    // Terminal "save before teardown" checkpoint. The supervisor only
    // commits+pushes at a clean stage boundary, so an exit that breaks out
    // mid-stage — a provider 401 / `Failed` stage outcome, a cancelled stage,
    // or a drive-level error — would otherwise strand the worker's uncommitted
    // in-flight edits (plus anything committed since the last ~180s periodic
    // push) in the ephemeral clone. The SIGTERM and soft-deadline handlers
    // checkpoint on their paths; this covers the in-process failure/interrupt
    // exit that neither signal fires on, so a re-dispatch resumes from the work
    // instead of redoing it. Idempotent: a no-op commit on a clean tree
    // (successful runs already committed at the stage boundary) and a no-op push
    // when the mirror is current. MUST run before `drop(supervisor)` below,
    // which deletes the ephemeral stage clone the checkpoint reads from.
    checkpoint_workspace(
        &terminal_captured_workspace_path,
        &spec.task_branch,
        &terminal_checkpoint_identity,
        &args.task_run_id,
        checkpoint::CheckpointReason::Terminal,
    )
    .await;

    let report = report_result?;

    // 8. Ship the terminal report back to the launcher as a `WorkerEvent::
    //    TerminalReport` on the same RPC connection. The launcher's
    //    `KubernetesRuntime::teardown` drains the pending connection's event
    //    channel looking for this frame and uses it as the authoritative
    //    terminal report, falling back to Job-status polling only if the
    //    stream closes without emitting one. Best-effort: if the writer
    //    task already exited (e.g. the launcher tore the connection down
    //    first) we log the drop but still exit zero — the Job-status
    //    fallback on the launcher side covers that case.
    if let Err(e) = rpc.emit_event(WorkerEvent::TerminalReport(report)).await {
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

/// Classify an [`anyhow::Error`] from [`lifecycle::execute_task_run_startup_boundary`]
/// into a machine-readable environmental non-attempt reason string.
///
/// The classification is used in the `EnvironmentalNonAttempt::reason` field
/// of the terminal report so the host can distinguish pre-task failures from
/// service readiness failures without parsing the error message.
fn classify_environmental_failure(err: &anyhow::Error) -> &'static str {
    let msg = format!("{err:#}");
    // Service readiness failures come from `check_service_readiness` inside
    // the startup boundary.  The readiness stub returns a generic error; real
    // implementations surface TCP/port probe failures.  Check the message
    // for readiness-related keywords.
    if msg.contains("readiness") || msg.contains("service") || msg.contains("sidecar") {
        return "service_readiness_failed";
    }
    // Blocking pre-task command failures come from `run_pre_task_commands`
    // returning `PreTaskCommandsResult::Blocked`, which the startup boundary
    // converts into an anyhow error containing "blocking command failed".
    if msg.contains("timed_out=true") || msg.contains("timed out") {
        return "pre_task_timed_out";
    }
    if msg.contains("cancelled=true") {
        return "pre_task_cancelled";
    }
    // Default: generic pre-task failure (non-zero exit code, spawn error, etc.)
    "pre_task_failed"
}

/// Commit author/committer identity for the wind-down checkpoint. Owned
/// `String`s (not the borrowed [`GitIdentity`]) so it can be cloned into the
/// detached signal / deadline tasks.
#[derive(Clone)]
struct CheckpointIdentity {
    name: String,
    email: String,
}

/// Safety-scanned "save work before we die" checkpoint: scan the LIVE
/// ephemeral stage clone via the shared [`checkpoint::preserve_checkpoint`]
/// path, create a WIP commit with only safety-approved files, and push
/// `task_branch` with lease-aware bounded retry. All bounded by
/// [`CHECKPOINT_TIMEOUT`].
///
/// The path is read lazily, at fire time, from the `captured_workspace_path`
/// slot that [`WorkerSupervisorServices`] populates on its first
/// `execute_stage` call. That is the supervisor's own ephemeral `TempDir`
/// clone (`MirrorManager::clone_ephemeral`), where every worker commit lands —
/// NOT the host-materialised `/workspace` bind mount, which the in-pod
/// supervisor never writes to. If no stage has started yet the slot is `None`
/// and there is no in-flight work to save, so the checkpoint is a clean no-op.
///
/// Called from the SIGTERM handler, the soft-deadline timer, and the terminal
/// checkpoint after the supervisor returns. All callers route through this
/// single shared preservation path. It races the supervisor's own (cancelled)
/// shutdown, so it may arrive mid-git-operation — a locked index, a
/// half-applied merge. Every failure is logged and swallowed; this function
/// never panics and never blocks past its timeout, because it runs inside the
/// kubelet's short `terminationGracePeriodSeconds` window and must leave room
/// for the terminal RPC flush.
///
/// Returns the structured [`checkpoint::CheckpointPreservationResult`] for
/// callers that need to inspect the outcome (events, metrics, persistence).
async fn checkpoint_workspace(
    captured_workspace_path: &std::sync::Mutex<Option<PathBuf>>,
    task_branch: &str,
    identity: &CheckpointIdentity,
    task_run_id: &str,
    reason: checkpoint::CheckpointReason,
) -> checkpoint::CheckpointPreservationResult {
    let workspace_path = {
        captured_workspace_path
            .lock()
            .expect("captured_workspace_path mutex poisoned")
            .clone()
    };
    let Some(workspace_path) = workspace_path else {
        info!(
            task_run_id,
            branch = task_branch,
            reason = %reason,
            "checkpoint: no stage has started (no captured workspace); nothing to save"
        );
        return checkpoint::CheckpointPreservationResult::default();
    };

    // The session ID is the task-run ID for this worker binary (each Pod
    // runs exactly one task-run). The turn number is unknown at the worker
    // level — the reply-loop turn counter lives in the coordinator/agent,
    // not here. `turn_known: false` in the result distinguishes this.
    let metadata = checkpoint::CheckpointMetadata {
        session_id: task_run_id.to_string(),
        task_run_id: task_run_id.to_string(),
        turn: None,
        reason,
        author_name: identity.name.clone(),
        author_email: identity.email.clone(),
        task_branch: task_branch.to_string(),
    };

    let result = tokio::time::timeout(
        CHECKPOINT_TIMEOUT,
        checkpoint::preserve_checkpoint(&workspace_path, &metadata, None),
    )
    .await;

    match result {
        Ok(result) => {
            info!(
                task_run_id,
                branch = task_branch,
                reason = %reason,
                outcome = %result.outcome,
                commit_attempted = result.commit_attempted,
                commit_succeeded = result.commit_succeeded,
                commit_sha = ?result.commit_sha,
                parent_sha = ?result.parent_sha,
                local_sha = ?result.local_sha,
                remote_sha = ?result.remote_sha,
                target_ref = %result.target_ref,
                push_attempts = result.push_attempts,
                conflict_strategy = %result.conflict_strategy,
                push_result = %result.push_result,
                staged_count = result.staged_count,
                excluded_count = result.excluded_count,
                blocked_count = result.blocked_count,
                had_changes = result.had_changes,
                failure_reason = ?result.failure_reason,
                "checkpoint preservation complete"
            );
            result
        }
        Err(_) => {
            error!(
                task_run_id,
                branch = task_branch,
                reason = %reason,
                timeout_secs = CHECKPOINT_TIMEOUT.as_secs(),
                "checkpoint: timed out (wedged git operation?); in-flight work may be LOST"
            );
            checkpoint::CheckpointPreservationResult {
                outcome: checkpoint::PreservationOutcome::TimedOut,
                failure_reason: Some("checkpoint timed out".to_string()),
                ..checkpoint::CheckpointPreservationResult::default()
            }
        }
    }
}

/// Spawn background listeners for SIGTERM and SIGINT that flip `cancel` AND run
/// the wind-down checkpoint when the kubelet (or operator) signals shutdown.
///
/// The supervisor body checks `cancel` between stages, exits its for-loop with
/// `Interrupted`, and runs `finalize_interrupted` — which calls
/// `update_task_run_status(Interrupted)` over the still-live RPC channel. The
/// checkpoint (commit + push of `task_branch`) runs alongside that so a
/// mid-stage kill doesn't strand the worker's commits in the ephemeral clone;
/// the supervisor's own post-stage push only fires at a stage boundary, which a
/// cancelled mid-stage run never reaches. The Pod's
/// `terminationGracePeriodSeconds=60` gives both the checkpoint and the RPC
/// flush time to land before SIGKILL.
///
/// `captured_workspace_path` is the slot [`WorkerSupervisorServices`] shares;
/// it is read lazily inside [`checkpoint_workspace`] at signal time, so the
/// cancel wiring can stay early (before the RPC connect / services
/// construction) while the checkpoint still sees the live ephemeral stage clone
/// the supervisor records once stages begin.
fn install_termination_handlers(
    cancel: CancellationToken,
    task_run_id: String,
    captured_workspace_path: Arc<std::sync::Mutex<Option<PathBuf>>>,
    task_branch: String,
    identity: CheckpointIdentity,
) {
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
        let captured_workspace_path = captured_workspace_path.clone();
        let task_branch = task_branch.clone();
        let identity = identity.clone();
        tokio::spawn(async move {
            if stream.recv().await.is_some() {
                info!(
                    signal = label,
                    task_run_id = %task_run_id,
                    "received termination signal; cancelling supervisor + checkpointing work"
                );
                // Cancel first so the supervisor stops streaming and starts its
                // own graceful exit; then checkpoint to save in-flight work.
                cancel.cancel();
                checkpoint_workspace(
                    &captured_workspace_path,
                    &task_branch,
                    &identity,
                    &task_run_id,
                    checkpoint::CheckpointReason::Signal,
                )
                .await;
            }
        });
    }
}

/// Compute when the soft deadline should fire from the configured Job
/// `activeDeadlineSeconds`: `deadline - margin`, clamped to at least
/// [`SOFT_DEADLINE_MIN`] so a small configured deadline (tests, tuned-down
/// installs) doesn't underflow to zero and fire immediately at startup.
fn soft_deadline_interval(deadline_secs: u64) -> Duration {
    Duration::from_secs(deadline_secs)
        .saturating_sub(SOFT_DEADLINE_MARGIN)
        .max(SOFT_DEADLINE_MIN)
}

/// Arm an in-pod soft deadline at `configured_deadline - margin`. When it
/// fires it drives the same graceful wind-down as SIGTERM (cancel + checkpoint)
/// so a healthy-but-slow run saves its work and exits `Interrupted` BEFORE the
/// kubelet's hard `activeDeadlineSeconds` SIGKILLs the Pod. An interrupted run
/// is NOT fed to the host's model circuit-breaker (it is not a model failure),
/// so a slow model isn't penalised for hitting the wall.
///
/// Reads `DJINN_TASK_RUN_DEADLINE_SECONDS` (set by `build_task_run_job` from
/// the same config the Job's `activeDeadlineSeconds` carries). Absent or
/// unparseable → no soft deadline; the kubelet backstop still applies. The
/// armed interval is clamped to at least [`SOFT_DEADLINE_MIN`] so small
/// configured deadlines (tests) don't fire immediately at startup.
fn install_soft_deadline(
    cancel: CancellationToken,
    task_run_id: String,
    captured_workspace_path: Arc<std::sync::Mutex<Option<PathBuf>>>,
    task_branch: String,
    identity: CheckpointIdentity,
) {
    let deadline_secs = match std::env::var("DJINN_TASK_RUN_DEADLINE_SECONDS") {
        Ok(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(e) => {
                warn!(
                    value = %v,
                    error = %e,
                    "DJINN_TASK_RUN_DEADLINE_SECONDS not a valid u64; soft deadline disabled"
                );
                return;
            }
        },
        Err(_) => {
            // No deadline plumbed (out-of-cluster test, older host). The
            // kubelet's activeDeadlineSeconds (if any) is the only backstop.
            return;
        }
    };

    let fire_after = soft_deadline_interval(deadline_secs);

    info!(
        task_run_id = %task_run_id,
        deadline_secs,
        soft_deadline_secs = fire_after.as_secs(),
        "armed in-pod soft deadline"
    );

    tokio::spawn(async move {
        tokio::select! {
            // Already winding down via SIGTERM / normal completion — stand down.
            _ = cancel.cancelled() => {}
            _ = tokio::time::sleep(fire_after) => {
                warn!(
                    task_run_id = %task_run_id,
                    soft_deadline_secs = fire_after.as_secs(),
                    "soft deadline reached; winding down (cancel + checkpoint) before the kubelet hard-kills the Pod"
                );
                cancel.cancel();
                checkpoint_workspace(
                    &captured_workspace_path,
                    &task_branch,
                    &identity,
                    &task_run_id,
                    checkpoint::CheckpointReason::SoftDeadline,
                )
                .await;
            }
        }
    });
}

/// Decide whether the periodic push loop should push this tick.
///
/// Pure so it can be unit-tested without git: push iff the local `task_branch`
/// SHA differs from the last SHA this loop successfully pushed. A `None`
/// `last_pushed` (first observed SHA this run) always pushes — we can't assume
/// the mirror is current, and the push refspec is idempotent if it happens to
/// be. Equal SHAs skip (nothing new since the last successful push).
fn push_needed(current: &str, last_pushed: Option<&str>) -> bool {
    match last_pushed {
        Some(prev) => prev != current,
        None => true,
    }
}

/// Resolve the local SHA of `branch` in the workspace at `path` via
/// `git rev-parse`. Returns `None` on any failure (branch not yet created, git
/// busy mid-operation, etc.) — the periodic push loop treats that as "skip this
/// tick" and retries on the next one.
///
/// Delegates to [`djinn_git::run_git_command_in`] which applies
/// `safe.directory=*` and lowers process priority.
async fn resolve_branch_sha(path: &Path, branch: &str) -> Option<String> {
    let output = djinn_git::run_git_command_in(
        path,
        vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            // `<branch>^{commit}` resolves the local branch ref to its commit SHA
            // and fails cleanly (non-zero, empty stdout under --quiet) if the ref
            // doesn't exist yet.
            format!("{branch}^{{commit}}"),
        ],
    )
    .await
    .ok()?;
    let sha = output.stdout.trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

/// Spawn the periodic push loop: every [`PERIODIC_PUSH_INTERVAL`] push
/// `task_branch` to the mirror IF the local branch head moved since the last
/// successful push. This is the OOM-proof durability backstop.
///
/// OOM kills are SIGKILL — no signal reaches the pod, so neither the SIGTERM
/// handler nor the soft-deadline timer (which both run [`checkpoint_workspace`])
/// gets a chance to save work. This loop closes that gap by pushing
/// already-committed work on a cadence, so an unsignalled SIGKILL strands at
/// most one interval of committed commits in the (now-gone) ephemeral clone.
///
/// PUSH ONLY — this deliberately does NOT commit. Workers commit their own work
/// via shell frequently during a session, so pushing those commits captures
/// most of the value with zero behavioural risk. A periodic auto-commit would
/// mutate HEAD/index under the live agent mid-edit (the model runs `git status`
/// / `git diff` and would get confused; half-staged states would freeze), so it
/// is explicitly out of scope — the SIGTERM/soft-deadline checkpoint already
/// handles uncommitted changes for signal deaths. Do NOT "improve" this into an
/// add/commit: `git push` is read-only on the working tree (it reads
/// refs/objects only), which is exactly why it needs no locking against cargo or
/// the worker's own git invocations.
///
/// Behaviour per tick:
/// - slot is `None` (no stage started yet) → skip silently.
/// - `git rev-parse` fails (branch absent, git mid-operation) → warn-and-skip,
///   retry next tick.
/// - SHA unchanged since the last successful push → skip (debug log).
/// - SHA changed → `push_to_origin(task_branch)`; on success record the SHA and
///   log info; on failure warn and leave `last_pushed` unchanged so the next
///   tick retries. A non-fast-forward rejection (the mirror is somehow ahead —
///   shouldn't happen for a task branch owned by one run, but a stale prior
///   push could linger) is tolerated identically: it surfaces as a push error,
///   we warn and retry. We deliberately do NOT force-push — clobbering the
///   mirror would risk discarding commits, and the SIGTERM/soft-deadline
///   checkpoint plus the supervisor's own post-stage push remain the
///   authoritative paths.
///
/// Stands down via `tokio::select!` on `cancel.cancelled()` so it exits cleanly
/// when the supervisor finishes or any wind-down path cancels.
fn install_periodic_push(
    cancel: CancellationToken,
    task_run_id: String,
    captured_workspace_path: Arc<std::sync::Mutex<Option<PathBuf>>>,
    task_branch: String,
) {
    tokio::spawn(async move {
        // SHA this loop last pushed successfully. Kept in local state so we
        // never have to interrogate the mirror — a no-op push on the first
        // changed SHA is cheap and the refspec is idempotent anyway.
        let mut last_pushed: Option<String> = None;
        let mut ticker = tokio::time::interval(PERIODIC_PUSH_INTERVAL);
        // The first `tick()` completes immediately; burn it so we wait a full
        // interval before the first push (a freshly-started stage rarely has
        // commits in the first 180s, and the supervisor's own pushes cover
        // boundaries).
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!(
                        task_run_id = %task_run_id,
                        "periodic push: cancelled; standing down"
                    );
                    return;
                }
                _ = ticker.tick() => {}
            }

            let workspace_path = {
                captured_workspace_path
                    .lock()
                    .expect("captured_workspace_path mutex poisoned")
                    .clone()
            };
            let Some(workspace_path) = workspace_path else {
                // No stage has started yet → no ephemeral clone, nothing to
                // push. Silent: this is the expected state during setup.
                continue;
            };

            let Some(current) = resolve_branch_sha(&workspace_path, &task_branch).await else {
                warn!(
                    task_run_id = %task_run_id,
                    branch = %task_branch,
                    path = %workspace_path.display(),
                    "periodic push: could not resolve local branch SHA (branch absent or git busy); skipping this tick"
                );
                continue;
            };

            if !push_needed(&current, last_pushed.as_deref()) {
                tracing::debug!(
                    task_run_id = %task_run_id,
                    branch = %task_branch,
                    sha = %current,
                    "periodic push: branch head unchanged since last push; skipping"
                );
                continue;
            }

            let ws = match Workspace::attach_existing(&workspace_path, task_branch.clone()) {
                Ok(ws) => ws,
                Err(e) => {
                    warn!(
                        task_run_id = %task_run_id,
                        branch = %task_branch,
                        path = %workspace_path.display(),
                        error = %e,
                        "periodic push: failed to attach workspace; skipping this tick"
                    );
                    continue;
                }
            };

            match ws.push_to_origin(&task_branch).await {
                Ok(()) => {
                    last_pushed = Some(current.clone());
                    info!(
                        task_run_id = %task_run_id,
                        branch = %task_branch,
                        sha = %current,
                        "periodic push: pushed task_branch to mirror — committed work is durable against OOM"
                    );
                }
                Err(e) => warn!(
                    task_run_id = %task_run_id,
                    branch = %task_branch,
                    error = %e,
                    "periodic push: push_to_origin failed; will retry next tick"
                ),
            }
        }
    });
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
            if !worker_bridge_should_serialize_event(envelope.entity_type, envelope.action) {
                return;
            }

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
        background_work_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
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
        runtime_ops: None,
        cargo_target_runs_root: None,
        mirror: None,
        rpc_registry: None,
        // The K8s worker only ever serves one project per Pod, so default
        // every multi-project-aware tool call (epic_show, epic_tasks,
        // task_*, …) to spec.project_id. Without this the LLM has to
        // remember to pass `project` to every call or burn tokens
        // retrying past the "project is required when multiple projects
        // are configured" error from helpers::resolve_project_id_for_agent_tools.
        default_project_id: Some(project_id),
        reconciliation_sweep: ReconciliationSweepConfig::default(),
        memory_intent_planner: djinn_agent::context::MemoryIntentPlannerConfig::from_env(),
        compaction_cs: djinn_slot::reply_loop::CompactionCriticalSection::default(),
    }
}

fn worker_bridge_should_serialize_event(entity_type: &str, action: &str) -> bool {
    !worker_bridge_ignores_pair(entity_type, action)
}

fn worker_bridge_ignores_pair(entity_type: &str, action: &str) -> bool {
    matches!(
        (entity_type, action),
        ("session_message", "inserted")
            | ("note", "created")
            | ("note", "updated")
            | ("note", "contradiction_candidates")
    )
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
    let env_config = match lifecycle::load_environment_config(&env_config_path).await {
        Ok(Some(cfg)) => {
            tracing::info!(
                project_id,
                schema_version = cfg.schema_version,
                workspace_count = cfg.workspaces.len(),
                pre_anything_hooks = cfg.lifecycle.pre_anything.len(),
                "environment_config loaded from {}",
                env_config_path.display()
            );
            if let Err(e) =
                lifecycle::run_phase(&lifecycle_root, "pre_anything", &cfg.lifecycle.pre_anything)
                    .await
            {
                warn!(
                    project_id,
                    project_root = %lifecycle_root.display(),
                    error = %format!("{e:#}"),
                    "pre_anything hook failed; continuing with warm-graph anyway"
                );
            }
            Some(cfg)
        }
        Ok(None) => {
            tracing::debug!(
                project_id,
                "no environment_config mounted at {} — continuing without hooks",
                env_config_path.display()
            );
            None
        }
        Err(e) => {
            warn!(
                project_id,
                error = %format!("{e:#}"),
                "environment_config present but failed to load; ignoring"
            );
            None
        }
    };

    // Warm the cargo target base from `main` so task-run pods seed
    // it and recompile only their delta incrementally. This runs in the worker
    // (not the warm-Job shell) so we can normalize tracked-file mtimes to commit
    // times FIRST — the SAME normalization task-run applies before it
    // compiles. Cargo freshness keys on mtimes, so without matching them the
    // base's fingerprints would never match task-run's fresh clone and reuse
    // would never hit (the bug this fixes). `lifecycle_root` is
    // `DJINN_PROJECT_ROOT` = the cloned `main` tree; `resolve_cargo_workspace_dir`
    // lands on `<root>/server` — the exact dir task-run's `cd server`
    // compiles in. Best-effort: never fails the graph warm.
    djinn_workspace::normalize_mtimes_at(&lifecycle_root).await;
    let policy =
        cargo_cache_policy::resolve_cargo_cache_policy(&lifecycle_root, env_config.as_ref())
            .unwrap_or_default();
    warm_cargo_target_base(project_id, &lifecycle_root, &policy).await;

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedGraphArtifactParitySuccess {
    project_id: String,
    old_commit: String,
    new_commit: String,
}

#[derive(Debug)]
enum CachedGraphArtifactParityError {
    MissingCache {
        project_id: String,
        requested_commit: String,
    },
    Repository(anyhow::Error),
    BlobParity(GraphArtifactBlobParityError),
}

impl std::fmt::Display for CachedGraphArtifactParityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCache {
                project_id,
                requested_commit,
            } => write!(
                f,
                "missing cached graph artifact for project {project_id} at commit {requested_commit}"
            ),
            Self::Repository(err) => write!(f, "load cached graph artifact: {err:#}"),
            Self::BlobParity(GraphArtifactBlobParityError::Diff(diff)) => {
                let diff_json = serde_json::to_string_pretty(diff).map_err(|_| std::fmt::Error)?;
                write!(f, "cached graph artifacts are not at parity:\n{diff_json}")
            }
            Self::BlobParity(err) => write!(f, "cached graph artifact comparison failed: {err}"),
        }
    }
}

impl std::error::Error for CachedGraphArtifactParityError {}

#[async_trait]
trait CachedGraphArtifactCache: Sync {
    async fn get_cached_graph_artifact(
        &self,
        project_id: &str,
        commit_sha: &str,
    ) -> djinn_db::Result<Option<djinn_db::repositories::repo_graph_cache::CachedRepoGraph>>;

    async fn latest_cached_graph_artifact(
        &self,
        project_id: &str,
    ) -> djinn_db::Result<Option<djinn_db::repositories::repo_graph_cache::CachedRepoGraph>>;
}

#[async_trait]
impl CachedGraphArtifactCache
    for djinn_db::repositories::repo_graph_cache::RepoGraphCacheRepository
{
    async fn get_cached_graph_artifact(
        &self,
        project_id: &str,
        commit_sha: &str,
    ) -> djinn_db::Result<Option<djinn_db::repositories::repo_graph_cache::CachedRepoGraph>> {
        self.get(project_id, commit_sha).await
    }

    async fn latest_cached_graph_artifact(
        &self,
        project_id: &str,
    ) -> djinn_db::Result<Option<djinn_db::repositories::repo_graph_cache::CachedRepoGraph>> {
        self.latest_for_project(project_id).await
    }
}

// CLI binary subcommand: println is the correct output channel for reporting
// comparison results to the operator.
#[allow(clippy::print_stdout)]
async fn run_compare_graph_artifacts(
    project_id: &str,
    old_commit: &str,
    new_commit: &str,
) -> Result<()> {
    let db = bootstrap_warm_database().await?;
    let repo = djinn_db::repositories::repo_graph_cache::RepoGraphCacheRepository::new(db);
    let success = compare_cached_graph_artifacts(&repo, project_id, old_commit, new_commit)
        .await
        .map_err(anyhow::Error::from)?;

    println!(
        "cached graph artifacts match for project {} (old_commit={}, new_commit={})",
        success.project_id, success.old_commit, success.new_commit
    );
    Ok(())
}

async fn compare_cached_graph_artifacts(
    repo: &impl CachedGraphArtifactCache,
    project_id: &str,
    old_commit: &str,
    new_commit: &str,
) -> std::result::Result<CachedGraphArtifactParitySuccess, CachedGraphArtifactParityError> {
    let old = load_cached_graph_artifact(repo, project_id, old_commit).await?;
    let new = load_cached_graph_artifact(repo, project_id, new_commit).await?;

    assert_graph_artifact_blob_parity(&old.graph_blob, &new.graph_blob)
        .map_err(CachedGraphArtifactParityError::BlobParity)?;

    Ok(CachedGraphArtifactParitySuccess {
        project_id: project_id.to_string(),
        old_commit: old.commit_sha,
        new_commit: new.commit_sha,
    })
}

async fn load_cached_graph_artifact(
    repo: &impl CachedGraphArtifactCache,
    project_id: &str,
    requested_commit: &str,
) -> std::result::Result<
    djinn_db::repositories::repo_graph_cache::CachedRepoGraph,
    CachedGraphArtifactParityError,
> {
    let cached = if requested_commit == "latest" {
        repo.latest_cached_graph_artifact(project_id).await
    } else {
        repo.get_cached_graph_artifact(project_id, requested_commit)
            .await
    }
    .with_context(|| {
        format!("load cached graph artifact for project {project_id} at commit {requested_commit}")
    })
    .map_err(CachedGraphArtifactParityError::Repository)?;

    cached.ok_or_else(|| CachedGraphArtifactParityError::MissingCache {
        project_id: project_id.to_string(),
        requested_commit: requested_commit.to_string(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::repositories::repo_graph_cache::CachedRepoGraph;
    use djinn_graph::communities::Community;
    use djinn_graph::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphEdgeKind,
        RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    };
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::process::Command;
    use tracing::dispatcher::Dispatch;

    static CARGO_INSTRUMENT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn worker_bridge_exact_ignored_pairs_skip_serialization() {
        for (entity_type, action) in [
            ("session_message", "inserted"),
            ("note", "created"),
            ("note", "updated"),
            ("note", "contradiction_candidates"),
        ] {
            assert!(
                worker_bridge_ignores_pair(entity_type, action),
                "expected {entity_type}.{action} to be ignored"
            );
            assert!(
                !worker_bridge_should_serialize_event(entity_type, action),
                "expected {entity_type}.{action} to be skipped before bridge serialization"
            );
        }
    }

    #[test]
    fn worker_bridge_nearby_unlisted_pairs_continue_to_host() {
        for (entity_type, action) in [
            ("note", "deleted"),
            ("note", "missing_summary"),
            ("proposal", "updated"),
            ("proposal_feedback", "created"),
            ("proposal_debate_trail", "created"),
            ("session", "message"),
        ] {
            assert!(
                !worker_bridge_ignores_pair(entity_type, action),
                "expected {entity_type}.{action} not to be ignored"
            );
            assert!(
                worker_bridge_should_serialize_event(entity_type, action),
                "expected {entity_type}.{action} to continue toward bridge serialization"
            );
        }
    }

    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn take(&self) -> String {
            let mut buf = self.0.lock().expect("captured logs mutex poisoned");
            let out =
                String::from_utf8(buf.clone()).expect("captured log bytes were not valid utf-8");
            buf.clear();
            out
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogsWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogsWriter {
                inner: std::sync::Arc::clone(&self.0),
            }
        }
    }

    struct CapturedLogsWriter {
        inner: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl Write for CapturedLogsWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner
                .lock()
                .expect("captured logs mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn resolve_cargo_workspace_prefers_env_config_rust_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("server")).expect("mkdir server");
        std::fs::write(root.join("server/Cargo.toml"), "[workspace]\n").expect("write Cargo.toml");

        let cfg = djinn_stack::environment::EnvironmentConfig {
            workspaces: vec![djinn_stack::environment::Workspace {
                slug: None,
                name: None,
                tags: vec![],
                root: "server".into(),
                language: "rust".into(),
                toolchain: None,
                version: None,
                package_manager: None,
            }],
            ..Default::default()
        };

        let resolved = resolve_cargo_workspace_dir(root, Some(&cfg)).expect("resolve");
        assert_eq!(resolved, root.join("server"));
    }

    #[test]
    fn resolve_cargo_workspace_finds_subdir_without_env_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("server")).expect("mkdir server");
        std::fs::write(root.join("server/Cargo.toml"), "[workspace]\n").expect("write Cargo.toml");

        let resolved = resolve_cargo_workspace_dir(root, None).expect("resolve");
        assert_eq!(resolved, root.join("server"));
    }

    #[test]
    fn resolve_cargo_workspace_prefers_root_cargo_toml() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").expect("write Cargo.toml");

        let resolved = resolve_cargo_workspace_dir(root, None).expect("resolve");
        assert_eq!(resolved, root.to_path_buf());
    }

    #[test]
    fn resolve_cargo_workspace_none_for_non_rust_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        std::fs::write(root.join("package.json"), "{}").expect("write package.json");

        assert!(resolve_cargo_workspace_dir(root, None).is_none());
    }

    #[derive(Default)]
    struct FakeGraphArtifactCache {
        rows: BTreeMap<(String, String), CachedRepoGraph>,
    }

    impl FakeGraphArtifactCache {
        fn with_row(mut self, project_id: &str, commit_sha: &str, graph_blob: Vec<u8>) -> Self {
            self.rows.insert(
                (project_id.to_string(), commit_sha.to_string()),
                CachedRepoGraph {
                    project_id: project_id.to_string(),
                    commit_sha: commit_sha.to_string(),
                    graph_blob,
                    built_at: commit_sha.to_string(),
                },
            );
            self
        }
    }

    #[async_trait]
    impl CachedGraphArtifactCache for FakeGraphArtifactCache {
        async fn get_cached_graph_artifact(
            &self,
            project_id: &str,
            commit_sha: &str,
        ) -> djinn_db::Result<Option<CachedRepoGraph>> {
            Ok(self
                .rows
                .get(&(project_id.to_string(), commit_sha.to_string()))
                .cloned())
        }

        async fn latest_cached_graph_artifact(
            &self,
            project_id: &str,
        ) -> djinn_db::Result<Option<CachedRepoGraph>> {
            Ok(self
                .rows
                .values()
                .filter(|row| row.project_id == project_id)
                .max_by(|a, b| a.built_at.cmp(&b.built_at))
                .cloned())
        }
    }

    fn graph_artifact_blob(extra_file: Option<&str>) -> Vec<u8> {
        let mut nodes = vec![
            RepoGraphNode {
                id: RepoNodeKey::File("src/lib.rs".into()),
                kind: RepoGraphNodeKind::File,
                display_name: "src/lib.rs".to_string(),
                language: Some("rust".to_string()),
                file_path: Some("src/lib.rs".into()),
                symbol: None,
                symbol_kind: None,
                is_external: false,
                visibility: None,
                signature: None,
                documentation: Vec::new(),
                signature_parts: None,
                is_test: false,
                complexity: None,
                workspace: Some("root".to_string()),
                route_framework: None,
                route_handler_symbol: None,
            },
            RepoGraphNode {
                id: RepoNodeKey::Symbol("pkg src/lib.rs `alpha`().".to_string()),
                kind: RepoGraphNodeKind::Symbol,
                display_name: "alpha".to_string(),
                language: Some("rust".to_string()),
                file_path: Some("src/lib.rs".into()),
                symbol: Some("pkg src/lib.rs `alpha`().".to_string()),
                symbol_kind: None,
                is_external: false,
                visibility: None,
                signature: None,
                documentation: Vec::new(),
                signature_parts: None,
                is_test: false,
                complexity: None,
                workspace: Some("root".to_string()),
                route_framework: None,
                route_handler_symbol: None,
            },
        ];
        if let Some(path) = extra_file {
            nodes.push(RepoGraphNode {
                id: RepoNodeKey::File(path.into()),
                kind: RepoGraphNodeKind::File,
                display_name: path.to_string(),
                language: Some("rust".to_string()),
                file_path: Some(path.into()),
                symbol: None,
                symbol_kind: None,
                is_external: false,
                visibility: None,
                signature: None,
                documentation: Vec::new(),
                signature_parts: None,
                is_test: false,
                complexity: None,
                workspace: Some("root".to_string()),
                route_framework: None,
                route_handler_symbol: None,
            });
        }

        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges: vec![RepoGraphArtifactEdge {
                source: 0,
                target: 1,
                kind: RepoGraphEdgeKind::ContainsDefinition,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.95,
                reason: None,
                step: None,
            }],
            symbol_ranges: BTreeMap::new(),
            communities: vec![Community {
                id: "community-alpha".to_string(),
                label: "community-alpha".to_string(),
                symbol_count: 2,
                member_ids: vec![0, 1],
                cohesion: 1.0,
                keywords: Vec::new(),
            }],
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
        };
        bincode::serialize(&artifact).expect("serialize graph artifact")
    }

    #[tokio::test]
    async fn compare_cached_graph_artifacts_reports_success_without_dumping_graphs() {
        let blob = graph_artifact_blob(None);
        let repo = FakeGraphArtifactCache::default()
            .with_row("project-1", "old-sha", blob.clone())
            .with_row("project-1", "new-sha", blob);

        let success = compare_cached_graph_artifacts(&repo, "project-1", "old-sha", "new-sha")
            .await
            .expect("matching cached artifacts should compare cleanly");

        assert_eq!(success.project_id, "project-1");
        assert_eq!(success.old_commit, "old-sha");
        assert_eq!(success.new_commit, "new-sha");
    }

    #[tokio::test]
    async fn compare_cached_graph_artifacts_resolves_latest_via_repository() {
        let blob = graph_artifact_blob(None);
        let repo = FakeGraphArtifactCache::default().with_row("project-1", "latest-sha", blob);

        let success = compare_cached_graph_artifacts(&repo, "project-1", "latest", "latest")
            .await
            .expect("latest cached artifact should compare to itself");

        assert_eq!(success.old_commit, "latest-sha");
        assert_eq!(success.new_commit, "latest-sha");
    }

    #[tokio::test]
    async fn compare_cached_graph_artifacts_names_missing_project_and_commit() {
        let repo = FakeGraphArtifactCache::default();

        let err = compare_cached_graph_artifacts(&repo, "project-missing", "abc123", "latest")
            .await
            .expect_err("missing cache row should error");

        let message = err.to_string();
        assert!(message.contains("project-missing"));
        assert!(message.contains("abc123"));
    }

    #[tokio::test]
    async fn compare_cached_graph_artifacts_surfaces_structured_diff_as_json() {
        let old_blob = graph_artifact_blob(None);
        let new_blob = graph_artifact_blob(Some("src/extra.rs"));
        let repo = FakeGraphArtifactCache::default()
            .with_row("project-1", "old-sha", old_blob)
            .with_row("project-1", "new-sha", new_blob);

        let err = compare_cached_graph_artifacts(&repo, "project-1", "old-sha", "new-sha")
            .await
            .expect_err("different cached artifacts should report diff");

        let message = err.to_string();
        assert!(message.contains("cached graph artifacts are not at parity"));
        assert!(message.contains("\"added_count\": 1"));
        assert!(message.contains("src/extra.rs"));
    }

    #[test]
    fn soft_deadline_subtracts_margin_for_large_deadlines() {
        // 3h deadline → fires 10 min early (2h50m).
        let fire = soft_deadline_interval(10_800);
        assert_eq!(fire, Duration::from_secs(10_800 - 600));
        assert_eq!(fire, Duration::from_secs(10_200));
    }

    #[test]
    fn soft_deadline_clamps_small_deadlines_to_min() {
        // A deadline at/below the margin would underflow to 0 and fire at
        // startup; the clamp keeps it at the floor instead.
        assert_eq!(soft_deadline_interval(0), SOFT_DEADLINE_MIN);
        assert_eq!(soft_deadline_interval(600), SOFT_DEADLINE_MIN);
        // Just above margin but still under margin+min → clamped.
        assert_eq!(soft_deadline_interval(650), SOFT_DEADLINE_MIN);
    }

    #[test]
    fn soft_deadline_uses_computed_value_once_above_floor() {
        // margin + min = 660s; anything above yields deadline - margin.
        let fire = soft_deadline_interval(700);
        assert_eq!(fire, Duration::from_secs(100));
        assert!(fire >= SOFT_DEADLINE_MIN);
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The wind-down checkpoint commits an uncommitted edit and pushes it to
    /// the mirror — the durability seam that lets work survive a Pod kill.
    ///
    /// This helper is intentionally test-local: it creates temporary git
    /// repositories so the checkpoint durability tests can exercise the real
    /// `checkpoint_workspace` path without relying on a production mirror.
    /// It is not used by production worker or checkpoint logic.
    #[tokio::test]
    async fn checkpoint_commits_and_pushes_uncommitted_work() {
        let origin = tempfile::TempDir::new().expect("origin");
        let op = origin.path();
        git(op, &["init", "--bare", "-b", "main"]);

        // Seed main + a `task` branch via a working clone we'll treat as the Pod
        // workspace.
        let clone = tempfile::TempDir::new().expect("clone");
        let cp = clone.path();
        git(cp, &["clone", op.to_str().unwrap(), "."]);
        std::fs::write(cp.join("base.txt"), "base\n").unwrap();
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "base"]);
        git(cp, &["push", "origin", "main"]);
        git(cp, &["checkout", "-b", "task"]);

        // Worker leaves an UNCOMMITTED edit (the lossy case the checkpoint saves).
        std::fs::write(cp.join("work.txt"), "in-flight\n").unwrap();

        let identity = CheckpointIdentity {
            name: "djinn-bot".into(),
            email: "bot@djinn.local".into(),
        };
        let captured = std::sync::Mutex::new(Some(cp.to_path_buf()));
        checkpoint_workspace(
            &captured,
            "task",
            &identity,
            "run-xyz",
            checkpoint::CheckpointReason::Terminal,
        )
        .await;

        // The edit is committed locally...
        let status = git(cp, &["status", "--porcelain"]);
        assert!(
            status.trim().is_empty(),
            "checkpoint must commit the dirty tree: {status:?}"
        );
        // ...and pushed to the mirror under `task`.
        let remote = git(op, &["rev-parse", "task"]);
        let local = git(cp, &["rev-parse", "task"]);
        assert_eq!(
            remote.trim(),
            local.trim(),
            "checkpoint must push task to the mirror"
        );
    }

    /// Checkpoint on an already-clean tree is a no-op commit but still pushes
    /// committed-but-unpushed work (the common case: workers commit via shell).
    #[tokio::test]
    async fn checkpoint_pushes_committed_but_unpushed_work() {
        let origin = tempfile::TempDir::new().expect("origin");
        let op = origin.path();
        git(op, &["init", "--bare", "-b", "main"]);

        let clone = tempfile::TempDir::new().expect("clone");
        let cp = clone.path();
        git(cp, &["clone", op.to_str().unwrap(), "."]);
        std::fs::write(cp.join("base.txt"), "base\n").unwrap();
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "base"]);
        git(cp, &["push", "origin", "main"]);
        git(cp, &["checkout", "-b", "task"]);
        // Worker COMMITTED its own work via shell (clean tree), but never pushed.
        std::fs::write(cp.join("work.txt"), "done\n").unwrap();
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "worker self-commit"]);
        let local = git(cp, &["rev-parse", "task"]);

        let identity = CheckpointIdentity {
            name: "djinn-bot".into(),
            email: "bot@djinn.local".into(),
        };
        let captured = std::sync::Mutex::new(Some(cp.to_path_buf()));
        checkpoint_workspace(
            &captured,
            "task",
            &identity,
            "run-xyz",
            checkpoint::CheckpointReason::Terminal,
        )
        .await;

        let remote = git(op, &["rev-parse", "task"]);
        assert_eq!(
            remote.trim(),
            local.trim(),
            "checkpoint must push the worker's own commit even on a clean tree"
        );
    }

    #[test]
    fn push_needed_pushes_first_observed_sha() {
        // No prior push this run → we can't assume the mirror is current, so
        // push (the refspec is idempotent if it already matches).
        assert!(push_needed("abc123", None));
    }

    #[test]
    fn push_needed_skips_unchanged_head() {
        // Same SHA as the last successful push → nothing new, skip.
        assert!(!push_needed("abc123", Some("abc123")));
    }

    #[test]
    fn push_needed_pushes_when_head_moved() {
        // Worker committed since the last push → head moved, push.
        assert!(push_needed("def456", Some("abc123")));
    }

    /// `resolve_branch_sha` returns the branch's commit SHA and matches
    /// `git rev-parse`; an absent branch resolves to `None` (skip-this-tick).
    #[tokio::test]
    async fn resolve_branch_sha_reads_local_head_and_handles_absent_branch() {
        let clone = tempfile::TempDir::new().expect("clone");
        let cp = clone.path();
        git(cp, &["init", "-b", "main"]);
        std::fs::write(cp.join("base.txt"), "base\n").unwrap();
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "base"]);
        git(cp, &["checkout", "-b", "task"]);

        let resolved = resolve_branch_sha(cp, "task").await.expect("resolve task");
        let expected = git(cp, &["rev-parse", "task"]);
        assert_eq!(resolved, expected.trim());

        // A branch that doesn't exist → None, not an error.
        assert!(resolve_branch_sha(cp, "nope").await.is_none());
    }

    /// The periodic push loop pushes the worker's already-committed work to the
    /// mirror without committing anything itself — the OOM-proof durability
    /// seam. Drives one tick by stubbing the tick logic the loop runs.
    #[tokio::test]
    async fn periodic_push_pushes_committed_work_without_committing() {
        let origin = tempfile::TempDir::new().expect("origin");
        let op = origin.path();
        git(op, &["init", "--bare", "-b", "main"]);

        let clone = tempfile::TempDir::new().expect("clone");
        let cp = clone.path();
        git(cp, &["clone", op.to_str().unwrap(), "."]);
        std::fs::write(cp.join("base.txt"), "base\n").unwrap();
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "base"]);
        git(cp, &["push", "origin", "main"]);
        git(cp, &["checkout", "-b", "task"]);
        // Worker committed work via shell but never pushed it.
        std::fs::write(cp.join("work.txt"), "done\n").unwrap();
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "worker self-commit"]);
        // Leave an UNCOMMITTED edit too: the push must NOT commit it (push-only).
        std::fs::write(cp.join("dirty.txt"), "uncommitted\n").unwrap();

        let local = git(cp, &["rev-parse", "task"]);

        // One tick of the loop's logic: resolve, decide, attach, push.
        let current = resolve_branch_sha(cp, "task").await.expect("resolve");
        assert!(push_needed(&current, None));
        let ws = Workspace::attach_existing(cp, "task".to_string()).expect("attach");
        ws.push_to_origin("task").await.expect("push");

        // The committed work reached the mirror...
        let remote = git(op, &["rev-parse", "task"]);
        assert_eq!(
            remote.trim(),
            local.trim(),
            "periodic push must push the worker's committed task head to the mirror"
        );
        // ...and the uncommitted edit is STILL uncommitted (push-only, no commit).
        let status = git(cp, &["status", "--porcelain"]);
        assert!(
            status.contains("dirty.txt"),
            "periodic push must not commit uncommitted work: {status:?}"
        );
    }

    /// When no stage has started the captured path is `None` — there is no live
    /// ephemeral clone and no in-flight work to save, so the checkpoint is a
    /// clean no-op (no attach, no commit, no push, no panic).
    #[tokio::test]
    async fn checkpoint_with_no_captured_path_is_noop() {
        let identity = CheckpointIdentity {
            name: "djinn-bot".into(),
            email: "bot@djinn.local".into(),
        };
        // Empty slot: execute_stage never ran, so nothing was captured.
        let captured: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
        // Must return cleanly without touching the filesystem or panicking.
        checkpoint_workspace(
            &captured,
            "task",
            &identity,
            "run-xyz",
            checkpoint::CheckpointReason::Terminal,
        )
        .await;
        // The slot is unchanged — the no-op path doesn't mutate it.
        assert!(
            captured.lock().unwrap().is_none(),
            "no-op checkpoint must leave the empty captured-path slot untouched"
        );
    }

    // ── parse_cargo_fresh_compiling unit tests ──────────────────────

    #[test]
    fn parse_cargo_fresh_compiling_zero_fresh_zero_compiling() {
        let (fresh, compiling) = parse_cargo_fresh_compiling("");
        assert_eq!(fresh, 0);
        assert_eq!(compiling, 0);
    }

    #[test]
    fn parse_cargo_fresh_compiling_only_non_cargo_output() {
        let stdout = "warning: unused variable `x`\n   --> src/lib.rs:10:9\n";
        let (fresh, compiling) = parse_cargo_fresh_compiling(stdout);
        assert_eq!(fresh, 0);
        assert_eq!(compiling, 0);
    }

    #[test]
    fn parse_cargo_fresh_compiling_mixed_lines() {
        let stdout = "\
   Compiling serde v1.0.0
   Compiling serde_derive v1.0.0
   Fresh libc v0.2.0
   Fresh memchr v2.0.0
   Fresh proc-macro2 v1.0.0
warning: something
   Compiling syn v2.0.0
";
        let (fresh, compiling) = parse_cargo_fresh_compiling(stdout);
        assert_eq!(fresh, 3);
        assert_eq!(compiling, 3);
    }

    #[test]
    fn parse_cargo_fresh_compiling_all_fresh() {
        let stdout = "   Fresh crate-a v0.1.0\n   Fresh crate-b v0.2.0\n";
        let (fresh, compiling) = parse_cargo_fresh_compiling(stdout);
        assert_eq!(fresh, 2);
        assert_eq!(compiling, 0);
    }

    #[test]
    fn parse_cargo_fresh_compiling_all_compiling() {
        let stdout = "   Compiling crate-a v0.1.0\n   Compiling crate-b v0.2.0\n";
        let (fresh, compiling) = parse_cargo_fresh_compiling(stdout);
        assert_eq!(fresh, 0);
        assert_eq!(compiling, 2);
    }

    #[test]
    fn parse_cargo_fresh_compiling_edge_case_partial_match() {
        // "Freshly" and "Compilation" should NOT match.
        let stdout = "Freshly ground coffee\nCompilation finished\n   Fresh foo v0.1.0\n";
        let (fresh, compiling) = parse_cargo_fresh_compiling(stdout);
        assert_eq!(fresh, 1);
        assert_eq!(compiling, 0);
    }

    #[test]
    fn parse_cargo_fresh_compiling_delegates_from_byte_version() {
        let raw = b"   Fresh foo v0.1.0\n   Compiling bar v0.2.0\n";
        let (f, c) = cargo_fresh_compiling_counts(raw);
        assert_eq!(f, 1);
        assert_eq!(c, 1);
    }

    // ── DJINN_CARGO_INSTRUMENT toggle tests ─────────────────────────

    #[test]
    fn cargo_instrument_toggle_absent_by_default() {
        let _guard = CARGO_INSTRUMENT_ENV_LOCK.lock().expect("env lock poisoned");
        // In a clean test environment the var should be absent.
        // Remove it in case a previous test set it.
        // SAFETY: test-only env mutation; these tests must not run in parallel.
        unsafe { std::env::remove_var("DJINN_CARGO_INSTRUMENT") };
        assert!(!cargo_instrument_enabled());
    }

    #[test]
    fn cargo_instrument_toggle_enabled_when_set() {
        let _guard = CARGO_INSTRUMENT_ENV_LOCK.lock().expect("env lock poisoned");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("DJINN_CARGO_INSTRUMENT", "1") };
        assert!(cargo_instrument_enabled());
        unsafe { std::env::remove_var("DJINN_CARGO_INSTRUMENT") };
    }

    #[test]
    fn cargo_instrument_toggle_enabled_for_any_value() {
        let _guard = CARGO_INSTRUMENT_ENV_LOCK.lock().expect("env lock poisoned");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("DJINN_CARGO_INSTRUMENT", "") };
        assert!(cargo_instrument_enabled());
        unsafe { std::env::remove_var("DJINN_CARGO_INSTRUMENT") };
    }

    #[test]
    fn cargo_warm_execution_plan_absent_toggle_is_status_only_and_does_not_add_verbose() {
        let plan = cargo_warm_execution_plan(&["check", "--workspace"], false);

        assert_eq!(plan.output_mode, CargoWarmOutputMode::InheritStatusOnly);
        assert_eq!(plan.args, vec!["check", "--workspace"]);
        assert!(
            !plan.args.iter().any(|arg| arg == "-v"),
            "cheap-off path must not add verbose instrumentation args"
        );
    }

    #[test]
    fn cargo_warm_execution_plan_enabled_toggle_captures_output_and_adds_verbose() {
        let plan = cargo_warm_execution_plan(&["test", "--no-run"], true);

        assert_eq!(
            plan.output_mode,
            CargoWarmOutputMode::CaptureForInstrumentation
        );
        assert_eq!(plan.args, vec!["test", "--no-run", "-v"]);
    }

    #[test]
    fn run_cargo_warm_step_instrumented_parses_mock_output_and_logs_counts() {
        let _guard = CARGO_INSTRUMENT_ENV_LOCK.lock().expect("env lock poisoned");
        // SAFETY: guarded test-only env mutation.
        unsafe { std::env::set_var("DJINN_CARGO_INSTRUMENT", "1") };

        let tmp = tempfile::tempdir().expect("tempdir");
        let cargo_bin = tmp.path().join("cargo-mock.sh");
        std::fs::write(
            &cargo_bin,
            "#!/bin/sh\nprintf '   Fresh mock-a v0.1.0\\n   Compiling mock-b v0.1.0\\n'\nprintf '   Fresh mock-c v0.1.0\\n' >&2\nexit 0\n",
        )
        .expect("write mock cargo");
        let mut perms = std::fs::metadata(&cargo_bin)
            .expect("metadata")
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
            std::fs::set_permissions(&cargo_bin, perms).expect("chmod mock cargo");
        }

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(false)
            .with_ansi(false)
            .finish();
        let dispatch = Dispatch::new(subscriber);

        let ok = tracing::dispatcher::with_default(&dispatch, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .expect("runtime")
                .block_on(run_cargo_warm_step_with_cargo(
                    &cargo_bin,
                    "project-cargo-log",
                    tmp.path(),
                    &["check"],
                    "check",
                ))
        });

        // SAFETY: guarded test-only env mutation.
        unsafe { std::env::remove_var("DJINN_CARGO_INSTRUMENT") };

        assert!(ok, "mock cargo should succeed");
        let logs = logs.take();
        assert!(
            logs.contains("cargo warm: instrumented Fresh/Compiling counts"),
            "instrumented cargo path should emit the structured count event: {logs}"
        );
        assert!(
            logs.contains("fresh_count=2"),
            "missing fresh count: {logs}"
        );
        assert!(
            logs.contains("compiling_count=1"),
            "missing compiling count: {logs}"
        );
        assert!(
            logs.contains("step=\"check\""),
            "missing step label: {logs}"
        );
    }

    /// Write an executable stub `cargo` that ignores its args and exits `code`.
    #[cfg(unix)]
    fn write_stub_cargo(dir: &Path, code: i32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("cargo-stub.sh");
        std::fs::write(&bin, format!("#!/bin/sh\nexit {code}\n")).expect("write stub cargo");
        let mut perms = std::fs::metadata(&bin).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).expect("chmod stub cargo");
        bin
    }

    #[cfg(unix)]
    fn block_on_sweep(cargo_bin: &Path, workspace: &Path, args: &[&str]) -> bool {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(run_cargo_sweep_step_with_cargo(
                cargo_bin,
                "project-sweep",
                workspace,
                args,
                "sweep-file",
            ))
    }

    #[cfg(unix)]
    #[test]
    fn cargo_sweep_step_returns_true_when_cargo_exits_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cargo_bin = write_stub_cargo(tmp.path(), 0);
        assert!(
            block_on_sweep(&cargo_bin, tmp.path(), &["--file"]),
            "a zero-exit cargo sweep must report success"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cargo_sweep_step_returns_false_when_cargo_fails() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Exit 101 mimics `cargo`'s "no such subcommand: sweep" on an image that
        // predates cargo-sweep — the warm must treat it as a non-fatal no-op.
        let cargo_bin = write_stub_cargo(tmp.path(), 101);
        assert!(
            !block_on_sweep(&cargo_bin, tmp.path(), &["--file"]),
            "a non-zero cargo sweep must report failure, not panic"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cargo_sweep_step_returns_false_when_cargo_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("definitely-not-a-real-cargo");
        assert!(
            !block_on_sweep(&missing, tmp.path(), &["--stamp"]),
            "a spawn error (cargo-sweep absent) must degrade to false, not panic"
        );
    }

    #[test]
    fn run_cargo_warm_step_absent_toggle_uses_status_path_without_instrumentation_log() {
        let _guard = CARGO_INSTRUMENT_ENV_LOCK.lock().expect("env lock poisoned");
        // SAFETY: guarded test-only env mutation.
        unsafe { std::env::remove_var("DJINN_CARGO_INSTRUMENT") };

        let tmp = tempfile::tempdir().expect("tempdir");
        let cargo_bin = tmp.path().join("cargo-mock.sh");
        std::fs::write(
            &cargo_bin,
            "#!/bin/sh\nprintf '   Fresh should-not-be-parsed v0.1.0\\n'\nexit 0\n",
        )
        .expect("write mock cargo");
        let mut perms = std::fs::metadata(&cargo_bin)
            .expect("metadata")
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
            std::fs::set_permissions(&cargo_bin, perms).expect("chmod mock cargo");
        }

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(false)
            .with_ansi(false)
            .finish();
        let dispatch = Dispatch::new(subscriber);

        let ok = tracing::dispatcher::with_default(&dispatch, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .expect("runtime")
                .block_on(run_cargo_warm_step_with_cargo(
                    &cargo_bin,
                    "project-cargo-cheap-off",
                    tmp.path(),
                    &["check"],
                    "check",
                ))
        });

        assert!(ok, "mock cargo should succeed");
        let logs = logs.take();
        assert!(
            !logs.contains("instrumented Fresh/Compiling counts"),
            "cheap-off status path must not capture/parse stdout or log instrumentation counts: {logs}"
        );
        assert!(
            logs.contains("cargo warm: step succeeded"),
            "status path should still log success: {logs}"
        );
    }

    // ---- c9l4: classify_environmental_failure tests --------------------

    #[test]
    fn classify_environmental_failure_pre_task_failed() {
        let err = anyhow::anyhow!(
            "pre-task blocking command 'setup' failed (exit=Some(1), timed_out=false, cancelled=false)"
        );
        assert_eq!(classify_environmental_failure(&err), "pre_task_failed");
    }

    #[test]
    fn classify_environmental_failure_pre_task_timed_out() {
        let err = anyhow::anyhow!(
            "pre-task blocking command 'build' failed (exit=None, timed_out=true, cancelled=false)"
        );
        assert_eq!(classify_environmental_failure(&err), "pre_task_timed_out");
    }

    #[test]
    fn classify_environmental_failure_pre_task_cancelled() {
        let err = anyhow::anyhow!(
            "pre-task blocking command 'check' failed (exit=None, timed_out=false, cancelled=true)"
        );
        assert_eq!(classify_environmental_failure(&err), "pre_task_cancelled");
    }

    #[test]
    fn classify_environmental_failure_service_readiness() {
        let err =
            anyhow::anyhow!("service readiness check failed: postgres not accepting connections");
        assert_eq!(
            classify_environmental_failure(&err),
            "service_readiness_failed"
        );
    }

    #[test]
    fn classify_environmental_failure_sidecar_error() {
        let err = anyhow::anyhow!("sidecar probe failed on port 5432");
        assert_eq!(
            classify_environmental_failure(&err),
            "service_readiness_failed"
        );
    }

    // ── Workspace seed telemetry tests (proposal zp5t) ──────────────────

    /// Serialize telemetry-scrape tests so each can use a precise delta.
    static SEED_TELEMETRY_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) fn seed_telemetry_guard() -> std::sync::MutexGuard<'static, ()> {
        SEED_TELEMETRY_MUTEX
            .lock()
            .expect("seed telemetry test mutex poisoned")
    }

    /// Count `workspace_seed_seconds_count` samples for a given outcome.
    fn seed_count(rendered: &str, outcome: &str) -> f64 {
        rendered
            .lines()
            .find(|line| {
                line.starts_with("djinn_workspace_seed_seconds_count")
                    && line.contains(&format!("outcome=\"{outcome}\""))
            })
            .and_then(|line| line.rsplit_once(' ').and_then(|(_, v)| v.parse().ok()))
            .unwrap_or(0.0)
    }

    /// Read the `_sum` value for `workspace_seed_seconds` for a given outcome.
    fn seed_sum(rendered: &str, outcome: &str) -> f64 {
        rendered
            .lines()
            .find(|line| {
                line.starts_with("djinn_workspace_seed_seconds_sum")
                    && line.contains(&format!("outcome=\"{outcome}\""))
            })
            .and_then(|line| line.rsplit_once(' ').and_then(|(_, v)| v.parse().ok()))
            .unwrap_or(0.0)
    }

    /// Assert that rendered workspace_seed samples carry no high-cardinality
    /// identity labels.
    fn assert_no_seed_identity_labels(rendered: &str) {
        for line in rendered.lines() {
            if !line.starts_with("djinn_workspace_seed_seconds") {
                continue;
            }
            for forbidden in [
                "task_id=",
                "session_id=",
                "project_id=",
                "user_id=",
                "path=",
                "error=",
                "reason=",
            ] {
                assert!(
                    !line.contains(forbidden),
                    "seed metric line must not carry high-cardinality label {forbidden}: {line}",
                );
            }
        }
    }

    /// `classify_seed_outcome`: Ok(Ok(_)) → ok.
    #[test]
    fn classify_seed_outcome_ok() {
        let result: Result<std::io::Result<CargoTargetSeedResult>, tokio::task::JoinError> =
            Ok(Ok(CargoTargetSeedResult {
                elapsed: Duration::from_millis(1),
                linked_file_count: 0,
                copied_file_count: 0,
                skipped_file_count: 0,
                linked_bytes: 0,
                copied_bytes: 0,
                fallback_reason: None,
            }));
        assert_eq!(
            classify_seed_outcome(&result),
            djinn_telemetry::workspace_seed::OUTCOME_OK,
        );
    }

    /// `classify_seed_outcome`: Ok(Err(_)) → error.
    #[test]
    fn classify_seed_outcome_error() {
        let result: Result<std::io::Result<CargoTargetSeedResult>, tokio::task::JoinError> =
            Ok(Err(std::io::Error::other("setup failed")));
        assert_eq!(
            classify_seed_outcome(&result),
            djinn_telemetry::workspace_seed::OUTCOME_ERROR,
        );
    }

    /// A deterministic terminal cancellation is classified as `cancelled`.
    /// This injects cancellation at the terminal recording seam rather than
    /// aborting `spawn_blocking`, because started blocking tasks cannot be
    /// reliably aborted by Tokio.
    #[test]
    fn classify_seed_terminal_cancelled() {
        assert_eq!(
            classify_seed_terminal(SeedAttemptTerminal::Cancelled),
            djinn_telemetry::workspace_seed::OUTCOME_CANCELLED,
        );
    }

    /// A successful seed records exactly one `ok` `workspace_seed_seconds`
    /// sample and does not change existing cold-start fallback behavior.
    #[test]
    fn seed_records_one_ok_sample_on_success() {
        let _guard = seed_telemetry_guard();
        djinn_telemetry::init().expect("telemetry init");

        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("warm-base");
        let run = tmp.path().join("run-target");
        // Create a warm-base file so the seed has something to hardlink.
        let artifact = base.join("debug/deps/libsuccess.rlib");
        std::fs::create_dir_all(artifact.parent().expect("parent")).expect("create parent");
        std::fs::write(&artifact, b"seeded artifact").expect("write artifact");

        let before = djinn_telemetry::render().expect("render before");
        let ok_before = seed_count(&before, "ok");
        let ok_sum_before = seed_sum(&before, "ok");
        let elapsed = Duration::from_millis(300);

        let join_result =
            completed_seed_join(cargo_target_seed::seed_cargo_target_dir(&base, &run));
        record_seed_terminal_seconds(elapsed, SeedAttemptTerminal::Join(&join_result));

        let result = join_result
            .expect("join must succeed")
            .expect("seed must succeed");
        assert!(result.fallback_reason.is_none(), "seed should succeed");

        let after = djinn_telemetry::render().expect("render after");
        assert_eq!(
            seed_count(&after, "ok"),
            ok_before + 1.0,
            "one ok seed sample expected"
        );
        assert!(
            (seed_sum(&after, "ok") - ok_sum_before - elapsed.as_secs_f64()).abs() < 0.001,
            "ok seed sum delta must equal elapsed"
        );
        assert_no_seed_identity_labels(&after);
    }

    /// A cold-start fallback seed (missing base) records exactly one `ok`
    /// sample — the seed helper returns `Ok` with a fallback reason, so the
    /// seed attempt itself completed successfully.
    #[test]
    fn seed_records_ok_on_cold_start_fallback() {
        let _guard = seed_telemetry_guard();
        djinn_telemetry::init().expect("telemetry init");

        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("missing-base");
        let run = tmp.path().join("run-target");

        let before = djinn_telemetry::render().expect("render before");
        let ok_before = seed_count(&before, "ok");
        let ok_sum_before = seed_sum(&before, "ok");
        let elapsed = Duration::from_millis(180);

        let join_result =
            completed_seed_join(cargo_target_seed::seed_cargo_target_dir(&base, &run));
        record_seed_terminal_seconds(elapsed, SeedAttemptTerminal::Join(&join_result));

        let result = join_result
            .expect("join must succeed")
            .expect("seed helper returns Ok on missing base");
        assert_eq!(
            result.fallback_reason,
            Some(cargo_target_seed::CargoTargetSeedFallback::BaseMissing),
            "missing base must produce cold-start fallback"
        );

        let after = djinn_telemetry::render().expect("render after");
        assert_eq!(
            seed_count(&after, "ok"),
            ok_before + 1.0,
            "cold-start fallback seed is a successful attempt: one ok sample"
        );
        assert!(
            (seed_sum(&after, "ok") - ok_sum_before - elapsed.as_secs_f64()).abs() < 0.001,
            "ok seed sum delta must equal elapsed for cold-start fallback"
        );
        assert_no_seed_identity_labels(&after);
    }

    /// A seed that returns a setup error records exactly one `error` sample.
    #[test]
    fn seed_records_one_error_sample_on_setup_failure() {
        let _guard = seed_telemetry_guard();
        djinn_telemetry::init().expect("telemetry init");

        // Point base at a path under a file (not a dir) so create_dir_all fails.
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocker = tmp.path().join("blocker-file");
        std::fs::write(&blocker, b"not a dir").expect("write blocker");
        // run_dir is under blocker-file → create_dir_all fails → io::Err
        let run = blocker.join("run-target");

        let before = djinn_telemetry::render().expect("render before");
        let err_before = seed_count(&before, "error");
        let err_sum_before = seed_sum(&before, "error");
        let elapsed = Duration::from_millis(90);

        let base = tmp.path().join("any-base");
        let join_result =
            completed_seed_join(cargo_target_seed::seed_cargo_target_dir(&base, &run));
        record_seed_terminal_seconds(elapsed, SeedAttemptTerminal::Join(&join_result));

        // The injected join succeeds but the seed returns Err (create_dir_all failed).
        assert!(
            join_result.is_ok(),
            "spawn_blocking join must succeed even if seed returns Err"
        );

        let after = djinn_telemetry::render().expect("render after");
        assert_eq!(
            seed_count(&after, "error"),
            err_before + 1.0,
            "one error seed sample expected for setup failure"
        );
        assert!(
            (seed_sum(&after, "error") - err_sum_before - elapsed.as_secs_f64()).abs() < 0.001,
            "error seed sum delta must equal elapsed"
        );
        assert_no_seed_identity_labels(&after);
    }

    /// A deterministically injected terminal cancellation records exactly one
    /// `cancelled` sample via the same terminal recording boundary used after
    /// the worker seed join resolves.
    #[test]
    fn seed_records_one_cancelled_sample() {
        let _guard = seed_telemetry_guard();
        djinn_telemetry::init().expect("telemetry init");

        let before = djinn_telemetry::render().expect("render before");
        let cancel_before = seed_count(&before, "cancelled");
        let cancel_sum_before = seed_sum(&before, "cancelled");
        let elapsed = Duration::from_millis(50);

        record_seed_terminal_seconds(elapsed, SeedAttemptTerminal::Cancelled);

        let after = djinn_telemetry::render().expect("render after");
        assert_eq!(
            seed_count(&after, "cancelled"),
            cancel_before + 1.0,
            "one cancelled seed sample expected"
        );
        assert!(
            (seed_sum(&after, "cancelled") - cancel_sum_before - elapsed.as_secs_f64()).abs()
                < 0.001,
            "cancelled seed sum delta must equal elapsed"
        );
        assert_no_seed_identity_labels(&after);
    }

    // ── Workspace cleanup telemetry tests (proposal zp5t) ──────────────────

    /// Serialize telemetry-scrape tests so each can use a precise delta.
    static CLEANUP_TELEMETRY_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) fn cleanup_telemetry_guard() -> std::sync::MutexGuard<'static, ()> {
        CLEANUP_TELEMETRY_MUTEX
            .lock()
            .expect("cleanup telemetry test mutex poisoned")
    }

    /// Count `workspace_cleanup_seconds_count` samples for a given
    /// trigger/outcome pair.
    fn cleanup_count(rendered: &str, trigger: &str, outcome: &str) -> f64 {
        rendered
            .lines()
            .find(|line| {
                line.starts_with("djinn_workspace_cleanup_seconds_count")
                    && line.contains(&format!("trigger=\"{trigger}\""))
                    && line.contains(&format!("outcome=\"{outcome}\""))
            })
            .and_then(|line| line.rsplit_once(' ').and_then(|(_, v)| v.parse().ok()))
            .unwrap_or(0.0)
    }

    /// Read the `_sum` value for `workspace_cleanup_seconds` for a given
    /// trigger/outcome pair.
    #[test]
    fn attached_workspace_teardown_owned_is_noop_no_sample() {
        let _guard = cleanup_telemetry_guard();
        djinn_telemetry::init().expect("telemetry init");

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().to_path_buf();
        let ws = Workspace::attach_existing(&path, "main").expect("attach");
        assert!(!ws.is_owned(), "attached workspace must not be owned");

        let before = djinn_telemetry::render().expect("render before");
        let shutdown_ok_before = cleanup_count(&before, "shutdown", "ok");

        // teardown_owned is a no-op for Attached — no delete, no observation.
        ws.teardown_owned().expect("attached teardown must be Ok");

        let after = djinn_telemetry::render().expect("render after");
        assert!(
            path.exists(),
            "attached directory must NOT be deleted by teardown_owned"
        );
        assert_eq!(
            cleanup_count(&after, "shutdown", "ok"),
            shutdown_ok_before,
            "attached teardown must NOT emit any sample"
        );
    }
}
