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
    ApiSurfaceEntry, BoundaryRule, BoundaryViolation, CallerRef, ChangedRange, CoordinatorOps,
    CoordinatorStatus, CycleGroup, CycleMember, DeadSymbolEntry, DeprecatedHit, DiffTouchesResult,
    EdgeEntry, GitOps, GraphNeighbor, GraphStatus, HotPathHit, HotspotEntry, ImpactEntry,
    ImpactResult, LspOps, LspWarning, MetricsAtResult, ModelPoolStatus, NeighborsResult,
    OrphanEntry, PathHop, PathResult, PoolStatus, ProjectCtx, RankedNode, RepoGraphOps,
    ResolveOutcome, RunningTaskInfo, RuntimeOps, SearchHit, SemanticQueryEmbedding, SlotPoolOps,
    SymbolAtHit, SymbolDescription, TouchedSymbol,
};
use petgraph::visit::EdgeRef;
use djinn_agent::actors::coordinator::CoordinatorHandle;
use djinn_agent::actors::slot::SlotPoolHandle;
use djinn_agent::lsp::LspManager;

pub(crate) mod graph_neighbors;

use self::graph_neighbors::{
    format_node_key, group_impact_by_file, group_neighbors_by_file, resolve_node_or_err,
    resolve_node_with_hint,
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
    ) -> Result<NeighborsResult, String> {
        use petgraph::Direction;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let node_index = resolve_node_or_err(&graph, key)?;
        let directions: Vec<Direction> = match direction {
            Some("incoming") => vec![Direction::Incoming],
            Some("outgoing") => vec![Direction::Outgoing],
            _ => vec![Direction::Incoming, Direction::Outgoing],
        };

        let mut neighbors = Vec::new();
        for dir in directions {
            let dir_label = match dir {
                Direction::Incoming => "incoming",
                Direction::Outgoing => "outgoing",
            };
            for edge in graph.graph().edges_directed(node_index, dir) {
                let other_index = match dir {
                    Direction::Outgoing => edge.target(),
                    Direction::Incoming => edge.source(),
                };
                let other_node = graph.node(other_index);
                neighbors.push((
                    other_node,
                    GraphNeighbor {
                        key: format_node_key(&other_node.id),
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
        let filter = match kind_filter {
            Some("file") => Some(RepoGraphNodeKind::File),
            Some("symbol") => Some(RepoGraphNodeKind::Symbol),
            _ => None,
        };
        let mut nodes: Vec<RankedNode> = ranking
            .nodes
            .iter()
            .filter(|node| filter.is_none() || Some(node.kind) == filter)
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
                }
            })
            .collect();

        match sort_by {
            None | Some("pagerank") => {
                // already in pagerank order
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
                    "invalid sort_by '{other}': expected 'pagerank', 'in_degree', \
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
        let node_index = graph
            .symbol_node(symbol)
            .ok_or_else(|| format!("symbol '{symbol}' not found in graph"))?;
        let mut impls = Vec::new();
        for edge in graph
            .graph()
            .edges_directed(node_index, petgraph::Direction::Incoming)
        {
            if edge.weight().kind == RepoGraphEdgeKind::SymbolRelationshipImplementation {
                let source_node = graph.node(edge.source());
                if let Some(sym) = &source_node.symbol {
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
        let start = resolve_node_or_err(&graph, key)?;
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
                        // PR C3: surface file_path so the response-shaping
                        // layer can bucket entries into modules for risk
                        // classification.
                        file_path: node
                            .file_path
                            .as_ref()
                            .map(|p| p.display().to_string()),
                    },
                ));
            }
            if depth < max_depth {
                for edge in graph
                    .graph()
                    .edges_directed(current, petgraph::Direction::Incoming)
                {
                    // PR A2: skip weak edges before they enter the BFS
                    // frontier so the impact set reflects only the
                    // confidence band the caller asked for.
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
        let filter = match kind_filter {
            Some("file") => Some(RepoGraphNodeKind::File),
            Some("symbol") => Some(RepoGraphNodeKind::Symbol),
            _ => None,
        };
        let hits = graph.search_by_name(query, filter, limit);
        Ok(hits
            .into_iter()
            .map(|hit| {
                let node = graph.node(hit.node_index);
                SearchHit {
                    key: format_node_key(&node.id),
                    kind: format!("{:?}", node.kind).to_lowercase(),
                    display_name: node.display_name.clone(),
                    score: hit.score,
                    file: node.file_path.as_ref().map(|p| p.display().to_string()),
                }
            })
            .collect())
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
        let nodes = graph.orphans(filter, vis, limit);
        Ok(nodes
            .into_iter()
            .map(|idx| {
                let node = graph.node(idx);
                OrphanEntry {
                    key: format_node_key(&node.id),
                    kind: format!("{:?}", node.kind).to_lowercase(),
                    display_name: node.display_name.clone(),
                    file: node.file_path.as_ref().map(|p| p.display().to_string()),
                    visibility: node
                        .visibility
                        .map(|v| v.as_str().to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                }
            })
            .collect())
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

    /// Symbols with zero incoming edges from the entry-point set
    /// (`main` functions, test/bench heuristics, public symbols in
    /// crate-root files).
    ///
    /// **V1 approximations** (documented for future parser work):
    /// * Test entry points are inferred heuristically from file paths
    ///   (`**/tests/**`, `**/*_test.rs`, `**/*_test.go`) and display
    ///   names (`test_*`, `*_test`) because the SCIP parser does not
    ///   yet surface `#[test]` / `#[tokio::test]` / `#[bench]`
    ///   annotations.
    /// * "Crate root re-export surface" is inferred from the file
    ///   path (`**/src/lib.rs` or `**/src/main.rs`).
    async fn dead_symbols(
        &self,
        ctx: &ProjectCtx,
        confidence: &str,
        limit: usize,
    ) -> Result<Vec<DeadSymbolEntry>, String> {
        use djinn_graph::repo_graph::{RepoGraphEdgeKind, RepoGraphNodeKind};
        use djinn_graph::scip_parser::{ScipSymbolKind, ScipVisibility};
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

        // Compile entry-path heuristics once.
        let test_dir = globset::Glob::new("**/tests/**")
            .map_err(|e| e.to_string())?
            .compile_matcher();
        let rs_test_file = globset::Glob::new("**/*_test.rs")
            .map_err(|e| e.to_string())?
            .compile_matcher();
        let go_test_file = globset::Glob::new("**/*_test.go")
            .map_err(|e| e.to_string())?
            .compile_matcher();
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
            let file_str = node
                .file_path
                .as_ref()
                .map(|p| p.display().to_string());
            let is_main = node.display_name == "main"
                && node.symbol_kind.as_ref() == Some(&ScipSymbolKind::Function);
            let name_hints_test = node.display_name.starts_with("test_")
                || node.display_name.ends_with("_test");
            let path_hints_test = file_str
                .as_deref()
                .map(|f| {
                    test_dir.is_match(f) || rs_test_file.is_match(f) || go_test_file.is_match(f)
                })
                .unwrap_or(false);
            let crate_root_public = node.visibility == Some(ScipVisibility::Public)
                && file_str
                    .as_deref()
                    .map(|f| crate_root_lib.is_match(f) || crate_root_main.is_match(f))
                    .unwrap_or(false);
            if is_main || name_hints_test || path_hints_test || crate_root_public {
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
                    RepoGraphEdgeKind::SymbolRelationshipImplementation => {
                        has_any_incoming = true;
                        has_relationship_ref_or_impl = true;
                        has_relationship_impl = true;
                    }
                    RepoGraphEdgeKind::SymbolRelationshipReference => {
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
                    RepoGraphEdgeKind::SymbolReference
                    | RepoGraphEdgeKind::SymbolRelationshipReference
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
        };
        let trait_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
            kind: Some(ScipSymbolKind::Type),
            display_name: Some("HelperTrait".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
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
                }
            })
            .collect();
        assert!(!nodes.is_empty());
        for node in &nodes {
            assert!(node.score >= 0.0);
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
            if edge.weight().kind
                == djinn_graph::repo_graph::RepoGraphEdgeKind::SymbolRelationshipImplementation
            {
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

}
