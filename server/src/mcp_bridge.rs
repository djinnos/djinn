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

#[cfg(test)]
use self::graph_neighbors::{
    build_method_metadata, build_related_symbol, classify_edge_category, group_impact_by_file,
    group_neighbors_by_file, kind_label_for_node, read_symbol_content, resolve_node_or_err,
    resolve_node_with_hint,
};
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

#[cfg(test)]
pub(crate) mod graph_bridge_tests {
    use super::*;
    // PR C2: import the inner `ResolveOutcome` (NodeIndex) under the
    // unqualified name so the existing test patterns keep compiling.
    // The bridge crate's `ResolveOutcome` (String) is different — we
    // never use it directly in these tests.
    use crate::mcp_bridge::graph_neighbors::{ResolveOutcome, resolve_node, resolve_node_or_err};
    use djinn_control_plane::bridge::{ComplexityMetrics as WireComplexityMetrics, ImpactEntry};
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
            workspace_slug: "root".to_string(),
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

    fn multi_workspace_graph() -> RepoDependencyGraph {
        use djinn_graph::repo_graph::{
            REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
            RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
        };
        use djinn_graph::scip_parser::ScipVisibility;

        let mk_symbol =
            |symbol: &str, display_name: &str, file_path: &str, workspace: &str| RepoGraphNode {
                id: RepoNodeKey::Symbol(symbol.to_string()),
                kind: RepoGraphNodeKind::Symbol,
                display_name: display_name.to_string(),
                language: Some("rust".to_string()),
                file_path: Some(PathBuf::from(file_path)),
                symbol: Some(symbol.to_string()),
                symbol_kind: Some(ScipSymbolKind::Function),
                is_external: false,
                visibility: Some(ScipVisibility::Public),
                signature: None,
                documentation: vec![],
                signature_parts: None,
                is_test: false,
                complexity: None,
                workspace: Some(workspace.to_string()),
            };
        let mk_edge = |source: usize, target: usize| RepoGraphArtifactEdge {
            source,
            target,
            kind: RepoGraphEdgeKind::SymbolReference,
            weight: 1.0,
            evidence_count: 1,
            confidence: 0.95,
            reason: None,
            step: None,
        };

        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: vec![
                mk_symbol(
                    "scip-rust pkg server/src/entry.rs `entry`().",
                    "server_entry",
                    "server/src/entry.rs",
                    "server",
                ),
                mk_symbol(
                    "scip-rust pkg desktop/src/shared.rs `shared`().",
                    "desktop_shared",
                    "desktop/src/shared.rs",
                    "desktop",
                ),
                mk_symbol(
                    "scip-rust pkg server/src/sink.rs `sink`().",
                    "server_sink",
                    "server/src/sink.rs",
                    "server",
                ),
                mk_symbol(
                    "scip-rust pkg desktop/src/leaf.rs `leaf`().",
                    "desktop_leaf",
                    "desktop/src/leaf.rs",
                    "desktop",
                ),
            ],
            // The first two edges form a server → desktop → server chain.
            // Traversal ops should keep this cross-workspace middle hop when
            // the server workspace scopes only seed resolution.
            edges: vec![mk_edge(0, 1), mk_edge(1, 2), mk_edge(3, 1)],
            symbol_ranges: std::collections::BTreeMap::new(),
            communities: vec![],
            processes: vec![],
        };
        RepoDependencyGraph::from_artifact(&artifact)
    }

    #[test]
    fn workspace_hint_for_nonexistent_slug_returns_available_slugs() {
        let graph = multi_workspace_graph();

        assert_eq!(
            shared::workspace_hint_from_graph(&graph, Some("nonexistent")),
            Some(vec!["desktop".to_string(), "server".to_string()])
        );
        assert_eq!(
            shared::workspace_hint_from_graph(&graph, Some("server")),
            None
        );
        assert_eq!(shared::workspace_hint_from_graph(&graph, Some("")), None);
    }

    #[test]
    fn valid_workspace_restricts_listing_style_graph_ops() {
        let graph = multi_workspace_graph();
        let workspace = shared::active_workspace_prefix(&graph, Some("server"))
            .expect("server workspace should be active");

        let ranked_kept: Vec<String> = graph
            .rank()
            .nodes
            .iter()
            .filter_map(|ranked| {
                let node = graph.node(ranked.node_index);
                shared::repo_graph_node_matches_workspace(node, &workspace)
                    .then(|| node.display_name.clone())
            })
            .collect();
        assert!(ranked_kept.iter().any(|name| name == "server_entry"));
        assert!(ranked_kept.iter().any(|name| name == "server_sink"));
        assert!(
            !ranked_kept.iter().any(|name| name.starts_with("desktop_")),
            "ranked hard filter leaked desktop nodes: {ranked_kept:?}"
        );

        let orphan_kept: Vec<String> = graph
            .orphans(None, None, usize::MAX)
            .into_iter()
            .filter_map(|idx| {
                let node = graph.node(idx);
                shared::repo_graph_node_matches_workspace(node, &workspace)
                    .then(|| node.display_name.clone())
            })
            .collect();
        assert!(orphan_kept.iter().any(|name| name == "server_entry"));
        assert!(
            !orphan_kept.iter().any(|name| name.starts_with("desktop_")),
            "orphans hard filter leaked desktop nodes: {orphan_kept:?}"
        );

        let api_surface_kept: Vec<String> = graph
            .graph()
            .node_indices()
            .filter_map(|idx| {
                let node = graph.node(idx);
                (node.visibility == Some(djinn_graph::scip_parser::ScipVisibility::Public)
                    && shared::repo_graph_node_matches_workspace(node, &workspace))
                .then(|| node.display_name.clone())
            })
            .collect();
        assert_eq!(api_surface_kept.len(), 2);
        assert!(
            api_surface_kept
                .iter()
                .all(|name| name.starts_with("server_"))
        );

        let snapshot_payload = build_snapshot_payload(
            &graph,
            &graph.rank(),
            "project".to_string(),
            "head".to_string(),
            "now".to_string(),
            &djinn_control_plane::tools::graph_exclusions::GraphExclusions::empty(),
            Some("server"),
            SnapshotLevel::Symbol,
            20,
        );
        assert_eq!(snapshot_payload.total_nodes, 2);
        assert!(
            snapshot_payload
                .nodes
                .iter()
                .all(|node| node.workspace.as_deref() == Some("server")),
            "snapshot hard filter leaked non-server nodes: {:?}",
            snapshot_payload.nodes
        );
    }

    #[test]
    fn traversal_ops_scope_seeds_but_do_not_restrict_the_walk() {
        let graph = multi_workspace_graph();
        let entry = "scip-rust pkg server/src/entry.rs `entry`().";
        let shared = "scip-rust pkg desktop/src/shared.rs `shared`().";
        let sink = "scip-rust pkg server/src/sink.rs `sink`().";

        let sink_idx =
            shared::resolve_node_or_err_for_workspace_seed(&graph, sink, Some("server")).unwrap();
        let impact_keys: Vec<String> = shared::impact_bfs(&graph, sink_idx, 3, Some(0.0))
            .into_iter()
            .map(|(_, entry)| entry.key)
            .collect();
        assert!(
            impact_keys
                .iter()
                .any(|key| key.contains("desktop/src/shared.rs")),
            "impact should preserve cross-workspace blast radius after resolving a server seed: {impact_keys:?}"
        );

        let entry_idx =
            shared::resolve_node_or_err_for_workspace_seed(&graph, entry, Some("server")).unwrap();
        let path = graph
            .shortest_path(entry_idx, sink_idx, Some(4))
            .expect("server seeds should connect through desktop shared node");
        let path_keys: Vec<String> = path
            .iter()
            .map(|idx| format_node_key(&graph.node(*idx).id))
            .collect();
        assert!(
            path_keys
                .iter()
                .any(|key| key.contains("desktop/src/shared.rs")),
            "path should not hard-filter the desktop middle hop: {path_keys:?}"
        );

        let shared_idx = resolve_node_or_err(&graph, shared).unwrap();
        let touches_hot_path = path.contains(&shared_idx);
        assert!(
            touches_hot_path,
            "touches_hot_path semantics should allow an unscoped queried desktop symbol on a server seed path: {path_keys:?}"
        );

        assert!(
            shared::resolve_node_or_err_for_workspace_seed(&graph, shared, Some("server")).is_err(),
            "workspace scoping should apply to traversal seeds, even though the walk itself is unscoped"
        );
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
            workspace_slug: "root".to_string(),
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
                assert_eq!(symbol_count, 3, "expected exactly 3 symbol candidates");
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
            complexity: None,
            workspace: None,
        };
        // Both file-path match (User in path) and kind hint ("class")
        // fire. Tiebreaker for Type/Class is 0.05.
        let s = crate::mcp_bridge::graph_neighbors::score_candidate(&node, "User", Some("class"));
        let expected = 0.5 + 0.4 * 1.0 + 0.2 * 1.0 + 0.05;
        assert!(
            (s - expected).abs() < 1e-9,
            "score {s} != expected {expected}"
        );

        // Same node, no kind hint: drop the 0.2 component.
        let s_no_hint = crate::mcp_bridge::graph_neighbors::score_candidate(&node, "User", None);
        let expected_no_hint = 0.5 + 0.4 * 1.0 + 0.05;
        assert!(
            (s_no_hint - expected_no_hint).abs() < 1e-9,
            "score {s_no_hint} != expected {expected_no_hint}"
        );

        // Query that doesn't appear in path: drop the 0.4 component.
        let s_no_path =
            crate::mcp_bridge::graph_neighbors::score_candidate(&node, "Account", Some("class"));
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
                    file_path: node.file_path.as_ref().map(|p| p.display().to_string()),
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
            REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact,
            RepoGraphArtifactEdge, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
            RepoNodeKey,
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
            complexity: None,
            workspace: None,
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
        let mk_edge =
            |source: usize, target: usize, kind: RepoGraphEdgeKind| RepoGraphArtifactEdge {
                source,
                target,
                kind,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.95,
                reason: None,
                step: None,
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

        let result = shared::impact_bfs(&graph, target_idx, 3, Some(0.0));
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
        let strict = shared::impact_bfs(&graph, helper_idx, 3, Some(1.5));
        assert!(
            strict.is_empty(),
            "min_confidence above 1.0 must collapse the frontier to empty"
        );

        // Default (None → 0.85) admits high-confidence FileReference
        // edges (floor 0.85) and Reads/Writes/SymbolReference (0.85+)
        // — fixture's app.rs ↔ helper.rs FileReference at 0.85
        // qualifies, so default-walked result contains the helper
        // file's caller file.
        let with_default = shared::impact_bfs(&graph, helper_idx, 3, None);
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
        let start = resolve_node_or_err(&graph, "scip-rust pkg src/helper.rs `helper`().").unwrap();

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
    fn build_context_fixture() -> (
        djinn_graph::repo_graph::RepoDependencyGraph,
        petgraph::graph::NodeIndex,
        String,
    ) {
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
            outgoing
                .get(&EdgeCategory::Extends)
                .is_none_or(|v| v.is_empty()),
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
            complexity: None,
            workspace: None,
        };

        let any_node = mk_node(None);
        // SymbolReference with non-callable target → References.
        assert_eq!(
            edge_category_for(
                Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)),
                &any_node
            ),
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
            edge_category_for(
                Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)),
                &method_node
            ),
            EdgeCategory::Calls
        );
        // SymbolReference with Constructor target → Calls.
        let ctor_node = mk_node(Some(ScipSymbolKind::Constructor));
        assert_eq!(
            edge_category_for(
                Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)),
                &ctor_node
            ),
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
            edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::DeclaredInFile)), &any_node),
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
            workspace_slug: "root".to_string(),
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
            workspace_slug: "root".to_string(),
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
        assert_eq!(
            meta.return_type.as_deref(),
            Some("Result<Vec<Item>, Error>")
        );
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
            complexity: None,
            workspace: None,
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
            &GraphExclusions::empty(),
            None,
            SnapshotLevel::Symbol,
            2_000,
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
            if matches!(node.kind.as_str(), "file" | "symbol") {
                assert_eq!(
                    node.workspace.as_deref(),
                    Some("root"),
                    "snapshot node {} should carry workspace slug from RepoGraphNode.workspace",
                    node.id
                );
            }
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
            &GraphExclusions::empty(),
            None,
            SnapshotLevel::Symbol,
            cap,
        );
        assert_eq!(payload.node_cap, cap, "node_cap echoed back unchanged");
        assert!(
            payload.truncated,
            "should be truncated when total_nodes={} > cap={}",
            payload.total_nodes, cap
        );
        assert!(
            payload.nodes.len() >= cap,
            "emitted {} nodes, should include at least the initial cap {}",
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
                node_ids.contains(edge.from.as_str()) && node_ids.contains(edge.to.as_str()),
                "truncated snapshot leaked an edge {} → {} into the wire",
                edge.from,
                edge.to
            );
        }
    }

    #[test]
    fn snapshot_payload_rescues_cross_workspace_endpoint_under_cap() {
        use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
        use djinn_graph::repo_graph::{
            REPO_GRAPH_ARTIFACT_VERSION, RankedRepoGraphNode, RepoDependencyGraph,
            RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphEdgeKind, RepoGraphNode,
            RepoGraphNodeKind, RepoGraphRanking, RepoNodeKey,
        };

        let mk_node = |name: &str, workspace: &str| RepoGraphNode {
            id: RepoNodeKey::Symbol(name.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(format!("{workspace}/src/{name}.rs"))),
            symbol: Some(name.to_string()),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some(workspace.to_string()),
        };
        let graph = RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: vec![
                mk_node("a_hot_0", "workspace-a"),
                mk_node("a_hot_1", "workspace-a"),
                mk_node("a_hot_2", "workspace-a"),
                mk_node("a_hot_3", "workspace-a"),
                mk_node("a_hot_4", "workspace-a"),
                mk_node("b_quiet_endpoint", "workspace-b"),
            ],
            edges: vec![RepoGraphArtifactEdge {
                source: 1,
                target: 5,
                kind: RepoGraphEdgeKind::SymbolReference,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.9,
                reason: None,
                step: None,
            }],
            symbol_ranges: std::collections::BTreeMap::new(),
            communities: vec![],
            processes: vec![],
        });
        let ranking = RepoGraphRanking {
            nodes: graph
                .graph()
                .node_indices()
                .enumerate()
                .map(|(rank, node_index)| RankedRepoGraphNode {
                    node_index,
                    key: graph.node(node_index).id.clone(),
                    kind: graph.node(node_index).kind,
                    score: (10 - rank) as f64,
                    page_rank: (10 - rank) as f64,
                    structural_weight: 1.0,
                    inbound_edge_weight: 0.0,
                    outbound_edge_weight: 0.0,
                    is_entry_point: false,
                    entry_point_distance: None,
                    fused_rank: (10 - rank) as f64,
                })
                .collect(),
        };

        let payload = build_snapshot_payload(
            &graph,
            &ranking,
            "proj-test".to_string(),
            "deadbeef".to_string(),
            "2026-04-28T00:00:00Z".to_string(),
            &GraphExclusions::empty(),
            None,
            SnapshotLevel::Symbol,
            2,
        );

        let node_ids: std::collections::HashSet<&str> =
            payload.nodes.iter().map(|node| node.id.as_str()).collect();
        assert!(node_ids.contains("symbol:a_hot_1"));
        assert!(node_ids.contains("symbol:b_quiet_endpoint"));
        assert!(
            payload.edges.iter().any(|edge| {
                edge.from == "symbol:a_hot_1" && edge.to == "symbol:b_quiet_endpoint"
            })
        );
        for edge in &payload.edges {
            assert!(node_ids.contains(edge.from.as_str()));
            assert!(node_ids.contains(edge.to.as_str()));
        }
    }

    #[test]
    fn community_snapshot_aggregates_cross_workspace_edges() {
        use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
        use djinn_graph::communities::Community;
        use djinn_graph::repo_graph::{
            REPO_GRAPH_ARTIFACT_VERSION, RankedRepoGraphNode, RepoDependencyGraph,
            RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphEdgeKind, RepoGraphNode,
            RepoGraphNodeKind, RepoGraphRanking, RepoNodeKey,
        };

        let mk_node = |name: &str, workspace: &str| RepoGraphNode {
            id: RepoNodeKey::Symbol(name.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(format!("{workspace}/src/{name}.rs"))),
            symbol: Some(name.to_string()),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some(workspace.to_string()),
        };

        let graph = RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: vec![
                mk_node("api_entry", "api"),
                mk_node("api_helper", "api"),
                mk_node("web_entry", "web"),
                mk_node("web_helper", "web"),
            ],
            edges: vec![
                RepoGraphArtifactEdge {
                    source: 0,
                    target: 1,
                    kind: RepoGraphEdgeKind::SymbolReference,
                    weight: 1.0,
                    evidence_count: 1,
                    confidence: 0.8,
                    reason: None,
                    step: None,
                },
                RepoGraphArtifactEdge {
                    source: 2,
                    target: 3,
                    kind: RepoGraphEdgeKind::SymbolReference,
                    weight: 1.0,
                    evidence_count: 1,
                    confidence: 0.8,
                    reason: None,
                    step: None,
                },
                RepoGraphArtifactEdge {
                    source: 1,
                    target: 2,
                    kind: RepoGraphEdgeKind::SymbolReference,
                    weight: 1.0,
                    evidence_count: 1,
                    confidence: 0.9,
                    reason: Some("cross-workspace call".to_string()),
                    step: None,
                },
            ],
            symbol_ranges: std::collections::BTreeMap::new(),
            communities: vec![
                Community {
                    id: "community-api".to_string(),
                    label: "api".to_string(),
                    member_ids: vec![0, 1],
                    cohesion: 0.5,
                    symbol_count: 2,
                    keywords: vec!["api".to_string()],
                },
                Community {
                    id: "community-web".to_string(),
                    label: "web".to_string(),
                    member_ids: vec![2, 3],
                    cohesion: 0.5,
                    symbol_count: 2,
                    keywords: vec!["web".to_string()],
                },
            ],
            processes: vec![],
        });
        let ranking = RepoGraphRanking {
            nodes: graph
                .graph()
                .node_indices()
                .enumerate()
                .map(|(rank, node_index)| RankedRepoGraphNode {
                    node_index,
                    key: graph.node(node_index).id.clone(),
                    kind: graph.node(node_index).kind,
                    score: (10 - rank) as f64,
                    page_rank: (10 - rank) as f64,
                    structural_weight: 1.0,
                    inbound_edge_weight: 0.0,
                    outbound_edge_weight: 0.0,
                    is_entry_point: false,
                    entry_point_distance: None,
                    fused_rank: (10 - rank) as f64,
                })
                .collect(),
        };

        let payload = build_snapshot_payload(
            &graph,
            &ranking,
            "proj-test".to_string(),
            "deadbeef".to_string(),
            "2026-04-28T00:00:00Z".to_string(),
            &GraphExclusions::empty(),
            None,
            SnapshotLevel::Community,
            1,
        );

        assert_eq!(payload.nodes.len(), 2);
        let node_ids: std::collections::HashSet<&str> =
            payload.nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(
            node_ids,
            std::collections::HashSet::from(["community-api", "community-web"])
        );
        assert!(payload.nodes.iter().all(|node| node.kind == "community"));
        assert!(payload.nodes.iter().any(|node| {
            node.id == "community-api"
                && node.workspace.as_deref() == Some("api")
                && node.workspace_kind.as_deref() == Some("single")
                && node.member_count == Some(2)
                && node.internal_edge_count == Some(1)
        }));
        assert!(payload.edges.iter().any(|edge| {
            edge.from == "community-api"
                && edge.to == "community-web"
                && edge.kind == "SymbolReference"
        }));
        for edge in &payload.edges {
            assert!(node_ids.contains(edge.from.as_str()));
            assert!(node_ids.contains(edge.to.as_str()));
        }
    }

    #[test]
    fn snapshot_payload_preserves_quiet_workspace_when_cap_allows() {
        use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
        use djinn_graph::repo_graph::{
            REPO_GRAPH_ARTIFACT_VERSION, RankedRepoGraphNode, RepoDependencyGraph,
            RepoGraphArtifact, RepoGraphNode, RepoGraphNodeKind, RepoGraphRanking, RepoNodeKey,
        };

        let mk_node = |name: &str, workspace: &str| RepoGraphNode {
            id: RepoNodeKey::Symbol(name.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(format!("{workspace}/src/{name}.rs"))),
            symbol: Some(name.to_string()),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some(workspace.to_string()),
        };

        let graph = RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: vec![
                mk_node("a_hot_0", "workspace-a"),
                mk_node("a_hot_1", "workspace-a"),
                mk_node("a_hot_2", "workspace-a"),
                mk_node("a_hot_3", "workspace-a"),
                mk_node("b_quiet", "workspace-b"),
            ],
            edges: vec![],
            symbol_ranges: std::collections::BTreeMap::new(),
            communities: vec![],
            processes: vec![],
        });
        let ranking = RepoGraphRanking {
            nodes: graph
                .graph()
                .node_indices()
                .enumerate()
                .map(|(rank, node_index)| RankedRepoGraphNode {
                    node_index,
                    key: graph.node(node_index).id.clone(),
                    kind: graph.node(node_index).kind,
                    score: (10 - rank) as f64,
                    page_rank: (10 - rank) as f64,
                    structural_weight: 1.0,
                    inbound_edge_weight: 0.0,
                    outbound_edge_weight: 0.0,
                    is_entry_point: false,
                    entry_point_distance: None,
                    fused_rank: (10 - rank) as f64,
                })
                .collect(),
        };

        let payload = build_snapshot_payload(
            &graph,
            &ranking,
            "proj-test".to_string(),
            "deadbeef".to_string(),
            "2026-04-28T00:00:00Z".to_string(),
            &GraphExclusions::empty(),
            None,
            SnapshotLevel::Symbol,
            2,
        );

        let workspaces: std::collections::HashSet<&str> = payload
            .nodes
            .iter()
            .filter_map(|node| node.workspace.as_deref())
            .collect();
        assert_eq!(payload.nodes.len(), 2);
        assert!(workspaces.contains("workspace-a"));
        assert!(workspaces.contains("workspace-b"));
        assert!(
            payload.nodes.iter().any(|node| node.id == "symbol:b_quiet"),
            "quiet workspace should retain a representative node: {:?}",
            payload.nodes
        );
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
            complexity: None,
            workspace: None,
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
            &GraphExclusions::empty(),
            None,
            SnapshotLevel::Symbol,
            2_000,
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

    // ── Iter 28: complexity op ranking + aggregation ─────────────────────

    fn complexity_metrics(
        cog: u16,
        cyc: u16,
        nloc: u16,
        nest: u8,
        params: u8,
    ) -> WireComplexityMetrics {
        WireComplexityMetrics {
            cyclomatic: cyc,
            cognitive: cog,
            nloc,
            max_nesting: nest,
            param_count: params,
        }
    }

    fn function_entry(
        key: &str,
        display_name: &str,
        file: &str,
        metrics: WireComplexityMetrics,
    ) -> djinn_control_plane::bridge::FunctionComplexityEntry {
        djinn_control_plane::bridge::FunctionComplexityEntry {
            key: key.to_string(),
            display_name: display_name.to_string(),
            file: file.to_string(),
            start_line: 1,
            end_line: 10,
            metrics,
        }
    }

    #[test]
    fn complexity_sort_functions_by_cognitive_iter28() {
        // Two functions in two files — one with cognitive=10, one with
        // cognitive=1. After sorting by cognitive desc, the high-
        // complexity entry must lead.
        let mut entries = vec![
            function_entry(
                "symbol:a",
                "easy",
                "src/a.rs",
                complexity_metrics(1, 1, 5, 0, 0),
            ),
            function_entry(
                "symbol:b",
                "hard",
                "src/b.rs",
                complexity_metrics(10, 8, 50, 4, 3),
            ),
        ];
        super::refactor::sort_function_complexity_entries(&mut entries, "cognitive");
        assert_eq!(entries[0].display_name, "hard");
        assert_eq!(entries[0].metrics.cognitive, 10);
        assert_eq!(entries[1].display_name, "easy");
        assert_eq!(entries[1].metrics.cognitive, 1);
    }

    #[test]
    fn complexity_sort_functions_by_cyclomatic_iter28() {
        // Verify the non-default sort key actually rotates the ordering.
        // `easy` has higher cognitive but lower cyclomatic.
        let mut entries = vec![
            function_entry(
                "symbol:easy",
                "easy",
                "src/a.rs",
                complexity_metrics(10, 2, 5, 0, 0),
            ),
            function_entry(
                "symbol:hard",
                "hard",
                "src/b.rs",
                complexity_metrics(5, 9, 50, 4, 3),
            ),
        ];
        super::refactor::sort_function_complexity_entries(&mut entries, "cyclomatic");
        assert_eq!(
            entries[0].display_name, "hard",
            "cyclomatic=9 should win over cyclomatic=2"
        );
        assert_eq!(entries[0].metrics.cyclomatic, 9);
    }

    #[test]
    fn complexity_aggregate_files_groups_by_path_iter28() {
        // Two functions in `src/big.rs` (cognitive 7+3) and one in
        // `src/small.rs` (cognitive 2). After aggregation: big.rs has
        // function_count=2, total_cognitive=10, max_function_name="big_fn"
        // (worst offender); small.rs has 1 function. Sorted by total
        // cognitive desc, big.rs leads.
        let entries = vec![
            function_entry(
                "symbol:big1",
                "big_fn",
                "src/big.rs",
                complexity_metrics(7, 5, 30, 2, 2),
            ),
            function_entry(
                "symbol:big2",
                "small_fn",
                "src/big.rs",
                complexity_metrics(3, 2, 12, 1, 1),
            ),
            function_entry(
                "symbol:s1",
                "tiny",
                "src/small.rs",
                complexity_metrics(2, 1, 8, 0, 0),
            ),
        ];
        let files = super::refactor::aggregate_files_complexity(&entries, "cognitive");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file, "src/big.rs");
        assert_eq!(files[0].function_count, 2);
        assert_eq!(files[0].total_cognitive, 10);
        assert_eq!(files[0].total_cyclomatic, 7);
        assert_eq!(files[0].total_nloc, 42);
        assert_eq!(files[0].max_function_cognitive, 7);
        assert_eq!(files[0].max_function_name, "big_fn");
        assert_eq!(files[1].file, "src/small.rs");
        assert_eq!(files[1].function_count, 1);
    }

    #[test]
    fn complexity_aggregate_files_param_count_proxy_iter28() {
        // For `target=files, sort_by=param_count` we proxy to
        // function_count (formal params don't sum meaningfully across
        // functions). A file with one 5-param function ranks BELOW a
        // file with two 1-param functions.
        let entries = vec![
            function_entry(
                "symbol:a",
                "single",
                "src/wide.rs",
                complexity_metrics(1, 1, 5, 0, 5),
            ),
            function_entry(
                "symbol:b",
                "first",
                "src/many.rs",
                complexity_metrics(1, 1, 5, 0, 1),
            ),
            function_entry(
                "symbol:c",
                "second",
                "src/many.rs",
                complexity_metrics(1, 1, 5, 0, 1),
            ),
        ];
        let files = super::refactor::aggregate_files_complexity(&entries, "param_count");
        assert_eq!(
            files[0].file, "src/many.rs",
            "two functions should win by param_count proxy"
        );
        assert_eq!(files[0].function_count, 2);
        assert_eq!(files[1].file, "src/wide.rs");
        assert_eq!(files[1].function_count, 1);
    }

    #[test]
    fn complexity_result_serializes_as_array_iter28() {
        // Serde-untagged invariant: a `Functions` variant serializes as
        // a bare JSON array (the inner Vec). Pinning this so a future
        // refactor that wraps it in a discriminator breaks the test
        // explicitly.
        use djinn_control_plane::bridge::ComplexityResult;
        let entry = function_entry(
            "symbol:x",
            "x",
            "src/x.rs",
            complexity_metrics(1, 1, 1, 0, 0),
        );
        let result = ComplexityResult::Functions(vec![entry]);
        let json = serde_json::to_value(&result).expect("serialize");
        assert!(
            json.is_array(),
            "Functions should serialize as bare array: {json}"
        );
        assert_eq!(json.as_array().unwrap().len(), 1);
    }

    // ── Iter 29: refactor_candidates composite ranking ────────────────────

    fn refactor_input(
        key: &str,
        display_name: &str,
        file: &str,
        cognitive: u16,
        cyclomatic: u16,
        page_rank: f64,
    ) -> super::refactor::RefactorCandidateInput {
        super::refactor::RefactorCandidateInput {
            key: key.to_string(),
            display_name: display_name.to_string(),
            file: file.to_string(),
            start_line: 1,
            end_line: 10,
            cognitive,
            cyclomatic,
            page_rank,
        }
    }

    #[test]
    fn refactor_candidates_composite_ranks_top_function_iter29() {
        // Three functions with monotonically-increasing signals across
        // all three axes. Function B (cognitive=10, churn=20, pr=0.5)
        // tops every signal AND the composite z-score; the ranker
        // must surface it at index 0.
        use std::collections::HashMap;
        let candidates = vec![
            refactor_input("symbol:a", "a", "src/a.rs", 1, 1, 0.1),
            refactor_input("symbol:b", "b", "src/b.rs", 10, 8, 0.5),
            refactor_input("symbol:c", "c", "src/c.rs", 5, 4, 0.2),
        ];
        let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
        churn_map.insert(std::path::PathBuf::from("src/a.rs"), 1);
        churn_map.insert(std::path::PathBuf::from("src/b.rs"), 20);
        churn_map.insert(std::path::PathBuf::from("src/c.rs"), 5);

        let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 30);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0].display_name, "b",
            "B should be the top refactor target"
        );
        assert_eq!(out[0].cognitive, 10);
        assert_eq!(out[0].churn_commits, 20);
        // Score is the mean of three z-scores; with B at the top of
        // every axis the composite must be strictly positive.
        assert!(out[0].composite_score > 0.0, "B composite should be > 0");
    }

    #[test]
    fn refactor_candidates_zero_stddev_returns_zero_z_iter29() {
        // Degenerate small-project shape: every function has the same
        // cognitive / churn / pagerank. Stddev across each axis is 0;
        // the helper must clamp z-scores to 0 (not produce NaN), and
        // the composite score for every entry must be exactly 0.
        // Order is stable on the display_name tiebreaker.
        use std::collections::HashMap;
        let candidates = vec![
            refactor_input("symbol:a", "alpha", "src/x.rs", 5, 3, 0.2),
            refactor_input("symbol:b", "beta", "src/x.rs", 5, 3, 0.2),
            refactor_input("symbol:c", "gamma", "src/x.rs", 5, 3, 0.2),
        ];
        let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
        churn_map.insert(std::path::PathBuf::from("src/x.rs"), 7);

        let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 30);
        assert_eq!(out.len(), 3);
        for entry in &out {
            assert_eq!(
                entry.composite_score, 0.0,
                "composite should be 0: {entry:?}"
            );
            assert_eq!(entry.z_cognitive, 0.0);
            assert_eq!(entry.z_churn, 0.0);
            assert_eq!(entry.z_page_rank, 0.0);
        }
        // Stable order: alphabetical by display_name on the
        // composite-score tie.
        assert_eq!(out[0].display_name, "alpha");
        assert_eq!(out[1].display_name, "beta");
        assert_eq!(out[2].display_name, "gamma");
    }

    #[test]
    fn refactor_candidates_tier_assignment_iter29() {
        // Build 20 candidates with monotonically-increasing cognitive +
        // churn so the composite ranks them in the same order. After
        // sorting:
        //   - 10% × 20 = 2 entries get tier="high"
        //   - 15% × 20 = 3 entries get tier="medium"
        //   - the remaining 15 get tier="low"
        use std::collections::HashMap;
        let mut candidates = Vec::new();
        let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
        for i in 0..20 {
            // Higher i → higher cognitive AND higher churn → higher composite.
            let key = format!("symbol:{i:02}");
            let display = format!("fn_{i:02}");
            let file = format!("src/f{i:02}.rs");
            candidates.push(refactor_input(
                &key,
                &display,
                &file,
                u16::try_from(i + 1).unwrap(),
                1,
                f64::from(i),
            ));
            churn_map.insert(
                std::path::PathBuf::from(&file),
                u32::try_from(i + 1).unwrap(),
            );
        }
        let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 20);
        assert_eq!(out.len(), 20);
        let high_count = out.iter().filter(|c| c.tier == "high").count();
        let medium_count = out.iter().filter(|c| c.tier == "medium").count();
        let low_count = out.iter().filter(|c| c.tier == "low").count();
        assert_eq!(high_count, 2, "10% of 20 = 2 high");
        assert_eq!(medium_count, 3, "15% of 20 = 3 medium");
        assert_eq!(low_count, 15, "rest are low");
        // Top entries are the high tier; bottom entries are low.
        assert_eq!(out[0].tier, "high");
        assert_eq!(out[1].tier, "high");
        assert_eq!(out[2].tier, "medium");
        assert_eq!(out[3].tier, "medium");
        assert_eq!(out[4].tier, "medium");
        assert_eq!(out[5].tier, "low");
        assert_eq!(out[19].tier, "low");
    }

    #[test]
    fn refactor_candidates_small_set_all_high_iter29() {
        // Sets with fewer than 10 candidates collapse to all-high
        // (degenerate small project). The 10/15/75 split needs enough
        // entries for the rounding to be meaningful.
        use std::collections::HashMap;
        let candidates = vec![
            refactor_input("symbol:a", "a", "src/a.rs", 5, 3, 0.2),
            refactor_input("symbol:b", "b", "src/b.rs", 8, 4, 0.3),
        ];
        let churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();

        let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 30);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].tier, "high");
        assert_eq!(out[1].tier, "high");
    }

    #[test]
    fn refactor_candidates_missing_file_in_churn_yields_zero_iter29() {
        // Spec: a function whose file isn't in the churn map gets
        // churn_commits=0 (not skipped). With one function in the map
        // (high churn) and one absent (zero churn), the absent
        // function gets a negative z_churn — correct, "this file
        // changes less than average".
        use std::collections::HashMap;
        let candidates = vec![
            refactor_input("symbol:a", "a", "src/in_map.rs", 5, 3, 0.2),
            refactor_input("symbol:b", "b", "src/missing.rs", 5, 3, 0.2),
        ];
        let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
        churn_map.insert(std::path::PathBuf::from("src/in_map.rs"), 50);

        let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 30);
        assert_eq!(out.len(), 2);
        // The missing-file function inherits churn_commits=0.
        let missing = out.iter().find(|c| c.display_name == "b").unwrap();
        assert_eq!(missing.churn_commits, 0);
        assert!(
            missing.z_churn < 0.0,
            "absent file should have negative z_churn"
        );
        // The in-map function has positive z_churn.
        let present = out.iter().find(|c| c.display_name == "a").unwrap();
        assert_eq!(present.churn_commits, 50);
        assert!(
            present.z_churn > 0.0,
            "high-churn file should have positive z_churn"
        );
    }

    #[test]
    fn refactor_candidates_empty_input_returns_empty_iter29() {
        // No candidates → empty Vec (success, not error). Caller must
        // tolerate empty results without a special-case branch.
        use std::collections::HashMap;
        let out = super::refactor::compute_refactor_candidates(&[], &HashMap::new(), 30);
        assert!(out.is_empty());
    }

    #[test]
    fn refactor_candidates_truncates_to_limit_iter29() {
        // Limit caps the returned set; the surviving entries are the
        // top-`limit` by composite score.
        use std::collections::HashMap;
        let mut candidates = Vec::new();
        let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
        for i in 0..50 {
            let key = format!("symbol:{i:02}");
            let display = format!("fn_{i:02}");
            let file = format!("src/f{i:02}.rs");
            candidates.push(refactor_input(
                &key,
                &display,
                &file,
                u16::try_from(i + 1).unwrap(),
                1,
                f64::from(i),
            ));
            churn_map.insert(
                std::path::PathBuf::from(&file),
                u32::try_from(i + 1).unwrap(),
            );
        }
        let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 5);
        assert_eq!(out.len(), 5);
        // Top entry is the highest-index candidate (largest signals).
        assert_eq!(out[0].display_name, "fn_49");
    }
}
