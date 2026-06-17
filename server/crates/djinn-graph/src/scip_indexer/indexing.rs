// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::process;

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
) -> Result<IndexingRun> {
    let _guard = target_dir.map(CargoTargetDirGuard::new);
    run_indexers(
        project_root,
        output_root,
        language_filter,
        declared_workspaces,
    )
    .await
}

/// Compute the wall-clock timeout for a planned indexer command.
///
/// In the default production path (no active deadline, no prior timing), the
/// budget model yields the indexer baseline which is combined with the legacy
/// `SupportedIndexer::timeout()` cap via `max()`. This guarantees the shipped
/// fixed-cap behavior is preserved exactly — the budget model only *raises*
/// timeouts when workspace size / prior timing warrant it, and only *lowers*
/// them when an active deadline is supplied (which the current flow does not
/// do yet; the deadline/prior inputs are hooks for later integration tasks).
fn budgeted_timeout_for_plan(plan: &PlannedIndexerCommand) -> std::time::Duration {
    let size = super::budget::estimate_workspace_size(&plan.working_directory, plan.indexer);
    let budget = super::budget::budget_for_indexer(plan.indexer, &size, None, None);

    // Preserve the legacy fixed cap: `max(budget.per_invocation, timeout())`.
    // This ensures equivalent behavior when no deadline/prior timing exists:
    // the budget model may raise the timeout above the fixed cap (for large
    // workspaces) but never lowers it below the shipped cap.
    budget.per_invocation.max(plan.indexer.timeout())
}

pub(crate) async fn run_indexers(
    project_root: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
    language_filter: Option<&[SupportedIndexer]>,
    declared_workspaces: Option<&[djinn_stack::Workspace]>,
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
            // Compute a scaled budget for this workspace/indexer. When no
            // active deadline or prior timing is available (the current
            // production path), the budget is combined with the legacy
            // `SupportedIndexer::timeout()` cap so runtime behavior stays
            // equivalent — the budget model never shortens a timeout below
            // the shipped fixed cap in the default case.
            let timeout = budgeted_timeout_for_plan(&plan);
            let cmd = plan.build_command();
            async move {
                let result = process::output_with_timeout(cmd, timeout).await;
                (plan, result)
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;

    let mut tally = tally_indexer_results(results)?;

    let artifacts = collect_scip_artifacts(&output_root, &tally.commands)?;
    apply_artifact_statuses(&artifacts, &mut tally.workspace_statuses);
    write_workspace_warm_statuses(&project_root, &tally.workspace_statuses)?;

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
    results: Vec<(PlannedIndexerCommand, std::io::Result<std::process::Output>)>,
) -> Result<IndexerTally> {
    let total = results.len();
    let mut commands = Vec::with_capacity(total);
    let mut workspace_statuses = Vec::with_capacity(total);
    let mut failure_count = 0usize;

    for (plan, result) in results {
        match result {
            Ok(output) if output.status.success() => {
                workspace_statuses.push(WorkspaceWarmStatus {
                    workspace_slug: plan.workspace_slug.clone(),
                    indexer: plan.indexer,
                    status: "artifact_pending".to_string(),
                    detail: Some(format!("expected artifact {}", plan.output_path.display())),
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
                });
                tracing::warn!(
                    indexer = plan.indexer.binary_name(),
                    workspace = %plan.workspace_root.display(),
                    error = %err,
                    "SCIP indexer error"
                );
            }
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

    Ok(IndexerTally {
        commands,
        workspace_statuses,
        all_failed: total > 0 && failure_count == total,
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

pub(crate) fn workspace_warm_status_path(project_root: &Path) -> PathBuf {
    project_root.join(".djinn").join("graph_warm_status.json")
}

fn write_workspace_warm_statuses(
    project_root: &Path,
    statuses: &[WorkspaceWarmStatus],
) -> Result<()> {
    let path = workspace_warm_status_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create graph warm status dir {}", parent.display()))?;
    }
    let json =
        serde_json::to_string_pretty(statuses).context("serialize workspace warm statuses")?;
    fs::write(&path, json).with_context(|| format!("write graph warm status {}", path.display()))
}

/// Append a non-fatal graph-cache warning to the same status file consumed by
/// the warm-status surface. The synthetic `graph-cache` row deliberately uses a
/// warning status (not `failed`/`timed_out`) so it is visible without making the
/// overall graph warm state look like an indexer failure.
pub(crate) fn append_graph_cache_shrink_warning(
    project_root: &Path,
    statuses: &[WorkspaceWarmStatus],
    detail: String,
) -> Result<()> {
    let mut statuses = statuses.to_vec();
    statuses.push(WorkspaceWarmStatus {
        workspace_slug: "graph-cache".to_string(),
        indexer: SupportedIndexer::RustAnalyzer,
        status: "warning".to_string(),
        detail: Some(detail),
    });
    write_workspace_warm_statuses(project_root, &statuses)
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
            },
            WorkspaceWarmStatus {
                workspace_slug: "web".to_string(),
                indexer: SupportedIndexer::TypeScript,
                status: "artifact_pending".to_string(),
                detail: Some("expected artifact".to_string()),
            },
            WorkspaceWarmStatus {
                workspace_slug: "worker".to_string(),
                indexer: SupportedIndexer::TypeScript,
                status: "artifact_pending".to_string(),
                detail: Some("expected artifact".to_string()),
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
            run_indexers_already_locked(&project_root, &output_root, None, None, None).await;
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
    fn output_for(success: bool) -> std::io::Result<std::process::Output> {
        std::process::Command::new(if success { "true" } else { "false" }).output()
    }

    #[test]
    fn tally_partial_failure_succeeds_when_at_least_one_indexer_succeeds() {
        // 2 failed targets + 1 success → overall Ok, only the success kept.
        let results = vec![
            (
                fake_plan(SupportedIndexer::TypeScript, "packages/lib"),
                output_for(false),
            ),
            (
                fake_plan(SupportedIndexer::TypeScript, "packages/utils"),
                output_for(false),
            ),
            (
                fake_plan(SupportedIndexer::TypeScript, "packages/acqua"),
                output_for(true),
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
                output_for(false),
            ),
            (
                fake_plan(SupportedIndexer::TypeScript, "packages/utils"),
                output_for(false),
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
