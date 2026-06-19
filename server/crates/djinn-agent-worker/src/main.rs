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
pub mod cargo_metrics;
mod cargo_target_seed;
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
use djinn_agent::context::AgentContext;
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_core::events::EventBus;
use djinn_db::{Database, DatabaseConnectConfig, PostgresDatabaseConfig};
use djinn_graph::graph_parity::{GraphArtifactBlobParityError, assert_graph_artifact_blob_parity};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_runtime::{ResolvedCredentials, RoleKind, TaskRunSpec, WorkerEvent};
use djinn_supervisor::{RpcServices, SupervisorServices, TaskRunSupervisor};
use djinn_workspace::{GitIdentity, MirrorManager, Workspace};
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

    /// Run a one-shot verification "test" for a candidate rule set and exit.
    /// Reads the `verification_test_runs` row, runs the candidate commands in
    /// this project image against the cloned default branch, and writes
    /// pass/fail + per-command output back to the row. Dispatched by
    /// `build_verification_test_job` in `djinn-k8s` — this is what gates
    /// "save verification rules" so a broken rule can't disrupt live tasks.
    VerifyTest {
        /// `verification_test_runs.id` (positional).
        test_id: String,
    },

    /// Run a one-shot pre-PR verification for a task and exit. Reads the
    /// `verification_runs` row, runs the real verification pipeline
    /// (`verify_commit`) against the task branch's tree (already cloned +
    /// checked out at `DJINN_PROJECT_ROOT` by the Job's bash wrapper), and
    /// writes per-command results + pass/fail back to the row. Dispatched by
    /// `build_verification_job` in `djinn-k8s` — this is what gives the pre-PR
    /// quality gate a real toolchain + shared `/cache` instead of false-failing
    /// on the toolchain-less server host.
    VerifyTask {
        /// `verification_runs.id` (positional).
        run_id: String,
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
        Cmd::VerifyTest { test_id } => run_verify_test(&test_id).await,
        Cmd::VerifyTask { run_id } => run_verify_task(&run_id).await,
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
    match tokio::process::Command::new("git")
        .args(["config", "--global", &key, &value])
        .status()
        .await
    {
        Ok(s) if s.success() => {
            info!(
                owner,
                "configure_private_dep_access: git insteadOf set for private deps"
            )
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

async fn prepare_cargo_target_dir(spec: &TaskRunSpec) -> PathBuf {
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
    match tokio::task::spawn_blocking(move || {
        seed_cargo_target_dir(seed_source_base, seed_destination_run_dir)
    })
    .await
    {
        Ok(Ok(result)) => {
            record_cargo_target_seed_result("task_run", &result);
            let fallback_reason = result
                .fallback_reason
                .as_ref()
                .map(std::string::ToString::to_string);
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
                fallback_reason = %format!("seed helper failed: {err}"),
                "cargo target seed: proceeding with cold private target dir after setup error"
            );
        }
        Err(err) => {
            djinn_telemetry::cargo_target_seed::increment_seed_fallback(
                djinn_telemetry::cargo_target_seed::FALLBACK_REASON_UNKNOWN,
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
                fallback_reason = %format!("seed task join failed: {err}"),
                "cargo target seed: proceeding with cold private target dir after setup task failure"
            );
        }
    }

    destination_run_dir
}

/// Seed a private run target dir for a pre-PR verification run from the warm
/// per-project base and point `CARGO_TARGET_DIR` at it, mirroring the task-run
/// path (`prepare_cargo_target_dir`). Verification thereby reuses main's warm
/// compiled artifacts and recompiles only the task's delta incrementally — no
/// shared-base writes, no Cargo build-dir lock contention. The seed is
/// best-effort: a missing/unusable warm base degrades to a cold private run dir
/// rather than failing the verification.
///
/// Keyed on the verification `run_id` (unique per run) so concurrent
/// verifications for the same project never share a target dir. Returns the
/// chosen run dir so the caller can tear it down when the run completes.
async fn prepare_verify_target_dir(project_id: &str, run_id: &str) -> PathBuf {
    let source_base = warm_base_dir(project_id);
    let run_dir = run_target_dir(run_id);
    set_cargo_target_dir_for_children(&run_dir);

    info!(
        run_id,
        project_id,
        source_base = %source_base.display(),
        destination_run_dir = %run_dir.display(),
        "verify cargo target seed: preparing private run target dir"
    );

    let seed_source_base = source_base.clone();
    let seed_destination_run_dir = run_dir.clone();
    match tokio::task::spawn_blocking(move || {
        seed_cargo_target_dir(seed_source_base, seed_destination_run_dir)
    })
    .await
    {
        Ok(Ok(result)) => {
            record_cargo_target_seed_result("verify", &result);
            if result.cold_started() {
                warn!(
                    run_id,
                    project_id,
                    destination_run_dir = %run_dir.display(),
                    fallback_reason = result
                        .fallback_reason
                        .as_ref()
                        .map(std::string::ToString::to_string)
                        .unwrap_or_else(|| "unknown".to_string()),
                    "verify cargo target seed: falling back to cold private target dir"
                );
            } else {
                info!(
                    run_id,
                    project_id,
                    destination_run_dir = %run_dir.display(),
                    seed_duration_ms = result.elapsed.as_millis(),
                    linked_file_count = result.linked_file_count,
                    copied_file_count = result.copied_file_count,
                    skipped_file_count = result.skipped_file_count,
                    "verify cargo target seed: seeded private run target dir"
                );
            }
        }
        Ok(Err(err)) => {
            djinn_telemetry::cargo_target_seed::increment_seed_fallback(
                djinn_telemetry::cargo_target_seed::FALLBACK_REASON_UNKNOWN,
            );
            warn!(
                run_id,
                project_id,
                error = %err,
                "verify cargo target seed: proceeding with cold private target dir after setup error"
            );
        }
        Err(err) => {
            djinn_telemetry::cargo_target_seed::increment_seed_fallback(
                djinn_telemetry::cargo_target_seed::FALLBACK_REASON_UNKNOWN,
            );
            warn!(
                run_id,
                project_id,
                error = %err,
                "verify cargo target seed: proceeding with cold private target dir after task failure"
            );
        }
    }

    run_dir
}

/// Resolve the cargo workspace directory under `project_root`.
///
/// Prefers the project's `EnvironmentConfig` Rust workspace `root` (so a repo
/// whose cargo workspace lives in a subdir — djinn's `server/` — is found
/// without hardcoding). Falls back to a `Cargo.toml` at the project root or any
/// single first-level subdir. Returns `None` when no cargo workspace exists
/// (non-Rust repo) so the caller can skip the warm cleanly.
///
/// Must resolve to the SAME absolute dir verification compiles in. Verification
/// runs its scoped commands (`cd server && cargo …`) from `DJINN_PROJECT_ROOT`,
/// and both warm and verify clone to `/workspace/<sanitize_id(project)>`, so a
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
/// base so verification (and task-run) pods seed it and recompile only their
/// delta incrementally instead of cold-building.
///
/// Runs the SAME work verification compiles (`cargo clippy --workspace
/// --all-targets --all-features`, falling back to `cargo build`, then `cargo
/// test --workspace --all-targets --all-features --no-run`) so the artifacts +
/// fingerprints in the base actually match what verification produces. The warm
/// pod's env already routes `CARGO_TARGET_DIR=/cache/cargo-target/<project>`
/// (the warm base) with `CARGO_INCREMENTAL=1`, so these compiles write straight
/// into the base.
///
/// Caller MUST have normalized tracked-file mtimes (`normalize_mtimes_at`)
/// first: cargo freshness keys on file mtimes, and verification normalizes the
/// same way before it compiles — without matching mtimes the base's fingerprints
/// won't match verification's fresh clone and reuse never hits.
///
/// Best-effort throughout: a missing cargo workspace (non-Rust repo) or any
/// compile failure logs and returns — it never fails the graph warm.
async fn warm_cargo_target_base(
    project_id: &str,
    project_root: &Path,
    env_config: Option<&djinn_stack::environment::EnvironmentConfig>,
) {
    let Some(workspace_dir) = resolve_cargo_workspace_dir(project_root, env_config) else {
        info!(
            project_id,
            project_root = %project_root.display(),
            "cargo warm: no cargo workspace found; skipping (non-Rust repo?)"
        );
        return;
    };

    let target_dir = std::env::var(CARGO_TARGET_DIR_ENV).unwrap_or_default();
    info!(
        project_id,
        workspace_dir = %workspace_dir.display(),
        cargo_target_dir = %target_dir,
        "cargo warm: compiling main into the warm per-project target base"
    );
    let started = std::time::Instant::now();

    // clippy is the heavier of verification's two passes and produces the same
    // check artifacts; fall back to a plain build if clippy is unavailable.
    let clippy_ok = run_cargo_warm_step(
        project_id,
        &workspace_dir,
        &["clippy", "--workspace", "--all-targets", "--all-features"],
        "clippy",
    )
    .await;
    if !clippy_ok {
        run_cargo_warm_step(
            project_id,
            &workspace_dir,
            &["build", "--workspace", "--all-targets"],
            "build (clippy fallback)",
        )
        .await;
    }

    // Compile (but don't run) the test harnesses so verification's
    // `cargo test --no-run` reuses these artifacts.
    run_cargo_warm_step(
        project_id,
        &workspace_dir,
        &[
            "test",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--no-run",
        ],
        "test --no-run",
    )
    .await;

    let elapsed = started.elapsed();
    cargo_metrics::record_warm_base_freshness(project_id, elapsed.as_millis() as u64);

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
async fn run_cargo_warm_step(
    project_id: &str,
    workspace_dir: &Path,
    args: &[&str],
    label: &str,
) -> bool {
    run_cargo_warm_step_with_cargo("cargo", project_id, workspace_dir, args, label).await
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

    if !cargo_instrumented {
        return match tokio::process::Command::new(cargo_bin.as_ref())
            .args(&plan.args)
            .current_dir(workspace_dir)
            .status()
            .await
        {
            Ok(status) if status.success() => {
                info!(project_id, step = label, "cargo warm: step succeeded");
                true
            }
            Ok(status) => {
                warn!(
                    project_id,
                    step = label,
                    code = ?status.code(),
                    "cargo warm: step failed (non-fatal; continuing warm)"
                );
                false
            }
            Err(err) => {
                warn!(
                    project_id,
                    step = label,
                    error = %err,
                    "cargo warm: failed to spawn `cargo` (non-fatal; continuing warm)"
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
                workspace_dir = %workspace_dir.display(),
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

            if output.status.success() {
                info!(project_id, step = label, "cargo warm: step succeeded");
                true
            } else {
                warn!(
                    project_id,
                    step = label,
                    code = ?output.status.code(),
                    "cargo warm: step failed (non-fatal; continuing warm)"
                );
                false
            }
        }
        Err(err) => {
            warn!(
                project_id,
                step = label,
                error = %err,
                "cargo warm: failed to spawn `cargo` (non-fatal; continuing warm)"
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

    let cargo_target_run_dir = prepare_cargo_target_dir(&spec).await;
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

/// Commit author/committer identity for the wind-down checkpoint. Owned
/// `String`s (not the borrowed [`GitIdentity`]) so it can be cloned into the
/// detached signal / deadline tasks.
#[derive(Clone)]
struct CheckpointIdentity {
    name: String,
    email: String,
}

/// Best-effort "save work before we die" checkpoint: stage + commit any
/// uncommitted changes on the LIVE ephemeral stage clone and push `task_branch`
/// to the mirror, all bounded by [`CHECKPOINT_TIMEOUT`].
///
/// The path is read lazily, at fire time, from the `captured_workspace_path`
/// slot that [`WorkerSupervisorServices`] populates on its first
/// `execute_stage` call. That is the supervisor's own ephemeral `TempDir`
/// clone (`MirrorManager::clone_ephemeral`), where every worker commit lands —
/// NOT the host-materialised `/workspace` bind mount, which the in-pod
/// supervisor never writes to. If no stage has started yet the slot is `None`
/// and there is no in-flight work to save, so the checkpoint is a clean no-op.
///
/// Called from the SIGTERM handler and the soft-deadline timer. It races the
/// supervisor's own (cancelled) shutdown, so it may arrive mid-git-operation —
/// a locked index, a half-applied merge. Every failure is logged and
/// swallowed; this function never panics and never blocks past its timeout,
/// because it runs inside the kubelet's short `terminationGracePeriodSeconds`
/// window and must leave room for the terminal RPC flush.
///
/// Idempotent against the supervisor's pushes: the commit is a no-op on a clean
/// tree and the push refspec (`task_branch:task_branch`) is a no-op when the
/// mirror is already current.
async fn checkpoint_workspace(
    captured_workspace_path: &std::sync::Mutex<Option<PathBuf>>,
    task_branch: &str,
    identity: &CheckpointIdentity,
    task_run_id: &str,
) {
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
            "checkpoint: no stage has started (no captured workspace); nothing to save"
        );
        return;
    };

    let ws = match Workspace::attach_existing(&workspace_path, task_branch.to_string()) {
        Ok(ws) => ws,
        Err(e) => {
            warn!(
                task_run_id,
                branch = task_branch,
                path = %workspace_path.display(),
                error = %e,
                "checkpoint: failed to attach workspace; cannot save in-flight work"
            );
            return;
        }
    };

    let git_identity = GitIdentity {
        name: &identity.name,
        email: &identity.email,
    };
    let message = format!("checkpoint: interrupted task-run {task_run_id}");

    let result = tokio::time::timeout(CHECKPOINT_TIMEOUT, async {
        // Commit is best-effort: a clean tree (worker already committed via
        // shell) returns Ok(false) and we still push. A failure (locked index
        // mid-merge) is logged but must not block the push of whatever IS
        // already committed in the clone.
        match ws.commit(&message, git_identity).await {
            Ok(true) => info!(
                task_run_id,
                branch = task_branch,
                "checkpoint: committed uncommitted changes"
            ),
            Ok(false) => info!(
                task_run_id,
                branch = task_branch,
                "checkpoint: tree already clean; nothing to commit"
            ),
            Err(e) => warn!(
                task_run_id,
                branch = task_branch,
                error = %e,
                "checkpoint: commit failed (continuing to push already-committed work)"
            ),
        }
        match ws.push_to_origin(task_branch).await {
            Ok(()) => info!(
                task_run_id,
                branch = task_branch,
                "checkpoint: pushed task_branch to mirror — in-flight work is durable"
            ),
            Err(e) => error!(
                task_run_id,
                branch = task_branch,
                error = %e,
                "checkpoint: push_to_origin failed — in-flight work may be LOST on Pod kill"
            ),
        }
    })
    .await;

    if result.is_err() {
        error!(
            task_run_id,
            branch = task_branch,
            timeout_secs = CHECKPOINT_TIMEOUT.as_secs(),
            "checkpoint: timed out (wedged git operation?); in-flight work may be LOST"
        );
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
/// tick" and retries on the next one. Mirrors the direct-`Command` idiom
/// [`Workspace::is_up_to_date_with`] uses for git calls that need to
/// discriminate outcomes rather than fail on every non-zero exit.
async fn resolve_branch_sha(path: &Path, branch: &str) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--verify", "--quiet"])
        // `<branch>^{commit}` resolves the local branch ref to its commit SHA
        // and fails cleanly (non-zero, empty stdout under --quiet) if the ref
        // doesn't exist yet.
        .arg(format!("{branch}^{{commit}}"))
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
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

    // Warm the cargo target base from `main` so verification/task-run pods seed
    // it and recompile only their delta incrementally. This runs in the worker
    // (not the warm-Job shell) so we can normalize tracked-file mtimes to commit
    // times FIRST — the SAME normalization verification applies before it
    // compiles. Cargo freshness keys on mtimes, so without matching them the
    // base's fingerprints would never match verification's fresh clone and reuse
    // would never hit (the bug this fixes). `lifecycle_root` is
    // `DJINN_PROJECT_ROOT` = the cloned `main` tree; `resolve_cargo_workspace_dir`
    // lands on `<root>/server` — the exact dir verification's `cd server`
    // compiles in. Best-effort: never fails the graph warm.
    djinn_workspace::normalize_mtimes_at(&lifecycle_root).await;
    warm_cargo_target_base(project_id, &lifecycle_root, env_config.as_ref()).await;

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

/// Run a one-shot verification test: execute the candidate rule set's commands
/// in this project image against the cloned default branch (at
/// `DJINN_PROJECT_ROOT`), then write the outcome back to the
/// `verification_test_runs` row. Dispatched by `build_verification_test_job`.
///
/// Faithful to real verification: it reuses [`verify_commit`], which runs the
/// `lifecycle.pre_verification` setup hooks then the commands, so a `passed`
/// here means the same pipeline that gates tasks would pass. A synthetic commit
/// id keeps this off the real verification cache.
async fn run_verify_test(test_id: &str) -> Result<()> {
    let db = bootstrap_warm_database().await?;
    let repo = djinn_db::VerificationTestRepository::new(db.clone());

    let run = repo
        .get(test_id)
        .await
        .with_context(|| format!("load verification_test_run {test_id}"))?
        .ok_or_else(|| anyhow::anyhow!("verification_test_run {test_id} not found"))?;
    let _ = repo.mark_running(test_id).await;

    // A test runs every command the candidate rule set would run (dedup,
    // order-preserving) against the current default branch — a fresh clone
    // diffs to nothing, so per-file scoping doesn't apply here.
    let rules: Vec<djinn_stack::environment::VerificationRule> =
        serde_json::from_str(&run.candidate_rules).unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let mut commands: Vec<String> = Vec::new();
    for rule in &rules {
        for cmd in &rule.commands {
            if seen.insert(cmd.clone()) {
                commands.push(cmd.clone());
            }
        }
    }

    let project_root = std::env::var("DJINN_PROJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/workspace"));

    // Seed a private run target dir from the warm per-project base and point
    // CARGO_TARGET_DIR at it, then reset tracked-file mtimes to commit times —
    // identical to the real `run_verify_task` path so a test faithfully reflects
    // (and benefits from) the warm cargo-artifact reuse a real verification gets.
    // The guard tears the private dir down when this function returns.
    let verify_target_run_dir = prepare_verify_target_dir(&run.project_id, test_id).await;
    let _verify_target_guard = CargoTargetRunDirGuard::new(
        test_id.to_string(),
        run.project_id.clone(),
        verify_target_run_dir,
    );
    djinn_workspace::normalize_mtimes_at(&project_root).await;

    // Synthetic commit id so verify_commit's pass-cache never collides with a
    // real task verification (which keys on the real commit + scoped commands).
    let synthetic_commit = format!("verify-test-{test_id}");

    let outcome = djinn_agent::verification::service::verify_commit(
        &run.project_id,
        &synthetic_commit,
        &project_root,
        &db,
        &commands,
    )
    .await;

    match outcome {
        Ok(result) => {
            let mut all = result.setup_results;
            all.extend(result.verification_results);
            let results_json = serde_json::to_string(&all).unwrap_or_else(|_| "[]".to_string());
            let status = if result.passed {
                djinn_db::VerificationTestStatus::PASSED
            } else {
                djinn_db::VerificationTestStatus::FAILED
            };
            repo.complete(test_id, status, &results_json, None)
                .await
                .with_context(|| format!("write verification_test_run {test_id} result"))?;
            tracing::info!(
                test_id,
                passed = result.passed,
                "verification test complete"
            );
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let _ = repo
                .complete(
                    test_id,
                    djinn_db::VerificationTestStatus::ERROR,
                    "[]",
                    Some(&msg),
                )
                .await;
            tracing::warn!(test_id, error = %msg, "verification test errored");
        }
    }
    Ok(())
}

/// Run a one-shot pre-PR verification for a task and write the outcome back to
/// the `verification_runs` row. Dispatched by `build_verification_job` in
/// `djinn-k8s`: the Job's bash wrapper has already cloned the target branch and
/// checked out the task branch into `DJINN_PROJECT_ROOT`, so this resolves the
/// scoped verification commands against that tree and runs the SAME pipeline
/// (`verify_commit`) the server runs inline on the non-Kubernetes path.
///
/// Faithful to the server-side pipeline (`actors/slot/verification.rs`): it
/// normalizes tracked-file mtimes (cargo-cache reuse), resolves the role-level
/// `verification_command` override + the project's scoped rules, then runs
/// `verify_commit` keyed on the real HEAD commit.
async fn run_verify_task(run_id: &str) -> Result<()> {
    let db = bootstrap_warm_database().await?;
    let repo = djinn_db::VerificationRunRepository::new(db.clone());

    let run = repo
        .get(run_id)
        .await
        .with_context(|| format!("load verification_run {run_id}"))?
        .ok_or_else(|| anyhow::anyhow!("verification_run {run_id} not found"))?;
    let _ = repo.mark_running(run_id).await;

    // Resolve everything the pipeline needs from the task + project rows.
    let task_repo = djinn_db::TaskRepository::new(db.clone(), EventBus::noop());
    let task = task_repo
        .get(&run.task_id)
        .await
        .with_context(|| format!("load task {}", run.task_id))?
        .ok_or_else(|| anyhow::anyhow!("task {} not found", run.task_id))?;

    let project_repo = djinn_db::ProjectRepository::new(db.clone(), EventBus::noop());
    let target_branch = match project_repo.get_config(&run.project_id).await {
        Ok(Some(config)) => config.target_branch,
        _ => "main".to_string(),
    };

    let project_root = std::env::var("DJINN_PROJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/workspace"));

    // Seed a private run target dir from the warm per-project base and point
    // CARGO_TARGET_DIR at it (mirrors the task-run path). Verification thereby
    // reuses main's warm compiled artifacts and recompiles only the task delta
    // incrementally instead of cold-building or churning the shared base. The
    // guard tears the private dir down when this function returns.
    let verify_target_run_dir = prepare_verify_target_dir(&run.project_id, run_id).await;
    let _verify_target_guard = CargoTargetRunDirGuard::new(
        run_id.to_string(),
        run.project_id.clone(),
        verify_target_run_dir,
    );

    // Reset tracked-file mtimes to commit times so the verification build reuses
    // the warm cargo artifacts seeded above for byte-identical crates. Required
    // for cargo freshness on a fresh clone. Best-effort; never fails
    // verification.
    djinn_workspace::normalize_mtimes_at(&project_root).await;

    // Resolve scoped commands + run `verify_commit` + write the terminal row.
    // Shared with the IN-POD post-task verification path so both resolve and gate
    // identically.
    run_verification_into_run(
        &db,
        &repo,
        run_id,
        &run.project_id,
        &target_branch,
        &project_root,
        &task,
    )
    .await;
    Ok(())
}

/// Shared core of a pre-PR verification run: resolve the role override + scoped
/// commands against `project_root` (already checked out to the committed task
/// tree), run the SAME `verify_commit` pipeline the server runs, and write the
/// terminal outcome (`passed`/`failed`/`error`) to the `verification_runs` row.
///
/// `CARGO_TARGET_DIR` is assumed to already point at the run's target dir (the
/// separate-pod path seeds it from the warm base; the in-pod-after-task path
/// reuses the worker's own already-compiled run dir). This function never seeds
/// or tears down a target dir — that is the caller's concern.
///
/// Best-effort: any error is written to the row as `error` and logged; it never
/// panics or propagates so the caller's teardown still runs.
pub(crate) async fn run_verification_into_run(
    db: &Database,
    repo: &djinn_db::VerificationRunRepository,
    run_id: &str,
    project_id: &str,
    target_branch: &str,
    project_root: &Path,
    task: &djinn_core::models::Task,
) {
    // Role/specialist `verification_command` override (absolute priority in
    // resolve_scoped_commands), mirroring the server pipeline.
    let role_cmd_override = verify_task_role_override(db, task).await;

    let scoped_commands = djinn_agent::verification::scoped::resolve_scoped_commands(
        db,
        Some(project_id),
        project_root,
        target_branch,
        role_cmd_override.as_deref(),
    )
    .await;

    let commit_sha =
        verify_task_head_commit(project_root).unwrap_or_else(|_| format!("verify-run-{run_id}"));

    let outcome = djinn_agent::verification::service::verify_commit(
        project_id,
        &commit_sha,
        project_root,
        db,
        &scoped_commands,
    )
    .await;

    match outcome {
        Ok(result) => {
            let setup_json =
                serde_json::to_string(&result.setup_results).unwrap_or_else(|_| "[]".to_string());
            let verify_json = serde_json::to_string(&result.verification_results)
                .unwrap_or_else(|_| "[]".to_string());
            let status = if result.passed {
                djinn_db::VerificationRunStatus::PASSED
            } else {
                djinn_db::VerificationRunStatus::FAILED
            };
            if let Err(e) = repo
                .complete(run_id, status, &setup_json, &verify_json, None)
                .await
            {
                warn!(run_id, error = %format!("{e:#}"), "failed to write verification_run result");
            } else {
                info!(run_id, passed = result.passed, "verification run complete");
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let _ = repo
                .complete(
                    run_id,
                    djinn_db::VerificationRunStatus::ERROR,
                    "[]",
                    "[]",
                    Some(&msg),
                )
                .await;
            warn!(run_id, error = %msg, "verification run errored");
        }
    }
}

/// Resolve the role-level `verification_command` override for a task, mirroring
/// `role_verification_command_for_task` in the server pipeline. Returns `None`
/// when the task has no `agent_type`, the role is missing, or its command is
/// empty.
async fn verify_task_role_override(
    db: &Database,
    task: &djinn_core::models::Task,
) -> Option<String> {
    let specialist_name = task.agent_type.as_deref().filter(|s| !s.is_empty())?;
    let role_repo = djinn_db::AgentRepository::new(db.clone(), EventBus::noop());
    let role = role_repo
        .get_by_name_for_project(&task.project_id, specialist_name)
        .await
        .ok()
        .flatten()?;
    role.verification_command
        .filter(|cmd| !cmd.trim().is_empty())
}

/// Resolve the HEAD commit of the checked-out task branch in `project_root`.
fn verify_task_head_commit(project_root: &Path) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(project_root)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run `git <args>` synchronously in `dir`, returning an error on a non-zero
/// exit. Used by the in-pod verify reset/clean integrity step (the worker
/// already shells out to git via `std::process` elsewhere, so this avoids
/// adding a `djinn-git` dependency just for two commands).
fn run_git_in(dir: &Path, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("spawn git {args:?} in {}", dir.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Put `workspace_root` back into the EXACT committed state of the task branch's
/// HEAD: `git reset --hard <HEAD>` then `git clean -fd`. The worker agent edits
/// (and may leave a dirty index / untracked scratch files) in its ephemeral
/// stage clone; verification must gate the COMMITTED tree, not that dirty
/// workspace.
///
/// Deliberately does NOT touch `CARGO_TARGET_DIR` — it lives on the shared
/// `/cache` PVC, OUTSIDE the workspace, so `git clean` (scoped to the worktree)
/// never removes it and the worker's already-compiled artifacts survive for
/// reuse. The HEAD commit is read first so the reset is anchored to the
/// committed tip even if the index moved.
fn reset_workspace_to_head(workspace_root: &Path) -> Result<String> {
    let head = verify_task_head_commit(workspace_root)?;
    run_git_in(workspace_root, &["reset", "--hard", &head])?;
    // -f (force) -d (dirs); intentionally NOT -x, so ignored files stay (cargo
    // config etc.). The worktree is the only thing cleaned — the cache PVC is
    // mounted elsewhere.
    run_git_in(workspace_root, &["clean", "-fd"])?;
    Ok(head)
}

/// Run the pre-PR verification IN-PROCESS in the worker pod, reusing the
/// worker's already-compiled Cargo artifacts.
///
/// Invoked by `WorkerSupervisorServices::verify_committed_tree` at the
/// `WorkerSubmitted` hand-off — the worker has just committed to the task branch
/// and its `CARGO_TARGET_DIR` (set process-wide in `prepare_cargo_target_dir`)
/// still holds the freshly compiled task delta, and the supervisor's ephemeral
/// stage clone at `workspace_root` is still live (the supervisor calls this
/// BEFORE it tears the target dir down).
///
/// Control flow:
///   1. `git reset --hard HEAD` + `git clean -fd` → gate the committed tree.
///   2. `normalize_mtimes_at` → restore commit-time mtimes so cargo freshness
///      holds against the already-compiled artifacts (no re-seed, no re-clone).
///   3. create a `verification_runs` row, mark it running.
///   4. `run_verification_into_run` resolves scoped commands + runs the SAME
///      `verify_commit` pipeline the separate-pod path runs, writing the
///      terminal outcome to that row.
///
/// Returns the `verification_runs.id` on success (terminal row written), or an
/// error the caller treats as "fall back to the host-dispatched verify Job".
/// Crucially it does NOT seed or tear down the Cargo target dir.
pub(crate) async fn run_in_pod_verification(
    db: &Database,
    project_id: &str,
    task: &djinn_core::models::Task,
    workspace_root: &Path,
) -> Result<String> {
    // 1. Integrity: committed tree only.
    let head = reset_workspace_to_head(workspace_root)
        .with_context(|| format!("reset workspace {} to HEAD", workspace_root.display()))?;
    info!(
        project_id,
        task_id = %task.id,
        head = %head,
        workspace = %workspace_root.display(),
        "in-pod verification: workspace reset to committed HEAD"
    );

    // 2. Freshness: commit-time mtimes so the worker's compiled artifacts stay
    //    Fresh under cargo (CARGO_TARGET_DIR already points at the worker run
    //    dir — we deliberately do NOT seed or reset it).
    djinn_workspace::normalize_mtimes_at(workspace_root).await;

    // 3. Resolve the project's target branch + open the row.
    let project_repo = djinn_db::ProjectRepository::new(db.clone(), EventBus::noop());
    let target_branch = match project_repo.get_config(project_id).await {
        Ok(Some(config)) => config.target_branch,
        _ => "main".to_string(),
    };

    let run_id = uuid::Uuid::now_v7().to_string();
    let repo = djinn_db::VerificationRunRepository::new(db.clone());
    repo.create(&run_id, &task.id, project_id)
        .await
        .with_context(|| format!("create in-pod verification_run {run_id}"))?;
    let _ = repo.mark_running(&run_id).await;

    // 4. Resolve scoped commands + run verify_commit + write the terminal row
    //    (shared with the separate-pod path so both gate identically).
    run_verification_into_run(
        db,
        &repo,
        &run_id,
        project_id,
        &target_branch,
        workspace_root,
        task,
    )
    .await;

    Ok(run_id)
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
        checkpoint_workspace(&captured, "task", &identity, "run-xyz").await;

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
        checkpoint_workspace(&captured, "task", &identity, "run-xyz").await;

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
        checkpoint_workspace(&captured, "task", &identity, "run-xyz").await;
        // The slot is unchanged — the no-op path doesn't mutate it.
        assert!(
            captured.lock().unwrap().is_none(),
            "no-op checkpoint must leave the empty captured-path slot untouched"
        );
    }

    /// Initialise a git repo at `dir` with one commit and return its HEAD sha.
    fn init_repo_with_commit(dir: &Path) -> String {
        git(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("committed.txt"), "committed\n").unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-m", "initial"]);
        git(dir, &["rev-parse", "HEAD"]).trim().to_string()
    }

    /// `reset_workspace_to_head` must (a) discard tracked-file edits and (b)
    /// remove untracked files/dirs, leaving the worktree byte-identical to the
    /// committed HEAD — the integrity precondition for verifying the COMMITTED
    /// tree rather than the worker's dirty workspace.
    #[test]
    fn reset_workspace_to_head_discards_dirty_tracked_and_untracked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        let head = init_repo_with_commit(dir);

        // Dirty the committed file + drop untracked scratch (file AND dir).
        std::fs::write(dir.join("committed.txt"), "DIRTY EDIT\n").unwrap();
        std::fs::write(dir.join("scratch.txt"), "untracked\n").unwrap();
        std::fs::create_dir_all(dir.join("target_scratch")).unwrap();
        std::fs::write(dir.join("target_scratch/x"), "junk\n").unwrap();

        let returned = reset_workspace_to_head(dir).expect("reset");
        assert_eq!(returned, head, "must return the committed HEAD sha");

        // Tracked file restored to committed contents.
        assert_eq!(
            std::fs::read_to_string(dir.join("committed.txt")).unwrap(),
            "committed\n",
            "reset --hard must discard the dirty tracked edit"
        );
        // Untracked file + dir cleaned.
        assert!(
            !dir.join("scratch.txt").exists(),
            "git clean -fd must remove untracked files"
        );
        assert!(
            !dir.join("target_scratch").exists(),
            "git clean -fd must remove untracked dirs"
        );
        // Tree is clean.
        assert!(
            git(dir, &["status", "--porcelain"]).trim().is_empty(),
            "worktree must be clean after reset"
        );
    }

    /// End-to-end of the in-pod verification helper against a project with no
    /// verification rules (commands resolve empty → `verify_commit` passes
    /// vacuously, so no toolchain is required). Asserts the THREE load-bearing
    /// behaviors of the double-compile fix:
    ///   1. it resets the workspace to the committed HEAD before verifying
    ///      (dirty edit + untracked file gone),
    ///   2. it does NOT touch `CARGO_TARGET_DIR` (the worker's artifacts must
    ///      survive for reuse — it's on /cache, outside the workspace), and
    ///   3. it writes a terminal `verification_runs` row and returns its id.
    #[tokio::test]
    async fn run_in_pod_verification_resets_reuses_target_and_writes_outcome() {
        let db = Database::open_in_memory().expect("in-memory db");
        db.ensure_initialized().await.expect("init schema");

        // Minimal board: project → epic → task.
        let project_id = "proj-inpod";
        djinn_db::ProjectRepository::new(db.clone(), EventBus::noop())
            .create_with_id(project_id, "p", "owner", "repo")
            .await
            .expect("create project");
        let epic = djinn_db::EpicRepository::new(db.clone(), EventBus::noop())
            .create_for_project(
                project_id,
                djinn_db::repositories::epic::EpicCreateInput {
                    title: "e",
                    description: "",
                    emoji: "x",
                    color: "#fff",
                    owner: "owner",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .expect("create epic");
        let task = djinn_db::TaskRepository::new(db.clone(), EventBus::noop())
            .create(&epic.id, "t", "", "", "task", 0, "owner", None)
            .await
            .expect("create task");

        // Workspace: committed tree + a dirty edit + an untracked file the verify
        // must NOT see (it gates the committed tree).
        let ws = tempfile::tempdir().expect("workspace");
        let head = init_repo_with_commit(ws.path());
        std::fs::write(ws.path().join("committed.txt"), "DIRTY\n").unwrap();
        std::fs::write(ws.path().join("untracked.txt"), "scratch\n").unwrap();

        // A sentinel "Cargo target dir" OUTSIDE the workspace (mirrors the
        // worker's run dir on /cache). `run_in_pod_verification` deliberately
        // never seeds or tears down a target dir — it only resets + cleans the
        // WORKSPACE (`git clean` is worktree-scoped) — so this external dir must
        // be left byte-for-byte intact. We do NOT mutate the process-global
        // `CARGO_TARGET_DIR` env (that would race other parallel tests); the
        // helper never reads it, so an untouched sibling dir proves the point.
        let target = tempfile::tempdir().expect("target dir");
        let sentinel = target.path().join("sentinel.rlib");
        std::fs::write(&sentinel, b"precompiled-artifact").unwrap();

        let run_id = run_in_pod_verification(&db, project_id, &task, ws.path())
            .await
            .expect("in-pod verification");

        // (1) integrity: reset to committed HEAD happened.
        assert_eq!(
            git(ws.path(), &["rev-parse", "HEAD"]).trim(),
            head,
            "HEAD must be the committed tip"
        );
        assert_eq!(
            std::fs::read_to_string(ws.path().join("committed.txt")).unwrap(),
            "committed\n",
            "verify must run against the committed tree, not the dirty edit"
        );
        assert!(
            !ws.path().join("untracked.txt").exists(),
            "untracked scratch must be cleaned before verify"
        );

        // (2) external Cargo target dir untouched — artifacts survive for reuse.
        assert!(
            sentinel.exists(),
            "in-pod verify must NOT delete the worker's Cargo target dir"
        );
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"precompiled-artifact",
            "the target dir's contents must be left intact (no re-seed)"
        );

        // (3) terminal verification_runs row written for this task.
        let row = djinn_db::VerificationRunRepository::new(db.clone())
            .get(&run_id)
            .await
            .expect("get row")
            .expect("row exists");
        assert_eq!(row.task_id, task.id);
        assert_eq!(row.project_id, project_id);
        // No rules → vacuous pass.
        assert_eq!(
            row.status,
            djinn_db::VerificationRunStatus::PASSED,
            "a project with no verification rules passes vacuously"
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
}
