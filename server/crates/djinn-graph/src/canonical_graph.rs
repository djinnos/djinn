use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::WarmContext;
use crate::architect::ArchitectWarmToken;

/// Output bundle of the CPU-bound canonical graph build pipeline,
/// produced on a `spawn_blocking` thread and consumed by the async tail that
/// writes DB caches and installs the in-memory canonical slot.
type CanonicalGraphBuildOutput = (
    crate::repo_graph::RepoDependencyGraph,
    Vec<u8>,
    Arc<crate::repo_graph::RepoGraphRanking>,
    Arc<CachedSccs>,
    u64,
    u64,
    u64,
    u64,
    usize,
    usize,
);

/// Pre-computed strongly-connected components, one set per `kind_filter`
/// variant the `cycles` op exposes (`None` / `File` / `Symbol`).
pub struct CachedSccs {
    pub full: Vec<Vec<petgraph::graph::NodeIndex>>,
    pub file: Vec<Vec<petgraph::graph::NodeIndex>>,
    pub symbol: Vec<Vec<petgraph::graph::NodeIndex>>,
}

pub struct CachedGraph {
    pub graph: crate::repo_graph::RepoDependencyGraph,
    pub project_path: PathBuf,
    pub git_head: String,
    pub pagerank: Arc<crate::repo_graph::RepoGraphRanking>,
    pub sccs: Arc<CachedSccs>,
}

pub static GRAPH_CACHE: std::sync::LazyLock<RwLock<Option<CachedGraph>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

pub fn derive_graph_caches(
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

/// Public entrypoint invoked by `djinn-agent-worker warm-graph
/// <project_id>` (Phase 3 PR 8 §6.4).  The caller provides a minimal
/// [`WarmContext`] (DB + event bus + indexer lock); this function
/// resolves the project's working root from the DB, then drives a
/// single [`ensure_canonical_graph`] pass.  Returns a human-readable
/// error on failure so the subcommand can exit(1) with a useful
/// message.
///
/// This is intentionally separate from the daemon boot path — the warm
/// Pod is short-lived and has no inbound traffic, so spinning up the
/// HTTP server + coordinator + RPC listener would be ~2.5s of wasted
/// latency per warm run.
pub async fn run_warm_graph_command<C: WarmContext>(
    ctx: &C,
    project_id: &str,
    token: ArchitectWarmToken,
) -> anyhow::Result<()> {
    use djinn_db::ProjectRepository;

    let repo = ProjectRepository::new(ctx.db().clone(), ctx.event_bus());
    let project = repo
        .get(project_id)
        .await
        .map_err(|e| anyhow::anyhow!("lookup project {project_id}: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("project {project_id} not found"))?;
    // When `DJINN_PROJECT_ROOT` is set (K8s warm path) the caller has
    // already cloned the mirror into a Pod-local workspace — the DB's
    // `projects.path` points at a server-local directory that isn't
    // available in the warm Pod, so we honor the override.
    let project_root = match std::env::var("DJINN_PROJECT_ROOT") {
        Ok(v) if !v.is_empty() => {
            tracing::info!(
                project_id,
                project_root = %v,
                "run_warm_graph_command: DJINN_PROJECT_ROOT override in effect"
            );
            PathBuf::from(v)
        }
        _ => djinn_core::paths::project_dir(&project.github_owner, &project.github_repo),
    };
    tracing::info!(
        project_id,
        project_root = %project_root.display(),
        "run_warm_graph_command: starting warm pipeline"
    );
    let started = std::time::Instant::now();
    let (_handle, graph) = ensure_canonical_graph(ctx, project_id, &project_root, token)
        .await
        .map_err(|e| anyhow::anyhow!("ensure_canonical_graph failed: {e}"))?;
    tracing::info!(
        project_id,
        elapsed_ms = started.elapsed().as_millis() as u64,
        node_count = graph.node_count(),
        edge_count = graph.edge_count(),
        "run_warm_graph_command: warm pipeline complete"
    );
    Ok(())
}

pub async fn ensure_canonical_graph<C: WarmContext>(
    ctx: &C,
    project_id: &str,
    project_root: &Path,
    // Architect-only capability token.  Consumed (taken by value) so each
    // warm call has to justify itself at the type system level; the token
    // carries no data so the move is free.  Construct via
    // `djinn_graph::architect::ArchitectWarmToken::new` on a sanctioned
    // warm path only.
    _token: ArchitectWarmToken,
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
    let cache_repo = RepoGraphCacheRepository::new(ctx.db().clone());

    {
        let cache = GRAPH_CACHE.read().await;
        if let Some(cached) = cache.as_ref()
            && cached.project_path == handle.path()
            && cached.git_head == commit_sha
        {
            ingest_coupling_best_effort(ctx, project_id, handle.path()).await;
            spawn_chunk_and_embed_best_effort(ctx, project_id, handle.path(), &cached.graph);
            return Ok((handle, cached.graph.clone()));
        }
    }

    if let Ok(Some(row)) = cache_repo.get(project_id, &commit_sha).await {
        match load_cached_artifact(row.graph_blob).await {
            Ok((graph, pagerank, sccs)) => {
                install_as_canonical(
                    ctx,
                    project_id,
                    handle.path().to_path_buf(),
                    commit_sha.clone(),
                    graph.clone(),
                    pagerank,
                    sccs,
                )
                .await;
                ingest_coupling_best_effort(ctx, project_id, handle.path()).await;
                spawn_chunk_and_embed_best_effort(ctx, project_id, handle.path(), &graph);
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

    let lock = ctx.indexer_lock();
    let _permit = lock.lock().await;

    {
        let cache = GRAPH_CACHE.read().await;
        if let Some(cached) = cache.as_ref()
            && cached.project_path == handle.path()
            && cached.git_head == commit_sha
        {
            ingest_coupling_best_effort(ctx, project_id, handle.path()).await;
            spawn_chunk_and_embed_best_effort(ctx, project_id, handle.path(), &cached.graph);
            return Ok((handle, cached.graph.clone()));
        }
    }
    if let Ok(Some(row)) = cache_repo.get(project_id, &commit_sha).await {
        match load_cached_artifact(row.graph_blob).await {
            Ok((graph, pagerank, sccs)) => {
                install_as_canonical(
                    ctx,
                    project_id,
                    handle.path().to_path_buf(),
                    commit_sha.clone(),
                    graph.clone(),
                    pagerank,
                    sccs,
                )
                .await;
                ingest_coupling_best_effort(ctx, project_id, handle.path()).await;
                spawn_chunk_and_embed_best_effort(ctx, project_id, handle.path(), &graph);
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

    let temp_base = std::env::current_dir()
        .map_err(|e| format!("resolve current dir for canonical-graph tempdir: {e}"))?
        .join("target")
        .join("test-tmp");
    std::fs::create_dir_all(&temp_base)
        .map_err(|e| format!("create canonical-graph tempdir base: {e}"))?;
    let output_temp = tempfile::Builder::new()
        .prefix("djinn-canonical-graph-")
        .tempdir_in(&temp_base)
        .map_err(|e| format!("create canonical-graph tempdir: {e}"))?;
    let output_dir = output_temp.path().to_path_buf();
    let target_dir = handle.target_dir().to_path_buf();

    // Phase 3 PR 8: ask the DB for the detected stack and filter the SCIP
    // indexer set to languages the project actually uses. Falls back to
    // running every indexer when no stack has been persisted yet (fresh
    // project, or a pre-PR-2 deployment).
    let stack_filter = resolve_stack_indexer_filter(ctx, project_id).await;

    let t_indexers = std::time::Instant::now();
    let run = crate::scip_indexer::run_indexers_already_locked(
        handle.path(),
        &output_dir,
        Some(&target_dir),
        stack_filter.as_deref(),
    )
    .await
    .map_err(|e| format!("run_indexers: {e}"))?;
    let indexers_ms = t_indexers.elapsed().as_millis() as u64;

    let output_dir_for_blocking = output_dir.clone();
    let artifacts = run.artifacts;
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

            let t_serial = std::time::Instant::now();
            let serialized = bincode::serialize(&graph.to_artifact())
                .map_err(|e| format!("bincode serialize graph: {e}"))?;
            let serial_ms = t_serial.elapsed().as_millis() as u64;

            Ok((
                graph, serialized, pagerank, sccs, parse_ms, build_ms, derive_ms, serial_ms,
                node_count, edge_count,
            ))
        })
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?;
    let (
        graph,
        serialized_blob,
        pagerank,
        sccs,
        parse_ms,
        build_ms,
        derive_ms,
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
        serial_ms,
        node_count,
        edge_count,
        "ensure_canonical_graph: build pipeline complete"
    );

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

    ingest_coupling_best_effort(ctx, project_id, handle.path()).await;

    install_as_canonical(
        ctx,
        project_id,
        handle.path().to_path_buf(),
        commit_sha.clone(),
        graph.clone(),
        pagerank,
        sccs,
    )
    .await;
    spawn_chunk_and_embed_best_effort(ctx, project_id, handle.path(), &graph);
    Ok((handle, graph))
}

/// Fire the PR B3 chunk-and-embed pipeline on a detached `tokio::spawn`
/// when the warm context exposes both an embedding provider and a
/// vector store. Skipped when either is `None` (warm worker / tests
/// that don't ship the embedding model) or when the
/// `DJINN_CODE_CHUNKS_BACKEND` flag is unset (default).
fn spawn_chunk_and_embed_best_effort<C: WarmContext>(
    ctx: &C,
    project_id: &str,
    project_root: &Path,
    graph: &crate::repo_graph::RepoDependencyGraph,
) {
    let (Some(embeddings), Some(vector_store)) =
        (ctx.code_chunk_embeddings(), ctx.code_chunk_vector_store())
    else {
        return;
    };
    crate::chunk_and_embed::spawn_chunk_and_embed_pass(
        ctx.db().clone(),
        embeddings,
        vector_store,
        Arc::new(graph.clone()),
        project_id.to_string(),
        project_root.to_path_buf(),
    );
}

/// Keep the per-project commit-coupling index current. Non-fatal on
/// failure — the canonical graph succeeding matters more than coupling
/// data being fresh. Called from every return site of
/// [`ensure_canonical_graph`] so projects that only ever hit the cache
/// still feed the coupling table.
async fn ingest_coupling_best_effort<C: WarmContext>(
    ctx: &C,
    project_id: &str,
    project_root: &Path,
) {
    if let Err(e) =
        crate::coupling_index::ingest_new_commits(ctx.db(), project_id, project_root).await
    {
        tracing::warn!(
            project_id = %project_id,
            error = %e,
            "ensure_canonical_graph: coupling ingest failed"
        );
    }
}

pub async fn canonical_graph_cache_has_entry_for(index_tree_path: &Path) -> bool {
    let cache = GRAPH_CACHE.read().await;
    cache
        .as_ref()
        .is_some_and(|cached| cached.project_path == index_tree_path)
}

pub async fn canonical_graph_cache_pinned_commit_for(
    index_tree_path: &Path,
) -> Option<String> {
    let cache = GRAPH_CACHE.read().await;
    cache
        .as_ref()
        .filter(|cached| cached.project_path == index_tree_path)
        .map(|cached| cached.git_head.clone())
}

pub async fn canonical_graph_count_commits_since(
    project_root: &Path,
    pinned_commit: &str,
) -> Option<u64> {
    count_commits_since(project_root, pinned_commit).await
}

async fn install_as_canonical<C: WarmContext>(
    _ctx: &C,
    _project_id: &str,
    project_path: PathBuf,
    git_head: String,
    graph: crate::repo_graph::RepoDependencyGraph,
    pagerank: Arc<crate::repo_graph::RepoGraphRanking>,
    sccs: Arc<CachedSccs>,
) {
    let mut cache = GRAPH_CACHE.write().await;
    *cache = Some(CachedGraph {
        graph,
        project_path,
        git_head,
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
    "canonical graph not warmed yet — K8s graph warmer will populate it once the project's devcontainer image is ready";

/// Server-side read-only load: return the canonical graph for the given
/// `project_id` + `project_path`.
///
/// Tries the in-process RAM cache first; on miss, deserializes the most
/// recent entry from `repo_graph_cache` and installs it in RAM. Never
/// rebuilds — that is exclusively the K8s graph warmer's job (the warm
/// Pod runs `djinn-agent-worker warm-graph <project_id>` which goes through
/// [`ensure_canonical_graph`]).
pub async fn load_canonical_graph<C: WarmContext>(
    ctx: &C,
    project_id: &str,
    project_path: &str,
) -> Result<
    (
        crate::repo_graph::RepoDependencyGraph,
        Arc<crate::repo_graph::RepoGraphRanking>,
        Arc<CachedSccs>,
    ),
    String,
> {
    use djinn_db::RepoGraphCacheRepository;

    let (_project_root, index_tree_path) = normalize_graph_query_paths(project_path);

    {
        let cache = GRAPH_CACHE.read().await;
        if let Some(cached) = cache
            .as_ref()
            .filter(|c| c.project_path == index_tree_path)
        {
            return Ok((
                cached.graph.clone(),
                cached.pagerank.clone(),
                cached.sccs.clone(),
            ));
        }
    }

    let cache_repo = RepoGraphCacheRepository::new(ctx.db().clone());
    let row = cache_repo
        .latest_for_project(project_id)
        .await
        .map_err(|e| format!("read repo_graph_cache for '{project_id}': {e}"))?
        .ok_or_else(|| GRAPH_NOT_WARMED_ERR.to_string())?;

    let (graph, pagerank, sccs) = load_cached_artifact(row.graph_blob).await?;
    install_as_canonical(
        ctx,
        project_id,
        index_tree_path,
        row.commit_sha,
        graph.clone(),
        pagerank.clone(),
        sccs.clone(),
    )
    .await;
    Ok((graph, pagerank, sccs))
}

/// Thin wrapper for callers that only need the graph.
pub async fn load_canonical_graph_only<C: WarmContext>(
    ctx: &C,
    project_id: &str,
    project_path: &str,
) -> Result<crate::repo_graph::RepoDependencyGraph, String> {
    let (graph, _pagerank, _sccs) = load_canonical_graph(ctx, project_id, project_path).await?;
    Ok(graph)
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

/// Consult the persisted `projects.stack` JSON and translate the detected
/// languages into the subset of [`crate::scip_indexer::SupportedIndexer`]
/// variants [`crate::scip_indexer::run_indexers_already_locked`] should run
/// for this project.
///
/// Returns `None` when:
/// * the project row is missing,
/// * the stack JSON is empty / the default `{}`,
/// * no language entries map onto a known indexer.
///
/// The canonical-graph pipeline treats `None` as "run every known indexer"
/// — the legacy behaviour pre-PR-8.  A non-empty `Vec` trims the indexer
/// fan-out down to just the matched languages.
async fn resolve_stack_indexer_filter<C: WarmContext>(
    ctx: &C,
    project_id: &str,
) -> Option<Vec<crate::scip_indexer::SupportedIndexer>> {
    use djinn_db::ProjectRepository;
    use djinn_stack::Stack;

    let repo = ProjectRepository::new(ctx.db().clone(), ctx.event_bus());
    let raw = match repo.get_stack(project_id).await {
        Ok(Some(s)) => s,
        _ => return None,
    };
    if raw.trim().is_empty() || raw.trim() == "{}" {
        return None;
    }
    let stack: Stack = serde_json::from_str(&raw).ok()?;
    if stack.languages.is_empty() {
        return None;
    }

    let mut wanted: Vec<crate::scip_indexer::SupportedIndexer> = Vec::new();
    let mut push = |ind: crate::scip_indexer::SupportedIndexer| {
        if !wanted.contains(&ind) {
            wanted.push(ind);
        }
    };
    for lang in &stack.languages {
        let name = lang.name.to_ascii_lowercase();
        match name.as_str() {
            "rust" => push(crate::scip_indexer::SupportedIndexer::RustAnalyzer),
            "typescript" | "javascript" | "tsx" | "jsx" => {
                push(crate::scip_indexer::SupportedIndexer::TypeScript)
            }
            "python" => push(crate::scip_indexer::SupportedIndexer::Python),
            "go" => push(crate::scip_indexer::SupportedIndexer::Go),
            "java" | "kotlin" | "scala" => push(crate::scip_indexer::SupportedIndexer::Java),
            "c" | "c++" | "cpp" | "objective-c" | "objective-c++" => {
                push(crate::scip_indexer::SupportedIndexer::Clang)
            }
            "ruby" => push(crate::scip_indexer::SupportedIndexer::Ruby),
            "c#" | "csharp" | "f#" => push(crate::scip_indexer::SupportedIndexer::DotNet),
            _ => {}
        }
    }
    if wanted.is_empty() { None } else { Some(wanted) }
}

pub fn normalize_graph_query_paths(project_path: &str) -> (PathBuf, PathBuf) {
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
pub async fn clear_test_caches() {
    let mut cache = GRAPH_CACHE.write().await;
    *cache = None;
}

#[cfg(test)]
pub fn build_test_parsed_index_fixture() -> crate::scip_parser::ParsedScipIndex {
    use crate::scip_parser::{
        ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
        ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    let helper_symbol_name = "scip-rust pkg src/helper.rs `helper`().".to_string();
    let helper_symbol = ScipSymbol {
        symbol: helper_symbol_name.clone(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("helper".to_string()),
        signature: Some("fn helper()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
    };
    let trait_symbol = ScipSymbol {
        symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("HelperTrait".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
    };
    let main_symbol = ScipSymbol {
        symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("main".to_string()),
        signature: Some("fn main()".to_string()),
        documentation: vec![],
        relationships: vec![ScipRelationship {
            source_symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
            target_symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
            kinds: BTreeSet::from([ScipRelationshipKind::Implementation]),
        }],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
    };

    fn def_occ(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 6,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::Definition]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    fn ref_occ(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 1,
                start_character: 4,
                end_line: 1,
                end_character: 10,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::ReadAccess]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    ParsedScipIndex {
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo".to_string()),
            tool_name: Some("rust-analyzer".to_string()),
            tool_version: Some("1.0.0".to_string()),
        },
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/helper.rs"),
                definitions: vec![def_occ(&helper_symbol_name)],
                references: vec![],
                occurrences: vec![def_occ(&helper_symbol_name)],
                symbols: vec![helper_symbol],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/app.rs"),
                definitions: vec![def_occ(&main_symbol.symbol)],
                references: vec![ref_occ(&helper_symbol_name)],
                occurrences: vec![def_occ(&main_symbol.symbol), ref_occ(&helper_symbol_name)],
                symbols: vec![main_symbol, trait_symbol],
            },
        ],
        external_symbols: vec![],
    }
}

#[cfg(test)]
pub fn build_test_graph_fixture() -> crate::repo_graph::RepoDependencyGraph {
    crate::repo_graph::RepoDependencyGraph::build(&[build_test_parsed_index_fixture()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{TestWarmContext, create_test_db, workspace_tempdir};
    use djinn_core::events::EventBus;
    use djinn_db::{ProjectRepository, RepoGraphCacheInsert, RepoGraphCacheRepository};

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
        let tmp = workspace_tempdir("canonical-graph-");
        let project_root = make_project(tmp.path()).await;
        let db = create_test_db();
        let ctx = TestWarmContext::new(db.clone());
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("test-canonical", "test", "test-canonical")
            .await
            .expect("create project");

        let head_out = tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project_root)
            .output()
            .await
            .unwrap();
        let head_sha = String::from_utf8_lossy(&head_out.stdout).trim().to_string();

        let graph = build_test_graph_fixture();
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

        let result = ensure_canonical_graph(
            &ctx,
            &project.id,
            &project_root,
            ArchitectWarmToken::for_tests(),
        )
        .await;
        assert!(result.is_ok(), "expected cache-hit success, got {result:?}");
        let (_handle, returned_graph) = result.unwrap();
        let _ = head_sha;
        assert_eq!(returned_graph.node_count(), graph.node_count());
    }

    #[tokio::test]
    async fn ensure_canonical_graph_treats_stale_blob_as_cache_miss() {
        let tmp = workspace_tempdir("canonical-graph-");
        let project_root = make_project(tmp.path()).await;
        let db = create_test_db();
        let ctx = TestWarmContext::new(db.clone());
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("test-canonical-stale", "test", "test-canonical-stale")
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

        let result = ensure_canonical_graph(
            &ctx,
            &project.id,
            &project_root,
            ArchitectWarmToken::for_tests(),
        )
        .await;
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
        let tmp = workspace_tempdir("canonical-graph-");
        let project_root = make_project(tmp.path()).await;
        let db = create_test_db();
        let ctx = TestWarmContext::new(db.clone());
        let _ = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-cache-only-readers", "test", "test-cache-only-readers")
            .await
            .expect("create project");

        let index_tree_path = project_root.join(".djinn").join("worktrees").join("_index");
        let stale_sha = "0000000000000000000000000000000000000000".to_string();
        let expected_node_count = {
            let graph = build_test_graph_fixture();
            let node_count = graph.node_count();
            let (pagerank, sccs) = derive_graph_caches(&graph);
            let mut cache = GRAPH_CACHE.write().await;
            *cache = Some(CachedGraph {
                graph,
                project_path: index_tree_path.clone(),
                git_head: stale_sha,
                pagerank,
                sccs,
            });
            node_count
        };

        let project_root_str = project_root.to_string_lossy().into_owned();
        // Find the project we just created so we can pass its id.
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .get_by_github("test", "test-cache-only-readers")
            .await
            .expect("lookup project")
            .expect("project exists");
        let graph_only = load_canonical_graph_only(&ctx, &project.id, &project_root_str)
            .await
            .expect("cache-only reader must succeed without warming");
        let (graph_with_caches, pagerank, _sccs) =
            load_canonical_graph(&ctx, &project.id, &project_root_str)
                .await
                .expect("cache-only reader (with caches) must succeed without warming");

        clear_test_caches().await;

        assert_eq!(graph_only.node_count(), expected_node_count);
        assert_eq!(graph_with_caches.node_count(), expected_node_count);
        assert_eq!(pagerank.nodes.len(), expected_node_count);
    }

    #[tokio::test]
    async fn resolve_stack_indexer_filter_maps_languages_to_indexers() {
        use djinn_stack::{LanguageStat, ManifestSignals, Runtimes, Stack};

        let tmp = workspace_tempdir("canonical-graph-");
        let db = create_test_db();
        let ctx = TestWarmContext::new(db.clone());
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("stack-filter", "test", "stack-filter")
            .await
            .expect("create project");

        // Rust + TypeScript stack → expect two indexers.
        let mut stack = Stack::empty();
        stack.languages = vec![
            LanguageStat {
                name: "Rust".to_string(),
                bytes: 1000,
                pct: 60.0,
            },
            LanguageStat {
                name: "TypeScript".to_string(),
                bytes: 400,
                pct: 24.0,
            },
            LanguageStat {
                name: "Dockerfile".to_string(),
                bytes: 50,
                pct: 3.0,
            },
        ];
        stack.primary_language = Some("Rust".to_string());
        stack.package_managers = vec!["cargo".to_string()];
        let _: ManifestSignals = stack.manifest_signals.clone();
        let _: Runtimes = stack.runtimes.clone();
        proj_repo
            .set_stack(&project.id, &serde_json::to_string(&stack).unwrap())
            .await
            .expect("set stack");

        let filter = resolve_stack_indexer_filter(&ctx, &project.id)
            .await
            .expect("filter is Some for non-empty stack");
        assert!(filter.contains(&crate::scip_indexer::SupportedIndexer::RustAnalyzer));
        assert!(filter.contains(&crate::scip_indexer::SupportedIndexer::TypeScript));
        assert!(!filter.contains(&crate::scip_indexer::SupportedIndexer::Go));
        assert_eq!(filter.len(), 2, "unknown language must not add an indexer");

        // Empty stack (default) → None (fall back to all indexers).
        let other_root = tmp.path().join("repo-empty");
        tokio::fs::create_dir_all(&other_root).await.unwrap();
        let project2 = proj_repo
            .create("stack-empty", "test", "stack-empty")
            .await
            .expect("create second project");
        let filter_none = resolve_stack_indexer_filter(&ctx, &project2.id).await;
        assert!(
            filter_none.is_none(),
            "empty `{{}}` stack must return None so callers run every indexer"
        );
    }

}
