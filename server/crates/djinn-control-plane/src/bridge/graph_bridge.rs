use async_trait::async_trait;

use super::graph_data::*;

#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait RepoGraphOps: Send + Sync {
    /// Enumerate graph workspaces by combining distinct `RepoGraphNode.workspace`
    /// tags with persisted per-workspace freshness metadata. Implementations
    /// should include graph-only and freshness-only workspaces deterministically.
    async fn workspaces(&self, _ctx: &ProjectCtx) -> Result<WorkspacesResult, String> {
        Ok(WorkspacesResult {
            project_id: _ctx.id.clone(),
            workspaces: Vec::new(),
        })
    }

    /// Return node counts grouped by workspace slug from the warmed graph when
    /// a graph artifact is available. Implementations should not warm.
    async fn workspace_node_counts(
        &self,
        _ctx: &ProjectCtx,
    ) -> Result<std::collections::HashMap<String, usize>, String> {
        Ok(std::collections::HashMap::new())
    }

    /// Return workspace slugs that should be suggested for an unknown non-empty
    /// workspace request, or `None` when the request is absent/known or the
    /// project does not expose multiple workspace choices.
    ///
    /// Semantics contract (pb94 epic):
    /// - `None` when the caller did not supply a workspace (omit / `""` after
    ///   normalization) — the operation should run unscoped.
    /// - `None` when the requested slug matches at least one node in the
    ///   graph (i.e. the workspace is real, even if the graph has only one
    ///   workspace — single-workspace graphs are a no-op rather than a
    ///   hard-empty filter).
    /// - `Some(candidates)` when the requested slug is non-empty and matches
    ///   no node — the operation should run unscoped AND surface the
    ///   candidate list so the caller can recover.
    /// - `None` when the project exposes zero or one workspace slugs (a
    ///   "no choice" project never produces a hint).
    async fn workspace_hint(
        &self,
        _ctx: &ProjectCtx,
        _workspace: Option<&str>,
    ) -> Result<Option<Vec<String>>, String> {
        Ok(None)
    }

    /// Neighbors of a file or symbol node (edges in/out). When `group_by` is
    /// `Some("file")`, results are collapsed into per-file rollups.
    ///
    /// `kind_filter` (PR A3) restricts the response to neighbors reached by
    /// edges of a specific kind: `Some("reads")` keeps only `Reads` edges,
    /// `Some("writes")` only `Writes`. `None` keeps every kind (the
    /// pre-PR-A3 behaviour).
    async fn neighbors(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        direction: Option<&str>,
        group_by: Option<&str>,
        kind_filter: Option<&str>,
    ) -> Result<NeighborsResult, String>;

    /// Top-ranked nodes by PageRank + structural weight. `sort_by` can be one
    /// of `pagerank` (default), `in_degree`, `out_degree`, or `total_degree`.
    ///
    /// `workspace` hard-scopes this listing operation: when present, returned
    /// nodes should be bounded to that workspace rather than merely biasing
    /// resolution. Implementations that do not yet support workspace filtering
    /// may accept and ignore it while follow-up behavior lands.
    async fn ranked(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        kind_filter: Option<&str>,
        sort_by: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RankedNode>, String>;

    /// Symbols that implement a given trait/interface symbol.
    async fn implementations(&self, ctx: &ProjectCtx, symbol: &str) -> Result<Vec<String>, String>;

    /// Transitive impact set — nodes that depend on the queried node. When
    /// `group_by` is `Some("file")`, results are collapsed into per-file
    /// rollups.
    ///
    /// `min_confidence` filters the BFS frontier: edges whose
    /// [`djinn_graph::repo_graph::RepoGraphEdge::confidence`] falls below the
    /// threshold are skipped, so weak SCIP signals (e.g. `local`-prefixed
    /// references that took the visibility-heuristic penalty) drop out of the
    /// blast radius. `None` keeps every edge — the pre-PR-A2 behaviour.
    ///
    /// `workspace` scopes only seed resolution for this traversal operation:
    /// the initial `key` should be resolved inside the workspace when present,
    /// but the walk itself must never be constrained so cross-workspace blast
    /// radius remains visible.
    async fn impact(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        key: &str,
        depth: usize,
        group_by: Option<&str>,
        min_confidence: Option<f64>,
    ) -> Result<ImpactResult, String>;

    /// Name-based symbol search.
    async fn search(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String>;

    /// PR B4: hybrid lexical + semantic + structural search via RRF
    /// fusion (k=60). The bridge implementation orchestrates the three
    /// signals (lexical = SQL `LIKE` over `code_chunks.embedded_text`,
    /// semantic = Qdrant cosine over the `code_chunks` collection,
    /// structural = `search_by_name` against the canonical graph),
    /// caps each signal at top-3 chunks per file, fuses the resulting
    /// rankings, and stamps each hit's `match_kind` for debug surfaces.
    ///
    /// Default impl falls back to [`Self::search`] so test stubs that
    /// only care about the structural signal don't have to plumb the
    /// hybrid pipeline. Production wires this on the server side via
    /// `RepoGraphBridge` (`server/src/mcp_bridge.rs`).
    async fn hybrid_search(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        // Default: degrade to the structural-only path. This keeps the
        // trait surface backwards-compatible for stubs while letting
        // production override with the full RRF orchestrator.
        let mut hits = self.search(ctx, query, kind_filter, limit).await?;
        for hit in hits.iter_mut() {
            hit.match_kind = Some("structural".to_string());
        }
        Ok(hits)
    }

    /// Budgeted natural-language subgraph query. Implementations map this
    /// bridge DTO to the graph-layer planner params and return the bounded
    /// subgraph plus seed/budget/traversal debug metadata.
    async fn query_subgraph(
        &self,
        _ctx: &ProjectCtx,
        req: QuerySubgraphRequest,
    ) -> Result<QuerySubgraphResult, String> {
        Ok(QuerySubgraphResult {
            query: req.query,
            nodes: Vec::new(),
            edges: Vec::new(),
            seeds: Vec::new(),
            inferred_edge_kinds: Vec::new(),
            budget: QuerySubgraphBudget {
                requested_tokens: 0,
                estimated_tokens: 0,
                truncated: false,
                omitted_nodes: 0,
                omitted_edges: 0,
            },
            traversal: QuerySubgraphTraversalDebug {
                max_depth: 0,
                hub_degree_threshold: 0,
                hubs_blocked: Vec::new(),
                skipped_edge_kinds: Vec::new(),
            },
            narrowing_hints: Vec::new(),
        })
    }

    /// Route graph surface stub. Follow-up route extraction tasks will resolve
    /// route nodes and walk HandlesRoute/Fetches/EntryPointOf edges; until then
    /// an empty route set with a zero summary is the production-safe success
    /// shape for graphs without route/process data.
    async fn route_map(
        &self,
        _ctx: &ProjectCtx,
        _route_id: Option<&str>,
        _method: Option<&str>,
        _path: Option<&str>,
        _path_glob: Option<&str>,
        _framework: Option<&str>,
        _limit: usize,
    ) -> Result<RouteMapResult, String> {
        Ok(RouteMapResult::default())
    }

    /// Route response-shape drift surface stub. Implementation tasks will
    /// populate route shape keys and consumer drift; empty graphs return an
    /// empty shape/drift result rather than a not-found error.
    async fn shape_check(
        &self,
        _ctx: &ProjectCtx,
        _route_id: Option<&str>,
        _method: Option<&str>,
        _path: Option<&str>,
        _include_optional: bool,
    ) -> Result<ShapeCheckResult, String> {
        Ok(ShapeCheckResult::default())
    }

    /// Route API-impact surface stub. Follow-up work will combine impact and
    /// shape-check scoring; until route data exists, return no impacted
    /// consumers.
    async fn api_impact(
        &self,
        _ctx: &ProjectCtx,
        _route_id: Option<&str>,
        _method: Option<&str>,
        _path: Option<&str>,
        _min_confidence: f64,
        _limit: usize,
    ) -> Result<ApiImpactResult, String> {
        Ok(ApiImpactResult::default())
    }

    /// Execution-flow search surface stub. The implementation task will reuse
    /// hybrid_search + process memberships; graphs without process data return
    /// an empty hit list.
    async fn flow(
        &self,
        _ctx: &ProjectCtx,
        _query: &str,
        _kind_filter: Option<&str>,
        _limit: usize,
    ) -> Result<FlowResult, String> {
        Ok(FlowResult::default())
    }

    /// Strongly-connected components of size >= `min_size`.
    async fn cycles(
        &self,
        ctx: &ProjectCtx,
        kind_filter: Option<&str>,
        min_size: usize,
    ) -> Result<Vec<CycleGroup>, String>;

    /// Bulk dead-symbol enumeration (nodes with zero incoming references).
    ///
    /// `workspace` hard-scopes this listing operation: when present, returned
    /// orphan candidates should be bounded to that workspace.
    async fn orphans(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        kind_filter: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OrphanEntry>, String>;

    /// Shortest dependency path between two nodes.
    ///
    /// `workspace` scopes only endpoint resolution for this traversal operation:
    /// `from` and `to` should be resolved inside the workspace when present,
    /// but the shortest-path walk itself must not be constrained.
    async fn path(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        from: &str,
        to: &str,
        max_depth: Option<usize>,
    ) -> Result<Option<PathResult>, String>;

    /// Enumerate edges matching path globs.
    async fn edges(
        &self,
        ctx: &ProjectCtx,
        from_glob: &str,
        to_glob: &str,
        edge_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EdgeEntry>, String>;

    /// Detailed description of a single symbol.
    async fn describe(
        &self,
        ctx: &ProjectCtx,
        key: &str,
    ) -> Result<Option<SymbolDescription>, String>;

    /// PR C1: 360° view of a symbol — resolved node identity plus
    /// categorized incoming/outgoing neighbors. Each category list is
    /// hard-capped at 30 entries server-side. When `include_content` is
    /// `true`, [`SymbolNode::content`] is populated with the symbol's
    /// body text (best-effort: requires the file to be readable from the
    /// project clone). The `processes` list is empty until F2 backfills
    /// process membership.
    async fn context(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        include_content: bool,
    ) -> Result<Option<SymbolContext>, String>;

    /// Peek at the in-memory canonical graph cache for the given project.
    /// MUST NOT trigger any warming or SCIP indexing.  When the cache is
    /// empty for this project, returns `warmed: false` with the timestamp/
    /// commit fields set to `None`.
    async fn status(&self, ctx: &ProjectCtx) -> Result<GraphStatus, String>;

    /// PR D2: full-graph snapshot capped by PageRank tier — the wire
    /// payload that drives the `/code-graph` UI's Sigma render. The
    /// caller passes a `node_cap` (default 2000); we keep the top
    /// `node_cap` nodes by PageRank, then emit every edge whose source
    /// AND target survived the cap. `excluded_keys` is the pre-resolved
    /// set of node keys filtered out by `graph_excluded_paths`; both
    /// node and edge filtering happens against this set so the wire
    /// shape is consistent with what the rest of `code_graph` returns.
    ///
    /// `workspace` hard-scopes this bounded/listing operation: when present,
    /// snapshot nodes and the retained induced edges should be bounded to that
    /// workspace.
    async fn snapshot(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        level: SnapshotLevel,
        node_cap: usize,
        exclusions: &crate::tools::graph_exclusions::GraphExclusions,
    ) -> Result<SnapshotPayload, String>;

    /// Resolve a `(file, start_line, end_line?)` tuple into the set of
    /// base-graph symbols whose definition range encloses the queried
    /// lines. Used for diff-hunk → symbol mapping during PR review.
    async fn symbols_at(
        &self,
        ctx: &ProjectCtx,
        file: &str,
        start_line: u32,
        end_line: Option<u32>,
    ) -> Result<Vec<SymbolAtHit>, String>;

    /// Map a list of changed line ranges (parsed from
    /// `git diff --unified=0 base..head`) to the set of base-graph
    /// symbols they touch, with fan-in/fan-out and file grouping.
    ///
    /// Runs entirely against the already-warmed canonical graph on the
    /// project's base branch — it does NOT build a head graph.
    async fn diff_touches(
        &self,
        ctx: &ProjectCtx,
        changed_ranges: &[ChangedRange],
    ) -> Result<DiffTouchesResult, String>;

    /// Given a SHA range (`from_sha..to_sha`) or an explicit
    /// `changed_files` list, return every symbol whose enclosing range
    /// overlaps a hunk, bucketed by current-project PageRank tier.
    ///
    /// SHA-range mode runs `git diff --unified=0 from..to` against the
    /// project clone and pipes the hunks through
    /// `RepoDependencyGraph::symbols_enclosing`. The `changed_files`
    /// fallback considers every symbol in the listed files as touched
    /// (no line-level filtering). Both modes can be combined; line-level
    /// wins.
    ///
    /// PageRank tiers are quartile-bucketed against the current
    /// project graph at request time, NOT a graph rebuilt at the from
    /// or to sha — review weight reflects "what matters now."
    async fn detect_changes(
        &self,
        ctx: &ProjectCtx,
        from_sha: Option<&str>,
        to_sha: Option<&str>,
        changed_files: &[String],
    ) -> Result<DetectedChangesResult, String>;

    /// List every public (or private/any, per `visibility`) symbol in
    /// the base graph, enriched with fan-in / fan-out and a
    /// "used outside crate" signal.
    ///
    /// `workspace` hard-scopes this listing operation: when present, returned
    /// API-surface symbols should be bounded to that workspace.
    async fn api_surface(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        module_glob: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ApiSurfaceEntry>, String>;

    /// Match edges whose source matches `from_glob` AND target matches
    /// `to_glob`, returning the forbidden ones.
    async fn boundary_check(
        &self,
        ctx: &ProjectCtx,
        rules: &[BoundaryRule],
    ) -> Result<Vec<BoundaryViolation>, String>;

    /// Churn × centrality ranking over files in the project.
    async fn hotspots(
        &self,
        ctx: &ProjectCtx,
        window_days: u32,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HotspotEntry>, String>;

    /// Iter 28: rank function-like symbols (Function/Method/Constructor)
    /// or files by complexity. Reads the per-node `complexity` metrics
    /// the SCIP build pipeline (iter 26) attaches via the tree-sitter
    /// walker; nodes without metrics (non-function, unsupported language,
    /// external symbols) are skipped.
    ///
    /// `target`: `"functions"` | `"files"`. `sort_by`: `"cognitive"` (default)
    /// `| "cyclomatic" | "nloc" | "max_nesting" | "param_count"`. For
    /// `target=files`, the file-level analog of each `sort_by` field is
    /// used (`max_nesting` → `max_function_cognitive`, `param_count` →
    /// `function_count`).
    async fn complexity(
        &self,
        ctx: &ProjectCtx,
        target: &str,
        sort_by: &str,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<ComplexityResult, String>;

    /// Iter 29: composite refactor-priority ranking. Walks every
    /// function-like canonical-graph node carrying complexity metrics,
    /// joins on file-level churn (over the `since_days` window) and
    /// PageRank (from the canonical ranking), and returns a sorted top
    /// `limit` ranked by the mean of the three z-scores.
    ///
    /// `since_days` defaults to 90 and is clamped to `[1, 365]` server-
    /// side. `file_glob` filters the candidate set the same way the
    /// `complexity` op's glob does. Empty result is the success shape
    /// for projects with no function-like nodes carrying complexity
    /// (not an error).
    async fn refactor_candidates(
        &self,
        ctx: &ProjectCtx,
        since_days: Option<u32>,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RefactorCandidate>, String>;

    /// Scalar graph snapshot of the currently-pinned canonical graph.
    async fn metrics_at(&self, ctx: &ProjectCtx) -> Result<MetricsAtResult, String>;

    /// Symbols with zero incoming edges from the entry-point set
    /// (main + tests + crate-root re-exports), tiered by caller
    /// confidence.
    async fn dead_symbols(
        &self,
        ctx: &ProjectCtx,
        confidence: &str,
        limit: usize,
    ) -> Result<Vec<DeadSymbolEntry>, String>;

    /// Scan symbols whose `documentation` or `signature` contains a
    /// `#[deprecated]` / `@deprecated` marker, and return their callers.
    async fn deprecated_callers(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
    ) -> Result<Vec<DeprecatedHit>, String>;

    /// Given entry-point and sink keys (plus queried symbols), return
    /// which queried symbols sit on any shortest path from any entry
    /// to any sink.
    ///
    /// `workspace` scopes only seed/entry/sink resolution for this traversal
    /// operation: seed entries, seed sinks, and queried symbols may be resolved
    /// within the workspace, but shortest-path walks must remain unconstrained.
    async fn touches_hot_path(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        seed_entries: &[String],
        seed_sinks: &[String],
        symbols: &[String],
    ) -> Result<Vec<HotPathHit>, String>;

    /// Files most frequently co-edited with `file_path`, derived from
    /// the commit-based coupling index (see
    /// `djinn_graph::coupling_index`). Does not consult the SCIP graph.
    async fn coupling(
        &self,
        ctx: &ProjectCtx,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CouplingEntry>, String>;

    /// Top files by distinct-commit count over the optional window,
    /// pulling from the coupling index. `since_days` maps to a UTC
    /// lower bound on `committed_at`; omit for all-time churn.
    async fn churn(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
    ) -> Result<Vec<ChurnEntry>, String>;

    /// Top file *pairs* by co-edit count, project-wide. `since_days`
    /// and `max_files_per_commit` mirror the coupling-index knobs (see
    /// `djinn_db::CommitFileChangeRepository::top_coupled_pairs`).
    async fn coupling_hotspots(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
        max_files_per_commit: usize,
    ) -> Result<Vec<CoupledPairEntry>, String>;

    /// Top files by cumulative coupling across all partners (sum of
    /// `co_edits` over every pair the file participates in). Useful
    /// for change-propagation risk mapping.
    async fn coupling_hubs(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
        max_files_per_commit: usize,
    ) -> Result<Vec<CouplingHubEntry>, String>;

    /// Pre-resolve a caller-supplied `key` (file path, SCIP symbol
    /// string, or short identifier) into either a single canonical node
    /// (`Found`), a ranked candidate list (`Ambiguous`), or a hard miss
    /// (`NotFound`). Powers the PR C2 ambiguity response — the
    /// `code_graph` dispatcher and the chat tool both call this before
    /// the heavier op so they can surface a candidate list instead of a
    /// generic `not found` error string.
    ///
    /// `kind_hint` (e.g. `"class"`, `"function"`) feeds into the score
    /// formula and lets the caller bias the disambiguation list.
    async fn resolve(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        kind_hint: Option<&str>,
    ) -> Result<ResolveOutcome, String>;

    /// Crate-level dependency graph: nodes are workspace crates, edges
    /// are aggregated cross-crate references (sum of file/symbol edges
    /// that cross a crate boundary), weighted, with per-crate rollups
    /// (LOC / node count / fan-in / fan-out / inbound vs outbound edge
    /// weight).
    ///
    /// Default returns an empty graph so implementations that don't yet
    /// support crate aggregation compile without change.
    async fn crate_graph(&self, _ctx: &ProjectCtx) -> Result<CrateGraphResponse, String> {
        Ok(CrateGraphResponse {
            crates: Vec::new(),
            edges: Vec::new(),
            message: Some("crate_graph not yet implemented".to_string()),
        })
    }
}
