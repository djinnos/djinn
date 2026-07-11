// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::WarmContext;
use crate::architect::ArchitectWarmToken;
use djinn_core::clock::{Clock, SystemClock};

pub type CachedLayoutPositions =
    std::collections::BTreeMap<String, crate::layout::GraphLayoutPosition>;
pub type CrateMap = std::collections::BTreeMap<PathBuf, String>;

type DerivedGraphCaches = (
    Arc<crate::repo_graph::RepoGraphRanking>,
    Arc<CachedSccs>,
    Arc<CachedLayoutPositions>,
    Arc<CrateMap>,
);

/// Output bundle of the CPU-bound canonical graph build pipeline,
/// produced on a `spawn_blocking` thread and consumed by the async tail that
/// writes DB caches and installs the in-memory canonical slot.
type CanonicalGraphBuildOutput = (
    crate::repo_graph::RepoDependencyGraph,
    Vec<u8>,
    Arc<crate::repo_graph::RepoGraphRanking>,
    Arc<CachedSccs>,
    Arc<CachedLayoutPositions>,
    Arc<CrateMap>,
    u64,
    u64,
    u64,
    u64,
    usize,
    usize,
);

/// Ignore very small graph-size fluctuations: route/process extraction and
/// indexer metadata can legitimately move a few nodes between commits.
const GRAPH_CACHE_SHRINK_MIN_ABSOLUTE_DELTA: usize = 100;
/// Require at least a 10% drop as well as the absolute floor before warning,
/// so small repositories do not warn on normal day-to-day edits.
const GRAPH_CACHE_SHRINK_MIN_PERCENT: f64 = 0.10;

#[derive(Debug, Clone, PartialEq)]
struct GraphCacheShrinkWarning {
    old_node_count: usize,
    new_node_count: usize,
    delta: usize,
    tolerance_min_absolute_delta: usize,
    tolerance_min_percent: f64,
    workspace_status_summary: String,
}

/// Read the production canonical warm cache-reuse env toggle.
///
/// When enabled, the canonical warm path reuses already-parsed SCIP artifacts
/// from the content-addressed parse cache as an **input** optimization. The
/// full artifact set is still parsed (or served from cache) and the whole
/// graph is still built — cache reuse must NEVER become a changed-file-only
/// or partial graph resolution path. The returned value flows through
/// [`resolve_canonical_warm_cache_reuse`] into
/// [`crate::scip_parser::parse_scip_artifacts_with_cache_reuse`], which is
/// the same whole-artifact parse/build seam exercised by the td55 parity
/// tests in [`crate::graph_parity`].
fn cache_reuse_enabled() -> bool {
    std::env::var("DJINN_GRAPH_CACHE_REUSE_ENABLED")
        .or_else(|_| std::env::var("DJINN_CACHE_REUSE_ENABLED"))
        .or_else(|_| std::env::var("CACHE_REUSE_ENABLED"))
        .map(|value| {
            matches!(
                value.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

/// Resolve whether the canonical warm path should reuse cached parse results.
///
/// This is the single decision point that feeds the `cache_reuse_enabled`
/// argument to
/// [`crate::scip_parser::parse_scip_artifacts_with_cache_reuse`] from
/// [`ensure_canonical_graph`]. The td55 regression
/// `canonical_warm_cache_reuse_toggle_reaches_parity_seam` guards that this
/// resolution stays wired to the same whole-artifact parse/build seam
/// covered by the td55 equivalence gate.
///
/// Cache reuse is allowed ONLY as an input/artifact reuse optimization
/// before the whole-graph build — it must NOT introduce a changed-file-only
/// or partial graph assembly path.
pub(crate) fn resolve_canonical_warm_cache_reuse(force_full_rebuild: bool) -> bool {
    cache_reuse_enabled() && !force_full_rebuild
}

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
    pub layout_positions: Arc<CachedLayoutPositions>,
    pub crate_map: Arc<CrateMap>,
}

impl CachedGraph {
    pub fn layout_position_by_uid(
        &self,
        stable_uid: &str,
    ) -> Option<crate::layout::GraphLayoutPosition> {
        self.layout_positions.get(stable_uid).copied()
    }

    /// Return `true` when this cached graph's pinned `git_head` differs
    /// from the caller-supplied commit, or when `git_head` is missing/blank.
    ///
    /// Used by `impact_check` (epic z3en) to short-circuit consumer
    /// computation when the canonical graph is stale: a stale graph would
    /// produce unreliable consumer sets, so the safe default is to surface
    /// a `needs_spike` recommendation rather than asserting on bad data.
    ///
    /// Both `caller_head` and `self.git_head` are trimmed before
    /// comparison; a blank `git_head` (un-warmed or un-pinned graph) is
    /// always treated as stale so callers never trust an unpinned graph
    /// for an impact-critical decision.
    pub fn is_stale(&self, caller_head: &str) -> bool {
        git_head_is_strictly_stale(caller_head, &self.git_head)
    }
}

/// Strict staleness primitive shared between `CachedGraph::is_stale` and
/// downstream control-plane flows (epic z3en). Returns `true` when:
/// - `git_head` is missing/blank (un-warmed or un-pinned graph), OR
/// - the trimmed `git_head` and `caller_head` differ.
///
/// This is the strict counterpart of the lenient default in
/// `GraphStaleness::compute` (which returns `is_stale=false` for missing
/// cached commits to avoid blocking unrelated queries). For impact
/// preflight we want the strict form — when the graph has no anchor,
/// we must surface a `needs_spike` rather than silently answering
/// from unanchored data.
pub fn git_head_is_strictly_stale(caller_head: &str, git_head: &str) -> bool {
    let cached = git_head.trim();
    if cached.is_empty() {
        return true;
    }
    let caller = caller_head.trim();
    cached != caller
}

pub static GRAPH_CACHE: std::sync::LazyLock<RwLock<Option<CachedGraph>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

pub fn derive_graph_caches(
    graph: &crate::repo_graph::RepoDependencyGraph,
    project_root: &Path,
) -> DerivedGraphCaches {
    derive_graph_caches_with_crate_map(graph, derive_crate_map(project_root))
}

pub fn derive_graph_caches_with_crate_map(
    graph: &crate::repo_graph::RepoDependencyGraph,
    crate_map: CrateMap,
) -> DerivedGraphCaches {
    use crate::repo_graph::RepoGraphNodeKind;
    let pagerank = Arc::new(graph.rank());
    let sccs = Arc::new(CachedSccs {
        full: graph.strongly_connected_components(None, 2),
        file: graph.strongly_connected_components(Some(RepoGraphNodeKind::File), 2),
        symbol: graph.strongly_connected_components(Some(RepoGraphNodeKind::Symbol), 2),
    });
    let layout_positions = Arc::new(crate::layout::derive_layout_positions(graph));
    let crate_map = Arc::new(crate_map);
    (pagerank, sccs, layout_positions, crate_map)
}

pub fn derive_crate_map(project_root: &Path) -> CrateMap {
    let manifest_path = project_root.join("Cargo.toml");
    let Ok(manifest) = std::fs::read_to_string(&manifest_path) else {
        return std::collections::BTreeMap::new();
    };
    let members = workspace_members_from_manifest(&manifest);
    if members.is_empty() {
        return std::collections::BTreeMap::new();
    }

    let mut out = std::collections::BTreeMap::new();
    for member in members {
        for member_dir in expand_workspace_member(project_root, &member) {
            let member_manifest =
                if member_dir.file_name() == Some(std::ffi::OsStr::new("Cargo.toml")) {
                    member_dir.clone()
                } else {
                    member_dir.join("Cargo.toml")
                };
            let Ok(member_manifest_text) = std::fs::read_to_string(&member_manifest) else {
                continue;
            };
            let Some(crate_name) = package_name_from_manifest(&member_manifest_text) else {
                continue;
            };
            let crate_dir = member_manifest
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or(member_dir);
            out.insert(crate_dir, crate_name);
        }
    }
    out
}

fn workspace_members_from_manifest(manifest: &str) -> Vec<String> {
    let toml_members: Vec<String> = manifest
        .parse::<toml::Value>()
        .ok()
        .and_then(|value| {
            value
                .get("workspace")
                .and_then(|workspace| workspace.get("members"))
                .and_then(toml::Value::as_array)
                .map(|members| {
                    members
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
        })
        .unwrap_or_default();
    if !toml_members.is_empty() {
        return toml_members;
    }

    let mut in_workspace = false;
    let mut members_text = String::new();
    let mut collecting_members = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_workspace = trimmed == "[workspace]";
            collecting_members = false;
            continue;
        }
        if !in_workspace {
            continue;
        }
        if collecting_members || trimmed.starts_with("members") {
            collecting_members = true;
            members_text.push_str(trimmed);
            members_text.push('\n');
            if trimmed.contains(']') {
                break;
            }
        }
    }
    let Some(start) = members_text.find('[') else {
        return Vec::new();
    };
    let Some(end) = members_text[start + 1..].find(']') else {
        return Vec::new();
    };
    members_text[start + 1..start + 1 + end]
        .split(',')
        .filter_map(|raw| {
            let trimmed = raw.trim().trim_matches('"');
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .collect()
}

fn package_name_from_manifest(manifest: &str) -> Option<String> {
    if let Some(name) = manifest.parse::<toml::Value>().ok().and_then(|value| {
        value
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    }) {
        return Some(name);
    }

    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package
            && let Some(raw_name) = trimmed.strip_prefix("name")
            && let Some((_, value)) = raw_name.split_once('=')
        {
            let name = value.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn expand_workspace_member(project_root: &Path, member: &str) -> Vec<PathBuf> {
    let components: Vec<&str> = member.split('/').filter(|part| !part.is_empty()).collect();
    let mut dirs = vec![project_root.to_path_buf()];
    for component in components {
        let mut next = Vec::new();
        if component == "*" {
            for dir in dirs {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        next.push(path);
                    }
                }
            }
        } else {
            next.extend(dirs.into_iter().map(|dir| dir.join(component)));
        }
        dirs = next;
    }
    dirs
}

fn run_route_extraction_post_processor(
    graph: &mut crate::repo_graph::RepoDependencyGraph,
    project_root: &Path,
) -> Result<Option<crate::route_extraction::RouteExtractionReport>, String> {
    if !crate::route_extraction::route_detection_enabled() {
        return Ok(None);
    }

    // Temporary ykcg Route rollout gate: snapshot the graph after
    // `build_with_source` and DB-access but before route extraction. This is
    // the `DJINN_ROUTE_DETECTION=0` baseline shape; it must not become a
    // permanent alternate graph pipeline and should be deleted after rollout.
    let parity_baseline = crate::route_extraction::route_parity_enabled().then(|| graph.clone());
    let report = crate::route_extraction::detect_routes(graph, project_root);
    if let Some(baseline) = &parity_baseline {
        let parity_report =
            crate::route_extraction::assert_route_extraction_graph_parity(baseline, graph)
                .map_err(|err| format!("route extraction parity failed:\n{err}"))?;
        tracing::info!(
            route_parity_report = %parity_report.render_for_ci(),
            "ensure_canonical_graph: route_extraction parity gate passed"
        );
    }
    for (file, messages) in &report.file_failures {
        for message in messages {
            tracing::warn!(
                file = %file.display(),
                error = %message,
                "ensure_canonical_graph: route_extraction skipped file/error"
            );
        }
    }
    tracing::info!(
        route_nodes = report.route_nodes_added,
        handles_route_edges = report.handles_route_edges_added,
        fetches_edges = report.fetches_edges_added,
        route_parity_enabled = crate::route_extraction::route_parity_enabled(),
        unmatched_fetch_count = report.unmatched_fetch_count,
        unresolved_consumer_count = report.unresolved_consumer_count,
        suggested_consumer_edges = report.consumer_edge_suggestions.len(),
        skipped_files = report.skipped_files.len(),
        file_failures = report.file_failures.len(),
        "ensure_canonical_graph: route_extraction pass complete"
    );

    Ok(Some(report))
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
    let clock = SystemClock::new();
    let started = clock.now_instant();
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

    // Coupling ingest is independent of canonical graph success —
    // SCIP indexer panics (e.g. scip-go's `index out of range` bug)
    // used to silently freeze coupling data forever because the call
    // sat AFTER the SCIP step. Run it once, early, before any cache
    // check or indexer work, so coupling stays fresh regardless of
    // SCIP / graph-build outcomes. `ingest_new_commits` is cheap on
    // already-current projects (cursor..HEAD = empty log) and
    // idempotent on replays.
    ingest_coupling_best_effort(ctx, project_id, handle.path()).await;

    {
        let cache = GRAPH_CACHE.read().await;
        if let Some(cached) = cache.as_ref()
            && cached.project_path == handle.path()
            && cached.git_head == commit_sha
        {
            spawn_chunk_and_embed_best_effort(ctx, project_id, handle.path(), &cached.graph);
            spawn_cluster_docs_best_effort(ctx, project_id, &cached.graph);
            return Ok((handle, cached.graph.clone()));
        }
    }

    if let Ok(Some(row)) = cache_repo.get(project_id, &commit_sha).await {
        match load_cached_artifact(row.graph_blob, handle.path().to_path_buf()).await {
            Ok((graph, pagerank, sccs, layout_positions, crate_map)) => {
                install_as_canonical(
                    handle.path().to_path_buf(),
                    commit_sha.clone(),
                    graph.clone(),
                    pagerank,
                    sccs,
                    layout_positions,
                    crate_map,
                )
                .await;
                spawn_chunk_and_embed_best_effort(ctx, project_id, handle.path(), &graph);
                spawn_cluster_docs_best_effort(ctx, project_id, &graph);
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
            spawn_chunk_and_embed_best_effort(ctx, project_id, handle.path(), &cached.graph);
            spawn_cluster_docs_best_effort(ctx, project_id, &cached.graph);
            return Ok((handle, cached.graph.clone()));
        }
    }
    if let Ok(Some(row)) = cache_repo.get(project_id, &commit_sha).await {
        match load_cached_artifact(row.graph_blob, handle.path().to_path_buf()).await {
            Ok((graph, pagerank, sccs, layout_positions, crate_map)) => {
                install_as_canonical(
                    handle.path().to_path_buf(),
                    commit_sha.clone(),
                    graph.clone(),
                    pagerank,
                    sccs,
                    layout_positions,
                    crate_map,
                )
                .await;
                spawn_chunk_and_embed_best_effort(ctx, project_id, handle.path(), &graph);
                spawn_cluster_docs_best_effort(ctx, project_id, &graph);
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

    // --- Crash-sentinel contract (warm_sentinel) ---
    // Before entering the file-based cache-mutating section, check whether a
    // previous warm run crashed mid-mutation. If so, force a full rebuild
    // (skip parse cache reuse), clean up orphaned tmp artifacts, and log a
    // clear recovery message. See `warm_sentinel` module-level docs.
    let sentinel_cache_root = crate::scip_indexer::cache::ScipCacheStore::from_environment();
    let force_full_rebuild =
        crate::warm_sentinel::observe_and_recover(sentinel_cache_root.cache_root());
    if force_full_rebuild {
        tracing::info!(
            project_id,
            commit_sha = %commit_sha,
            "ensure_canonical_graph: stale warm sentinel recovered — forcing full rebuild (cache reuse disabled)"
        );
    }
    // Write the sentinel checkpoint before any file-based cache mutation.
    if let Err(e) = crate::warm_sentinel::checkpoint(sentinel_cache_root.cache_root(), &commit_sha)
    {
        tracing::warn!(
            error = %e,
            "ensure_canonical_graph: failed to write warm sentinel checkpoint; proceeding without sentinel protection"
        );
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
    // In the K8s warm-Pod path this resolves to `None` when the Pod already
    // routes `CARGO_TARGET_DIR` at the warmed per-project base
    // (`/cache/cargo-target/<project>`), so the indexer inherits that
    // pre-warmed target instead of recompiling into the Pod's ephemeral
    // `_index-target` every warm. In the in-process (dev/peer) path it
    // resolves to `Some(<_index-target>)` and the CargoTargetDirGuard isolates
    // the indexer build from the host server's own target dir as before.
    let target_dir = handle.indexer_target_dir_override();

    // Phase 3 PR 8: ask the DB for the detected stack and filter the SCIP
    // indexer set to languages the project actually uses. Falls back to
    // running every indexer when no stack has been persisted yet (fresh
    // project, or a pre-PR-2 deployment).
    let stack_filter = resolve_stack_indexer_filter(ctx, project_id).await;
    let declared_workspaces = resolve_declared_workspaces(ctx, project_id).await;

    let clock = SystemClock::new();
    let t_indexers = clock.now_instant();
    let run = crate::scip_indexer::run_indexers_already_locked(
        handle.path(),
        &output_dir,
        target_dir.as_deref(),
        stack_filter.as_deref(),
        declared_workspaces.as_deref(),
    )
    .await
    .map_err(|e| format!("run_indexers: {e}"))?;
    let indexers_ms = t_indexers.elapsed().as_millis() as u64;

    let output_dir_for_blocking = output_dir.clone();
    let artifacts = run.artifacts;
    let workspace_statuses = run.workspace_statuses;
    let project_root_for_blocking = handle.path().to_path_buf();
    // Capture recovery flag for the blocking thread — when a stale sentinel
    // was detected, disable parse cache reuse to force a clean rebuild.
    let effective_cache_reuse = resolve_canonical_warm_cache_reuse(force_full_rebuild);
    let blocking =
        tokio::task::spawn_blocking(move || -> Result<CanonicalGraphBuildOutput, String> {
            let t_parse = clock.now_instant();
            let parsed = crate::scip_parser::parse_scip_artifacts_with_cache_reuse(
                &artifacts,
                effective_cache_reuse,
            )
            .map_err(|e| format!("parse_scip_artifacts: {e}"))?;
            let parse_ms = t_parse.elapsed().as_millis() as u64;
            let _ = std::fs::remove_dir_all(&output_dir_for_blocking);

            // Out-of-core sharding: when enabled (env flag + threshold),
            // shard each parsed SCIP file into the out-of-core store so
            // the graph builder can consume files one-at-a-time from
            // disk, keeping resident memory bounded.
            let total_parsed_files: usize =
                parsed.iter().map(|p| p.files.len()).sum();
            let ooc_engaged = crate::out_of_core::resolve_out_of_core_config(total_parsed_files);
            let crate_map_for_build = derive_crate_map(&project_root_for_blocking);
            let graph_build_options = crate::repo_graph::RepoGraphBuildOptions::from_env();

            // If out-of-core is engaged, shard the parsed data to disk
            // and build the graph from the store's file iterator. The
            // in-memory `parsed.files` vecs are dropped after sharding
            // to free memory.
            let t_build = clock.now_instant();
            let mut graph = if let Some(ref ooc_config) = ooc_engaged {
                match crate::out_of_core::OutOfCoreStore::open(&ooc_config.storage_path) {
                    Ok(mut ooc_store) => {
                        let mut total_shards = 0usize;
                        let mut shard_err = false;
                        for index in &parsed {
                            match ooc_store.put_parsed_index(index) {
                                Ok(count) => total_shards += count,
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        workspace = %index.workspace_slug,
                                        "ensure_canonical_graph: failed to shard parsed index to out-of-core store"
                                    );
                                    shard_err = true;
                                    break;
                                }
                            }
                        }
                        if shard_err {
                            // Sharding failed — fall back to in-memory path.
                            tracing::warn!(
                                "ensure_canonical_graph: sharding failed; falling back to in-memory build"
                            );
                            crate::repo_graph::RepoDependencyGraph::try_build_with_source_options_and_crate_map(
                                &parsed,
                                Some(&project_root_for_blocking),
                                graph_build_options,
                                Some(&crate_map_for_build),
                            )?
                        } else {
                            tracing::info!(
                                shard_count = total_shards,
                                storage_path = %ooc_config.storage_path.display(),
                                "ensure_canonical_graph: sharded parsed SCIP data to out-of-core store"
                            );
                            // Build the graph using the bounded-memory file
                            // iterator from the store. Files are loaded
                            // one-at-a-time from disk — only one ScipFile
                            // is resident per iteration step (O(1) file
                            // data residency).
                            //
                            // Merge external_symbols from all parsed indices
                            // and use the first index's workspace_slug.
                            // (In practice, warm runs typically have a single
                            // workspace; multi-workspace OOC is a known
                            // simplification.)
                            let first = &parsed[0];
                            let all_external: Vec<crate::scip_parser::ScipSymbol> = parsed
                                .iter()
                                .flat_map(|idx| idx.external_symbols.iter().cloned())
                                .collect();
                            let graph_result =
                                crate::repo_graph::RepoDependencyGraph::try_build_with_scip_file_iter_options_and_crate_map(
                                    ooc_store.scip_file_iter(),
                                    &first.workspace_slug,
                                    &all_external,
                                    Some(&project_root_for_blocking),
                                    graph_build_options,
                                    Some(&crate_map_for_build),
                                );
                            // Drop the in-memory parsed file data now that
                            // it has been sharded to disk. This frees the
                            // large Vec<ScipFile> allocations.
                            drop(parsed);
                            graph_result?
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            storage_path = %ooc_config.storage_path.display(),
                            "ensure_canonical_graph: failed to open out-of-core store; proceeding with in-memory path"
                        );
                        crate::repo_graph::RepoDependencyGraph::try_build_with_source_options_and_crate_map(
                            &parsed,
                            Some(&project_root_for_blocking),
                            graph_build_options,
                            Some(&crate_map_for_build),
                        )?
                    }
                }
            } else {
                // Out-of-core not engaged (env flag off or below threshold):
                // use the existing in-memory build path unchanged.
                crate::repo_graph::RepoDependencyGraph::try_build_with_source_options_and_crate_map(
                    &parsed,
                    Some(&project_root_for_blocking),
                    graph_build_options,
                    Some(&crate_map_for_build),
                )?
            };
            // DB-access post-processor: opt-in via
            // `DJINN_DB_ACCESS_DETECTION`. Reads files from the index
            // tree and stamps `Reads`/`Writes` edges from caller
            // symbols to synthetic `Table` nodes. Logged at info level
            // so we can see the size of the signal during rollout.
            if crate::db_access::db_access_detection_enabled() {
                let added =
                    crate::db_access::detect_db_access(&mut graph, &project_root_for_blocking);
                tracing::info!(
                    db_access_edges = added,
                    "ensure_canonical_graph: db_access pass complete"
                );
            }
            // Route extraction intentionally runs after `build_with_source` has
            // completed the SCIP graph plus its built-in entry-point, process,
            // and complexity/default post-processors, and after DB-access has
            // stamped any source-derived edges. Keep it before cache derivation
            // and bincode serialization so Route/HandlesRoute/Fetches metadata
            // is installed both in memory and in repo_graph_cache.
            let _ = run_route_extraction_post_processor(&mut graph, &project_root_for_blocking)?;
            let build_ms = t_build.elapsed().as_millis() as u64;
            let node_count = graph.node_count();
            let edge_count = graph.edge_count();

            let t_derive = clock.now_instant();
            let (pagerank, sccs, layout_positions, crate_map) =
                derive_graph_caches_with_crate_map(&graph, crate_map_for_build);
            graph.set_layout_positions((*layout_positions).clone());
            let derive_ms = t_derive.elapsed().as_millis() as u64;

            let t_serial = clock.now_instant();
            let serialized = bincode::serialize(&graph.to_artifact())
                .map_err(|e| format!("bincode serialize graph: {e}"))?;
            let serial_ms = t_serial.elapsed().as_millis() as u64;

            Ok((
                graph,
                serialized,
                pagerank,
                sccs,
                layout_positions,
                crate_map,
                parse_ms,
                build_ms,
                derive_ms,
                serial_ms,
                node_count,
                edge_count,
            ))
        })
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?;
    let (
        graph,
        serialized_blob,
        pagerank,
        sccs,
        layout_positions,
        crate_map,
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

    // Out-of-core scope/parse store: resolve configuration and log whether
    // the bounded accessor path is available. When engaged (env flag set AND
    // node count meets threshold), downstream consumers can use
    // `out_of_core::BoundedScopeAccessor` to keep resident memory bounded.
    // The graph output is identical to the in-memory path — the out-of-core
    // store is an accessor-layer optimization, not a different build pipeline.
    match crate::out_of_core::resolve_out_of_core_config(node_count) {
        Some(ooc_config) => {
            tracing::info!(
                project_id = %project_id,
                node_count,
                lru_capacity = ooc_config.lru_capacity,
                min_nodes = ooc_config.min_nodes,
                storage_path = %ooc_config.storage_path.display(),
                "ensure_canonical_graph: out-of-core scope store available (env flag + threshold met)"
            );
        }
        None if crate::out_of_core::out_of_core_enabled() => {
            tracing::debug!(
                project_id = %project_id,
                node_count,
                min_nodes = crate::out_of_core::out_of_core_min_nodes(),
                "ensure_canonical_graph: out-of-core flag set but node count below threshold; using in-memory path"
            );
        }
        None => {}
    }

    // Never cache an empty graph. A real project always indexes to >0
    // nodes; node_count==0 means this warmer ran without the project
    // source (e.g. the server-side in-process AppStateGraphWarmer, whose
    // server-local clone may be absent) or the indexer produced nothing.
    // Persisting it poisons the (project_id, commit_sha)-keyed cache: the
    // K8s warm pod that DOES have source then cache-hits the empty blob
    // and skips indexing forever. Skip both the DB upsert and the
    // in-memory install so the next warm with real source re-indexes.
    if node_count == 0 {
        tracing::warn!(
            project_id = %project_id,
            commit_sha = %commit_sha,
            "ensure_canonical_graph: indexed 0 nodes — not caching (likely warmer without project source); leaving cache for a source-bearing warm"
        );
        return Ok((handle, graph));
    }

    let pre_write_latest = match cache_repo.latest_for_project(project_id).await {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                commit_sha = %commit_sha,
                error = %e,
                "ensure_canonical_graph: failed to read previous graph cache row before upsert"
            );
            None
        }
    };

    let shrink_warning = detect_graph_cache_shrink_warning(
        pre_write_latest
            .as_ref()
            .map(|row| row.graph_blob.as_slice()),
        node_count,
        &workspace_statuses,
    );
    if let Some(warning) = &shrink_warning {
        tracing::warn!(
            project_id = %project_id,
            commit_sha = %commit_sha,
            old_node_count = warning.old_node_count,
            new_node_count = warning.new_node_count,
            delta = warning.delta,
            tolerance_min_absolute_delta = warning.tolerance_min_absolute_delta,
            tolerance_min_percent = warning.tolerance_min_percent,
            workspace_status_summary = %warning.workspace_status_summary,
            "ensure_canonical_graph: graph cache node count shrank beyond tolerance without failed/timed_out workspace status"
        );
        let detail = serde_json::json!({
            "kind": "graph_cache_shrink_warning",
            "project_id": project_id,
            "commit_sha": commit_sha,
            "old_node_count": warning.old_node_count,
            "new_node_count": warning.new_node_count,
            "delta": warning.delta,
            "tolerance_min_absolute_delta": warning.tolerance_min_absolute_delta,
            "tolerance_min_percent": warning.tolerance_min_percent,
            "workspace_status_summary": warning.workspace_status_summary,
        })
        .to_string();
        if let Err(e) = crate::scip_indexer::append_graph_cache_shrink_warning(
            handle.path(),
            &workspace_statuses,
            detail,
        ) {
            tracing::warn!(
                project_id = %project_id,
                commit_sha = %commit_sha,
                error = %e,
                "ensure_canonical_graph: failed to persist graph cache shrink warning status"
            );
        }
    }

    match cache_repo
        .upsert(RepoGraphCacheInsert {
            project_id,
            commit_sha: &commit_sha,
            graph_blob: &serialized_blob,
        })
        .await
    {
        Ok(()) => {
            persist_workspace_graph_freshness_best_effort(
                ctx,
                project_id,
                &commit_sha,
                &graph,
                &workspace_statuses,
            )
            .await;
        }
        Err(e) => {
            tracing::warn!(error = %e, "ensure_canonical_graph: failed to persist graph cache row");
        }
    }

    // Coupling ingest already ran early in this function, so no
    // duplicate call here. See the comment at the top of
    // `ensure_canonical_graph` for the rationale.

    install_as_canonical(
        handle.path().to_path_buf(),
        commit_sha.clone(),
        graph.clone(),
        pagerank,
        sccs,
        layout_positions,
        crate_map,
    )
    .await;

    // Warm pipeline succeeded — clear the crash sentinel so the next warm
    // does not observe a stale in-progress marker.
    crate::warm_sentinel::clear(sentinel_cache_root.cache_root());

    spawn_chunk_and_embed_best_effort(ctx, project_id, handle.path(), &graph);
    spawn_cluster_docs_best_effort(ctx, project_id, &graph);
    Ok((handle, graph))
}

fn detect_graph_cache_shrink_warning(
    previous_blob: Option<&[u8]>,
    new_node_count: usize,
    workspace_statuses: &[crate::scip_indexer::WorkspaceWarmStatus],
) -> Option<GraphCacheShrinkWarning> {
    let previous_blob = previous_blob?;
    // Suppress shrink warnings whenever the warm explains a node-count drop:
    // `failed`/`timed_out` rows mean an indexer could not produce an artifact,
    // and `ready_with_quarantine` means a below-workspace partition was
    // quarantined while the rest of the workspace succeeded — both are
    // expected causes of a smaller graph and should not append a misleading
    // synthetic `graph-cache` warning row. Only an *unexplained* shrink with
    // no such status still emits the warning.
    if workspace_statuses.iter().any(|status| {
        matches!(
            status.status.as_str(),
            "failed" | "timed_out" | "ready_with_quarantine"
        )
    }) {
        return None;
    }

    let previous =
        crate::repo_graph::deserialize_repo_graph_artifact_bincode(previous_blob).ok()?;
    let old_node_count = previous.nodes.len();
    if new_node_count >= old_node_count {
        return None;
    }
    let delta = old_node_count - new_node_count;
    let percent = if old_node_count == 0 {
        0.0
    } else {
        delta as f64 / old_node_count as f64
    };
    if delta < GRAPH_CACHE_SHRINK_MIN_ABSOLUTE_DELTA || percent < GRAPH_CACHE_SHRINK_MIN_PERCENT {
        return None;
    }

    Some(GraphCacheShrinkWarning {
        old_node_count,
        new_node_count,
        delta,
        tolerance_min_absolute_delta: GRAPH_CACHE_SHRINK_MIN_ABSOLUTE_DELTA,
        tolerance_min_percent: GRAPH_CACHE_SHRINK_MIN_PERCENT,
        workspace_status_summary: workspace_status_summary(workspace_statuses),
    })
}

fn workspace_status_summary(statuses: &[crate::scip_indexer::WorkspaceWarmStatus]) -> String {
    if statuses.is_empty() {
        return "none".to_string();
    }
    statuses
        .iter()
        .map(|status| {
            format!(
                "{}:{}={}",
                status.workspace_slug,
                status.indexer.binary_name(),
                status.status
            )
        })
        .collect::<Vec<_>>()
        .join(",")
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

/// PR F4: fire the cluster-doc generator on a detached `tokio::spawn`
/// when the `DJINN_CLUSTER_DOCS` flag is on and the graph carries at
/// least one community. Idempotent — already-written notes are
/// skipped at the permalink check inside `generate_for_all`. Always a
/// no-op when the flag is unset (default).
fn spawn_cluster_docs_best_effort<C: WarmContext>(
    ctx: &C,
    project_id: &str,
    graph: &crate::repo_graph::RepoDependencyGraph,
) {
    crate::cluster_doc::spawn_generate_for_all(
        ctx.db().clone(),
        ctx.event_bus(),
        project_id.to_string(),
        Arc::new(graph.clone()),
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

fn distinct_workspace_slugs(graph: &crate::repo_graph::RepoDependencyGraph) -> Vec<String> {
    let mut slugs = std::collections::BTreeSet::new();
    for node in graph.graph().node_weights() {
        if let Some(slug) = node
            .workspace
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            slugs.insert(slug.to_string());
        }
    }

    // Pre-workspace v10 artifacts and synthetic nodes may not carry workspace
    // metadata. For a non-empty freshly warmed graph, stamp a stable root row
    // rather than leaving the project with no per-workspace freshness signal.
    if slugs.is_empty() && graph.node_count() > 0 {
        slugs.insert("root".to_string());
    }

    slugs.into_iter().collect()
}

async fn persist_workspace_graph_freshness_best_effort<C: WarmContext>(
    ctx: &C,
    project_id: &str,
    commit_sha: &str,
    graph: &crate::repo_graph::RepoDependencyGraph,
    workspace_statuses: &[crate::scip_indexer::WorkspaceWarmStatus],
) {
    use djinn_db::{
        CODELESS_WORKSPACE_SLUG, ProjectWorkspaceGraphRepository, ProjectWorkspaceGraphUpsert,
    };

    let workspaces = distinct_workspace_slugs(graph);
    if workspaces.is_empty() {
        return;
    }

    let mut rows: Vec<_> = workspaces
        .iter()
        .map(|workspace_slug| ProjectWorkspaceGraphUpsert {
            project_id,
            workspace_slug,
            commit_sha,
            status: "ready",
        })
        .collect();

    // A workspace whose indexer failed (or timed out) contributes no nodes,
    // so it never makes `distinct_workspace_slugs` — without an explicit row
    // its previous "ready" stamp (possibly from an older commit) survives and
    // lies about freshness, and a never-yet-indexed workspace stays invisible.
    // Stamp those at THIS commit with their failure status so the workspaces
    // op / UI can tell "indexed empty" from "indexer wiped out". Partial
    // success still caches the merged graph (see `tally_indexer_results` for
    // the policy); this is purely visibility.
    let ready: std::collections::BTreeSet<&str> = workspaces.iter().map(String::as_str).collect();
    let mut failed_seen = std::collections::BTreeSet::new();
    for status in workspace_statuses {
        if !matches!(status.status.as_str(), "failed" | "timed_out") {
            continue;
        }
        if ready.contains(status.workspace_slug.as_str()) {
            // Another indexer covered this workspace (polyglot roots run
            // several) — the graph has nodes for it, so "ready" wins.
            continue;
        }
        if failed_seen.insert(status.workspace_slug.as_str()) {
            rows.push(ProjectWorkspaceGraphUpsert {
                project_id,
                workspace_slug: &status.workspace_slug,
                commit_sha,
                status: &status.status,
            });
        }
    }

    let workspace_count = rows.len();
    let repo = ProjectWorkspaceGraphRepository::new(ctx.db().clone());
    if let Err(e) = repo.upsert_many(&rows).await {
        tracing::warn!(
            project_id = %project_id,
            commit_sha = %commit_sha,
            workspace_count,
            error = %e,
            "ensure_canonical_graph: failed to persist project_workspace_graph freshness rows"
        );
    }

    // A successful warm with real workspaces contradicts any lingering
    // code-less sentinel (e.g. stamped while the warm gate misfired on a
    // catalog-image project). Retire it so freshness derives from real rows.
    if let Err(e) = repo.delete(project_id, CODELESS_WORKSPACE_SLUG).await {
        tracing::warn!(
            project_id = %project_id,
            error = %e,
            "ensure_canonical_graph: failed to delete code-less sentinel row"
        );
    }
}

pub async fn canonical_graph_cache_has_entry_for(index_tree_path: &Path) -> bool {
    let cache = GRAPH_CACHE.read().await;
    cache
        .as_ref()
        .is_some_and(|cached| cached.project_path == index_tree_path)
}

pub async fn canonical_graph_cache_pinned_commit_for(index_tree_path: &Path) -> Option<String> {
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

async fn install_as_canonical(
    project_path: PathBuf,
    git_head: String,
    graph: crate::repo_graph::RepoDependencyGraph,
    pagerank: Arc<crate::repo_graph::RepoGraphRanking>,
    sccs: Arc<CachedSccs>,
    layout_positions: Arc<std::collections::BTreeMap<String, crate::layout::GraphLayoutPosition>>,
    crate_map: Arc<std::collections::BTreeMap<PathBuf, String>>,
) {
    let mut cache = GRAPH_CACHE.write().await;
    *cache = Some(CachedGraph {
        graph,
        project_path,
        git_head,
        pagerank,
        sccs,
        layout_positions,
        crate_map,
    });
}

async fn load_cached_artifact(
    blob: Vec<u8>,
    project_root: PathBuf,
) -> Result<
    (
        crate::repo_graph::RepoDependencyGraph,
        Arc<crate::repo_graph::RepoGraphRanking>,
        Arc<CachedSccs>,
        Arc<std::collections::BTreeMap<String, crate::layout::GraphLayoutPosition>>,
        Arc<std::collections::BTreeMap<PathBuf, String>>,
    ),
    String,
> {
    tokio::task::spawn_blocking(move || -> Result<_, String> {
        let artifact = crate::repo_graph::deserialize_repo_graph_artifact_bincode(&blob)?;
        let graph = crate::repo_graph::RepoDependencyGraph::from_artifact(&artifact);
        let (pagerank, sccs, layout_positions, crate_map) =
            derive_graph_caches(&graph, &project_root);
        Ok((graph, pagerank, sccs, layout_positions, crate_map))
    })
    .await
    .map_err(|e| format!("spawn_blocking join: {e}"))?
}

const GRAPH_NOT_WARMED_ERR: &str = "canonical graph not warmed yet — K8s graph warmer will populate it once the project's devcontainer image is ready";

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
        if let Some(cached) = cache.as_ref().filter(|c| c.project_path == index_tree_path) {
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

    // Treat unreadable blobs (artifact-version drift, schema migration,
    // partial writes) the same as "not warmed yet". The architect warm pass
    // will rewrite the row; surfacing the raw bincode error to the user is
    // never useful.
    let (graph, pagerank, sccs, layout_positions, crate_map) =
        load_cached_artifact(row.graph_blob, index_tree_path.clone())
            .await
            .map_err(|e| {
                tracing::warn!(
                    project_id = %project_id,
                    error = %e,
                    "load_canonical_graph: stale or unreadable graph_blob; reporting as not-warmed"
                );
                GRAPH_NOT_WARMED_ERR.to_string()
            })?;
    install_as_canonical(
        index_tree_path,
        row.commit_sha,
        graph.clone(),
        pagerank.clone(),
        sccs.clone(),
        layout_positions.clone(),
        crate_map.clone(),
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
    let range = format!("{pinned_commit}..origin/main");
    djinn_git::rev_list_count(project_root, &range).await.ok()
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
    if wanted.is_empty() {
        None
    } else {
        Some(wanted)
    }
}

async fn resolve_declared_workspaces<C: WarmContext>(
    ctx: &C,
    project_id: &str,
) -> Option<Vec<djinn_stack::Workspace>> {
    use djinn_db::ProjectRepository;
    use djinn_stack::EnvironmentConfig;

    let repo = ProjectRepository::new(ctx.db().clone(), ctx.event_bus());
    let raw = match repo.get_environment_config(project_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "ensure_canonical_graph: failed to load EnvironmentConfig workspaces"
            );
            return None;
        }
    };
    if raw.trim().is_empty() || raw.trim() == "{}" {
        return None;
    }

    let config: EnvironmentConfig = match serde_json::from_str(&raw) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "ensure_canonical_graph: failed to parse EnvironmentConfig workspaces"
            );
            return None;
        }
    };

    if config.workspaces.is_empty() {
        None
    } else {
        Some(config.workspaces)
    }
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
        signature_parts: None,
    };
    let trait_symbol = ScipSymbol {
        symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("HelperTrait".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
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
        signature_parts: None,
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
        workspace_slug: "root".to_string(),
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

    static OUT_OF_CORE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static CACHE_REUSE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(previous) => std::env::set_var(self.key, previous),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    async fn make_project(tmp: &std::path::Path) -> std::path::PathBuf {
        let project_root = tmp.join("repo");
        tokio::fs::create_dir_all(&project_root).await.unwrap();
        let run = |args: &[&str]| {
            let pr = project_root.clone();
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            async move { djinn_git::run_git_command_in(&pr, args).await.unwrap() }
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

    fn graph_artifact_blob_from_graph(
        mut graph: crate::repo_graph::RepoDependencyGraph,
    ) -> Vec<u8> {
        graph.set_layout_positions(crate::layout::derive_layout_positions(&graph));
        bincode::serialize(&graph.to_artifact()).expect("serialize graph artifact")
    }

    #[test]
    fn derive_crate_map_parses_workspace_members_and_globs() {
        let tmp = workspace_tempdir("crate-map-");
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join("crates/alpha/src")).unwrap();
        std::fs::create_dir_all(root.join("crates/beta/src")).unwrap();
        std::fs::create_dir_all(root.join("nested/gamma/src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/*", "nested/gamma"]
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("crates/alpha/Cargo.toml"),
            r#"[package]
name = "alpha-crate"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("crates/beta/Cargo.toml"),
            r#"[package]
name = "beta-crate"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("nested/gamma/Cargo.toml"),
            r#"[package]
name = "gamma-crate"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();

        let crate_map = derive_crate_map(&root);

        assert_eq!(crate_map.len(), 3);
        assert_eq!(
            crate_map.get(&root.join("crates/alpha")),
            Some(&"alpha-crate".to_string())
        );
        assert_eq!(
            crate_map.get(&root.join("crates/beta")),
            Some(&"beta-crate".to_string())
        );
        assert_eq!(
            crate_map.get(&root.join("nested/gamma")),
            Some(&"gamma-crate".to_string())
        );
    }

    #[test]
    fn derive_crate_map_returns_empty_without_workspace() {
        let tmp = workspace_tempdir("crate-map-empty-");
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "single"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();

        assert!(derive_crate_map(&root).is_empty());
    }

    /// Build a `CachedGraph` fixture with the given `git_head` and all
    /// other fields defaulted. The graph/caches/positions are zeroed-out
    /// because `is_stale` only inspects `git_head`; the other fields are
    /// never read by the test path.
    fn cached_graph_fixture(git_head: &str) -> CachedGraph {
        let graph = crate::repo_graph::RepoDependencyGraph::build(&[]);
        let pagerank = std::sync::Arc::new(graph.rank());
        let sccs = std::sync::Arc::new(CachedSccs {
            full: Vec::new(),
            file: Vec::new(),
            symbol: Vec::new(),
        });
        let layout_positions = std::sync::Arc::new(std::collections::BTreeMap::new());
        let crate_map = std::sync::Arc::new(std::collections::BTreeMap::new());
        CachedGraph {
            graph,
            project_path: std::path::PathBuf::from("/tmp/fake-project"),
            git_head: git_head.to_string(),
            pagerank,
            sccs,
            layout_positions,
            crate_map,
        }
    }

    /// kfgh/AC#4: matching HEAD → not stale.
    #[test]
    fn cached_graph_is_stale_returns_false_for_matching_head() {
        let cached = cached_graph_fixture("abc123");
        assert!(
            !cached.is_stale("abc123"),
            "matching caller head must not be flagged stale"
        );
    }

    /// kfgh/AC#4: differing HEAD → stale.
    #[test]
    fn cached_graph_is_stale_returns_true_for_differing_head() {
        let cached = cached_graph_fixture("abc123");
        assert!(
            cached.is_stale("def456"),
            "differing caller head must be flagged stale"
        );
    }

    /// kfgh/AC#4: missing HEAD (empty `git_head`) → stale. An un-warmed or
    /// un-pinned graph must never be trusted for impact preflight.
    #[test]
    fn cached_graph_is_stale_returns_true_for_missing_head() {
        let cached = cached_graph_fixture("");
        assert!(
            cached.is_stale("abc123"),
            "missing git_head must always be flagged stale"
        );
    }

    /// kfgh/AC#4: blank HEAD (whitespace-only `git_head`) → stale. The
    /// trim-then-check semantic must surface whitespace as missing.
    #[test]
    fn cached_graph_is_stale_returns_true_for_blank_head() {
        let cached = cached_graph_fixture("   \t  \n");
        assert!(
            cached.is_stale("abc123"),
            "blank/whitespace git_head must be flagged stale"
        );
    }

    /// kfgh: caller-head whitespace is trimmed before comparison. A
    /// caller that passes `"  abc123  "` matches a cached `"abc123"`.
    #[test]
    fn cached_graph_is_stale_trims_caller_head_whitespace() {
        let cached = cached_graph_fixture("abc123");
        assert!(
            !cached.is_stale("  abc123  "),
            "caller head whitespace must be trimmed before equality check"
        );
    }

    /// kfgh: `git_head_is_strictly_stale` is the shared primitive that
    /// `CachedGraph::is_stale` and downstream flows both consume. Cover
    /// the same 4 cases at the free-function level so refactors that
    /// move the instance method body to the free helper don't silently
    /// break the semantics.
    #[test]
    fn git_head_is_strictly_stale_primitive_covers_all_cases() {
        // Matching → not stale.
        assert!(!git_head_is_strictly_stale("abc123", "abc123"));
        // Differing → stale.
        assert!(git_head_is_strictly_stale("def456", "abc123"));
        // Missing git_head → stale.
        assert!(git_head_is_strictly_stale("abc123", ""));
        // Blank git_head → stale.
        assert!(git_head_is_strictly_stale("abc123", "  \t"));
        // Caller whitespace is trimmed.
        assert!(!git_head_is_strictly_stale("  abc123  ", "abc123"));
    }

    fn in_memory_graph_artifact_blob(index: &crate::scip_parser::ParsedScipIndex) -> Vec<u8> {
        let graph = crate::repo_graph::RepoDependencyGraph::try_build_with_source(
            std::slice::from_ref(index),
            None,
        )
        .expect("in-memory graph build must succeed");
        graph_artifact_blob_from_graph(graph)
    }

    fn out_of_core_graph_artifact_blob(
        index: &crate::scip_parser::ParsedScipIndex,
        storage_path: &std::path::Path,
    ) -> Vec<u8> {
        let config = crate::out_of_core::resolve_out_of_core_config(index.files.len())
            .expect("out-of-core config must engage for fixture file count");
        assert_eq!(
            config.storage_path, storage_path,
            "test must exercise the configured out-of-core storage path"
        );

        let mut store = crate::out_of_core::OutOfCoreStore::open(&config.storage_path)
            .expect("open out-of-core store");
        let shard_count = store
            .put_parsed_index(index)
            .expect("shard parsed SCIP index into out-of-core store");
        assert_eq!(
            shard_count,
            index.files.len(),
            "one out-of-core shard must be written per parsed SCIP file"
        );

        let graph = crate::repo_graph::RepoDependencyGraph::try_build_with_scip_file_iter(
            store.scip_file_iter(),
            &index.workspace_slug,
            &index.external_symbols,
            None,
        )
        .expect("out-of-core bounded graph build must succeed");
        graph_artifact_blob_from_graph(graph)
    }

    fn add_extra_occurrence_to_first_file(index: &mut crate::scip_parser::ParsedScipIndex) {
        use crate::scip_parser::{
            ScipOccurrence, ScipRange, ScipSymbol, ScipSymbolKind, ScipSymbolRole, ScipVisibility,
        };

        let symbol = "scip-rust pkg src/helper.rs `extra_helper`().".to_string();
        let occurrence = ScipOccurrence {
            symbol: symbol.clone(),
            range: ScipRange {
                start_line: 2,
                start_character: 0,
                end_line: 2,
                end_character: 12,
            },
            enclosing_range: None,
            roles: std::collections::BTreeSet::from([ScipSymbolRole::Definition]),
            syntax_kind: None,
            override_documentation: vec![],
        };
        let file = index
            .files
            .first_mut()
            .expect("test fixture must contain at least one file");
        file.definitions.push(occurrence.clone());
        file.occurrences.push(occurrence);
        file.symbols.push(ScipSymbol {
            symbol,
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("extra_helper".to_string()),
            signature: Some("fn extra_helper()".to_string()),
            documentation: vec![],
            relationships: vec![],
            visibility: Some(ScipVisibility::Public),
            signature_parts: None,
        });
    }

    #[test]
    fn test_out_of_core_graph_parity_with_in_memory() {
        let _guard = OUT_OF_CORE_ENV_LOCK.lock().unwrap();
        let tmp = workspace_tempdir("ooc-graph-parity-");
        let store_path = tmp.path().join("store");

        unsafe {
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_PATH");
        }
        let index = build_test_parsed_index_fixture();
        let in_memory_blob = in_memory_graph_artifact_blob(&index);

        unsafe {
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE", "1");
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES", "1");
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE_PATH", &store_path);
        }
        let out_of_core_blob = out_of_core_graph_artifact_blob(&index, &store_path);

        crate::graph_parity::assert_graph_artifact_blob_parity(&in_memory_blob, &out_of_core_blob)
            .expect(
                "out-of-core graph artifact must be structurally identical to in-memory artifact",
            );

        unsafe {
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_PATH");
        }
    }

    #[test]
    fn test_out_of_core_graph_diverges_on_file_change() {
        let _guard = OUT_OF_CORE_ENV_LOCK.lock().unwrap();
        let tmp = workspace_tempdir("ooc-graph-diverge-");
        let store_path = tmp.path().join("store");

        unsafe {
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_PATH");
        }
        let base_index = build_test_parsed_index_fixture();
        let in_memory_blob = in_memory_graph_artifact_blob(&base_index);

        let mut changed_index = base_index.clone();
        add_extra_occurrence_to_first_file(&mut changed_index);

        unsafe {
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE", "1");
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES", "1");
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE_PATH", &store_path);
        }
        let changed_out_of_core_blob = out_of_core_graph_artifact_blob(&changed_index, &store_path);

        let err = crate::graph_parity::assert_graph_artifact_blob_parity(
            &in_memory_blob,
            &changed_out_of_core_blob,
        )
        .expect_err("changed file occurrence must trip the out-of-core parity gate");
        assert!(
            matches!(
                err,
                crate::graph_parity::GraphArtifactBlobParityError::Diff(_)
            ),
            "expected structured graph diff for changed file occurrence, got {err:?}"
        );

        unsafe {
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_PATH");
        }
    }

    #[test]
    fn distinct_workspace_slugs_returns_each_graph_workspace_once() {
        let mut graph = build_test_graph_fixture();
        for (i, node) in graph.graph_mut_unchecked().node_weights_mut().enumerate() {
            node.workspace = Some(if i % 2 == 0 { "api" } else { "web" }.to_string());
        }

        assert_eq!(
            distinct_workspace_slugs(&graph),
            vec!["api".to_string(), "web".to_string()]
        );
    }

    #[test]
    fn distinct_workspace_slugs_skips_empty_graphs() {
        let graph = crate::repo_graph::RepoDependencyGraph::build(&[]);

        assert!(distinct_workspace_slugs(&graph).is_empty());
    }

    fn graph_artifact_blob_with_nodes(node_count: usize) -> Vec<u8> {
        let graph = build_test_graph_fixture();
        let mut artifact = graph.to_artifact();
        let template = artifact.nodes.first().expect("fixture node").clone();
        artifact.nodes.resize(node_count, template);
        bincode::serialize(&artifact).expect("serialize graph artifact")
    }

    fn warm_status(status: &str) -> crate::scip_indexer::WorkspaceWarmStatus {
        crate::scip_indexer::WorkspaceWarmStatus {
            workspace_slug: "root".to_string(),
            indexer: crate::scip_indexer::SupportedIndexer::RustAnalyzer,
            status: status.to_string(),
            detail: None,
        }
    }

    #[test]
    fn shrink_decision_ignores_missing_previous_artifact() {
        assert_eq!(detect_graph_cache_shrink_warning(None, 10, &[]), None);
    }

    #[test]
    fn shrink_decision_ignores_unreadable_previous_artifact() {
        assert_eq!(
            detect_graph_cache_shrink_warning(Some(b"not-bincode"), 10, &[warm_status("ready")]),
            None
        );
    }

    #[test]
    fn shrink_decision_ignores_shrinks_within_tolerance() {
        let blob = graph_artifact_blob_with_nodes(1_000);

        assert_eq!(
            detect_graph_cache_shrink_warning(Some(&blob), 950, &[warm_status("ready")]),
            None
        );
    }

    #[test]
    fn shrink_decision_ignores_explained_failed_or_timed_out_workspace() {
        let blob = graph_artifact_blob_with_nodes(1_000);

        assert_eq!(
            detect_graph_cache_shrink_warning(Some(&blob), 700, &[warm_status("failed")]),
            None
        );
        assert_eq!(
            detect_graph_cache_shrink_warning(Some(&blob), 700, &[warm_status("timed_out")]),
            None
        );
    }

    #[test]
    fn shrink_decision_ignores_ready_with_quarantine_workspace() {
        // `ready_with_quarantine` means a below-workspace partition was
        // quarantined while the rest succeeded — the smaller graph is
        // explained and must not append a misleading shrink warning.
        let blob = graph_artifact_blob_with_nodes(1_000);

        assert_eq!(
            detect_graph_cache_shrink_warning(
                Some(&blob),
                700,
                &[warm_status("ready_with_quarantine")]
            ),
            None
        );
    }

    #[test]
    fn shrink_decision_warns_on_unexplained_shrink_with_only_ready() {
        // Only `ready` statuses (no quarantine/explanation) → shrink is
        // unexplained and the warning MUST fire.
        let blob = graph_artifact_blob_with_nodes(1_000);

        let warning = detect_graph_cache_shrink_warning(Some(&blob), 700, &[warm_status("ready")])
            .expect("warning decision");
        assert_eq!(warning.delta, 300);
    }

    #[test]
    fn shrink_decision_warns_on_unexplained_shrink_beyond_tolerance() {
        let blob = graph_artifact_blob_with_nodes(1_000);

        let warning = detect_graph_cache_shrink_warning(Some(&blob), 700, &[warm_status("ready")])
            .expect("warning decision");
        assert_eq!(warning.old_node_count, 1_000);
        assert_eq!(warning.new_node_count, 700);
        assert_eq!(warning.delta, 300);
        assert_eq!(
            warning.tolerance_min_absolute_delta,
            GRAPH_CACHE_SHRINK_MIN_ABSOLUTE_DELTA
        );
        assert_eq!(
            warning.tolerance_min_percent,
            GRAPH_CACHE_SHRINK_MIN_PERCENT
        );
        assert!(
            warning
                .workspace_status_summary
                .contains("root:rust-analyzer=ready")
        );
    }

    #[tokio::test]
    async fn persist_freshness_stamps_failures_and_retires_codeless_sentinel() {
        use djinn_db::{
            CODELESS_WORKSPACE_SLUG, ProjectWorkspaceGraphRepository, ProjectWorkspaceGraphUpsert,
        };

        let db = create_test_db();
        db.ensure_initialized().await.unwrap();
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create_with_id("p1", "p1", "test", "p1")
            .await
            .unwrap();
        let repo = ProjectWorkspaceGraphRepository::new(db.clone());
        // Pre-existing state from a misfiring gate + an older partial warm:
        // a code-less sentinel and a stale "ready" row for `server`.
        repo.upsert_many(&[
            ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: CODELESS_WORKSPACE_SLUG,
                commit_sha: "no-code",
                status: "ready",
            },
            ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "server",
                commit_sha: "old-commit",
                status: "ready",
            },
        ])
        .await
        .unwrap();

        // New warm at `new-commit`: graph carries `ui` only; the `server`
        // indexer timed out.
        let mut graph = build_test_graph_fixture();
        for node in graph.graph_mut_unchecked().node_weights_mut() {
            node.workspace = Some("ui".to_string());
        }
        let statuses = vec![
            crate::scip_indexer::WorkspaceWarmStatus {
                workspace_slug: "ui".to_string(),
                indexer: crate::scip_indexer::SupportedIndexer::TypeScript,
                status: "ready".to_string(),
                detail: None,
            },
            crate::scip_indexer::WorkspaceWarmStatus {
                workspace_slug: "server".to_string(),
                indexer: crate::scip_indexer::SupportedIndexer::RustAnalyzer,
                status: "timed_out".to_string(),
                detail: Some("indexer timed out".to_string()),
            },
        ];

        let ctx = TestWarmContext::new(db);
        persist_workspace_graph_freshness_best_effort(&ctx, "p1", "new-commit", &graph, &statuses)
            .await;

        let ui = repo.get("p1", "ui").await.unwrap().expect("ui row");
        assert_eq!(ui.status, "ready");
        assert_eq!(ui.commit_sha, "new-commit");

        // The stale `server` "ready" row must be overwritten with the real
        // outcome at THIS commit — not left lying about freshness.
        let server = repo.get("p1", "server").await.unwrap().expect("server row");
        assert_eq!(server.status, "timed_out");
        assert_eq!(server.commit_sha, "new-commit");

        // A successful real warm contradicts the code-less sentinel.
        assert!(
            repo.get("p1", CODELESS_WORKSPACE_SLUG)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn distinct_workspace_slugs_falls_back_to_root_for_non_empty_legacy_graph() {
        let mut graph = build_test_graph_fixture();
        for node in graph.graph_mut_unchecked().node_weights_mut() {
            node.workspace = None;
        }

        assert_eq!(distinct_workspace_slugs(&graph), vec!["root".to_string()]);
    }

    #[test]
    fn route_extraction_post_processor_respects_disabled_env_gate() {
        let _guard = crate::route_extraction::ROUTE_DETECTION_ENV_LOCK
            .lock()
            .unwrap();
        let tmp = workspace_tempdir("route-extraction-gate-");
        let project_root = tmp.path();
        std::fs::create_dir_all(project_root.join("src")).unwrap();
        std::fs::write(
            project_root.join("src/routes.rs"),
            "Router::new().route(\"/api/agents\", get(list_agents))",
        )
        .unwrap();

        let mut index = build_test_parsed_index_fixture();
        index.files.clear();
        index.files.push(crate::scip_parser::ScipFile {
            language: "rust".to_string(),
            relative_path: PathBuf::from("src/routes.rs"),
            definitions: vec![],
            references: vec![],
            occurrences: vec![],
            symbols: vec![],
        });
        let mut graph = crate::repo_graph::RepoDependencyGraph::build(&[index]);

        for disabled_value in ["0", "false", "no", "off", " OFF "] {
            // SAFETY: test-only env mutation is serialized by ROUTE_DETECTION_ENV_LOCK.
            unsafe { std::env::set_var("DJINN_ROUTE_DETECTION", disabled_value) };
            let report = run_route_extraction_post_processor(&mut graph, project_root)
                .expect("disabled route extraction should not fail");

            assert!(
                report.is_none(),
                "DJINN_ROUTE_DETECTION={disabled_value:?} must skip the post-processor"
            );
            assert!(
                graph
                    .graph()
                    .node_weights()
                    .all(|node| { node.kind != crate::repo_graph::RepoGraphNodeKind::Route })
            );
        }
        unsafe { std::env::remove_var("DJINN_ROUTE_DETECTION") };
    }

    #[test]
    fn route_extraction_post_processor_counts_file_failures_without_poisoning_graph() {
        let _guard = crate::route_extraction::ROUTE_DETECTION_ENV_LOCK
            .lock()
            .unwrap();
        unsafe { std::env::remove_var("DJINN_ROUTE_DETECTION") };
        let tmp = workspace_tempdir("route-extraction-failure-");
        let project_root = tmp.path();

        let mut index = build_test_parsed_index_fixture();
        index.files.clear();
        index.files.push(crate::scip_parser::ScipFile {
            language: "rust".to_string(),
            relative_path: PathBuf::from("server/src/missing.rs"),
            definitions: vec![],
            references: vec![],
            occurrences: vec![],
            symbols: vec![],
        });
        let mut graph = crate::repo_graph::RepoDependencyGraph::build(&[index]);

        let report = run_route_extraction_post_processor(&mut graph, project_root)
            .expect("default-on route extraction should not fail")
            .expect("default-on route extraction should run");

        assert_eq!(report.route_nodes_added, 0);
        assert_eq!(report.handles_route_edges_added, 0);
        assert_eq!(report.fetches_edges_added, 0);
        assert_eq!(
            report.skipped_files,
            vec![PathBuf::from("server/src/missing.rs")]
        );
        assert_eq!(report.file_failures.len(), 1);
        assert!(
            graph
                .graph()
                .node_weights()
                .all(|node| { node.kind != crate::repo_graph::RepoGraphNodeKind::Route })
        );
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

        let head_sha = djinn_git::head_commit_sha(&project_root)
            .await
            .expect("resolve HEAD commit");

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

        let head_sha = djinn_git::head_commit_sha(&project_root)
            .await
            .expect("resolve HEAD commit");

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
            let (pagerank, sccs, layout_positions, crate_map) =
                derive_graph_caches(&graph, &project_root);
            let mut cache = GRAPH_CACHE.write().await;
            *cache = Some(CachedGraph {
                graph,
                project_path: index_tree_path.clone(),
                git_head: stale_sha,
                pagerank,
                sccs,
                layout_positions,
                crate_map,
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

    // -------------------------------------------------------------------
    // Declared EnvironmentConfig workspace overlay — k62f regression.
    //
    // `ensure_canonical_graph` reads `projects.environment_config` and
    // hands the parsed `Workspace` list to
    // `run_indexers_already_locked(..., declared_workspaces)` purely as a
    // log-only / additive overlay. Marker + discovery remain
    // authoritative for what is actually planned and indexed. These
    // tests pin the contract for that plumbing so a refactor can't
    // accidentally make the declared list authoritative (or silently
    // drop it).
    // -------------------------------------------------------------------

    /// Round-trip an `EnvironmentConfig` blob through the DB and confirm
    /// `resolve_declared_workspaces` returns the declared list in a
    /// shape ready to pass straight into
    /// `run_indexers_already_locked(.., declared_workspaces)`.
    #[tokio::test]
    async fn resolve_declared_workspaces_loads_persisted_workspaces_as_overlay_input() {
        let tmp = workspace_tempdir("canonical-graph-");
        let db = create_test_db();
        let ctx = TestWarmContext::new(db.clone());
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("declared-overlay", "test", "declared-overlay")
            .await
            .expect("create project");

        // Persist a config with slug/name/tags populated for two
        // workspaces — this is the post-2026-04-22 shape the k62f
        // epic lands. `from_stack` is the only producer that derives
        // `slug` today; we set it explicitly here so the test is
        // independent of stack-detection helpers.
        let cfg = djinn_stack::EnvironmentConfig {
            schema_version: djinn_stack::SCHEMA_VERSION,
            source: djinn_stack::ConfigSource::UserEdited,
            workspaces: vec![
                djinn_stack::Workspace {
                    slug: Some("server".to_string()),
                    name: Some("API server".to_string()),
                    tags: vec!["backend".to_string(), "rust".to_string()],
                    root: "server".to_string(),
                    language: "rust".to_string(),
                    toolchain: Some("stable".to_string()),
                    version: None,
                    package_manager: None,
                },
                djinn_stack::Workspace {
                    slug: Some("ui".to_string()),
                    name: Some("Frontend".to_string()),
                    tags: vec!["frontend".to_string()],
                    root: "ui".to_string(),
                    language: "node".to_string(),
                    toolchain: None,
                    version: Some("22".to_string()),
                    package_manager: Some("pnpm".to_string()),
                },
            ],
            ..djinn_stack::EnvironmentConfig::empty()
        };
        proj_repo
            .set_environment_config(&project.id, &serde_json::to_string(&cfg).unwrap())
            .await
            .expect("set environment_config");

        let declared = resolve_declared_workspaces(&ctx, &project.id)
            .await
            .expect("declared list");

        // Same length, same slugs, same names, same tags — i.e. the
        // resolver hands the overlay through verbatim and the
        // values are usable as
        // `run_indexers_already_locked(.., Some(&declared))`.
        assert_eq!(declared.len(), 2, "two declared workspaces round-trip");
        assert_eq!(declared[0].slug.as_deref(), Some("server"));
        assert_eq!(declared[0].name.as_deref(), Some("API server"));
        assert_eq!(declared[0].tags, vec!["backend", "rust"]);
        assert_eq!(declared[0].root, "server");
        assert_eq!(declared[0].language, "rust");
        assert_eq!(declared[1].slug.as_deref(), Some("ui"));
        assert_eq!(declared[1].name.as_deref(), Some("Frontend"));
        assert_eq!(declared[1].tags, vec!["frontend"]);
        assert_eq!(declared[1].version.as_deref(), Some("22"));
        assert_eq!(declared[1].package_manager.as_deref(), Some("pnpm"));

        // Sanity-check the contract from the other side: the same Vec
        // is the type `run_indexers_already_locked` expects for its
        // `declared_workspaces` parameter. We can't actually run the
        // indexer from a unit test (no rust-analyzer / scip-typescript
        // on PATH), but we can confirm the call site is type-correct
        // by handing the Vec to the same function in a no-op
        // invocation against an empty project root.
        let output_root = tmp.path().join("scip-out");
        std::fs::create_dir_all(&output_root).unwrap();
        let result = crate::scip_indexer::run_indexers_already_locked(
            tmp.path(),
            &output_root,
            None,
            None,
            Some(&declared),
        )
        .await;
        // An empty tree with no indexers on PATH plans zero indexers
        // and returns Ok. The point of the call is purely to prove
        // the declared list flows through the type system into the
        // overlay parameter.
        assert!(
            result.is_ok(),
            "declared overlay must be accepted by run_indexers_already_locked; got {result:?}"
        );
    }

    /// The column default is `'{}'`; `resolve_declared_workspaces` must
    /// treat that as "no overlay" so the indexer path falls back to
    /// marker/discovery without forcing a synthetic empty list.
    #[tokio::test]
    async fn resolve_declared_workspaces_returns_none_for_default_empty_config() {
        let db = create_test_db();
        let ctx = TestWarmContext::new(db.clone());
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("declared-empty", "test", "declared-empty")
            .await
            .expect("create project");

        // The default `EnvironmentConfig::empty()` serialises to
        // `{"schema_version":1, ...}` — the `{}` literal short-circuits
        // *before* we even attempt to parse. Verify both shapes.
        let raw_empty = proj_repo
            .get_environment_config(&project.id)
            .await
            .expect("get default config");
        assert!(
            raw_empty.as_deref() == Some("{}") || raw_empty.as_deref() == Some(""),
            "migration-10 column default should be `{{}}` or empty; got {raw_empty:?}"
        );
        assert!(
            resolve_declared_workspaces(&ctx, &project.id)
                .await
                .is_none(),
            "column default must resolve to None (no overlay)"
        );

        // An empty (zero-workspace) EnvironmentConfig — not the column
        // default, but a real config with an empty `workspaces` list —
        // also returns None. This is the "user saved an empty config"
        // path and must not be confused with a request to index
        // nothing.
        let empty_cfg = djinn_stack::EnvironmentConfig::empty();
        proj_repo
            .set_environment_config(&project.id, &serde_json::to_string(&empty_cfg).unwrap())
            .await
            .expect("set empty config");
        assert!(
            resolve_declared_workspaces(&ctx, &project.id)
                .await
                .is_none(),
            "EnvironmentConfig::empty() with no workspaces must resolve to None"
        );
    }

    /// A declared workspace list must not turn into a hard filter on
    /// the planned indexer commands. Even when the declared list is
    /// non-empty, `run_indexers_already_locked` should be callable
    /// against an empty project root with no indexers available and
    /// return Ok — the overlay is log-only and additive, never
    /// authoritative.
    #[tokio::test]
    async fn declared_workspaces_overlay_is_log_only_not_authoritative() {
        let tmp = workspace_tempdir("canonical-graph-");
        let db = create_test_db();
        let ctx = TestWarmContext::new(db.clone());
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create(
                "declared-overlay-log-only",
                "test",
                "declared-overlay-log-only",
            )
            .await
            .expect("create project");

        // Declare a workspace that doesn't exist on disk. The overlay
        // must not crash or short-circuit the run; the divergence is
        // expected to surface as a structured warning (covered by the
        // indexer-side tests in `scip_indexer/indexing.rs`).
        let cfg = djinn_stack::EnvironmentConfig {
            schema_version: djinn_stack::SCHEMA_VERSION,
            source: djinn_stack::ConfigSource::UserEdited,
            workspaces: vec![djinn_stack::Workspace {
                slug: Some("phantom".to_string()),
                name: Some("Does not exist on disk".to_string()),
                tags: vec!["ghost".to_string()],
                root: "this/path/does/not/exist".to_string(),
                language: "rust".to_string(),
                toolchain: None,
                version: None,
                package_manager: None,
            }],
            ..djinn_stack::EnvironmentConfig::empty()
        };
        proj_repo
            .set_environment_config(&project.id, &serde_json::to_string(&cfg).unwrap())
            .await
            .expect("set environment_config");

        let declared = resolve_declared_workspaces(&ctx, &project.id)
            .await
            .expect("declared list present");

        // Empty project root + no indexers on PATH = zero planned
        // commands. The overlay is ignored for planning — marker
        // + discovery are authoritative — so the run returns Ok
        // rather than tripping on the phantom workspace.
        let project_root = tmp.path().join("empty");
        tokio::fs::create_dir_all(&project_root).await.unwrap();
        let output_root = tmp.path().join("scip-out");
        let result = crate::scip_indexer::run_indexers_already_locked(
            &project_root,
            &output_root,
            None,
            None,
            Some(&declared),
        )
        .await;
        assert!(
            result.is_ok(),
            "declared overlay must not block the run; got {result:?}"
        );
    }

    // -------------------------------------------------------------------
    // Incremental == full equivalence regression gate.
    //
    // Two consecutive `ensure_canonical_graph` calls on the same project
    // + commit, with the in-memory GRAPH_CACHE cleared between them,
    // must produce graph-artifact blobs that satisfy
    // `assert_graph_artifact_blob_parity`.  The first call seeds the DB
    // cache with a fixture graph artifact (simulating a full/cold
    // pipeline run); the second call exercises the DB cache-hit
    // (warm/incremental) path.
    //
    // The negative test proves the harness is a real gate: two
    // *different* fixture graphs must fail the parity check with a
    // `GraphArtifactBlobParityError::Diff` variant.
    // -------------------------------------------------------------------

    /// Positive equivalence test: cold-warm and warm (incremental) blobs
    /// for the same commit must pass `assert_graph_artifact_blob_parity`.
    #[tokio::test]
    async fn incremental_full_equivalence_same_commit() {
        let tmp = workspace_tempdir("incremental-equiv-");
        let project_root = make_project(tmp.path()).await;
        let db = create_test_db();
        let ctx = TestWarmContext::new(db.clone());
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("test-equiv", "test", "test-equiv")
            .await
            .expect("create project");

        // Resolve HEAD commit SHA from the tempdir git repo.
        let commit_sha = djinn_git::head_commit_sha(&project_root)
            .await
            .expect("resolve HEAD commit");

        // Seed the DB cache with a fixture graph artifact — simulates
        // the output of a full (cold) pipeline run.
        let graph = build_test_graph_fixture();
        let seeded_blob =
            bincode::serialize(&graph.to_artifact()).expect("serialize fixture graph");
        let cache_repo = RepoGraphCacheRepository::new(db.clone());
        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: &commit_sha,
                graph_blob: &seeded_blob,
            })
            .await
            .expect("seed cache");

        // --- Cold warm: ensure_canonical_graph reads from DB cache ---
        let result = ensure_canonical_graph(
            &ctx,
            &project.id,
            &project_root,
            ArchitectWarmToken::for_tests(),
        )
        .await;
        assert!(result.is_ok(), "cold warm failed: {result:?}");

        let cold_blob = cache_repo
            .get(&project.id, &commit_sha)
            .await
            .expect("get cold blob")
            .expect("cold blob should exist in DB")
            .graph_blob;

        // --- Drop in-memory GRAPH_CACHE so the next call must re-read DB ---
        clear_test_caches().await;

        // --- Warm (incremental) warm: DB cache-hit path ---
        let result = ensure_canonical_graph(
            &ctx,
            &project.id,
            &project_root,
            ArchitectWarmToken::for_tests(),
        )
        .await;
        assert!(result.is_ok(), "warm (incremental) warm failed: {result:?}");

        let warm_blob = cache_repo
            .get(&project.id, &commit_sha)
            .await
            .expect("get warm blob")
            .expect("warm blob should exist in DB")
            .graph_blob;

        // --- Parity gate: cold and warm blobs must match ---
        crate::graph_parity::assert_graph_artifact_blob_parity(&cold_blob, &warm_blob)
            .expect("incremental == full parity violation: cold and warm blobs must match");
    }

    fn write_fake_rust_analyzer(tmp: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        use protobuf::{EnumOrUnknown, Message};
        use scip::types::{Document, Index, Occurrence, SymbolInformation, symbol_information};

        let fixture_path = tmp.join("fixture.scip");
        let mut doc = Document::new();
        doc.relative_path = "src/lib.rs".to_string();
        doc.language = "rust".to_string();
        doc.occurrences = vec![Occurrence {
            range: vec![0, 7, 13],
            symbol: "scip-rust test src/lib.rs `answer`().".to_string(),
            symbol_roles: scip::types::SymbolRole::Definition as i32,
            ..Occurrence::new()
        }];
        doc.symbols = vec![SymbolInformation {
            symbol: "scip-rust test src/lib.rs `answer`().".to_string(),
            display_name: "answer".to_string(),
            kind: EnumOrUnknown::new(symbol_information::Kind::Function),
            ..SymbolInformation::new()
        }];

        let mut index = Index::new();
        index.documents = vec![doc];
        std::fs::write(
            &fixture_path,
            index.write_to_bytes().expect("encode SCIP fixture"),
        )
        .expect("write SCIP fixture");

        let fake_bin = tmp.join("fake-bin");
        std::fs::create_dir_all(&fake_bin).expect("create fake indexer bin dir");
        let script_path = fake_bin.join("rust-analyzer");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
set -eu
out=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output" ]; then
    shift
    out="$1"
  fi
  shift || true
done
if [ -z "$out" ]; then
  echo "missing --output" >&2
  exit 2
fi
mkdir -p "$(dirname "$out")"
cp "$DJINN_TEST_SCIP_FIXTURE" "$out"
"#,
        )
        .expect("write fake rust-analyzer");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path)
                .expect("fake rust-analyzer metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).expect("chmod fake rust-analyzer");
        }

        (fake_bin, fixture_path)
    }

    /// Out-of-core warm regression: with the out-of-core flag forced on
    /// (threshold=0), a rebuild for the same commit must persist a graph blob
    /// byte-identical to the default in-memory warm path.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn out_of_core_warm_produces_identical_graph_blob() {
        let _env_lock = OUT_OF_CORE_ENV_LOCK.lock().unwrap();
        let _ooc_flag = EnvVarGuard::remove("DJINN_GRAPH_OUT_OF_CORE");
        let _ooc_min_nodes = EnvVarGuard::remove("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES");
        let _ooc_path = EnvVarGuard::remove("DJINN_GRAPH_OUT_OF_CORE_PATH");

        let tmp = workspace_tempdir("ooc-warm-parity-");
        let project_root = make_project(tmp.path()).await;
        // `Cargo.toml` must declare a `[workspace]` section so the
        // RustAnalyzer indexer's workspace discovery picks up this
        // fixture. Without `[workspace]`, no Rust workspace is
        // discovered, the fake rust-analyzer never runs, the parsed
        // SCIP set is empty, and `ensure_canonical_graph` skips the
        // cache upsert (the `node_count == 0` guard at the bottom of
        // the warm pipeline).
        tokio::fs::write(
            project_root.join("Cargo.toml"),
            "[package]\nname = \"ooc_warm_parity\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
        )
        .await
        .expect("write Cargo.toml");
        tokio::fs::create_dir_all(project_root.join("src"))
            .await
            .expect("create src dir");
        tokio::fs::write(
            project_root.join("src/lib.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        )
        .await
        .expect("write src/lib.rs");
        let commit_output = djinn_git::run_git_command_in(
            &project_root,
            vec!["add".into(), "Cargo.toml".into(), "src/lib.rs".into()],
        )
        .await
        .expect("git add rust fixture");
        assert!(
            commit_output.code == 0,
            "git add rust fixture failed: {commit_output:?}"
        );
        let commit_output = djinn_git::run_git_command_in(
            &project_root,
            vec![
                "commit".into(),
                "-q".into(),
                "-m".into(),
                "add rust fixture".into(),
            ],
        )
        .await
        .expect("git commit rust fixture");
        assert!(
            commit_output.code == 0,
            "git commit rust fixture failed: {commit_output:?}"
        );

        let (fake_bin, fixture_path) = write_fake_rust_analyzer(tmp.path());
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined_path =
            std::env::join_paths(std::iter::once(fake_bin).chain(std::env::split_paths(&path)))
                .expect("join PATH with fake rust-analyzer");
        let _path_guard = EnvVarGuard::set("PATH", joined_path);
        let _fixture_guard = EnvVarGuard::set("DJINN_TEST_SCIP_FIXTURE", &fixture_path);

        let db = create_test_db();
        let ctx = TestWarmContext::new(db.clone());
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("test-ooc-warm-parity", "test", "test-ooc-warm-parity")
            .await
            .expect("create project");
        let cache_repo = RepoGraphCacheRepository::new(db.clone());

        let result = ensure_canonical_graph(
            &ctx,
            &project.id,
            &project_root,
            ArchitectWarmToken::for_tests(),
        )
        .await;
        assert!(result.is_ok(), "in-memory warm failed: {result:?}");

        let commit_sha = djinn_git::head_commit_sha(&project_root)
            .await
            .expect("resolve HEAD commit");
        let cold_blob = cache_repo
            .get(&project.id, &commit_sha)
            .await
            .expect("get in-memory warm blob")
            .expect("in-memory warm blob should exist")
            .graph_blob;

        clear_test_caches().await;
        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: &commit_sha,
                graph_blob: b"stale graph blob that forces a rebuild",
            })
            .await
            .expect("poison cache row to force out-of-core rebuild");
        clear_test_caches().await;

        let _ooc_flag = EnvVarGuard::set("DJINN_GRAPH_OUT_OF_CORE", "1");
        let _ooc_min_nodes = EnvVarGuard::set("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES", "0");
        // Pin the out-of-core storage path under the test tempdir so the
        // shard store stays inside the workspace's writable region and
        // gets cleaned up alongside the test fixtures on drop. Without
        // this, the default path falls back to `/tmp/djinn-ooc-<pid>`,
        // which is outside the sandbox's allowed write list and
        // produces intermittent OOC open failures.
        let ooc_storage_path = tmp.path().join("ooc-store");
        let _ooc_path = EnvVarGuard::set("DJINN_GRAPH_OUT_OF_CORE_PATH", &ooc_storage_path);

        let result = ensure_canonical_graph(
            &ctx,
            &project.id,
            &project_root,
            ArchitectWarmToken::for_tests(),
        )
        .await;
        assert!(result.is_ok(), "out-of-core warm failed: {result:?}");

        let warm_blob = cache_repo
            .get(&project.id, &commit_sha)
            .await
            .expect("get out-of-core warm blob")
            .expect("out-of-core warm blob should exist")
            .graph_blob;

        crate::graph_parity::assert_graph_artifact_blob_parity(&cold_blob, &warm_blob)
            .expect("out-of-core warm graph blob must match in-memory warm graph blob");
    }

    /// Cache-reuse warm regression: with the cache-reuse flag enabled,
    /// a rebuild for the same commit must persist a graph blob identical
    /// to the default non-cache-reuse warm path.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn cache_reuse_produces_identical_graph_blob() {
        let _env_lock = CACHE_REUSE_ENV_LOCK.lock().unwrap();

        // Remove any pre-existing cache-reuse env var.
        unsafe {
            std::env::remove_var("DJINN_GRAPH_CACHE_REUSE_ENABLED");
            std::env::remove_var("DJINN_CACHE_REUSE_ENABLED");
            std::env::remove_var("CACHE_REUSE_ENABLED");
        }

        let tmp = workspace_tempdir("cache-reuse-parity-");
        let project_root = make_project(tmp.path()).await;
        // `Cargo.toml` must declare a `[workspace]` section so the
        // RustAnalyzer indexer's workspace discovery picks up this
        // fixture. Without `[workspace]`, no Rust workspace is
        // discovered, the fake rust-analyzer never runs, the parsed
        // SCIP set is empty, and `ensure_canonical_graph` skips the
        // cache upsert (the `node_count == 0` guard at the bottom of
        // the warm pipeline).
        tokio::fs::write(
            project_root.join("Cargo.toml"),
            "[package]\nname = \"cache_reuse_parity\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
        )
        .await
        .expect("write Cargo.toml");
        tokio::fs::create_dir_all(project_root.join("src"))
            .await
            .expect("create src dir");
        tokio::fs::write(
            project_root.join("src/lib.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        )
        .await
        .expect("write src/lib.rs");
        let commit_output = djinn_git::run_git_command_in(
            &project_root,
            vec!["add".into(), "Cargo.toml".into(), "src/lib.rs".into()],
        )
        .await
        .expect("git add rust fixture");
        assert!(
            commit_output.code == 0,
            "git add rust fixture failed: {commit_output:?}"
        );
        let commit_output = djinn_git::run_git_command_in(
            &project_root,
            vec![
                "commit".into(),
                "-q".into(),
                "-m".into(),
                "add rust fixture".into(),
            ],
        )
        .await
        .expect("git commit rust fixture");
        assert!(
            commit_output.code == 0,
            "git commit rust fixture failed: {commit_output:?}"
        );

        let (fake_bin, fixture_path) = write_fake_rust_analyzer(tmp.path());
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined_path =
            std::env::join_paths(std::iter::once(fake_bin).chain(std::env::split_paths(&path)))
                .expect("join PATH with fake rust-analyzer");

        // Save previous env vars so we can restore them after the test.
        let prev_path = std::env::var_os("PATH");
        let prev_fixture = std::env::var_os("DJINN_TEST_SCIP_FIXTURE");

        unsafe {
            std::env::set_var("PATH", joined_path);
            std::env::set_var("DJINN_TEST_SCIP_FIXTURE", &fixture_path);
        }

        let db = create_test_db();
        let ctx = TestWarmContext::new(db.clone());
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("test-cache-reuse-parity", "test", "test-cache-reuse-parity")
            .await
            .expect("create project");
        let cache_repo = RepoGraphCacheRepository::new(db.clone());

        // --- Cold warm: no cache-reuse env var ---
        let result = ensure_canonical_graph(
            &ctx,
            &project.id,
            &project_root,
            ArchitectWarmToken::for_tests(),
        )
        .await;
        assert!(result.is_ok(), "cold warm failed: {result:?}");

        let commit_sha = djinn_git::head_commit_sha(&project_root)
            .await
            .expect("resolve HEAD commit");
        let cold_blob = cache_repo
            .get(&project.id, &commit_sha)
            .await
            .expect("get cold blob")
            .expect("cold blob should exist")
            .graph_blob;

        // --- Poison cache and clear in-memory caches to force rebuild ---
        clear_test_caches().await;
        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: &commit_sha,
                graph_blob: b"stale graph blob that forces a rebuild",
            })
            .await
            .expect("poison cache row to force cache-reuse rebuild");
        clear_test_caches().await;

        // --- Warm with cache-reuse enabled ---
        unsafe {
            std::env::set_var("DJINN_GRAPH_CACHE_REUSE_ENABLED", "1");
        }

        let result = ensure_canonical_graph(
            &ctx,
            &project.id,
            &project_root,
            ArchitectWarmToken::for_tests(),
        )
        .await;
        assert!(result.is_ok(), "cache-reuse warm failed: {result:?}");

        let warm_blob = cache_repo
            .get(&project.id, &commit_sha)
            .await
            .expect("get warm blob")
            .expect("warm blob should exist")
            .graph_blob;

        // --- Restore env vars ---
        unsafe {
            std::env::remove_var("DJINN_GRAPH_CACHE_REUSE_ENABLED");
            std::env::remove_var("DJINN_CACHE_REUSE_ENABLED");
            std::env::remove_var("CACHE_REUSE_ENABLED");
            match prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
            match prev_fixture {
                Some(f) => std::env::set_var("DJINN_TEST_SCIP_FIXTURE", f),
                None => std::env::remove_var("DJINN_TEST_SCIP_FIXTURE"),
            }
        }

        crate::graph_parity::assert_graph_artifact_blob_parity(&cold_blob, &warm_blob)
            .expect("cache-reuse warm graph blob must match cold warm graph blob");
    }

    /// Negative equivalence test: two different fixture graphs (base vs.
    /// base + extra file) seeded at different commit SHAs must fail the
    /// parity check with a `Diff` variant.  This proves the harness is
    /// a real gate, not a tautology.
    #[tokio::test]
    async fn incremental_full_equivalence_differs_for_different_commits() {
        let tmp = workspace_tempdir("incremental-neg-");
        let _project_root = make_project(tmp.path()).await;
        let db = create_test_db();
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("test-equiv-neg", "test", "test-equiv-neg")
            .await
            .expect("create project");

        let cache_repo = RepoGraphCacheRepository::new(db.clone());

        // Commit A: base fixture graph.
        let graph_a = build_test_graph_fixture();
        let blob_a = bincode::serialize(&graph_a.to_artifact()).expect("serialize graph A");
        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: "commit-a-sha",
                graph_blob: &blob_a,
            })
            .await
            .expect("seed cache A");

        // Commit B: base fixture + an extra file — different graph.
        let mut index_b = build_test_parsed_index_fixture();
        use crate::scip_parser::{ScipOccurrence, ScipRange, ScipSymbolRole};
        index_b.files.push(crate::scip_parser::ScipFile {
            language: "rust".to_string(),
            relative_path: std::path::PathBuf::from("src/extra.rs"),
            definitions: vec![ScipOccurrence {
                symbol: "scip-rust pkg src/extra.rs `extra_fn`().".to_string(),
                range: ScipRange {
                    start_line: 0,
                    start_character: 0,
                    end_line: 0,
                    end_character: 9,
                },
                enclosing_range: None,
                roles: std::collections::BTreeSet::from([ScipSymbolRole::Definition]),
                syntax_kind: None,
                override_documentation: vec![],
            }],
            references: vec![],
            occurrences: vec![],
            symbols: vec![],
        });
        let graph_b = crate::repo_graph::RepoDependencyGraph::build(&[index_b]);
        let blob_b = bincode::serialize(&graph_b.to_artifact()).expect("serialize graph B");
        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: "commit-b-sha",
                graph_blob: &blob_b,
            })
            .await
            .expect("seed cache B");

        // Read both blobs back from DB.
        let cold_blob = cache_repo
            .get(&project.id, "commit-a-sha")
            .await
            .expect("get blob A")
            .expect("blob A should exist")
            .graph_blob;
        let different_blob = cache_repo
            .get(&project.id, "commit-b-sha")
            .await
            .expect("get blob B")
            .expect("blob B should exist")
            .graph_blob;

        // Parity must fail with Diff variant — proves the gate is real.
        let err =
            crate::graph_parity::assert_graph_artifact_blob_parity(&cold_blob, &different_blob)
                .expect_err("different graphs must produce a parity error");
        assert!(
            matches!(
                err,
                crate::graph_parity::GraphArtifactBlobParityError::Diff(_)
            ),
            "expected Diff variant, got {err:?}"
        );
    }
}
