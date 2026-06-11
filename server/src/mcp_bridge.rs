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
use djinn_control_plane::bridge::{
    CoordinatorOps, GitOps, ProvisionServiceRequest, ProvisionedService, RuntimeOps,
    SemanticQueryEmbedding, SlotPoolOps, SnapshotEdge, SnapshotLevel, SnapshotNode,
    SnapshotPayload,
};
use djinn_git::{GitActorHandle, GitError};

use petgraph::visit::EdgeRef;

mod bridges;
pub(crate) mod graph_neighbors;
mod graph_ops;
pub(crate) mod hybrid_search;
pub(crate) mod refactor;
mod shared;

use self::bridges::{CoordinatorBridge, LspBridge, SlotPoolBridge};
use self::graph_neighbors::format_node_key;

pub(crate) use self::graph_ops::RepoGraphBridge;

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

    async fn apply_user_model_change(&self) {
        AppState::apply_user_model_change(self).await;
    }

    async fn dispatch_verification_test(
        &self,
        test_id: &str,
        project_id: &str,
    ) -> Result<(), String> {
        // The K8s graph warmer owns the one-shot Job dispatcher + project-image
        // resolution; the in-process warmer's default impl errors (no kube).
        self.graph_warmer()
            .await
            .dispatch_verification_test(test_id, project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn provision_backing_service(
        &self,
        req: ProvisionServiceRequest,
    ) -> Result<ProvisionedService, String> {
        let rt_req = djinn_runtime::BackingServiceRequest {
            instance_id: req.instance_id,
            task_run_id: req.task_run_id,
            service_type: req.service_type,
            image: req.image,
            port: req.port,
            env: req.env,
            cpu_request: req.cpu_request,
            memory_request: req.memory_request,
            cpu_limit: req.cpu_limit,
            memory_limit: req.memory_limit,
            conn_template: req.conn_template,
        };
        let conn = self
            .graph_warmer()
            .await
            .provision_backing_service(rt_req)
            .await
            .map_err(|e| e.to_string())?;
        Ok(ProvisionedService {
            pod_name: conn.pod_name,
            service_name: conn.service_name,
            conn_string: conn.conn_string,
        })
    }

    async fn release_backing_service(&self, instance_id: &str) -> Result<(), String> {
        self.graph_warmer()
            .await
            .release_backing_service(instance_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn cleanup_task_branches(&self, task_id: &str) {
        let mirror = self.mirror();
        djinn_agent::task_merge::cleanup_task_branches_post_close(
            task_id,
            self.db(),
            &self.event_bus(),
            Some(mirror.as_ref()),
        )
        .await;
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

    async fn trigger_mirror_refresh(&self, project_id: &str) {
        // Fire-and-forget: a fresh mirror clone + stack detection + image
        // enqueue can take many seconds, and the caller (project_add) wants a
        // snappy response. Errors are logged and swallowed — the periodic
        // mirror-fetch tick retries anything that fails here.
        let state = self.clone();
        let project_id = project_id.to_string();
        tokio::spawn(async move {
            match crate::mirror_fetcher::fetch_project(&state, &project_id).await {
                Ok(true) => {
                    tracing::info!(project_id, "post-add mirror refresh complete")
                }
                Ok(false) => tracing::debug!(
                    project_id,
                    "post-add mirror refresh skipped: project not GitHub-linked yet"
                ),
                Err(err) => tracing::warn!(
                    project_id,
                    error = %err,
                    "post-add mirror refresh failed; periodic tick will retry"
                ),
            }
        });
    }

    async fn enqueue_image_build(&self, image_id: &str) -> Result<(), String> {
        // No controller in dev mode (no kube client) — the badge stays
        // `none` locally, which is correct: nothing builds images locally.
        let Some(controller) = self.image_controller().await else {
            return Ok(());
        };
        let image_repo = djinn_db::ImageRepository::new(self.db().clone());
        let image = image_repo
            .get(image_id)
            .await
            .map_err(|e| format!("get image {image_id}: {e}"))?
            .ok_or_else(|| format!("image not found: {image_id}"))?;
        controller
            .enqueue_image(image_id.to_string(), &image_repo, image)
            .await
            .map_err(|e| e.to_string())
    }

    async fn trigger_graph_warm(&self, project_id: &str) {
        // Fire-and-forget: the warm Job dispatch + watch can take a while and
        // the caller (image assignment) wants a snappy response. The warmer's
        // own freshness gate + single-flight guard make this cheap if nothing
        // changed or the image isn't ready yet.
        let warmer = self.graph_warmer().await;
        let project_id = project_id.to_string();
        tokio::spawn(async move {
            warmer.trigger(&project_id).await;
        });
    }
}

#[async_trait]
impl GitOps for AppState {
    async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        AppState::git_actor(self, path).await
    }
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
        let repo = djinn_db::ProjectRepository::new(self.db().clone(), self.event_bus());
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

/// PR D2: pure helper that builds a `SnapshotPayload` from an already-
/// loaded canonical graph + ranking, applying the project's
/// `graph_excluded_paths` filter and capping the surviving population
/// at `node_cap`. Unscoped multi-workspace snapshots reserve enough of
/// that cap to show each non-empty workspace when possible, then fill the
/// remainder from the global ranking; scoped snapshots remain hard-filtered
/// to the requested workspace.
///
/// Extracted from `RepoGraphBridge::snapshot` so unit tests can exercise
/// the truncation / exclusion / wire-shape logic without spinning up
/// the full bridge (which needs `AppState`, a Dolt connection, and a
/// warmed K8s job).
#[allow(clippy::too_many_arguments)]
fn build_snapshot_payload(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    ranking: &djinn_graph::repo_graph::RepoGraphRanking,
    project_id: String,
    git_head: String,
    generated_at: String,
    exclusions: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    workspace: Option<&str>,
    level: SnapshotLevel,
    node_cap: usize,
) -> SnapshotPayload {
    if level == SnapshotLevel::Community {
        return build_community_snapshot_payload(
            graph,
            ranking,
            project_id,
            git_head,
            generated_at,
            exclusions,
            workspace,
            node_cap,
        );
    }

    use std::collections::{BTreeMap, HashMap, HashSet};

    let workspace_prefix = shared::active_workspace_prefix(graph, workspace);

    // Tally totals against the post-exclusion graph so the
    // truncation decision lines up with what the UI actually sees.
    let mut total_nodes_post_excl: usize = 0;
    let mut eligible_nodes: Vec<petgraph::graph::NodeIndex> = Vec::new();
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
        if let Some(prefix) = workspace_prefix.as_deref()
            && !shared::repo_graph_node_matches_workspace(node, prefix)
        {
            continue;
        }
        total_nodes_post_excl += 1;
        pagerank_lookup.insert(ranked_node.node_index, ranked_node.page_rank);
        // `ranking.nodes` is fused-rank-sorted (PR F4 RRF); preserve that
        // order as the global fill order after any fair workspace seeding.
        eligible_nodes.push(ranked_node.node_index);
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
        if let Some(prefix) = workspace_prefix.as_deref()
            && !shared::repo_graph_node_matches_workspace(node, prefix)
        {
            continue;
        }
        total_nodes_post_excl += 1;
        pagerank_lookup.insert(idx, 0.0);
        eligible_nodes.push(idx);
    }

    let mut surviving: HashSet<petgraph::graph::NodeIndex> = HashSet::new();

    if workspace_prefix.is_none() {
        // Unscoped multi-workspace snapshots must not starve quiet workspaces
        // simply because one workspace dominates global PageRank. When the cap
        // can represent all non-empty workspaces, seed one top-ranked node per
        // workspace in slug order, then use the global ranking for the
        // remaining budget. BTreeMap gives deterministic slug tie-breaking.
        let mut by_workspace: BTreeMap<String, Vec<petgraph::graph::NodeIndex>> = BTreeMap::new();
        for &idx in &eligible_nodes {
            let node = graph.node(idx);
            let slug = node
                .workspace
                .as_deref()
                .and_then(|slug| shared::normalize_workspace_slug(Some(slug)))
                .unwrap_or_else(|| "root".to_string());
            by_workspace.entry(slug).or_default().push(idx);
        }
        if by_workspace.len() > 1 && node_cap >= by_workspace.len() {
            for nodes in by_workspace.values() {
                if surviving.len() >= node_cap {
                    break;
                }
                if let Some(&idx) = nodes.first() {
                    surviving.insert(idx);
                }
            }
        }
    }

    for idx in eligible_nodes.iter().copied() {
        if surviving.len() >= node_cap {
            break;
        }
        surviving.insert(idx);
    }

    // Cross-workspace edges are the edges the workspace zoom UI most needs to
    // see. A plain top-N induced subgraph can select one high-rank endpoint and
    // drop the lower-rank endpoint from a quieter workspace, which makes the
    // edge vanish entirely. After the initial cap, rescue both endpoints of any
    // cross-workspace edge touching the selected set. This intentionally may
    // emit slightly more nodes than `node_cap`; `node_cap` remains the initial
    // top-N budget echoed to callers, while `truncated` still tells them the
    // full post-exclusion graph was larger than the initial cap.
    for edge_ref in graph.graph().edge_references() {
        let source = edge_ref.source();
        let target = edge_ref.target();
        if !pagerank_lookup.contains_key(&source) || !pagerank_lookup.contains_key(&target) {
            continue;
        }
        let source_workspace = graph.node(source).workspace.as_deref();
        let target_workspace = graph.node(target).workspace.as_deref();
        if source_workspace == target_workspace {
            continue;
        }
        if surviving.contains(&source) || surviving.contains(&target) {
            surviving.insert(source);
            surviving.insert(target);
        }
    }

    let truncated = total_nodes_post_excl > node_cap;

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
            let label = djinn_graph::scip_parser::prettify_scip_descriptor(&node.display_name);
            SnapshotNode {
                id: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                label,
                workspace: node.workspace.clone(),
                workspace_kind: None,
                member_count: None,
                internal_edge_count: None,
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
                // Iter 30: per-function cognitive complexity from the
                // tree-sitter walker (iter 26 post-pass). Drives the
                // `/code-graph` heatmap overlay; `None` for non-function
                // nodes and unsupported languages — the UI maps null →
                // muted gray so non-function nodes don't dominate.
                cognitive: node.complexity.map(|c| c.cognitive),
                // v10: canonical test flag (file-path convention OR SCIP
                // Test role), stamped at graph-build time.
                is_test: node.is_test,
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
        if !surviving.contains(&edge_ref.source()) || !surviving.contains(&edge_ref.target()) {
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

#[allow(clippy::too_many_arguments)]
fn build_community_snapshot_payload(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    ranking: &djinn_graph::repo_graph::RepoGraphRanking,
    project_id: String,
    git_head: String,
    generated_at: String,
    exclusions: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    workspace: Option<&str>,
    node_cap: usize,
) -> SnapshotPayload {
    use petgraph::visit::EdgeRef;
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

    struct CommunityAgg {
        id: String,
        label: String,
        members: BTreeSet<petgraph::graph::NodeIndex>,
        workspaces: BTreeSet<String>,
        missing_workspace: bool,
        pagerank_sum: f64,
        internal_edges: usize,
    }

    let workspace_prefix = shared::active_workspace_prefix(graph, workspace);
    let community_meta: HashMap<&str, &djinn_graph::communities::Community> = graph
        .communities()
        .iter()
        .map(|community| (community.id.as_str(), community))
        .collect();
    let mut pagerank_lookup: HashMap<petgraph::graph::NodeIndex, f64> = HashMap::new();
    for ranked_node in &ranking.nodes {
        pagerank_lookup.insert(ranked_node.node_index, ranked_node.page_rank);
    }

    let mut eligible_nodes: HashSet<petgraph::graph::NodeIndex> = HashSet::new();
    let mut communities: BTreeMap<String, CommunityAgg> = BTreeMap::new();
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        let key = format_node_key(&node.id);
        let file_hint = node.file_path.as_ref().map(|p| p.display().to_string());
        if node.is_external || exclusions.excludes(&key, file_hint.as_deref(), &node.display_name) {
            continue;
        }
        if let Some(prefix) = workspace_prefix.as_deref()
            && !shared::repo_graph_node_matches_workspace(node, prefix)
        {
            continue;
        }
        let Some(community_id) = graph.community_id(idx) else {
            continue;
        };

        eligible_nodes.insert(idx);
        let meta = community_meta.get(community_id).copied();
        let agg = communities
            .entry(community_id.to_string())
            .or_insert_with(|| CommunityAgg {
                id: community_id.to_string(),
                label: meta
                    .map(|community| community.label.clone())
                    .unwrap_or_else(|| community_id.to_string()),
                members: BTreeSet::new(),
                workspaces: BTreeSet::new(),
                missing_workspace: false,
                pagerank_sum: 0.0,
                internal_edges: 0,
            });
        agg.members.insert(idx);
        if let Some(workspace) = node.workspace.as_deref() {
            agg.workspaces.insert(workspace.to_string());
        } else {
            agg.missing_workspace = true;
        }
        agg.pagerank_sum += pagerank_lookup.get(&idx).copied().unwrap_or(0.0);
    }

    type CommunityEdgeKey = (String, String, String);
    type CommunityEdgeAgg = (f64, Option<String>, usize);
    let mut edge_aggs: BTreeMap<CommunityEdgeKey, CommunityEdgeAgg> = BTreeMap::new();
    let mut total_inter_community_edges = 0usize;
    for edge_ref in graph.graph().edge_references() {
        let source = edge_ref.source();
        let target = edge_ref.target();
        if !eligible_nodes.contains(&source) || !eligible_nodes.contains(&target) {
            continue;
        }
        let Some(source_community) = graph.community_id(source) else {
            continue;
        };
        let Some(target_community) = graph.community_id(target) else {
            continue;
        };
        if source_community == target_community {
            if let Some(agg) = communities.get_mut(source_community) {
                agg.internal_edges += 1;
            }
            continue;
        }
        total_inter_community_edges += 1;
        let weight = edge_ref.weight();
        let key = (
            source_community.to_string(),
            target_community.to_string(),
            format!("{:?}", weight.kind),
        );
        let entry = edge_aggs.entry(key).or_insert((0.0, None, 0));
        entry.0 += weight.confidence;
        if entry.1.is_none() {
            entry.1 = weight.reason.clone();
        }
        entry.2 += 1;
    }

    let mut snapshot_nodes: Vec<SnapshotNode> = communities
        .into_values()
        .filter(|agg| !agg.members.is_empty())
        .map(|agg| {
            let (workspace, workspace_kind) = match (agg.workspaces.len(), agg.missing_workspace) {
                (1, false) => (agg.workspaces.iter().next().cloned(), "single".to_string()),
                (0, true) => (None, "unknown".to_string()),
                _ => (None, "mixed".to_string()),
            };
            SnapshotNode {
                id: agg.id.clone(),
                kind: "community".to_string(),
                label: agg.label,
                workspace,
                workspace_kind: Some(workspace_kind),
                member_count: Some(agg.members.len()),
                internal_edge_count: Some(agg.internal_edges),
                symbol_kind: None,
                file_path: None,
                pagerank: agg.pagerank_sum,
                community_id: Some(agg.id),
                cognitive: None,
                is_test: false,
            }
        })
        .collect();
    snapshot_nodes.sort_by(|a, b| {
        b.pagerank
            .total_cmp(&a.pagerank)
            .then_with(|| a.id.cmp(&b.id))
    });

    let emitted_ids: HashSet<&str> = snapshot_nodes.iter().map(|node| node.id.as_str()).collect();
    let mut snapshot_edges: Vec<SnapshotEdge> = edge_aggs
        .into_iter()
        .filter_map(|((from, to, kind), (confidence_sum, reason, count))| {
            if !emitted_ids.contains(from.as_str()) || !emitted_ids.contains(to.as_str()) {
                return None;
            }
            Some(SnapshotEdge {
                from,
                to,
                kind,
                confidence: (confidence_sum / count as f64).clamp(0.0, 1.0),
                reason,
            })
        })
        .collect();
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
        truncated: false,
        total_nodes: snapshot_nodes.len(),
        total_edges: total_inter_community_edges,
        node_cap,
        nodes: snapshot_nodes,
        edges: snapshot_edges,
    }
}

#[cfg(test)]
mod detect_changes_helper_tests {
    use super::shared;
    use djinn_control_plane::bridge::PagerankTier;

    #[test]
    fn bucket_pagerank_uses_q33_q67() {
        let thresholds = (0.10, 0.20);
        assert_eq!(
            shared::bucket_pagerank(&thresholds, 0.05),
            PagerankTier::Low
        );
        assert_eq!(
            shared::bucket_pagerank(&thresholds, 0.10),
            PagerankTier::Medium
        );
        assert_eq!(
            shared::bucket_pagerank(&thresholds, 0.15),
            PagerankTier::Medium
        );
        assert_eq!(
            shared::bucket_pagerank(&thresholds, 0.20),
            PagerankTier::High
        );
        assert_eq!(
            shared::bucket_pagerank(&thresholds, 0.99),
            PagerankTier::High
        );
    }

    #[test]
    fn tier_rank_orders_high_first() {
        assert!(shared::tier_rank(PagerankTier::High) < shared::tier_rank(PagerankTier::Medium));
        assert!(shared::tier_rank(PagerankTier::Medium) < shared::tier_rank(PagerankTier::Low));
    }

    #[test]
    fn quartile_thresholds_handles_empty_ranking() {
        let ranking = djinn_graph::repo_graph::RepoGraphRanking { nodes: vec![] };
        assert_eq!(shared::quartile_thresholds(&ranking), (0.0, 0.0));
    }
}

#[cfg(test)]
mod helper_tests {
    use super::shared;

    #[test]
    fn scip_crate_name_extracts_cargo_package() {
        let sym = "scip-rust cargo my-crate 0.1.0 foo/Bar#";
        assert_eq!(shared::scip_crate_name(sym), Some("my-crate"));
    }

    #[test]
    fn scip_crate_name_extracts_go_module() {
        let sym = "scip-go gomod github.com/acme/foo v1 pkg/Thing#";
        assert_eq!(shared::scip_crate_name(sym), Some("github.com/acme/foo"));
    }

    #[test]
    fn scip_crate_name_returns_none_for_short_input() {
        assert_eq!(shared::scip_crate_name(""), None);
        assert_eq!(shared::scip_crate_name("scip-rust"), None);
        assert_eq!(shared::scip_crate_name("scip-rust cargo"), None);
        assert_eq!(shared::scip_crate_name("scip-rust cargo pkg"), None);
    }

    #[test]
    fn scip_crate_name_skips_locals_and_dot_placeholder() {
        // Local symbols have no crate identity.
        assert_eq!(shared::scip_crate_name("local 42"), None);
        // Some SCIP scheme/manager slots use "." when missing — and
        // the package slot does the same. In that case we have no
        // identity to compare against.
        let sym = "scip-rust cargo . 0.1.0 foo/Bar#";
        assert_eq!(shared::scip_crate_name(sym), None);
    }

    #[test]
    fn is_deprecated_text_matches_rust_attribute() {
        assert!(shared::is_deprecated_text(
            Some("#[deprecated] fn foo()"),
            &[]
        ));
        assert!(shared::is_deprecated_text(
            Some(r#"#[deprecated(since = "0.1", note = "use bar")] fn foo()"#),
            &[]
        ));
    }

    #[test]
    fn is_deprecated_text_matches_jsdoc_marker_case_insensitive() {
        let doc = vec!["/**".into(), " * @Deprecated use `bar` instead".into()];
        assert!(shared::is_deprecated_text(None, &doc));
    }

    #[test]
    fn is_deprecated_text_ignores_unrelated_text() {
        let doc = vec!["A documented symbol.".into()];
        assert!(!shared::is_deprecated_text(Some("fn foo()"), &doc));
    }
}
