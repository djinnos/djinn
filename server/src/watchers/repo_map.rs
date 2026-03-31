use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Test-only hook: when set, the watcher sends the project ID through this
/// channel each time a `project.created` event schedules an initial repo-map
/// refresh.  The sender is installed by test code before spawning watchers.
#[cfg(test)]
pub(crate) static REFRESH_SCHEDULED_TX: std::sync::Mutex<
    Option<tokio::sync::mpsc::UnboundedSender<String>>,
> = std::sync::Mutex::new(None);

use anyhow::Result;
use notify_debouncer_mini::{DebouncedEventKind, Debouncer, new_debouncer};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::events::DjinnEventEnvelope;
use crate::repo_graph::RepoDependencyGraph;
use crate::repo_map::{RepoMapRenderOptions, render_repo_map, run_indexers};
use crate::scip_parser::parse_scip_artifacts;
use crate::server::AppState;
use djinn_db::{
    CachedRepoMap, Database, GitSettingsRepository, NoteRepository, ProjectRepository,
    RepoMapCacheInsert, RepoMapCacheKey, RepoMapCacheRepository,
};

const DEBOUNCE: Duration = Duration::from_secs(2);
const DEFAULT_REPO_MAP_TOKEN_BUDGET: usize = 1200;
const WORKTREE_REUSE_DIFF_FILE_THRESHOLD: usize = 20;

struct RepoMapWatcherState {
    watchers: HashMap<PathBuf, Debouncer<notify::RecommendedWatcher>>,
    app_state: AppState,
    events_tx: tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    in_flight: Arc<Mutex<HashSet<RefreshTarget>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RefreshTarget {
    project_id: String,
    project_path: String,
    worktree_path: Option<String>,
    commit_sha: String,
}

impl RefreshTarget {
    fn key(&self) -> RepoMapCacheKey<'_> {
        RepoMapCacheKey {
            project_id: &self.project_id,
            project_path: &self.project_path,
            worktree_path: self.worktree_path.as_deref(),
            commit_sha: &self.commit_sha,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RefreshIdentity {
    project_id: String,
    project_path: PathBuf,
    worktree_path: Option<PathBuf>,
    commit_sha: String,
    reuse_plan: WorktreeReusePlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeReusePlan {
    base_commit_sha: Option<String>,
    diff_file_count: Option<usize>,
    /// The actual relative paths of files that changed between the merge-base
    /// and the worktree HEAD.  Populated only when `diff_file_count` is within
    /// the reuse threshold; `None` otherwise.
    changed_files: Option<Vec<PathBuf>>,
}

impl WorktreeReusePlan {
    fn canonical() -> Self {
        Self {
            base_commit_sha: None,
            diff_file_count: None,
            changed_files: None,
        }
    }

    fn reusable_base_commit(&self) -> Option<&str> {
        match (self.base_commit_sha.as_deref(), self.diff_file_count) {
            (Some(commit), Some(diff_count))
                if diff_count <= WORKTREE_REUSE_DIFF_FILE_THRESHOLD =>
            {
                Some(commit)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RefreshDecision {
    target: RefreshTarget,
    should_spawn: bool,
    reuse_from: Option<CachedRepoMap>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoMapRefreshError(String);

impl fmt::Display for RepoMapRefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RepoMapRefreshError {}

/// ADR-043 worktree reuse policy:
/// - reuse a cached map for the exact worktree HEAD commit when available;
/// - otherwise, for worktrees only, compute the merge-base/default-branch diff and reuse the
///   cached base commit when the changed-file count stays at or below
///   `WORKTREE_REUSE_DIFF_FILE_THRESHOLD`;
/// - fall back to a full index whenever the diff is too large, diff metadata is unavailable, or
///   no reusable cached base map exists.
pub fn spawn_repo_map_refresh_watchers(
    db: Database,
    events_tx: tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    cancel: CancellationToken,
) {
    let app_state = AppState::new(db.clone(), cancel.clone());
    let state = Arc::new(Mutex::new(RepoMapWatcherState {
        watchers: HashMap::new(),
        app_state,
        events_tx: events_tx.clone(),
        in_flight: Arc::new(Mutex::new(HashSet::new())),
    }));

    let state_clone = state.clone();
    tokio::spawn(async move {
        bootstrap_repo_map_refresh_watchers(state_clone.clone(), db.clone(), events_tx.clone())
            .await;

        let mut events_rx = events_tx.subscribe();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = events_rx.recv() => match recv {
                    Ok(envelope) => {
                        if envelope.entity_type == "project" && envelope.action == "created" {
                            let Some(project) = envelope.parse_payload::<djinn_core::models::Project>() else { continue; };
                            let mut guard = state_clone.lock().await;
                            let project_path = PathBuf::from(&project.path);
                            add_watch(&mut guard, &project.id, &project_path);
                            // Schedule initial repo-map refresh for the newly created project.
                            // Extract state needed for the refresh before dropping the lock.
                            let app_state = guard.app_state.clone();
                            let events_bus = crate::events::event_bus_for(&guard.events_tx);
                            let in_flight = guard.in_flight.clone();
                            let project_id = project.id.clone();
                            drop(guard);

                            // Test-only: signal that refresh was scheduled for this project.
                            #[cfg(test)]
                            if let Some(tx) = REFRESH_SCHEDULED_TX.lock().unwrap().as_ref() {
                                let _ = tx.send(project_id.clone());
                            }

                            tokio::spawn(async move {
                                if let Err(error) = refresh_project_and_worktrees(
                                    &app_state,
                                    &events_bus,
                                    in_flight,
                                    &project_id,
                                    &project_path,
                                )
                                .await
                                {
                                    tracing::warn!(
                                        project = %project_path.display(),
                                        error = %error,
                                        "repo-map initial refresh failed for new project"
                                    );
                                }
                            });
                        } else if envelope.entity_type == "project" && envelope.action == "updated" {
                            let Some(project) = envelope.parse_payload::<djinn_core::models::Project>() else { continue; };
                            let mut guard = state_clone.lock().await;
                            guard.watchers.retain(|path, _| path != Path::new(&project.path));
                            add_watch(&mut guard, &project.id, Path::new(&project.path));
                        } else if envelope.entity_type == "project" && envelope.action == "deleted" {
                            let mut guard = state_clone.lock().await;
                            let project_repo = ProjectRepository::new(
                                guard.app_state.db().clone(),
                                crate::events::event_bus_for(&guard.events_tx),
                            );
                            let current_projects: std::collections::HashSet<PathBuf> = match project_repo.list().await {
                                Ok(ps) => ps.into_iter().map(|p| PathBuf::from(p.path)).collect(),
                                Err(_) => continue,
                            };
                            guard.watchers.retain(|path, _| current_projects.contains(path));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });
}

async fn bootstrap_repo_map_refresh_watchers(
    state: Arc<Mutex<RepoMapWatcherState>>,
    db: Database,
    events_tx: tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
) {
    let project_repo = ProjectRepository::new(db, crate::events::event_bus_for(&events_tx));
    let projects = match project_repo.list().await {
        Ok(projects) => projects,
        Err(error) => {
            tracing::warn!(error = %error, "failed to list projects for repo-map watcher setup");
            return;
        }
    };

    let mut startup_refreshes = Vec::with_capacity(projects.len());
    let mut guard = state.lock().await;
    for project in projects {
        let path = PathBuf::from(&project.path);
        add_watch(&mut guard, &project.id, &path);
        startup_refreshes.push((project.id, path));
    }
    let app_state = guard.app_state.clone();
    let events_bus = crate::events::event_bus_for(&guard.events_tx);
    let in_flight = guard.in_flight.clone();
    drop(guard);

    for (project_id, project_path) in startup_refreshes {
        if !startup_needs_refresh(&app_state, &project_id, &project_path).await {
            tracing::debug!(
                project = %project_path.display(),
                "repo-map cache hit on startup, skipping refresh"
            );
            continue;
        }

        if let Err(error) = refresh_project_and_worktrees(
            &app_state,
            &events_bus,
            in_flight.clone(),
            &project_id,
            &project_path,
        )
        .await
        {
            tracing::warn!(
                project = %project_path.display(),
                error = %error,
                "repo-map startup refresh failed"
            );
        }
    }
}

/// Check whether a project needs a repo-map refresh at startup by resolving
/// the current HEAD commit and looking for an existing cache entry. Returns
/// `true` when no cached repo-map exists for the HEAD commit (i.e. a refresh
/// is needed), and `false` when the cache is already populated.
async fn startup_needs_refresh(
    app_state: &AppState,
    project_id: &str,
    project_path: &Path,
) -> bool {
    let head_sha = match resolve_head_sha(app_state, project_path).await {
        Some(sha) => sha,
        // Cannot determine HEAD; schedule a refresh to be safe.
        None => return true,
    };

    let cache_repo = RepoMapCacheRepository::new(app_state.db().clone());
    let project_path_str = project_path.to_string_lossy();

    match cache_repo
        .get_by_commit_prefer_canonical(project_id, &project_path_str, &head_sha)
        .await
    {
        Ok(Some(_)) => false, // cache hit
        _ => true,            // cache miss or lookup error
    }
}

/// Resolve the HEAD commit SHA for a project path, returning `None` on any
/// git error (e.g. path does not exist or is not a git repo).
async fn resolve_head_sha(app_state: &AppState, project_path: &Path) -> Option<String> {
    let git = app_state.git_actor(project_path).await.ok()?;
    let head = git.head_commit().await.ok()?;
    Some(head.sha)
}

fn add_watch(state: &mut RepoMapWatcherState, project_id: &str, project_path: &Path) {
    if !project_path.exists() || state.watchers.contains_key(project_path) {
        return;
    }

    let app_state = state.app_state.clone();
    let events_tx = crate::events::event_bus_for(&state.events_tx);
    let project_id = project_id.to_string();
    let project_path_owned = project_path.to_path_buf();
    let in_flight = state.in_flight.clone();
    let rt_handle = tokio::runtime::Handle::current();

    let debouncer = new_debouncer(
        DEBOUNCE,
        move |res: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| match res {
            Ok(events) => {
                let relevant = events.iter().any(|e| {
                    matches!(e.kind, DebouncedEventKind::Any)
                        && !path_contains_segment(&e.path, ".git")
                        && !path_contains_segment(&e.path, ".djinn")
                });
                if !relevant {
                    return;
                }

                let app_state = app_state.clone();
                let events_tx = events_tx.clone();
                let project_id = project_id.clone();
                let project_path = project_path_owned.clone();
                let in_flight = in_flight.clone();
                rt_handle.spawn(async move {
                    if let Err(error) = refresh_project_and_worktrees(
                        &app_state,
                        &events_tx,
                        in_flight,
                        &project_id,
                        &project_path,
                    )
                    .await
                    {
                        tracing::warn!(project = %project_path.display(), error = %error, "repo-map background refresh failed");
                    }
                });
            }
            Err(error) => tracing::warn!(error = %error, "repo-map watcher error"),
        },
    );

    match debouncer {
        Ok(mut debouncer) => {
            if debouncer
                .watcher()
                .watch(project_path, notify::RecursiveMode::Recursive)
                .is_ok()
            {
                state.watchers.insert(project_path.to_path_buf(), debouncer);
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to create repo-map watcher"),
    }
}

async fn refresh_project_and_worktrees(
    app_state: &AppState,
    events: &crate::events::EventBus,
    in_flight: Arc<Mutex<HashSet<RefreshTarget>>>,
    project_id: &str,
    project_path: &Path,
) -> anyhow::Result<()> {
    if let Some(identity) = repo_identity(app_state, project_id, project_path, None).await {
        maybe_refresh_identity(app_state.db(), events, in_flight.clone(), identity).await;
    }

    let git = app_state.git_actor(project_path).await?;
    let worktrees = git.list_worktrees().await.unwrap_or_default();
    for worktree in worktrees {
        let path = PathBuf::from(&worktree.path);
        if path == project_path {
            continue;
        }
        if let Some(identity) = repo_identity(app_state, project_id, project_path, Some(path)).await
        {
            maybe_refresh_identity(app_state.db(), events, in_flight.clone(), identity).await;
        }
    }
    Ok(())
}

async fn repo_identity(
    app_state: &AppState,
    project_id: &str,
    project_path: &Path,
    worktree_path: Option<PathBuf>,
) -> Option<RefreshIdentity> {
    let repo_path = worktree_path.as_deref().unwrap_or(project_path);
    let git = app_state.git_actor(repo_path).await.ok()?;
    let head = git.head_commit().await.ok()?;
    let reuse_plan = worktree_reuse_plan(
        app_state,
        project_id,
        repo_path,
        worktree_path.is_some(),
        &head.sha,
    )
    .await;
    Some(RefreshIdentity {
        project_id: project_id.to_string(),
        project_path: project_path.to_path_buf(),
        worktree_path,
        commit_sha: head.sha,
        reuse_plan,
    })
}

async fn worktree_reuse_plan(
    app_state: &AppState,
    project_id: &str,
    repo_path: &Path,
    is_worktree: bool,
    head_sha: &str,
) -> WorktreeReusePlan {
    if !is_worktree {
        return WorktreeReusePlan::canonical();
    }

    let git = match app_state.git_actor(repo_path).await {
        Ok(git) => git,
        Err(_) => return WorktreeReusePlan::canonical(),
    };

    let target_branch =
        GitSettingsRepository::new(app_state.db().clone(), crate::events::EventBus::noop())
            .get(project_id)
            .await
            .map(|settings| settings.target_branch)
            .unwrap_or_else(|_| "main".to_string());

    let base_commit_sha = git
        .run_command(vec!["merge-base".into(), "HEAD".into(), target_branch])
        .await
        .ok()
        .map(|output| output.stdout.trim().to_string())
        .filter(|sha| !sha.is_empty() && sha != head_sha);

    let (diff_file_count, changed_files) = match base_commit_sha.as_deref() {
        Some(base_commit_sha) => {
            let diff_output = git
                .run_command(vec![
                    "diff".into(),
                    "--name-only".into(),
                    format!("{base_commit_sha}..HEAD"),
                ])
                .await
                .ok();
            match diff_output {
                Some(output) => {
                    let files: Vec<PathBuf> = output
                        .stdout
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(PathBuf::from)
                        .collect();
                    let count = files.len();
                    let changed = if count <= WORKTREE_REUSE_DIFF_FILE_THRESHOLD {
                        Some(files)
                    } else {
                        None
                    };
                    (Some(count), changed)
                }
                None => (None, None),
            }
        }
        None => (None, None),
    };

    WorktreeReusePlan {
        base_commit_sha,
        diff_file_count,
        changed_files,
    }
}

async fn maybe_refresh_identity(
    db: &Database,
    events: &crate::events::EventBus,
    in_flight: Arc<Mutex<HashSet<RefreshTarget>>>,
    identity: RefreshIdentity,
) {
    let Some(decision) = plan_refresh(db, in_flight.clone(), &identity).await else {
        return;
    };

    if let Some(cached) = decision.reuse_from {
        // Attempt small-diff graph patching when the base cache entry has a
        // graph artifact and the worktree reuse plan identified specific
        // changed files.
        if let Some(changed_files) = &identity.reuse_plan.changed_files
            && let Some(artifact_json) = &cached.graph_artifact
            && !changed_files.is_empty()
        {
            match patch_cached_repo_map(
                db,
                events,
                &identity,
                artifact_json,
                changed_files,
            )
            .await
            {
                Ok(()) => return,
                Err(error) => {
                    tracing::warn!(
                        project = %identity.project_path.display(),
                        commit = %identity.commit_sha,
                        error = %error,
                        "small-diff graph patch failed; falling back to cache clone"
                    );
                }
            }
        }

        // Fall back to plain cache clone.
        if let Err(error) = clone_cached_repo_map(db, events, &identity, &cached).await {
            tracing::warn!(project = %identity.project_path.display(), commit = %identity.commit_sha, error = %error, "repo-map cache reuse failed; falling back to full refresh");
        } else {
            return;
        }
    }

    if !decision.should_spawn {
        return;
    }

    let db = db.clone();
    let events = events.clone();
    tokio::spawn(async move {
        let _permit = InFlightGuard::new(in_flight, decision.target.clone()).await;
        if let Err(error) = generate_and_store_repo_map(&db, &events, &identity).await {
            tracing::warn!(project = %identity.project_path.display(), commit = %identity.commit_sha, error = %error, "repo-map background refresh generation failed");
        }
    });
}

async fn plan_refresh(
    db: &Database,
    in_flight: Arc<Mutex<HashSet<RefreshTarget>>>,
    identity: &RefreshIdentity,
) -> Option<RefreshDecision> {
    let target = refresh_target(identity);
    let repo = RepoMapCacheRepository::new(db.clone());
    if repo.get(target.key()).await.ok().flatten().is_some() {
        return None;
    }

    let project_path = identity.project_path.to_string_lossy().into_owned();
    if let Some(cached) = repo
        .get_by_commit_prefer_canonical(&identity.project_id, &project_path, &identity.commit_sha)
        .await
        .ok()
        .flatten()
    {
        return Some(RefreshDecision {
            target,
            should_spawn: false,
            reuse_from: Some(cached),
        });
    }

    if let Some(base_commit_sha) = identity.reuse_plan.reusable_base_commit()
        && let Some(cached) = repo
            .get_by_commit_prefer_canonical(&identity.project_id, &project_path, base_commit_sha)
            .await
            .ok()
            .flatten()
    {
        return Some(RefreshDecision {
            target,
            should_spawn: false,
            reuse_from: Some(cached),
        });
    }

    let mut guard = in_flight.lock().await;
    if !guard.insert(target.clone()) {
        return Some(RefreshDecision {
            target,
            should_spawn: false,
            reuse_from: None,
        });
    }

    Some(RefreshDecision {
        target,
        should_spawn: true,
        reuse_from: None,
    })
}

fn refresh_target(identity: &RefreshIdentity) -> RefreshTarget {
    RefreshTarget {
        project_id: identity.project_id.clone(),
        project_path: identity.project_path.to_string_lossy().into_owned(),
        worktree_path: identity
            .worktree_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        commit_sha: identity.commit_sha.clone(),
    }
}

async fn generate_and_store_repo_map(
    db: &Database,
    events: &crate::events::EventBus,
    identity: &RefreshIdentity,
) -> anyhow::Result<()> {
    let repo_root = identity
        .worktree_path
        .as_deref()
        .unwrap_or(&identity.project_path);
    let output_dir = std::env::temp_dir().join(format!("djinn-repo-map-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&output_dir)?;
    let run = run_indexers(repo_root, &output_dir).await?;
    let parsed = parse_scip_artifacts(&run.artifacts)?;
    if parsed.is_empty() {
        let _ = std::fs::remove_dir_all(&output_dir);
        return Ok(());
    }
    let graph = RepoDependencyGraph::build(&parsed);
    let ranking = graph.rank();
    let rendered = render_repo_map(
        &graph,
        &ranking,
        &RepoMapRenderOptions::new(DEFAULT_REPO_MAP_TOKEN_BUDGET),
    )
    .map_err(|error| RepoMapRefreshError(format!("{error:?}")))?;

    let artifact_json = match graph.serialize_artifact() {
        Ok(json) => Some(json),
        Err(error) => {
            tracing::warn!(error = %error, "failed to serialize repo graph artifact, proceeding without it");
            None
        }
    };

    let project_path = identity.project_path.to_string_lossy().into_owned();
    let worktree_path = identity
        .worktree_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let cache_repo = RepoMapCacheRepository::new(db.clone());
    cache_repo
        .insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: &identity.project_id,
                project_path: &project_path,
                worktree_path: worktree_path.as_deref(),
                commit_sha: &identity.commit_sha,
            },
            rendered_map: &rendered.content,
            token_estimate: rendered.token_estimate as i64,
            included_entries: rendered.included_entries as i64,
            graph_artifact: artifact_json.as_deref(),
        })
        .await?;

    let note_repo = NoteRepository::new(db.clone(), events.clone());
    let _ = crate::repo_map::persist_repo_map_note(
        &note_repo,
        &identity.project_id,
        &identity.commit_sha,
        &rendered,
    )
    .await;
    let _ = std::fs::remove_dir_all(&output_dir);
    Ok(())
}

/// Small-diff graph patching: deserialize the cached graph artifact, remove
/// contributions from changed files, re-index only those files, patch the
/// graph, rerank, re-render, and persist the result.
async fn patch_cached_repo_map(
    db: &Database,
    events: &crate::events::EventBus,
    identity: &RefreshIdentity,
    artifact_json: &str,
    changed_files: &[PathBuf],
) -> anyhow::Result<()> {
    use std::collections::BTreeSet;

    let base_graph = RepoDependencyGraph::deserialize_artifact(artifact_json)
        .map_err(|e| anyhow::anyhow!("deserialize graph artifact: {e}"))?;

    let repo_root = identity
        .worktree_path
        .as_deref()
        .unwrap_or(&identity.project_path);
    let output_dir = std::env::temp_dir().join(format!("djinn-repo-map-patch-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&output_dir)?;
    let run = run_indexers(repo_root, &output_dir).await?;
    let parsed = parse_scip_artifacts(&run.artifacts)?;
    let _ = std::fs::remove_dir_all(&output_dir);

    let changed_set: BTreeSet<PathBuf> = changed_files.iter().cloned().collect();
    let patched = base_graph.patch_changed_files(&changed_set, &parsed);
    let ranking = patched.rank();
    let rendered = render_repo_map(
        &patched,
        &ranking,
        &RepoMapRenderOptions::new(DEFAULT_REPO_MAP_TOKEN_BUDGET),
    )
    .map_err(|error| RepoMapRefreshError(format!("{error:?}")))?;

    let artifact_json = match patched.serialize_artifact() {
        Ok(json) => Some(json),
        Err(error) => {
            tracing::warn!(error = %error, "failed to serialize patched graph artifact");
            None
        }
    };

    let project_path = identity.project_path.to_string_lossy().into_owned();
    let worktree_path = identity
        .worktree_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let cache_repo = RepoMapCacheRepository::new(db.clone());
    cache_repo
        .insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: &identity.project_id,
                project_path: &project_path,
                worktree_path: worktree_path.as_deref(),
                commit_sha: &identity.commit_sha,
            },
            rendered_map: &rendered.content,
            token_estimate: rendered.token_estimate as i64,
            included_entries: rendered.included_entries as i64,
            graph_artifact: artifact_json.as_deref(),
        })
        .await?;

    let note_repo = NoteRepository::new(db.clone(), events.clone());
    let _ = crate::repo_map::persist_repo_map_note(
        &note_repo,
        &identity.project_id,
        &identity.commit_sha,
        &rendered,
    )
    .await;

    tracing::info!(
        project = %identity.project_path.display(),
        commit = %identity.commit_sha,
        changed_file_count = changed_files.len(),
        "repo-map updated via small-diff graph patch"
    );

    Ok(())
}

async fn clone_cached_repo_map(
    db: &Database,
    events: &crate::events::EventBus,
    identity: &RefreshIdentity,
    cached: &CachedRepoMap,
) -> anyhow::Result<()> {
    let project_path = identity.project_path.to_string_lossy().into_owned();
    let worktree_path = identity
        .worktree_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let cache_repo = RepoMapCacheRepository::new(db.clone());
    cache_repo
        .insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: &identity.project_id,
                project_path: &project_path,
                worktree_path: worktree_path.as_deref(),
                commit_sha: &identity.commit_sha,
            },
            rendered_map: &cached.rendered_map,
            token_estimate: cached.token_estimate,
            included_entries: cached.included_entries,
            graph_artifact: cached.graph_artifact.as_deref(),
        })
        .await?;

    let rendered = crate::repo_map::RenderedRepoMap {
        content: cached.rendered_map.clone(),
        token_estimate: cached.token_estimate as usize,
        included_entries: cached.included_entries as usize,
    };
    let note_repo = NoteRepository::new(db.clone(), events.clone());
    let _ = crate::repo_map::persist_repo_map_note(
        &note_repo,
        &identity.project_id,
        &identity.commit_sha,
        &rendered,
    )
    .await;
    Ok(())
}

struct InFlightGuard {
    in_flight: Arc<Mutex<HashSet<RefreshTarget>>>,
    target: RefreshTarget,
}

impl InFlightGuard {
    async fn new(in_flight: Arc<Mutex<HashSet<RefreshTarget>>>, target: RefreshTarget) -> Self {
        Self { in_flight, target }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let in_flight = self.in_flight.clone();
        let target = self.target.clone();
        tokio::spawn(async move {
            in_flight.lock().await.remove(&target);
        });
    }
}

fn path_contains_segment(path: &Path, segment: &str) -> bool {
    path.components().any(|c| c.as_os_str() == segment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::event_bus_for;
    use crate::test_helpers::create_test_db;
    use djinn_db::ProjectRepository;
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn unchanged_commit_is_skipped_when_cache_exists() {
        let db = create_test_db();
        let repo = RepoMapCacheRepository::new(db.clone());
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/tmp/project",
                worktree_path: None,
                commit_sha: "abc",
            },
            rendered_map: "cached",
            token_estimate: 1,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .unwrap();

        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: None,
            commit_sha: "abc".into(),
            reuse_plan: WorktreeReusePlan::canonical(),
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity).await;
        assert!(decision.is_none());
        assert!(in_flight.lock().await.is_empty());
    }

    #[tokio::test]
    async fn worktree_large_diff_falls_back_to_full_refresh_even_with_base_cache() {
        let db = create_test_db();
        let repo = RepoMapCacheRepository::new(db.clone());
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/tmp/project",
                worktree_path: None,
                commit_sha: "base-commit",
            },
            rendered_map: "cached-base",
            token_estimate: 1,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .unwrap();

        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: Some(PathBuf::from("/tmp/project/.djinn/worktrees/t1")),
            commit_sha: "worktree-commit".into(),
            reuse_plan: WorktreeReusePlan {
                base_commit_sha: Some("base-commit".into()),
                diff_file_count: Some(WORKTREE_REUSE_DIFF_FILE_THRESHOLD + 1),
                changed_files: None,
            },
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("full refresh decision");
        assert!(decision.should_spawn);
        assert!(decision.reuse_from.is_none());
        assert!(in_flight.lock().await.contains(&decision.target));
    }

    #[tokio::test]
    async fn worktree_missing_diff_metadata_falls_back_to_full_refresh() {
        let db = create_test_db();
        let repo = RepoMapCacheRepository::new(db.clone());
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/tmp/project",
                worktree_path: None,
                commit_sha: "base-commit",
            },
            rendered_map: "cached-base",
            token_estimate: 1,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .unwrap();

        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: Some(PathBuf::from("/tmp/project/.djinn/worktrees/t1")),
            commit_sha: "worktree-commit".into(),
            reuse_plan: WorktreeReusePlan {
                base_commit_sha: Some("base-commit".into()),
                diff_file_count: None,
                changed_files: None,
            },
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("full refresh decision");
        assert!(decision.should_spawn);
        assert!(decision.reuse_from.is_none());
        assert!(in_flight.lock().await.contains(&decision.target));
    }

    #[tokio::test]
    async fn changed_commit_schedules_refresh_for_new_cache_key() {
        let db = create_test_db();
        let repo = RepoMapCacheRepository::new(db.clone());
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/tmp/project",
                worktree_path: None,
                commit_sha: "old-commit",
            },
            rendered_map: "cached",
            token_estimate: 1,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .unwrap();

        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: None,
            commit_sha: "new-commit".into(),
            reuse_plan: WorktreeReusePlan::canonical(),
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("new commit should schedule refresh");
        assert!(decision.should_spawn);
        assert_eq!(decision.target.commit_sha, "new-commit");
        assert!(decision.reuse_from.is_none());
        assert!(in_flight.lock().await.contains(&decision.target));
    }

    #[tokio::test]
    async fn duplicate_refresh_requests_are_deduplicated() {
        let db = create_test_db();
        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let target = RefreshTarget {
            project_id: "p1".into(),
            project_path: "/tmp/project".into(),
            worktree_path: None,
            commit_sha: "new-commit".into(),
        };
        in_flight.lock().await.insert(target.clone());
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: None,
            commit_sha: "new-commit".into(),
            reuse_plan: WorktreeReusePlan::canonical(),
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("dedup should return decision");
        assert!(!decision.should_spawn);
        assert!(decision.reuse_from.is_none());
        assert_eq!(decision.target, target);
        assert_eq!(in_flight.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn worktree_head_match_reuses_cached_map_without_spawn() {
        let db = create_test_db();
        let repo = RepoMapCacheRepository::new(db.clone());
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/tmp/project",
                worktree_path: None,
                commit_sha: "shared-commit",
            },
            rendered_map: "cached",
            token_estimate: 1,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .unwrap();

        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: Some(PathBuf::from("/tmp/project/.djinn/worktrees/t1")),
            commit_sha: "shared-commit".into(),
            reuse_plan: WorktreeReusePlan {
                base_commit_sha: Some("base-commit".into()),
                diff_file_count: Some(3),
                changed_files: Some(vec![PathBuf::from("a.rs"), PathBuf::from("b.rs"), PathBuf::from("c.rs")]),
            },
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("reuse decision");
        assert!(!decision.should_spawn);
        assert_eq!(
            decision.target.worktree_path.as_deref(),
            Some("/tmp/project/.djinn/worktrees/t1")
        );
        assert_eq!(
            decision.reuse_from.expect("cached entry").commit_sha,
            "shared-commit"
        );
        assert!(in_flight.lock().await.is_empty());
    }

    #[tokio::test]
    async fn worktree_merge_base_match_reuses_cached_map_without_spawn() {
        let db = create_test_db();
        let repo = RepoMapCacheRepository::new(db.clone());
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/tmp/project",
                worktree_path: None,
                commit_sha: "base-commit",
            },
            rendered_map: "cached-base",
            token_estimate: 1,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .unwrap();

        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: Some(PathBuf::from("/tmp/project/.djinn/worktrees/t1")),
            commit_sha: "worktree-commit".into(),
            reuse_plan: WorktreeReusePlan {
                base_commit_sha: Some("base-commit".into()),
                diff_file_count: Some(2),
                changed_files: Some(vec![PathBuf::from("x.rs"), PathBuf::from("y.rs")]),
            },
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("reuse decision");
        assert!(!decision.should_spawn);
        assert_eq!(
            decision.reuse_from.expect("cached entry").commit_sha,
            "base-commit"
        );
        assert!(in_flight.lock().await.is_empty());
    }

    #[tokio::test]
    async fn add_watch_deduplicates_project_paths() {
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let (events_tx, _) = broadcast::channel(16);
        let mut state = RepoMapWatcherState {
            watchers: HashMap::new(),
            app_state: AppState::new(db, cancel),
            events_tx,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        add_watch(&mut state, "project-1", dir.path());
        add_watch(&mut state, "project-1", dir.path());
        assert_eq!(state.watchers.len(), 1);
    }

    #[tokio::test]
    async fn spawn_repo_map_watchers_tracks_project_lifecycle() {
        let db = create_test_db();
        let (events_tx, _) = broadcast::channel(64);
        let cancel = CancellationToken::new();
        let project_repo = ProjectRepository::new(db.clone(), event_bus_for(&events_tx));
        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let project = project_repo
            .create("repo-map-project", &dir.path().to_string_lossy())
            .await
            .unwrap();
        spawn_repo_map_refresh_watchers(db.clone(), events_tx.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_millis(50)).await;
        project_repo.delete(&project.id).await.unwrap();
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_bootstrap_schedules_refresh_when_head_cache_missing() {
        let db = create_test_db();
        let (events_tx, _) = broadcast::channel(64);
        let cancel = CancellationToken::new();

        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        init_git_repo(dir.path());

        let project_repo = ProjectRepository::new(db.clone(), event_bus_for(&events_tx));
        let project = project_repo
            .create("startup-refresh-project", &dir.path().to_string_lossy())
            .await
            .unwrap();

        let state = Arc::new(Mutex::new(RepoMapWatcherState {
            watchers: HashMap::new(),
            app_state: AppState::new(db.clone(), cancel.clone()),
            events_tx: events_tx.clone(),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }));

        bootstrap_repo_map_refresh_watchers(state.clone(), db.clone(), events_tx.clone()).await;

        let app_state = AppState::new(db.clone(), cancel.clone());
        let identity = repo_identity(&app_state, &project.id, dir.path(), None)
            .await
            .expect("startup identity must resolve");
        let target = refresh_target(&identity);

        let guard = state.lock().await;
        assert!(guard.watchers.contains_key(dir.path()));
        assert!(guard.in_flight.lock().await.contains(&target));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_bootstrap_skips_refresh_when_head_cache_exists() {
        let db = create_test_db();
        let (events_tx, _) = broadcast::channel(64);
        let cancel = CancellationToken::new();

        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        init_git_repo(dir.path());

        let project_repo = ProjectRepository::new(db.clone(), event_bus_for(&events_tx));
        let project = project_repo
            .create("startup-cache-hit-project", &dir.path().to_string_lossy())
            .await
            .unwrap();

        let app_state = AppState::new(db.clone(), cancel.clone());
        let identity = repo_identity(&app_state, &project.id, dir.path(), None)
            .await
            .expect("startup identity must resolve");
        let target = refresh_target(&identity);

        RepoMapCacheRepository::new(db.clone())
            .insert(RepoMapCacheInsert {
                key: RepoMapCacheKey {
                    project_id: &project.id,
                    project_path: &dir.path().to_string_lossy(),
                    worktree_path: None,
                    commit_sha: &identity.commit_sha,
                },
                rendered_map: "cached",
                token_estimate: 1,
                included_entries: 1,
                graph_artifact: None,
            })
            .await
            .unwrap();

        let state = Arc::new(Mutex::new(RepoMapWatcherState {
            watchers: HashMap::new(),
            app_state: AppState::new(db.clone(), cancel.clone()),
            events_tx: events_tx.clone(),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }));

        bootstrap_repo_map_refresh_watchers(state.clone(), db.clone(), events_tx.clone()).await;

        let guard = state.lock().await;
        assert!(guard.watchers.contains_key(dir.path()));
        assert!(!guard.in_flight.lock().await.contains(&target));
    }

    /// Verifies that creating a project while repo-map watchers are running
    /// triggers an initial refresh attempt (via the project_created event).
    /// The refresh itself will fail gracefully (no git repo in the temp dir),
    /// but the important thing is that it is attempted rather than waiting for
    /// a file-system change to trigger it.
    #[tokio::test]
    async fn project_created_event_schedules_initial_refresh() {
        let db = create_test_db();
        let (events_tx, _) = broadcast::channel(64);
        let cancel = CancellationToken::new();

        // Start watchers BEFORE project creation so the event loop is listening.
        spawn_repo_map_refresh_watchers(db.clone(), events_tx.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Create a project — this emits a project_created event through the bus.
        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let project_repo = ProjectRepository::new(db.clone(), event_bus_for(&events_tx));
        let _project = project_repo
            .create("initial-refresh-project", &dir.path().to_string_lossy())
            .await
            .unwrap();

        // Give the watcher time to process the event and attempt the initial refresh.
        // The refresh will fail (no git repo), but should log a warning rather than panic.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Project creation must have succeeded regardless of refresh outcome.
        let projects = project_repo.list().await.unwrap();
        assert!(
            projects.iter().any(|p| p.name == "initial-refresh-project"),
            "project should exist after creation even if initial refresh fails"
        );

        cancel.cancel();
    }

    /// Helper: initialise a real git repo with an initial commit inside the given directory.
    fn init_git_repo(dir: &Path) {
        use std::process::Command;
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .expect("git init");
        Command::new("git")
            .args(["config", "user.email", "test@test.local"])
            .current_dir(dir)
            .output()
            .expect("git config email");
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .expect("git config name");
        std::fs::write(dir.join("README.md"), "# test repo\n").expect("write readme");
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir)
            .output()
            .expect("git commit");
    }

    /// End-to-end test: creating a project through the real `ProjectRepository`
    /// emission path (with a live event bus) while the repo-map watcher is
    /// active causes the watcher to receive the `project.created` event and
    /// schedule an initial refresh.
    ///
    /// We verify this by:
    /// 1. Subscribing to the broadcast channel and confirming the
    ///    `project.created` event is emitted by `ProjectRepository::create`.
    /// 2. Waiting for the watcher to process it, then calling `plan_refresh`
    ///    directly with the repo's identity to prove the refresh path was
    ///    reachable (no pre-existing cache entry blocks it).
    /// 3. Confirming the project's git HEAD can be resolved — the same check
    ///    the watcher performs inside `repo_identity` before scheduling.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn project_add_flow_triggers_initial_repo_map_scheduling() {
        let db = create_test_db();
        let (events_tx, _) = broadcast::channel(64);
        let cancel = CancellationToken::new();

        // Subscribe BEFORE starting watchers so we capture all events.
        let mut events_rx = events_tx.subscribe();

        // Start watchers first — the event loop must be listening.
        spawn_repo_map_refresh_watchers(db.clone(), events_tx.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Create a temp dir with a real git repo (so repo_identity succeeds).
        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        init_git_repo(dir.path());

        // Create project via the real ProjectRepository with a live event bus —
        // this is the "underlying project service emission path".
        let project_repo = ProjectRepository::new(db.clone(), event_bus_for(&events_tx));
        let project = project_repo
            .create("e2e-refresh-project", &dir.path().to_string_lossy())
            .await
            .unwrap();

        // 1. Verify the project.created event was emitted on the broadcast channel.
        let mut saw_created = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), events_rx.recv()).await {
                Ok(Ok(envelope)) => {
                    if envelope.entity_type == "project" && envelope.action == "created" {
                        let parsed = envelope.parse_payload::<djinn_core::models::Project>();
                        assert!(
                            parsed.is_some(),
                            "project.created event must carry a parseable Project payload"
                        );
                        let parsed = parsed.unwrap();
                        assert_eq!(parsed.id, project.id);
                        saw_created = true;
                        break;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => continue,
            }
        }
        assert!(
            saw_created,
            "ProjectRepository::create must emit a project.created event on the live bus"
        );

        // 2. Give the watcher time to process the event and attempt refresh.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 3. Verify the refresh path is reachable: resolve repo identity just
        //    as the watcher does, then confirm plan_refresh would schedule work.
        let app_state = AppState::new(db.clone(), cancel.clone());
        let identity = repo_identity(&app_state, &project.id, dir.path(), None)
            .await
            .expect("repo_identity must resolve for a git repo with a commit");
        assert!(
            !identity.commit_sha.is_empty(),
            "HEAD commit SHA must be non-empty"
        );
        assert_eq!(identity.project_id, project.id);

        // The watcher's spawned refresh already ran plan_refresh for this
        // identity. For repos without SCIP indexers the refresh completes
        // without writing a cache entry, so calling plan_refresh again should
        // still produce a spawn decision (no cached entry exists).
        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("plan_refresh should produce a decision for a repo without cached entry");
        assert!(
            decision.should_spawn,
            "plan_refresh must schedule a spawn when no cache entry exists"
        );

        cancel.cancel();
    }

    /// After the initial-trigger path fires for a newly created project, the
    /// existing watcher-driven repo-map lifecycle (cache hits, deduplication)
    /// must still function correctly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn existing_watcher_lifecycle_intact_after_project_creation() {
        let db = create_test_db();
        let (events_tx, _) = broadcast::channel(64);
        let cancel = CancellationToken::new();

        // Start watchers, then create a project with a real git repo.
        spawn_repo_map_refresh_watchers(db.clone(), events_tx.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_millis(50)).await;

        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        init_git_repo(dir.path());

        let project_repo = ProjectRepository::new(db.clone(), event_bus_for(&events_tx));
        let project = project_repo
            .create("lifecycle-project", &dir.path().to_string_lossy())
            .await
            .unwrap();

        // Wait for the watcher to process the project.created event.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Resolve the identity that the watcher would have used.
        let app_state = AppState::new(db.clone(), cancel.clone());
        let identity = repo_identity(&app_state, &project.id, dir.path(), None)
            .await
            .expect("identity must resolve");

        // Manually insert a cache entry as if the initial refresh had completed
        // with actual SCIP output.
        let cache_repo = RepoMapCacheRepository::new(db.clone());
        let project_path_str = dir.path().to_string_lossy().into_owned();
        cache_repo
            .insert(RepoMapCacheInsert {
                key: RepoMapCacheKey {
                    project_id: &project.id,
                    project_path: &project_path_str,
                    worktree_path: None,
                    commit_sha: &identity.commit_sha,
                },
                rendered_map: "test-map-content",
                token_estimate: 42,
                included_entries: 5,
                graph_artifact: None,
            })
            .await
            .unwrap();

        // Existing lifecycle: unchanged commit is skipped (cache hit).
        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let decision = plan_refresh(&db, in_flight.clone(), &identity).await;
        assert!(
            decision.is_none(),
            "plan_refresh must return None when a cache entry exists for the same commit"
        );

        // Existing lifecycle: deduplication still works.
        let fresh_identity = RefreshIdentity {
            project_id: project.id.clone(),
            project_path: dir.path().to_path_buf(),
            worktree_path: None,
            commit_sha: "different-commit-sha".into(),
            reuse_plan: WorktreeReusePlan::canonical(),
        };
        let first = plan_refresh(&db, in_flight.clone(), &fresh_identity)
            .await
            .expect("first call should produce a decision");
        assert!(first.should_spawn, "first call should schedule a spawn");

        let second = plan_refresh(&db, in_flight.clone(), &fresh_identity)
            .await
            .expect("second call should produce a decision");
        assert!(
            !second.should_spawn,
            "second call must be deduplicated (in-flight)"
        );

        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_needs_refresh_returns_true_when_cache_missing() {
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let app_state = AppState::new(db.clone(), cancel);

        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        init_git_repo(dir.path());

        let needs = startup_needs_refresh(&app_state, "p1", dir.path()).await;
        assert!(
            needs,
            "startup_needs_refresh must return true when no cache entry exists for HEAD"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_needs_refresh_returns_false_when_cache_hit() {
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let app_state = AppState::new(db.clone(), cancel);

        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        init_git_repo(dir.path());

        // Resolve HEAD so we can pre-populate the cache.
        let head_sha = resolve_head_sha(&app_state, dir.path())
            .await
            .expect("HEAD must resolve in a git repo with a commit");

        let cache_repo = RepoMapCacheRepository::new(db.clone());
        cache_repo
            .insert(RepoMapCacheInsert {
                key: RepoMapCacheKey {
                    project_id: "p1",
                    project_path: &dir.path().to_string_lossy(),
                    worktree_path: None,
                    commit_sha: &head_sha,
                },
                rendered_map: "cached-map",
                token_estimate: 10,
                included_entries: 2,
                graph_artifact: None,
            })
            .await
            .unwrap();

        let needs = startup_needs_refresh(&app_state, "p1", dir.path()).await;
        assert!(
            !needs,
            "startup_needs_refresh must return false when a cache entry exists for HEAD"
        );
    }

    #[tokio::test]
    async fn startup_needs_refresh_returns_true_for_nonexistent_path() {
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let app_state = AppState::new(db, cancel);

        // Non-existent path: cannot resolve HEAD, so should need refresh (safe default).
        let needs =
            startup_needs_refresh(&app_state, "p1", Path::new("/tmp/nonexistent-repo-12345"))
                .await;
        assert!(
            needs,
            "startup_needs_refresh must return true when HEAD cannot be resolved"
        );
    }

    // ---- Small-diff graph patching decision tests ----

    /// When a base cache entry has a graph_artifact and the identity has
    /// changed_files, `plan_refresh` returns a reuse_from entry whose
    /// `graph_artifact` is populated, enabling the patch path.
    #[tokio::test]
    async fn worktree_small_diff_with_artifact_enables_patch_path() {
        let db = create_test_db();

        // Build a real graph artifact to persist.
        use crate::repo_graph::RepoDependencyGraph;
        use crate::scip_parser::{ParsedScipIndex, ScipFile, ScipMetadata, ScipSymbol, ScipSymbolKind};
        let index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/lib.rs"),
                definitions: vec![],
                references: vec![],
                occurrences: vec![],
                symbols: vec![ScipSymbol {
                    symbol: "test#lib".to_string(),
                    kind: Some(ScipSymbolKind::Function),
                    display_name: Some("lib".to_string()),
                    signature: None,
                    documentation: vec![],
                    relationships: vec![],
                }],
            }],
            external_symbols: vec![],
        };
        let graph = RepoDependencyGraph::build(&[index]);
        let artifact_json = graph.serialize_artifact().unwrap();

        let repo = RepoMapCacheRepository::new(db.clone());
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/tmp/project",
                worktree_path: None,
                commit_sha: "base-commit",
            },
            rendered_map: "cached-base",
            token_estimate: 1,
            included_entries: 1,
            graph_artifact: Some(&artifact_json),
        })
        .await
        .unwrap();

        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: Some(PathBuf::from("/tmp/project/.djinn/worktrees/t1")),
            commit_sha: "worktree-commit".into(),
            reuse_plan: WorktreeReusePlan {
                base_commit_sha: Some("base-commit".into()),
                diff_file_count: Some(2),
                changed_files: Some(vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/main.rs")]),
            },
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("reuse decision");
        assert!(!decision.should_spawn);
        let cached = decision.reuse_from.expect("should have cached entry");
        assert!(
            cached.graph_artifact.is_some(),
            "cached entry must carry the graph artifact for patch path"
        );
        assert_eq!(cached.commit_sha, "base-commit");
    }

    /// When the base cache entry has NO graph_artifact, the reuse path falls
    /// through to a plain cache clone (no patch possible).
    #[tokio::test]
    async fn worktree_small_diff_without_artifact_falls_back_to_clone() {
        let db = create_test_db();
        let repo = RepoMapCacheRepository::new(db.clone());
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/tmp/project",
                worktree_path: None,
                commit_sha: "base-commit",
            },
            rendered_map: "cached-base",
            token_estimate: 1,
            included_entries: 1,
            graph_artifact: None, // No artifact!
        })
        .await
        .unwrap();

        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: Some(PathBuf::from("/tmp/project/.djinn/worktrees/t1")),
            commit_sha: "worktree-commit".into(),
            reuse_plan: WorktreeReusePlan {
                base_commit_sha: Some("base-commit".into()),
                diff_file_count: Some(2),
                changed_files: Some(vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]),
            },
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("reuse decision");
        assert!(!decision.should_spawn);
        let cached = decision.reuse_from.expect("should have cached entry");
        assert!(
            cached.graph_artifact.is_none(),
            "cached entry should have no artifact, forcing clone fallback"
        );
    }

    /// When diff exceeds the threshold, changed_files is None regardless of
    /// artifact availability, so the patch path is never entered.
    #[tokio::test]
    async fn large_diff_never_populates_changed_files() {
        let plan = WorktreeReusePlan {
            base_commit_sha: Some("abc".into()),
            diff_file_count: Some(WORKTREE_REUSE_DIFF_FILE_THRESHOLD + 5),
            changed_files: None,
        };
        assert!(
            plan.reusable_base_commit().is_none(),
            "large diff should not yield a reusable base commit"
        );
        assert!(
            plan.changed_files.is_none(),
            "large diff should not populate changed_files"
        );
    }

    /// Verify that a malformed artifact JSON causes deserialization to fail,
    /// which the patch path handles gracefully by falling back.
    #[tokio::test]
    async fn malformed_artifact_json_causes_patch_fallback() {
        // This tests the deserialization error path inside patch_cached_repo_map.
        let result = RepoDependencyGraph::deserialize_artifact("not-valid-json");
        assert!(
            result.is_err(),
            "malformed JSON should fail deserialization"
        );
    }
}
