use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::server::AppState;

/// Output bundle of the CPU-bound canonical graph build pipeline,
/// produced on a `spawn_blocking` thread and consumed by the async tail that
/// writes DB caches and installs the in-memory canonical slot.
type CanonicalGraphBuildOutput = (
    crate::repo_graph::RepoDependencyGraph,
    crate::repo_map::RenderedRepoMap,
    Vec<u8>,
    Arc<crate::repo_graph::RepoGraphRanking>,
    Arc<CachedSccs>,
    u64,
    u64,
    u64,
    u64,
    u64,
    usize,
    usize,
);

/// Pre-computed strongly-connected components, one set per `kind_filter`
/// variant the `cycles` op exposes (`None` / `File` / `Symbol`).
pub(crate) struct CachedSccs {
    pub(crate) full: Vec<Vec<petgraph::graph::NodeIndex>>,
    pub(crate) file: Vec<Vec<petgraph::graph::NodeIndex>>,
    pub(crate) symbol: Vec<Vec<petgraph::graph::NodeIndex>>,
}

pub(crate) struct CachedGraph {
    pub(crate) graph: crate::repo_graph::RepoDependencyGraph,
    pub(crate) project_path: PathBuf,
    pub(crate) git_head: String,
    pub(crate) last_warm_at: time::OffsetDateTime,
    pub(crate) pagerank: Arc<crate::repo_graph::RepoGraphRanking>,
    pub(crate) sccs: Arc<CachedSccs>,
}

pub(crate) static GRAPH_CACHE: std::sync::LazyLock<RwLock<Option<CachedGraph>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

pub(crate) static PREVIOUS_GRAPH_CACHE: std::sync::LazyLock<RwLock<Option<CachedGraph>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

#[cfg(test)]
pub(crate) static GRAPH_CACHE_TEST_GUARD: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

pub(crate) fn derive_graph_caches(
    graph: &crate::repo_graph::RepoDependencyGraph,
) -> (Arc<crate::repo_graph::RepoGraphRanking>, Arc<CachedSccs>) {
    use crate::repo_graph::RepoGraphNodeKind;
    let pagerank = Arc::new(graph.rank());
    let sccs = Arc::new(CachedSccs {
        full: graph.strongly_connected_components(None, 2),
        file: graph.strongly_connected_components(Some(RepoGraphNodeKind::File), 2),
        symbol: graph.strongly_connected_components(Some(RepoGraphNodeKind::Symbol), 2),
    });
    (pagerank, sccs)
}

#[allow(dead_code)]
pub(crate) async fn status_snapshot(
    index_tree_path: &Path,
) -> Option<(String, time::OffsetDateTime)> {
    let cache = GRAPH_CACHE.read().await;
    cache.as_ref().and_then(|cached| {
        if cached.project_path == index_tree_path {
            Some((cached.git_head.clone(), cached.last_warm_at))
        } else {
            None
        }
    })
}

#[allow(dead_code)]
pub(crate) async fn current_and_previous_graphs() -> (
    Option<(crate::repo_graph::RepoDependencyGraph, String)>,
    Option<(crate::repo_graph::RepoDependencyGraph, String)>,
) {
    let current = {
        let cache = GRAPH_CACHE.read().await;
        cache
            .as_ref()
            .map(|c| (c.graph.clone(), c.git_head.clone()))
    };
    let previous = {
        let cache = PREVIOUS_GRAPH_CACHE.read().await;
        cache
            .as_ref()
            .map(|c| (c.graph.clone(), c.git_head.clone()))
    };
    (current, previous)
}

pub(crate) async fn ensure_canonical_graph(
    state: &AppState,
    project_id: &str,
    project_root: &Path,
) -> Result<
    (
        crate::index_tree::IndexTreeHandle,
        crate::repo_graph::RepoDependencyGraph,
    ),
    String,
> {
    use djinn_db::{RepoGraphCacheInsert, RepoGraphCacheRepository};

    let mut handle = crate::index_tree::IndexTree::ensure(project_id, project_root)
        .await
        .map_err(|e| format!("ensure index tree: {e}"))?;
    let _ = handle
        .fetch_if_stale(crate::index_tree::DEFAULT_FETCH_COOLDOWN)
        .await;
    let _ = handle.reset_to_origin_main().await;

    let commit_sha = handle.commit_sha().to_string();
    let cache_repo = RepoGraphCacheRepository::new(state.db().clone());

    {
        let cache = GRAPH_CACHE.read().await;
        if let Some(cached) = cache.as_ref()
            && cached.project_path == handle.path()
            && cached.git_head == commit_sha
        {
            return Ok((handle, cached.graph.clone()));
        }
    }

    if let Ok(Some(row)) = cache_repo.get(project_id, &commit_sha).await {
        match load_cached_artifact(row.graph_blob).await {
            Ok((graph, pagerank, sccs)) => {
                install_as_canonical(
                    handle.path().to_path_buf(),
                    commit_sha.clone(),
                    graph.clone(),
                    pagerank,
                    sccs,
                )
                .await;
                return Ok((handle, graph));
            }
            Err(e) => {
                tracing::warn!(
                    project_id = %project_id,
                    commit_sha = %commit_sha,
                    error = %e,
                    "ensure_canonical_graph: stale or unreadable graph_blob; re-indexing"
                );
            }
        }
    }

    let lock = state.indexer_lock();
    let _permit = lock.lock().await;

    {
        let cache = GRAPH_CACHE.read().await;
        if let Some(cached) = cache.as_ref()
            && cached.project_path == handle.path()
            && cached.git_head == commit_sha
        {
            return Ok((handle, cached.graph.clone()));
        }
    }
    if let Ok(Some(row)) = cache_repo.get(project_id, &commit_sha).await {
        match load_cached_artifact(row.graph_blob).await {
            Ok((graph, pagerank, sccs)) => {
                install_as_canonical(
                    handle.path().to_path_buf(),
                    commit_sha.clone(),
                    graph.clone(),
                    pagerank,
                    sccs,
                )
                .await;
                return Ok((handle, graph));
            }
            Err(e) => {
                tracing::warn!(
                    project_id = %project_id,
                    commit_sha = %commit_sha,
                    error = %e,
                    "ensure_canonical_graph: stale or unreadable graph_blob; re-indexing"
                );
            }
        }
    }

    let output_temp = tempfile::Builder::new()
        .prefix("djinn-canonical-graph-")
        .tempdir()
        .map_err(|e| format!("create canonical-graph tempdir: {e}"))?;
    let output_dir = output_temp.path().to_path_buf();
    let target_dir = handle.target_dir().to_path_buf();
    let t_indexers = std::time::Instant::now();
    let run =
        crate::repo_map::run_indexers_already_locked(handle.path(), &output_dir, Some(&target_dir))
            .await
            .map_err(|e| format!("run_indexers: {e}"))?;
    let indexers_ms = t_indexers.elapsed().as_millis() as u64;

    let output_dir_for_blocking = output_dir.clone();
    let artifacts = run.artifacts;
    const SKELETON_TOKEN_BUDGET: usize = 1200;
    let blocking =
        tokio::task::spawn_blocking(move || -> Result<CanonicalGraphBuildOutput, String> {
            let t_parse = std::time::Instant::now();
            let parsed = crate::scip_parser::parse_scip_artifacts(&artifacts)
                .map_err(|e| format!("parse_scip_artifacts: {e}"))?;
            let parse_ms = t_parse.elapsed().as_millis() as u64;
            let _ = std::fs::remove_dir_all(&output_dir_for_blocking);

            let t_build = std::time::Instant::now();
            let graph = crate::repo_graph::RepoDependencyGraph::build(&parsed);
            let build_ms = t_build.elapsed().as_millis() as u64;
            let node_count = graph.node_count();
            let edge_count = graph.edge_count();

            let t_derive = std::time::Instant::now();
            let (pagerank, sccs) = derive_graph_caches(&graph);
            let derive_ms = t_derive.elapsed().as_millis() as u64;

            let t_render = std::time::Instant::now();
            let rendered = crate::repo_map::render_repo_map(
                &graph,
                pagerank.as_ref(),
                &crate::repo_map::RepoMapRenderOptions::new(SKELETON_TOKEN_BUDGET),
            )
            .map_err(|e| format!("render_repo_map: {e:?}"))?;
            let render_ms = t_render.elapsed().as_millis() as u64;

            let t_serial = std::time::Instant::now();
            let serialized = bincode::serialize(&graph.to_artifact())
                .map_err(|e| format!("bincode serialize graph: {e}"))?;
            let serial_ms = t_serial.elapsed().as_millis() as u64;

            Ok((
                graph, rendered, serialized, pagerank, sccs, parse_ms, build_ms, derive_ms,
                render_ms, serial_ms, node_count, edge_count,
            ))
        })
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?;
    let (
        graph,
        rendered,
        serialized_blob,
        pagerank,
        sccs,
        parse_ms,
        build_ms,
        derive_ms,
        render_ms,
        serial_ms,
        node_count,
        edge_count,
    ) = blocking?;

    tracing::info!(
        project_id = %project_id,
        commit_sha = %commit_sha,
        indexers_ms,
        parse_ms,
        build_ms,
        derive_ms,
        render_ms,
        serial_ms,
        node_count,
        edge_count,
        "ensure_canonical_graph: build pipeline complete"
    );

    persist_canonical_skeleton(
        state,
        project_id,
        project_root,
        &commit_sha,
        &graph,
        &rendered,
    )
    .await;

    if let Err(e) = cache_repo
        .upsert(RepoGraphCacheInsert {
            project_id,
            commit_sha: &commit_sha,
            graph_blob: &serialized_blob,
        })
        .await
    {
        tracing::warn!(error = %e, "ensure_canonical_graph: failed to persist graph cache row");
    }

    install_as_canonical(
        handle.path().to_path_buf(),
        commit_sha.clone(),
        graph.clone(),
        pagerank,
        sccs,
    )
    .await;
    Ok((handle, graph))
}

async fn persist_canonical_skeleton(
    state: &AppState,
    project_id: &str,
    project_root: &Path,
    commit_sha: &str,
    graph: &crate::repo_graph::RepoDependencyGraph,
    rendered: &crate::repo_map::RenderedRepoMap,
) {
    use djinn_db::{NoteRepository, RepoMapCacheInsert, RepoMapCacheKey, RepoMapCacheRepository};

    let project_path = project_root.to_string_lossy().into_owned();
    let cache_repo = RepoMapCacheRepository::new(state.db().clone());
    if let Err(e) = cache_repo
        .insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id,
                project_path: &project_path,
                worktree_path: None,
                commit_sha,
            },
            rendered_map: &rendered.content,
            token_estimate: rendered.token_estimate as i64,
            included_entries: rendered.included_entries as i64,
            graph_artifact: graph.serialize_artifact().ok().as_deref(),
        })
        .await
    {
        tracing::warn!(
            project_id = %project_id,
            commit_sha = %commit_sha,
            error = %e,
            "persist_canonical_skeleton: repo_map_cache insert failed"
        );
    }

    let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());
    if let Err(e) =
        crate::repo_map::persist_repo_map_note(&note_repo, project_id, commit_sha, rendered).await
    {
        tracing::warn!(
            project_id = %project_id,
            commit_sha = %commit_sha,
            error = %e,
            "persist_canonical_skeleton: repo_map note persist failed"
        );
    }
}

pub(crate) async fn canonical_graph_cache_has_entry_for(index_tree_path: &Path) -> bool {
    let cache = GRAPH_CACHE.read().await;
    cache
        .as_ref()
        .is_some_and(|cached| cached.project_path == index_tree_path)
}

pub(crate) async fn canonical_graph_cache_pinned_commit_for(
    index_tree_path: &Path,
) -> Option<String> {
    let cache = GRAPH_CACHE.read().await;
    cache
        .as_ref()
        .filter(|cached| cached.project_path == index_tree_path)
        .map(|cached| cached.git_head.clone())
}

pub(crate) async fn canonical_graph_count_commits_since(
    project_root: &Path,
    pinned_commit: &str,
) -> Option<u64> {
    count_commits_since(project_root, pinned_commit).await
}

async fn install_as_canonical(
    project_path: PathBuf,
    git_head: String,
    graph: crate::repo_graph::RepoDependencyGraph,
    pagerank: Arc<crate::repo_graph::RepoGraphRanking>,
    sccs: Arc<CachedSccs>,
) {
    let mut cache = GRAPH_CACHE.write().await;
    let old = cache.take();
    if let Some(prior) = old {
        let mut previous = PREVIOUS_GRAPH_CACHE.write().await;
        *previous = Some(prior);
    }
    *cache = Some(CachedGraph {
        graph,
        project_path,
        git_head,
        last_warm_at: time::OffsetDateTime::now_utc(),
        pagerank,
        sccs,
    });
}

async fn load_cached_artifact(
    blob: Vec<u8>,
) -> Result<
    (
        crate::repo_graph::RepoDependencyGraph,
        Arc<crate::repo_graph::RepoGraphRanking>,
        Arc<CachedSccs>,
    ),
    String,
> {
    tokio::task::spawn_blocking(move || -> Result<_, String> {
        let artifact: crate::repo_graph::RepoGraphArtifact =
            bincode::deserialize(&blob).map_err(|e| format!("deserialize graph: {e}"))?;
        let graph = crate::repo_graph::RepoDependencyGraph::from_artifact(&artifact);
        let (pagerank, sccs) = derive_graph_caches(&graph);
        Ok((graph, pagerank, sccs))
    })
    .await
    .map_err(|e| format!("spawn_blocking join: {e}"))?
}

const GRAPH_NOT_WARMED_ERR: &str =
    "canonical graph not warmed yet — will populate on the next Planner patrol or Architect spike";

pub(crate) async fn read_cached_canonical_graph(
    project_path: &str,
) -> Result<
    (
        crate::repo_graph::RepoDependencyGraph,
        Arc<crate::repo_graph::RepoGraphRanking>,
        Arc<CachedSccs>,
    ),
    String,
> {
    let (_project_root, index_tree_path) = normalize_graph_query_paths(project_path);
    let cache = GRAPH_CACHE.read().await;
    let cached = cache
        .as_ref()
        .filter(|c| c.project_path == index_tree_path)
        .ok_or_else(|| GRAPH_NOT_WARMED_ERR.to_string())?;
    Ok((
        cached.graph.clone(),
        cached.pagerank.clone(),
        cached.sccs.clone(),
    ))
}

pub(crate) async fn build_graph_for_project(
    state: &AppState,
    project_path: &str,
) -> Result<crate::repo_graph::RepoDependencyGraph, String> {
    match read_cached_canonical_graph(project_path).await {
        Ok((graph, _pagerank, _sccs)) => Ok(graph),
        Err(e) => {
            if e == GRAPH_NOT_WARMED_ERR {
                maybe_kick_background_warm(state, project_path);
            }
            Err(e)
        }
    }
}

pub(crate) async fn build_graph_with_caches_for_project(
    state: &AppState,
    project_path: &str,
) -> Result<
    (
        crate::repo_graph::RepoDependencyGraph,
        Arc<crate::repo_graph::RepoGraphRanking>,
        Arc<CachedSccs>,
    ),
    String,
> {
    match read_cached_canonical_graph(project_path).await {
        Ok(triple) => Ok(triple),
        Err(e) => {
            if e == GRAPH_NOT_WARMED_ERR {
                maybe_kick_background_warm(state, project_path);
            }
            Err(e)
        }
    }
}

fn maybe_kick_background_warm(state: &AppState, project_path: &str) {
    let (project_root, _index_tree_path) = normalize_graph_query_paths(project_path);
    let state = state.clone();
    let project_path_owned = project_path.to_string();
    tokio::spawn(async move {
        let project_repo = djinn_db::ProjectRepository::new(state.db().clone(), state.event_bus());
        let project_id = match project_repo
            .resolve_id_by_path_fuzzy(&project_path_owned)
            .await
        {
            Ok(Some(id)) => id,
            Ok(None) => {
                tracing::debug!(
                    project_path = %project_path_owned,
                    "maybe_kick_background_warm: project not registered, skipping warm"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    project_path = %project_path_owned,
                    error = %e,
                    "maybe_kick_background_warm: project lookup failed"
                );
                return;
            }
        };

        if !state.try_claim_canonical_warm_slot(&project_id) {
            tracing::debug!(
                project_id = %project_id,
                "maybe_kick_background_warm: warm already in flight, coalescing"
            );
            return;
        }

        tracing::info!(
            project_id = %project_id,
            project_path = %project_root.display(),
            "maybe_kick_background_warm: cold-cache trigger, spawning background warm"
        );
        let started = std::time::Instant::now();
        let result = ensure_canonical_graph(&state, &project_id, &project_root).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok((handle, graph)) => {
                tracing::info!(
                    project_id = %project_id,
                    elapsed_ms,
                    commit_sha = %handle.commit_sha(),
                    node_count = graph.node_count(),
                    edge_count = graph.edge_count(),
                    "maybe_kick_background_warm: background warm complete"
                );
            }
            Err(e) => {
                tracing::warn!(
                    project_id = %project_id,
                    elapsed_ms,
                    error = %e,
                    "maybe_kick_background_warm: background warm failed"
                );
            }
        }
        state.release_canonical_warm_slot(&project_id);
    });
}

async fn count_commits_since(project_root: &Path, pinned_commit: &str) -> Option<u64> {
    let output = tokio::process::Command::new("git")
        .current_dir(project_root)
        .args([
            "rev-list",
            "--count",
            &format!("{pinned_commit}..origin/main"),
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    raw.trim().parse::<u64>().ok()
}

pub(crate) fn normalize_graph_query_paths(project_path: &str) -> (PathBuf, PathBuf) {
    let requested = PathBuf::from(project_path);
    let is_index_tree = requested.file_name() == Some(std::ffi::OsStr::new("_index"))
        && requested.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("worktrees"))
        && requested
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            == Some(std::ffi::OsStr::new(".djinn"));

    if is_index_tree
        && let Some(project_root) = requested
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
    {
        return (project_root.to_path_buf(), requested);
    }

    let project_root = requested;
    let index_tree_path = djinn_core::index_tree::index_tree_path(&project_root);
    (project_root, index_tree_path)
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) async fn install_test_graphs(
    project_path: &Path,
    previous: Option<crate::repo_graph::RepoDependencyGraph>,
    current: crate::repo_graph::RepoDependencyGraph,
) {
    {
        let mut prev_cache = PREVIOUS_GRAPH_CACHE.write().await;
        *prev_cache = previous.map(|graph| {
            let (pagerank, sccs) = derive_graph_caches(&graph);
            CachedGraph {
                graph,
                project_path: project_path.to_path_buf(),
                git_head: "previous".into(),
                last_warm_at: time::OffsetDateTime::now_utc(),
                pagerank,
                sccs,
            }
        });
    }
    {
        let (pagerank, sccs) = derive_graph_caches(&current);
        let mut cache = GRAPH_CACHE.write().await;
        *cache = Some(CachedGraph {
            graph: current,
            project_path: project_path.to_path_buf(),
            git_head: "current".into(),
            last_warm_at: time::OffsetDateTime::now_utc(),
            pagerank,
            sccs,
        });
    }
}

#[cfg(test)]
pub(crate) async fn clear_test_caches() {
    {
        let mut cache = GRAPH_CACHE.write().await;
        *cache = None;
    }
    {
        let mut cache = PREVIOUS_GRAPH_CACHE.write().await;
        *cache = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::create_test_db;
    use djinn_db::{
        ProjectRepository, RepoGraphCacheInsert, RepoGraphCacheRepository, RepoMapCacheRepository,
    };
    use tokio_util::sync::CancellationToken;

    async fn make_project(tmp: &std::path::Path) -> std::path::PathBuf {
        let project_root = tmp.join("repo");
        tokio::fs::create_dir_all(&project_root).await.unwrap();
        let run = |args: &[&str]| {
            let pr = project_root.clone();
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            async move {
                tokio::process::Command::new("git")
                    .current_dir(&pr)
                    .args(&args)
                    .output()
                    .await
                    .unwrap()
            }
        };
        run(&["init", "-q", "-b", "main"]).await;
        run(&["config", "user.email", "t@t"]).await;
        run(&["config", "user.name", "t"]).await;
        tokio::fs::write(project_root.join("a.txt"), "hi")
            .await
            .unwrap();
        run(&["add", "a.txt"]).await;
        run(&["commit", "-q", "-m", "init"]).await;
        project_root
    }

    #[tokio::test]
    async fn ensure_canonical_graph_serves_cache_hit_without_running_indexer() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = make_project(tmp.path()).await;
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let state = crate::server::AppState::new(db.clone(), cancel);
        let proj_repo = ProjectRepository::new(db.clone(), state.event_bus());
        let project = proj_repo
            .create("test-canonical", project_root.to_string_lossy().as_ref())
            .await
            .expect("create project");

        let head_out = tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project_root)
            .output()
            .await
            .unwrap();
        let head_sha = String::from_utf8_lossy(&head_out.stdout).trim().to_string();

        let graph = crate::mcp_bridge::graph_bridge_tests::build_test_graph();
        let blob = bincode::serialize(&graph.to_artifact()).expect("serialize fixture graph");
        let cache_repo = RepoGraphCacheRepository::new(db.clone());
        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: &head_sha,
                graph_blob: &blob,
            })
            .await
            .expect("seed cache");

        let result = ensure_canonical_graph(&state, &project.id, &project_root).await;
        assert!(result.is_ok(), "expected cache-hit success, got {result:?}");
        let (_handle, returned_graph) = result.unwrap();
        assert_eq!(returned_graph.node_count(), graph.node_count());
    }

    #[tokio::test]
    async fn ensure_canonical_graph_treats_stale_blob_as_cache_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = make_project(tmp.path()).await;
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let state = crate::server::AppState::new(db.clone(), cancel);
        let proj_repo = ProjectRepository::new(db.clone(), state.event_bus());
        let project = proj_repo
            .create(
                "test-canonical-stale",
                project_root.to_string_lossy().as_ref(),
            )
            .await
            .expect("create project");

        let head_out = tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project_root)
            .output()
            .await
            .unwrap();
        let head_sha = String::from_utf8_lossy(&head_out.stdout).trim().to_string();

        let garbage = b"this is definitely not a bincoded RepoDependencyGraph";
        RepoGraphCacheRepository::new(db.clone())
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: &head_sha,
                graph_blob: garbage,
            })
            .await
            .expect("seed cache");

        let result = ensure_canonical_graph(&state, &project.id, &project_root).await;
        if let Err(msg) = &result {
            assert!(
                !msg.contains("deserialize cached graph")
                    && !msg.contains("graph_blob is not valid UTF-8"),
                "stale blob bubbled cache-path error instead of falling through: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn cache_only_readers_serve_cached_graph_and_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = make_project(tmp.path()).await;
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let state = crate::server::AppState::new(db.clone(), cancel);
        let _ = ProjectRepository::new(db.clone(), state.event_bus())
            .create(
                "test-cache-only-readers",
                project_root.to_string_lossy().as_ref(),
            )
            .await
            .expect("create project");

        let index_tree_path = project_root.join(".djinn").join("worktrees").join("_index");
        let stale_sha = "0000000000000000000000000000000000000000".to_string();
        let expected_node_count = {
            let graph = crate::mcp_bridge::graph_bridge_tests::build_test_graph();
            let node_count = graph.node_count();
            let (pagerank, sccs) = derive_graph_caches(&graph);
            let mut cache = GRAPH_CACHE.write().await;
            *cache = Some(CachedGraph {
                graph,
                project_path: index_tree_path.clone(),
                git_head: stale_sha,
                last_warm_at: time::OffsetDateTime::now_utc(),
                pagerank,
                sccs,
            });
            node_count
        };

        let project_root_str = project_root.to_string_lossy().into_owned();
        let graph_only = build_graph_for_project(&state, &project_root_str)
            .await
            .expect("cache-only reader must succeed without warming");
        let (graph_with_caches, pagerank, _sccs) =
            build_graph_with_caches_for_project(&state, &project_root_str)
                .await
                .expect("cache-only reader (with caches) must succeed without warming");

        clear_test_caches().await;

        assert_eq!(graph_only.node_count(), expected_node_count);
        assert_eq!(graph_with_caches.node_count(), expected_node_count);
        assert_eq!(pagerank.nodes.len(), expected_node_count);
    }

    #[tokio::test]
    async fn install_and_persist_paths_share_the_service_seam() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = make_project(tmp.path()).await;
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let state = crate::server::AppState::new(db.clone(), cancel);
        let proj_repo = ProjectRepository::new(db.clone(), state.event_bus());
        let project = proj_repo
            .create(
                "test-canonical-persist",
                project_root.to_string_lossy().as_ref(),
            )
            .await
            .expect("create project");

        let head_out = tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project_root)
            .output()
            .await
            .unwrap();
        let head_sha = String::from_utf8_lossy(&head_out.stdout).trim().to_string();

        let graph = crate::mcp_bridge::graph_bridge_tests::build_test_graph();
        let rendered = crate::repo_map::render_repo_map(
            &graph,
            &graph.rank(),
            &crate::repo_map::RepoMapRenderOptions::new(1200),
        )
        .expect("render repo map");

        persist_canonical_skeleton(
            &state,
            &project.id,
            &project_root,
            &head_sha,
            &graph,
            &rendered,
        )
        .await;

        let cache_repo = RepoMapCacheRepository::new(db.clone());
        let project_path = project_root.to_string_lossy().into_owned();
        let row = cache_repo
            .get(djinn_db::RepoMapCacheKey {
                project_id: &project.id,
                project_path: &project_path,
                worktree_path: None,
                commit_sha: &head_sha,
            })
            .await
            .expect("lookup repo_map_cache")
            .expect("repo_map_cache row inserted");
        assert_eq!(row.commit_sha, head_sha);
        assert_eq!(row.rendered_map, rendered.content);
    }
}
