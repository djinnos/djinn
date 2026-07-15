use std::collections::HashMap;

use async_trait::async_trait;
use djinn_control_plane::bridge::{
    ApiImpactResult, ApiSurfaceEntry, BoundaryRule, BoundaryViolation, CallerRef, ChangeKind,
    ChangedRange, ChurnEntry, ComplexityResult, CoupledPairEntry, CouplingEntry, CouplingHubEntry,
    CrateGraphResponse, CycleGroup, CycleMember, DeadSymbolEntry, DeprecatedHit,
    DetectedChangesResult, DetectedTouchedSymbol, DiffTouchesResult, EdgeCategory, EdgeEntry,
    FlowResult, GraphNeighbor, GraphStatus, GraphWorkspaceEntry, HotPathHit, HotspotEntry,
    ImpactResult, MetricsAtResult, NeighborsResult, OrphanEntry, PathHop, PathResult, ProcessRef,
    ProjectCtx, QuerySubgraphBudget as WireQuerySubgraphBudget,
    QuerySubgraphEdge as WireQuerySubgraphEdge, QuerySubgraphNode as WireQuerySubgraphNode,
    QuerySubgraphRequest, QuerySubgraphResult as WireQuerySubgraphResult,
    QuerySubgraphSeedDebug as WireQuerySubgraphSeedDebug,
    QuerySubgraphTraversalDebug as WireQuerySubgraphTraversalDebug, RankedNode, RefactorCandidate,
    RelatedSymbol, RepoGraphOps, ResolveOutcome, RouteMapResult, SearchHit, ShapeCheckResult,
    SnapshotLevel, SnapshotPayload, SymbolAtHit, SymbolContext, SymbolDescription, SymbolNode,
    TouchedSymbol, WorkspacesResult,
};
use petgraph::visit::EdgeRef;

use super::graph_neighbors::{
    build_method_metadata, build_related_symbol, classify_edge_category, format_node_key,
    group_impact_by_file, group_neighbors_by_file, kind_label_for_node, read_symbol_content,
    resolve_node_or_err, resolve_node_with_hint,
};
use super::{build_snapshot_payload, refactor, shared};
use crate::server::AppState;

mod edges_op;
mod flow;
mod insights;
mod query;
mod query_helpers;
mod routes;
mod snapshot;
#[cfg(test)]
mod tests;

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
    async fn workspaces(&self, ctx: &ProjectCtx) -> Result<WorkspacesResult, String> {
        RepoGraphBridge::workspaces(self, ctx).await
    }

    async fn workspace_node_counts(
        &self,
        ctx: &ProjectCtx,
    ) -> Result<HashMap<String, usize>, String> {
        RepoGraphBridge::workspace_node_counts(self, ctx).await
    }

    async fn workspace_hint(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
    ) -> Result<Option<Vec<String>>, String> {
        RepoGraphBridge::workspace_hint(self, ctx, workspace).await
    }

    async fn neighbors(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        direction: Option<&str>,
        group_by: Option<&str>,
        kind_filter: Option<&str>,
    ) -> Result<NeighborsResult, String> {
        RepoGraphBridge::neighbors(self, ctx, key, direction, group_by, kind_filter).await
    }

    async fn query_subgraph(
        &self,
        ctx: &ProjectCtx,
        req: QuerySubgraphRequest,
    ) -> Result<WireQuerySubgraphResult, String> {
        RepoGraphBridge::query_subgraph(self, ctx, req).await
    }

    async fn flow(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<FlowResult, String> {
        RepoGraphBridge::flow(self, ctx, query, kind_filter, limit).await
    }

    async fn ranked(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        kind_filter: Option<&str>,
        sort_by: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RankedNode>, String> {
        RepoGraphBridge::ranked(self, ctx, workspace, kind_filter, sort_by, limit).await
    }

    async fn implementations(&self, ctx: &ProjectCtx, symbol: &str) -> Result<Vec<String>, String> {
        RepoGraphBridge::implementations(self, ctx, symbol).await
    }

    async fn impact(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        key: &str,
        depth: usize,
        group_by: Option<&str>,
        min_confidence: Option<f64>,
    ) -> Result<ImpactResult, String> {
        RepoGraphBridge::impact(self, ctx, workspace, key, depth, group_by, min_confidence).await
    }

    async fn search(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        RepoGraphBridge::search(self, ctx, query, kind_filter, limit).await
    }

    async fn hybrid_search(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        RepoGraphBridge::hybrid_search(self, ctx, query, kind_filter, limit).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn route_map(
        &self,
        ctx: &ProjectCtx,
        route_id: Option<&str>,
        method: Option<&str>,
        path: Option<&str>,
        path_glob: Option<&str>,
        framework: Option<&str>,
        limit: usize,
    ) -> Result<RouteMapResult, String> {
        RepoGraphBridge::route_map(
            self, ctx, route_id, method, path, path_glob, framework, limit,
        )
        .await
    }

    async fn shape_check(
        &self,
        ctx: &ProjectCtx,
        route_id: Option<&str>,
        method: Option<&str>,
        path: Option<&str>,
        include_optional: bool,
    ) -> Result<ShapeCheckResult, String> {
        RepoGraphBridge::shape_check(self, ctx, route_id, method, path, include_optional).await
    }

    async fn api_impact(
        &self,
        ctx: &ProjectCtx,
        route_id: Option<&str>,
        method: Option<&str>,
        path: Option<&str>,
        min_confidence: f64,
        limit: usize,
    ) -> Result<ApiImpactResult, String> {
        RepoGraphBridge::api_impact(self, ctx, route_id, method, path, min_confidence, limit).await
    }

    async fn cycles(
        &self,
        ctx: &ProjectCtx,
        kind_filter: Option<&str>,
        min_size: usize,
    ) -> Result<Vec<CycleGroup>, String> {
        RepoGraphBridge::cycles(self, ctx, kind_filter, min_size).await
    }

    async fn orphans(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        kind_filter: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OrphanEntry>, String> {
        RepoGraphBridge::orphans(self, ctx, workspace, kind_filter, visibility, limit).await
    }

    async fn path(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        from: &str,
        to: &str,
        max_depth: Option<usize>,
    ) -> Result<Option<PathResult>, String> {
        RepoGraphBridge::path(self, ctx, workspace, from, to, max_depth).await
    }

    async fn edges(
        &self,
        ctx: &ProjectCtx,
        from_glob: &str,
        to_glob: &str,
        edge_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EdgeEntry>, String> {
        RepoGraphBridge::edges(self, ctx, from_glob, to_glob, edge_kind, limit).await
    }

    async fn describe(
        &self,
        ctx: &ProjectCtx,
        key: &str,
    ) -> Result<Option<SymbolDescription>, String> {
        RepoGraphBridge::describe(self, ctx, key).await
    }

    async fn context(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        include_content: bool,
    ) -> Result<Option<SymbolContext>, String> {
        RepoGraphBridge::context(self, ctx, key, include_content).await
    }

    async fn status(&self, ctx: &ProjectCtx) -> Result<GraphStatus, String> {
        RepoGraphBridge::status(self, ctx).await
    }

    async fn snapshot(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        level: SnapshotLevel,
        node_cap: usize,
        exclusions: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    ) -> Result<SnapshotPayload, String> {
        RepoGraphBridge::snapshot(self, ctx, workspace, level, node_cap, exclusions).await
    }

    async fn symbols_at(
        &self,
        ctx: &ProjectCtx,
        file: &str,
        start_line: u32,
        end_line: Option<u32>,
    ) -> Result<Vec<SymbolAtHit>, String> {
        RepoGraphBridge::symbols_at(self, ctx, file, start_line, end_line).await
    }

    async fn diff_touches(
        &self,
        ctx: &ProjectCtx,
        changed_ranges: &[ChangedRange],
    ) -> Result<DiffTouchesResult, String> {
        RepoGraphBridge::diff_touches(self, ctx, changed_ranges).await
    }

    async fn detect_changes(
        &self,
        ctx: &ProjectCtx,
        from_sha: Option<&str>,
        to_sha: Option<&str>,
        changed_files: &[String],
    ) -> Result<DetectedChangesResult, String> {
        RepoGraphBridge::detect_changes(self, ctx, from_sha, to_sha, changed_files).await
    }

    async fn api_surface(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        module_glob: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ApiSurfaceEntry>, String> {
        RepoGraphBridge::api_surface(self, ctx, workspace, module_glob, visibility, limit).await
    }

    async fn boundary_check(
        &self,
        ctx: &ProjectCtx,
        rules: &[BoundaryRule],
        level: &str,
    ) -> Result<Vec<BoundaryViolation>, String> {
        RepoGraphBridge::boundary_check(self, ctx, rules, level).await
    }

    async fn hotspots(
        &self,
        ctx: &ProjectCtx,
        window_days: u32,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HotspotEntry>, String> {
        RepoGraphBridge::hotspots(self, ctx, window_days, file_glob, limit).await
    }

    async fn complexity(
        &self,
        ctx: &ProjectCtx,
        target: &str,
        sort_by: &str,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<ComplexityResult, String> {
        RepoGraphBridge::complexity(self, ctx, target, sort_by, file_glob, limit).await
    }

    async fn refactor_candidates(
        &self,
        ctx: &ProjectCtx,
        since_days: Option<u32>,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RefactorCandidate>, String> {
        RepoGraphBridge::refactor_candidates(self, ctx, since_days, file_glob, limit).await
    }

    async fn metrics_at(&self, ctx: &ProjectCtx) -> Result<MetricsAtResult, String> {
        RepoGraphBridge::metrics_at(self, ctx).await
    }

    async fn dead_symbols(
        &self,
        ctx: &ProjectCtx,
        confidence: &str,
        limit: usize,
    ) -> Result<Vec<DeadSymbolEntry>, String> {
        RepoGraphBridge::dead_symbols(self, ctx, confidence, limit).await
    }

    async fn deprecated_callers(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
    ) -> Result<Vec<DeprecatedHit>, String> {
        RepoGraphBridge::deprecated_callers(self, ctx, limit).await
    }

    async fn touches_hot_path(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        seed_entries: &[String],
        seed_sinks: &[String],
        symbols: &[String],
    ) -> Result<Vec<HotPathHit>, String> {
        RepoGraphBridge::touches_hot_path(self, ctx, workspace, seed_entries, seed_sinks, symbols)
            .await
    }

    async fn coupling(
        &self,
        ctx: &ProjectCtx,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CouplingEntry>, String> {
        RepoGraphBridge::coupling(self, ctx, file_path, limit).await
    }

    async fn churn(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
    ) -> Result<Vec<ChurnEntry>, String> {
        RepoGraphBridge::churn(self, ctx, limit, since_days).await
    }

    async fn coupling_hotspots(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
        max_files_per_commit: usize,
    ) -> Result<Vec<CoupledPairEntry>, String> {
        RepoGraphBridge::coupling_hotspots(self, ctx, limit, since_days, max_files_per_commit).await
    }

    async fn coupling_hubs(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
        max_files_per_commit: usize,
    ) -> Result<Vec<CouplingHubEntry>, String> {
        RepoGraphBridge::coupling_hubs(self, ctx, limit, since_days, max_files_per_commit).await
    }

    async fn resolve(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        kind_hint: Option<&str>,
    ) -> Result<ResolveOutcome, String> {
        RepoGraphBridge::resolve(self, ctx, key, kind_hint).await
    }

    async fn crate_graph(&self, ctx: &ProjectCtx) -> Result<CrateGraphResponse, String> {
        RepoGraphBridge::crate_graph(self, ctx).await
    }
}
