// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process;
use serde::{Deserialize, Serialize};

use super::workspaces::{discover_workspaces, visit_dirs};
use super::{
    ExecutedIndexerCommand, IndexerAvailability, IndexingRun, PlannedIndexerCommand, ScipArtifact,
    SupportedIndexer, WorkspaceWarmStatus, note_missing_indexer_once,
};

const SCIP_ARTIFACT_EXTENSION: &str = "scip";

pub(crate) fn detect_indexers_in_path(path_var: impl AsRef<str>) -> Vec<IndexerAvailability> {
    let path_var = path_var.as_ref();
    SupportedIndexer::ALL
        .into_iter()
        .map(|indexer| IndexerAvailability {
            indexer,
            binary: indexer.binary_name().to_string(),
            path: which_in_path(indexer.binary_name(), path_var),
        })
        .collect()
}

pub(crate) fn detect_indexers() -> Vec<IndexerAvailability> {
    detect_indexers_in_path(std::env::var("PATH").unwrap_or_default())
}

pub(crate) fn plan_indexer_commands(
    project_root: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
    available_indexers: &[IndexerAvailability],
    declared_workspaces: Option<&[djinn_stack::Workspace]>,
) -> Vec<PlannedIndexerCommand> {
    let project_root = project_root.as_ref();
    let output_root = output_root.as_ref();

    // Discovered roots under test trees (e.g.
    // `crates/*/tests/fixtures/<repo>` carrying its own Cargo.toml) are
    // synthetic repos for tests, not indexable source: their manifests
    // routinely reference members that only exist at test runtime, so the
    // indexer fails on them every warm. Skip them UNLESS the project
    // explicitly declares that root as a workspace — declaration is the
    // escape hatch for repos that genuinely keep code there.
    let declared_roots: std::collections::HashSet<PathBuf> = declared_workspaces
        .map(|workspaces| {
            workspaces
                .iter()
                .map(|workspace| PathBuf::from(&workspace.root))
                .collect()
        })
        .unwrap_or_default();

    let plans: Vec<_> = available_indexers
        .iter()
        .flat_map(|availability| {
            let Some(binary_path) = availability.path.as_ref() else {
                return Vec::new();
            };

            discover_workspaces(project_root, availability.indexer)
                .into_iter()
                .filter(|workspace| {
                    if declared_roots.contains(&workspace.root) {
                        return true;
                    }
                    let rel = workspace.root.to_string_lossy().replace('\\', "/");
                    if djinn_core::test_paths::is_test_path(&rel) {
                        tracing::info!(
                            workspace_root = %workspace.root.display(),
                            indexer = availability.indexer.binary_name(),
                            "skipping discovered workspace under a test path (declare it in EnvironmentConfig workspaces to index it)"
                        );
                        return false;
                    }
                    true
                })
                .map(|workspace| {
                    let working_directory = project_root.join(&workspace.root);
                    let output_path = availability.indexer.default_output_path(
                        project_root,
                        output_root,
                        &workspace.slug,
                    );
                    PlannedIndexerCommand {
                        indexer: availability.indexer,
                        binary_path: binary_path.clone(),
                        args: availability.indexer.command_args(&output_path),
                        working_directory: working_directory.clone(),
                        workspace_root: working_directory,
                        workspace_rel_root: workspace.root,
                        workspace_slug: workspace.slug,
                        output_path,
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    warn_on_workspace_divergence(project_root, &plans, declared_workspaces);

    plans
}

fn workspace_declared_slug(workspace: &djinn_stack::Workspace) -> String {
    workspace
        .slug
        .as_deref()
        .filter(|slug| !slug.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            crate::scip_indexer::workspaces::workspace_slug(Path::new(&workspace.root))
        })
}

/// Pure divergence computation between the declared EnvironmentConfig
/// workspace slugs and the slugs produced by marker/discovery for the
/// planned indexer commands. Returned as a single struct so the
/// structured `tracing::warn!` fields in [`warn_on_workspace_divergence`]
/// keep their exact `declared_but_not_found` / `found_but_undeclared`
/// names. Kept `pub(crate)` so in-crate tests can hit it directly
/// without needing a `tracing` subscriber — the production warning path
/// still emits one structured log per call.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceDivergence {
    pub declared_but_not_found: Vec<String>,
    pub found_but_undeclared: Vec<String>,
}

impl WorkspaceDivergence {
    fn is_empty(&self) -> bool {
        self.declared_but_not_found.is_empty() && self.found_but_undeclared.is_empty()
    }
}

/// Compute the divergence between declared workspaces and discovered
/// workspace slugs (one entry per planned indexer command). Returns
/// [`WorkspaceDivergence::default`] when no declared workspaces were
/// supplied — the caller short-circuits the warning in that case.
fn compute_workspace_divergence(
    plans: &[PlannedIndexerCommand],
    declared_workspaces: Option<&[djinn_stack::Workspace]>,
) -> WorkspaceDivergence {
    let Some(declared_workspaces) = declared_workspaces else {
        return WorkspaceDivergence::default();
    };

    let declared: BTreeSet<String> = declared_workspaces
        .iter()
        .map(workspace_declared_slug)
        .collect();

    let found: BTreeSet<String> = plans
        .iter()
        .map(|plan| plan.workspace_slug.clone())
        .collect();
    WorkspaceDivergence {
        declared_but_not_found: declared.difference(&found).cloned().collect(),
        found_but_undeclared: found.difference(&declared).cloned().collect(),
    }
}

fn warn_on_workspace_divergence(
    project_root: &Path,
    plans: &[PlannedIndexerCommand],
    declared_workspaces: Option<&[djinn_stack::Workspace]>,
) {
    if declared_workspaces.is_none() {
        return;
    }

    let divergence = compute_workspace_divergence(plans, declared_workspaces);
    if divergence.is_empty() {
        return;
    }

    // The field names below are part of the structured log contract:
    // operators grep on `declared_but_not_found` / `found_but_undeclared`
    // to surface config drift. Do not rename without coordinating with
    // dashboards / alerting.
    tracing::warn!(
        project_root = %project_root.display(),
        declared_but_not_found = ?divergence.declared_but_not_found,
        found_but_undeclared = ?divergence.found_but_undeclared,
        "SCIP workspace discovery diverged from declared EnvironmentConfig workspaces"
    );
}

/// RAII guard that temporarily sets `CARGO_TARGET_DIR` to a caller-supplied
/// path and restores the previous value (or unsets it) on drop, including on
/// panic unwind. Constructed only inside the indexer single-flight critical
/// section so the env mutation is serialised against other indexer runs.
///
/// SAFETY contract: at most one [`CargoTargetDirGuard`] may be alive at a
/// time across the whole server. This invariant is enforced by the
/// `IndexerLock` (`AppState::indexer_lock`) — every construction site must
/// be inside a critical section that holds that lock (either directly or
/// transitively). Violating the contract leads to a torn env-var state.
struct CargoTargetDirGuard {
    previous: Option<std::ffi::OsString>,
}

impl CargoTargetDirGuard {
    /// Set `CARGO_TARGET_DIR=dir` for the current process and capture the
    /// previous value so [`Drop`] can restore it.
    fn new(dir: &Path) -> Self {
        let previous = std::env::var_os("CARGO_TARGET_DIR");
        // SAFETY: env mutation is serialised by the IndexerLock invariant
        // documented on the type. See contract above.
        unsafe { std::env::set_var("CARGO_TARGET_DIR", dir) };
        Self { previous }
    }
}

impl Drop for CargoTargetDirGuard {
    fn drop(&mut self) {
        // SAFETY: env mutation is serialised by the IndexerLock invariant
        // documented on the type. Drop runs unconditionally on scope exit
        // (including panic unwind), guaranteeing the host process never
        // observes a leaked CARGO_TARGET_DIR after a panic mid-indexer-run.
        unsafe {
            match self.previous.take() {
                Some(prev) => std::env::set_var("CARGO_TARGET_DIR", prev),
                None => std::env::remove_var("CARGO_TARGET_DIR"),
            }
        }
    }
}

/// Indexer entrypoint for callers that **already hold** the server-wide
/// `IndexerLock` (`AppState::indexer_lock`). Behaves like the standard
/// indexer path, including installing the [`CargoTargetDirGuard`] when
/// `target_dir` is supplied, but assumes the caller has already provided
/// the required single-flight lock.
///
/// # Lock contract
///
/// The caller MUST hold `AppState::indexer_lock` (or another mutex with
/// equivalent server-wide single-flight semantics) for the entire duration
/// of this call. Otherwise the `CARGO_TARGET_DIR` mutation can race with
/// other indexer runs and corrupt their build state.
///
/// Used by `mcp_bridge::ensure_canonical_graph`, which acquires the lock
/// itself before doing several other operations and then needs to call
/// the indexer without re-entering the lock.
pub(crate) async fn run_indexers_already_locked(
    project_root: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
    target_dir: Option<&Path>,
    language_filter: Option<&[SupportedIndexer]>,
    declared_workspaces: Option<&[djinn_stack::Workspace]>,
    priors: Option<&super::PriorTimingMap>,
) -> Result<IndexingRun> {
    let _guard = target_dir.map(CargoTargetDirGuard::new);
    run_indexers(
        project_root,
        output_root,
        language_filter,
        declared_workspaces,
        priors,
    )
    .await
}

/// Compute the wall-clock timeout for a planned indexer command.
///
/// When no prior timing exists (a project's first-ever warm), the budget model
/// yields the size-scaled baseline, combined with the legacy
/// `SupportedIndexer::timeout()` cap via `max()` — byte-identical to the
/// shipped fixed-cap behavior. When prior timings ARE supplied (fed from
/// persisted `scip_indexer_timing` rows), the budget adapts: a slow-but-passing
/// run raises the timeout within `max_cap`, and a run that keeps TIMING OUT
/// grows headroom above `max_cap` up to the adaptive ceiling (`max_cap * 3`),
/// so a heavy workspace stops silently dropping out of the graph on every warm.
/// The `max(..., timeout())` floor guarantees the adaptive value never dips
/// below today's static cap.
fn budgeted_timeout_for_plan(
    plan: &PlannedIndexerCommand,
    prior: Option<&super::IndexerPriorTiming>,
) -> std::time::Duration {
    let size = super::budget::estimate_workspace_size(&plan.working_directory, plan.indexer);
    let prior_timing = prior.map(|p| super::budget::PriorIndexerTiming {
        last_success_ms: p.last_success_ms,
        last_timed_out_ms: p.last_timed_out_ms,
        ..Default::default()
    });
    let budget =
        super::budget::budget_for_indexer(plan.indexer, &size, prior_timing.as_ref(), None);

    // Preserve the legacy fixed cap as a floor: `max(budget, timeout())`. The
    // budget model may raise the timeout above the fixed cap (large workspaces
    // or timeout headroom) but never lowers it below the shipped cap.
    budget.per_invocation.max(plan.indexer.timeout())
}

/// Classify a completed plan execution into the outcome recorded for the
/// adaptive budget. Cache hits and deadline-skipped plans never invoked the
/// indexer, so they yield no observation (`None`).
fn timing_outcome_for(execution: &PlanExecution) -> Option<super::IndexerRunOutcome> {
    use super::IndexerRunOutcome;
    match execution {
        PlanExecution::CachedHit | PlanExecution::DeadlineExhausted(_) => None,
        PlanExecution::Ran(Ok(output)) if output.status.success() => {
            Some(IndexerRunOutcome::Success)
        }
        PlanExecution::Ran(Ok(_)) => Some(IndexerRunOutcome::Failed),
        PlanExecution::Ran(Err(err)) if err.kind() == std::io::ErrorKind::TimedOut => {
            Some(IndexerRunOutcome::TimedOut)
        }
        PlanExecution::Ran(Err(_)) => Some(IndexerRunOutcome::Failed),
        PlanExecution::Partitioned(summary) => match summary.status.as_str() {
            "timed_out" => Some(IndexerRunOutcome::TimedOut),
            "failed" => Some(IndexerRunOutcome::Failed),
            // "artifact_pending" / "ready" / "ready_with_quarantine": at least
            // one partition produced an index → treat as a successful run.
            _ => Some(IndexerRunOutcome::Success),
        },
    }
}

/// Result of running a single planned indexer command (with optional cache
/// integration). This is the rich per-plan outcome consumed by
/// [`tally_indexer_results`].
///
/// - `CachedHit` — the SCIP cache produced the planned artifact and the
///   indexer was NOT invoked. The artifact is already on disk at the planned
///   output path; tally treats it like a successful run.
/// - `Ran` — the indexer was invoked and produced an `io::Result<Output>`.
#[derive(Debug)]
enum PlanExecution {
    /// Cache hit: the cached artifact was copied to the planned output path
    /// and the indexer was not run.
    CachedHit,
    /// The indexer binary was invoked (cache miss or unavailable).
    Ran(std::io::Result<std::process::Output>),
    /// The indexer was skipped because the active deadline was already
    /// exhausted before invocation could start. `detail` carries the JSON
    /// status detail that will be surfaced in `WorkspaceWarmStatus`.
    DeadlineExhausted(String),
    /// Go/Clang below-workspace partitioned run summary.
    Partitioned(PartitionExecutionSummary),
}

#[derive(Debug)]
struct PartitionExecutionSummary {
    commands: Vec<ExecutedIndexerCommand>,
    status: String,
    detail: String,
    failure_count: usize,
    total_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PartitionUnit {
    scope: String,
    label: String,
    args: Vec<String>,
    output_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct PartitionCacheSummary {
    hits: usize,
    misses: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartitionOutcomeStatus {
    Produced,
    Failed,
    TimedOut,
}

#[derive(Debug)]
struct PartitionOutcome {
    unit: PartitionUnit,
    plan: PlannedIndexerCommand,
    status: PartitionOutcomeStatus,
    cache_hit: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    detail: String,
}

trait GoPackageLister {
    fn list_packages(&self, working_directory: &Path) -> Result<Vec<String>>;
}

struct CommandGoPackageLister;

impl GoPackageLister for CommandGoPackageLister {
    fn list_packages(&self, working_directory: &Path) -> Result<Vec<String>> {
        let output = std::process::Command::new("go")
            .arg("list")
            .arg("./...")
            .current_dir(working_directory)
            .output()
            .with_context(|| format!("run go list under {}", working_directory.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!("go list failed: {stderr}");
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }
}

/// Execute a single planned indexer command, performing a SCIP cache lookup
/// before invoking the indexer and a cache write after a successful
/// non-empty artifact is produced.
///
/// Cache failures are non-fatal misses: if looking up the key fails or the
/// cache store is unavailable, the indexer runs as if there were no cache.
/// A cache hit skips the indexer entirely and copies the cached artifact to
/// the planned output path so `collect_scip_artifacts` still finds it.
async fn execute_plan_with_cache(
    plan: PlannedIndexerCommand,
    timeout: std::time::Duration,
    prior: Option<&super::IndexerPriorTiming>,
) -> PlanExecution {
    if matches!(plan.indexer, SupportedIndexer::Go | SupportedIndexer::Clang) {
        return execute_partitioned_plan(plan, prior).await;
    }
    // Attempt cache lookup. Any error is a non-fatal miss — we fall through
    // to running the indexer.
    let store = super::cache::ScipCacheStore::from_environment();
    let cache_key = compute_cache_key_for_plan(&plan);

    if let Some(key) = cache_key.as_ref() {
        match store.lookup(key, &plan.output_path) {
            super::cache::CacheLookup::Hit => {
                tracing::info!(
                    indexer = plan.indexer.binary_name(),
                    workspace = %plan.workspace_root.display(),
                    "SCIP cache hit — skipping indexer invocation"
                );
                return PlanExecution::CachedHit;
            }
            super::cache::CacheLookup::Miss => {}
        }
    }

    // No active deadline is plumbed through the production path yet, so the
    // deadline-exhausted branch is only reachable from tests / future
    // integration tasks. When an active deadline is supplied in future, a
    // zero budget should skip invocation and record the deadline reason.
    if timeout == std::time::Duration::ZERO {
        let detail = serde_json::json!({
            "kind": "deadline_exhausted",
            "indexer": plan.indexer.binary_name(),
            "workspace_slug": plan.workspace_slug,
            "reason": "no usable time remaining after reserve"
        })
        .to_string();
        return PlanExecution::DeadlineExhausted(detail);
    }

    // Cache miss (or cache unavailable): invoke the indexer.
    let cmd = plan.build_command();
    let result = process::output_with_timeout(cmd, timeout).await;

    // On success, attempt to cache the produced artifact if it is non-empty.
    // Cache write failures are non-fatal: the warm still proceeds with the
    // freshly produced artifact.
    if let Ok(output) = &result
        && output.status.success()
        && let Ok(metadata) = fs::metadata(&plan.output_path)
        && metadata.len() > 0
        && let Some(key) = cache_key.as_ref()
        && let Err(e) = store.store_artifact(key, &plan.output_path)
    {
        tracing::warn!(
            indexer = plan.indexer.binary_name(),
            workspace = %plan.workspace_root.display(),
            error = %e,
            "SCIP cache store failed (non-fatal — indexer output still used)"
        );
    }

    PlanExecution::Ran(result)
}

async fn execute_partitioned_plan(
    plan: PlannedIndexerCommand,
    prior: Option<&super::IndexerPriorTiming>,
) -> PlanExecution {
    let units = match partition_units_for_plan(&plan, &CommandGoPackageLister) {
        Ok(units) if !units.is_empty() => units,
        Ok(_) | Err(_) => {
            let timeout = budgeted_timeout_for_plan(&plan, prior);
            return execute_workspace_plan_with_cache(plan, timeout).await;
        }
    };

    let mut size = super::budget::estimate_workspace_size(&plan.working_directory, plan.indexer);
    size.partition_count = units.len();
    let budget = super::budget::budget_for_indexer(plan.indexer, &size, None, None);
    if budget.total == Duration::ZERO {
        let detail = serde_json::json!({
            "kind": "deadline_exhausted",
            "indexer": plan.indexer.binary_name(),
            "workspace_slug": plan.workspace_slug,
            "reason": budget.reason,
        })
        .to_string();
        return PlanExecution::DeadlineExhausted(detail);
    }

    let per_partition_timeout = budget
        .per_partition
        .unwrap_or(budget.per_invocation)
        .max(Duration::from_secs(1));
    let mut remaining_budget = budget.total;
    let total_count = units.len();
    let mut outcomes = Vec::with_capacity(total_count);

    for unit in units {
        if remaining_budget == Duration::ZERO {
            let partition_plan = partition_plan(&plan, &unit);
            outcomes.push(PartitionOutcome {
                unit,
                plan: partition_plan,
                status: PartitionOutcomeStatus::TimedOut,
                cache_hit: false,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                detail: "cumulative partition budget exhausted".to_string(),
            });
            continue;
        }
        let timeout = per_partition_timeout.min(remaining_budget);
        let partition_plan = partition_plan(&plan, &unit);
        match execute_workspace_plan_with_cache_with_stats(partition_plan.clone(), timeout).await {
            (PlanExecution::CachedHit, hit) => {
                outcomes.push(PartitionOutcome {
                    unit,
                    plan: partition_plan,
                    status: PartitionOutcomeStatus::Produced,
                    cache_hit: hit,
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                    detail: String::new(),
                });
            }
            (PlanExecution::Ran(Ok(output)), hit) if output.status.success() => {
                if fs::metadata(&partition_plan.output_path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
                {
                    outcomes.push(PartitionOutcome {
                        unit,
                        plan: partition_plan,
                        status: PartitionOutcomeStatus::Produced,
                        cache_hit: hit,
                        exit_code: output.status.code(),
                        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                        detail: String::new(),
                    });
                } else {
                    outcomes.push(PartitionOutcome {
                        unit,
                        plan: partition_plan,
                        status: PartitionOutcomeStatus::Failed,
                        cache_hit: hit,
                        exit_code: output.status.code(),
                        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                        detail: "indexer exited successfully but produced no SCIP artifact"
                            .to_string(),
                    });
                }
            }
            (PlanExecution::Ran(Ok(output)), hit) => {
                outcomes.push(PartitionOutcome {
                    unit,
                    plan: partition_plan,
                    status: PartitionOutcomeStatus::Failed,
                    cache_hit: hit,
                    exit_code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                });
            }
            (PlanExecution::Ran(Err(err)), hit) => {
                let status = if err.kind() == std::io::ErrorKind::TimedOut {
                    PartitionOutcomeStatus::TimedOut
                } else {
                    PartitionOutcomeStatus::Failed
                };
                outcomes.push(PartitionOutcome {
                    unit,
                    plan: partition_plan,
                    status,
                    cache_hit: hit,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    detail: err.to_string(),
                });
            }
            _ => {}
        }
        remaining_budget = remaining_budget.saturating_sub(timeout);
    }

    summarize_partition_outcomes(&plan, outcomes, &budget, remaining_budget, total_count)
}

fn summarize_partition_outcomes(
    plan: &PlannedIndexerCommand,
    outcomes: Vec<PartitionOutcome>,
    budget: &super::budget::IndexerBudget,
    remaining_budget: Duration,
    total_count: usize,
) -> PlanExecution {
    let mut commands = Vec::new();
    let mut quarantined = Vec::new();
    let mut cache = PartitionCacheSummary::default();
    let mut produced = 0usize;
    let mut timed_out = 0usize;
    let mut failed = 0usize;

    for outcome in outcomes {
        if outcome.cache_hit {
            cache.hits += 1;
        } else {
            cache.misses += 1;
        }

        match outcome.status {
            PartitionOutcomeStatus::Produced => {
                produced += 1;
                commands.push(ExecutedIndexerCommand {
                    plan: outcome.plan,
                    exit_code: outcome.exit_code.or(Some(0)),
                    stdout: outcome.stdout,
                    stderr: outcome.stderr,
                });
            }
            PartitionOutcomeStatus::Failed => {
                failed += 1;
                quarantined.push(serde_json::json!({
                    "scope": outcome.unit.scope,
                    "label": outcome.unit.label,
                    "status": "failed",
                    "detail": outcome.detail,
                    "exit_code": outcome.exit_code,
                }));
            }
            PartitionOutcomeStatus::TimedOut => {
                timed_out += 1;
                quarantined.push(serde_json::json!({
                    "scope": outcome.unit.scope,
                    "label": outcome.unit.label,
                    "status": "timed_out",
                    "detail": outcome.detail,
                }));
            }
        }
    }

    let failure_count = failed + timed_out;
    let status = partition_workspace_status(produced, failed, timed_out, total_count);
    let detail = quarantine_detail_json(
        plan,
        &quarantined,
        produced,
        &cache,
        budget,
        remaining_budget,
        total_count,
    );
    PlanExecution::Partitioned(PartitionExecutionSummary {
        commands,
        status,
        detail,
        failure_count,
        total_count,
    })
}

async fn execute_workspace_plan_with_cache(
    plan: PlannedIndexerCommand,
    timeout: Duration,
) -> PlanExecution {
    execute_workspace_plan_with_cache_with_stats(plan, timeout)
        .await
        .0
}

async fn execute_workspace_plan_with_cache_with_stats(
    plan: PlannedIndexerCommand,
    timeout: Duration,
) -> (PlanExecution, bool) {
    let store = super::cache::ScipCacheStore::from_environment();
    let cache_key = compute_cache_key_for_plan(&plan);
    if let Some(key) = cache_key.as_ref()
        && store.lookup(key, &plan.output_path) == super::cache::CacheLookup::Hit
    {
        return (PlanExecution::CachedHit, true);
    }
    if timeout == Duration::ZERO {
        let detail = serde_json::json!({
            "kind": "deadline_exhausted",
            "indexer": plan.indexer.binary_name(),
            "workspace_slug": plan.workspace_slug,
            "reason": "no usable time remaining after reserve"
        })
        .to_string();
        return (PlanExecution::DeadlineExhausted(detail), false);
    }
    let cmd = plan.build_command();
    let result = process::output_with_timeout(cmd, timeout).await;
    if let Ok(output) = &result
        && output.status.success()
        && let Ok(metadata) = fs::metadata(&plan.output_path)
        && metadata.len() > 0
        && let Some(key) = cache_key.as_ref()
    {
        let _ = store.store_artifact(key, &plan.output_path);
    }
    (PlanExecution::Ran(result), false)
}

fn partition_workspace_status(
    produced: usize,
    failed: usize,
    timed_out: usize,
    total: usize,
) -> String {
    (if produced > 0 && failed + timed_out > 0 {
        "ready_with_quarantine"
    } else if produced > 0 {
        "artifact_pending"
    } else if timed_out > 0 && timed_out >= failed && failed + timed_out == total {
        "timed_out"
    } else {
        "failed"
    })
    .to_string()
}

fn quarantine_detail_json(
    plan: &PlannedIndexerCommand,
    quarantined: &[serde_json::Value],
    produced: usize,
    cache: &PartitionCacheSummary,
    budget: &super::budget::IndexerBudget,
    remaining_budget: Duration,
    total_count: usize,
) -> String {
    serde_json::json!({
        "kind": "quarantine_v1",
        "scope": match plan.indexer { SupportedIndexer::Go => "go_package", SupportedIndexer::Clang => "clang_translation_unit", _ => "workspace" },
        "workspace_slug": plan.workspace_slug,
        "indexer": plan.indexer.binary_name(),
        "quarantined_units": quarantined,
        "produced_artifact_count": produced,
        "partition_count": total_count,
        "cache": { "hits": cache.hits, "misses": cache.misses },
        "budget": {
            "total_ms": budget.total.as_millis(),
            "per_partition_ms": budget.per_partition.unwrap_or(budget.per_invocation).as_millis(),
            "remaining_ms": remaining_budget.as_millis(),
            "reason": budget.reason,
        }
    }).to_string()
}

fn partition_plan(plan: &PlannedIndexerCommand, unit: &PartitionUnit) -> PlannedIndexerCommand {
    let mut cloned = plan.clone();
    cloned.args = unit.args.clone();
    cloned.output_path = unit.output_path.clone();
    cloned
}

fn partition_units_for_plan(
    plan: &PlannedIndexerCommand,
    go_lister: &dyn GoPackageLister,
) -> Result<Vec<PartitionUnit>> {
    match plan.indexer {
        SupportedIndexer::Go => go_partition_units(plan, go_lister),
        SupportedIndexer::Clang => clang_partition_units(plan),
        _ => Ok(Vec::new()),
    }
}

fn go_partition_units(
    plan: &PlannedIndexerCommand,
    go_lister: &dyn GoPackageLister,
) -> Result<Vec<PartitionUnit>> {
    let packages = go_lister.list_packages(&plan.working_directory)?;
    Ok(packages
        .into_iter()
        .map(|package| {
            let output_path = partition_output_path(plan, &package);
            PartitionUnit {
                scope: "go_package".to_string(),
                label: package.clone(),
                args: vec![
                    "index".to_string(),
                    "-o".to_string(),
                    output_path.to_string_lossy().into_owned(),
                    package,
                ],
                output_path,
            }
        })
        .collect())
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct CompileCommand {
    directory: String,
    command: Option<String>,
    arguments: Option<Vec<String>>,
    file: String,
}

fn clang_partition_units(plan: &PlannedIndexerCommand) -> Result<Vec<PartitionUnit>> {
    let compdb = plan.working_directory.join("compile_commands.json");
    let bytes = fs::read(&compdb).with_context(|| format!("read {}", compdb.display()))?;
    let commands: Vec<CompileCommand> =
        serde_json::from_slice(&bytes).context("parse compile_commands.json")?;
    let temp_dir = plan
        .output_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("clang-partitions")
        .join(&plan.workspace_slug);
    fs::create_dir_all(&temp_dir).with_context(|| format!("create {}", temp_dir.display()))?;
    let mut units = Vec::new();
    for command in commands {
        let label = command.file.clone();
        let output_path = partition_output_path(plan, &label);
        let compdb_path = temp_dir.join(format!(
            "{}.compile_commands.json",
            sanitize_partition_label(&label)
        ));
        fs::write(
            &compdb_path,
            serde_json::to_vec_pretty(&vec![command.clone()])?,
        )?;
        units.push(PartitionUnit {
            scope: "clang_translation_unit".to_string(),
            label,
            args: vec![
                "--compdb-path".to_string(),
                compdb_path.to_string_lossy().into_owned(),
                "--index-output-path".to_string(),
                output_path.to_string_lossy().into_owned(),
            ],
            output_path,
        });
    }
    Ok(units)
}

fn partition_output_path(plan: &PlannedIndexerCommand, label: &str) -> PathBuf {
    let stem = plan
        .output_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("index");
    let parent = plan.output_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}-{}.scip", sanitize_partition_label(label)))
}

fn sanitize_partition_label(label: &str) -> String {
    let mut out: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-')
        .chars()
        .take(80)
        .collect::<String>()
        .if_empty_then("unit")
}

trait EmptyDefault {
    fn if_empty_then(self, default: &str) -> String;
}
impl EmptyDefault for String {
    fn if_empty_then(self, default: &str) -> String {
        if self.is_empty() {
            default.to_string()
        } else {
            self
        }
    }
}

/// Compute a SCIP cache key for a planned indexer command.
///
/// Collects source/config/lockfile hashes for the workspace and combines them
/// with the indexer's reported tool version and the relevant environment.
/// Returns `None` if the tool version cannot be determined or the key cannot
/// be computed — callers treat `None` as "no cache" and invoke the indexer.
fn compute_cache_key_for_plan(plan: &PlannedIndexerCommand) -> Option<super::cache::ScipCacheKey> {
    let reported_version = detect_tool_version(plan.indexer, &plan.binary_path)?;
    let (source_hashes, config_hashes, lockfile_hashes) =
        collect_workspace_hashes(&plan.workspace_root, plan.indexer);
    let environment = super::cache::relevant_environment(plan.indexer);
    let ingredients = super::cache::CacheKeyIngredients::from_plan(
        plan,
        reported_version,
        source_hashes,
        config_hashes,
        lockfile_hashes,
        environment,
    );
    match ingredients.cache_key() {
        Ok(key) => Some(key),
        Err(e) => {
            tracing::warn!(
                indexer = plan.indexer.binary_name(),
                workspace = %plan.workspace_root.display(),
                error = %e,
                "SCIP cache key computation failed (non-fatal)"
            );
            None
        }
    }
}

/// Detect the reported version string for an indexer binary.
///
/// Tries `<binary> --version` first, then `<binary> version` as a fallback.
/// Returns `None` if neither produces a usable version string. Failures here
/// are non-fatal: the caller simply skips caching for that plan.
fn detect_tool_version(indexer: SupportedIndexer, binary_path: &Path) -> Option<String> {
    let try_args = &[&["--version"][..], &["version"][..]];
    for args in try_args {
        let output = std::process::Command::new(binary_path)
            .args(*args)
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !version.is_empty() {
            return Some(version);
        }
    }
    let _ = indexer; // indexer reserved for future version-override logic
    None
}

/// Collect source, config, and lockfile content hashes for a workspace root.
///
/// Source files are hashed by walking the workspace root with the indexer's
/// source extensions. Config and lockfiles are the known marker/lock files
/// for the indexer that, if present, are hashed and included in the cache key.
fn collect_workspace_hashes(
    workspace_root: &Path,
    indexer: SupportedIndexer,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let source_extensions = super::budget::source_extensions(indexer);
    let mut source_files: Vec<PathBuf> = Vec::new();
    let _ = visit_dirs(workspace_root, &mut |path| {
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && source_extensions.contains(&ext)
        {
            source_files.push(path.to_path_buf());
        }
        Ok(())
    });

    let source_hashes = super::cache::hash_existing_files(
        workspace_root,
        source_files
            .iter()
            .map(|p| p.strip_prefix(workspace_root).unwrap_or(p).to_path_buf()),
    );

    let config_names = config_files_for_indexer(indexer);
    let lockfile_names = lockfile_files_for_indexer(indexer);

    let config_hashes =
        super::cache::hash_existing_files(workspace_root, config_names.iter().map(PathBuf::from));
    let lockfile_hashes =
        super::cache::hash_existing_files(workspace_root, lockfile_names.iter().map(PathBuf::from));

    (source_hashes, config_hashes, lockfile_hashes)
}

/// Config / marker files whose contents affect indexer output for a given
/// indexer. These are hashed into the cache key.
fn config_files_for_indexer(indexer: SupportedIndexer) -> &'static [&'static str] {
    match indexer {
        SupportedIndexer::RustAnalyzer => &["Cargo.toml", "rust-toolchain.toml"],
        SupportedIndexer::TypeScript => &["tsconfig.json", "package.json"],
        SupportedIndexer::Python => &["pyproject.toml", "setup.py", "setup.cfg"],
        SupportedIndexer::Go => &["go.mod"],
        SupportedIndexer::Java => &["build.gradle", "pom.xml", "settings.gradle"],
        SupportedIndexer::Clang => &["CMakeLists.txt", "compile_commands.json"],
        SupportedIndexer::Ruby => &["Gemfile"],
        SupportedIndexer::DotNet => &["Directory.Build.props"],
    }
}

/// Lockfiles whose contents affect indexer output for a given indexer.
fn lockfile_files_for_indexer(indexer: SupportedIndexer) -> &'static [&'static str] {
    match indexer {
        SupportedIndexer::RustAnalyzer => &["Cargo.lock"],
        SupportedIndexer::TypeScript => &["package-lock.json", "pnpm-lock.yaml", "yarn.lock"],
        SupportedIndexer::Python => &["poetry.lock", "requirements.txt"],
        SupportedIndexer::Go => &["go.sum"],
        SupportedIndexer::Java => &[],
        SupportedIndexer::Clang => &[],
        SupportedIndexer::Ruby => &["Gemfile.lock"],
        SupportedIndexer::DotNet => &[],
    }
}

pub(crate) async fn run_indexers(
    project_root: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
    language_filter: Option<&[SupportedIndexer]>,
    declared_workspaces: Option<&[djinn_stack::Workspace]>,
    priors: Option<&super::PriorTimingMap>,
) -> Result<IndexingRun> {
    let project_root = project_root.as_ref().to_path_buf();
    let output_root = output_root.as_ref().to_path_buf();
    fs::create_dir_all(&output_root)
        .with_context(|| format!("create SCIP output dir {}", output_root.display()))?;

    // Phase 3 PR 8: filter detected indexers by the caller-supplied language
    // set (derived from `projects.stack`). `None` keeps the legacy
    // "run every known indexer" behaviour.
    let available: Vec<IndexerAvailability> = detect_indexers()
        .into_iter()
        .filter(|a| match language_filter {
            None => true,
            Some(langs) => langs.contains(&a.indexer),
        })
        .collect();

    for availability in &available {
        if availability.is_available() {
            continue;
        }
        let workspaces = discover_workspaces(&project_root, availability.indexer);
        if workspaces.is_empty() {
            continue;
        }
        if note_missing_indexer_once(&project_root, availability.indexer) {
            tracing::info!(
                project_root = %project_root.display(),
                language = availability.indexer.language(),
                indexer = availability.indexer.binary_name(),
                "SCIP indexer binary not found on PATH; skipping language for this project"
            );
        }
    }

    let plans = plan_indexer_commands(&project_root, &output_root, &available, declared_workspaces);
    let futures: Vec<_> = plans
        .into_iter()
        .map(|plan| {
            // Size the timeout from persisted prior timings for this
            // (workspace, indexer). With no history the budget resolves to the
            // size-scaled baseline floored by the legacy `timeout()` cap —
            // byte-identical to the shipped behaviour. With history it adapts:
            // slow-but-passing runs raise within `max_cap`; repeated timeouts
            // grow headroom above it (up to the adaptive ceiling).
            let prior = priors.and_then(|m| m.get(&(plan.workspace_slug.clone(), plan.indexer)));
            let timeout = budgeted_timeout_for_plan(&plan, prior);
            let prior = prior.copied();
            let plan_for_future = plan.clone();
            async move {
                use djinn_core::clock::{Clock, SystemClock};
                let started = SystemClock::new().now_instant();
                let outcome =
                    execute_plan_with_cache(plan_for_future, timeout, prior.as_ref()).await;
                let elapsed_ms = started.elapsed().as_millis() as u64;
                (plan, outcome, elapsed_ms)
            }
        })
        .collect();

    let raw_results = futures::future::join_all(futures).await;

    // Split the timing observations off before tally consumes the executions.
    // Only actual invocations (not cache hits / deadline skips) yield a row.
    let mut timings: Vec<super::IndexerTimingObservation> = Vec::with_capacity(raw_results.len());
    let mut results = Vec::with_capacity(raw_results.len());
    for (plan, outcome, elapsed_ms) in raw_results {
        if let Some(run_outcome) = timing_outcome_for(&outcome) {
            timings.push(super::IndexerTimingObservation {
                workspace_slug: plan.workspace_slug.clone(),
                indexer: plan.indexer,
                outcome: run_outcome,
                elapsed_ms,
            });
        }
        results.push((plan, outcome));
    }

    let mut tally = tally_indexer_results(results)?;

    let artifacts = collect_scip_artifacts(&output_root, &tally.commands)?;
    apply_artifact_statuses(&artifacts, &mut tally.workspace_statuses);
    if tally.all_failed {
        return Err(anyhow!(
            "all {} SCIP indexers failed (no index produced)",
            tally.workspace_statuses.len()
        ));
    }

    Ok(IndexingRun {
        project_root,
        output_root,
        commands: tally.commands,
        artifacts,
        workspace_statuses: tally.workspace_statuses,
        timings,
    })
}

struct IndexerTally {
    commands: Vec<ExecutedIndexerCommand>,
    workspace_statuses: Vec<WorkspaceWarmStatus>,
    all_failed: bool,
}

/// Aggregate the per-target indexer results into the set of successful
/// [`ExecutedIndexerCommand`]s, applying the partial-success policy.
///
/// **Policy:** the warm succeeds as long as ≥1 indexer target produced an
/// index. Monorepos routinely contain workspaces that can't be indexed (no
/// usable `tsconfig`, generated package, …); failing the whole warm on those
/// would leave the project stuck "Warming" forever and burn compute re-trying
/// every few minutes. Only a *total* wipe-out — every planned target failed —
/// is treated as a hard error. Per-target failures are `warn`-logged (no
/// silent truncation) and a summary line records the succeeded/failed split.
///
/// An empty input (`total == 0`, e.g. a code-less repo) is `Ok` with no
/// commands — there was nothing to index, which is not a failure.
fn tally_indexer_results(
    results: Vec<(PlannedIndexerCommand, PlanExecution)>,
) -> Result<IndexerTally> {
    let total = results.len();
    let mut commands = Vec::with_capacity(total);
    let mut workspace_statuses = Vec::with_capacity(total);
    let mut failure_count = 0usize;

    for (plan, execution) in results {
        match execution {
            PlanExecution::Partitioned(summary) => {
                failure_count += summary.failure_count;
                let has_success = !summary.commands.is_empty();
                commands.extend(summary.commands);
                workspace_statuses.push(WorkspaceWarmStatus {
                    workspace_slug: plan.workspace_slug.clone(),
                    indexer: plan.indexer,
                    status: summary.status,
                    detail: Some(summary.detail),
                    workspace_rel_root: plan.workspace_rel_root.to_string_lossy().into_owned(),
                });
                if !has_success && summary.failure_count == 0 {
                    failure_count += summary.total_count;
                }
            }
            PlanExecution::CachedHit => {
                // Cache hit: the cached artifact is already on disk at the
                // planned output path. Treat it like a successful invocation
                // (exit 0, empty stdout/stderr) so `collect_scip_artifacts`
                // and `apply_artifact_statuses` proceed unchanged.
                let detail = serde_json::json!({
                    "kind": "cache_hit",
                    "indexer": plan.indexer.binary_name(),
                    "workspace_slug": plan.workspace_slug.clone(),
                    "output": plan.output_path.display().to_string(),
                })
                .to_string();
                workspace_statuses.push(WorkspaceWarmStatus {
                    workspace_slug: plan.workspace_slug.clone(),
                    indexer: plan.indexer,
                    status: "artifact_pending".to_string(),
                    detail: Some(detail),
                    workspace_rel_root: plan.workspace_rel_root.to_string_lossy().into_owned(),
                });
                commands.push(ExecutedIndexerCommand {
                    plan,
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            PlanExecution::DeadlineExhausted(detail) => {
                // The active deadline was exhausted before this invocation
                // could start. Record a `timed_out` status with a JSON detail
                // so operators can see the deadline/budget reason.
                failure_count += 1;
                workspace_statuses.push(WorkspaceWarmStatus {
                    workspace_slug: plan.workspace_slug.clone(),
                    indexer: plan.indexer,
                    status: "timed_out".to_string(),
                    detail: Some(detail),
                    workspace_rel_root: plan.workspace_rel_root.to_string_lossy().into_owned(),
                });
                tracing::warn!(
                    indexer = plan.indexer.binary_name(),
                    workspace = %plan.workspace_root.display(),
                    "SCIP indexer skipped — active deadline exhausted"
                );
            }
            PlanExecution::Ran(result) => match result {
                Ok(output) if output.status.success() => {
                    workspace_statuses.push(WorkspaceWarmStatus {
                        workspace_slug: plan.workspace_slug.clone(),
                        indexer: plan.indexer,
                        status: "artifact_pending".to_string(),
                        detail: Some(format!("expected artifact {}", plan.output_path.display())),
                        workspace_rel_root: plan.workspace_rel_root.to_string_lossy().into_owned(),
                    });
                    commands.push(ExecutedIndexerCommand {
                        plan,
                        exit_code: output.status.code(),
                        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    });
                }
                Ok(output) => {
                    failure_count += 1;
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    workspace_statuses.push(WorkspaceWarmStatus {
                        workspace_slug: plan.workspace_slug.clone(),
                        indexer: plan.indexer,
                        status: "failed".to_string(),
                        detail: Some(if stderr.is_empty() {
                            format!("indexer exited with status {:?}", output.status.code())
                        } else {
                            stderr.clone()
                        }),
                        workspace_rel_root: plan.workspace_rel_root.to_string_lossy().into_owned(),
                    });
                    tracing::warn!(
                        indexer = plan.indexer.binary_name(),
                        workspace = %plan.workspace_root.display(),
                        exit_code = ?output.status.code(),
                        stderr = %String::from_utf8_lossy(&output.stderr),
                        "SCIP indexer failed"
                    );
                }
                Err(err) => {
                    failure_count += 1;
                    let status = if err.kind() == std::io::ErrorKind::TimedOut {
                        "timed_out"
                    } else {
                        "failed"
                    };
                    workspace_statuses.push(WorkspaceWarmStatus {
                        workspace_slug: plan.workspace_slug.clone(),
                        indexer: plan.indexer,
                        status: status.to_string(),
                        detail: Some(err.to_string()),
                        workspace_rel_root: plan.workspace_rel_root.to_string_lossy().into_owned(),
                    });
                    tracing::warn!(
                        indexer = plan.indexer.binary_name(),
                        workspace = %plan.workspace_root.display(),
                        error = %err,
                        "SCIP indexer error"
                    );
                }
            },
        }
    }

    if total > 0 {
        tracing::info!(
            total,
            succeeded = commands.len(),
            failed = failure_count,
            "SCIP indexer run complete"
        );
    }

    let all_failed = total > 0 && commands.is_empty();
    Ok(IndexerTally {
        commands,
        workspace_statuses,
        all_failed,
    })
}

fn apply_artifact_statuses(artifacts: &[ScipArtifact], statuses: &mut [WorkspaceWarmStatus]) {
    let produced: std::collections::HashSet<(String, SupportedIndexer)> = artifacts
        .iter()
        .filter_map(|artifact| {
            artifact
                .indexer
                .map(|indexer| (artifact.workspace_slug.clone(), indexer))
        })
        .collect();

    for status in statuses {
        if status.status != "artifact_pending" {
            continue;
        }
        if produced.contains(&(status.workspace_slug.clone(), status.indexer)) {
            status.status = "ready".to_string();
            status.detail = None;
        } else {
            status.status = "failed".to_string();
            status.detail =
                Some("indexer exited successfully but produced no SCIP artifact".to_string());
        }
    }
}

pub(crate) fn collect_scip_artifacts(
    output_root: impl AsRef<Path>,
    commands: &[ExecutedIndexerCommand],
) -> Result<Vec<ScipArtifact>> {
    let output_root = output_root.as_ref();
    let mut seen = std::collections::HashSet::new();
    let mut artifacts = Vec::new();

    let expected_paths: Vec<(PathBuf, SupportedIndexer, String, PathBuf)> = commands
        .iter()
        .map(|command| {
            (
                command.plan.output_path.clone(),
                command.plan.indexer,
                command.plan.workspace_slug.clone(),
                command.plan.workspace_rel_root.clone(),
            )
        })
        .collect();

    for path in discover_scip_files(output_root)? {
        if seen.insert(path.clone()) {
            let matched = expected_paths
                .iter()
                .find(|(expected, _, _, _)| expected == &path);
            let indexer = matched.map(|(_, indexer, _, _)| *indexer);
            let workspace_slug = matched
                .map(|(_, _, workspace_slug, _)| workspace_slug.clone())
                .unwrap_or_else(|| "root".to_string());
            let workspace_root = matched
                .map(|(_, _, _, rel_root)| rel_root.clone())
                .unwrap_or_default();
            artifacts.push(ScipArtifact {
                path,
                indexer,
                workspace_slug,
                workspace_root,
            });
        }
    }

    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(artifacts)
}

fn discover_scip_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut artifacts = Vec::new();
    visit_dirs(root, &mut |path| {
        if path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|ext| ext == SCIP_ARTIFACT_EXTENSION)
        {
            artifacts.push(path.to_path_buf());
        }
        Ok(())
    })?;
    Ok(artifacts)
}

fn which_in_path(binary: &str, path_var: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(binary);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }

        let nested_candidate = dir.join("bin").join(binary);
        if is_executable_file(&nested_candidate) {
            return Some(nested_candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => is_executable(metadata),
        _ => false,
    }
}

#[cfg(unix)]
fn is_executable(metadata: fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::test_helpers::workspace_tempdir;

    fn tempdir_in_tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("djinn-repo-map-")
            .tempdir_in(".")
            .expect("create test tempdir")
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("set permissions");
    }

    #[test]
    fn detect_indexers_reports_supported_binaries() {
        let tmp = tempdir_in_tmp();
        for indexer in SupportedIndexer::ALL {
            let path = tmp.path().join(indexer.binary_name());
            fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write binary");
            #[cfg(unix)]
            make_executable(&path);
        }

        let detections = detect_indexers_in_path(tmp.path().display().to_string());

        assert_eq!(detections.len(), SupportedIndexer::ALL.len());
        for detection in detections {
            assert!(detection.is_available(), "{detection:?}");
            assert_eq!(detection.path, Some(tmp.path().join(detection.binary)));
        }
    }

    #[test]
    fn plan_indexer_commands_only_includes_available_indexers() {
        let project_root = PathBuf::from("/tmp/example-project");
        let output_root = PathBuf::from("/tmp/example-project/.djinn/scip");
        let available = vec![
            IndexerAvailability {
                indexer: SupportedIndexer::RustAnalyzer,
                binary: "rust-analyzer".to_string(),
                path: Some(PathBuf::from("/tooling/rust-analyzer")),
            },
            IndexerAvailability {
                indexer: SupportedIndexer::Python,
                binary: "scip-python".to_string(),
                path: None,
            },
            IndexerAvailability {
                indexer: SupportedIndexer::TypeScript,
                binary: "scip-typescript".to_string(),
                path: Some(PathBuf::from("/tooling/scip-typescript")),
            },
        ];

        let plans = plan_indexer_commands(&project_root, &output_root, &available, None);

        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].indexer, SupportedIndexer::RustAnalyzer);
        assert_eq!(
            plans[0].working_directory,
            PathBuf::from("/tmp/example-project")
        );
        assert_eq!(
            plans[0].workspace_root,
            PathBuf::from("/tmp/example-project")
        );
        assert_eq!(plans[0].workspace_slug, "root");
        assert_eq!(
            plans[0].args,
            vec![
                "scip",
                ".",
                "--output",
                "/tmp/example-project/.djinn/scip/example-project-rust-root.scip"
            ]
        );
        assert_eq!(plans[1].indexer, SupportedIndexer::TypeScript);
        assert_eq!(plans[1].workspace_slug, "root");
        assert_eq!(
            plans[1].args,
            vec![
                "index",
                "--output",
                "/tmp/example-project/.djinn/scip/example-project-typescript-root.scip"
            ]
        );
    }

    #[test]
    fn monorepo_command_planning_emits_per_workspace_outputs() {
        let tmp = tempdir_in_tmp();
        let project_root = tmp.path().join("djinn");
        let output_root = project_root.join(".djinn/scip");
        fs::create_dir_all(project_root.join("server")).expect("create server dir");
        fs::create_dir_all(project_root.join("desktop")).expect("create desktop dir");
        fs::create_dir_all(project_root.join("website")).expect("create website dir");
        fs::write(
            project_root.join("server/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("write rust workspace");
        fs::write(project_root.join("desktop/tsconfig.json"), "{}\n")
            .expect("write desktop tsconfig");
        fs::write(
            project_root.join("website/package.json"),
            "{\"private\": true, \"workspaces\": [\"apps/*\"]}\n",
        )
        .expect("write website package.json");
        // scip-typescript needs a tsconfig in the working dir, so a workspace
        // root must carry one to be a target.
        fs::write(project_root.join("website/tsconfig.json"), "{}\n")
            .expect("write website tsconfig");

        let available = vec![
            IndexerAvailability {
                indexer: SupportedIndexer::RustAnalyzer,
                binary: "rust-analyzer".to_string(),
                path: Some(PathBuf::from("/tooling/rust-analyzer")),
            },
            IndexerAvailability {
                indexer: SupportedIndexer::TypeScript,
                binary: "scip-typescript".to_string(),
                path: Some(PathBuf::from("/tooling/scip-typescript")),
            },
        ];

        let plans = plan_indexer_commands(&project_root, &output_root, &available, None);
        assert_eq!(plans.len(), 3);
        assert_eq!(plans[0].working_directory, project_root.join("server"));
        assert_eq!(plans[0].workspace_root, project_root.join("server"));
        assert_eq!(plans[0].workspace_slug, "server");
        assert_eq!(
            plans[0].output_path,
            output_root.join(format!(
                "djinn-rust-{}.scip",
                crate::scip_indexer::workspaces::workspace_slug(Path::new("server"))
            ))
        );
        assert_eq!(
            plans[1..]
                .iter()
                .map(|plan| plan
                    .working_directory
                    .strip_prefix(&project_root)
                    .unwrap()
                    .to_path_buf())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("desktop"), PathBuf::from("website")]
        );
        assert_eq!(
            plans[1..]
                .iter()
                .map(|plan| plan.workspace_slug.as_str())
                .collect::<Vec<_>>(),
            vec!["desktop", "website"]
        );
        let ts_output_names = plans[1..]
            .iter()
            .map(|plan| {
                plan.output_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ts_output_names,
            vec![
                format!(
                    "djinn-typescript-{}.scip",
                    crate::scip_indexer::workspaces::workspace_slug(Path::new("desktop"))
                ),
                format!(
                    "djinn-typescript-{}.scip",
                    crate::scip_indexer::workspaces::workspace_slug(Path::new("website"))
                )
            ]
        );
    }

    #[test]
    fn command_planning_falls_back_to_project_root_when_no_workspace_detected() {
        let project_root = PathBuf::from("/workspace/repo");
        let output_root = PathBuf::from("/workspace/repo/.djinn/scip");
        let available = vec![IndexerAvailability {
            indexer: SupportedIndexer::Python,
            binary: "scip-python".to_string(),
            path: Some(PathBuf::from("/tooling/scip-python")),
        }];

        let plans = plan_indexer_commands(&project_root, &output_root, &available, None);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].working_directory, project_root);
        assert_eq!(plans[0].workspace_root, PathBuf::from("/workspace/repo"));
        assert_eq!(plans[0].workspace_slug, "root");
        assert_eq!(
            plans[0].args,
            vec![
                "index",
                "--output",
                "/workspace/repo/.djinn/scip/repo-python-root.scip"
            ]
        );
    }

    #[test]
    fn collect_scip_artifacts_tags_multiple_planned_outputs_per_indexer() {
        let tmp = tempdir_in_tmp();
        let output_root = tmp.path().join("out");
        fs::create_dir_all(&output_root).expect("create output dirs");

        let planned_rust = PlannedIndexerCommand {
            indexer: SupportedIndexer::RustAnalyzer,
            binary_path: PathBuf::from("/tooling/rust-analyzer"),
            args: vec![
                "scip".to_string(),
                output_root
                    .join("repo-rust-server.scip")
                    .display()
                    .to_string(),
            ],
            working_directory: PathBuf::from("/tmp/project/server"),
            workspace_root: PathBuf::from("/tmp/project/server"),
            workspace_rel_root: PathBuf::from("server"),
            workspace_slug: "server".to_string(),
            output_path: output_root.join("repo-rust-server.scip"),
        };
        let planned_ts = PlannedIndexerCommand {
            indexer: SupportedIndexer::TypeScript,
            binary_path: PathBuf::from("/tooling/scip-typescript"),
            args: vec![
                "index".to_string(),
                output_root
                    .join("repo-typescript-desktop.scip")
                    .display()
                    .to_string(),
            ],
            working_directory: PathBuf::from("/tmp/project/desktop"),
            workspace_root: PathBuf::from("/tmp/project/desktop"),
            workspace_rel_root: PathBuf::from("desktop"),
            workspace_slug: "desktop".to_string(),
            output_path: output_root.join("repo-typescript-desktop.scip"),
        };
        fs::write(&planned_rust.output_path, b"rust-index").expect("write rust output");
        fs::write(&planned_ts.output_path, b"ts-index").expect("write ts output");

        let artifacts = collect_scip_artifacts(
            &output_root,
            &[
                ExecutedIndexerCommand {
                    plan: planned_rust,
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                },
                ExecutedIndexerCommand {
                    plan: planned_ts,
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ],
        )
        .expect("collect artifacts");

        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].indexer, Some(SupportedIndexer::RustAnalyzer));
        assert_eq!(artifacts[0].workspace_slug, "server");
        assert_eq!(artifacts[1].indexer, Some(SupportedIndexer::TypeScript));
        assert_eq!(artifacts[1].workspace_slug, "desktop");
    }

    #[test]
    fn artifact_statuses_surface_success_and_missing_output_per_workspace() {
        let artifacts = vec![ScipArtifact {
            path: PathBuf::from("/out/repo-typescript-web.scip"),
            indexer: Some(SupportedIndexer::TypeScript),
            workspace_slug: "web".to_string(),
            workspace_root: PathBuf::from("web"),
        }];
        let mut statuses = vec![
            WorkspaceWarmStatus {
                workspace_slug: "api".to_string(),
                indexer: SupportedIndexer::TypeScript,
                status: "failed".to_string(),
                detail: Some("crashed".to_string()),
                workspace_rel_root: String::new(),
            },
            WorkspaceWarmStatus {
                workspace_slug: "web".to_string(),
                indexer: SupportedIndexer::TypeScript,
                status: "artifact_pending".to_string(),
                detail: Some("expected artifact".to_string()),
                workspace_rel_root: String::new(),
            },
            WorkspaceWarmStatus {
                workspace_slug: "worker".to_string(),
                indexer: SupportedIndexer::TypeScript,
                status: "artifact_pending".to_string(),
                detail: Some("expected artifact".to_string()),
                workspace_rel_root: String::new(),
            },
        ];

        apply_artifact_statuses(&artifacts, &mut statuses);

        assert_eq!(statuses[0].status, "failed");
        assert_eq!(statuses[0].detail.as_deref(), Some("crashed"));
        assert_eq!(statuses[1].status, "ready");
        assert!(statuses[1].detail.is_none());
        assert_eq!(statuses[2].status, "failed");
        assert_eq!(
            statuses[2].detail.as_deref(),
            Some("indexer exited successfully but produced no SCIP artifact")
        );
    }

    #[test]
    fn collect_scip_artifacts_finds_nested_files_and_tags_known_outputs() {
        let tmp = tempdir_in_tmp();
        let output_root = tmp.path().join("out");
        fs::create_dir_all(output_root.join("nested")).expect("create output dirs");

        let planned = PlannedIndexerCommand {
            indexer: SupportedIndexer::Go,
            binary_path: PathBuf::from("/tooling/scip-go"),
            args: vec![
                "index".to_string(),
                output_root.join("example-go.scip").display().to_string(),
            ],
            working_directory: PathBuf::from("/tmp/project"),
            workspace_root: PathBuf::from("/tmp/project"),
            workspace_rel_root: PathBuf::new(),
            workspace_slug: "root".to_string(),
            output_path: output_root.join("example-go.scip"),
        };
        fs::write(&planned.output_path, b"go-index").expect("write planned output");
        let nested = output_root.join("nested").join("manual.scip");
        fs::write(&nested, b"nested").expect("write nested output");

        let artifacts = collect_scip_artifacts(
            &output_root,
            &[ExecutedIndexerCommand {
                plan: planned,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            }],
        )
        .expect("collect artifacts");

        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].indexer, Some(SupportedIndexer::Go));
        assert_eq!(artifacts[0].workspace_slug, "root");
        assert_eq!(artifacts[1].indexer, None);
        assert_eq!(artifacts[1].workspace_slug, "root");
    }

    #[test]
    fn command_planning_covers_all_supported_indexers() {
        let project_root = PathBuf::from("/workspace/repo");
        let output_root = PathBuf::from("/workspace/repo/.djinn/scip");

        let available: Vec<_> = SupportedIndexer::ALL
            .into_iter()
            .enumerate()
            .map(|(idx, indexer)| IndexerAvailability {
                indexer,
                binary: indexer.binary_name().to_string(),
                path: Some(PathBuf::from(format!(
                    "/tool/{idx}/{}",
                    indexer.binary_name()
                ))),
            })
            .collect();

        let plans = plan_indexer_commands(&project_root, &output_root, &available, None);
        assert_eq!(plans.len(), SupportedIndexer::ALL.len());
        assert_eq!(
            plans.iter().map(|plan| plan.indexer).collect::<Vec<_>>(),
            SupportedIndexer::ALL
        );
        assert_eq!(
            plans[0].args,
            vec![
                "scip",
                ".",
                "--output",
                "/workspace/repo/.djinn/scip/repo-rust-root.scip"
            ]
        );
        assert_eq!(
            plans[1].args,
            vec![
                "index",
                "--output",
                "/workspace/repo/.djinn/scip/repo-typescript-root.scip"
            ]
        );
        assert_eq!(
            plans[2].args,
            vec![
                "index",
                "--output",
                "/workspace/repo/.djinn/scip/repo-python-root.scip"
            ]
        );
        assert_eq!(
            plans[3].args,
            vec![
                "index",
                "-o",
                "/workspace/repo/.djinn/scip/repo-go-root.scip"
            ]
        );
        assert_eq!(
            plans[4].args,
            vec![
                "index",
                "--output",
                "/workspace/repo/.djinn/scip/repo-java-root.scip"
            ]
        );
    }

    #[test]
    fn collect_scip_artifacts_ignores_missing_root() {
        let missing = PathBuf::from("/tmp/does-not-exist-djinn-scip");
        let artifacts = collect_scip_artifacts(&missing, &[]).expect("collect artifacts");
        assert!(artifacts.is_empty());
    }

    // These tests touch the process-global `CARGO_TARGET_DIR` env var and
    // therefore must serialise against each other. In production the
    // server-wide `IndexerLock` provides this guarantee; in tests we use a
    // local `Mutex` so the tests are deterministic regardless of how cargo
    // schedules them.
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn cargo_target_dir_guard_round_trip_restores_previous() {
        let _serial = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by ENV_TEST_LOCK above.
        unsafe { std::env::set_var("CARGO_TARGET_DIR", "/tmp/sentinel-original") };

        let new_dir = std::path::PathBuf::from("/tmp/sentinel-guarded");
        {
            let _g = CargoTargetDirGuard::new(&new_dir);
            assert_eq!(
                std::env::var_os("CARGO_TARGET_DIR"),
                Some(new_dir.clone().into_os_string())
            );
        }
        assert_eq!(
            std::env::var_os("CARGO_TARGET_DIR"),
            Some(std::ffi::OsString::from("/tmp/sentinel-original"))
        );
        // SAFETY: serialised by ENV_TEST_LOCK above.
        unsafe { std::env::remove_var("CARGO_TARGET_DIR") };
    }

    #[test]
    fn cargo_target_dir_guard_unsets_when_previous_was_unset() {
        let _serial = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by ENV_TEST_LOCK above.
        unsafe { std::env::remove_var("CARGO_TARGET_DIR") };
        assert!(std::env::var_os("CARGO_TARGET_DIR").is_none());

        {
            let _g = CargoTargetDirGuard::new(Path::new("/tmp/sentinel-temp"));
            assert!(std::env::var_os("CARGO_TARGET_DIR").is_some());
        }
        assert!(std::env::var_os("CARGO_TARGET_DIR").is_none());
    }

    /// `run_indexers_already_locked` must be callable directly by code that
    /// already holds the real server-wide IndexerLock. This is the entrypoint
    /// `mcp_bridge::ensure_canonical_graph` uses after taking that lock, so
    /// it should not require any extra wrapper or test-only locking shim.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_TEST_LOCK serialises env-mutating tests
    async fn run_indexers_already_locked_callable_without_outer_lock() {
        let _serial = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = workspace_tempdir("repo-map-indexing-");
        let project_root = tmp.path().join("empty-project");
        std::fs::create_dir_all(&project_root).unwrap();
        let output_root = tmp.path().join("scip-out");

        let result =
            run_indexers_already_locked(&project_root, &output_root, None, None, None, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(std::env::var_os("CARGO_TARGET_DIR").is_none());
    }

    /// Build a `PlannedIndexerCommand` skeleton for the tally tests — only
    /// the fields the tally reads (`indexer`, `workspace_root`) matter.
    fn fake_plan(indexer: SupportedIndexer, workspace: &str) -> PlannedIndexerCommand {
        PlannedIndexerCommand {
            indexer,
            binary_path: PathBuf::from("/tool/bin"),
            args: vec!["index".to_string()],
            working_directory: PathBuf::from(workspace),
            workspace_root: PathBuf::from(workspace),
            workspace_rel_root: PathBuf::from(workspace),
            workspace_slug: workspace.replace('/', "-"),
            output_path: PathBuf::from(workspace).join("out.scip"),
        }
    }

    /// Real `std::process::Output` with a genuine `ExitStatus` — built by
    /// running `true`/`false` so the test stays portable (no `ExitStatusExt`).
    /// Wrapped in `PlanExecution::Ran` so tests can call `tally_indexer_results`
    /// with the same shape the production `run_indexers` flow produces.
    fn ran_output(success: bool) -> PlanExecution {
        PlanExecution::Ran(
            std::process::Command::new(if success { "true" } else { "false" }).output(),
        )
    }

    struct FakeGoLister(Vec<String>);

    impl GoPackageLister for FakeGoLister {
        fn list_packages(&self, _working_directory: &Path) -> Result<Vec<String>> {
            Ok(self.0.clone())
        }
    }

    fn partition_test_budget(
        indexer: SupportedIndexer,
        partition_count: usize,
    ) -> super::super::budget::IndexerBudget {
        super::super::budget::budget_for_indexer(
            indexer,
            &super::super::budget::WorkspaceSizeHint {
                source_file_count: partition_count,
                source_bytes: 128,
                partition_count,
            },
            None,
            None,
        )
    }

    fn partition_outcome(
        parent: &PlannedIndexerCommand,
        unit: PartitionUnit,
        status: PartitionOutcomeStatus,
    ) -> PartitionOutcome {
        let plan = partition_plan(parent, &unit);
        PartitionOutcome {
            unit,
            plan,
            status,
            cache_hit: false,
            exit_code: match status {
                PartitionOutcomeStatus::Produced => Some(0),
                PartitionOutcomeStatus::Failed => Some(1),
                PartitionOutcomeStatus::TimedOut => None,
            },
            stdout: String::new(),
            stderr: if status == PartitionOutcomeStatus::Failed {
                "partition failed".to_string()
            } else {
                String::new()
            },
            detail: match status {
                PartitionOutcomeStatus::Produced => String::new(),
                PartitionOutcomeStatus::Failed => "partition failed".to_string(),
                PartitionOutcomeStatus::TimedOut => "partition timed out".to_string(),
            },
        }
    }

    fn partition_summary(
        plan: &PlannedIndexerCommand,
        units: Vec<PartitionUnit>,
        statuses: Vec<PartitionOutcomeStatus>,
    ) -> PartitionExecutionSummary {
        let total_count = units.len();
        let budget = partition_test_budget(plan.indexer, total_count);
        let outcomes = units
            .into_iter()
            .zip(statuses)
            .map(|(unit, status)| partition_outcome(plan, unit, status))
            .collect();
        match summarize_partition_outcomes(
            plan,
            outcomes,
            &budget,
            Duration::from_secs(5),
            total_count,
        ) {
            PlanExecution::Partitioned(summary) => summary,
            other => panic!("expected partition summary, got {other:?}"),
        }
    }

    #[test]
    fn go_package_partitions_are_discovered_through_fakeable_lister() {
        let plan = fake_plan(SupportedIndexer::Go, "repo");
        let units = go_partition_units(
            &plan,
            &FakeGoLister(vec!["./cmd/api".to_string(), "./pkg/lib".to_string()]),
        )
        .expect("go partitions");

        assert_eq!(units.len(), 2);
        assert_eq!(units[0].label, "./cmd/api");
        assert_eq!(
            units[0].args,
            vec![
                "index".to_string(),
                "-o".to_string(),
                PathBuf::from("repo/out-cmd-api.scip")
                    .to_string_lossy()
                    .into_owned(),
                "./cmd/api".to_string()
            ]
        );
    }

    #[test]
    fn clang_partitions_write_filtered_compilation_databases() {
        let tmp = workspace_tempdir("clang-partitions-");
        let workspace = tmp.path().join("repo");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(
            workspace.join("compile_commands.json"),
            serde_json::to_vec(&vec![
                serde_json::json!({"directory": workspace, "command": "cc -c a.cc", "file": "a.cc"}),
                serde_json::json!({"directory": workspace, "arguments": ["cc", "-c", "b.cc"], "file": "src/b.cc"}),
            ])
            .unwrap(),
        )
        .expect("compdb");
        let mut plan = fake_plan(SupportedIndexer::Clang, "repo");
        plan.working_directory = workspace;
        plan.output_path = tmp.path().join("out/repo-cpp-root.scip");

        let units = clang_partition_units(&plan).expect("clang units");
        assert_eq!(units.len(), 2);
        assert!(units[0].args.contains(&"--index-output-path".to_string()));
        let compdb_arg = units[0].args[1].clone();
        let filtered: Vec<CompileCommand> =
            serde_json::from_slice(&fs::read(compdb_arg).expect("filtered compdb")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].file, "a.cc");
    }

    #[test]
    fn go_partition_status_covers_success_quarantine_and_wipeout() {
        assert_eq!(partition_workspace_status(2, 0, 0, 2), "artifact_pending");
        assert_eq!(
            partition_workspace_status(1, 1, 0, 2),
            "ready_with_quarantine"
        );
        assert_eq!(
            partition_workspace_status(1, 0, 1, 2),
            "ready_with_quarantine"
        );
        assert_eq!(partition_workspace_status(0, 2, 0, 2), "failed");
        assert_eq!(partition_workspace_status(0, 0, 2, 2), "timed_out");
    }

    #[test]
    fn go_partition_execution_outcomes_cover_success_quarantine_and_wipeout() {
        let plan = fake_plan(SupportedIndexer::Go, "repo");
        let units = go_partition_units(
            &plan,
            &FakeGoLister(vec!["./cmd/api".to_string(), "./pkg/lib".to_string()]),
        )
        .expect("go partitions");

        let success = partition_summary(
            &plan,
            units.clone(),
            vec![
                PartitionOutcomeStatus::Produced,
                PartitionOutcomeStatus::Produced,
            ],
        );
        assert_eq!(success.status, "artifact_pending");
        assert_eq!(
            success.commands.len(),
            2,
            "both Go packages produced SCIP output"
        );
        assert_eq!(success.failure_count, 0);
        let success_detail: serde_json::Value =
            serde_json::from_str(&success.detail).expect("success detail");
        assert_eq!(success_detail["scope"], "go_package");
        assert_eq!(success_detail["produced_artifact_count"], 2);

        let partial_failure = partition_summary(
            &plan,
            units.clone(),
            vec![
                PartitionOutcomeStatus::Produced,
                PartitionOutcomeStatus::Failed,
            ],
        );
        assert_eq!(partial_failure.status, "ready_with_quarantine");
        assert_eq!(partial_failure.commands.len(), 1);
        let partial_detail: serde_json::Value =
            serde_json::from_str(&partial_failure.detail).expect("partial detail");
        assert_eq!(partial_detail["quarantined_units"][0]["label"], "./pkg/lib");
        assert_eq!(partial_detail["quarantined_units"][0]["status"], "failed");

        let partial_timeout = partition_summary(
            &plan,
            units.clone(),
            vec![
                PartitionOutcomeStatus::Produced,
                PartitionOutcomeStatus::TimedOut,
            ],
        );
        assert_eq!(partial_timeout.status, "ready_with_quarantine");
        let timeout_detail: serde_json::Value =
            serde_json::from_str(&partial_timeout.detail).expect("timeout detail");
        assert_eq!(
            timeout_detail["quarantined_units"][0]["status"],
            "timed_out"
        );

        let failed_wipeout = partition_summary(
            &plan,
            units.clone(),
            vec![
                PartitionOutcomeStatus::Failed,
                PartitionOutcomeStatus::Failed,
            ],
        );
        assert_eq!(failed_wipeout.status, "failed");
        assert_eq!(failed_wipeout.commands.len(), 0);
        assert_eq!(failed_wipeout.failure_count, 2);

        let timeout_wipeout = partition_summary(
            &plan,
            units,
            vec![
                PartitionOutcomeStatus::TimedOut,
                PartitionOutcomeStatus::TimedOut,
            ],
        );
        assert_eq!(timeout_wipeout.status, "timed_out");
        assert_eq!(timeout_wipeout.commands.len(), 0);
        assert_eq!(timeout_wipeout.failure_count, 2);
    }

    #[test]
    fn clang_partition_execution_outcomes_cover_tu_success_quarantine_and_wipeout() {
        let tmp = workspace_tempdir("clang-execution-");
        let workspace = tmp.path().join("repo");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(
            workspace.join("compile_commands.json"),
            serde_json::to_vec(&vec![
                serde_json::json!({"directory": workspace, "command": "cc -c a.cc", "file": "a.cc"}),
                serde_json::json!({"directory": workspace, "command": "cc -c b.cc", "file": "src/b.cc"}),
            ])
            .unwrap(),
        )
        .expect("compdb");
        let mut plan = fake_plan(SupportedIndexer::Clang, "repo");
        plan.working_directory = workspace;
        plan.output_path = tmp.path().join("out/repo-cpp-root.scip");
        let units = clang_partition_units(&plan).expect("clang units");

        let success = partition_summary(
            &plan,
            units.clone(),
            vec![
                PartitionOutcomeStatus::Produced,
                PartitionOutcomeStatus::Produced,
            ],
        );
        assert_eq!(success.status, "artifact_pending");
        assert_eq!(
            success.commands.len(),
            2,
            "both translation units produced SCIP output"
        );
        let success_detail: serde_json::Value =
            serde_json::from_str(&success.detail).expect("success detail");
        assert_eq!(success_detail["scope"], "clang_translation_unit");
        assert_eq!(success_detail["produced_artifact_count"], 2);

        let partial_failure = partition_summary(
            &plan,
            units.clone(),
            vec![
                PartitionOutcomeStatus::Produced,
                PartitionOutcomeStatus::Failed,
            ],
        );
        assert_eq!(partial_failure.status, "ready_with_quarantine");
        assert_eq!(partial_failure.commands.len(), 1);
        let partial_detail: serde_json::Value =
            serde_json::from_str(&partial_failure.detail).expect("partial detail");
        assert_eq!(partial_detail["quarantined_units"][0]["label"], "src/b.cc");
        assert_eq!(partial_detail["quarantined_units"][0]["status"], "failed");

        let partial_timeout = partition_summary(
            &plan,
            units.clone(),
            vec![
                PartitionOutcomeStatus::Produced,
                PartitionOutcomeStatus::TimedOut,
            ],
        );
        assert_eq!(partial_timeout.status, "ready_with_quarantine");
        let timeout_detail: serde_json::Value =
            serde_json::from_str(&partial_timeout.detail).expect("timeout detail");
        assert_eq!(
            timeout_detail["quarantined_units"][0]["status"],
            "timed_out"
        );

        let failed_wipeout = partition_summary(
            &plan,
            units.clone(),
            vec![
                PartitionOutcomeStatus::Failed,
                PartitionOutcomeStatus::Failed,
            ],
        );
        assert_eq!(failed_wipeout.status, "failed");
        assert_eq!(failed_wipeout.commands.len(), 0);
        assert_eq!(failed_wipeout.failure_count, 2);

        let timeout_wipeout = partition_summary(
            &plan,
            units,
            vec![
                PartitionOutcomeStatus::TimedOut,
                PartitionOutcomeStatus::TimedOut,
            ],
        );
        assert_eq!(timeout_wipeout.status, "timed_out");
        assert_eq!(timeout_wipeout.commands.len(), 0);
        assert_eq!(timeout_wipeout.failure_count, 2);
    }

    #[test]
    fn clang_partition_detail_records_quarantine_cache_and_budget() {
        let tmp = workspace_tempdir("clang-detail-");
        let mut plan = fake_plan(SupportedIndexer::Clang, "repo");
        plan.output_path = tmp.path().join("out/repo-cpp-root.scip");
        let budget = super::super::budget::budget_for_indexer(
            SupportedIndexer::Clang,
            &super::super::budget::WorkspaceSizeHint {
                source_file_count: 2,
                source_bytes: 128,
                partition_count: 2,
            },
            None,
            None,
        );
        let cache = PartitionCacheSummary { hits: 1, misses: 1 };
        let detail = quarantine_detail_json(
            &plan,
            &[serde_json::json!({
                "scope": "clang_translation_unit",
                "label": "src/b.cc",
                "status": "timed_out",
                "detail": "partition timeout",
            })],
            1,
            &cache,
            &budget,
            Duration::from_secs(5),
            2,
        );
        let parsed: serde_json::Value = serde_json::from_str(&detail).expect("detail json");

        assert_eq!(parsed["kind"], "quarantine_v1");
        assert_eq!(parsed["scope"], "clang_translation_unit");
        assert_eq!(parsed["quarantined_units"][0]["label"], "src/b.cc");
        assert_eq!(parsed["quarantined_units"][0]["status"], "timed_out");
        assert_eq!(parsed["produced_artifact_count"], 1);
        assert_eq!(parsed["partition_count"], 2);
        assert_eq!(parsed["cache"]["hits"], 1);
        assert_eq!(parsed["cache"]["misses"], 1);
        assert!(parsed["budget"]["total_ms"].as_u64().unwrap() > 0);
        assert!(parsed["budget"]["per_partition_ms"].as_u64().unwrap() > 0);
    }

    #[test]
    fn partition_tally_surfaces_ready_with_quarantine_and_total_wipeout() {
        let success_plan = fake_plan(SupportedIndexer::Go, "repo/pkg-a");
        let partial = PartitionExecutionSummary {
            commands: vec![ExecutedIndexerCommand {
                plan: success_plan,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            }],
            status: "ready_with_quarantine".to_string(),
            detail: serde_json::json!({
                "kind": "quarantine_v1",
                "scope": "go_package",
                "quarantined_units": [{"label":"./pkg/b","status":"timed_out"}],
                "produced_artifact_count": 1,
                "cache": {"hits": 0, "misses": 2},
                "budget": {"total_ms": 60000, "per_partition_ms": 30000}
            })
            .to_string(),
            failure_count: 1,
            total_count: 2,
        };
        let tally = tally_indexer_results(vec![(
            fake_plan(SupportedIndexer::Go, "repo"),
            PlanExecution::Partitioned(partial),
        )])
        .expect("partial partition tally");
        assert!(!tally.all_failed);
        assert_eq!(tally.workspace_statuses[0].status, "ready_with_quarantine");
        assert!(
            tally.workspace_statuses[0]
                .detail
                .as_ref()
                .unwrap()
                .contains("quarantine_v1")
        );

        let wipeout = PartitionExecutionSummary {
            commands: Vec::new(),
            status: "timed_out".to_string(),
            detail:
                serde_json::json!({"kind":"quarantine_v1","quarantined_units":[{"label":"a.cc"}]})
                    .to_string(),
            failure_count: 2,
            total_count: 2,
        };
        let tally = tally_indexer_results(vec![(
            fake_plan(SupportedIndexer::Clang, "repo"),
            PlanExecution::Partitioned(wipeout),
        )])
        .expect("wipeout tally");
        assert!(tally.all_failed);
        assert_eq!(tally.workspace_statuses[0].status, "timed_out");
    }

    #[test]
    fn tally_partial_failure_succeeds_when_at_least_one_indexer_succeeds() {
        // 2 failed targets + 1 success → overall Ok, only the success kept.
        let results = vec![
            (
                fake_plan(SupportedIndexer::TypeScript, "packages/lib"),
                ran_output(false),
            ),
            (
                fake_plan(SupportedIndexer::TypeScript, "packages/utils"),
                ran_output(false),
            ),
            (
                fake_plan(SupportedIndexer::TypeScript, "packages/acqua"),
                ran_output(true),
            ),
        ];
        let tally = tally_indexer_results(results).expect("partial success must be Ok");
        assert_eq!(
            tally.commands.len(),
            1,
            "only the succeeding target should be retained"
        );
        assert_eq!(
            tally.commands[0].plan.workspace_root,
            PathBuf::from("packages/acqua")
        );
        assert_eq!(tally.workspace_statuses.len(), 3);
        assert_eq!(tally.workspace_statuses[0].workspace_slug, "packages-lib");
        assert_eq!(tally.workspace_statuses[0].status, "failed");
        assert_eq!(tally.workspace_statuses[2].workspace_slug, "packages-acqua");
        assert_eq!(tally.workspace_statuses[2].status, "artifact_pending");
        assert!(!tally.all_failed);
    }

    #[test]
    fn tally_total_failure_records_all_failed_without_swallowing_statuses() {
        let results = vec![
            (
                fake_plan(SupportedIndexer::TypeScript, "packages/lib"),
                ran_output(false),
            ),
            (
                fake_plan(SupportedIndexer::TypeScript, "packages/utils"),
                ran_output(false),
            ),
        ];
        let tally =
            tally_indexer_results(results).expect("tally records statuses before caller errors");
        assert!(tally.all_failed);
        assert!(tally.commands.is_empty());
        assert_eq!(
            tally
                .workspace_statuses
                .iter()
                .map(|entry| (entry.workspace_slug.as_str(), entry.status.as_str()))
                .collect::<Vec<_>>(),
            vec![("packages-lib", "failed"), ("packages-utils", "failed")]
        );
    }

    #[test]
    fn tally_empty_input_is_ok() {
        // A code-less repo plans zero indexers — that's not a failure.
        let tally = tally_indexer_results(Vec::new()).expect("empty is Ok");
        assert!(tally.commands.is_empty());
        assert!(tally.workspace_statuses.is_empty());
        assert!(!tally.all_failed);
    }

    // -----------------------------------------------------------------
    // Cache hit / miss / error integration (acceptance criterion 2)
    // -----------------------------------------------------------------

    /// `PlanExecution::CachedHit` is treated as a successful invocation:
    /// the command is retained with exit 0 and an `artifact_pending` status,
    /// and `all_failed` is false.
    #[test]
    fn tally_cache_hit_is_treated_as_success() {
        let results = vec![(
            fake_plan(SupportedIndexer::TypeScript, "ui"),
            PlanExecution::CachedHit,
        )];
        let tally = tally_indexer_results(results).expect("cache hit must be Ok");
        assert_eq!(tally.commands.len(), 1);
        assert_eq!(tally.commands[0].exit_code, Some(0));
        assert_eq!(tally.workspace_statuses[0].status, "artifact_pending");
        assert!(
            tally.workspace_statuses[0]
                .detail
                .as_ref()
                .is_some_and(|d| d.contains("cache_hit"))
        );
        assert!(!tally.all_failed);
    }

    /// A cache hit combined with a failed invocation still yields overall
    /// success — the cache hit counts as a produced artifact.
    #[test]
    fn tally_cache_hit_with_failed_run_still_succeeds() {
        let results = vec![
            (
                fake_plan(SupportedIndexer::TypeScript, "ui"),
                PlanExecution::CachedHit,
            ),
            (
                fake_plan(SupportedIndexer::TypeScript, "api"),
                ran_output(false),
            ),
        ];
        let tally = tally_indexer_results(results).expect("partial success must be Ok");
        assert_eq!(
            tally.commands.len(),
            1,
            "only the cached hit command retained"
        );
        assert!(!tally.all_failed);
    }

    /// Deadline-exhausted invocations record `timed_out` with a JSON detail
    /// carrying the `deadline_exhausted` kind, and count as failures.
    #[test]
    fn tally_deadline_exhausted_records_timed_out_with_detail() {
        let detail = serde_json::json!({
            "kind": "deadline_exhausted",
            "reason": "no usable time remaining"
        })
        .to_string();
        let results = vec![(
            fake_plan(SupportedIndexer::RustAnalyzer, "server"),
            PlanExecution::DeadlineExhausted(detail.clone()),
        )];
        let tally = tally_indexer_results(results).expect("tally records statuses");
        assert_eq!(tally.workspace_statuses.len(), 1);
        assert_eq!(tally.workspace_statuses[0].status, "timed_out");
        assert_eq!(
            tally.workspace_statuses[0].detail.as_deref(),
            Some(detail.as_str())
        );
        assert!(tally.commands.is_empty());
        assert!(
            tally.all_failed,
            "sole target deadline-exhausted → all_failed"
        );
    }

    /// Deadline-exhausted + cache hit still succeeds: the cache hit prevents
    /// total wipeout.
    #[test]
    fn tally_deadline_exhausted_with_cache_hit_succeeds() {
        let detail = r#"{"kind":"deadline_exhausted"}"#.to_string();
        let results = vec![
            (
                fake_plan(SupportedIndexer::RustAnalyzer, "server"),
                PlanExecution::DeadlineExhausted(detail),
            ),
            (
                fake_plan(SupportedIndexer::TypeScript, "ui"),
                PlanExecution::CachedHit,
            ),
        ];
        let tally = tally_indexer_results(results).expect("partial success");
        assert!(!tally.all_failed);
        assert_eq!(tally.commands.len(), 1);
        // Both statuses remain visible.
        assert_eq!(tally.workspace_statuses.len(), 2);
        let statuses: Vec<&str> = tally
            .workspace_statuses
            .iter()
            .map(|s| s.status.as_str())
            .collect();
        assert!(statuses.contains(&"timed_out"));
        assert!(statuses.contains(&"artifact_pending"));
    }

    /// Cache lookup before invocation: a stored artifact produces a hit and
    /// the indexer is not invoked. Uses a fake cache store and fake artifact,
    /// not real SCIP binaries.
    #[tokio::test]
    async fn cache_hit_copies_artifact_and_skips_invocation() {
        let tmp = workspace_tempdir("scip-cache-hit-");
        let cache_root = tmp.path().join("cache");
        let store = super::super::cache::ScipCacheStore::new(&cache_root);

        // Build a fake plan pointing at the temp workspace.
        let workspace_root = tmp.path().join("repo");
        fs::create_dir_all(&workspace_root).unwrap();
        fs::write(workspace_root.join("main.ts"), b"console.log('hi')\n").unwrap();

        let output_path = tmp.path().join("output/index.scip");
        let plan = PlannedIndexerCommand {
            indexer: SupportedIndexer::TypeScript,
            binary_path: PathBuf::from("/fake/scip-typescript"),
            args: vec!["index".to_string(), "--output".to_string()],
            working_directory: workspace_root.clone(),
            workspace_root: workspace_root.clone(),
            workspace_rel_root: PathBuf::new(),
            workspace_slug: "repo".to_string(),
            output_path: output_path.clone(),
        };

        // Compute the cache key (tool version detection will fail for the
        // fake binary, so we construct the key manually).
        let ingredients = super::super::cache::CacheKeyIngredients::from_plan(
            &plan,
            "scip-typescript 1.0.0-fake",
            std::collections::BTreeMap::from([("main.ts".to_string(), "hash-a".to_string())]),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        );
        let key = ingredients.cache_key().expect("compute key");

        // Seed the cache with a fake artifact.
        let seed = tmp.path().join("seed.scip");
        fs::write(&seed, b"fake scip artifact bytes").unwrap();
        store.store_artifact(&key, &seed).expect("store seed");

        // Lookup should be a hit and copy the artifact to the output path.
        assert_eq!(
            store.lookup(&key, &output_path),
            super::super::cache::CacheLookup::Hit
        );
        assert_eq!(fs::read(&output_path).unwrap(), b"fake scip artifact bytes");
    }

    /// Cache miss leaves no output file and does not invoke the indexer.
    #[tokio::test]
    async fn cache_miss_does_not_copy_artifact() {
        let tmp = workspace_tempdir("scip-cache-miss-");
        let store = super::super::cache::ScipCacheStore::new(tmp.path().join("cache"));

        let ingredients = super::super::cache::CacheKeyIngredients::new(
            SupportedIndexer::TypeScript,
            super::super::cache::ToolVersionRecord::new(
                SupportedIndexer::TypeScript,
                "/fake/scip-typescript",
                "1.0.0",
                &std::collections::BTreeMap::new(),
            ),
            super::super::cache::CommandShape {
                binary_name: "scip-typescript".to_string(),
                args: vec!["index".to_string()],
                working_directory: super::super::cache::WorkspaceIdentity {
                    workspace_rel_root: PathBuf::from("ui"),
                    workspace_slug: "ui".to_string(),
                },
            },
            super::super::cache::WorkspaceIdentity {
                workspace_rel_root: PathBuf::from("ui"),
                workspace_slug: "ui".to_string(),
            },
        );
        let key = ingredients.cache_key().expect("key");
        let output = tmp.path().join("miss/out.scip");

        assert_eq!(
            store.lookup(&key, &output),
            super::super::cache::CacheLookup::Miss
        );
        assert!(!output.exists(), "miss must not write an output file");
    }

    /// Cache write after successful artifact production stores the artifact
    /// for later reuse.
    #[tokio::test]
    async fn cache_write_after_success_enables_future_hit() {
        let tmp = workspace_tempdir("scip-cache-write-");
        let store = super::super::cache::ScipCacheStore::new(tmp.path().join("cache"));

        let ingredients = super::super::cache::CacheKeyIngredients::new(
            SupportedIndexer::TypeScript,
            super::super::cache::ToolVersionRecord::new(
                SupportedIndexer::TypeScript,
                "/fake/scip-typescript",
                "1.0.0",
                &std::collections::BTreeMap::new(),
            ),
            super::super::cache::CommandShape {
                binary_name: "scip-typescript".to_string(),
                args: vec!["index".to_string()],
                working_directory: super::super::cache::WorkspaceIdentity {
                    workspace_rel_root: PathBuf::from("ui"),
                    workspace_slug: "ui".to_string(),
                },
            },
            super::super::cache::WorkspaceIdentity {
                workspace_rel_root: PathBuf::from("ui"),
                workspace_slug: "ui".to_string(),
            },
        );
        let key = ingredients.cache_key().expect("key");

        // Simulate a successful indexer run producing a non-empty artifact.
        let artifact = tmp.path().join("produced.scip");
        fs::write(&artifact, b"freshly produced scip bytes").unwrap();
        store
            .store_artifact(&key, &artifact)
            .expect("store after success");

        // A subsequent lookup with the same key must hit.
        let output = tmp.path().join("reuse/out.scip");
        assert_eq!(
            store.lookup(&key, &output),
            super::super::cache::CacheLookup::Hit
        );
        assert_eq!(fs::read(&output).unwrap(), b"freshly produced scip bytes");
    }

    /// `apply_artifact_statuses` converts a cache-hit `artifact_pending` row
    /// to `ready` when the artifact is collected, and converts a no-artifact
    /// `artifact_pending` to `failed`.
    #[test]
    fn apply_artifact_statuses_preserves_partial_success_for_cache_hits() {
        let mut statuses = vec![
            WorkspaceWarmStatus {
                workspace_slug: "ui".to_string(),
                indexer: SupportedIndexer::TypeScript,
                status: "artifact_pending".to_string(),
                detail: Some(r#"{"kind":"cache_hit"}"#.to_string()),
                workspace_rel_root: String::new(),
            },
            WorkspaceWarmStatus {
                workspace_slug: "api".to_string(),
                indexer: SupportedIndexer::TypeScript,
                status: "artifact_pending".to_string(),
                detail: Some("expected artifact /out/api.scip".to_string()),
                workspace_rel_root: String::new(),
            },
        ];
        let artifacts = vec![ScipArtifact {
            path: PathBuf::from("/out/ui.scip"),
            indexer: Some(SupportedIndexer::TypeScript),
            workspace_slug: "ui".to_string(),
            workspace_root: PathBuf::new(),
        }];
        apply_artifact_statuses(&artifacts, &mut statuses);
        assert_eq!(statuses[0].status, "ready");
        assert_eq!(statuses[1].status, "failed");
    }

    #[test]
    fn cargo_target_dir_guard_restores_on_panic_unwind() {
        let _serial = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by ENV_TEST_LOCK above.
        unsafe { std::env::set_var("CARGO_TARGET_DIR", "/tmp/sentinel-pre-panic") };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = CargoTargetDirGuard::new(Path::new("/tmp/sentinel-during-panic"));
            assert_eq!(
                std::env::var_os("CARGO_TARGET_DIR"),
                Some(std::ffi::OsString::from("/tmp/sentinel-during-panic"))
            );
            panic!("simulated indexer panic");
        }));
        assert!(result.is_err(), "expected panic to propagate");
        assert_eq!(
            std::env::var_os("CARGO_TARGET_DIR"),
            Some(std::ffi::OsString::from("/tmp/sentinel-pre-panic"))
        );
        // SAFETY: serialised by ENV_TEST_LOCK above.
        unsafe { std::env::remove_var("CARGO_TARGET_DIR") };
    }
    // -------------------------------------------------------------------
    // Workspace divergence overlay — declared EnvironmentConfig workspaces
    // are log-only / additive. Marker/discovery remains authoritative for
    // what gets indexed, and the structured warning retains the exact
    // `declared_but_not_found` / `found_but_undeclared` field names.
    // -------------------------------------------------------------------

    /// A `PlannedIndexerCommand` skeleton that exercises the divergence
    /// helper without going through filesystem discovery. Mirrors the
    /// shape `plan_indexer_commands` produces, but with a fixed
    /// `workspace_slug` so the divergence math is deterministic.
    fn plan_with_slug(slug: &str) -> PlannedIndexerCommand {
        PlannedIndexerCommand {
            indexer: SupportedIndexer::TypeScript,
            binary_path: PathBuf::from("/tooling/scip-typescript"),
            args: vec!["index".to_string()],
            working_directory: PathBuf::from(slug),
            workspace_root: PathBuf::from(slug),
            workspace_rel_root: PathBuf::from(slug),
            workspace_slug: slug.to_string(),
            output_path: PathBuf::from(slug).join("out.scip"),
        }
    }

    fn declared_workspace(slug: Option<&str>, root: &str) -> djinn_stack::Workspace {
        djinn_stack::Workspace {
            slug: slug.map(str::to_owned),
            name: None,
            tags: Vec::new(),
            root: root.to_string(),
            language: "typescript".to_string(),
            toolchain: None,
            version: None,
            package_manager: None,
        }
    }

    /// `None` declared → empty divergence (no warning emitted).
    #[test]
    fn compute_workspace_divergence_returns_default_when_no_declared_workspaces() {
        let plans = vec![plan_with_slug("server"), plan_with_slug("desktop")];
        let divergence = compute_workspace_divergence(&plans, None);
        assert_eq!(
            divergence,
            WorkspaceDivergence::default(),
            "no declared list must short-circuit the warning"
        );
        assert!(divergence.is_empty());
    }

    /// Empty `Some([])` declared → still empty divergence; the warning
    /// must not fire just because the caller passed an empty list (no
    /// declared set to drift away from).
    #[test]
    fn compute_workspace_divergence_empty_declared_list_is_not_divergence() {
        let plans = vec![plan_with_slug("server")];
        let empty: &[djinn_stack::Workspace] = &[];
        let divergence = compute_workspace_divergence(&plans, Some(empty));
        assert_eq!(
            divergence,
            WorkspaceDivergence {
                declared_but_not_found: vec![],
                found_but_undeclared: vec!["server".to_string()],
            },
            "an empty declared list means nothing was claimed; the discovered set is fully undeclared"
        );
    }

    /// `declared_but_not_found` covers the case where the config claims a
    /// workspace that has no marker files on disk (e.g. a stale
    /// `EnvironmentConfig` entry from a deleted directory). The
    /// discovered `server` is declared as well, so the undeclared side
    /// must stay empty — only the stale claim surfaces.
    #[test]
    fn compute_workspace_divergence_reports_declared_but_not_found() {
        let plans = vec![plan_with_slug("server")];
        let declared = vec![
            declared_workspace(Some("server"), "server"), // matches disk
            declared_workspace(Some("legacy"), "stale/path"), // declared, not on disk
        ];
        let divergence = compute_workspace_divergence(&plans, Some(&declared));
        assert_eq!(
            divergence.declared_but_not_found,
            vec!["legacy".to_string()],
            "explicit declared slug with no matching discovered target must appear in declared_but_not_found"
        );
        assert!(
            divergence.found_but_undeclared.is_empty(),
            "no extra divergence on the undeclared side: {divergence:?}"
        );
        assert!(!divergence.is_empty());
    }

    /// `found_but_undeclared` covers the case where marker/discovery
    /// finds a workspace the config hasn't claimed (e.g. a new package
    /// added under `packages/` that the operator hasn't told
    /// `EnvironmentConfig` about yet).
    #[test]
    fn compute_workspace_divergence_reports_found_but_undeclared() {
        let plans = vec![
            plan_with_slug("server"),
            plan_with_slug("desktop"),
            plan_with_slug("acqua"),
        ];
        // Only `server` is declared; the other two are unindexed-but-on-disk.
        let declared = vec![declared_workspace(Some("server"), "server")];
        let divergence = compute_workspace_divergence(&plans, Some(&declared));
        assert!(
            divergence.declared_but_not_found.is_empty(),
            "server is declared and discovered, so declared_but_not_found must be empty"
        );
        assert_eq!(
            divergence.found_but_undeclared,
            vec!["acqua".to_string(), "desktop".to_string()],
            "unclaimed discovered workspaces appear in found_but_undeclared (sorted)"
        );
    }

    /// Both directions can fire at once — a project whose declared list
    /// partially overlaps its discovered set.
    #[test]
    fn compute_workspace_divergence_reports_both_directions_simultaneously() {
        let plans = vec![plan_with_slug("server"), plan_with_slug("desktop")];
        let declared = vec![
            declared_workspace(Some("server"), "server"), // matches
            declared_workspace(Some("stale"), "stale/path"), // declared, not on disk
        ];
        let divergence = compute_workspace_divergence(&plans, Some(&declared));
        assert_eq!(divergence.declared_but_not_found, vec!["stale".to_string()]);
        assert_eq!(divergence.found_but_undeclared, vec!["desktop".to_string()]);
    }

    /// `slug: None` must fall back to the shared
    /// `djinn_stack::workspace_slug` derivation, so a `root` of `server`
    /// maps to the same slug discovery uses for `server/Cargo.toml`.
    #[test]
    fn compute_workspace_divergence_uses_fallback_slug_when_unset() {
        let plans = vec![plan_with_slug("server")];
        let declared = vec![declared_workspace(None, "server")];
        let divergence = compute_workspace_divergence(&plans, Some(&declared));
        assert!(
            divergence.is_empty(),
            "fallback slug should match the discovered `server` slug: {divergence:?}"
        );
    }

    /// An explicit `slug` in `EnvironmentConfig` is compared as-is — the
    /// declared side uses the operator's chosen identifier rather than
    /// the shared derivation. This is how a workspace with a
    /// sanitisation-conflicting root (e.g. `packages/api`) can be
    /// tagged with a stable manual slug like `api-prod`.
    #[test]
    fn compute_workspace_divergence_uses_explicit_slug_when_set() {
        let plans = vec![plan_with_slug("packages-api-f59bf297")];
        let declared = vec![declared_workspace(Some("api-prod"), "packages/api")];
        let divergence = compute_workspace_divergence(&plans, Some(&declared));
        assert_eq!(
            divergence.declared_but_not_found,
            vec!["api-prod".to_string()],
            "explicit slug is the source of truth on the declared side"
        );
        assert_eq!(
            divergence.found_but_undeclared,
            vec!["packages-api-f59bf297".to_string()],
            "and the shared-derivation slug is the source of truth on the discovered side"
        );
    }

    /// Empty-string `slug` should be treated as "not set" so the
    /// fallback derivation kicks in. (Matches `workspace_declared_slug`.)
    #[test]
    fn compute_workspace_divergence_treats_empty_slug_as_unset() {
        let plans = vec![plan_with_slug("server")];
        let declared = vec![declared_workspace(Some(""), "server")];
        let divergence = compute_workspace_divergence(&plans, Some(&declared));
        assert!(
            divergence.is_empty(),
            "empty slug must fall back to shared derivation: {divergence:?}"
        );
    }

    /// End-to-end: the divergence math sees one plan per
    /// `(indexer, discovered workspace)` pair. If the same discovered
    /// workspace is planned by two indexers, the slug should still
    /// de-duplicate on the divergence side — the warning is about
    /// workspace coverage, not about per-indexer plan cardinality.
    #[test]
    fn compute_workspace_divergence_dedupes_repeated_plan_slugs() {
        let plans = vec![
            PlannedIndexerCommand {
                indexer: SupportedIndexer::RustAnalyzer,
                binary_path: PathBuf::from("/tooling/rust-analyzer"),
                args: vec!["scip".to_string()],
                working_directory: PathBuf::from("server"),
                workspace_root: PathBuf::from("server"),
                workspace_rel_root: PathBuf::from("server"),
                workspace_slug: "server".to_string(),
                output_path: PathBuf::from("server/rust.scip"),
            },
            PlannedIndexerCommand {
                indexer: SupportedIndexer::TypeScript,
                binary_path: PathBuf::from("/tooling/scip-typescript"),
                args: vec!["index".to_string()],
                working_directory: PathBuf::from("server"),
                workspace_root: PathBuf::from("server"),
                workspace_rel_root: PathBuf::from("server"),
                workspace_slug: "server".to_string(),
                output_path: PathBuf::from("server/ts.scip"),
            },
        ];
        let declared = vec![
            declared_workspace(Some("server"), "server"), // matches both plans
            declared_workspace(Some("stale"), "stale/path"), // declared, not on disk
        ];
        let divergence = compute_workspace_divergence(&plans, Some(&declared));
        assert_eq!(divergence.declared_but_not_found, vec!["stale".to_string()]);
        assert!(
            divergence.found_but_undeclared.is_empty(),
            "server appears in two plans but the warning is slug-scoped, so it must not double-report"
        );
    }

    /// Discovered roots under test trees (fixture repos with their own
    /// `Cargo.toml`) are synthetic — their manifests routinely reference
    /// members that only exist at test runtime, so indexing them fails
    /// every warm. They must be skipped unless explicitly declared.
    #[test]
    fn plan_indexer_commands_skips_undeclared_test_path_workspaces() {
        let tmp = tempdir_in_tmp();
        let project_root = tmp.path().join("djinn");
        fs::create_dir_all(project_root.join("server")).expect("create server dir");
        fs::write(
            project_root.join("server/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("write rust workspace");
        let fixture_root = project_root.join("server/tests/fixtures/polyglot");
        fs::create_dir_all(&fixture_root).expect("create fixture dir");
        fs::write(
            fixture_root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .expect("write fixture workspace");
        let output_root = project_root.join(".djinn/scip");

        let available = vec![IndexerAvailability {
            indexer: SupportedIndexer::RustAnalyzer,
            binary: "rust-analyzer".to_string(),
            path: Some(PathBuf::from("/tooling/rust-analyzer")),
        }];

        let plans = plan_indexer_commands(&project_root, &output_root, &available, None);
        assert_eq!(
            plans.iter().map(|p| &p.workspace_slug).collect::<Vec<_>>(),
            vec!["server"],
            "fixture workspace under tests/ must not be planned"
        );

        // Declaring the fixture root is the escape hatch — declared roots
        // are planned even under test paths.
        let declared = vec![declared_workspace(None, "server/tests/fixtures/polyglot")];
        let plans = plan_indexer_commands(&project_root, &output_root, &available, Some(&declared));
        assert_eq!(
            plans.len(),
            2,
            "explicitly declared test-path workspace must be planned"
        );
    }

    /// `plan_indexer_commands` must NOT add a planned command for a
    /// declared workspace that has no marker files. The declared config
    /// is additive / log-only — discovery is the only source of
    /// `PlannedIndexerCommand`s.
    #[test]
    fn plan_indexer_commands_ignores_declared_only_workspaces() {
        let tmp = tempdir_in_tmp();
        let project_root = tmp.path().join("djinn");
        fs::create_dir_all(project_root.join("server")).expect("create server dir");
        fs::write(
            project_root.join("server/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("write rust workspace");
        let output_root = project_root.join(".djinn/scip");

        let available = vec![IndexerAvailability {
            indexer: SupportedIndexer::RustAnalyzer,
            binary: "rust-analyzer".to_string(),
            path: Some(PathBuf::from("/tooling/rust-analyzer")),
        }];

        // Declare a workspace that has NO marker files on disk. The
        // planner must still only produce the `server` plan.
        let declared = vec![
            declared_workspace(Some("phantom"), "phantom/never-existed"),
            declared_workspace(Some("ghost"), "ghost"),
        ];

        let plans = plan_indexer_commands(&project_root, &output_root, &available, Some(&declared));
        assert_eq!(
            plans.len(),
            1,
            "declared-only workspaces must not become planned indexer commands"
        );
        assert_eq!(plans[0].workspace_slug, "server");
    }

    /// Mirror of the above: a discovered workspace that was NOT claimed
    /// in `EnvironmentConfig` must still be planned and indexed. Marker /
    /// discovery is authoritative; the declared list is metadata, not a
    /// filter.
    #[test]
    fn plan_indexer_commands_plans_undeclared_but_discovered_workspaces() {
        let tmp = tempdir_in_tmp();
        let project_root = tmp.path().join("djinn");
        fs::create_dir_all(project_root.join("server")).expect("create server dir");
        fs::create_dir_all(project_root.join("desktop")).expect("create desktop dir");
        fs::write(
            project_root.join("server/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("write rust workspace");
        fs::write(project_root.join("desktop/tsconfig.json"), "{}\n")
            .expect("write desktop tsconfig");
        let output_root = project_root.join(".djinn/scip");

        let available = vec![
            IndexerAvailability {
                indexer: SupportedIndexer::RustAnalyzer,
                binary: "rust-analyzer".to_string(),
                path: Some(PathBuf::from("/tooling/rust-analyzer")),
            },
            IndexerAvailability {
                indexer: SupportedIndexer::TypeScript,
                binary: "scip-typescript".to_string(),
                path: Some(PathBuf::from("/tooling/scip-typescript")),
            },
        ];

        // Declared config is *narrower* than discovery: only `server` is
        // listed. `desktop` (TS) is discovered but undeclared and must
        // still get planned.
        let declared = vec![declared_workspace(Some("server"), "server")];

        let plans = plan_indexer_commands(&project_root, &output_root, &available, Some(&declared));
        let slugs: Vec<&str> = plans.iter().map(|p| p.workspace_slug.as_str()).collect();
        assert!(
            slugs.contains(&"server"),
            "declared-and-discovered workspace must be planned: {slugs:?}"
        );
        assert!(
            slugs.contains(&"desktop"),
            "discovered-but-undeclared workspace must still be planned: {slugs:?}"
        );
        assert_eq!(plans.len(), 2);
    }

    /// End-to-end with both a `slug: None` (fallback) and an explicit
    /// `slug` declared entry. The fallback-derived entry matches the
    /// discovered workspace, and the explicit-slug entry is reported as
    /// `declared_but_not_found` because nothing on disk has that slug.
    /// Confirms the planner-side contract end-to-end via the
    /// `tracing::warn!` field names — these would surface in the
    /// `tracing-subscriber` JSON output the warmer uses.
    #[test]
    fn plan_indexer_commands_with_mixed_declared_slugs_exercises_warning_fields() {
        let tmp = tempdir_in_tmp();
        let project_root = tmp.path().join("djinn");
        fs::create_dir_all(project_root.join("server")).expect("create server dir");
        fs::write(
            project_root.join("server/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("write rust workspace");
        let output_root = project_root.join(".djinn/scip");

        let available = vec![IndexerAvailability {
            indexer: SupportedIndexer::RustAnalyzer,
            binary: "rust-analyzer".to_string(),
            path: Some(PathBuf::from("/tooling/rust-analyzer")),
        }];

        // 1. `slug: None` → fallback derivation, matches discovered
        //    `server` slug → no divergence on this side.
        // 2. `slug: Some("api")` with a `root` of `phantom/api` → no
        //    marker files on disk under that root, so `api` ends up
        //    in `declared_but_not_found`.
        let declared = vec![
            declared_workspace(None, "server"),
            declared_workspace(Some("api"), "phantom/api"),
        ];

        let plans = plan_indexer_commands(&project_root, &output_root, &available, Some(&declared));
        assert_eq!(plans.len(), 1, "only `server` is on disk");
        assert_eq!(plans[0].workspace_slug, "server");

        // Now exercise the pure helper on the same inputs to assert the
        // structured-field contract. `plan_indexer_commands` swallowed
        // the warning into the global tracing dispatcher; here we
        // assert exactly what its structured fields would contain.
        let divergence = compute_workspace_divergence(&plans, Some(&declared));
        assert_eq!(divergence.declared_but_not_found, vec!["api".to_string()]);
        assert!(
            divergence.found_but_undeclared.is_empty(),
            "server is declared and discovered, so no undeclared side: {divergence:?}"
        );
    }
}
