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

use djinn_agent::actors::coordinator::CoordinatorHandle;
use djinn_agent::actors::slot::SlotPoolHandle;
use djinn_agent::lsp::LspManager;

#[cfg(test)]
use crate::canonical_graph::GRAPH_CACHE_TEST_GUARD;
use crate::canonical_graph::{CachedGraph, GRAPH_CACHE, PREVIOUS_GRAPH_CACHE};
use crate::sync::SyncManager;

// ── Newtype wrappers ───────────────────────────────────────────────────────────

struct CoordinatorBridge(pub CoordinatorHandle);
struct SlotPoolBridge(pub SlotPoolHandle);
struct LspBridge(pub LspManager);
struct SyncBridge(pub SyncManager);

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
        project_path: &str,
        key: &str,
        direction: Option<&str>,
        group_by: Option<&str>,
    ) -> Result<NeighborsResult, String> {
        use petgraph::Direction;
        let graph =
            crate::canonical_graph::build_graph_for_project(&self.state, project_path).await?;
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
        // Read the cached PageRank populated by `ensure_canonical_graph`
        // during warm.  Without this cache, every `ranked` call re-ran a full
        // PageRank pass and hung for 30+ s on real-world graphs even when
        // `code_graph status` reported `warmed: true`.
        let (graph, ranking, _sccs) =
            crate::canonical_graph::build_graph_with_caches_for_project(&self.state, project_path)
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
        project_path: &str,
        symbol: &str,
    ) -> Result<Vec<String>, String> {
        use crate::repo_graph::RepoGraphEdgeKind;
        let graph =
            crate::canonical_graph::build_graph_for_project(&self.state, project_path).await?;
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
        let graph =
            crate::canonical_graph::build_graph_for_project(&self.state, project_path).await?;
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
        let graph =
            crate::canonical_graph::build_graph_for_project(&self.state, project_path).await?;
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
        // Read the cached per-kind SCC sets populated by
        // `ensure_canonical_graph` during warm.  Without this cache, every
        // `cycles` call re-ran `tarjan_scc` over the full graph (or a
        // node-filtered subgraph) and hung for tens of seconds on real-world
        // graphs.  The cache holds three precomputed sets — full / file /
        // symbol — because `kind_filter` filters the graph *before* the SCC
        // search, so a single unfiltered representation cannot reproduce the
        // kind-specific results.  `min_size` is applied at read time against
        // the cached set (which is materialised at `min_size = 2`).
        let (graph, _ranking, sccs) =
            crate::canonical_graph::build_graph_with_caches_for_project(&self.state, project_path)
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
        project_path: &str,
        kind_filter: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OrphanEntry>, String> {
        use crate::repo_graph::RepoGraphNodeKind;
        use crate::scip_parser::ScipVisibility;
        let graph =
            crate::canonical_graph::build_graph_for_project(&self.state, project_path).await?;
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
        let graph =
            crate::canonical_graph::build_graph_for_project(&self.state, project_path).await?;
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
        let graph =
            crate::canonical_graph::build_graph_for_project(&self.state, project_path).await?;
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
        let graph =
            crate::canonical_graph::build_graph_for_project(&self.state, project_path).await?;
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

        let (project_root, index_tree_path) =
            crate::canonical_graph::normalize_graph_query_paths(project_path);
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_id = repo
            .resolve(project_root.to_string_lossy().as_ref())
            .await
            .map_err(|e| format!("resolve project: {e}"))?
            .ok_or_else(|| format!("no project registered for path '{project_path}'"))?;

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
            // Cold cache.  ADR-051 §3 "first consumer demand" — Pulse's
            // very first `code_graph status` call on mount is the signal
            // we want to key off.  Kick a single-flight background warm
            // here (non-blocking) so the next status poll a few seconds
            // later surfaces `warmed: true` and the panels can render.
            //
            // Without this, Pulse would get stuck on the empty state
            // because every other `code_graph` op is gated behind
            // `status.warmed == true` on the frontend — `build_graph_*`
            // (which has the sibling kicker) would never be reached.
            crate::canonical_graph::build_graph_for_project(&self.state, project_path)
                .await
                .err();
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

        let commits_since_pin = crate::canonical_graph::canonical_graph_count_commits_since(
            &project_root,
            &pinned_commit,
        )
        .await;

        Ok(GraphStatus {
            project_id,
            warmed: true,
            last_warm_at: Some(last_warm_at_str),
            pinned_commit: Some(pinned_commit),
            commits_since_pin,
        })
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
pub(crate) mod graph_bridge_tests {
    use super::*;
    use crate::repo_graph::{RepoDependencyGraph, RepoNodeKey};
    use crate::scip_parser::{
        ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
        ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    async fn clear_graph_test_caches() {
        {
            let mut cache = GRAPH_CACHE.write().await;
            *cache = None;
        }
        {
            let mut cache = PREVIOUS_GRAPH_CACHE.write().await;
            *cache = None;
        }
    }

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

    pub(crate) fn build_test_graph() -> RepoDependencyGraph {
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

        let _guard = GRAPH_CACHE_TEST_GUARD.lock().await;
        clear_graph_test_caches().await;

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

        clear_graph_test_caches().await;
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

        let _guard = GRAPH_CACHE_TEST_GUARD.lock().await;
        clear_graph_test_caches().await;

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
            let graph = build_test_graph();
            let (pagerank, sccs) = derive_graph_caches(&graph);
            let mut cache = GRAPH_CACHE.write().await;
            *cache = Some(CachedGraph {
                graph,
                project_path: index_tree_path.clone(),
                git_head: pinned_sha.clone(),
                last_warm_at: time::OffsetDateTime::now_utc(),
                pagerank,
                sccs,
            });
        }

        let bridge = RepoGraphBridge::new(state);
        let project_root_str = project_root.to_string_lossy().into_owned();
        let status = bridge.status(&project_root_str).await.expect("status ok");

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

        clear_graph_test_caches().await;
    }

    #[tokio::test]
    async fn chat_code_graph_ranked_succeeds_via_agent_bridge_from_index_tree_root() {
        use crate::test_helpers::create_test_db;
        use djinn_db::ProjectRepository;
        use tokio_util::sync::CancellationToken;

        let _guard = GRAPH_CACHE_TEST_GUARD.lock().await;
        clear_graph_test_caches().await;

        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("chat-code-graph-repo");
        tokio::fs::create_dir_all(&project_root).await.unwrap();

        let db = create_test_db();
        let cancel = CancellationToken::new();
        let state = crate::server::AppState::new(db.clone(), cancel);
        let proj_repo = ProjectRepository::new(db.clone(), state.event_bus());
        proj_repo
            .create(
                "test-chat-code-graph",
                project_root.to_string_lossy().as_ref(),
            )
            .await
            .expect("create project");

        let index_tree_path = djinn_core::index_tree::index_tree_path(&project_root);
        let graph = build_test_graph();
        let (pagerank, sccs) = derive_graph_caches(&graph);
        {
            let mut cache = GRAPH_CACHE.write().await;
            *cache = Some(CachedGraph {
                graph,
                project_path: index_tree_path.clone(),
                git_head: "deadbeefcafebabe1234567890abcdef00000001".to_string(),
                last_warm_at: time::OffsetDateTime::now_utc(),
                pagerank,
                sccs,
            });
        }

        let mut ctx = state.agent_context();
        ctx.working_root = Some(index_tree_path);
        let result = djinn_agent::chat_tools::dispatch_chat_tool(
            &ctx,
            "code_graph",
            serde_json::json!({
                "operation": "ranked",
                "kind_filter": "file",
                "limit": 10,
            }),
            &project_root,
        )
        .await
        .expect("chat code_graph ranked should succeed through agent bridge");

        let ranked = result
            .as_array()
            .expect("ranked response should be an array");
        assert!(
            !ranked.is_empty(),
            "expected ranked files from fixture graph"
        );
        let keys: Vec<&str> = ranked
            .iter()
            .filter_map(|entry| entry.get("key").and_then(|value| value.as_str()))
            .collect();
        assert!(
            keys.iter()
                .any(|key| *key == "file:src/app.rs" || *key == "src/app.rs"),
            "expected fixture file in ranked output, got {keys:?}"
        );

        let rendered = result.to_string();
        assert!(
            !rendered.contains("code_graph not available in agent bridge"),
            "bridge should use the real RepoGraphOps implementation, got {rendered}"
        );

        clear_graph_test_caches().await;
    }
}
