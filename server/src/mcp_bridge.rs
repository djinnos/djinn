/// Bridge trait implementations: connect djinn-mcp's abstract traits to
/// the server's concrete actor handles and managers.
///
/// Newtypes are required for CoordinatorHandle, SlotPoolHandle, LspManager,
/// and SyncManager because both the trait (djinn-mcp) and the implementor
/// (djinn-agent / crate::sync) are external to the server — orphan rule.
/// AppState is a server-local type so it implements RuntimeOps and GitOps directly.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use djinn_git::{GitActorHandle, GitError};
use djinn_mcp::bridge::{
    ChannelStatus, CoordinatorOps, CoordinatorStatus, CycleGroup, CycleMember, EdgeEntry,
    FileGroupEntry, GitOps, GraphDiff, GraphDiffEdge, GraphDiffNode, GraphNeighbor, GraphStatus,
    ImpactEntry, ImpactResult, LspOps, LspWarning, ModelPoolStatus, NeighborsResult, OrphanEntry,
    PathHop, PathResult, PoolStatus, RankedNode, RepoGraphOps, RunningTaskInfo, RuntimeOps,
    SearchHit, SlotPoolOps, SymbolDescription, SyncOps, SyncResult,
};
use petgraph::visit::EdgeRef;
use tokio::sync::RwLock;

use djinn_agent::actors::coordinator::CoordinatorHandle;
use djinn_agent::actors::slot::SlotPoolHandle;
use djinn_agent::lsp::LspManager;

use crate::sync::SyncManager;

// ── Graph cache ────────────────────────────────────────────────────────────────

/// Output bundle of the CPU-bound `ensure_canonical_graph` build pipeline,
/// produced on a `spawn_blocking` thread and consumed by the async tail that
/// writes DB caches and installs the in-memory canonical slot.
///
/// Tuple layout (kept as a type alias rather than a struct so the blocking
/// closure stays terse):
/// `(graph, rendered, serialized_blob, parse_ms, build_ms, rank_ms,
///   render_ms, serial_ms, node_count, edge_count)`
type CanonicalGraphBuildOutput = (
    crate::repo_graph::RepoDependencyGraph,
    crate::repo_map::RenderedRepoMap,
    Vec<u8>,
    u64,
    u64,
    u64,
    u64,
    u64,
    usize,
    usize,
);

struct CachedGraph {
    graph: crate::repo_graph::RepoDependencyGraph,
    project_path: PathBuf,
    git_head: String,
    last_warm_at: time::OffsetDateTime,
}

static GRAPH_CACHE: std::sync::LazyLock<RwLock<Option<CachedGraph>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

/// Previous canonical graph, kept around so `code_graph(operation="diff")`
/// can compare the current graph against the most recent prior version. The
/// slot is replaced (not accumulated) on every rebuild — only one historical
/// step is retained, in-memory only.
static PREVIOUS_GRAPH_CACHE: std::sync::LazyLock<RwLock<Option<CachedGraph>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

// ── Newtype wrappers ───────────────────────────────────────────────────────────

pub struct CoordinatorBridge(pub CoordinatorHandle);
pub struct SlotPoolBridge(pub SlotPoolHandle);
pub struct LspBridge(pub LspManager);
pub struct SyncBridge(pub SyncManager);

// ── CoordinatorBridge → CoordinatorOps ───────────────────────────────────────

#[async_trait]
impl CoordinatorOps for CoordinatorBridge {
    async fn resume_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .resume_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn resume(&self) -> Result<(), String> {
        self.0.resume().await.map_err(|e| e.to_string())
    }

    async fn pause_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .pause_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn pause_project_immediate(&self, project_id: &str, reason: &str) -> Result<(), String> {
        self.0
            .pause_project_immediate(project_id, reason)
            .await
            .map_err(|e| e.to_string())
    }

    async fn pause_immediate(&self, reason: &str) -> Result<(), String> {
        self.0
            .pause_immediate(reason)
            .await
            .map_err(|e| e.to_string())
    }

    fn get_status(&self) -> Result<CoordinatorStatus, String> {
        let s = self.0.get_status().map_err(|e| e.to_string())?;
        Ok(CoordinatorStatus {
            paused: s.paused,
            tasks_dispatched: s.tasks_dispatched,
            sessions_recovered: s.sessions_recovered,
            unhealthy_projects: s.unhealthy_projects,
            epic_throughput: s.epic_throughput,
            pr_errors: s.pr_errors,
        })
    }

    fn get_project_status(&self, project_id: &str) -> Result<CoordinatorStatus, String> {
        let s = self
            .0
            .get_project_status(project_id)
            .map_err(|e| e.to_string())?;
        Ok(CoordinatorStatus {
            paused: s.paused,
            tasks_dispatched: s.tasks_dispatched,
            sessions_recovered: s.sessions_recovered,
            unhealthy_projects: s.unhealthy_projects,
            epic_throughput: s.epic_throughput,
            pr_errors: s.pr_errors,
        })
    }

    async fn validate_project_health(&self, project_id: Option<String>) -> Result<(), String> {
        self.0
            .validate_project_health(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn trigger_dispatch_for_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .trigger_dispatch_for_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn pause(&self) -> Result<(), String> {
        self.0.pause().await.map_err(|e| e.to_string())
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

// ── SyncBridge → SyncOps ─────────────────────────────────────────────────────

#[async_trait]
impl SyncOps for SyncBridge {
    async fn enable_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .enable_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn disable_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .disable_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_remote_branch(&self, channel: &str, project_path: &Path) -> Result<(), String> {
        self.0
            .delete_remote_branch(channel, project_path)
            .await
            .map_err(|e| e.to_string())
    }

    async fn export_all(&self, user_id: Option<&str>) -> Vec<SyncResult> {
        self.0
            .export_all(user_id)
            .await
            .into_iter()
            .map(|r| SyncResult {
                channel: r.channel,
                ok: r.ok,
                count: r.count,
                error: r.error,
            })
            .collect()
    }

    async fn import_all(&self) -> Vec<SyncResult> {
        self.0
            .import_all()
            .await
            .into_iter()
            .map(|r| SyncResult {
                channel: r.channel,
                ok: r.ok,
                count: r.count,
                error: r.error,
            })
            .collect()
    }

    async fn status(&self) -> Vec<ChannelStatus> {
        self.0
            .status()
            .await
            .into_iter()
            .map(|s| ChannelStatus {
                name: s.name,
                branch: s.branch,
                enabled: s.enabled,
                project_paths: s.project_paths,
                last_synced_at: s.last_synced_at,
                last_error: s.last_error,
                failure_count: s.failure_count,
                backoff_secs: s.backoff_secs,
                needs_attention: s.needs_attention,
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

    async fn reset_runtime_settings(&self) {
        AppState::reset_runtime_settings(self).await;
    }

    async fn persist_model_health_state(&self) {
        AppState::persist_model_health_state(self).await;
    }

    async fn purge_worktrees(&self) {
        djinn_agent::actors::slot::purge_all_worktrees(&self.agent_context()).await;
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
pub struct RepoGraphBridge {
    state: AppState,
}

impl RepoGraphBridge {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl RepoGraphOps for RepoGraphBridge {
    async fn neighbors(
        &self,
        project_path: &str,
        key: &str,
        direction: Option<&str>,
        group_by: Option<&str>,
    ) -> Result<NeighborsResult, String> {
        use petgraph::Direction;
        let graph = build_graph_for_project(&self.state, project_path).await?;
        let node_index = resolve_node(&graph, key)?;
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
        project_path: &str,
        kind_filter: Option<&str>,
        sort_by: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RankedNode>, String> {
        use crate::repo_graph::RepoGraphNodeKind;
        let graph = build_graph_for_project(&self.state, project_path).await?;
        let ranking = graph.rank();
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
        project_path: &str,
        symbol: &str,
    ) -> Result<Vec<String>, String> {
        use crate::repo_graph::RepoGraphEdgeKind;
        let graph = build_graph_for_project(&self.state, project_path).await?;
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
        project_path: &str,
        key: &str,
        max_depth: usize,
        group_by: Option<&str>,
    ) -> Result<ImpactResult, String> {
        let graph = build_graph_for_project(&self.state, project_path).await?;
        let start = resolve_node(&graph, key)?;
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
                    },
                ));
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
        project_path: &str,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        use crate::repo_graph::RepoGraphNodeKind;
        let graph = build_graph_for_project(&self.state, project_path).await?;
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
        project_path: &str,
        kind_filter: Option<&str>,
        min_size: usize,
    ) -> Result<Vec<CycleGroup>, String> {
        use crate::repo_graph::RepoGraphNodeKind;
        let graph = build_graph_for_project(&self.state, project_path).await?;
        let filter = match kind_filter {
            Some("file") => Some(RepoGraphNodeKind::File),
            Some("symbol") => Some(RepoGraphNodeKind::Symbol),
            _ => None,
        };
        let min = min_size.max(2);
        let sccs = graph.strongly_connected_components(filter, min);
        Ok(sccs
            .into_iter()
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
        project_path: &str,
        kind_filter: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OrphanEntry>, String> {
        use crate::repo_graph::RepoGraphNodeKind;
        use crate::scip_parser::ScipVisibility;
        let graph = build_graph_for_project(&self.state, project_path).await?;
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
        project_path: &str,
        from: &str,
        to: &str,
        max_depth: Option<usize>,
    ) -> Result<Option<PathResult>, String> {
        let graph = build_graph_for_project(&self.state, project_path).await?;
        let from_idx = resolve_node(&graph, from)?;
        let to_idx = resolve_node(&graph, to)?;
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
        project_path: &str,
        from_glob: &str,
        to_glob: &str,
        edge_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EdgeEntry>, String> {
        use globset::Glob;
        let graph = build_graph_for_project(&self.state, project_path).await?;
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

    async fn diff(
        &self,
        project_path: &str,
        since: Option<&str>,
    ) -> Result<Option<GraphDiff>, String> {
        match since {
            None | Some("previous") => {}
            Some(other) => {
                return Err(format!(
                    "invalid since '{other}': only 'previous' is currently supported \
                     (persistent cross-commit diff is not yet implemented)"
                ));
            }
        }
        // Ensure the current canonical graph for this project is built / cached.
        let _ = build_graph_for_project(&self.state, project_path).await?;

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
        let Some((current, head_commit)) = current else {
            return Ok(None);
        };
        let Some((previous, base_commit)) = previous else {
            return Ok(Some(GraphDiff {
                base_commit: None,
                head_commit: Some(head_commit),
                added_nodes: collect_diff_nodes(&current),
                removed_nodes: vec![],
                added_edges: collect_diff_edges(&current),
                removed_edges: vec![],
            }));
        };
        Ok(Some(compute_graph_diff(
            &previous,
            base_commit,
            &current,
            head_commit,
        )))
    }

    async fn describe(
        &self,
        project_path: &str,
        key: &str,
    ) -> Result<Option<SymbolDescription>, String> {
        let graph = build_graph_for_project(&self.state, project_path).await?;
        let node_index = match resolve_node(&graph, key) {
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

    async fn status(&self, project_path: &str) -> Result<GraphStatus, String> {
        use djinn_db::ProjectRepository;
        use time::format_description::well_known::Rfc3339;

        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_id = repo
            .resolve(project_path)
            .await
            .map_err(|e| format!("resolve project: {e}"))?
            .ok_or_else(|| format!("no project registered for path '{project_path}'"))?;

        let project_root = PathBuf::from(project_path);
        let index_tree_path = project_root.join(".djinn").join("worktrees").join("_index");

        let snapshot = {
            let cache = GRAPH_CACHE.read().await;
            cache.as_ref().and_then(|cached| {
                if cached.project_path == index_tree_path {
                    Some((cached.git_head.clone(), cached.last_warm_at))
                } else {
                    None
                }
            })
        };

        let Some((pinned_commit, last_warm_at)) = snapshot else {
            return Ok(GraphStatus {
                project_id,
                warmed: false,
                last_warm_at: None,
                pinned_commit: None,
                commits_since_pin: None,
            });
        };

        let last_warm_at_str = last_warm_at
            .format(&Rfc3339)
            .map_err(|e| format!("format last_warm_at: {e}"))?;

        let commits_since_pin = count_commits_since(&project_root, &pinned_commit).await;

        Ok(GraphStatus {
            project_id,
            warmed: true,
            last_warm_at: Some(last_warm_at_str),
            pinned_commit: Some(pinned_commit),
            commits_since_pin,
        })
    }
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

fn group_neighbors_by_file(
    neighbors: &[(&crate::repo_graph::RepoGraphNode, GraphNeighbor)],
) -> Vec<FileGroupEntry> {
    let mut by_file: std::collections::BTreeMap<String, FileGroupEntry> =
        std::collections::BTreeMap::new();
    for (node, neighbor) in neighbors {
        let file_label = node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| match &node.id {
                crate::repo_graph::RepoNodeKey::File(p) => p.display().to_string(),
                crate::repo_graph::RepoNodeKey::Symbol(s) => s.clone(),
            });
        let entry = by_file.entry(file_label.clone()).or_insert(FileGroupEntry {
            file: file_label,
            occurrence_count: 0,
            max_depth: 1,
            sample_keys: Vec::new(),
        });
        entry.occurrence_count += 1;
        if entry.sample_keys.len() < 5 {
            entry.sample_keys.push(neighbor.key.clone());
        }
    }
    by_file.into_values().collect()
}

fn group_impact_by_file(
    graph: &crate::repo_graph::RepoDependencyGraph,
    entries: &[(petgraph::graph::NodeIndex, ImpactEntry)],
) -> Vec<FileGroupEntry> {
    let mut by_file: std::collections::BTreeMap<String, FileGroupEntry> =
        std::collections::BTreeMap::new();
    for (idx, entry) in entries {
        let node = graph.node(*idx);
        let file_label = node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| node.display_name.clone());
        let group = by_file.entry(file_label.clone()).or_insert(FileGroupEntry {
            file: file_label,
            occurrence_count: 0,
            max_depth: 0,
            sample_keys: Vec::new(),
        });
        group.occurrence_count += 1;
        if entry.depth > group.max_depth {
            group.max_depth = entry.depth;
        }
        if group.sample_keys.len() < 5 {
            group.sample_keys.push(entry.key.clone());
        }
    }
    by_file.into_values().collect()
}

fn collect_diff_nodes(graph: &crate::repo_graph::RepoDependencyGraph) -> Vec<GraphDiffNode> {
    graph
        .graph()
        .node_indices()
        .map(|idx| {
            let node = graph.node(idx);
            GraphDiffNode {
                key: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
            }
        })
        .collect()
}

fn collect_diff_edges(graph: &crate::repo_graph::RepoDependencyGraph) -> Vec<GraphDiffEdge> {
    graph
        .graph()
        .edge_references()
        .map(|edge| GraphDiffEdge {
            from: format_node_key(&graph.node(edge.source()).id),
            to: format_node_key(&graph.node(edge.target()).id),
            edge_kind: format!("{:?}", edge.weight().kind),
        })
        .collect()
}

fn compute_graph_diff(
    previous: &crate::repo_graph::RepoDependencyGraph,
    base_commit: String,
    current: &crate::repo_graph::RepoDependencyGraph,
    head_commit: String,
) -> GraphDiff {
    use std::collections::BTreeSet;

    fn node_keys(graph: &crate::repo_graph::RepoDependencyGraph) -> BTreeSet<String> {
        graph
            .graph()
            .node_indices()
            .map(|idx| format_node_key(&graph.node(idx).id))
            .collect()
    }

    fn edge_keys(
        graph: &crate::repo_graph::RepoDependencyGraph,
    ) -> BTreeSet<(String, String, String)> {
        graph
            .graph()
            .edge_references()
            .map(|edge| {
                (
                    format_node_key(&graph.node(edge.source()).id),
                    format_node_key(&graph.node(edge.target()).id),
                    format!("{:?}", edge.weight().kind),
                )
            })
            .collect()
    }

    let prev_nodes = node_keys(previous);
    let curr_nodes = node_keys(current);
    let prev_edges = edge_keys(previous);
    let curr_edges = edge_keys(current);

    let added_nodes: Vec<GraphDiffNode> = curr_nodes
        .difference(&prev_nodes)
        .map(|key| {
            let display = current
                .graph()
                .node_indices()
                .find(|idx| format_node_key(&current.node(*idx).id) == *key)
                .map(|idx| {
                    let node = current.node(idx);
                    (
                        node.display_name.clone(),
                        format!("{:?}", node.kind).to_lowercase(),
                    )
                })
                .unwrap_or_else(|| (key.clone(), "unknown".to_string()));
            GraphDiffNode {
                key: key.clone(),
                kind: display.1,
                display_name: display.0,
            }
        })
        .collect();
    let removed_nodes: Vec<GraphDiffNode> = prev_nodes
        .difference(&curr_nodes)
        .map(|key| {
            let display = previous
                .graph()
                .node_indices()
                .find(|idx| format_node_key(&previous.node(*idx).id) == *key)
                .map(|idx| {
                    let node = previous.node(idx);
                    (
                        node.display_name.clone(),
                        format!("{:?}", node.kind).to_lowercase(),
                    )
                })
                .unwrap_or_else(|| (key.clone(), "unknown".to_string()));
            GraphDiffNode {
                key: key.clone(),
                kind: display.1,
                display_name: display.0,
            }
        })
        .collect();
    let added_edges: Vec<GraphDiffEdge> = curr_edges
        .difference(&prev_edges)
        .map(|(from, to, edge_kind)| GraphDiffEdge {
            from: from.clone(),
            to: to.clone(),
            edge_kind: edge_kind.clone(),
        })
        .collect();
    let removed_edges: Vec<GraphDiffEdge> = prev_edges
        .difference(&curr_edges)
        .map(|(from, to, edge_kind)| GraphDiffEdge {
            from: from.clone(),
            to: to.clone(),
            edge_kind: edge_kind.clone(),
        })
        .collect();

    GraphDiff {
        base_commit: Some(base_commit),
        head_commit: Some(head_commit),
        added_nodes,
        removed_nodes,
        added_edges,
        removed_edges,
    }
}

fn format_node_key(key: &crate::repo_graph::RepoNodeKey) -> String {
    match key {
        crate::repo_graph::RepoNodeKey::File(path) => {
            format!("file:{}", path.display())
        }
        crate::repo_graph::RepoNodeKey::Symbol(sym) => {
            format!("symbol:{sym}")
        }
    }
}

fn resolve_node(
    graph: &crate::repo_graph::RepoDependencyGraph,
    key: &str,
) -> Result<petgraph::graph::NodeIndex, String> {
    // Try file path first (strip optional "file:" prefix)
    let stripped = key.strip_prefix("file:").unwrap_or(key);
    if let Some(index) = graph.file_node(stripped) {
        return Ok(index);
    }
    // Try symbol (strip optional "symbol:" prefix)
    let stripped = key.strip_prefix("symbol:").unwrap_or(key);
    if let Some(index) = graph.symbol_node(stripped) {
        return Ok(index);
    }
    Err(format!("node '{key}' not found in graph"))
}

/// ADR-050 §3 Chunk C canonical-main graph entrypoint.
///
/// Idempotently makes the SCIP graph for `origin/main` of the supplied
/// project available, returning a clone of the cached `RepoDependencyGraph`.
/// Flow:
///
/// 1. `IndexTree::ensure(project_id, project_root)` brings up
///    `<project_root>/.djinn/worktrees/_index/`.
/// 2. `fetch_if_stale(60s)` runs `git fetch origin main` against the
///    project root unless the cooldown blocks it.
/// 3. `reset_to_origin_main()` hard-resets the index tree to the freshly
///    fetched `origin/main` commit.
/// 4. Look up `repo_graph_cache[(project_id, commit_sha)]`.
///    - **Hit**: deserialize, install as canonical (moving the prior
///      canonical into the in-memory previous slot per ADR-050 §3 diff
///      contract), return.
///    - **Miss**: acquire the server-wide `IndexerLock`, re-check the cache
///      under the lock, then run SCIP indexers in the index tree, build
///      the graph, persist to `repo_graph_cache`, install as canonical,
///      release the lock, and return.
///
/// The returned `IndexTreeHandle` is also exposed to callers so they can
/// reuse its `path()` as the architect/chat `working_root` and so worker
/// dispatch sites can render the canonical skeleton from the same path.
pub async fn ensure_canonical_graph(
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
    // Best-effort: a missing remote is fine for tests / fresh repos.
    let _ = handle
        .fetch_if_stale(crate::index_tree::DEFAULT_FETCH_COOLDOWN)
        .await;
    // Best-effort reset; if origin/main is unavailable we keep whatever
    // commit the index tree was created on.
    let _ = handle.reset_to_origin_main().await;

    let commit_sha = handle.commit_sha().to_string();
    let cache_repo = RepoGraphCacheRepository::new(state.db().clone());

    // ── In-memory fast path ─────────────────────────────────────────────
    {
        let cache = GRAPH_CACHE.read().await;
        if let Some(cached) = cache.as_ref()
            && cached.project_path == handle.path()
            && cached.git_head == commit_sha
        {
            return Ok((handle, cached.graph.clone()));
        }
    }

    // ── Persistent cache path ───────────────────────────────────────────
    if let Ok(Some(row)) = cache_repo.get(project_id, &commit_sha).await {
        match bincode::deserialize::<crate::repo_graph::RepoGraphArtifact>(&row.graph_blob) {
            Ok(artifact) => {
                let graph = crate::repo_graph::RepoDependencyGraph::from_artifact(&artifact);
                install_as_canonical(
                    handle.path().to_path_buf(),
                    commit_sha.clone(),
                    graph.clone(),
                )
                .await;
                return Ok((handle, graph));
            }
            Err(e) => {
                tracing::warn!(
                    project_id = %project_id,
                    commit_sha = %commit_sha,
                    error = %e,
                    "ensure_canonical_graph: stale JSON-encoded graph_blob; re-indexing"
                );
                // Fall through to the cache-miss path.
            }
        }
    }

    // ── Cache miss: single-flight indexer run ───────────────────────────
    let lock = state.indexer_lock();
    let _permit = lock.lock().await;

    // Re-check both caches under the lock; another task may have populated
    // them while we were queued.
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
        match bincode::deserialize::<crate::repo_graph::RepoGraphArtifact>(&row.graph_blob) {
            Ok(artifact) => {
                let graph = crate::repo_graph::RepoDependencyGraph::from_artifact(&artifact);
                install_as_canonical(
                    handle.path().to_path_buf(),
                    commit_sha.clone(),
                    graph.clone(),
                )
                .await;
                return Ok((handle, graph));
            }
            Err(e) => {
                tracing::warn!(
                    project_id = %project_id,
                    commit_sha = %commit_sha,
                    error = %e,
                    "ensure_canonical_graph: stale JSON-encoded graph_blob; re-indexing"
                );
                // Fall through to the indexer-run path below.
            }
        }
    }

    // Wrap the SCIP scratch dir in a `TempDir` so its destructor removes the
    // directory even when the build pipeline panics, the spawn_blocking task
    // is cancelled, or the lifecycle timeout fires before we reach the
    // explicit `remove_dir_all` below.  Without this, every aborted run
    // leaked ~150 MB of SCIP artifacts under `/tmp` (which is tmpfs on
    // many Linux setups), eventually filling RAM.
    let output_temp = tempfile::Builder::new()
        .prefix("djinn-canonical-graph-")
        .tempdir()
        .map_err(|e| format!("create canonical-graph tempdir: {e}"))?;
    let output_dir = output_temp.path().to_path_buf();
    let target_dir = handle.target_dir().to_path_buf();
    // The server-wide IndexerLock is already held above (`_permit`); use the
    // `_already_locked` entrypoint instead of re-acquiring it via a dummy
    // mutex.  ADR-050 Chunk C cleanup.
    let t_indexers = std::time::Instant::now();
    let run =
        crate::repo_map::run_indexers_already_locked(handle.path(), &output_dir, Some(&target_dir))
            .await
            .map_err(|e| format!("run_indexers: {e}"))?;
    let indexers_ms = t_indexers.elapsed().as_millis() as u64;

    // ── CPU-bound section on a blocking thread ─────────────────────────────
    // Parsing a 20 MB SCIP artifact, building the DiGraph, computing
    // PageRank, and rendering the skeleton are all synchronous CPU work.
    // Running them on a tokio worker starves the runtime for tens of
    // minutes on cold cache — observed 2026-04-07: a single warm pinned
    // one `tokio-rt-worker` at ~90% CPU for 30+ min and blocked every
    // other session lifecycle serialized behind `indexer_lock`.
    //
    // Moving the entire post-SCIP pipeline into `spawn_blocking` keeps
    // the runtime healthy: other tokio tasks progress while a blocking
    // thread churns through the graph.  Each sub-step is timed so the
    // next recurrence names its slow step in the logs instead of
    // forcing another offline investigation.
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

            let t_rank = std::time::Instant::now();
            let ranking = graph.rank();
            let rank_ms = t_rank.elapsed().as_millis() as u64;

            let t_render = std::time::Instant::now();
            let rendered = crate::repo_map::render_repo_map(
                &graph,
                &ranking,
                &crate::repo_map::RepoMapRenderOptions::new(SKELETON_TOKEN_BUDGET),
            )
            .map_err(|e| format!("render_repo_map: {e:?}"))?;
            let render_ms = t_render.elapsed().as_millis() as u64;

            let t_serial = std::time::Instant::now();
            let serialized = bincode::serialize(&graph.to_artifact())
                .map_err(|e| format!("bincode serialize graph: {e}"))?;
            let serial_ms = t_serial.elapsed().as_millis() as u64;

            Ok((
                graph, rendered, serialized, parse_ms, build_ms, rank_ms, render_ms, serial_ms,
                node_count, edge_count,
            ))
        })
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?;
    let (
        graph,
        rendered,
        serialized_blob,
        parse_ms,
        build_ms,
        rank_ms,
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
        rank_ms,
        render_ms,
        serial_ms,
        node_count,
        edge_count,
        "ensure_canonical_graph: build pipeline complete"
    );

    // ── ADR-050 Chunk C C10: persist canonical skeleton as a
    // `repo_map_cache` row + `repo_map` note so workers (which consume
    // the rendered skeleton through the existing note pipeline) and the
    // chat (which reads `RepoMapCacheRepository` directly) see the new
    // graph.  Best effort: failures are logged but do not abort the
    // graph hand-off.  CPU-bound rank/render already ran inside the
    // `spawn_blocking` above; this helper now only does async DB writes.
    persist_canonical_skeleton(
        state,
        project_id,
        project_root,
        &commit_sha,
        &graph,
        &rendered,
    )
    .await;

    // Persist bincode blob (best-effort — failure is logged but does not abort).
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
    )
    .await;
    Ok((handle, graph))
}

/// ADR-050 Chunk C C10: persist a pre-rendered canonical-main repo-map
/// skeleton via the existing `RepoMapCacheRepository` + `repo_map` note
/// pipeline so worker sessions (which consume the skeleton through the
/// standard note path) see the canonical view.  The caller is responsible
/// for producing `rendered` on a blocking thread — this function is purely
/// async DB work.  Failures are logged and do not propagate.
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

/// Fast-path lookup against the in-memory `GRAPH_CACHE` used by the
/// canonical-graph warmer to decide whether a detached background warm task
/// is required.  Returns `true` iff there is a cached graph whose
/// `project_path` matches the supplied `_index` worktree path — which, in
/// this process, is only ever populated by a previous successful
/// `ensure_canonical_graph` run for this project.  We intentionally do NOT
/// verify the commit SHA here: resolving the current `origin/main` SHA
/// requires a `git fetch` and `git rev-parse` round-trip that would
/// reintroduce the very blocking behavior the detached warmer is designed
/// to avoid.  Instead, the background task itself does the full
/// commit-accurate check and refetches the graph if `origin/main` has
/// advanced.  A cold cache (no entry at all) returns `false`, causing the
/// warmer to spawn the background task.
pub async fn canonical_graph_cache_has_entry_for(index_tree_path: &Path) -> bool {
    let cache = GRAPH_CACHE.read().await;
    cache
        .as_ref()
        .is_some_and(|cached| cached.project_path == index_tree_path)
}

/// Replace the in-memory canonical graph slot, moving the previous canonical
/// into the diff predecessor slot per ADR-050 §3.
async fn install_as_canonical(
    project_path: PathBuf,
    git_head: String,
    graph: crate::repo_graph::RepoDependencyGraph,
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
    });
}

/// `RepoGraphOps` shim used by every operation: resolves the project ID for
/// the supplied `project_path` and delegates to `ensure_canonical_graph`,
/// returning the cached graph clone.
async fn build_graph_for_project(
    state: &AppState,
    project_path: &str,
) -> Result<crate::repo_graph::RepoDependencyGraph, String> {
    use djinn_db::ProjectRepository;
    let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let project_id = repo
        .resolve(project_path)
        .await
        .map_err(|e| format!("resolve project: {e}"))?
        .ok_or_else(|| format!("no project registered for path '{project_path}'"))?;
    let project_root = std::path::PathBuf::from(project_path);
    let (_handle, graph) = ensure_canonical_graph(state, &project_id, &project_root).await?;
    Ok(graph)
}

/// Test helper: install a graph as the current canonical graph and the
/// supplied predecessor in the previous slot. Used by integration tests for
/// the `diff` operation.
#[cfg(test)]
#[allow(dead_code)]
async fn install_test_graphs(
    project_path: &Path,
    previous: Option<crate::repo_graph::RepoDependencyGraph>,
    current: crate::repo_graph::RepoDependencyGraph,
) {
    {
        let mut prev_cache = PREVIOUS_GRAPH_CACHE.write().await;
        *prev_cache = previous.map(|graph| CachedGraph {
            graph,
            project_path: project_path.to_path_buf(),
            git_head: "previous".into(),
            last_warm_at: time::OffsetDateTime::now_utc(),
        });
    }
    {
        let mut cache = GRAPH_CACHE.write().await;
        *cache = Some(CachedGraph {
            graph: current,
            project_path: project_path.to_path_buf(),
            git_head: "current".into(),
            last_warm_at: time::OffsetDateTime::now_utc(),
        });
    }
}

impl AppState {
    /// Build a `djinn_mcp::McpState` from this AppState, wiring all bridge impls.
    ///
    /// Snapshots the current coordinator and pool handles via `try_lock()`.
    /// In production this is called after `initialize_agents()`, so both are
    /// populated. In tests neither is initialised; tools return graceful errors.
    pub fn mcp_state(&self) -> djinn_mcp::McpState {
        let coordinator = self
            .coordinator_sync()
            .map(|c| Arc::new(CoordinatorBridge(c)) as Arc<dyn CoordinatorOps>);
        let pool = self
            .pool_sync()
            .map(|p| Arc::new(SlotPoolBridge(p)) as Arc<dyn SlotPoolOps>);

        djinn_mcp::McpState::new(
            self.db().clone(),
            self.event_bus(),
            self.catalog().clone(),
            self.health_tracker().clone(),
            self.sync_user_id().to_string(),
            coordinator,
            pool,
            Arc::new(LspBridge(self.lsp().clone())),
            Arc::new(SyncBridge(self.sync_manager().clone())),
            Arc::new(self.clone()),
            Arc::new(self.clone()),
            Arc::new(RepoGraphBridge::new(self.clone())),
        )
    }
}

#[cfg(test)]
mod graph_bridge_tests {
    use super::*;
    use crate::repo_graph::{RepoDependencyGraph, RepoNodeKey};
    use crate::scip_parser::{
        ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
        ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn fixture_index() -> ParsedScipIndex {
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

    pub(super) fn build_test_graph() -> RepoDependencyGraph {
        RepoDependencyGraph::build(&[fixture_index()])
    }

    #[test]
    fn resolve_node_finds_file_by_path() {
        let graph = build_test_graph();
        assert!(resolve_node(&graph, "src/app.rs").is_ok());
        assert!(resolve_node(&graph, "file:src/app.rs").is_ok());
    }

    #[test]
    fn resolve_node_finds_symbol_by_name() {
        let graph = build_test_graph();
        assert!(resolve_node(&graph, "scip-rust pkg src/helper.rs `helper`().").is_ok());
        assert!(resolve_node(&graph, "symbol:scip-rust pkg src/helper.rs `helper`().").is_ok());
    }

    #[test]
    fn resolve_node_returns_error_for_unknown() {
        let graph = build_test_graph();
        let err = resolve_node(&graph, "nonexistent").unwrap_err();
        assert!(err.contains("not found"));
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
        let node_index = resolve_node(&graph, "src/app.rs").unwrap();
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
        // Should contain the helper symbol as a neighbor
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
        // Scores should be non-negative
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
                == crate::repo_graph::RepoGraphEdgeKind::SymbolRelationshipImplementation
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
        let start = resolve_node(&graph, "scip-rust pkg src/helper.rs `helper`().").unwrap();
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
        // The helper symbol should be depended on by src/app.rs (via file reference)
        assert!(
            !result.is_empty(),
            "expected at least one node in the impact set"
        );
    }

    #[test]
    fn compute_graph_diff_reports_added_and_removed_nodes() {
        // Previous graph has one file; current graph has that file plus a new one.
        let previous = build_test_graph();
        let current = {
            let new_index = ParsedScipIndex {
                metadata: ScipMetadata::default(),
                files: vec![ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/new_module.rs"),
                    definitions: vec![ScipOccurrence {
                        symbol: "scip-rust pkg src/new_module.rs `new_sym`().".to_string(),
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
                    symbols: vec![ScipSymbol {
                        symbol: "scip-rust pkg src/new_module.rs `new_sym`().".to_string(),
                        kind: Some(ScipSymbolKind::Function),
                        display_name: Some("new_sym".to_string()),
                        signature: None,
                        documentation: vec![],
                        relationships: vec![],
                        visibility: Some(crate::scip_parser::ScipVisibility::Public),
                    }],
                }],
                external_symbols: vec![],
            };
            let mut files = fixture_index().files;
            files.push(new_index.files.into_iter().next().unwrap());
            RepoDependencyGraph::build(&[ParsedScipIndex {
                metadata: ScipMetadata::default(),
                files,
                external_symbols: vec![],
            }])
        };

        let diff = compute_graph_diff(&previous, "base".to_string(), &current, "head".to_string());
        assert_eq!(diff.base_commit.as_deref(), Some("base"));
        assert_eq!(diff.head_commit.as_deref(), Some("head"));
        let added_names: Vec<String> = diff
            .added_nodes
            .iter()
            .map(|n| n.display_name.clone())
            .collect();
        assert!(
            added_names
                .iter()
                .any(|n| n.contains("new_module") || n == "new_sym"),
            "expected new_module.rs or new_sym in added nodes, got {:?}",
            added_names
        );
        assert!(
            diff.removed_nodes.is_empty(),
            "no nodes should be removed in this scenario"
        );
    }

    /// `RepoGraphBridge::status` returns `warmed: false` with all optional
    /// fields `None` when `GRAPH_CACHE` has no entry matching the project's
    /// `_index` worktree path.  No SCIP indexing is triggered.
    #[tokio::test]
    async fn status_returns_unwarmed_for_empty_cache() {
        use crate::test_helpers::create_test_db;
        use djinn_db::ProjectRepository;
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().unwrap();
        // Use a unique project_root per test to avoid the global GRAPH_CACHE
        // colliding with concurrently-running test cases.
        let project_root = tmp.path().join("status-empty-repo");
        tokio::fs::create_dir_all(&project_root).await.unwrap();

        let db = create_test_db();
        let cancel = CancellationToken::new();
        let state = crate::server::AppState::new(db.clone(), cancel);
        let proj_repo = ProjectRepository::new(db.clone(), state.event_bus());
        let project = proj_repo
            .create("test-status-empty", project_root.to_string_lossy().as_ref())
            .await
            .expect("create project");

        let bridge = RepoGraphBridge::new(state);
        let project_root_str = project_root.to_string_lossy().into_owned();
        let status = bridge.status(&project_root_str).await.expect("status ok");
        assert_eq!(status.project_id, project.id);
        assert!(!status.warmed);
        assert!(status.last_warm_at.is_none());
        assert!(status.pinned_commit.is_none());
        assert!(status.commits_since_pin.is_none());
    }

    /// `RepoGraphBridge::status` returns `warmed: true` together with
    /// `pinned_commit` and an RFC3339 `last_warm_at` when the in-memory
    /// canonical cache slot has an entry whose `project_path` matches the
    /// project's `_index` worktree path.
    #[tokio::test]
    async fn status_returns_warmed_when_cache_populated() {
        use crate::test_helpers::create_test_db;
        use djinn_db::ProjectRepository;
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("status-warm-repo");
        tokio::fs::create_dir_all(&project_root).await.unwrap();

        let db = create_test_db();
        let cancel = CancellationToken::new();
        let state = crate::server::AppState::new(db.clone(), cancel);
        let proj_repo = ProjectRepository::new(db.clone(), state.event_bus());
        let project = proj_repo
            .create("test-status-warm", project_root.to_string_lossy().as_ref())
            .await
            .expect("create project");

        // Plant a CachedGraph entry whose project_path is exactly what
        // status() recomputes from the project root.
        let index_tree_path = project_root.join(".djinn").join("worktrees").join("_index");
        let pinned_sha = "deadbeefcafebabe1234567890abcdef00000001".to_string();
        {
            let mut cache = GRAPH_CACHE.write().await;
            *cache = Some(CachedGraph {
                graph: build_test_graph(),
                project_path: index_tree_path.clone(),
                git_head: pinned_sha.clone(),
                last_warm_at: time::OffsetDateTime::now_utc(),
            });
        }

        let bridge = RepoGraphBridge::new(state);
        let project_root_str = project_root.to_string_lossy().into_owned();
        let status = bridge.status(&project_root_str).await.expect("status ok");

        // Drain the cache so we don't poison sibling tests in this process.
        {
            let mut cache = GRAPH_CACHE.write().await;
            *cache = None;
        }

        assert_eq!(status.project_id, project.id);
        assert!(status.warmed);
        assert_eq!(status.pinned_commit.as_deref(), Some(pinned_sha.as_str()));
        let ts = status.last_warm_at.expect("last_warm_at populated");
        assert!(
            ts.contains('T') && (ts.ends_with('Z') || ts.contains('+') || ts.contains('-')),
            "expected RFC3339 timestamp, got {ts}"
        );
        // commits_since_pin is best-effort: project_root has no git repo so
        // the rev-list call fails and we expect None.  This still proves the
        // status path does not panic when git is unavailable.
        assert!(status.commits_since_pin.is_none());
    }
}

#[cfg(test)]
mod ensure_canonical_graph_tests {
    use super::*;
    use crate::test_helpers::create_test_db;
    use djinn_db::{ProjectRepository, RepoGraphCacheInsert, RepoGraphCacheRepository};
    use tokio_util::sync::CancellationToken;

    /// Build a tiny on-disk git project with a single commit so
    /// `ensure_canonical_graph` can resolve a HEAD SHA without touching
    /// any remote.
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

    /// Cache-hit path: when `repo_graph_cache` already contains a row for
    /// `(project_id, commit_sha)`, `ensure_canonical_graph` deserializes it
    /// and returns the graph WITHOUT spawning the SCIP indexer.  In tests
    /// the indexer would fail (no rust-analyzer on PATH and no SCIP files
    /// in tempdir), so a successful return is itself the proof.
    #[tokio::test]
    async fn ensure_canonical_graph_serves_cache_hit_without_running_indexer() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = make_project(tmp.path()).await;

        // Build AppState wired to a fresh in-memory DB.
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let state = crate::server::AppState::new(db.clone(), cancel);

        // Register the project so `build_graph_for_project` would resolve
        // it (this test calls `ensure_canonical_graph` directly so the
        // registration is only needed for parity with the prod path).
        let proj_repo = ProjectRepository::new(db.clone(), state.event_bus());
        let project = proj_repo
            .create("test-canonical", project_root.to_string_lossy().as_ref())
            .await
            .expect("create project");

        // Pre-build a tiny graph and stash it in repo_graph_cache for
        // BOTH possible commit SHAs the index tree could end up on
        // (origin/main fetch fails in tests, so the index tree resets to
        // HEAD).  We resolve HEAD before the call so we know which key
        // matters.
        let head_out = tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project_root)
            .output()
            .await
            .unwrap();
        let head_sha = String::from_utf8_lossy(&head_out.stdout).trim().to_string();

        let graph = graph_bridge_tests::build_test_graph();
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

        // Cache-hit path must succeed.  If it ran the indexer it would
        // fail (no SCIP artifacts produced).
        let result = ensure_canonical_graph(&state, &project.id, &project_root).await;
        assert!(result.is_ok(), "expected cache-hit success, got {result:?}");
        let (_handle, returned_graph) = result.unwrap();
        // The deserialized graph should be structurally identical to the
        // fixture (round-trip equality is the contract Chunk B added).
        assert_eq!(returned_graph.node_count(), graph.node_count());
    }

    /// IndexerLock contention: two concurrent `ensure_canonical_graph`
    /// calls against the same project should serialize on the lock and
    /// both succeed via the cache (the second is forced through the
    /// re-check-under-lock path).
    #[tokio::test]
    async fn ensure_canonical_graph_serializes_concurrent_callers() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = make_project(tmp.path()).await;

        let db = create_test_db();
        let cancel = CancellationToken::new();
        let state = crate::server::AppState::new(db.clone(), cancel);
        let proj_repo = ProjectRepository::new(db.clone(), state.event_bus());
        let project = proj_repo
            .create(
                "test-canonical-concurrent",
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

        let graph = graph_bridge_tests::build_test_graph();
        let blob = bincode::serialize(&graph.to_artifact()).expect("serialize");
        RepoGraphCacheRepository::new(db.clone())
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: &head_sha,
                graph_blob: &blob,
            })
            .await
            .expect("seed cache");

        let state_a = state.clone();
        let project_root_a = project_root.clone();
        let project_id_a = project.id.clone();
        let state_b = state.clone();
        let project_root_b = project_root.clone();
        let project_id_b = project.id.clone();
        let (a, b) = tokio::join!(
            tokio::spawn(async move {
                ensure_canonical_graph(&state_a, &project_id_a, &project_root_a).await
            }),
            tokio::spawn(async move {
                ensure_canonical_graph(&state_b, &project_id_b, &project_root_b).await
            }),
        );
        let a = a.expect("task a panicked").expect("a result");
        let b = b.expect("task b panicked").expect("b result");
        assert_eq!(a.1.node_count(), b.1.node_count());
        assert_eq!(a.1.node_count(), graph.node_count());
    }

    /// Stale-blob path: a row whose `graph_blob` is not bincode-decodable
    /// (e.g. left over from the brief Chunk C JSON era) must be treated as
    /// a cache miss.  We seed garbage bytes and assert that
    /// `ensure_canonical_graph` does NOT bubble the deserialize error;
    /// instead it falls through to the indexer path.  In this test
    /// environment the indexer has no SCIP toolchain available, so the
    /// expected outcome is a failure with an indexer-shaped error message
    /// (NOT a "deserialize cached graph" / UTF-8 error from the cache
    /// path).
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

        // Seed an undecodable blob.  Pure ASCII so a UTF-8 path would
        // _not_ trip; the only thing that should reject it is bincode.
        let garbage = b"this is definitely not a bincoded RepoDependencyGraph";
        RepoGraphCacheRepository::new(db.clone())
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: &head_sha,
                graph_blob: garbage,
            })
            .await
            .expect("seed cache");

        // The call must NOT short-circuit with a cache-deserialize error;
        // it must fall through to the indexer.  In tests the indexer has
        // no SCIP toolchain, so we expect either Err(indexer-shaped) or
        // Ok (if the host happens to have rust-analyzer on PATH).  In
        // either case, the failure mode we are guarding against — a hard
        // error mentioning the cache deserialize path — must NOT occur.
        let result = ensure_canonical_graph(&state, &project.id, &project_root).await;
        if let Err(msg) = &result {
            assert!(
                !msg.contains("deserialize cached graph")
                    && !msg.contains("graph_blob is not valid UTF-8"),
                "stale blob bubbled cache-path error instead of falling through: {msg}"
            );
        }
    }
}
