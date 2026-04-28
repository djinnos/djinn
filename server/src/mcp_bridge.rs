/// Bridge trait implementations: connect djinn-control-plane's abstract traits to
/// the server's concrete actor handles and managers.
///
/// Newtypes are required for CoordinatorHandle, SlotPoolHandle, and LspManager
/// because both the trait (djinn-control-plane) and the implementor (djinn-agent) are
/// external to the server — orphan rule.
/// AppState is a server-local type so it implements RuntimeOps and GitOps directly.
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use djinn_git::{GitActorHandle, GitError};
use djinn_control_plane::bridge::{
    ApiSurfaceEntry, BoundaryRule, BoundaryViolation, CallerRef, ChangeKind, ChangedRange,
    CoordinatorOps, CoordinatorStatus, CycleGroup, CycleMember, DeadSymbolEntry, DeprecatedHit,
    DetectedChangesResult, DetectedTouchedSymbol, DiffTouchesResult, EdgeCategory, EdgeEntry,
    GitOps, GraphNeighbor, GraphStatus, HotPathHit, HotspotEntry, ImpactEntry, ImpactResult,
    LspOps, LspWarning, MetricsAtResult, ModelPoolStatus, NeighborsResult, OrphanEntry,
    PagerankTier, PathHop, PathResult, PoolStatus, ProcessRef, ProjectCtx, RankedNode,
    RelatedSymbol, RepoGraphOps, ResolveOutcome, RunningTaskInfo, RuntimeOps, SearchHit,
    SemanticQueryEmbedding, SlotPoolOps, SnapshotEdge, SnapshotNode, SnapshotPayload,
    SymbolAtHit, SymbolContext, SymbolDescription, SymbolNode, TouchedSymbol,
};
use petgraph::visit::EdgeRef;
use djinn_agent::actors::coordinator::CoordinatorHandle;
use djinn_agent::actors::slot::SlotPoolHandle;
use djinn_agent::lsp::LspManager;

pub(crate) mod graph_neighbors;
pub(crate) mod hybrid_search;

use self::graph_neighbors::{
    build_method_metadata, build_related_symbol, classify_edge_category, format_node_key,
    group_impact_by_file, group_neighbors_by_file, kind_label_for_node, read_symbol_content,
    resolve_node_or_err, resolve_node_with_hint,
};

// ── Newtype wrappers ───────────────────────────────────────────────────────────

struct CoordinatorBridge(pub CoordinatorHandle);
struct SlotPoolBridge(pub SlotPoolHandle);
struct LspBridge(pub LspManager);

// ── CoordinatorBridge → CoordinatorOps ───────────────────────────────────────

#[async_trait]
impl CoordinatorOps for CoordinatorBridge {
    fn get_status(&self) -> Result<CoordinatorStatus, String> {
        let s = self.0.get_status().map_err(|e| e.to_string())?;
        Ok(CoordinatorStatus {
            tasks_dispatched: s.tasks_dispatched,
            sessions_recovered: s.sessions_recovered,
            epic_throughput: s.epic_throughput,
            pr_errors: s.pr_errors,
        })
    }

    async fn trigger_dispatch_for_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .trigger_dispatch_for_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }
}

// ── SlotPoolBridge → SlotPoolOps ──────────────────────────────────────────────

#[async_trait]
impl SlotPoolOps for SlotPoolBridge {
    async fn get_status(&self) -> Result<PoolStatus, String> {
        let s = self.0.get_status().await.map_err(|e| e.to_string())?;
        Ok(PoolStatus {
            active_slots: s.active_slots,
            total_slots: s.total_slots,
            per_model: s
                .per_model
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        ModelPoolStatus {
                            active: v.active,
                            free: v.free,
                            total: v.total,
                        },
                    )
                })
                .collect(),
            running_tasks: s
                .running_tasks
                .into_iter()
                .map(|t| RunningTaskInfo {
                    task_id: t.task_id,
                    model_id: t.model_id,
                    slot_id: t.slot_id,
                    duration_seconds: t.duration_seconds,
                    idle_seconds: t.idle_seconds,
                    project_id: t.project_id,
                })
                .collect(),
        })
    }

    async fn kill_session(&self, task_id: &str) -> Result<(), String> {
        self.0
            .kill_session(task_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn session_for_task(&self, task_id: &str) -> Result<Option<RunningTaskInfo>, String> {
        let result = self
            .0
            .session_for_task(task_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(result.map(|t| RunningTaskInfo {
            task_id: t.task_id,
            model_id: t.model_id,
            slot_id: t.slot_id,
            duration_seconds: t.duration_seconds,
            idle_seconds: t.idle_seconds,
            project_id: t.project_id,
        }))
    }

    async fn has_session(&self, task_id: &str) -> Result<bool, String> {
        self.0.has_session(task_id).await.map_err(|e| e.to_string())
    }
}

// ── LspBridge → LspOps ───────────────────────────────────────────────────────

#[async_trait]
impl LspOps for LspBridge {
    async fn warnings(&self) -> Vec<LspWarning> {
        self.0
            .warnings()
            .await
            .into_iter()
            .map(|w| LspWarning {
                server: w.server,
                message: w.message,
            })
            .collect()
    }
}

// ── AppState → RuntimeOps + GitOps + mcp_state() ─────────────────────────────

use crate::server::AppState;

#[async_trait]
impl RuntimeOps for AppState {
    async fn apply_settings(
        &self,
        settings: &djinn_core::models::DjinnSettings,
    ) -> Result<(), String> {
        AppState::apply_settings(self, settings).await
    }

    async fn embed_memory_query(
        &self,
        query: &str,
    ) -> Result<Option<SemanticQueryEmbedding>, String> {
        match self.embedding_service().embed_query(query).await {
            djinn_provider::embeddings::EmbeddingOutcome::Ready(vector) => {
                Ok(Some(SemanticQueryEmbedding {
                    values: vector.values,
                }))
            }
            djinn_provider::embeddings::EmbeddingOutcome::Degraded(_) => Ok(None),
        }
    }

    async fn reset_runtime_settings(&self) {
        AppState::reset_runtime_settings(self).await;
    }

    async fn persist_model_health_state(&self) {
        AppState::persist_model_health_state(self).await;
    }

    async fn apply_environment_config(
        &self,
        project_id: &str,
        config: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String> {
        // Route through the image-controller in prod so the runtime
        // ConfigMap gets upserted alongside the DB write. In dev mode
        // without a kube client there's no CM to reconcile; just write
        // the DB.
        if let Some(controller) = self.image_controller().await {
            controller
                .apply_environment_config(project_id, config)
                .await
                .map_err(|e| e.to_string())
        } else {
            let repo = djinn_db::ProjectRepository::new(
                self.db().clone(),
                djinn_core::events::EventBus::noop(),
            );
            let json = serde_json::to_string(config)
                .map_err(|e| format!("serialize environment_config: {e}"))?;
            repo.set_environment_config(project_id, &json)
                .await
                .map_err(|e| format!("db write: {e}"))
        }
    }
}

#[async_trait]
impl GitOps for AppState {
    async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        AppState::git_actor(self, path).await
    }
}

// ── RepoGraphBridge → RepoGraphOps ──────────────────────────────────────────

/// `RepoGraphOps` adapter wrapping the per-server `AppState`.  Holding the
/// state lets graph queries route through `ensure_canonical_graph`, which
/// owns the ADR-050 `_index/` worktree, single-flight `IndexerLock`, and
/// per-commit `repo_graph_cache`.
pub(crate) struct RepoGraphBridge {
    state: AppState,
}

impl RepoGraphBridge {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl RepoGraphOps for RepoGraphBridge {
    async fn neighbors(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        direction: Option<&str>,
        group_by: Option<&str>,
        kind_filter: Option<&str>,
    ) -> Result<NeighborsResult, String> {
        use djinn_graph::repo_graph::RepoGraphEdgeKind;
        use petgraph::Direction;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        // v8: apply project graph_excluded_paths to the neighbor set so
        // SCIP module-tree synthetic nodes (`crate/`, `…/MODULE.`) and
        // user-configured globs don't leak into dependents-discovery
        // queries — same as ranked / search / cycles / impact / dead.
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        let node_index = resolve_node_or_err(&graph, key)?;
        // v8: pre-compute the queried node's identity so we can filter
        // out self-referential neighbors. User feedback: querying a
        // file's outgoing neighbors returns the file itself (because
        // the file's own symbols reach back via DeclaredInFile/
        // FileReference) — same file path as the source, no useful
        // signal. Same for a symbol whose declaring file shows up.
        let self_node = graph.node(node_index);
        let self_key = format_node_key(&self_node.id);
        let self_file = self_node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string());
        let directions: Vec<Direction> = match direction {
            Some("incoming") => vec![Direction::Incoming],
            Some("outgoing") => vec![Direction::Outgoing],
            _ => vec![Direction::Incoming, Direction::Outgoing],
        };

        // PR A3: when the caller asks for `kind_filter=reads|writes`,
        // restrict the BFS frontier to that edge kind. Validation happens
        // upstream (`validate_edge_kind_filter`); anything else here is a
        // bug and we treat it as "no filter" rather than panic.
        let edge_kind_filter: Option<RepoGraphEdgeKind> = match kind_filter {
            Some("reads") => Some(RepoGraphEdgeKind::Reads),
            Some("writes") => Some(RepoGraphEdgeKind::Writes),
            _ => None,
        };

        let mut neighbors = Vec::new();
        for dir in directions {
            let dir_label = match dir {
                Direction::Incoming => "incoming",
                Direction::Outgoing => "outgoing",
            };
            for edge in graph.graph().edges_directed(node_index, dir) {
                if let Some(filter) = edge_kind_filter
                    && edge.weight().kind != filter
                {
                    continue;
                }
                let other_index = match dir {
                    Direction::Outgoing => edge.target(),
                    Direction::Incoming => edge.source(),
                };
                let other_node = graph.node(other_index);
                // v8: skip external (vendored / third-party / cross-crate)
                // neighbors. `neighbors` is "what's connected to this in
                // MY codebase"; an imported `tokio::spawn` showing up
                // among callers is noise.
                if other_node.is_external {
                    continue;
                }
                let other_key = format_node_key(&other_node.id);
                let other_file = other_node
                    .file_path
                    .as_ref()
                    .map(|p| p.display().to_string());
                if exclusions.excludes(&other_key, other_file.as_deref(), &other_node.display_name)
                {
                    continue;
                }
                // v8: drop self-references. Two flavours:
                //   1. Other node IS the queried node (rare — would
                //      require a self-loop edge).
                //   2. Other node lives in the SAME file as the queried
                //      node (very common: querying file:foo.rs returns
                //      its own symbols via FileReference; querying a
                //      symbol returns its declaring file via
                //      DeclaredInFile, which is the same file the
                //      symbol lives in).
                if other_index == node_index || other_key == self_key {
                    continue;
                }
                if let (Some(sf), Some(of)) = (self_file.as_deref(), other_file.as_deref())
                    && sf == of
                {
                    continue;
                }
                neighbors.push((
                    other_node,
                    GraphNeighbor {
                        key: other_key,
                        kind: format!("{:?}", other_node.kind).to_lowercase(),
                        display_name: other_node.display_name.clone(),
                        edge_kind: format!("{:?}", edge.weight().kind),
                        edge_weight: edge.weight().weight,
                        direction: dir_label.to_string(),
                    },
                ));
            }
        }

        match group_by {
            None => Ok(NeighborsResult::Detailed(
                neighbors.into_iter().map(|(_, n)| n).collect(),
            )),
            Some("file") => {
                let groups = group_neighbors_by_file(&neighbors);
                Ok(NeighborsResult::Grouped(groups))
            }
            Some(other) => Err(format!(
                "invalid group_by '{other}': only 'file' is supported"
            )),
        }
    }

    async fn ranked(
        &self,
        ctx: &ProjectCtx,
        kind_filter: Option<&str>,
        sort_by: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RankedNode>, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        // Read the cached PageRank populated by `ensure_canonical_graph`
        // during warm.  Without this cache, every `ranked` call re-ran a full
        // PageRank pass and hung for 30+ s on real-world graphs even when
        // `code_graph status` reported `warmed: true`.
        let (graph, ranking, _sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        let filter = match kind_filter {
            Some("file") => Some(RepoGraphNodeKind::File),
            Some("symbol") => Some(RepoGraphNodeKind::Symbol),
            _ => None,
        };
        let mut nodes: Vec<RankedNode> = ranking
            .nodes
            .iter()
            .filter(|node| filter.is_none() || Some(node.kind) == filter)
            .filter_map(|node| {
                let graph_node = graph.node(node.node_index);
                // v8: skip external (vendored / third-party / cross-crate)
                // symbols. `ranked` is "what's central in MY codebase"; an
                // imported `tokio::spawn` getting top-3 is noise. Mirrors
                // the long-standing filter in `orphans` and `dead`.
                if graph_node.is_external {
                    return None;
                }
                let key = format_node_key(&node.key);
                let file_hint = graph_node.file_path.as_ref().map(|p| p.display().to_string());
                // PR F4: apply graph exclusions BEFORE the limit truncate
                // so the user gets `limit` non-excluded results, not
                // `limit` raw results minus exclusions.
                if exclusions.excludes(&key, file_hint.as_deref(), &graph_node.display_name) {
                    return None;
                }
                // v8: drop test files from `ranked` centrality output.
                // User feedback: tests with high out-degree (test
                // files reference many production symbols) dominated
                // out_degree-sorted rankings without being
                // "architecturally meaningful". Conservative: only
                // skips file paths that match the per-language test
                // convention (`is_test_path`); test SYMBOLS in a
                // production file pass through. Tests that ARE in
                // a `tests/` directory or `*_test.go`-named file
                // also drop their symbol nodes (the file path on the
                // symbol matches).
                if let Some(path) = file_hint.as_deref()
                    && djinn_control_plane::tools::graph_exclusions::is_test_path(path)
                {
                    return None;
                }
                // PR F4: pick the lowest-ordinal step's process when the
                // node belongs to multiple — that's the "most upstream"
                // membership, which makes the bucket label the entry
                // point closest to this node.
                let process_id = pick_lowest_ordinal_process_id(&graph, node.node_index);
                let community_id = graph
                    .community_id(node.node_index)
                    .map(|s| s.to_string());
                Some(RankedNode {
                    key,
                    kind: format!("{:?}", node.kind).to_lowercase(),
                    display_name: graph_node.display_name.clone(),
                    score: node.score,
                    page_rank: node.page_rank,
                    structural_weight: node.structural_weight,
                    inbound_edge_weight: node.inbound_edge_weight,
                    outbound_edge_weight: node.outbound_edge_weight,
                    process_id,
                    community_id,
                    is_entry_point: node.is_entry_point,
                    entry_point_distance: node.entry_point_distance,
                })
            })
            .collect();

        match sort_by {
            None | Some("fused") => {
                // PR F4: already in fused (RRF) order — the canonical
                // ranking sorts by `fused_rank` desc.
            }
            Some("pagerank") => {
                nodes.sort_by(|a, b| b.page_rank.total_cmp(&a.page_rank));
            }
            Some("in_degree") => {
                nodes.sort_by(|a, b| b.inbound_edge_weight.total_cmp(&a.inbound_edge_weight));
            }
            Some("out_degree") => {
                nodes.sort_by(|a, b| b.outbound_edge_weight.total_cmp(&a.outbound_edge_weight));
            }
            Some("total_degree") => {
                nodes.sort_by(|a, b| {
                    let total_b = b.inbound_edge_weight + b.outbound_edge_weight;
                    let total_a = a.inbound_edge_weight + a.outbound_edge_weight;
                    total_b.total_cmp(&total_a)
                });
            }
            Some(other) => {
                return Err(format!(
                    "invalid sort_by '{other}': expected 'fused', 'pagerank', 'in_degree', \
                     'out_degree', or 'total_degree'"
                ));
            }
        }

        nodes.truncate(limit);
        Ok(nodes)
    }

    async fn implementations(
        &self,
        ctx: &ProjectCtx,
        symbol: &str,
    ) -> Result<Vec<String>, String> {
        use djinn_graph::repo_graph::RepoGraphEdgeKind;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        // v8: filter implementor symbols through the project's
        // graph_excluded_paths so vendored impl files (e.g. a `vendor/`
        // copy of an interface implementation) don't show up alongside
        // the in-repo implementors.
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        let node_index = graph
            .symbol_node(symbol)
            .ok_or_else(|| format!("symbol '{symbol}' not found in graph"))?;
        let mut impls = Vec::new();
        for edge in graph
            .graph()
            .edges_directed(node_index, petgraph::Direction::Incoming)
        {
            if edge.weight().kind == RepoGraphEdgeKind::Implements {
                let source_node = graph.node(edge.source());
                // v8: skip external (vendored / third-party) implementors.
                // "Who implements this trait" should be in-repo by default.
                if source_node.is_external {
                    continue;
                }
                if let Some(sym) = &source_node.symbol {
                    let src_key = format_node_key(&source_node.id);
                    let src_file = source_node
                        .file_path
                        .as_ref()
                        .map(|p| p.display().to_string());
                    if exclusions.excludes(&src_key, src_file.as_deref(), &source_node.display_name)
                    {
                        continue;
                    }
                    impls.push(sym.clone());
                }
            }
        }
        Ok(impls)
    }

    async fn impact(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        max_depth: usize,
        group_by: Option<&str>,
        min_confidence: Option<f64>,
    ) -> Result<ImpactResult, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        // v8: thread project graph_excluded_paths through impact too —
        // even with the behavioral-edge whitelist, the BFS frontier
        // can land on nodes the user has explicitly excluded
        // (vendored mirrors, generated dirs).
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        let start = resolve_node_or_err(&graph, key)?;
        let raw = impact_bfs(&graph, start, max_depth, min_confidence);
        let result: Vec<_> = raw
            .into_iter()
            .filter(|(idx, _)| {
                let node = graph.node(*idx);
                // v8: skip external (vendored / third-party / cross-crate)
                // dependents — "what breaks if I change this" should be
                // about MY code, not someone else's.
                if node.is_external {
                    return false;
                }
                let key = format_node_key(&node.id);
                let file_hint = node.file_path.as_ref().map(|p| p.display().to_string());
                !exclusions.excludes(&key, file_hint.as_deref(), &node.display_name)
            })
            .collect();

        match group_by {
            None => Ok(ImpactResult::Detailed(
                result.into_iter().map(|(_, e)| e).collect(),
            )),
            Some("file") => {
                let groups = group_impact_by_file(&graph, &result);
                Ok(ImpactResult::Grouped(groups))
            }
            Some(other) => Err(format!(
                "invalid group_by '{other}': only 'file' is supported"
            )),
        }
    }

    async fn search(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        let filter = match kind_filter {
            Some("file") => Some(RepoGraphNodeKind::File),
            Some("symbol") => Some(RepoGraphNodeKind::Symbol),
            _ => None,
        };
        // PR F4: ask `search_by_name` for an unbounded result set so the
        // exclusions filter runs BEFORE we cap to `limit`. Otherwise a
        // user with a noisy `tests/**` prefix would see ≤limit matches
        // even when there were plenty of legitimate hits past the
        // truncation point.
        let hits = graph.search_by_name(query, filter, usize::MAX);
        let mut out: Vec<SearchHit> = Vec::new();
        for hit in hits {
            let node = graph.node(hit.node_index);
            let key = format_node_key(&node.id);
            let file = node.file_path.as_ref().map(|p| p.display().to_string());
            if exclusions.excludes(&key, file.as_deref(), &node.display_name) {
                continue;
            }
            // v8: drop test-file results from name search by default.
            // User feedback (cross-repo eval): searching `Strategy`
            // returned mock-dominated results because mocks/test
            // fixtures share the same display name as core types.
            // Mocks are caught by Tier 1.5; tests need this extra
            // file-path check (a real `Strategy` symbol in
            // `internal/strategies/strategy_test.go` would otherwise
            // outrank the prod `Strategy` type at limit boundaries).
            // Externals already excluded by the search ranker upstream
            // for symbol-kind queries; this catches the residual file
            // case.
            if let Some(path) = file.as_deref()
                && djinn_control_plane::tools::graph_exclusions::is_test_path(path)
            {
                continue;
            }
            out.push(SearchHit {
                key,
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
                score: hit.score,
                file,
                match_kind: None,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn hybrid_search(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        // PR B4: cache-first orchestrator entrypoint. The actual three-
        // signal RRF fusion lives in `hybrid_search::run` so the
        // RepoGraphBridge stays thin.
        self::hybrid_search::run(&self.state, ctx, query, kind_filter, limit).await
    }

    async fn cycles(
        &self,
        ctx: &ProjectCtx,
        kind_filter: Option<&str>,
        min_size: usize,
    ) -> Result<Vec<CycleGroup>, String> {
        // Read the cached per-kind SCC sets populated by
        // `ensure_canonical_graph` during warm.  Without this cache, every
        // `cycles` call re-ran `tarjan_scc` over the full graph (or a
        // node-filtered subgraph) and hung for tens of seconds on real-world
        // graphs.  The cache holds three precomputed sets — full / file /
        // symbol — because `kind_filter` filters the graph *before* the SCC
        // search, so a single unfiltered representation cannot reproduce the
        // kind-specific results.  `min_size` is applied at read time against
        // the cached set (which is materialised at `min_size = 2`).
        let (graph, _ranking, sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let cached: &Vec<Vec<petgraph::graph::NodeIndex>> = match kind_filter {
            Some("file") => &sccs.file,
            Some("symbol") => &sccs.symbol,
            _ => &sccs.full,
        };
        let min = min_size.max(2);
        Ok(cached
            .iter()
            .filter(|component| component.len() >= min)
            .map(|component| {
                let members = component
                    .iter()
                    .map(|idx| {
                        let node = graph.node(*idx);
                        CycleMember {
                            key: format_node_key(&node.id),
                            display_name: node.display_name.clone(),
                            kind: format!("{:?}", node.kind).to_lowercase(),
                        }
                    })
                    .collect::<Vec<_>>();
                CycleGroup {
                    size: component.len(),
                    members,
                }
            })
            .collect())
    }

    async fn orphans(
        &self,
        ctx: &ProjectCtx,
        kind_filter: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OrphanEntry>, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        use djinn_graph::scip_parser::ScipVisibility;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let filter = match kind_filter {
            Some("file") => Some(RepoGraphNodeKind::File),
            Some("symbol") => Some(RepoGraphNodeKind::Symbol),
            _ => None,
        };
        let vis = match visibility {
            Some("public") => Some(ScipVisibility::Public),
            Some("private") => Some(ScipVisibility::Private),
            None | Some("any") => None,
            Some(other) => {
                return Err(format!(
                    "invalid visibility '{other}': expected 'public', 'private', or 'any'"
                ));
            }
        };
        // v8: over-fetch from the graph layer so we can post-filter
        // entry-points / tests / framework hooks without under-filling
        // `limit`. Cheap — graph.orphans is O(V) anyway.
        let raw_nodes = graph.orphans(filter, vis, limit.saturating_mul(4).clamp(limit, 1000));
        // Pre-collect EntryPointOf incoming-edge targets so we can
        // skip framework-invoked entry points without re-walking the
        // graph for each candidate.
        use djinn_graph::repo_graph::RepoGraphEdgeKind;
        let mut entry_set: std::collections::HashSet<petgraph::graph::NodeIndex> =
            std::collections::HashSet::new();
        for idx in graph.graph().node_indices() {
            if graph
                .graph()
                .edges_directed(idx, petgraph::Direction::Incoming)
                .any(|e| e.weight().kind == RepoGraphEdgeKind::EntryPointOf)
            {
                entry_set.insert(idx);
            }
        }
        let mut out: Vec<OrphanEntry> = Vec::new();
        for idx in raw_nodes {
            let node = graph.node(idx);
            // v8: framework-invoked entry points are not dead code.
            // The detector covers `fn main`, route handlers, tests,
            // python `__main__`, etc. via EntryPointOf edges; SCIP-
            // marked tests via is_test. Defensive name check for
            // Go's `init()` (often missed by detectors because
            // every Go file may have one).
            if entry_set.contains(&idx) || node.is_test {
                continue;
            }
            if matches!(node.display_name.as_str(), "main" | "init" | "_start" | "TestMain") {
                continue;
            }
            // v8: also skip test files (file-path heuristic) — they
            // legitimately have no incoming production references but
            // aren't "dead". Symbols inside a test file flagged
            // is_test handle the symbol case; this catches FILE nodes.
            if let Some(path) = node.file_path.as_ref()
                && djinn_control_plane::tools::graph_exclusions::is_test_path(
                    &path.display().to_string(),
                )
            {
                continue;
            }
            out.push(OrphanEntry {
                key: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
                file: node.file_path.as_ref().map(|p| p.display().to_string()),
                visibility: node
                    .visibility
                    .map(|v| v.as_str().to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn path(
        &self,
        ctx: &ProjectCtx,
        from: &str,
        to: &str,
        max_depth: Option<usize>,
    ) -> Result<Option<PathResult>, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let from_idx = resolve_node_or_err(&graph, from)?;
        let to_idx = resolve_node_or_err(&graph, to)?;
        let path = match graph.shortest_path(from_idx, to_idx, max_depth) {
            Some(p) => p,
            None => return Ok(None),
        };
        let mut hops = Vec::with_capacity(path.len());
        for window in path.windows(2) {
            let (src, dst) = (window[0], window[1]);
            let edge_kind = graph
                .graph()
                .edges_directed(src, petgraph::Direction::Outgoing)
                .find(|edge| edge.target() == dst)
                .map(|edge| format!("{:?}", edge.weight().kind))
                .unwrap_or_else(|| "unknown".to_string());
            let dst_node = graph.node(dst);
            hops.push(PathHop {
                key: format_node_key(&dst_node.id),
                edge_kind,
            });
        }
        Ok(Some(PathResult {
            from: format_node_key(&graph.node(from_idx).id),
            to: format_node_key(&graph.node(to_idx).id),
            length: hops.len(),
            hops,
        }))
    }

    async fn edges(
        &self,
        ctx: &ProjectCtx,
        from_glob: &str,
        to_glob: &str,
        edge_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EdgeEntry>, String> {
        use globset::Glob;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let from_matcher = Glob::new(from_glob)
            .map_err(|e| format!("invalid from_glob '{from_glob}': {e}"))?
            .compile_matcher();
        let to_matcher = Glob::new(to_glob)
            .map_err(|e| format!("invalid to_glob '{to_glob}': {e}"))?
            .compile_matcher();
        let mut out = Vec::new();
        for edge_ref in graph.graph().edge_references() {
            let src_node = graph.node(edge_ref.source());
            let dst_node = graph.node(edge_ref.target());
            let src_key = format_node_key(&src_node.id);
            let dst_key = format_node_key(&dst_node.id);
            let src_match_target = src_node
                .file_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| src_node.display_name.clone());
            let dst_match_target = dst_node
                .file_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| dst_node.display_name.clone());
            if !from_matcher.is_match(&src_match_target) {
                continue;
            }
            if !to_matcher.is_match(&dst_match_target) {
                continue;
            }
            let kind_label = format!("{:?}", edge_ref.weight().kind);
            if let Some(filter) = edge_kind
                && !kind_label.eq_ignore_ascii_case(filter)
            {
                continue;
            }
            out.push(EdgeEntry {
                from: src_key,
                to: dst_key,
                edge_kind: kind_label,
                edge_weight: edge_ref.weight().weight,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn describe(
        &self,
        ctx: &ProjectCtx,
        key: &str,
    ) -> Result<Option<SymbolDescription>, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let node_index = match resolve_node_or_err(&graph, key) {
            Ok(idx) => idx,
            Err(_) => return Ok(None),
        };
        let node = graph.node(node_index);
        let documentation = if node.documentation.is_empty() {
            None
        } else {
            Some(node.documentation.join("\n"))
        };
        Ok(Some(SymbolDescription {
            key: format_node_key(&node.id),
            kind: format!("{:?}", node.kind).to_lowercase(),
            display_name: node.display_name.clone(),
            signature: node.signature.clone(),
            documentation,
            file: node.file_path.as_ref().map(|p| p.display().to_string()),
        }))
    }

    /// PR C1: 360° symbol context. Resolve `key` to a single graph node,
    /// gather every incident edge, bucket by [`EdgeCategory`], and hard-cap
    /// each list at 30. When `include_content` is true, attempt to read
    /// the symbol body from disk (best-effort: failures degrade silently
    /// to `content: None`).
    async fn context(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        include_content: bool,
    ) -> Result<Option<SymbolContext>, String> {
        use petgraph::Direction;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let node_index = match resolve_node_or_err(&graph, key) {
            Ok(idx) => idx,
            Err(_) => return Ok(None),
        };
        let node = graph.node(node_index);

        // v8: filter related symbols through graph_excluded_paths so the
        // 360° view doesn't pull in synthetic SCIP module-tree nodes
        // (`crate/`, `…/MODULE.`) or vendored copies for the queried
        // symbol's neighborhood.
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        // Build incoming/outgoing buckets. We over-collect into per-category
        // Vecs and truncate at 30 once everything is in — sorting by
        // confidence (desc) so the highest-trust edges win the cap.
        let mut incoming: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
            std::collections::BTreeMap::new();
        let mut outgoing: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
            std::collections::BTreeMap::new();

        for dir in [Direction::Incoming, Direction::Outgoing] {
            for edge in graph.graph().edges_directed(node_index, dir) {
                let other_index = match dir {
                    Direction::Incoming => edge.source(),
                    Direction::Outgoing => edge.target(),
                };
                let other = graph.node(other_index);
                // v8: skip external (vendored / third-party / cross-crate)
                // related symbols. The 360° view is "what surrounds THIS
                // codebase symbol"; an imported `tokio::Future` showing
                // up alongside in-repo callers is noise.
                if other.is_external {
                    continue;
                }
                let other_key = format_node_key(&other.id);
                let other_file = other.file_path.as_ref().map(|p| p.display().to_string());
                if exclusions.excludes(&other_key, other_file.as_deref(), &other.display_name) {
                    continue;
                }
                let category = classify_edge_category(Some(edge.weight()), other);
                let related = build_related_symbol(other, edge.weight().confidence);
                let bucket = match dir {
                    Direction::Incoming => incoming.entry(category).or_default(),
                    Direction::Outgoing => outgoing.entry(category).or_default(),
                };
                bucket.push(related);
            }
        }

        // Plan-mandated hard limit: 30 per category. Sort desc by
        // confidence first so the bucket-truncation drops the
        // lowest-confidence entries.
        for buckets in [&mut incoming, &mut outgoing] {
            for entries in buckets.values_mut() {
                entries.sort_by(|a, b| {
                    b.confidence
                        .partial_cmp(&a.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.uid.cmp(&b.uid))
                });
                entries.truncate(30);
            }
        }

        // Pin the symbol's range and (optionally) body content.
        let (start_line, end_line) = node
            .file_path
            .as_ref()
            .and_then(|p| graph.range_for_node(node_index, p))
            .unwrap_or((0, 0));
        let file_path = node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        let content = if include_content && start_line > 0 && !file_path.is_empty() {
            read_symbol_content(&ctx.clone_path, &file_path, start_line, end_line)
        } else {
            None
        };

        let method_metadata = build_method_metadata(node);

        let symbol = SymbolNode {
            uid: format_node_key(&node.id),
            name: node.display_name.clone(),
            kind: kind_label_for_node(node),
            file_path,
            start_line,
            end_line,
            content,
            method_metadata,
        };

        // PR F2: populate process memberships from the per-graph
        // sidecar. Empty when process detection is disabled
        // (`DJINN_PROCESS_DETECTION=false`), when the cached artifact
        // pre-dates v4, or when the queried node doesn't appear in
        // any traced flow.
        let processes: Vec<ProcessRef> = graph
            .processes_for_node(node_index)
            .into_iter()
            .map(|p| ProcessRef {
                id: p.id.clone(),
                label: p.label.clone(),
                role: "step".to_string(),
            })
            .collect();

        Ok(Some(SymbolContext {
            symbol,
            incoming,
            outgoing,
            processes,
        }))
    }

    async fn status(&self, ctx: &ProjectCtx) -> Result<GraphStatus, String> {
        use djinn_db::RepoGraphCacheRepository;

        let (project_root, _index_tree_path) =
            djinn_graph::canonical_graph::normalize_graph_query_paths(&ctx.clone_path);

        // Source of truth: the `repo_graph_cache` row written by the K8s
        // graph warmer Job. The server process itself never rebuilds —
        // status reports whatever the warmer has persisted.
        let cache_repo = RepoGraphCacheRepository::new(self.state.db().clone());
        let row = cache_repo
            .latest_for_project(&ctx.id)
            .await
            .map_err(|e| format!("read repo_graph_cache: {e}"))?;

        let Some(row) = row else {
            return Ok(GraphStatus {
                project_id: ctx.id.clone(),
                warmed: false,
                last_warm_at: None,
                pinned_commit: None,
                commits_since_pin: None,
            });
        };

        let commits_since_pin = djinn_graph::canonical_graph::canonical_graph_count_commits_since(
            &project_root,
            &row.commit_sha,
        )
        .await;

        Ok(GraphStatus {
            project_id: ctx.id.clone(),
            warmed: true,
            last_warm_at: Some(row.built_at),
            pinned_commit: Some(row.commit_sha),
            commits_since_pin,
        })
    }

    async fn snapshot(
        &self,
        ctx: &ProjectCtx,
        node_cap: usize,
        exclusions: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    ) -> Result<SnapshotPayload, String> {
        use djinn_db::RepoGraphCacheRepository;

        // Pull the warmed graph + cached PageRank ranking. We deliberately
        // use `load_canonical_graph` (not `_only`) so the same warm step
        // that backs `ranked` / `cycles` is reused — recomputing PageRank
        // on every snapshot call would dominate the latency budget on
        // large repos.
        let (graph, ranking, _sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        // Look up the pinned commit SHA the warmer recorded. We treat
        // `git_head` as authoritative — there's no point asking the
        // worktree because the warmed graph is built from the pinned
        // commit, not HEAD.
        let cache_repo = RepoGraphCacheRepository::new(self.state.db().clone());
        let cache_row = cache_repo
            .latest_for_project(&ctx.id)
            .await
            .map_err(|e| format!("read repo_graph_cache: {e}"))?;
        let git_head = cache_row
            .as_ref()
            .map(|r| r.commit_sha.clone())
            .unwrap_or_default();

        // ISO8601 UTC. Reuse the same `OffsetDateTime` path the rest of
        // the bridge takes (e.g. `built_at` in `repo_graph_cache`).
        let generated_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| String::new());

        Ok(build_snapshot_payload(
            &graph,
            &ranking,
            ctx.id.clone(),
            git_head,
            generated_at,
            node_cap,
            exclusions,
        ))
    }

    async fn symbols_at(
        &self,
        ctx: &ProjectCtx,
        file: &str,
        start_line: u32,
        end_line: Option<u32>,
    ) -> Result<Vec<SymbolAtHit>, String> {
        use petgraph::Direction;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let end = end_line.unwrap_or(start_line);
        let (start, end) = if start_line <= end {
            (start_line, end)
        } else {
            (end, start_line)
        };

        let file_path = std::path::Path::new(file);
        let hits = graph.symbols_enclosing(file_path, start, end);

        // Also surface the file node itself when present — this gives
        // callers a stable anchor even when symbol_ranges is empty (e.g.
        // the artifact-restored cache path before the next rebuild).
        let mut out: Vec<SymbolAtHit> = Vec::new();
        if let Some(file_idx) = graph.file_node(file_path) {
            let node = graph.node(file_idx);
            out.push(SymbolAtHit {
                key: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
                file: node.file_path.as_ref().map(|p| p.display().to_string()),
                start_line: None,
                end_line: None,
                visibility: node.visibility.map(|v| v.as_str().to_string()),
                symbol_kind: None,
            });
        }

        for idx in hits {
            let node = graph.node(idx);
            // `symbols_enclosing` is populated by definitions only; we do
            // not have the exact range stored on the node itself, so the
            // per-hit start/end are omitted. Callers that need the range
            // can re-query via `symbols_at` with a tighter window.
            let _ = (
                graph.graph().edges_directed(idx, Direction::Incoming),
                graph.graph().edges_directed(idx, Direction::Outgoing),
            );
            out.push(SymbolAtHit {
                key: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
                file: node.file_path.as_ref().map(|p| p.display().to_string()),
                start_line: Some(start),
                end_line: Some(end),
                visibility: node.visibility.map(|v| v.as_str().to_string()),
                symbol_kind: node.symbol_kind.as_ref().map(|k| format!("{k:?}")),
            });
        }
        Ok(out)
    }

    async fn diff_touches(
        &self,
        ctx: &ProjectCtx,
        changed_ranges: &[ChangedRange],
    ) -> Result<DiffTouchesResult, String> {
        use petgraph::Direction;
        use std::collections::BTreeSet;

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        let mut touched_indices: BTreeSet<petgraph::graph::NodeIndex> = BTreeSet::new();
        let mut affected_files: Vec<String> = Vec::new();
        let mut unknown_files: Vec<String> = Vec::new();
        let mut seen_affected: BTreeSet<String> = BTreeSet::new();
        let mut seen_unknown: BTreeSet<String> = BTreeSet::new();

        for range in changed_ranges {
            let end = range.end_line.unwrap_or(range.start_line);
            let (start, end) = if range.start_line <= end {
                (range.start_line, end)
            } else {
                (end, range.start_line)
            };
            let start_u32 = u32::try_from(start.max(0)).unwrap_or(0);
            let end_u32 = u32::try_from(end.max(0)).unwrap_or(0);
            let file_path = std::path::Path::new(&range.file);
            let file_present = graph.file_node(file_path).is_some();
            if file_present {
                if seen_affected.insert(range.file.clone()) {
                    affected_files.push(range.file.clone());
                }
            } else if seen_unknown.insert(range.file.clone()) {
                unknown_files.push(range.file.clone());
            }
            for idx in graph.symbols_enclosing(file_path, start_u32, end_u32) {
                touched_indices.insert(idx);
            }
        }

        let mut touched_symbols: Vec<TouchedSymbol> = touched_indices
            .into_iter()
            .map(|idx| {
                let node = graph.node(idx);
                let fan_in = graph
                    .graph()
                    .edges_directed(idx, Direction::Incoming)
                    .count();
                let fan_out = graph
                    .graph()
                    .edges_directed(idx, Direction::Outgoing)
                    .count();
                TouchedSymbol {
                    key: format_node_key(&node.id),
                    display_name: node.display_name.clone(),
                    kind: format!("{:?}", node.kind).to_lowercase(),
                    symbol_kind: node.symbol_kind.as_ref().map(|k| format!("{k:?}")),
                    visibility: node.visibility.map(|v| v.as_str().to_string()),
                    file: node.file_path.as_ref().map(|p| p.display().to_string()),
                    start_line: None,
                    end_line: None,
                    fan_in,
                    fan_out,
                }
            })
            .collect();

        // Stable output: highest fan-in first so PR reviewers see the
        // most structurally central symbols at the top.
        touched_symbols.sort_by(|a, b| {
            b.fan_in
                .cmp(&a.fan_in)
                .then_with(|| b.fan_out.cmp(&a.fan_out))
                .then_with(|| a.key.cmp(&b.key))
        });

        Ok(DiffTouchesResult {
            touched_symbols,
            affected_files,
            unknown_files,
        })
    }

    async fn detect_changes(
        &self,
        ctx: &ProjectCtx,
        from_sha: Option<&str>,
        to_sha: Option<&str>,
        changed_files: &[String],
    ) -> Result<DetectedChangesResult, String> {
        use std::collections::{BTreeMap, BTreeSet};

        // Pull the warmed graph + pagerank ranking once. PageRank tiers
        // are computed against the *current* graph rather than a graph
        // rebuilt at the from/to shas — review weight should reflect
        // "what matters now" (per plan §C4).
        let (graph, ranking, _sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        // Pre-compute pagerank tiers via quartile bucketing across all
        // *symbol* nodes (file nodes' pagerank is structurally inflated
        // by the `ContainsDefinition` fan-out, so mixing them in
        // produces tier thresholds that flag every method as Low).
        let tier_thresholds = quartile_thresholds(&ranking);
        let pagerank_lookup: BTreeMap<petgraph::graph::NodeIndex, f64> = ranking
            .nodes
            .iter()
            .map(|n| (n.node_index, n.page_rank))
            .collect();

        // Resolve hunks → symbol indices.
        //
        // Line-mode wins when both inputs are present (per request-shape
        // contract). The `git diff` shell-out lives in `djinn_graph`;
        // we go through the helper so the worker binary can reuse it
        // without reaching into the bridge.
        let mut touched_indices: BTreeSet<petgraph::graph::NodeIndex> = BTreeSet::new();
        let line_mode = matches!((from_sha, to_sha), (Some(f), Some(t)) if !f.is_empty() && !t.is_empty());
        let (effective_from, effective_to) = if line_mode {
            (
                from_sha.unwrap_or("").to_string(),
                to_sha.unwrap_or("").to_string(),
            )
        } else {
            // Echo back empty strings when in changed_files mode — the
            // wire shape always includes both fields (see `DetectedChangesResult`).
            (String::new(), String::new())
        };

        if line_mode {
            let hunks = djinn_graph::git_diff::diff_changed_ranges(
                std::path::Path::new(&ctx.clone_path),
                &effective_from,
                &effective_to,
            )
            .await
            .map_err(|e| format!("git diff {}..{}: {e}", effective_from, effective_to))?;
            for hunk in &hunks {
                let start = hunk.start_line.max(0) as u32;
                let end = hunk.end_line.unwrap_or(hunk.start_line).max(0) as u32;
                let (start, end) = if start <= end { (start, end) } else { (end, start) };
                let path = std::path::Path::new(&hunk.file);
                for idx in graph.symbols_enclosing(path, start, end) {
                    touched_indices.insert(idx);
                }
            }
        } else {
            // changed_files mode: every symbol inside the listed files
            // is treated as touched. We find them by walking the
            // ContainsDefinition fan-out from the file node.
            use petgraph::Direction;
            use djinn_graph::repo_graph::RepoGraphEdgeKind;
            for file in changed_files {
                let path = std::path::Path::new(file);
                let Some(file_idx) = graph.file_node(path) else {
                    continue;
                };
                for edge in graph.graph().edges_directed(file_idx, Direction::Outgoing) {
                    if matches!(edge.weight().kind, RepoGraphEdgeKind::ContainsDefinition) {
                        touched_indices.insert(edge.target());
                    }
                }
            }
        }

        // Project to the wire type. Skip nodes that are file rather
        // than symbol — `detect_changes` is symbol-centric. (File
        // nodes still surface as symbols' `file_path` field.)
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        let mut touched_symbols: Vec<DetectedTouchedSymbol> = Vec::new();
        for idx in &touched_indices {
            let node = graph.node(*idx);
            if !matches!(node.kind, RepoGraphNodeKind::Symbol) {
                continue;
            }
            // Prefer the node's own `file_path`; fall back to the
            // SCIP file association is already stored there at build.
            let Some(file_pb) = node.file_path.as_ref() else {
                continue;
            };
            let file_path = file_pb.display().to_string();
            // Pull the symbol's enclosing range — we have to re-read it
            // out of the per-file `symbol_ranges` sidecar via the
            // public `symbols_enclosing` API (no direct getter), so
            // probe a max-window to collect the entry.
            let (start_line, end_line) = symbol_range_for_node(&graph, *idx, file_pb);

            let pagerank = pagerank_lookup.get(idx).copied().unwrap_or(0.0);
            let pagerank_tier = bucket_pagerank(&tier_thresholds, pagerank);

            touched_symbols.push(DetectedTouchedSymbol {
                uid: format_node_key(&node.id),
                name: node.display_name.clone(),
                kind: node
                    .symbol_kind
                    .as_ref()
                    .map(|k| format!("{k:?}").to_lowercase())
                    .unwrap_or_else(|| format!("{:?}", node.kind).to_lowercase()),
                file_path,
                start_line,
                end_line,
                pagerank_tier,
                // PR C4 only detects modification — full add/delete
                // classification needs a from-sha graph build (see
                // `ChangeKind` doc).
                change_kind: ChangeKind::Modified,
            });
        }

        // Stable, reviewer-friendly order: tier desc, then file path,
        // then start line.
        touched_symbols.sort_by(|a, b| {
            tier_rank(a.pagerank_tier)
                .cmp(&tier_rank(b.pagerank_tier))
                .then_with(|| a.file_path.cmp(&b.file_path))
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.uid.cmp(&b.uid))
        });

        // Per-file rollup (sorted by file path; per-file order matches
        // the global order above).
        let mut by_file: BTreeMap<String, Vec<DetectedTouchedSymbol>> = BTreeMap::new();
        for sym in &touched_symbols {
            by_file
                .entry(sym.file_path.clone())
                .or_default()
                .push(sym.clone());
        }

        Ok(DetectedChangesResult {
            from_sha: effective_from,
            to_sha: effective_to,
            touched_symbols,
            by_file,
        })
    }

    async fn api_surface(
        &self,
        ctx: &ProjectCtx,
        module_glob: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ApiSurfaceEntry>, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        use djinn_graph::scip_parser::ScipVisibility;
        use petgraph::Direction;

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        let vis_filter = match visibility {
            None | Some("public") => Some(ScipVisibility::Public),
            Some("private") => Some(ScipVisibility::Private),
            Some("any") => None,
            Some(other) => {
                return Err(format!(
                    "invalid visibility '{other}': expected 'public', 'private', or 'any'"
                ));
            }
        };
        let module_matcher = match module_glob {
            Some(pattern) => Some(
                globset::Glob::new(pattern)
                    .map_err(|e| format!("invalid module_glob '{pattern}': {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };

        let mut out: Vec<ApiSurfaceEntry> = Vec::new();
        for node_index in graph.graph().node_indices() {
            let node = graph.node(node_index);
            if node.kind != RepoGraphNodeKind::Symbol || node.is_external {
                continue;
            }
            if let Some(filter) = vis_filter
                && node.visibility != Some(filter)
            {
                continue;
            }
            let file_str = node
                .file_path
                .as_ref()
                .map(|p| p.display().to_string());
            if let Some(matcher) = &module_matcher {
                let Some(f) = &file_str else { continue };
                if !matcher.is_match(f) {
                    continue;
                }
            }
            let key = format_node_key(&node.id);
            // Self-crate = the SCIP `<tool> <scheme> <crate-name> ...` token.
            let own_crate = node
                .symbol
                .as_deref()
                .and_then(scip_crate_name);
            let mut used_outside_crate = false;
            let mut fan_in = 0usize;
            for edge in graph.graph().edges_directed(node_index, Direction::Incoming) {
                fan_in += 1;
                if !used_outside_crate && own_crate.is_some() {
                    let src = graph.node(edge.source());
                    if let Some(src_sym) = src.symbol.as_deref()
                        && let Some(src_crate) = scip_crate_name(src_sym)
                        && Some(src_crate) != own_crate
                    {
                        used_outside_crate = true;
                    }
                }
            }
            let fan_out = graph
                .graph()
                .edges_directed(node_index, Direction::Outgoing)
                .count();
            let doc_present = !node.documentation.is_empty()
                && node.documentation.iter().any(|l| !l.trim().is_empty());
            out.push(ApiSurfaceEntry {
                key,
                display_name: node.display_name.clone(),
                symbol_kind: node.symbol_kind.as_ref().map(|k| format!("{k:?}")),
                file: file_str,
                visibility: node.visibility.map(|v| v.as_str().to_string()),
                doc_present,
                fan_in,
                fan_out,
                used_outside_crate,
            });
        }
        // Stable order: highest fan-in first, then alpha by key.
        out.sort_by(|a, b| b.fan_in.cmp(&a.fan_in).then_with(|| a.key.cmp(&b.key)));
        out.truncate(limit);
        Ok(out)
    }

    async fn boundary_check(
        &self,
        ctx: &ProjectCtx,
        rules: &[BoundaryRule],
    ) -> Result<Vec<BoundaryViolation>, String> {
        use globset::Glob;

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        // Every submitted rule is treated as a forbidden edge.
        let compiled: Vec<(usize, globset::GlobMatcher, globset::GlobMatcher)> = rules
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let from = Glob::new(&r.from_glob)
                    .map_err(|e| format!("rule[{i}].from_glob '{}': {e}", r.from_glob))?
                    .compile_matcher();
                let to = Glob::new(&r.to_glob)
                    .map_err(|e| format!("rule[{i}].to_glob '{}': {e}", r.to_glob))?
                    .compile_matcher();
                Ok::<_, String>((i, from, to))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if compiled.is_empty() {
            return Ok(Vec::new());
        }

        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        let mut violations: Vec<BoundaryViolation> = Vec::new();
        for edge_ref in graph.graph().edge_references() {
            let src_node = graph.node(edge_ref.source());
            let dst_node = graph.node(edge_ref.target());
            let src_key = format_node_key(&src_node.id);
            let dst_key = format_node_key(&dst_node.id);
            // Skip the edge if either endpoint is filtered by exclusions.
            if exclusions.excludes(&src_key, src_node.file_path.as_ref().map(|p| p.display().to_string()).as_deref(), &src_node.display_name)
                || exclusions.excludes(&dst_key, dst_node.file_path.as_ref().map(|p| p.display().to_string()).as_deref(), &dst_node.display_name)
            {
                continue;
            }
            let src_match_target = src_node
                .file_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| src_node.display_name.clone());
            let dst_match_target = dst_node
                .file_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| dst_node.display_name.clone());

            for (rule_index, from_m, to_m) in &compiled {
                if from_m.is_match(&src_match_target) && to_m.is_match(&dst_match_target) {
                    violations.push(BoundaryViolation {
                        rule_index: *rule_index,
                        from_key: src_key.clone(),
                        to_key: dst_key.clone(),
                        edge_kind: format!("{:?}", edge_ref.weight().kind),
                        from_file: src_node.file_path.as_ref().map(|p| p.display().to_string()),
                        to_file: dst_node.file_path.as_ref().map(|p| p.display().to_string()),
                        witness_path: Some(vec![src_key.clone(), dst_key.clone()]),
                    });
                }
            }
        }
        Ok(violations)
    }

    async fn hotspots(
        &self,
        ctx: &ProjectCtx,
        window_days: u32,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HotspotEntry>, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        use std::collections::BTreeMap;

        let (graph, ranking, _sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        // Churn via git log, single invocation. Use git's relative-date
        // syntax ("N days ago") — that side-steps dragging in chrono just
        // for a date subtraction while still giving git a stable bound.
        let days = window_days.clamp(1, 365);
        let (project_root, _idx) =
            djinn_graph::canonical_graph::normalize_graph_query_paths(&ctx.clone_path);
        let mut churn: BTreeMap<String, usize> = BTreeMap::new();
        match std::process::Command::new("git")
            .current_dir(&project_root)
            .args([
                "log",
                "--name-only",
                "--pretty=format:",
                &format!("--since={days} days ago"),
            ])
            .output()
        {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                for line in stdout.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    *churn.entry(trimmed.to_string()).or_insert(0) += 1;
                }
            }
            Ok(out) => {
                tracing::warn!(
                    project_id = %ctx.id,
                    status = %out.status,
                    "hotspots: git log returned non-zero; returning empty result",
                );
                return Ok(Vec::new());
            }
            Err(e) => {
                tracing::warn!(
                    project_id = %ctx.id,
                    error = %e,
                    "hotspots: git log failed; returning empty result",
                );
                return Ok(Vec::new());
            }
        }

        // Build per-file centrality (Σ PR of owned symbols) and top-symbol list.
        // `RepoGraphRanking` is pagerank-sorted, so we can walk it directly
        // to pick up the highest-PR symbols per file.
        let mut per_file_centrality: BTreeMap<String, f64> = BTreeMap::new();
        let mut per_file_top: BTreeMap<String, Vec<(f64, String)>> = BTreeMap::new();
        for ranked in &ranking.nodes {
            if ranked.kind != RepoGraphNodeKind::Symbol {
                continue;
            }
            let node = graph.node(ranked.node_index);
            let Some(file) = node.file_path.as_ref().map(|p| p.display().to_string())
            else {
                continue;
            };
            *per_file_centrality.entry(file.clone()).or_insert(0.0) += ranked.page_rank;
            let top = per_file_top.entry(file).or_default();
            if top.len() < 3 {
                top.push((ranked.page_rank, node.display_name.clone()));
            }
        }

        let file_matcher = match file_glob {
            Some(pattern) => Some(
                globset::Glob::new(pattern)
                    .map_err(|e| format!("invalid file_glob '{pattern}': {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        let mut out: Vec<HotspotEntry> = Vec::new();
        for (file, count) in churn {
            if let Some(matcher) = &file_matcher
                && !matcher.is_match(&file)
            {
                continue;
            }
            // Apply GraphExclusions — we key on file path for discovery,
            // and use the same path as display_name for the key check.
            if exclusions.excludes(&file, Some(&file), &file) {
                continue;
            }
            let centrality = per_file_centrality.get(&file).copied().unwrap_or(0.0);
            let top_symbols = per_file_top
                .get(&file)
                .map(|v| v.iter().map(|(_, n)| n.clone()).collect())
                .unwrap_or_default();
            out.push(HotspotEntry {
                file,
                churn: count,
                centrality,
                composite_score: count as f64 * centrality,
                top_symbols,
            });
        }
        out.sort_by(|a, b| {
            b.composite_score
                .partial_cmp(&a.composite_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.file.cmp(&b.file))
        });
        out.truncate(limit);
        Ok(out)
    }

    async fn metrics_at(
        &self,
        ctx: &ProjectCtx,
    ) -> Result<MetricsAtResult, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        use djinn_graph::scip_parser::ScipVisibility;
        use petgraph::Direction;
        use std::collections::BTreeMap;

        let (graph, _ranking, sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        // Filter the node-set once; every downstream count uses it.
        let mut kept: Vec<petgraph::graph::NodeIndex> = Vec::new();
        let mut total_degree: Vec<usize> = Vec::new();
        let mut public_kept: Vec<petgraph::graph::NodeIndex> = Vec::new();
        for node_index in graph.graph().node_indices() {
            let node = graph.node(node_index);
            let file = node.file_path.as_ref().map(|p| p.display().to_string());
            let key = format_node_key(&node.id);
            if exclusions.excludes(&key, file.as_deref(), &node.display_name) {
                continue;
            }
            kept.push(node_index);
            let td = graph.graph().edges_directed(node_index, Direction::Incoming).count()
                + graph.graph().edges_directed(node_index, Direction::Outgoing).count();
            total_degree.push(td);
            if node.kind == RepoGraphNodeKind::Symbol
                && node.visibility == Some(ScipVisibility::Public)
            {
                public_kept.push(node_index);
            }
        }

        // p95 of total_degree across kept nodes → god-object floor.
        let mut td_sorted = total_degree.clone();
        td_sorted.sort_unstable();
        let p95_floor = if td_sorted.is_empty() {
            0
        } else {
            let idx = ((td_sorted.len() as f64) * 0.95).ceil() as usize;
            td_sorted[idx.saturating_sub(1).min(td_sorted.len() - 1)]
        };
        let god_object_count = if p95_floor == 0 {
            0
        } else {
            total_degree.iter().filter(|d| **d >= p95_floor).count()
        };

        // Edge count over kept nodes only — both endpoints must survive.
        let kept_set: std::collections::HashSet<petgraph::graph::NodeIndex> =
            kept.iter().copied().collect();
        let edge_count = graph
            .graph()
            .edge_references()
            .filter(|e| kept_set.contains(&e.source()) && kept_set.contains(&e.target()))
            .count();

        // Cycles — exclude SCCs whose kept members drop below size 2.
        let mut cycles_by_size_histogram: BTreeMap<usize, usize> = BTreeMap::new();
        let mut cycle_count = 0usize;
        for component in sccs.full.iter() {
            let surviving = component
                .iter()
                .filter(|idx| kept_set.contains(idx))
                .count();
            if surviving >= 2 {
                cycle_count += 1;
                *cycles_by_size_histogram.entry(surviving).or_insert(0) += 1;
            }
        }

        // Orphan count — defer to graph.orphans(), then strip via exclusions.
        let orphans = graph.orphans(None, None, usize::MAX);
        let orphan_count = orphans
            .into_iter()
            .filter(|idx| kept_set.contains(idx))
            .count();

        let public_api_count = public_kept.len();
        let docs_present = public_kept
            .iter()
            .filter(|idx| {
                let n = graph.node(**idx);
                !n.documentation.is_empty() && n.documentation.iter().any(|l| !l.trim().is_empty())
            })
            .count();
        let doc_coverage_pct = if public_api_count == 0 {
            0.0
        } else {
            100.0 * (docs_present as f64) / (public_api_count as f64)
        };

        // Resolve the pinned commit via repo_graph_cache. Best-effort;
        // fall back to an empty string if the lookup fails.
        let mut pinned = String::new();
        use djinn_db::RepoGraphCacheRepository;
        let cache_repo = RepoGraphCacheRepository::new(self.state.db().clone());
        if let Ok(Some(row)) = cache_repo.latest_for_project(&ctx.id).await {
            pinned = row.commit_sha;
        }

        Ok(MetricsAtResult {
            commit: pinned,
            node_count: kept.len(),
            edge_count,
            cycle_count,
            cycles_by_size_histogram,
            god_object_count,
            orphan_count,
            public_api_count,
            doc_coverage_pct,
        })
    }

    /// Symbols with zero incoming edges from the entry-point set.
    ///
    /// PR F1 cut-over: entry-point detection now lives in
    /// [`djinn_graph::entry_points`] and stamps `EntryPointOf` edges at
    /// build time. This method just asks "does the symbol have any
    /// incoming `EntryPointOf` edge?" — the per-language test / main /
    /// HTTP-route heuristics are handled centrally by the detector.
    /// "Crate root re-export surface" is still inferred locally from
    /// the file path (`**/src/lib.rs` or `**/src/main.rs`) so a
    /// `pub fn` re-exported from the crate root isn't flagged dead just
    /// because no in-tree caller hits it.
    async fn dead_symbols(
        &self,
        ctx: &ProjectCtx,
        confidence: &str,
        limit: usize,
    ) -> Result<Vec<DeadSymbolEntry>, String> {
        use djinn_graph::repo_graph::{RepoGraphEdgeKind, RepoGraphNodeKind};
        use djinn_graph::scip_parser::ScipVisibility;
        use petgraph::Direction;
        use std::collections::HashSet;

        if !matches!(confidence, "high" | "med" | "low") {
            return Err(format!(
                "invalid confidence '{confidence}': expected 'high', 'med', or 'low'"
            ));
        }

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        // Crate-root public-surface heuristic still runs locally — the
        // detector doesn't tag every public symbol re-exported from
        // `src/lib.rs` because that would over-fire for non-library
        // crates. We layer it in here so a `pub fn` at the crate root
        // is still considered an entry point.
        let crate_root_lib = globset::Glob::new("**/src/lib.rs")
            .map_err(|e| e.to_string())?
            .compile_matcher();
        let crate_root_main = globset::Glob::new("**/src/main.rs")
            .map_err(|e| e.to_string())?
            .compile_matcher();

        let mut entry_set: HashSet<petgraph::graph::NodeIndex> = HashSet::new();
        for idx in graph.graph().node_indices() {
            let node = graph.node(idx);
            if node.kind != RepoGraphNodeKind::Symbol || node.is_external {
                continue;
            }
            // PR F1: any node with an incoming `EntryPointOf` edge is
            // an entry point.
            let has_entry_point_edge = graph
                .graph()
                .edges_directed(idx, Direction::Incoming)
                .any(|e| e.weight().kind == RepoGraphEdgeKind::EntryPointOf);
            if has_entry_point_edge {
                entry_set.insert(idx);
                continue;
            }
            // Crate-root public-surface fallback (file-path heuristic
            // not covered by the detector).
            let file_str = node.file_path.as_ref().map(|p| p.display().to_string());
            let crate_root_public = node.visibility == Some(ScipVisibility::Public)
                && file_str
                    .as_deref()
                    .map(|f| crate_root_lib.is_match(f) || crate_root_main.is_match(f))
                    .unwrap_or(false);
            if crate_root_public {
                entry_set.insert(idx);
            }
        }

        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        let mut out: Vec<DeadSymbolEntry> = Vec::new();
        for idx in graph.graph().node_indices() {
            let node = graph.node(idx);
            if node.kind != RepoGraphNodeKind::Symbol || node.is_external {
                continue;
            }
            if entry_set.contains(&idx) {
                continue;
            }

            let mut has_any_incoming = false;
            let mut has_relationship_ref_or_impl = false;
            let mut has_relationship_impl = false;
            for edge in graph.graph().edges_directed(idx, Direction::Incoming) {
                match edge.weight().kind {
                    RepoGraphEdgeKind::ContainsDefinition
                    | RepoGraphEdgeKind::DeclaredInFile => {}
                    // PR F1: `EntryPointOf` is metadata, not a caller
                    // signal. Symbols with this edge already short-
                    // circuit above via `entry_set`; non-entry symbols
                    // shouldn't carry one, but skip defensively.
                    RepoGraphEdgeKind::EntryPointOf => {}
                    RepoGraphEdgeKind::Implements => {
                        has_any_incoming = true;
                        has_relationship_ref_or_impl = true;
                        has_relationship_impl = true;
                    }
                    RepoGraphEdgeKind::Extends => {
                        has_any_incoming = true;
                        has_relationship_ref_or_impl = true;
                    }
                    _ => {
                        has_any_incoming = true;
                    }
                }
            }
            // Tiers (strictest → loosest):
            // * `high` — exclude anything with an incoming impl *or*
            //   relationship-ref edge (they're likely dyn-dispatch callers).
            // * `med`  — exclude anything with an incoming impl edge.
            // * `low`  — keep any symbol with zero incoming "real" edges,
            //   regardless of relationship hints.
            let keep = match confidence {
                "low" => !has_any_incoming,
                "med" => !has_any_incoming && !has_relationship_impl,
                "high" => !has_any_incoming && !has_relationship_ref_or_impl,
                _ => unreachable!(),
            };
            if !keep {
                continue;
            }

            let key = format_node_key(&node.id);
            let file = node.file_path.as_ref().map(|p| p.display().to_string());
            if exclusions.excludes(&key, file.as_deref(), &node.display_name) {
                continue;
            }
            out.push(DeadSymbolEntry {
                key,
                display_name: node.display_name.clone(),
                symbol_kind: node.symbol_kind.as_ref().map(|k| format!("{k:?}")),
                file,
                visibility: node.visibility.map(|v| v.as_str().to_string()),
                confidence: confidence.to_string(),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn deprecated_callers(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
    ) -> Result<Vec<DeprecatedHit>, String> {
        use djinn_graph::repo_graph::{RepoGraphEdgeKind, RepoGraphNodeKind};
        use petgraph::Direction;

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        let mut out: Vec<DeprecatedHit> = Vec::new();
        for idx in graph.graph().node_indices() {
            let node = graph.node(idx);
            if node.kind != RepoGraphNodeKind::Symbol || node.is_external {
                continue;
            }
            // v1: text-scan signature + documentation for deprecation markers.
            // The SCIP parser does not yet set an explicit `deprecated` flag —
            // extending `ScipSymbol` to carry one is left for a later pass.
            if !is_deprecated_text(node.signature.as_deref(), &node.documentation) {
                continue;
            }
            let dep_key = format_node_key(&node.id);
            let dep_file = node.file_path.as_ref().map(|p| p.display().to_string());
            if exclusions.excludes(&dep_key, dep_file.as_deref(), &node.display_name) {
                continue;
            }
            let mut callers: Vec<CallerRef> = Vec::new();
            for edge in graph.graph().edges_directed(idx, Direction::Incoming) {
                match edge.weight().kind {
                    // PR A3: `Reads` / `Writes` are split-out variants of the
                    // legacy `SymbolReference` edge; they still count as
                    // "this caller touches the deprecated symbol".
                    RepoGraphEdgeKind::SymbolReference
                    | RepoGraphEdgeKind::Reads
                    | RepoGraphEdgeKind::Writes
                    | RepoGraphEdgeKind::Extends
                    | RepoGraphEdgeKind::FileReference => {
                        let src = graph.node(edge.source());
                        let src_key = format_node_key(&src.id);
                        let src_file = src.file_path.as_ref().map(|p| p.display().to_string());
                        if exclusions.excludes(&src_key, src_file.as_deref(), &src.display_name) {
                            continue;
                        }
                        callers.push(CallerRef {
                            key: src_key,
                            display_name: src.display_name.clone(),
                            file: src_file,
                        });
                    }
                    _ => {}
                }
            }
            out.push(DeprecatedHit {
                deprecated_symbol: dep_key,
                deprecated_display_name: node.display_name.clone(),
                deprecated_file: dep_file,
                callers,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn touches_hot_path(
        &self,
        ctx: &ProjectCtx,
        seed_entries: &[String],
        seed_sinks: &[String],
        symbols: &[String],
    ) -> Result<Vec<HotPathHit>, String> {
        use std::collections::{HashMap, HashSet};

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        if seed_entries.is_empty() || seed_sinks.is_empty() || symbols.is_empty() {
            return Ok(Vec::new());
        }

        // Resolve all keys once.
        let resolve = |key: &str| -> Option<petgraph::graph::NodeIndex> {
            resolve_node_or_err(&graph, key).ok()
        };
        let entry_ix: Vec<petgraph::graph::NodeIndex> =
            seed_entries.iter().filter_map(|k| resolve(k)).collect();
        let sink_ix: Vec<petgraph::graph::NodeIndex> =
            seed_sinks.iter().filter_map(|k| resolve(k)).collect();

        let pair_cap = 400usize;
        let total_pairs = entry_ix.len() * sink_ix.len();
        let truncated = total_pairs > pair_cap;
        if truncated {
            tracing::warn!(
                project_id = %ctx.id,
                total_pairs,
                cap = pair_cap,
                "touches_hot_path: pair count exceeds cap; truncating",
            );
        }

        // Precompute shortest paths, capping at pair_cap. Paths collected
        // as Vec<NodeIndex> for membership tests, and cached as formatted
        // keys for the first `example_path` hit per symbol.
        let mut paths: Vec<Vec<petgraph::graph::NodeIndex>> = Vec::new();
        let mut count = 0usize;
        'outer: for &e in &entry_ix {
            for &s in &sink_ix {
                if count >= pair_cap {
                    break 'outer;
                }
                count += 1;
                if let Some(p) = graph.shortest_path(e, s, None) {
                    paths.push(p);
                }
            }
        }

        // Build a lookup symbol-key → NodeIndex, then walk the path
        // list once per queried symbol (O(Q × P × |path|), P ≤ 400).
        let mut queried: HashMap<String, petgraph::graph::NodeIndex> = HashMap::new();
        for k in symbols {
            if let Some(idx) = resolve(k) {
                queried.insert(k.clone(), idx);
            }
        }

        let mut out: Vec<HotPathHit> = Vec::new();
        for k in symbols {
            let Some(idx) = queried.get(k).copied() else {
                out.push(HotPathHit {
                    symbol: k.clone(),
                    on_path_count: 0,
                    example_path: None,
                });
                continue;
            };
            let mut hits = 0usize;
            let mut example: Option<Vec<String>> = None;
            for path in &paths {
                let set: HashSet<petgraph::graph::NodeIndex> = path.iter().copied().collect();
                if set.contains(&idx) {
                    hits += 1;
                    if example.is_none() {
                        example = Some(
                            path.iter()
                                .map(|i| format_node_key(&graph.node(*i).id))
                                .collect(),
                        );
                    }
                }
            }
            out.push(HotPathHit {
                symbol: k.clone(),
                on_path_count: hits,
                example_path: example,
            });
        }
        Ok(out)
    }

    async fn coupling(
        &self,
        ctx: &ProjectCtx,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingEntry>, String> {
        use djinn_control_plane::bridge::CouplingEntry;
        use djinn_db::CommitFileChangeRepository;

        let repo = CommitFileChangeRepository::new(self.state.db().clone());
        let rows = repo
            .top_coupled(&ctx.id, file_path, limit.max(1))
            .await
            .map_err(|e| format!("coupling lookup: {e}"))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let samples: Vec<String> = row
                .supporting_commit_samples()
                .into_iter()
                .take(3)
                .collect();
            out.push(CouplingEntry {
                file_path: row.file_path,
                co_edit_count: row.co_edit_count.max(0) as usize,
                last_co_edit: row.last_co_edit,
                supporting_commit_samples: samples,
            });
        }
        Ok(out)
    }

    async fn churn(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
    ) -> Result<Vec<djinn_control_plane::bridge::ChurnEntry>, String> {
        use djinn_control_plane::bridge::ChurnEntry;
        use djinn_db::CommitFileChangeRepository;

        let since = since_days_to_cutoff(since_days);
        let repo = CommitFileChangeRepository::new(self.state.db().clone());
        let rows = repo
            .churn(&ctx.id, limit.max(1), since.as_deref())
            .await
            .map_err(|e| format!("churn lookup: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|row| ChurnEntry {
                file_path: row.file_path,
                commit_count: row.commit_count.max(0) as usize,
                insertions: row.insertions.max(0) as usize,
                deletions: row.deletions.max(0) as usize,
                last_commit_at: row.last_commit_at,
            })
            .collect())
    }

    async fn coupling_hotspots(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
        max_files_per_commit: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CoupledPairEntry>, String> {
        use djinn_control_plane::bridge::CoupledPairEntry;
        use djinn_db::CommitFileChangeRepository;

        let since = since_days_to_cutoff(since_days);
        let repo = CommitFileChangeRepository::new(self.state.db().clone());
        let rows = repo
            .top_coupled_pairs(&ctx.id, limit.max(1), since.as_deref(), max_files_per_commit)
            .await
            .map_err(|e| format!("coupling_hotspots lookup: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|row| CoupledPairEntry {
                file_a: row.file_a,
                file_b: row.file_b,
                co_edits: row.co_edits.max(0) as usize,
                last_co_edit: row.last_co_edit,
            })
            .collect())
    }

    async fn coupling_hubs(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
        max_files_per_commit: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingHubEntry>, String> {
        use djinn_control_plane::bridge::CouplingHubEntry;
        use djinn_db::CommitFileChangeRepository;

        let since = since_days_to_cutoff(since_days);
        let repo = CommitFileChangeRepository::new(self.state.db().clone());
        // Over-fetch 2000 pairs for stable hub aggregation — the SQL
        // sort is the work here, the limit is cheap.
        let rows = repo
            .coupling_hubs(
                &ctx.id,
                limit.max(1),
                since.as_deref(),
                max_files_per_commit,
                2000,
            )
            .await
            .map_err(|e| format!("coupling_hubs lookup: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|row| CouplingHubEntry {
                file_path: row.file_path,
                total_coupling: row.total_coupling.max(0) as usize,
                partner_count: row.partner_count.max(0) as usize,
            })
            .collect())
    }

    async fn resolve(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        kind_hint: Option<&str>,
    ) -> Result<ResolveOutcome, String> {
        // Pre-resolve the caller's key against the live graph. We honour
        // `DJINN_CODE_GRAPH_AMBIGUITY` inside `resolve_node_with_hint` so
        // the bridge layer doesn't need to gate the variant separately.
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let outcome = match resolve_node_with_hint(&graph, key, kind_hint) {
            self::graph_neighbors::ResolveOutcome::Found(idx) => {
                let node = graph.node(idx);
                ResolveOutcome::Found(format_node_key(&node.id))
            }
            self::graph_neighbors::ResolveOutcome::Ambiguous(candidates) => {
                ResolveOutcome::Ambiguous(candidates)
            }
            self::graph_neighbors::ResolveOutcome::NotFound => ResolveOutcome::NotFound,
        };
        Ok(outcome)
    }
}

/// Render a `since_days` window as an ISO-8601 UTC lower bound
/// (`YYYY-MM-DDTHH:MM:SSZ`). Stored `committed_at` timestamps use the
/// same fixed-width format, so a lexicographic string comparison on
/// the SQL side resolves the window correctly — no chrono dependency.
fn since_days_to_cutoff(since_days: Option<u32>) -> Option<String> {
    since_days.map(|d| {
        let clamped = d.clamp(1, 3650) as u64;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff = now.saturating_sub(clamped * 86_400);
        format_utc_iso8601(cutoff)
    })
}

/// Format a Unix timestamp (seconds since epoch) as ISO-8601 UTC with
/// second resolution (`YYYY-MM-DDTHH:MM:SSZ`). Used to render a
/// `since_days` cutoff for the `churn` op into the same lexical shape
/// our stored `committed_at` uses, so a string comparison on the SQL
/// side resolves the window correctly.
fn format_utc_iso8601(secs: u64) -> String {
    // Civil-from-Unix conversion via Howard Hinnant's algorithm
    // (public domain). Avoids a chrono dependency for the single
    // timestamp format we need.
    let days = (secs / 86_400) as i64;
    let rem_seconds = secs % 86_400;
    let hour = (rem_seconds / 3600) as u32;
    let minute = ((rem_seconds % 3600) / 60) as u32;
    let second = (rem_seconds % 60) as u32;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Extract the SCIP crate-name token from a symbol identifier.
///
/// SCIP symbols have the shape:
/// `<scheme> <manager> <package-name> <version> <descriptors>`
///
/// Example: `scip-rust cargo my-crate 0.1.0 foo/Bar#`
///
/// This helper returns the `<package-name>` slot (`my-crate`). Locals
/// (symbols of shape `local <id>`) and any symbol with fewer than four
/// leading tokens return `None`, signaling "no crate identity" to the
/// caller (who then conservatively skips the cross-crate check).
fn scip_crate_name(symbol: &str) -> Option<&str> {
    if symbol.starts_with("local ") || symbol.is_empty() {
        return None;
    }
    let mut parts = symbol.split_whitespace();
    let _scheme = parts.next()?;
    let _manager = parts.next()?;
    let package = parts.next()?;
    // Ensure there's at least one more token — the version — so we're
    // not mis-reading a malformed short header as a package name.
    let _version = parts.next()?;
    if package.is_empty() || package == "." {
        return None;
    }
    Some(package)
}

/// Scan a symbol's signature + documentation text for a `#[deprecated]`
/// or `@deprecated` marker.
///
/// `@deprecated` matching is case-insensitive so the common JSDoc and
/// Python-docstring conventions both engage. `#[deprecated` does not
/// require a closing bracket — Rust allows both the bare
/// `#[deprecated]` and `#[deprecated(...)]` forms.
fn is_deprecated_text(signature: Option<&str>, documentation: &[String]) -> bool {
    if let Some(sig) = signature
        && (sig.contains("#[deprecated") || sig.to_lowercase().contains("@deprecated"))
    {
        return true;
    }
    for line in documentation {
        if line.contains("#[deprecated") || line.to_lowercase().contains("@deprecated") {
            return true;
        }
    }
    false
}

impl AppState {
    /// Helper for graph handlers in this module: compiles a
    /// [`GraphExclusions`] predicate for the given project id,
    /// falling back to the empty (Tier 1 only) filter on any DB /
    /// lookup failure.
    async fn mcp_state_graph_exclusions(
        &self,
        project_id: &str,
    ) -> djinn_control_plane::tools::graph_exclusions::GraphExclusions {
        use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
        let repo =
            djinn_db::ProjectRepository::new(self.db().clone(), self.event_bus());
        match repo.get_config(project_id).await {
            Ok(Some(c)) => GraphExclusions::from_config(&c),
            _ => GraphExclusions::empty(),
        }
    }
}

/// PR F4: pick the [`djinn_graph::processes::Process`] id whose member
/// list places `node` at the lowest step ordinal (most upstream). When
/// the node sits in two flows — say it's `step=0` in process A and
/// `step=5` in process B — process A wins because it identifies this
/// v8: BFS used by `impact` and its tests. Walks Incoming edges from
/// `start` up to `max_depth`, returning each visited node with the
/// depth at which it was first reached.
///
/// Two filters cut the BFS frontier so transitive impact reflects
/// load-bearing propagation, not "every node anchored to the queried
/// file":
///
/// * **Behavioral-edge whitelist.** Only edges that actually carry
///   "if this changes, that breaks" semantics propagate the BFS:
///   `Reads`, `Writes`, `SymbolReference`, `FileReference` (the
///   file→file dependency edge that drives file-level impact),
///   `Implements`, `Extends`, `TypeDefines`, `Defines`. Pure
///   structural anchors (`ContainsDefinition` = "file contains this
///   symbol", `DeclaredInFile` = "this symbol lives in this file")
///   and synthetic side-channel edges (`MemberOf`, `StepInProcess`,
///   `EntryPointOf`) are skipped — they connect everything that
///   contains everything, not "this changes when that changes".
/// * **Confidence floor.** Defaults to 0.85 when the caller passes
///   `None`; pass `Some(0.0)` to opt back into the full set.
fn impact_bfs(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    start: petgraph::graph::NodeIndex,
    max_depth: usize,
    min_confidence: Option<f64>,
) -> Vec<(petgraph::graph::NodeIndex, ImpactEntry)> {
    use djinn_graph::repo_graph::RepoGraphEdgeKind;
    let propagates = |kind: RepoGraphEdgeKind| match kind {
        RepoGraphEdgeKind::Reads
        | RepoGraphEdgeKind::Writes
        | RepoGraphEdgeKind::SymbolReference
        | RepoGraphEdgeKind::FileReference
        | RepoGraphEdgeKind::Implements
        | RepoGraphEdgeKind::Extends
        | RepoGraphEdgeKind::TypeDefines
        | RepoGraphEdgeKind::Defines => true,
        RepoGraphEdgeKind::ContainsDefinition
        | RepoGraphEdgeKind::DeclaredInFile
        | RepoGraphEdgeKind::MemberOf
        | RepoGraphEdgeKind::StepInProcess
        | RepoGraphEdgeKind::EntryPointOf => false,
    };
    let confidence_threshold = min_confidence.unwrap_or(0.85);

    let mut visited = std::collections::HashSet::new();
    visited.insert(start);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((start, 0usize));
    let mut result: Vec<(petgraph::graph::NodeIndex, ImpactEntry)> = Vec::new();

    while let Some((current, depth)) = queue.pop_front() {
        if depth > 0 {
            let node = graph.node(current);
            result.push((
                current,
                ImpactEntry {
                    key: format_node_key(&node.id),
                    depth,
                    file_path: node.file_path.as_ref().map(|p| p.display().to_string()),
                },
            ));
        }
        if depth < max_depth {
            for edge in graph
                .graph()
                .edges_directed(current, petgraph::Direction::Incoming)
            {
                if !propagates(edge.weight().kind) {
                    continue;
                }
                if edge.weight().confidence < confidence_threshold {
                    continue;
                }
                let source = edge.source();
                if visited.insert(source) {
                    queue.push_back((source, depth + 1));
                }
            }
        }
    }
    result
}

/// node as an entry point (or near-entry), which is the more
/// actionable bucket label for the UI.
///
/// Returns `None` when the node is not a step in any process. Ties on
/// step ordinal are broken by `Process::id` (lex asc) so the result
/// is deterministic across rebuilds.
fn pick_lowest_ordinal_process_id(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    node: petgraph::graph::NodeIndex,
) -> Option<String> {
    let processes = graph.processes_for_node(node);
    if processes.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &str)> = None;
    for proc in processes {
        let step_ord = proc
            .steps
            .iter()
            .position(|step| *step == node)
            .unwrap_or(usize::MAX);
        match best {
            None => best = Some((step_ord, proc.id.as_str())),
            Some((cur_ord, cur_id)) => {
                if step_ord < cur_ord || (step_ord == cur_ord && proc.id.as_str() < cur_id) {
                    best = Some((step_ord, proc.id.as_str()));
                }
            }
        }
    }
    best.map(|(_, id)| id.to_string())
}

impl AppState {
    /// Build a `djinn_control_plane::McpState` from this AppState, wiring all bridge impls.
    ///
    /// Snapshots the current coordinator and pool handles via `try_lock()`.
    /// In production this is called after `initialize_agents()`, so both are
    /// populated. In tests neither is initialised; tools return graceful errors.
    pub fn mcp_state(&self) -> djinn_control_plane::McpState {
        let coordinator = self
            .coordinator_sync()
            .map(|c| Arc::new(CoordinatorBridge(c)) as Arc<dyn CoordinatorOps>);
        let pool = self
            .pool_sync()
            .map(|p| Arc::new(SlotPoolBridge(p)) as Arc<dyn SlotPoolOps>);

        djinn_control_plane::McpState::new(
            self.db().clone(),
            self.event_bus(),
            self.catalog().clone(),
            self.health_tracker().clone(),
            coordinator,
            pool,
            Some(Arc::new(self.embedding_service().clone())),
            Some(self.note_vector_store()),
            Arc::new(LspBridge(self.lsp().clone())),
            Arc::new(self.clone()),
            Arc::new(self.clone()),
            Arc::new(RepoGraphBridge::new(self.clone())),
        )
    }
}

/// PR D2: pure helper that builds a `SnapshotPayload` from an already-
/// loaded canonical graph + ranking, applying the project's
/// `graph_excluded_paths` filter and capping the surviving population
/// at `node_cap` (top-PageRank tier wins).
///
/// Extracted from `RepoGraphBridge::snapshot` so unit tests can exercise
/// the truncation / exclusion / wire-shape logic without spinning up
/// the full bridge (which needs `AppState`, a Dolt connection, and a
/// warmed K8s job).
fn build_snapshot_payload(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    ranking: &djinn_graph::repo_graph::RepoGraphRanking,
    project_id: String,
    git_head: String,
    generated_at: String,
    node_cap: usize,
    exclusions: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
) -> SnapshotPayload {
    use std::collections::{HashMap, HashSet};

    // Tally totals against the post-exclusion graph so the
    // truncation decision lines up with what the UI actually sees.
    let mut total_nodes_post_excl: usize = 0;
    let mut surviving: HashSet<petgraph::graph::NodeIndex> = HashSet::new();
    let mut pagerank_lookup: HashMap<petgraph::graph::NodeIndex, f64> = HashMap::new();
    for ranked_node in &ranking.nodes {
        let node = graph.node(ranked_node.node_index);
        let key = format_node_key(&node.id);
        let display_name = node.display_name.as_str();
        let file_hint = node.file_path.as_ref().map(|p| p.display().to_string());
        if exclusions.excludes(&key, file_hint.as_deref(), display_name) {
            continue;
        }
        // v8: skip external nodes from the snapshot too — they're
        // imported library symbols, not part of the codebase the UI
        // is rendering.
        if node.is_external {
            continue;
        }
        total_nodes_post_excl += 1;
        pagerank_lookup.insert(ranked_node.node_index, ranked_node.page_rank);
        // `ranking.nodes` is fused-rank-sorted (PR F4 RRF); collecting
        // the first `node_cap` survivors promotes entry points and the
        // shortest-distance neighborhood, not just raw PageRank leaves.
        if surviving.len() < node_cap {
            surviving.insert(ranked_node.node_index);
        }
    }

    // The ranking is built only from indexed nodes; if the graph
    // contains nodes that didn't make it into `ranking.nodes` (rare —
    // typically file nodes without symbols), fall back to a direct
    // walk. PageRank for those nodes is 0.0.
    for idx in graph.graph().node_indices() {
        if pagerank_lookup.contains_key(&idx) {
            continue;
        }
        let node = graph.node(idx);
        let key = format_node_key(&node.id);
        let file_hint = node.file_path.as_ref().map(|p| p.display().to_string());
        if exclusions.excludes(&key, file_hint.as_deref(), &node.display_name) {
            continue;
        }
        total_nodes_post_excl += 1;
        pagerank_lookup.insert(idx, 0.0);
        if surviving.len() < node_cap {
            surviving.insert(idx);
        }
    }

    let truncated = total_nodes_post_excl > surviving.len();

    // Materialize snapshot nodes in pagerank-sorted order so the wire
    // payload is deterministic and the UI can render
    // highest-importance nodes first if it streams.
    let mut snapshot_nodes: Vec<SnapshotNode> = surviving
        .iter()
        .map(|&idx| {
            let node = graph.node(idx);
            let pagerank = pagerank_lookup.get(&idx).copied().unwrap_or(0.0);
            // 2026-04-28: prettify SCIP descriptors at the wire boundary so
            // external/cross-package symbols (`scip-go gomod ...`) reach the
            // UI as the trailing identifier (`Context`, `Errorf()`, …)
            // instead of the raw 100-char descriptor. Pure display names
            // (already-resolved symbols, file paths) pass through unchanged.
            // The UI keeps a defensive `prettifyLabel` mirror in case a
            // future snapshot path forgets to call this — see
            // `djinn_graph::scip_parser::prettify_scip_descriptor`.
            let label =
                djinn_graph::scip_parser::prettify_scip_descriptor(&node.display_name);
            SnapshotNode {
                id: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                label,
                symbol_kind: node
                    .symbol_kind
                    .as_ref()
                    .map(|k| format!("{k:?}").to_lowercase()),
                file_path: node.file_path.as_ref().map(|p| p.display().to_string()),
                pagerank,
                // PR F3: populate from the canonical graph's community
                // sidecar; `None` when the node is a singleton (not in
                // any non-trivial community) or when detection was
                // skipped (`DJINN_COMMUNITY_DETECTION=0`).
                community_id: graph.community_id(idx).map(str::to_string),
            }
        })
        .collect();
    snapshot_nodes.sort_by(|a, b| {
        b.pagerank
            .partial_cmp(&a.pagerank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });

    // Walk every edge in the underlying petgraph; keep only those
    // whose source AND target survived the cap. `total_edges` is
    // the post-exclusion count (we drop edges that touch excluded
    // nodes so the totals match the visible graph).
    let mut total_edges_post_excl: usize = 0;
    let mut snapshot_edges: Vec<SnapshotEdge> = Vec::new();
    for edge_ref in graph.graph().edge_references() {
        let src_in = pagerank_lookup.contains_key(&edge_ref.source());
        let dst_in = pagerank_lookup.contains_key(&edge_ref.target());
        if !src_in || !dst_in {
            continue;
        }
        total_edges_post_excl += 1;
        if !surviving.contains(&edge_ref.source())
            || !surviving.contains(&edge_ref.target())
        {
            continue;
        }
        let from_node = graph.node(edge_ref.source());
        let to_node = graph.node(edge_ref.target());
        let weight = edge_ref.weight();
        snapshot_edges.push(SnapshotEdge {
            from: format_node_key(&from_node.id),
            to: format_node_key(&to_node.id),
            kind: format!("{:?}", weight.kind),
            confidence: weight.confidence,
            reason: weight.reason.clone(),
        });
    }

    // Sort edges deterministically (kind > from > to) so test snapshots
    // stay stable across runs.
    snapshot_edges.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.from.cmp(&b.from))
            .then_with(|| a.to.cmp(&b.to))
    });

    SnapshotPayload {
        project_id,
        git_head,
        generated_at,
        truncated,
        total_nodes: total_nodes_post_excl,
        total_edges: total_edges_post_excl,
        node_cap,
        nodes: snapshot_nodes,
        edges: snapshot_edges,
    }
}

/// Quartile thresholds for PageRank tiering, computed once per
/// `detect_changes` call.
///
/// Returns `(q33, q67)` from the symbol-only PageRank distribution:
/// scores ≥ q67 → High, q33..q67 → Medium, < q33 → Low.
///
/// Symbol nodes only because file nodes' PageRank is structurally
/// inflated by the `ContainsDefinition` fan-out (every symbol
/// declares-in its file), so mixing them in produces thresholds
/// that flag every method as Low and every file as High.
fn quartile_thresholds(ranking: &djinn_graph::repo_graph::RepoGraphRanking) -> (f64, f64) {
    use djinn_graph::repo_graph::RepoGraphNodeKind;
    let mut scores: Vec<f64> = ranking
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, RepoGraphNodeKind::Symbol))
        .map(|n| n.page_rank)
        .collect();
    scores.sort_by(|a, b| a.total_cmp(b));
    if scores.is_empty() {
        return (0.0, 0.0);
    }
    // Use 1/3 and 2/3 quantiles — three roughly equal-sized buckets.
    // True quartiles would split four ways; we want three (High /
    // Medium / Low) so 33rd and 67th percentiles are the right cuts.
    let q33_idx = (scores.len() as f64 * 0.34).floor() as usize;
    let q67_idx = (scores.len() as f64 * 0.67).floor() as usize;
    let q33 = scores[q33_idx.min(scores.len() - 1)];
    let q67 = scores[q67_idx.min(scores.len() - 1)];
    (q33, q67)
}

fn bucket_pagerank(thresholds: &(f64, f64), score: f64) -> PagerankTier {
    let (q33, q67) = *thresholds;
    if score >= q67 {
        PagerankTier::High
    } else if score >= q33 {
        PagerankTier::Medium
    } else {
        PagerankTier::Low
    }
}

fn tier_rank(t: PagerankTier) -> u8 {
    match t {
        PagerankTier::High => 0,
        PagerankTier::Medium => 1,
        PagerankTier::Low => 2,
    }
}

/// Resolve the (start_line, end_line) enclosing range for a touched
/// symbol. Falls back to (0, 0) when the per-file `symbol_ranges`
/// sidecar is empty (cache-restored graph) — see
/// `RepoDependencyGraph::range_for_node` for the limitation.
fn symbol_range_for_node(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    idx: petgraph::graph::NodeIndex,
    file: &std::path::Path,
) -> (u32, u32) {
    graph.range_for_node(idx, file).unwrap_or((0, 0))
}

#[cfg(test)]
mod detect_changes_helper_tests {
    use super::{bucket_pagerank, quartile_thresholds, tier_rank};
    use djinn_control_plane::bridge::PagerankTier;

    #[test]
    fn bucket_pagerank_uses_q33_q67() {
        let thresholds = (0.10, 0.20);
        assert_eq!(bucket_pagerank(&thresholds, 0.05), PagerankTier::Low);
        assert_eq!(bucket_pagerank(&thresholds, 0.10), PagerankTier::Medium);
        assert_eq!(bucket_pagerank(&thresholds, 0.15), PagerankTier::Medium);
        assert_eq!(bucket_pagerank(&thresholds, 0.20), PagerankTier::High);
        assert_eq!(bucket_pagerank(&thresholds, 0.99), PagerankTier::High);
    }

    #[test]
    fn tier_rank_orders_high_first() {
        assert!(tier_rank(PagerankTier::High) < tier_rank(PagerankTier::Medium));
        assert!(tier_rank(PagerankTier::Medium) < tier_rank(PagerankTier::Low));
    }

    #[test]
    fn quartile_thresholds_handles_empty_ranking() {
        let ranking = djinn_graph::repo_graph::RepoGraphRanking { nodes: vec![] };
        assert_eq!(quartile_thresholds(&ranking), (0.0, 0.0));
    }
}

#[cfg(test)]
mod helper_tests {
    use super::{is_deprecated_text, scip_crate_name};

    #[test]
    fn scip_crate_name_extracts_cargo_package() {
        let sym = "scip-rust cargo my-crate 0.1.0 foo/Bar#";
        assert_eq!(scip_crate_name(sym), Some("my-crate"));
    }

    #[test]
    fn scip_crate_name_extracts_go_module() {
        let sym = "scip-go gomod github.com/acme/foo v1 pkg/Thing#";
        assert_eq!(scip_crate_name(sym), Some("github.com/acme/foo"));
    }

    #[test]
    fn scip_crate_name_returns_none_for_short_input() {
        assert_eq!(scip_crate_name(""), None);
        assert_eq!(scip_crate_name("scip-rust"), None);
        assert_eq!(scip_crate_name("scip-rust cargo"), None);
        assert_eq!(scip_crate_name("scip-rust cargo pkg"), None);
    }

    #[test]
    fn scip_crate_name_skips_locals_and_dot_placeholder() {
        // Local symbols have no crate identity.
        assert_eq!(scip_crate_name("local 42"), None);
        // Some SCIP scheme/manager slots use "." when missing — and
        // the package slot does the same. In that case we have no
        // identity to compare against.
        let sym = "scip-rust cargo . 0.1.0 foo/Bar#";
        assert_eq!(scip_crate_name(sym), None);
    }

    #[test]
    fn is_deprecated_text_matches_rust_attribute() {
        assert!(is_deprecated_text(Some("#[deprecated] fn foo()"), &[]));
        assert!(is_deprecated_text(
            Some(r#"#[deprecated(since = "0.1", note = "use bar")] fn foo()"#),
            &[]
        ));
    }

    #[test]
    fn is_deprecated_text_matches_jsdoc_marker_case_insensitive() {
        let doc = vec!["/**".into(), " * @Deprecated use `bar` instead".into()];
        assert!(is_deprecated_text(None, &doc));
    }

    #[test]
    fn is_deprecated_text_ignores_unrelated_text() {
        let doc = vec!["A documented symbol.".into()];
        assert!(!is_deprecated_text(Some("fn foo()"), &doc));
    }
}

#[cfg(test)]
pub(crate) mod graph_bridge_tests {
    use super::*;
    // PR C2: import the inner `ResolveOutcome` (NodeIndex) under the
    // unqualified name so the existing test patterns keep compiling.
    // The bridge crate's `ResolveOutcome` (String) is different — we
    // never use it directly in these tests.
    use crate::mcp_bridge::graph_neighbors::{resolve_node, resolve_node_or_err, ResolveOutcome};
    use djinn_graph::repo_graph::{RepoDependencyGraph, RepoNodeKey};
    use djinn_graph::scip_parser::{
        ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
        ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Serialize tests that mutate `DJINN_CODE_GRAPH_AMBIGUITY` against
    /// every other test that calls `resolve_node` — cargo runs tests in
    /// parallel, so an env var set in one test would otherwise race with
    /// peer threads reading it. The mutex is held for the duration of
    /// the env mutation; tests that don't touch the env var still
    /// acquire the lock so they can't see a transient `false`.
    static AMBIGUITY_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fixture_index() -> ParsedScipIndex {
        let helper_symbol_name = "scip-rust pkg src/helper.rs `helper`().".to_string();
        let helper_symbol = ScipSymbol {
            symbol: helper_symbol_name.clone(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("helper".to_string()),
            signature: Some("fn helper()".to_string()),
            documentation: vec![],
            relationships: vec![],
            visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let trait_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
            kind: Some(ScipSymbolKind::Type),
            display_name: Some("HelperTrait".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
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
            visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/helper.rs"),
                    definitions: vec![ScipOccurrence {
                        symbol: helper_symbol_name.clone(),
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
                    }],
                    references: vec![],
                    occurrences: vec![],
                    symbols: vec![helper_symbol],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/app.rs"),
                    definitions: vec![ScipOccurrence {
                        symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
                        range: ScipRange {
                            start_line: 0,
                            start_character: 0,
                            end_line: 0,
                            end_character: 4,
                        },
                        enclosing_range: None,
                        roles: BTreeSet::from([ScipSymbolRole::Definition]),
                        syntax_kind: None,
                        override_documentation: vec![],
                    }],
                    references: vec![ScipOccurrence {
                        symbol: helper_symbol_name,
                        range: ScipRange {
                            start_line: 1,
                            start_character: 4,
                            end_line: 1,
                            end_character: 10,
                        },
                        enclosing_range: None,
                        roles: BTreeSet::new(),
                        syntax_kind: None,
                        override_documentation: vec![],
                    }],
                    occurrences: vec![],
                    symbols: vec![main_symbol, trait_symbol],
                },
            ],
            external_symbols: vec![],
        }
    }
    pub(crate) fn build_test_graph() -> RepoDependencyGraph {
        RepoDependencyGraph::build(&[fixture_index()])
    }

    #[test]
    fn resolve_node_finds_file_by_path() {
        let graph = build_test_graph();
        assert!(matches!(
            resolve_node(&graph, "src/app.rs"),
            ResolveOutcome::Found(_)
        ));
        assert!(matches!(
            resolve_node(&graph, "file:src/app.rs"),
            ResolveOutcome::Found(_)
        ));
    }

    #[test]
    fn resolve_node_finds_symbol_by_name() {
        let graph = build_test_graph();
        assert!(matches!(
            resolve_node(&graph, "scip-rust pkg src/helper.rs `helper`()."),
            ResolveOutcome::Found(_)
        ));
        assert!(matches!(
            resolve_node(&graph, "symbol:scip-rust pkg src/helper.rs `helper`()."),
            ResolveOutcome::Found(_)
        ));
    }

    #[test]
    fn resolve_node_returns_not_found_for_unknown() {
        let graph = build_test_graph();
        // The fixture has `helper`/`main` and `HelperTrait` symbols, but
        // none with a name index entry for "totally_absent".
        assert!(matches!(
            resolve_node(&graph, "totally_absent_zzz"),
            ResolveOutcome::NotFound
        ));
    }

    /// Build a fixture with three distinct symbols all named `User`,
    /// each in a different file. Used by the ambiguity / uid follow-up
    /// / feature-flag tests below.
    fn user_ambiguity_index() -> ParsedScipIndex {
        let mk_user_file = |path: &str, sym: &str, kind: ScipSymbolKind| ScipFile {
            language: "rust".to_string(),
            relative_path: PathBuf::from(path),
            definitions: vec![ScipOccurrence {
                symbol: sym.to_string(),
                range: ScipRange {
                    start_line: 0,
                    start_character: 0,
                    end_line: 0,
                    end_character: 4,
                },
                enclosing_range: None,
                roles: BTreeSet::from([ScipSymbolRole::Definition]),
                syntax_kind: None,
                override_documentation: vec![],
            }],
            references: vec![],
            occurrences: vec![],
            symbols: vec![ScipSymbol {
                symbol: sym.to_string(),
                kind: Some(kind),
                display_name: Some("User".to_string()),
                signature: None,
                documentation: vec![],
                relationships: vec![],
                visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
                signature_parts: None,
            }],
        };
        ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![
                mk_user_file(
                    "src/auth/User.rs",
                    "scip-rust pkg src/auth/User.rs `User`#",
                    ScipSymbolKind::Type,
                ),
                mk_user_file(
                    "src/billing/Account.rs",
                    "scip-rust pkg src/billing/Account.rs `User`#",
                    ScipSymbolKind::Function,
                ),
                mk_user_file(
                    "src/admin/Roles.rs",
                    "scip-rust pkg src/admin/Roles.rs `User`#",
                    ScipSymbolKind::Method,
                ),
            ],
            external_symbols: vec![],
        }
    }

    #[test]
    fn resolve_node_returns_ambiguous_when_multi_match() {
        // Three distinct symbols share display name `User`. The
        // file-path-substring signal dominates the score formula, so
        // candidates whose path contains the lowercased query rank
        // ahead of the others. The fixture also yields a file node for
        // `src/auth/User.rs` (its relative path is its display name and
        // contains "user") — so the candidate count is 3 symbols + 1
        // file = 4. Cap at 8 per the C2 spec.
        let _guard = AMBIGUITY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let graph = RepoDependencyGraph::build(&[user_ambiguity_index()]);
        let outcome = resolve_node(&graph, "User");
        match outcome {
            ResolveOutcome::Ambiguous(candidates) => {
                assert!(
                    candidates.len() >= 3 && candidates.len() <= 8,
                    "expected 3..=8 User candidates, got {}: {:?}",
                    candidates.len(),
                    candidates
                );
                assert!(
                    candidates[0].file_path.to_lowercase().contains("user"),
                    "highest-ranked candidate should match query in file path: {:?}",
                    candidates
                );
                // Verify the three symbol-kind candidates are present.
                let symbol_count = candidates
                    .iter()
                    .filter(|c| c.uid.starts_with("symbol:"))
                    .count();
                assert_eq!(
                    symbol_count, 3,
                    "expected exactly 3 symbol candidates"
                );
            }
            ResolveOutcome::Found(_) => panic!("expected Ambiguous, got Found"),
            ResolveOutcome::NotFound => panic!("expected Ambiguous, got NotFound"),
        }
    }

    #[test]
    fn resolve_node_after_uid_lookup_returns_unique() {
        // Once we have a candidate's `uid` (`"symbol:..."`), passing it
        // back as `key` resolves uniquely via the symbol index — that's
        // the C2 disambiguation handshake.
        let _guard = AMBIGUITY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let graph = RepoDependencyGraph::build(&[user_ambiguity_index()]);
        let candidates = match resolve_node(&graph, "User") {
            ResolveOutcome::Ambiguous(c) => c,
            _ => panic!("expected Ambiguous"),
        };
        let uid = candidates[0].uid.clone();
        match resolve_node(&graph, &uid) {
            ResolveOutcome::Found(_) => {}
            _ => panic!("uid follow-up should resolve to Found"),
        }
    }

    #[test]
    fn ambiguity_disabled_returns_not_found() {
        // With the feature flag off, a multi-match must collapse to
        // NotFound — preserving pre-PR-C2 semantics for callers that
        // haven't been updated to handle Ambiguous.
        //
        // SAFETY: env mutation races with parallel tests; AMBIGUITY_ENV_LOCK
        // serializes against every other resolver test in this module.
        let _guard = AMBIGUITY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let graph = RepoDependencyGraph::build(&[user_ambiguity_index()]);
        unsafe {
            std::env::set_var("DJINN_CODE_GRAPH_AMBIGUITY", "false");
        }
        let outcome = resolve_node(&graph, "User");
        unsafe {
            std::env::remove_var("DJINN_CODE_GRAPH_AMBIGUITY");
        }
        assert!(
            matches!(outcome, ResolveOutcome::NotFound),
            "with DJINN_CODE_GRAPH_AMBIGUITY=false a multi-match must collapse to NotFound"
        );
    }

    #[test]
    fn score_formula_components() {
        // Verifies the C2 score formula:
        //   0.5 + 0.4*file_path_substring + 0.2*kind_hint + tiebreaker.
        // Spot-check a Type-kind node where both signals fire and the
        // tiebreaker contributes 0.05.
        use djinn_graph::repo_graph::*;
        use djinn_graph::scip_parser::ScipSymbolKind;
        use std::path::PathBuf;

        let node = RepoGraphNode {
            id: RepoNodeKey::Symbol("scip-rust pkg src/auth/User.rs `User`#".into()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: "User".into(),
            language: Some("rust".into()),
            file_path: Some(PathBuf::from("src/auth/User.rs")),
            symbol: Some("scip-rust pkg src/auth/User.rs `User`#".into()),
            symbol_kind: Some(ScipSymbolKind::Type),
            is_external: false,
            visibility: None,
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
        };
        // Both file-path match (User in path) and kind hint ("class")
        // fire. Tiebreaker for Type/Class is 0.05.
        let s = crate::mcp_bridge::graph_neighbors::score_candidate(
            &node,
            "User",
            Some("class"),
        );
        let expected = 0.5 + 0.4 * 1.0 + 0.2 * 1.0 + 0.05;
        assert!(
            (s - expected).abs() < 1e-9,
            "score {s} != expected {expected}"
        );

        // Same node, no kind hint: drop the 0.2 component.
        let s_no_hint = crate::mcp_bridge::graph_neighbors::score_candidate(
            &node, "User", None,
        );
        let expected_no_hint = 0.5 + 0.4 * 1.0 + 0.05;
        assert!(
            (s_no_hint - expected_no_hint).abs() < 1e-9,
            "score {s_no_hint} != expected {expected_no_hint}"
        );

        // Query that doesn't appear in path: drop the 0.4 component.
        let s_no_path = crate::mcp_bridge::graph_neighbors::score_candidate(
            &node,
            "Account",
            Some("class"),
        );
        let expected_no_path = 0.5 + 0.2 * 1.0 + 0.05;
        assert!(
            (s_no_path - expected_no_path).abs() < 1e-9,
            "score {s_no_path} != expected {expected_no_path}"
        );
    }

    #[test]
    fn format_node_key_file() {
        let key = RepoNodeKey::File(PathBuf::from("src/lib.rs"));
        assert_eq!(format_node_key(&key), "file:src/lib.rs");
    }

    #[test]
    fn format_node_key_symbol() {
        let key = RepoNodeKey::Symbol("scip-rust . . . Foo#".to_string());
        assert_eq!(format_node_key(&key), "symbol:scip-rust . . . Foo#");
    }

    #[tokio::test]
    async fn neighbors_returns_connected_nodes() {
        let graph = build_test_graph();
        let node_index = match resolve_node(&graph, "src/app.rs") {
            ResolveOutcome::Found(idx) => idx,
            _ => panic!("expected Found"),
        };
        let mut neighbors = Vec::new();
        for dir in [petgraph::Direction::Incoming, petgraph::Direction::Outgoing] {
            let dir_label = match dir {
                petgraph::Direction::Incoming => "incoming",
                petgraph::Direction::Outgoing => "outgoing",
            };
            for edge in graph.graph().edges_directed(node_index, dir) {
                let other_index = match dir {
                    petgraph::Direction::Outgoing => edge.target(),
                    petgraph::Direction::Incoming => edge.source(),
                };
                let other_node = graph.node(other_index);
                neighbors.push(GraphNeighbor {
                    key: format_node_key(&other_node.id),
                    kind: format!("{:?}", other_node.kind).to_lowercase(),
                    display_name: other_node.display_name.clone(),
                    edge_kind: format!("{:?}", edge.weight().kind),
                    edge_weight: edge.weight().weight,
                    direction: dir_label.to_string(),
                });
            }
        }
        assert!(
            !neighbors.is_empty(),
            "expected at least one neighbor for src/app.rs"
        );
        assert!(neighbors.iter().any(|n| n.display_name == "helper"));
    }

    #[tokio::test]
    async fn ranked_returns_scored_nodes() {
        let graph = build_test_graph();
        let ranking = graph.rank();
        let nodes: Vec<RankedNode> = ranking
            .nodes
            .iter()
            .take(5)
            .map(|node| {
                let graph_node = graph.node(node.node_index);
                RankedNode {
                    key: format_node_key(&node.key),
                    kind: format!("{:?}", node.kind).to_lowercase(),
                    display_name: graph_node.display_name.clone(),
                    score: node.score,
                    page_rank: node.page_rank,
                    structural_weight: node.structural_weight,
                    inbound_edge_weight: node.inbound_edge_weight,
                    outbound_edge_weight: node.outbound_edge_weight,
                    process_id: None,
                    community_id: None,
                    is_entry_point: node.is_entry_point,
                    entry_point_distance: node.entry_point_distance,
                }
            })
            .collect();
        assert!(!nodes.is_empty());
        for node in &nodes {
            assert!(node.score >= 0.0);
        }
    }

    /// PR F4: build a graph with a `tests/**`-shadowed file and assert
    /// the post-exclusion `ranked` projection (the same filter the
    /// bridge applies in [`RepoGraphBridge::ranked`]) drops it. We
    /// exercise the predicate inline rather than spinning up the full
    /// async bridge — a DB-backed AppState would dominate the test
    /// runtime without adding signal.
    #[test]
    fn ranked_respects_graph_exclusions() {
        use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
        use djinn_graph::repo_graph::RepoDependencyGraph;

        // Promote a fixture file into `tests/` so the glob matches.
        let mut idx = fixture_index();
        idx.files[0].relative_path = PathBuf::from("tests/helper.rs");
        let graph = RepoDependencyGraph::build(&[idx]);
        let ranking = graph.rank();
        let exclusions = GraphExclusions::build(&["tests/**".to_string()], &[]);

        let kept: Vec<String> = ranking
            .nodes
            .iter()
            .filter_map(|node| {
                let g = graph.node(node.node_index);
                let key = format_node_key(&node.key);
                let file = g.file_path.as_ref().map(|p| p.display().to_string());
                if exclusions.excludes(&key, file.as_deref(), &g.display_name) {
                    return None;
                }
                Some(key)
            })
            .collect();

        assert!(
            !kept.iter().any(|k| k.contains("tests/helper.rs")),
            "tests/helper.rs leaked through GraphExclusions: {kept:?}",
        );
    }

    /// PR F4: same as `ranked_respects_graph_exclusions` but for the
    /// search code path.
    #[test]
    fn search_respects_graph_exclusions() {
        use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
        use djinn_graph::repo_graph::RepoDependencyGraph;

        let mut idx = fixture_index();
        idx.files[0].relative_path = PathBuf::from("tests/helper.rs");
        let graph = RepoDependencyGraph::build(&[idx]);
        let exclusions = GraphExclusions::build(&["tests/**".to_string()], &[]);

        let hits = graph.search_by_name("helper", None, usize::MAX);
        let mut kept: Vec<String> = Vec::new();
        for hit in hits {
            let node = graph.node(hit.node_index);
            let key = format_node_key(&node.id);
            let file = node.file_path.as_ref().map(|p| p.display().to_string());
            if exclusions.excludes(&key, file.as_deref(), &node.display_name) {
                continue;
            }
            kept.push(key);
        }
        assert!(
            !kept.iter().any(|k| k.contains("tests/helper.rs")),
            "tests/helper.rs leaked through search exclusions: {kept:?}",
        );
    }

    /// PR F4: with the new fused-rank default, an entry-point function
    /// (the fixture's `fn main`, picked up by the entry-point detector)
    /// must rank above a generic helper symbol. Before the multi-signal
    /// fusion landed, `helper` outranked `main` because it had a
    /// fan-in via `FileReference` from `src/app.rs`.
    ///
    /// We do NOT assert a strict main-outranks-helper position on this
    /// fixture: with only one caller-callee pair the entry-point
    /// distance signal is too weak to break a 2-out-of-3 RRF vote in
    /// helper's favour. The peer
    /// `rrf_fused_rank_promotes_entry_points_under_pagerank_tie`
    /// test in `repo_graph::tests` exercises the lift in isolation.
    #[test]
    fn ranked_default_sort_is_fused_and_promotes_entry_points() {
        use djinn_graph::repo_graph::{RepoDependencyGraph, RepoNodeKey};
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let ranking = graph.rank();

        let main_node = ranking
            .nodes
            .iter()
            .find(|node| {
                node.key == RepoNodeKey::Symbol("scip-rust pkg src/app.rs `main`().".to_string())
            })
            .expect("main symbol should be ranked");

        // The detector tagged `main` as an entry point, so the
        // side-channel that drives UI bucketing must reflect that.
        assert!(
            main_node.is_entry_point,
            "expected `main` to be marked as an entry point",
        );
        assert_eq!(
            main_node.entry_point_distance,
            Some(0),
            "entry-point function should sit at distance 0",
        );

        // Fused rank is the active sort signal: every adjacent pair
        // in the ranking is fused-rank-monotonic.
        for window in ranking.nodes.windows(2) {
            assert!(
                window[0].fused_rank >= window[1].fused_rank,
                "ranking not fused-rank-desc: {} < {} (keys {:?} vs {:?})",
                window[0].fused_rank,
                window[1].fused_rank,
                window[0].key,
                window[1].key,
            );
        }
    }

    #[tokio::test]
    async fn implementations_finds_implementors() {
        let graph = build_test_graph();
        let trait_symbol = "scip-rust pkg src/types.rs `HelperTrait`#";
        let node_index = graph
            .symbol_node(trait_symbol)
            .expect("trait symbol should exist");
        let mut impls = Vec::new();
        for edge in graph
            .graph()
            .edges_directed(node_index, petgraph::Direction::Incoming)
        {
            if edge.weight().kind == djinn_graph::repo_graph::RepoGraphEdgeKind::Implements {
                let source_node = graph.node(edge.source());
                if let Some(sym) = &source_node.symbol {
                    impls.push(sym.clone());
                }
            }
        }
        assert_eq!(impls.len(), 1);
        assert!(impls[0].contains("main"));
    }

    #[tokio::test]
    async fn impact_returns_transitive_dependents() {
        let graph = build_test_graph();
        let start = match resolve_node(&graph, "scip-rust pkg src/helper.rs `helper`().") {
            ResolveOutcome::Found(idx) => idx,
            _ => panic!("expected Found"),
        };
        let mut visited = std::collections::HashSet::new();
        visited.insert(start);
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((start, 0usize));
        let mut result = Vec::new();
        let max_depth = 3;

        while let Some((current, depth)) = queue.pop_front() {
            if depth > 0 {
                let node = graph.node(current);
                result.push(ImpactEntry {
                    key: format_node_key(&node.id),
                    depth,
                    file_path: node
                        .file_path
                        .as_ref()
                        .map(|p| p.display().to_string()),
                });
            }
            if depth < max_depth {
                for edge in graph
                    .graph()
                    .edges_directed(current, petgraph::Direction::Incoming)
                {
                    let source = edge.source();
                    if visited.insert(source) {
                        queue.push_back((source, depth + 1));
                    }
                }
            }
        }
        assert!(
            !result.is_empty(),
            "expected at least one node in the impact set"
        );
    }

    /// v8: `impact_bfs` skips structural anchors (`ContainsDefinition`,
    /// `DeclaredInFile`) and synthetic side-channels (`MemberOf`,
    /// `StepInProcess`, `EntryPointOf`) so an impact walk doesn't
    /// pull in "every node that's anchored to this file". The
    /// behavioral set (`Reads`/`Writes`/`SymbolReference`/`FileReference`
    /// /typing relationships) IS walked.
    ///
    /// Build a tiny graph with one structural and one behavioral
    /// incoming edge to a target node, run impact_bfs, assert the
    /// behavioral source is admitted and the structural source is
    /// not.
    #[tokio::test]
    async fn impact_bfs_skips_structural_anchors_but_walks_behavioral_edges() {
        use djinn_graph::repo_graph::{
            RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphEdgeKind,
            RepoGraphNode, RepoGraphNodeKind, RepoNodeKey, REPO_GRAPH_ARTIFACT_VERSION,
        };

        let mk_node = |key: RepoNodeKey, name: &str, kind: RepoGraphNodeKind| RepoGraphNode {
            id: key.clone(),
            kind,
            display_name: name.to_string(),
            language: None,
            file_path: None,
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
        };
        // Three nodes:
        //   [0] target — receives both edges
        //   [1] behavioral_src → target via Reads (should propagate)
        //   [2] structural_src → target via ContainsDefinition (should NOT)
        let nodes = vec![
            mk_node(
                RepoNodeKey::Symbol("symbol:target".to_string()),
                "target",
                RepoGraphNodeKind::Symbol,
            ),
            mk_node(
                RepoNodeKey::Symbol("symbol:behavioral".to_string()),
                "behavioral_caller",
                RepoGraphNodeKind::Symbol,
            ),
            mk_node(
                RepoNodeKey::File(std::path::PathBuf::from("src/foo.rs")),
                "src/foo.rs",
                RepoGraphNodeKind::File,
            ),
        ];
        let mk_edge = |source: usize, target: usize, kind: RepoGraphEdgeKind| {
            RepoGraphArtifactEdge {
                source,
                target,
                kind,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.95,
                reason: None,
                step: None,
            }
        };
        let edges = vec![
            mk_edge(1, 0, RepoGraphEdgeKind::Reads),
            mk_edge(2, 0, RepoGraphEdgeKind::ContainsDefinition),
        ];
        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges: std::collections::BTreeMap::new(),
            communities: vec![],
            processes: vec![],
        };
        let graph = RepoDependencyGraph::from_artifact(&artifact);
        let target_idx = graph
            .symbol_node("symbol:target")
            .expect("target should resolve");

        let result = impact_bfs(&graph, target_idx, 3, Some(0.0));
        let keys: Vec<&str> = result.iter().map(|(_, e)| e.key.as_str()).collect();
        assert!(
            keys.iter().any(|k| k.contains("symbol:behavioral")),
            "behavioral Reads edge should propagate; got {keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k.contains("src/foo.rs")),
            "structural ContainsDefinition edge should NOT propagate; got {keys:?}"
        );
    }

    /// v8: `impact_bfs` defaults `min_confidence` to 0.85 when the
    /// caller passes `None`. Floor above the highest possible
    /// confidence (1.0+) collapses the frontier to empty regardless
    /// of edge kind.
    #[tokio::test]
    async fn impact_bfs_min_confidence_default_and_strict_threshold() {
        let graph = build_test_graph();
        let helper_idx =
            resolve_node_or_err(&graph, "scip-rust pkg src/helper.rs `helper`().").unwrap();

        // A floor above the highest possible confidence drops
        // everything — proves the threshold is honored.
        let strict = impact_bfs(&graph, helper_idx, 3, Some(1.5));
        assert!(
            strict.is_empty(),
            "min_confidence above 1.0 must collapse the frontier to empty"
        );

        // Default (None → 0.85) admits high-confidence FileReference
        // edges (floor 0.85) and Reads/Writes/SymbolReference (0.85+)
        // — fixture's app.rs ↔ helper.rs FileReference at 0.85
        // qualifies, so default-walked result contains the helper
        // file's caller file.
        let with_default = impact_bfs(&graph, helper_idx, 3, None);
        let keys: Vec<&str> = with_default.iter().map(|(_, e)| e.key.as_str()).collect();
        assert!(
            keys.iter().any(|k| k.contains("src/app.rs")),
            "default 0.85 floor should still admit the file→file FileReference edge \
             (app.rs references helper.rs); got {keys:?}"
        );
    }

    /// PR A2: `min_confidence` on the BFS frontier drops weak edges. A
    /// threshold above the highest confidence in the fixture must collapse
    /// the impact set to empty; mid-band thresholds must shrink it.
    /// We replicate the impact BFS inline (the production handler is async
    /// and needs an `MCPBridge`/db, neither cheap to spin up here).
    #[tokio::test]
    async fn impact_min_confidence_filters_bfs_frontier_pr_a2() {
        let graph = build_test_graph();
        let start =
            resolve_node_or_err(&graph, "scip-rust pkg src/helper.rs `helper`().").unwrap();

        fn run_bfs(
            graph: &djinn_graph::repo_graph::RepoDependencyGraph,
            start: petgraph::graph::NodeIndex,
            max_depth: usize,
            min_confidence: Option<f64>,
        ) -> usize {
            let mut visited = std::collections::HashSet::new();
            visited.insert(start);
            let mut queue = std::collections::VecDeque::new();
            queue.push_back((start, 0usize));
            let mut count = 0;
            while let Some((current, depth)) = queue.pop_front() {
                if depth > 0 {
                    count += 1;
                }
                if depth < max_depth {
                    for edge in graph
                        .graph()
                        .edges_directed(current, petgraph::Direction::Incoming)
                    {
                        if let Some(threshold) = min_confidence
                            && edge.weight().confidence < threshold
                        {
                            continue;
                        }
                        let source = edge.source();
                        if visited.insert(source) {
                            queue.push_back((source, depth + 1));
                        }
                    }
                }
            }
            count
        }

        let unfiltered = run_bfs(&graph, start, 3, None);
        assert!(unfiltered > 0, "fixture must yield a non-empty impact set");

        // Threshold above 1.0 collapses the frontier to empty.
        let strict = run_bfs(&graph, start, 3, Some(1.5));
        assert_eq!(
            strict, 0,
            "min_confidence=1.5 must drop every edge — got {strict} entries"
        );

        // A modest threshold must not exceed the unfiltered count and may
        // shrink it.
        let mid = run_bfs(&graph, start, 3, Some(0.85));
        assert!(
            mid <= unfiltered,
            "filtered count {mid} must be <= unfiltered {unfiltered}"
        );
    }

    // ── PR C1: `context` op tests ────────────────────────────────────

    /// Builds a synthetic graph and returns
    ///   (graph, helper_node_index, helper_uid_string)
    /// — used by the C1 tests below so they don't repeat the setup.
    fn build_context_fixture()
    -> (djinn_graph::repo_graph::RepoDependencyGraph, petgraph::graph::NodeIndex, String) {
        let graph = build_test_graph();
        let key = "scip-rust pkg src/helper.rs `helper`().";
        let node_index = match resolve_node(&graph, key) {
            ResolveOutcome::Found(idx) => idx,
            _ => panic!("expected helper symbol in fixture"),
        };
        (graph, node_index, key.to_string())
    }

    /// Replicates the production `context()` bucketing logic without
    /// spinning up an `MCPBridge`/db. Returns the populated maps so we
    /// can assert against them directly.
    fn collect_context_buckets(
        graph: &djinn_graph::repo_graph::RepoDependencyGraph,
        node_index: petgraph::graph::NodeIndex,
    ) -> (
        std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>>,
        std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>>,
    ) {
        use crate::mcp_bridge::graph_neighbors::{build_related_symbol, edge_category_for};
        use petgraph::Direction;
        let mut incoming: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
            std::collections::BTreeMap::new();
        let mut outgoing: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
            std::collections::BTreeMap::new();
        for dir in [Direction::Incoming, Direction::Outgoing] {
            for edge in graph.graph().edges_directed(node_index, dir) {
                let other_index = match dir {
                    Direction::Incoming => edge.source(),
                    Direction::Outgoing => edge.target(),
                };
                let other = graph.node(other_index);
                let cat = edge_category_for(Some(edge.weight()), other);
                let related = build_related_symbol(other, edge.weight().confidence);
                let bucket = match dir {
                    Direction::Incoming => incoming.entry(cat).or_default(),
                    Direction::Outgoing => outgoing.entry(cat).or_default(),
                };
                bucket.push(related);
            }
        }
        for buckets in [&mut incoming, &mut outgoing] {
            for entries in buckets.values_mut() {
                entries.sort_by(|a, b| {
                    b.confidence
                        .partial_cmp(&a.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.uid.cmp(&b.uid))
                });
                entries.truncate(30);
            }
        }
        (incoming, outgoing)
    }

    #[tokio::test]
    async fn context_buckets_match_neighbors_count_pr_c1() {
        // Plan acceptance: `incoming.calls.len()` (and the union over
        // every bucket) must equal what a sibling `neighbors` call
        // returns for the same node. Rebuild the neighbors() count
        // inline to keep the assertion graph-only.
        use petgraph::Direction;
        let (graph, node_index, _) = build_context_fixture();
        let (incoming, outgoing) = collect_context_buckets(&graph, node_index);

        let incoming_total: usize = incoming.values().map(|v| v.len()).sum();
        let outgoing_total: usize = outgoing.values().map(|v| v.len()).sum();

        let raw_incoming = graph
            .graph()
            .edges_directed(node_index, Direction::Incoming)
            .count();
        let raw_outgoing = graph
            .graph()
            .edges_directed(node_index, Direction::Outgoing)
            .count();

        // `helper` has at most 30 incoming/outgoing in the synthetic
        // fixture; with the hard cap not engaging, the bucketed total
        // must equal the raw edge count.
        assert!(
            raw_incoming <= 30,
            "fixture has too many incoming edges; widen the test"
        );
        assert!(
            raw_outgoing <= 30,
            "fixture has too many outgoing edges; widen the test"
        );
        assert_eq!(
            incoming_total, raw_incoming,
            "context.incoming bucket sum {incoming_total} != raw neighbors {raw_incoming}"
        );
        assert_eq!(
            outgoing_total, raw_outgoing,
            "context.outgoing bucket sum {outgoing_total} != raw neighbors {raw_outgoing}"
        );
    }

    #[tokio::test]
    async fn context_relationship_bucket_implements_pr_c1() {
        // The fixture wires `main` → `HelperTrait` via a SCIP
        // `is_implementation=true` relationship, which the
        // `RepoGraphEdgeKind::Implements` → `EdgeCategory::Implements`
        // mapping must surface in the outgoing.implements bucket.
        let graph = build_test_graph();
        let main_index = match resolve_node(&graph, "scip-rust pkg src/app.rs `main`().") {
            ResolveOutcome::Found(idx) => idx,
            _ => panic!("expected main symbol"),
        };
        let (_, outgoing) = collect_context_buckets(&graph, main_index);

        let implements = outgoing
            .get(&EdgeCategory::Implements)
            .cloned()
            .unwrap_or_default();
        assert!(
            implements.iter().any(|r| r.name.contains("HelperTrait")),
            "expected HelperTrait in outgoing.implements: {implements:?}"
        );
        // Confirm Extends bucket is *empty* — the fixture's relationship
        // only sets `is_implementation`, not `is_reference`.
        assert!(
            outgoing.get(&EdgeCategory::Extends).is_none()
                || outgoing.get(&EdgeCategory::Extends).unwrap().is_empty(),
            "outgoing.extends should be empty when only is_implementation is set"
        );
    }

    #[tokio::test]
    async fn context_imports_bucket_for_file_references_pr_c1() {
        // FileReference edges (file → symbol or file → file) land in
        // the Imports bucket. The fixture's `src/app.rs` references
        // the `helper` symbol, so we expect `helper` in
        // `src/app.rs`'s outgoing.imports.
        let graph = build_test_graph();
        let app_index = match resolve_node(&graph, "src/app.rs") {
            ResolveOutcome::Found(idx) => idx,
            _ => panic!("expected src/app.rs file node"),
        };
        let (_, outgoing) = collect_context_buckets(&graph, app_index);

        let imports = outgoing
            .get(&EdgeCategory::Imports)
            .cloned()
            .unwrap_or_default();
        assert!(
            imports.iter().any(|r| r.name == "helper"),
            "expected `helper` in src/app.rs outgoing.imports: {imports:?}"
        );
    }

    #[test]
    fn edge_category_table_pr_c1() {
        // Spot-check the EdgeCategory mapping — the contract table is
        // load-bearing for the UI parser so any silent rewrite must
        // break this test.
        use crate::mcp_bridge::graph_neighbors::edge_category_for;
        use djinn_graph::repo_graph::{RepoGraphEdge, RepoGraphEdgeKind};
        use djinn_graph::scip_parser::ScipSymbolKind;
        use std::path::PathBuf;

        let mk_edge = |kind: RepoGraphEdgeKind| RepoGraphEdge {
            kind,
            weight: 1.0,
            evidence_count: 1,
            confidence: 0.9,
            reason: None,
            step: None,
        };
        let mk_node = |kind: Option<ScipSymbolKind>| djinn_graph::repo_graph::RepoGraphNode {
            id: djinn_graph::repo_graph::RepoNodeKey::Symbol("x".into()),
            kind: djinn_graph::repo_graph::RepoGraphNodeKind::Symbol,
            display_name: "x".into(),
            language: None,
            file_path: Some(PathBuf::from("x.rs")),
            symbol: Some("x".into()),
            symbol_kind: kind,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
        };

        let any_node = mk_node(None);
        // SymbolReference with non-callable target → References.
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)), &any_node),
            EdgeCategory::References
        );
        // SymbolReference with Function target → Calls.
        let fn_node = mk_node(Some(ScipSymbolKind::Function));
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)), &fn_node),
            EdgeCategory::Calls
        );
        // SymbolReference with Method target → Calls.
        let method_node = mk_node(Some(ScipSymbolKind::Method));
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)), &method_node),
            EdgeCategory::Calls
        );
        // SymbolReference with Constructor target → Calls.
        let ctor_node = mk_node(Some(ScipSymbolKind::Constructor));
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)), &ctor_node),
            EdgeCategory::Calls
        );
        // PR A3 splits.
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Reads)), &any_node),
            EdgeCategory::Reads
        );
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Writes)), &any_node),
            EdgeCategory::Writes
        );
        // FileReference → Imports.
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::FileReference)), &any_node),
            EdgeCategory::Imports
        );
        // Containment.
        assert_eq!(
            edge_category_for(
                Some(&mk_edge(RepoGraphEdgeKind::ContainsDefinition)),
                &any_node
            ),
            EdgeCategory::Contains
        );
        assert_eq!(
            edge_category_for(
                Some(&mk_edge(RepoGraphEdgeKind::DeclaredInFile)),
                &any_node
            ),
            EdgeCategory::Contains
        );
        // Symbol relationships.
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Extends)), &any_node),
            EdgeCategory::Extends
        );
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Implements)), &any_node),
            EdgeCategory::Implements
        );
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::TypeDefines)), &any_node),
            EdgeCategory::TypeDefines
        );
        assert_eq!(
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Defines)), &any_node),
            EdgeCategory::Defines
        );
    }

    #[test]
    fn context_limit_30_per_category_pr_c1() {
        // Build a fan-in of 35 callers on a single symbol and verify
        // the `Calls` bucket truncates at 30, sorted desc by
        // confidence so the highest-confidence callers survive.
        use crate::mcp_bridge::graph_neighbors::{build_related_symbol, edge_category_for};
        use djinn_graph::repo_graph::*;
        use djinn_graph::scip_parser::*;
        use std::collections::BTreeSet;
        use std::path::PathBuf;

        let target_sym = "scip-rust pkg src/lib.rs `target`().".to_string();
        let target_symbol = ScipSymbol {
            symbol: target_sym.clone(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("target".to_string()),
            signature: Some("fn target()".to_string()),
            documentation: vec![],
            relationships: vec![],
            visibility: Some(ScipVisibility::Public),
            signature_parts: None,
        };
        let mut files: Vec<ScipFile> = vec![ScipFile {
            language: "rust".into(),
            relative_path: PathBuf::from("src/lib.rs"),
            definitions: vec![ScipOccurrence {
                symbol: target_sym.clone(),
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
            }],
            references: vec![],
            occurrences: vec![],
            symbols: vec![target_symbol],
        }];
        for i in 0..35 {
            let caller_sym = format!("scip-rust pkg src/c{i}.rs `caller{i}`().");
            files.push(ScipFile {
                language: "rust".into(),
                relative_path: PathBuf::from(format!("src/c{i}.rs")),
                definitions: vec![ScipOccurrence {
                    symbol: caller_sym.clone(),
                    range: ScipRange {
                        start_line: 0,
                        start_character: 0,
                        end_line: 0,
                        end_character: 8,
                    },
                    enclosing_range: None,
                    roles: BTreeSet::from([ScipSymbolRole::Definition]),
                    syntax_kind: None,
                    override_documentation: vec![],
                }],
                references: vec![ScipOccurrence {
                    symbol: target_sym.clone(),
                    range: ScipRange {
                        start_line: 1,
                        start_character: 4,
                        end_line: 1,
                        end_character: 10,
                    },
                    enclosing_range: None,
                    roles: BTreeSet::new(),
                    syntax_kind: None,
                    override_documentation: vec![],
                }],
                occurrences: vec![],
                symbols: vec![ScipSymbol {
                    symbol: caller_sym,
                    kind: Some(ScipSymbolKind::Function),
                    display_name: Some(format!("caller{i}")),
                    signature: None,
                    documentation: vec![],
                    relationships: vec![],
                    visibility: Some(ScipVisibility::Public),
                    signature_parts: None,
                }],
            });
        }
        let parsed = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files,
            external_symbols: vec![],
        };
        let graph = RepoDependencyGraph::build(&[parsed]);
        let target_node = graph
            .symbol_node(&target_sym)
            .expect("target should be in graph");

        // Collect incoming edges directly and bucket them.
        use petgraph::Direction;
        let mut by_cat: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
            std::collections::BTreeMap::new();
        for edge in graph
            .graph()
            .edges_directed(target_node, Direction::Incoming)
        {
            let other = graph.node(edge.source());
            let cat = edge_category_for(Some(edge.weight()), other);
            let related = build_related_symbol(other, edge.weight().confidence);
            by_cat.entry(cat).or_default().push(related);
        }
        for entries in by_cat.values_mut() {
            entries.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.uid.cmp(&b.uid))
            });
            entries.truncate(30);
        }

        // The fan-in mints `FileReference` edges from each caller-file
        // into the target symbol, which the EdgeCategory mapping
        // routes to `Imports`. With 35 raw incoming references, the
        // bucket must truncate at 30 (the plan-mandated hard cap).
        let imports_count = by_cat
            .get(&EdgeCategory::Imports)
            .map(|v| v.len())
            .unwrap_or(0);
        assert_eq!(
            imports_count, 30,
            "incoming.imports must hard-cap at 30; got {imports_count}"
        );
        // And confirm: at least one bucket actually exceeded the cap
        // pre-truncation (otherwise the test isn't exercising the cap).
        let raw_incoming = graph
            .graph()
            .edges_directed(target_node, Direction::Incoming)
            .count();
        assert!(
            raw_incoming >= 35,
            "fan-in fixture should produce >= 35 raw incoming edges, got {raw_incoming}"
        );
    }

    #[test]
    fn context_emits_processes_for_step_node_pr_f2() {
        // Build a 5-symbol linear chain (`main → a → b → c → d`) so the
        // F2 process detector emits one process. Then assert that the
        // C1 context-op-style construction populates the `processes`
        // field on a node that's a step in that flow.
        use djinn_graph::repo_graph::*;
        use djinn_graph::scip_parser::*;
        use std::collections::BTreeSet;
        use std::path::PathBuf;

        fn def_occ(symbol: &str) -> ScipOccurrence {
            ScipOccurrence {
                symbol: symbol.to_string(),
                range: ScipRange {
                    start_line: 0,
                    start_character: 0,
                    end_line: 0,
                    end_character: 4,
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
                    start_line: 0,
                    start_character: 0,
                    end_line: 0,
                    end_character: 4,
                },
                enclosing_range: None,
                roles: BTreeSet::new(),
                syntax_kind: None,
                override_documentation: vec![],
            }
        }
        fn rust_function(symbol: &str, name: &str) -> ScipSymbol {
            ScipSymbol {
                symbol: symbol.to_string(),
                kind: Some(ScipSymbolKind::Function),
                display_name: Some(name.to_string()),
                signature: Some(format!("fn {name}()")),
                documentation: vec![],
                relationships: vec![],
                visibility: Some(ScipVisibility::Public),
                signature_parts: None,
            }
        }

        let main_sym = "scip-rust pkg src/main.rs `main`().";
        let a_sym = "scip-rust pkg src/a.rs `a`().";
        let b_sym = "scip-rust pkg src/b.rs `b`().";

        let parsed = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![
                ScipFile {
                    language: "rust".into(),
                    relative_path: PathBuf::from("src/main.rs"),
                    definitions: vec![def_occ(main_sym)],
                    references: vec![ref_occ(a_sym)],
                    occurrences: vec![],
                    symbols: vec![rust_function(main_sym, "main")],
                },
                ScipFile {
                    language: "rust".into(),
                    relative_path: PathBuf::from("src/a.rs"),
                    definitions: vec![def_occ(a_sym)],
                    references: vec![ref_occ(b_sym)],
                    occurrences: vec![],
                    symbols: vec![rust_function(a_sym, "a")],
                },
                ScipFile {
                    language: "rust".into(),
                    relative_path: PathBuf::from("src/b.rs"),
                    definitions: vec![def_occ(b_sym)],
                    references: vec![],
                    occurrences: vec![],
                    symbols: vec![rust_function(b_sym, "b")],
                },
            ],
            external_symbols: vec![],
        };
        let graph = RepoDependencyGraph::build(&[parsed]);

        // Sanity: the detector ran and produced at least one process.
        assert!(
            !graph.processes().is_empty(),
            "linear chain should produce a process; got {:?}",
            graph.processes()
        );

        // The `b` symbol is a step in the `main` process.
        let b_idx = graph
            .symbol_node(b_sym)
            .expect("b symbol should be in the graph");
        let memberships = graph.processes_for_node(b_idx);
        assert!(
            !memberships.is_empty(),
            "node `b` must have process memberships"
        );

        // Mirror the wire-shape construction the bridge does.
        let process_refs: Vec<ProcessRef> = memberships
            .iter()
            .map(|p| ProcessRef {
                id: p.id.clone(),
                label: p.label.clone(),
                role: "step".to_string(),
            })
            .collect();
        assert!(
            process_refs.iter().any(|r| r.role == "step"),
            "every process_ref must carry role=\"step\""
        );
        assert!(
            process_refs
                .iter()
                .any(|r| r.label.contains("main") && r.label.contains("process")),
            "expected a process labeled `\"main process\"`: {:?}",
            process_refs.iter().map(|r| &r.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn context_method_metadata_none_when_signature_parts_absent_pr_c1() {
        // SCIP 0.7 ships only the markdown signature blob, so
        // `signature_parts` is None on every fixture. Per the plan
        // contract this MUST surface as `method_metadata: None` —
        // never regex-extracted from the markdown.
        use crate::mcp_bridge::graph_neighbors::build_method_metadata;
        let graph = build_test_graph();
        let helper_idx = graph
            .symbol_node("scip-rust pkg src/helper.rs `helper`().")
            .expect("helper exists");
        let helper = graph.node(helper_idx);
        assert!(
            helper.signature_parts.is_none(),
            "fixture should not carry structured signature_parts"
        );
        assert!(
            build_method_metadata(helper).is_none(),
            "method_metadata must be None when signature_parts is absent"
        );
    }

    #[test]
    fn context_method_metadata_some_when_signature_parts_present_pr_c1() {
        // Synthesise a signature_parts payload (as a future indexer
        // would) and assert the bridge surfaces it as MethodMeta.
        use crate::mcp_bridge::graph_neighbors::build_method_metadata;
        use djinn_graph::scip_parser::{ScipSignatureParam, ScipSignatureParts};

        let mut node = graph_neighbors_test_node();
        node.signature_parts = Some(ScipSignatureParts {
            parameters: vec![
                ScipSignatureParam {
                    name: "user".into(),
                    type_name: Some("User".into()),
                    default_value: None,
                },
                ScipSignatureParam {
                    name: "limit".into(),
                    type_name: Some("usize".into()),
                    default_value: Some("20".into()),
                },
            ],
            return_type: Some("Result<Vec<Item>, Error>".into()),
            type_parameters: vec!["T".into()],
            visibility: Some("pub".into()),
            is_async: Some(true),
            annotations: vec!["#[tracing::instrument]".into()],
        });
        let meta = build_method_metadata(&node).expect("metadata expected");
        assert_eq!(meta.params.len(), 2);
        assert_eq!(meta.params[0].name, "user");
        assert_eq!(meta.params[1].default_value.as_deref(), Some("20"));
        assert_eq!(meta.return_type.as_deref(), Some("Result<Vec<Item>, Error>"));
        assert_eq!(meta.is_async, Some(true));
        assert_eq!(meta.visibility.as_deref(), Some("pub"));
        assert_eq!(meta.annotations, vec!["#[tracing::instrument]"]);
    }

    fn graph_neighbors_test_node() -> djinn_graph::repo_graph::RepoGraphNode {
        use std::path::PathBuf;
        djinn_graph::repo_graph::RepoGraphNode {
            id: djinn_graph::repo_graph::RepoNodeKey::Symbol("x".into()),
            kind: djinn_graph::repo_graph::RepoGraphNodeKind::Symbol,
            display_name: "list_items".into(),
            language: Some("rust".into()),
            file_path: Some(PathBuf::from("src/lib.rs")),
            symbol: Some("scip-rust pkg src/lib.rs `list_items`().".into()),
            symbol_kind: Some(djinn_graph::scip_parser::ScipSymbolKind::Function),
            is_external: false,
            visibility: None,
            signature: Some("pub async fn list_items(...) -> Result<...>".into()),
            documentation: vec![],
            signature_parts: None,
            is_test: false,
        }
    }

    // ── PR D2: snapshot op tests ─────────────────────────────────────────

    #[test]
    fn snapshot_payload_returns_full_graph_under_cap_pr_d2() {
        // Tiny fixture (3 file nodes + 3 symbol nodes + edges between
        // them) — way under the 2000 default cap, so the snapshot must
        // emit every node and `truncated` must be `false`.
        use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
        let graph = build_test_graph();
        let ranking = graph.rank();
        let payload = build_snapshot_payload(
            &graph,
            &ranking,
            "proj-test".to_string(),
            "deadbeef".to_string(),
            "2026-04-28T00:00:00Z".to_string(),
            2_000,
            &GraphExclusions::empty(),
        );
        assert_eq!(payload.project_id, "proj-test");
        assert_eq!(payload.git_head, "deadbeef");
        assert_eq!(payload.generated_at, "2026-04-28T00:00:00Z");
        assert_eq!(payload.node_cap, 2_000);
        assert!(!payload.truncated, "tiny graph should not truncate");
        assert!(
            payload.total_nodes == payload.nodes.len(),
            "total_nodes should match emitted node count when uncapped: \
             total={} emitted={}",
            payload.total_nodes,
            payload.nodes.len()
        );
        assert!(
            payload.total_edges == payload.edges.len(),
            "total_edges should match emitted edge count when uncapped"
        );

        // Every node must carry the canonical RepoNodeKey prefix.
        // PR F2 added a third kind, `process`, for synthetic
        // execution-flow nodes.
        for node in &payload.nodes {
            assert!(
                node.id.starts_with("file:")
                    || node.id.starts_with("symbol:")
                    || node.id.starts_with("process:"),
                "node id missing prefix: {}",
                node.id
            );
            assert!(
                matches!(node.kind.as_str(), "file" | "symbol" | "process"),
                "unexpected node.kind {}",
                node.kind
            );
        }

        // Nodes must be in pagerank-desc order.
        for window in payload.nodes.windows(2) {
            assert!(
                window[0].pagerank >= window[1].pagerank,
                "nodes not sorted by pagerank desc: {} < {}",
                window[0].pagerank,
                window[1].pagerank
            );
        }

        // Every emitted edge endpoint must be a node we emitted (no
        // dangling references) — D2 acceptance criterion.
        let node_ids: std::collections::HashSet<&str> =
            payload.nodes.iter().map(|n| n.id.as_str()).collect();
        for edge in &payload.edges {
            assert!(
                node_ids.contains(edge.from.as_str()),
                "edge.from {} not in node set",
                edge.from
            );
            assert!(
                node_ids.contains(edge.to.as_str()),
                "edge.to {} not in node set",
                edge.to
            );
            assert!(
                edge.confidence >= 0.0 && edge.confidence <= 1.0,
                "edge confidence out of range: {}",
                edge.confidence
            );
        }
    }

    #[test]
    fn snapshot_payload_truncates_when_node_cap_smaller_than_graph_pr_d2() {
        // Cap below the graph's node count — `truncated` must be true,
        // emitted nodes must equal cap, and every emitted edge's
        // endpoints must be among the survivors.
        use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
        let graph = build_test_graph();
        let ranking = graph.rank();
        let cap = 2_usize;
        let payload = build_snapshot_payload(
            &graph,
            &ranking,
            "proj-test".to_string(),
            "deadbeef".to_string(),
            "2026-04-28T00:00:00Z".to_string(),
            cap,
            &GraphExclusions::empty(),
        );
        assert_eq!(
            payload.node_cap, cap,
            "node_cap echoed back unchanged"
        );
        assert!(
            payload.truncated,
            "should be truncated when total_nodes={} > cap={}",
            payload.total_nodes,
            cap
        );
        assert!(
            payload.nodes.len() <= cap,
            "emitted {} nodes, exceeds cap {}",
            payload.nodes.len(),
            cap
        );
        assert!(
            payload.total_nodes >= payload.nodes.len(),
            "total_nodes {} should be ≥ emitted {} on a truncating snapshot",
            payload.total_nodes,
            payload.nodes.len()
        );

        // No dangling edge endpoints — UI rendering depends on this.
        let node_ids: std::collections::HashSet<&str> =
            payload.nodes.iter().map(|n| n.id.as_str()).collect();
        for edge in &payload.edges {
            assert!(
                node_ids.contains(edge.from.as_str())
                    && node_ids.contains(edge.to.as_str()),
                "truncated snapshot leaked an edge {} → {} into the wire",
                edge.from,
                edge.to
            );
        }
    }

    /// PR F3 acceptance: when the canonical graph has detected
    /// communities, the snapshot payload's `community_id` field is
    /// populated for every node that joined a non-trivial community.
    /// We synthesize a graph via the artifact seam (two tight 3-node
    /// clusters joined by a thin bridge — the same fixture pattern
    /// used in the `communities` module's unit tests) and verify the
    /// adapter wires `RepoDependencyGraph::community_id(...)` through
    /// to `SnapshotNode::community_id`.
    #[test]
    fn snapshot_payload_populates_community_id_pr_f3() {
        use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
        use djinn_graph::repo_graph::{
            REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact,
            RepoGraphArtifactEdge, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
            RepoNodeKey,
        };

        let mk_node = |name: &str, file: &str| RepoGraphNode {
            id: RepoNodeKey::Symbol(format!("symbol:{name}")),
            kind: RepoGraphNodeKind::Symbol,
            display_name: name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(file)),
            symbol: Some(format!("symbol:{name}")),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
        };
        let nodes = vec![
            mk_node("auth_login", "src/auth/login.rs"),
            mk_node("auth_session", "src/auth/session.rs"),
            mk_node("auth_token", "src/auth/token.rs"),
            mk_node("billing_charge", "src/billing/charge.rs"),
            mk_node("billing_invoice", "src/billing/invoice.rs"),
            mk_node("billing_refund", "src/billing/refund.rs"),
        ];
        let edge = |s, t, w| RepoGraphArtifactEdge {
            source: s,
            target: t,
            kind: RepoGraphEdgeKind::SymbolReference,
            weight: w,
            evidence_count: 1,
            confidence: 0.9,
            reason: None,
            step: None,
        };
        // Two tight triangles + a thin bridge between clusters.
        let mut edges = vec![
            edge(0, 1, 5.0),
            edge(1, 0, 5.0),
            edge(1, 2, 5.0),
            edge(2, 1, 5.0),
            edge(0, 2, 5.0),
            edge(2, 0, 5.0),
            edge(3, 4, 5.0),
            edge(4, 3, 5.0),
            edge(4, 5, 5.0),
            edge(5, 4, 5.0),
            edge(3, 5, 5.0),
            edge(5, 3, 5.0),
            edge(2, 3, 0.5),
            edge(3, 2, 0.5),
        ];
        // Sort to keep the artifact output stable across runs.
        edges.sort_by_key(|e| (e.source, e.target));

        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges: std::collections::BTreeMap::new(),
            communities: vec![],
            processes: vec![],
        };
        // `from_artifact` does NOT run community detection (it
        // restores the persisted sidecar — empty here). To exercise
        // the detector against this fixture we re-run it manually
        // and install the result, mirroring how `finish()` does it
        // at build time. The detector is `pub`, so this is a
        // legitimate adapter call.
        let mut graph = RepoDependencyGraph::from_artifact(&artifact);
        let communities = djinn_graph::communities::detect_communities(&graph);
        assert!(
            !communities.is_empty(),
            "fixture should produce at least one community"
        );
        // Bypass `install_communities` (private) by round-tripping
        // through a populated artifact.
        let mut a2 = graph.to_artifact();
        a2.communities = communities;
        graph = RepoDependencyGraph::from_artifact(&a2);

        let ranking = graph.rank();
        let payload = build_snapshot_payload(
            &graph,
            &ranking,
            "proj-f3".to_string(),
            "deadbeef".to_string(),
            "2026-04-28T00:00:00Z".to_string(),
            2_000,
            &GraphExclusions::empty(),
        );

        // Every emitted node should carry a community_id (these are
        // all symbols in the two tight triangles — none of them is a
        // singleton).
        let with_community = payload
            .nodes
            .iter()
            .filter(|n| n.community_id.is_some())
            .count();
        assert!(
            with_community >= 4,
            "expected ≥4 nodes with a community_id, got {with_community}: {:?}",
            payload
                .nodes
                .iter()
                .map(|n| (n.id.clone(), n.community_id.clone()))
                .collect::<Vec<_>>()
        );

        // The auth and billing clusters should map to *different*
        // community ids — proves the adapter isn't lazily handing
        // back a single global id.
        let auth_id = payload
            .nodes
            .iter()
            .find(|n| n.id.contains("auth_login"))
            .and_then(|n| n.community_id.clone())
            .expect("auth_login should carry a community_id");
        let billing_id = payload
            .nodes
            .iter()
            .find(|n| n.id.contains("billing_charge"))
            .and_then(|n| n.community_id.clone())
            .expect("billing_charge should carry a community_id");
        assert_ne!(
            auth_id, billing_id,
            "auth and billing clusters should not share community_id"
        );
    }
}
