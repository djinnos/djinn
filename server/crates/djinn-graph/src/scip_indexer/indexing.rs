use anyhow::{Context, Result, anyhow};
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
) -> Vec<PlannedIndexerCommand> {
    let project_root = project_root.as_ref();
    let output_root = output_root.as_ref();

    available_indexers
        .iter()
        .flat_map(|availability| {
            let Some(binary_path) = availability.path.as_ref() else {
                return Vec::new();
            };

            discover_workspaces(project_root, availability.indexer)
                .into_iter()
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
                        workspace_slug: workspace.slug,
                        output_path,
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
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
) -> Result<IndexingRun> {
    let _guard = target_dir.map(CargoTargetDirGuard::new);
    run_indexers(project_root, output_root, language_filter).await
}

pub(crate) async fn run_indexers(
    project_root: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
    language_filter: Option<&[SupportedIndexer]>,
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

    let plans = plan_indexer_commands(&project_root, &output_root, &available);
    let futures: Vec<_> = plans
        .into_iter()
        .map(|plan| {
            // Per-indexer cap: rust-analyzer needs minutes on a cold cache;
            // the rest stay on the short default. See `SupportedIndexer::timeout`.
            let timeout = plan.indexer.timeout();
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

pub(crate) fn collect_scip_artifacts(
    output_root: impl AsRef<Path>,
    commands: &[ExecutedIndexerCommand],
) -> Result<Vec<ScipArtifact>> {
    let output_root = output_root.as_ref();
    let mut seen = std::collections::HashSet::new();
    let mut artifacts = Vec::new();

    let expected_paths: Vec<(PathBuf, SupportedIndexer, String)> = commands
        .iter()
        .map(|command| {
            (
                command.plan.output_path.clone(),
                command.plan.indexer,
                command.plan.workspace_slug.clone(),
            )
        })
        .collect();

    for path in discover_scip_files(output_root)? {
        if seen.insert(path.clone()) {
            let matched = expected_paths
                .iter()
                .find(|(expected, _, _)| expected == &path);
            let indexer = matched.map(|(_, indexer, _)| *indexer);
            let workspace_slug = matched
                .map(|(_, _, workspace_slug)| workspace_slug.clone())
                .unwrap_or_else(|| "root".to_string());
            artifacts.push(ScipArtifact {
                path,
                indexer,
                workspace_slug,
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

        let plans = plan_indexer_commands(&project_root, &output_root, &available);

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

        let plans = plan_indexer_commands(&project_root, &output_root, &available);
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
        assert_eq!(
            plans[1..]
                .iter()
                .map(|plan| plan
                    .output_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned())
                .collect::<Vec<_>>(),
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

        let plans = plan_indexer_commands(&project_root, &output_root, &available);
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

        let plans = plan_indexer_commands(&project_root, &output_root, &available);
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

        let result = run_indexers_already_locked(&project_root, &output_root, None, None).await;
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
}
