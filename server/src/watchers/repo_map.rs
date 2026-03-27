use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
    Database, NoteRepository, ProjectRepository, RepoMapCacheInsert, RepoMapCacheKey,
    RepoMapCacheRepository,
};

const DEBOUNCE: Duration = Duration::from_secs(2);
const DEFAULT_REPO_MAP_TOKEN_BUDGET: usize = 1200;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RefreshDecision {
    target: RefreshTarget,
    should_spawn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoMapRefreshError(String);

impl fmt::Display for RepoMapRefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RepoMapRefreshError {}

/// Phase-1 worktree reuse policy from ADR-043:
/// - directly reuse a canonical/base cached map when refresh planning finds one for the
///   current commit lineage (for example the primary repo checkout or a merge-base-backed
///   canonical entry), because branch-local ranking stays close enough for small worktree diffs;
/// - fall back to a full index whenever no suitable cached base map exists;
/// - defer diff-threshold heuristics and graph patching to a later wave once reuse semantics
///   are validated end-to-end.
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
        {
            let project_repo =
                ProjectRepository::new(db.clone(), crate::events::event_bus_for(&events_tx));
            match project_repo.list().await {
                Ok(projects) => {
                    let mut guard = state_clone.lock().await;
                    for project in projects {
                        let path = PathBuf::from(&project.path);
                        add_watch(&mut guard, &project.id, &path);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to list projects for repo-map watcher setup")
                }
            }
        }

        let mut events_rx = events_tx.subscribe();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = events_rx.recv() => match recv {
                    Ok(envelope) => {
                        if envelope.entity_type == "project" && envelope.action == "created" {
                            let Some(project) = envelope.parse_payload::<djinn_core::models::Project>() else { continue; };
                            let mut guard = state_clone.lock().await;
                            add_watch(&mut guard, &project.id, Path::new(&project.path));
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
    Some(RefreshIdentity {
        project_id: project_id.to_string(),
        project_path: project_path.to_path_buf(),
        worktree_path,
        commit_sha: head.sha,
    })
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

    let mut guard = in_flight.lock().await;
    if !guard.insert(target.clone()) {
        return Some(RefreshDecision {
            target,
            should_spawn: false,
        });
    }

    Some(RefreshDecision {
        target,
        should_spawn: true,
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
        })
        .await
        .unwrap();

        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: None,
            commit_sha: "abc".into(),
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity).await;
        assert!(decision.is_none());
        assert!(in_flight.lock().await.is_empty());
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
        })
        .await
        .unwrap();

        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let identity = RefreshIdentity {
            project_id: "p1".into(),
            project_path: PathBuf::from("/tmp/project"),
            worktree_path: None,
            commit_sha: "new-commit".into(),
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("new commit should schedule refresh");
        assert!(decision.should_spawn);
        assert_eq!(decision.target.commit_sha, "new-commit");
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
        };

        let decision = plan_refresh(&db, in_flight.clone(), &identity)
            .await
            .expect("dedup should return decision");
        assert!(!decision.should_spawn);
        assert_eq!(decision.target, target);
        assert_eq!(in_flight.lock().await.len(), 1);
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
}
