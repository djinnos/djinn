//! `code_graph` tool handlers for querying the repository dependency graph.
//!
//! All graph queries are dispatched through the [`RepoGraphOps`] bridge trait,
//! keeping the MCP layer free of petgraph/SCIP dependencies.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bridge::{
    ApiSurfaceEntry, BoundaryRule, BoundaryViolation, ChangedRange, CycleGroup, DeadSymbolEntry,
    DeprecatedHit, EdgeEntry, FileGroupEntry, GraphNeighbor, GraphStatus, HotPathHit, HotspotEntry,
    ImpactEntry, ImpactResult, MetricsAtResult, NeighborsResult, OrphanEntry, PathResult,
    ProjectCtx, RankedNode, SearchHit, SymbolAtHit, SymbolDescription, TouchedSymbol,
};
use crate::server::DjinnMcpServer;
use crate::tools::graph_exclusions::GraphExclusions;
use crate::tools::task_tools::{ErrorOr, ErrorResponse};
use djinn_db::ProjectRepository;

// ── Request types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CodeGraphParams {
    /// The operation to perform.
    /// One of: `neighbors`, `ranked`, `impact`, `implementations`,
    /// `search`, `cycles`, `orphans`, `path`, `edges`, `symbols_at`,
    /// `diff_touches`, `describe`, `status`.
    pub operation: String,
    /// Project identifier — either the UUID (`project_id`) or the
    /// canonical `"owner/repo"` slug. The handler resolves it to the
    /// server-managed clone path via `djinn_core::paths::project_dir`
    /// before dispatching to the graph backend.
    pub project: String,
    /// Resolved absolute filesystem path. Populated by the `code_graph`
    /// dispatch after it resolves `project`; the inner operation handlers
    /// read this when they need to call into the graph backend.
    #[serde(skip, default)]
    pub project_path: String,
    /// Resolved project UUID. Populated by the dispatch alongside
    /// `project_path`; inner handlers read it for config lookups.
    #[serde(skip, default)]
    pub project_id: String,
    /// The node key to query (file path or SCIP symbol string).
    /// Required for `neighbors`, `impact`, `implementations`, and `describe`.
    #[serde(default)]
    pub key: Option<String>,
    /// Edge direction filter for `neighbors`: `incoming`, `outgoing`, or omit for both.
    #[serde(default)]
    pub direction: Option<String>,
    /// Node kind filter for `ranked`/`search`/`cycles`/`orphans`: `file` or `symbol`.
    #[serde(default)]
    pub kind_filter: Option<String>,
    /// Maximum results for `ranked`/`search`/`orphans`/`edges`/`neighbors`
    /// (default 20) or max traversal depth for `impact` (default 3).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Substring query for `search`.
    #[serde(default)]
    pub query: Option<String>,
    /// Source node for `path`.
    #[serde(default)]
    pub from: Option<String>,
    /// Destination node for `path`.
    #[serde(default)]
    pub to: Option<String>,
    /// Source path glob for `edges`.
    #[serde(default)]
    pub from_glob: Option<String>,
    /// Destination path glob for `edges`.
    #[serde(default)]
    pub to_glob: Option<String>,
    /// Minimum SCC size for `cycles` (default 2).
    #[serde(default)]
    pub min_size: Option<i64>,
    /// Visibility filter for `orphans`: `public`, `private`, or `any` (default).
    #[serde(default)]
    pub visibility: Option<String>,
    /// Sort key for `ranked`: `pagerank` (default), `in_degree`, `out_degree`,
    /// or `total_degree`.
    #[serde(default)]
    pub sort_by: Option<String>,
    /// Group results: only `file` is supported. Applies to `impact`/`neighbors`.
    #[serde(default)]
    pub group_by: Option<String>,
    /// Optional max depth for `path`.
    #[serde(default)]
    pub max_depth: Option<i64>,
    /// Optional edge-kind filter for `edges`.
    #[serde(default)]
    pub edge_kind: Option<String>,
    /// Repository-relative file path for `symbols_at`.
    #[serde(default)]
    pub file: Option<String>,
    /// 1-indexed inclusive start line for `symbols_at`.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// 1-indexed inclusive end line for `symbols_at`. Defaults to
    /// `start_line` when omitted.
    #[serde(default)]
    pub end_line: Option<u32>,
    /// List of `(file, start_line, end_line?)` hunks for `diff_touches`.
    #[serde(default)]
    pub changed_ranges: Option<Vec<ChangedRange>>,
    /// Optional module-path glob for `api_surface` (filter symbols by
    /// `file_path`).
    #[serde(default)]
    pub module_glob: Option<String>,
    /// Confidence tier for `dead_symbols`: `high`, `med`, or `low`.
    /// Default `high`.
    #[serde(default)]
    pub confidence: Option<String>,
    /// Churn look-back window in days for `hotspots` (default 90, clamped
    /// to 365).
    #[serde(default)]
    pub window_days: Option<u32>,
    /// Optional file glob restricting `hotspots` to a subset of paths.
    #[serde(default)]
    pub file_glob: Option<String>,
    /// Boundary rules for `boundary_check`.
    #[serde(default)]
    pub rules: Option<Vec<BoundaryRule>>,
    /// Entry-point symbol keys (route handlers, `main`, etc.) for
    /// `touches_hot_path`.
    #[serde(default)]
    pub seed_entries: Option<Vec<String>>,
    /// Sink symbol keys (DB queries, external APIs, etc.) for
    /// `touches_hot_path`.
    #[serde(default)]
    pub seed_sinks: Option<Vec<String>>,
    /// Queried symbol keys for `touches_hot_path` — which sit on any
    /// entry→sink shortest path?
    #[serde(default)]
    pub symbols: Option<Vec<String>>,
}

// ── Response types ──────────────────────────────────────────────────────────────

// NOTE: previously `result: NeighborsResult` was `#[serde(flatten)]`, but
// `NeighborsResult` is an untagged enum of `Vec<_>` variants — serde's flatten
// adapter only accepts map-like types, so serialization failed at runtime with
// "can only flatten structs and maps (got a sequence)". We now emit the list
// under a named field that matches the desktop client parsers (`neighbors` for
// the detailed shape, `file_groups` for the `group_by=file` rollup).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NeighborsResponse {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neighbors: Option<Vec<GraphNeighbor>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_groups: Option<Vec<FileGroupEntry>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RankedResponse {
    pub nodes: Vec<RankedNode>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImplementationsResponse {
    pub symbol: String,
    pub implementations: Vec<String>,
}

// See NeighborsResponse above — same flatten-on-sequence bug. Impact emits
// its detailed list under `impact` and its file rollup under `file_groups`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImpactResponse {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<Vec<ImpactEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_groups: Option<Vec<FileGroupEntry>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResponse {
    pub query: String,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CyclesResponse {
    pub cycles: Vec<CycleGroup>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OrphansResponse {
    pub orphans: Vec<OrphanEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PathResponse {
    pub path: Option<PathResult>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EdgesResponse {
    pub edges: Vec<EdgeEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DescribeResponse {
    pub description: Option<SymbolDescription>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusResponse {
    #[serde(flatten)]
    pub status: GraphStatus,
}

/// Response for the `symbols_at` op — the queried file and every symbol
/// hit whose definition range encloses the requested line window.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolsAtResponse {
    pub file: String,
    pub hits: Vec<SymbolAtHit>,
}

/// Response for the `diff_touches` op — touched-symbol rollup plus the
/// affected/unknown-file partition.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiffTouchesResponse {
    pub touched_symbols: Vec<TouchedSymbol>,
    pub affected_files: Vec<String>,
    pub unknown_files: Vec<String>,
}

/// Response for the `api_surface` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiSurfaceResponse {
    pub symbols: Vec<ApiSurfaceEntry>,
}

/// Response for the `boundary_check` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BoundaryCheckResponse {
    pub violations: Vec<BoundaryViolation>,
}

/// Response for the `hotspots` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HotspotsResponse {
    pub hotspots: Vec<HotspotEntry>,
}

/// Response for the `metrics_at` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MetricsAtResponse {
    #[serde(flatten)]
    pub metrics: MetricsAtResult,
}

/// Response for the `dead_symbols` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeadSymbolsResponse {
    pub symbols: Vec<DeadSymbolEntry>,
}

/// Response for the `deprecated_callers` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeprecatedCallersResponse {
    pub hits: Vec<DeprecatedHit>,
}

/// Response for the `touches_hot_path` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TouchesHotPathResponse {
    pub hits: Vec<HotPathHit>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum CodeGraphResponse {
    Neighbors(NeighborsResponse),
    Ranked(RankedResponse),
    Implementations(ImplementationsResponse),
    Impact(ImpactResponse),
    Search(SearchResponse),
    Cycles(CyclesResponse),
    Orphans(OrphansResponse),
    Path(PathResponse),
    Edges(EdgesResponse),
    Describe(DescribeResponse),
    Status(StatusResponse),
    SymbolsAt(SymbolsAtResponse),
    DiffTouches(DiffTouchesResponse),
    ApiSurface(ApiSurfaceResponse),
    BoundaryCheck(BoundaryCheckResponse),
    Hotspots(HotspotsResponse),
    MetricsAt(MetricsAtResponse),
    DeadSymbols(DeadSymbolsResponse),
    DeprecatedCallers(DeprecatedCallersResponse),
    TouchesHotPath(TouchesHotPathResponse),
}

// ── Validation ──────────────────────────────────────────────────────────────────

fn validate_direction(direction: Option<&str>) -> Result<(), String> {
    if let Some(d) = direction {
        match d {
            "incoming" | "outgoing" => Ok(()),
            _ => Err(format!(
                "invalid direction '{d}': expected 'incoming' or 'outgoing'"
            )),
        }
    } else {
        Ok(())
    }
}

fn validate_kind_filter(kind_filter: Option<&str>) -> Result<(), String> {
    if let Some(k) = kind_filter {
        match k {
            "file" | "symbol" => Ok(()),
            _ => Err(format!(
                "invalid kind_filter '{k}': expected 'file' or 'symbol'"
            )),
        }
    } else {
        Ok(())
    }
}

fn require_key(params: &CodeGraphParams) -> Result<&str, String> {
    params
        .key
        .as_deref()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| format!("'key' is required for operation '{}'", params.operation))
}

fn require_query(params: &CodeGraphParams) -> Result<&str, String> {
    params
        .query
        .as_deref()
        .filter(|q| !q.is_empty())
        .ok_or_else(|| format!("'query' is required for operation '{}'", params.operation))
}

fn require_from_to(params: &CodeGraphParams) -> Result<(&str, &str), String> {
    let from = params
        .from
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("'from' is required for operation '{}'", params.operation))?;
    let to = params
        .to
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("'to' is required for operation '{}'", params.operation))?;
    Ok((from, to))
}

fn require_globs(params: &CodeGraphParams) -> Result<(&str, &str), String> {
    let from = params
        .from_glob
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "'from_glob' is required for operation '{}'",
                params.operation
            )
        })?;
    let to = params
        .to_glob
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("'to_glob' is required for operation '{}'", params.operation))?;
    Ok((from, to))
}

fn validate_visibility(visibility: Option<&str>) -> Result<(), String> {
    if let Some(v) = visibility {
        match v {
            "public" | "private" | "any" => Ok(()),
            _ => Err(format!(
                "invalid visibility '{v}': expected 'public', 'private', or 'any'"
            )),
        }
    } else {
        Ok(())
    }
}

fn validate_sort_by(sort_by: Option<&str>) -> Result<(), String> {
    if let Some(s) = sort_by {
        match s {
            "pagerank" | "in_degree" | "out_degree" | "total_degree" => Ok(()),
            _ => Err(format!(
                "invalid sort_by '{s}': expected 'pagerank', 'in_degree', 'out_degree', or 'total_degree'"
            )),
        }
    } else {
        Ok(())
    }
}

fn validate_group_by(group_by: Option<&str>) -> Result<(), String> {
    if let Some(g) = group_by {
        match g {
            "file" => Ok(()),
            _ => Err(format!("invalid group_by '{g}': only 'file' is supported")),
        }
    } else {
        Ok(())
    }
}

// ── Handler ─────────────────────────────────────────────────────────────────────

#[tool_router(router = graph_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Query the repository dependency graph built from SCIP indexer output.
    #[tool(
        description = "Query the repository dependency graph built from SCIP indexer output. Operations: neighbors (edges in/out of a node, with optional group_by=file rollup), ranked (top nodes; sort_by pagerank/in_degree/out_degree/total_degree), impact (transitive dependents, with optional group_by=file rollup), implementations (find implementors of a trait/interface symbol), search (name-based symbol lookup), cycles (strongly-connected components), orphans (zero-incoming-reference nodes, with visibility filter), path (shortest dependency path), edges (enumerate edges by from_glob/to_glob), symbols_at (given file+line range, return SCIP symbols whose definition range encloses those lines — diff-hunk → symbol lookup), diff_touches (given a list of changed line ranges parsed from `git diff --unified=0 base..head`, return every base-graph symbol touched, with fan-in/fan-out and file grouping; the base graph is always current main — this op does NOT build a head graph), describe (symbol signature/documentation without an LSP round trip), status (peek at the persisted canonical graph cache; never warms), api_surface (list every public symbol with fan-in/fan-out and a used-outside-crate signal), boundary_check (edge-based architecture rule scanner over from_glob→to_glob pairs; returns forbidden violations), hotspots (file churn × centrality ranking over a configurable window; top_symbols per file), metrics_at (scalar graph snapshot: node/edge/cycle counts, god-object floor, orphans, public API and doc coverage), dead_symbols (no-incoming-edge-from-entry-points enumeration; confidence=high|med|low), deprecated_callers (symbols whose signature/documentation contains #[deprecated] or @deprecated, with caller list), touches_hot_path (given entry and sink SCIP keys, report which queried symbols sit on any entry→sink shortest path)."
    )]
    pub async fn code_graph(
        &self,
        Parameters(mut params): Parameters<CodeGraphParams>,
    ) -> Json<ErrorOr<CodeGraphResponse>> {
        // Resolve `project` (UUID or slug) to (project_id, clone_path)
        // once here; inner handlers read the pre-populated `project_id`
        // and `project_path` fields without hitting the DB again.
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project = match repo.resolve(&params.project).await {
            Ok(Some(id)) => match repo.get(&id).await {
                Ok(Some(p)) => p,
                _ => {
                    return Json(ErrorOr::Error(ErrorResponse {
                        error: format!("project not found: {}", params.project),
                    }));
                }
            },
            Ok(None) => {
                return Json(ErrorOr::Error(ErrorResponse {
                    error: format!("project not found: {}", params.project),
                }));
            }
            Err(e) => {
                return Json(ErrorOr::Error(ErrorResponse {
                    error: format!("project lookup failed: {e}"),
                }));
            }
        };
        params.project_id = project.id.clone();
        params.project_path = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
            .to_string_lossy()
            .into_owned();

        // Build the resolved `ProjectCtx` once. Inner handlers pass it
        // straight to the `RepoGraphOps` bridge so no downstream code
        // needs to reverse-parse `{projects_root}/{owner}/{repo}`.
        let ctx = ProjectCtx {
            id: params.project_id.clone(),
            clone_path: params.project_path.clone(),
        };

        let result = match params.operation.as_str() {
            "neighbors" => self.code_graph_neighbors(&ctx, &params).await,
            "ranked" => self.code_graph_ranked(&ctx, &params).await,
            "implementations" => self.code_graph_implementations(&ctx, &params).await,
            "impact" => self.code_graph_impact(&ctx, &params).await,
            "search" => self.code_graph_search(&ctx, &params).await,
            "cycles" => self.code_graph_cycles(&ctx, &params).await,
            "orphans" => self.code_graph_orphans(&ctx, &params).await,
            "path" => self.code_graph_path(&ctx, &params).await,
            "edges" => self.code_graph_edges(&ctx, &params).await,
            "describe" => self.code_graph_describe(&ctx, &params).await,
            "status" => self.code_graph_status(&ctx, &params).await,
            "symbols_at" => self.code_graph_symbols_at(&ctx, &params).await,
            "diff_touches" => self.code_graph_diff_touches(&ctx, &params).await,
            "api_surface" => self.code_graph_api_surface(&ctx, &params).await,
            "boundary_check" => self.code_graph_boundary_check(&ctx, &params).await,
            "hotspots" => self.code_graph_hotspots(&ctx, &params).await,
            "metrics_at" => self.code_graph_metrics_at(&ctx, &params).await,
            "dead_symbols" => self.code_graph_dead_symbols(&ctx, &params).await,
            "deprecated_callers" => self.code_graph_deprecated_callers(&ctx, &params).await,
            "touches_hot_path" => self.code_graph_touches_hot_path(&ctx, &params).await,
            other => Err(format!(
                "unknown code_graph operation '{other}': expected one of \
                 'neighbors', 'ranked', 'impact', 'implementations', \
                 'search', 'cycles', 'orphans', 'path', 'edges', \
                 'symbols_at', 'diff_touches', 'describe', 'status', \
                 'api_surface', 'boundary_check', 'hotspots', 'metrics_at', \
                 'dead_symbols', 'deprecated_callers', 'touches_hot_path'"
            )),
        };

        Json(match result {
            Ok(response) => ErrorOr::Ok(response),
            Err(error) => ErrorOr::Error(ErrorResponse { error }),
        })
    }
}

impl DjinnMcpServer {
    /// Load the per-project graph exclusions, rendered into a compiled
    /// [`GraphExclusions`] predicate. On any lookup failure we fall
    /// back to [`GraphExclusions::empty`], which still applies Tier 1
    /// (universal SCIP module-artifact suppression).
    async fn load_graph_exclusions(&self, project_id: &str) -> GraphExclusions {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.get_config(project_id).await {
            Ok(Some(config)) => GraphExclusions::from_config(&config),
            Ok(None) => GraphExclusions::empty(),
            Err(e) => {
                tracing::debug!(
                    project_id = %project_id,
                    error = %e,
                    "graph_exclusions: config read failed; using Tier 1 only",
                );
                GraphExclusions::empty()
            }
        }
    }

    async fn code_graph_neighbors(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        validate_direction(params.direction.as_deref())?;
        validate_group_by(params.group_by.as_deref())?;
        let result = self
            .state
            .repo_graph()
            .neighbors(
                ctx,
                key,
                params.direction.as_deref(),
                params.group_by.as_deref(),
            )
            .await?;
        // Bound the wire size — the underlying neighbors() call returns every
        // edge incident on the node (1k+ for high-centrality files), which
        // makes the MCP response unusably large. Sort by weight desc and cap
        // at `limit` (default 20, matching other list operations).
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let (neighbors, file_groups) = match result {
            NeighborsResult::Detailed(mut v) => {
                v.retain(|n| !exclusions.excludes(&n.key, None, &n.display_name));
                v.sort_by(|a, b| {
                    b.edge_weight
                        .partial_cmp(&a.edge_weight)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                v.truncate(limit);
                (Some(v), None)
            }
            NeighborsResult::Grouped(mut v) => {
                v.retain(|g| !exclusions.excludes(&g.file, Some(&g.file), &g.file));
                v.sort_by_key(|g| std::cmp::Reverse(g.occurrence_count));
                v.truncate(limit);
                (None, Some(v))
            }
        };
        Ok(CodeGraphResponse::Neighbors(NeighborsResponse {
            key: key.to_string(),
            neighbors,
            file_groups,
        }))
    }

    async fn code_graph_ranked(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        validate_sort_by(params.sort_by.as_deref())?;
        let limit = params.limit.unwrap_or(20) as usize;
        // Over-fetch so filtering doesn't leave us short of the
        // caller's requested limit. 4× is a cheap slack — on the
        // platform repo today Tier 1 strips ~2% of ranked nodes, so
        // 4× covers any realistic Tier 2 glob list without needing a
        // second round-trip. Clamp to 200 to keep the cache lookup
        // cheap and the post-filter linear.
        let fetch_limit = (limit.saturating_mul(4)).clamp(limit, 200);
        let nodes = self
            .state
            .repo_graph()
            .ranked(
                ctx,
                params.kind_filter.as_deref(),
                params.sort_by.as_deref(),
                fetch_limit,
            )
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let mut nodes: Vec<RankedNode> = nodes
            .into_iter()
            .filter(|n| !exclusions.excludes(&n.key, None, &n.display_name))
            .take(limit)
            .collect();
        // The bridge already returns ranked-order; `filter` preserves it.
        nodes.truncate(limit);
        Ok(CodeGraphResponse::Ranked(RankedResponse { nodes }))
    }

    async fn code_graph_implementations(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let implementations = self
            .state
            .repo_graph()
            .implementations(ctx, key)
            .await?;
        Ok(CodeGraphResponse::Implementations(
            ImplementationsResponse {
                symbol: key.to_string(),
                implementations,
            },
        ))
    }

    async fn code_graph_impact(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        validate_group_by(params.group_by.as_deref())?;
        let depth = params.limit.unwrap_or(3) as usize;
        let result = self
            .state
            .repo_graph()
            .impact(ctx, key, depth, params.group_by.as_deref())
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let (impact, file_groups) = match result {
            ImpactResult::Detailed(mut v) => {
                // ImpactEntry has no display_name; match key only (Tier
                // 1 still catches module artifacts; Tier 2 globs bound
                // against the SCIP key, matching the old client-side
                // behaviour).
                v.retain(|e| !exclusions.excludes(&e.key, None, &e.key));
                (Some(v), None)
            }
            ImpactResult::Grouped(mut v) => {
                v.retain(|g| !exclusions.excludes(&g.file, Some(&g.file), &g.file));
                (None, Some(v))
            }
        };
        Ok(CodeGraphResponse::Impact(ImpactResponse {
            key: key.to_string(),
            impact,
            file_groups,
        }))
    }

    async fn code_graph_search(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let query = require_query(params)?;
        validate_kind_filter(params.kind_filter.as_deref())?;
        let limit = params.limit.unwrap_or(20) as usize;
        let fetch_limit = (limit.saturating_mul(4)).clamp(limit, 200);
        let hits = self
            .state
            .repo_graph()
            .search(
                ctx,
                query,
                params.kind_filter.as_deref(),
                fetch_limit,
            )
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let hits: Vec<SearchHit> = hits
            .into_iter()
            .filter(|h| {
                !exclusions.excludes(&h.key, h.file.as_deref(), &h.display_name)
            })
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::Search(SearchResponse {
            query: query.to_string(),
            hits,
        }))
    }

    async fn code_graph_cycles(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        let min_size = params.min_size.unwrap_or(2).max(0) as usize;
        // Ask the warmer cache for SCCs with a size floor of 2 so we
        // can still shed an SCC whose surviving members drop below the
        // user-requested `min_size` after exclusion filtering. We
        // re-apply `min_size` post-filter below.
        let fetch_floor = min_size.max(2);
        let cycles = self
            .state
            .repo_graph()
            .cycles(
                ctx,
                params.kind_filter.as_deref(),
                fetch_floor,
            )
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let cycles: Vec<CycleGroup> = cycles
            .into_iter()
            .filter_map(|group| {
                let members: Vec<_> = group
                    .members
                    .into_iter()
                    .filter(|m| !exclusions.excludes(&m.key, None, &m.display_name))
                    .collect();
                if members.len() < min_size.max(2) {
                    None
                } else {
                    Some(CycleGroup {
                        size: members.len(),
                        members,
                    })
                }
            })
            .collect();
        Ok(CodeGraphResponse::Cycles(CyclesResponse { cycles }))
    }

    async fn code_graph_orphans(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        validate_visibility(params.visibility.as_deref())?;
        let limit = params.limit.unwrap_or(50) as usize;
        let fetch_limit = (limit.saturating_mul(4)).clamp(limit, 500);
        let orphans = self
            .state
            .repo_graph()
            .orphans(
                ctx,
                params.kind_filter.as_deref(),
                params.visibility.as_deref(),
                fetch_limit,
            )
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let orphans: Vec<OrphanEntry> = orphans
            .into_iter()
            .filter(|o| {
                !exclusions.excludes_orphan(&o.key, o.file.as_deref(), &o.display_name)
            })
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::Orphans(OrphansResponse { orphans }))
    }

    async fn code_graph_path(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let (from, to) = require_from_to(params)?;
        let max_depth = params.max_depth.map(|v| v.max(0) as usize);
        let path = self
            .state
            .repo_graph()
            .path(ctx, from, to, max_depth)
            .await?;
        Ok(CodeGraphResponse::Path(PathResponse { path }))
    }

    async fn code_graph_edges(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let (from_glob, to_glob) = require_globs(params)?;
        let limit = params.limit.unwrap_or(100) as usize;
        // Over-fetch so the exclusion post-filter doesn't starve the
        // requested limit. Edges are cheap to drop but we want the
        // returned set to honour `limit` after Tier 1+2 pruning.
        let fetch_limit = (limit.saturating_mul(4)).clamp(limit, 400);
        let edges = self
            .state
            .repo_graph()
            .edges(
                ctx,
                from_glob,
                to_glob,
                params.edge_kind.as_deref(),
                fetch_limit,
            )
            .await?;
        // Drop edges whose `from` OR `to` endpoint is filtered — a
        // boundary-check style query over the graph should not surface
        // edges that touch SCIP-artifact nodes or user-excluded paths,
        // even if the glob pair technically matches.
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let edges: Vec<EdgeEntry> = edges
            .into_iter()
            .filter(|e| {
                !exclusions.excludes(&e.from, Some(&e.from), &e.from)
                    && !exclusions.excludes(&e.to, Some(&e.to), &e.to)
            })
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::Edges(EdgesResponse { edges }))
    }

    async fn code_graph_describe(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let description = self
            .state
            .repo_graph()
            .describe(ctx, key)
            .await?;
        Ok(CodeGraphResponse::Describe(DescribeResponse {
            description,
        }))
    }

    async fn code_graph_status(
        &self,
        ctx: &ProjectCtx,
        _params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let status = self.state.repo_graph().status(ctx).await?;
        Ok(CodeGraphResponse::Status(StatusResponse { status }))
    }

    /// Handler for `operation = "symbols_at"`.
    ///
    /// Requires `file` + `start_line`; `end_line` defaults to `start_line`
    /// when omitted. No exclusion filter is applied here — the caller
    /// already named the file, so this is a lookup, not a discovery.
    async fn code_graph_symbols_at(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let file = params
            .file
            .as_deref()
            .filter(|f| !f.is_empty())
            .ok_or_else(|| format!("'file' is required for operation '{}'", params.operation))?;
        let start_line = params.start_line.ok_or_else(|| {
            format!(
                "'start_line' is required for operation '{}'",
                params.operation
            )
        })?;
        let end_line = params.end_line;
        let hits = self
            .state
            .repo_graph()
            .symbols_at(ctx, file, start_line, end_line)
            .await?;
        Ok(CodeGraphResponse::SymbolsAt(SymbolsAtResponse {
            file: file.to_string(),
            hits,
        }))
    }

    /// Handler for `operation = "diff_touches"`.
    ///
    /// Requires a non-empty `changed_ranges` list. The Phase 0 graph
    /// exclusions filter is applied post-query: touched symbols whose
    /// key, file, or display_name matches an exclusion are dropped
    /// because they are noise even in PR-review context (generated
    /// `mod.rs`, third-party shims, etc.).
    async fn code_graph_diff_touches(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let changed_ranges = params.changed_ranges.as_deref().ok_or_else(|| {
            format!(
                "'changed_ranges' is required for operation '{}'",
                params.operation
            )
        })?;
        if changed_ranges.is_empty() {
            return Err(format!(
                "'changed_ranges' must not be empty for operation '{}'",
                params.operation
            ));
        }
        let result = self
            .state
            .repo_graph()
            .diff_touches(ctx, changed_ranges)
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let touched_symbols: Vec<TouchedSymbol> = result
            .touched_symbols
            .into_iter()
            .filter(|s| !exclusions.excludes(&s.key, s.file.as_deref(), &s.display_name))
            .collect();
        Ok(CodeGraphResponse::DiffTouches(DiffTouchesResponse {
            touched_symbols,
            affected_files: result.affected_files,
            unknown_files: result.unknown_files,
        }))
    }

    /// Handler for `operation = "api_surface"`.
    async fn code_graph_api_surface(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_visibility(params.visibility.as_deref())?;
        let limit = params.limit.unwrap_or(100).max(0) as usize;
        let symbols = self
            .state
            .repo_graph()
            .api_surface(
                ctx,
                params.module_glob.as_deref(),
                params.visibility.as_deref(),
                limit.saturating_mul(4).clamp(limit, 500),
            )
            .await?;
        // The bridge already applies the exclusions; also defend against
        // noise that might slip in if the bridge is evolved later.
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let symbols: Vec<ApiSurfaceEntry> = symbols
            .into_iter()
            .filter(|e| {
                !exclusions.excludes(&e.key, e.file.as_deref(), &e.display_name)
            })
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::ApiSurface(ApiSurfaceResponse {
            symbols,
        }))
    }

    /// Handler for `operation = "boundary_check"`.
    async fn code_graph_boundary_check(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let rules = params.rules.as_deref().ok_or_else(|| {
            format!(
                "'rules' is required for operation '{}'",
                params.operation
            )
        })?;
        if rules.is_empty() {
            return Err(format!(
                "'rules' must not be empty for operation '{}'",
                params.operation
            ));
        }
        let violations = self
            .state
            .repo_graph()
            .boundary_check(ctx, rules)
            .await?;
        Ok(CodeGraphResponse::BoundaryCheck(BoundaryCheckResponse {
            violations,
        }))
    }

    /// Handler for `operation = "hotspots"`.
    async fn code_graph_hotspots(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let window = params.window_days.unwrap_or(90).clamp(1, 365);
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let limit = limit.clamp(1, 100);
        let hotspots = self
            .state
            .repo_graph()
            .hotspots(
                ctx,
                window,
                params.file_glob.as_deref(),
                limit,
            )
            .await?;
        Ok(CodeGraphResponse::Hotspots(HotspotsResponse { hotspots }))
    }

    /// Handler for `operation = "metrics_at"`.
    async fn code_graph_metrics_at(
        &self,
        ctx: &ProjectCtx,
        _params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let metrics = self
            .state
            .repo_graph()
            .metrics_at(ctx)
            .await?;
        Ok(CodeGraphResponse::MetricsAt(MetricsAtResponse { metrics }))
    }

    /// Handler for `operation = "dead_symbols"`.
    async fn code_graph_dead_symbols(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let confidence = params.confidence.as_deref().unwrap_or("high");
        if !matches!(confidence, "high" | "med" | "low") {
            return Err(format!(
                "invalid confidence '{confidence}': expected 'high', 'med', or 'low'"
            ));
        }
        let limit = params.limit.unwrap_or(100).max(0) as usize;
        let symbols = self
            .state
            .repo_graph()
            .dead_symbols(ctx, confidence, limit)
            .await?;
        Ok(CodeGraphResponse::DeadSymbols(DeadSymbolsResponse {
            symbols,
        }))
    }

    /// Handler for `operation = "deprecated_callers"`.
    async fn code_graph_deprecated_callers(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let limit = params.limit.unwrap_or(50).max(0) as usize;
        let hits = self
            .state
            .repo_graph()
            .deprecated_callers(ctx, limit)
            .await?;
        Ok(CodeGraphResponse::DeprecatedCallers(
            DeprecatedCallersResponse { hits },
        ))
    }

    /// Handler for `operation = "touches_hot_path"`.
    async fn code_graph_touches_hot_path(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let seed_entries = params
            .seed_entries
            .as_deref()
            .ok_or_else(|| {
                format!(
                    "'seed_entries' is required for operation '{}'",
                    params.operation
                )
            })?;
        let seed_sinks = params.seed_sinks.as_deref().ok_or_else(|| {
            format!(
                "'seed_sinks' is required for operation '{}'",
                params.operation
            )
        })?;
        let symbols = params.symbols.as_deref().ok_or_else(|| {
            format!(
                "'symbols' is required for operation '{}'",
                params.operation
            )
        })?;
        let hits = self
            .state
            .repo_graph()
            .touches_hot_path(ctx, seed_entries, seed_sinks, symbols)
            .await?;
        Ok(CodeGraphResponse::TouchesHotPath(TouchesHotPathResponse {
            hits,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_operation_field() {
        let params = test_params("unknown_op");
        // Just test validation logic, not the async handler
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn validates_direction() {
        assert!(validate_direction(Some("incoming")).is_ok());
        assert!(validate_direction(Some("outgoing")).is_ok());
        assert!(validate_direction(None).is_ok());
        assert!(validate_direction(Some("both")).is_err());
    }

    #[test]
    fn validates_kind_filter() {
        assert!(validate_kind_filter(Some("file")).is_ok());
        assert!(validate_kind_filter(Some("symbol")).is_ok());
        assert!(validate_kind_filter(None).is_ok());
        assert!(validate_kind_filter(Some("unknown")).is_err());
    }

    fn test_params(op: &str) -> CodeGraphParams {
        CodeGraphParams {
            operation: op.to_string(),
            project: "test/test".to_string(),
            project_id: String::new(),
            project_path: "/tmp".to_string(),
            key: None,
            direction: None,
            kind_filter: None,
            limit: None,
            query: None,
            from: None,
            to: None,
            from_glob: None,
            to_glob: None,
            min_size: None,
            visibility: None,
            sort_by: None,
            group_by: None,
            max_depth: None,
            edge_kind: None,
            file: None,
            start_line: None,
            end_line: None,
            changed_ranges: None,
            module_glob: None,
            confidence: None,
            window_days: None,
            file_glob: None,
            rules: None,
            seed_entries: None,
            seed_sinks: None,
            symbols: None,
        }
    }

    #[test]
    fn require_key_rejects_empty() {
        let mut params = test_params("neighbors");
        params.key = Some(String::new());
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn require_key_rejects_missing() {
        let params = test_params("neighbors");
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn require_key_accepts_valid() {
        let mut params = test_params("neighbors");
        params.key = Some("src/lib.rs".to_string());
        assert_eq!(require_key(&params).unwrap(), "src/lib.rs");
    }

    #[test]
    fn require_query_rejects_missing() {
        let params = test_params("search");
        assert!(require_query(&params).is_err());
    }

    #[test]
    fn require_from_to_rejects_partial() {
        let mut params = test_params("path");
        params.from = Some("src/a.rs".to_string());
        assert!(require_from_to(&params).is_err());
        params.to = Some("src/b.rs".to_string());
        assert_eq!(require_from_to(&params).unwrap(), ("src/a.rs", "src/b.rs"));
    }

    #[test]
    fn require_globs_rejects_partial() {
        let mut params = test_params("edges");
        params.from_glob = Some("**/*.rs".to_string());
        assert!(require_globs(&params).is_err());
        params.to_glob = Some("**/*.rs".to_string());
        assert!(require_globs(&params).is_ok());
    }

    #[test]
    fn validates_visibility() {
        assert!(validate_visibility(None).is_ok());
        assert!(validate_visibility(Some("public")).is_ok());
        assert!(validate_visibility(Some("private")).is_ok());
        assert!(validate_visibility(Some("any")).is_ok());
        assert!(validate_visibility(Some("internal")).is_err());
    }

    #[test]
    fn validates_sort_by() {
        assert!(validate_sort_by(None).is_ok());
        assert!(validate_sort_by(Some("pagerank")).is_ok());
        assert!(validate_sort_by(Some("in_degree")).is_ok());
        assert!(validate_sort_by(Some("out_degree")).is_ok());
        assert!(validate_sort_by(Some("total_degree")).is_ok());
        assert!(validate_sort_by(Some("alpha")).is_err());
    }

    #[test]
    fn validates_group_by() {
        assert!(validate_group_by(None).is_ok());
        assert!(validate_group_by(Some("file")).is_ok());
        assert!(validate_group_by(Some("symbol")).is_err());
    }

    #[test]
    fn parses_code_graph_params_from_json() {
        let json = serde_json::json!({
            "operation": "neighbors",
            "project_path": "/workspace/repo",
            "key": "src/main.rs",
            "direction": "outgoing"
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "neighbors");
        assert_eq!(params.key.as_deref(), Some("src/main.rs"));
        assert_eq!(params.direction.as_deref(), Some("outgoing"));
        assert!(params.kind_filter.is_none());
        assert!(params.limit.is_none());
    }

    #[test]
    fn parses_ranked_params_from_json() {
        let json = serde_json::json!({
            "operation": "ranked",
            "project_path": "/workspace/repo",
            "kind_filter": "file",
            "limit": 10
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "ranked");
        assert!(params.key.is_none());
        assert_eq!(params.kind_filter.as_deref(), Some("file"));
        assert_eq!(params.limit, Some(10));
    }

    #[test]
    fn parses_impact_params_from_json() {
        let json = serde_json::json!({
            "operation": "impact",
            "project_path": "/workspace/repo",
            "key": "scip-rust . . . MyStruct#",
            "limit": 5
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "impact");
        assert_eq!(params.key.as_deref(), Some("scip-rust . . . MyStruct#"));
        assert_eq!(params.limit, Some(5));
    }

    #[test]
    fn parses_implementations_params_from_json() {
        let json = serde_json::json!({
            "operation": "implementations",
            "project_path": "/workspace/repo",
            "key": "scip-rust . . . Trait#"
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "implementations");
        assert_eq!(params.key.as_deref(), Some("scip-rust . . . Trait#"));
    }

    #[test]
    fn parses_search_params_from_json() {
        let json = serde_json::json!({
            "operation": "search",
            "project_path": "/workspace/repo",
            "query": "AgentSession",
            "kind_filter": "symbol",
            "limit": 5,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "search");
        assert_eq!(params.query.as_deref(), Some("AgentSession"));
        assert_eq!(params.kind_filter.as_deref(), Some("symbol"));
        assert_eq!(params.limit, Some(5));
    }

    #[test]
    fn parses_cycles_params_from_json() {
        let json = serde_json::json!({
            "operation": "cycles",
            "project_path": "/workspace/repo",
            "min_size": 3,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "cycles");
        assert_eq!(params.min_size, Some(3));
    }

    #[test]
    fn parses_orphans_params_from_json() {
        let json = serde_json::json!({
            "operation": "orphans",
            "project_path": "/workspace/repo",
            "visibility": "private",
            "limit": 25,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.visibility.as_deref(), Some("private"));
        assert_eq!(params.limit, Some(25));
    }

    #[test]
    fn parses_path_params_from_json() {
        let json = serde_json::json!({
            "operation": "path",
            "project_path": "/workspace/repo",
            "from": "src/a.rs",
            "to": "src/b.rs",
            "max_depth": 6,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.from.as_deref(), Some("src/a.rs"));
        assert_eq!(params.to.as_deref(), Some("src/b.rs"));
        assert_eq!(params.max_depth, Some(6));
    }

    #[test]
    fn parses_edges_params_from_json() {
        let json = serde_json::json!({
            "operation": "edges",
            "project_path": "/workspace/repo",
            "from_glob": "server/src/**",
            "to_glob": "server/crates/**",
            "edge_kind": "FileReference",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.from_glob.as_deref(), Some("server/src/**"));
        assert_eq!(params.to_glob.as_deref(), Some("server/crates/**"));
        assert_eq!(params.edge_kind.as_deref(), Some("FileReference"));
    }

    #[test]
    fn parses_describe_params_from_json() {
        let json = serde_json::json!({
            "operation": "describe",
            "project_path": "/workspace/repo",
            "key": "scip-rust . . . AgentSession#",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "describe");
        assert_eq!(params.key.as_deref(), Some("scip-rust . . . AgentSession#"));
    }

    #[test]
    fn parses_symbols_at_params_from_json() {
        let json = serde_json::json!({
            "operation": "symbols_at",
            "project_path": "/workspace/repo",
            "file": "src/lib.rs",
            "start_line": 42,
            "end_line": 48,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "symbols_at");
        assert_eq!(params.file.as_deref(), Some("src/lib.rs"));
        assert_eq!(params.start_line, Some(42));
        assert_eq!(params.end_line, Some(48));
    }

    #[test]
    fn parses_symbols_at_params_without_end_line() {
        let json = serde_json::json!({
            "operation": "symbols_at",
            "project_path": "/workspace/repo",
            "file": "src/lib.rs",
            "start_line": 17,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.start_line, Some(17));
        assert!(params.end_line.is_none());
    }

    #[test]
    fn parses_api_surface_params_from_json() {
        let json = serde_json::json!({
            "operation": "api_surface",
            "project_path": "/workspace/repo",
            "module_glob": "server/src/**",
            "visibility": "public",
            "limit": 50,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "api_surface");
        assert_eq!(params.module_glob.as_deref(), Some("server/src/**"));
        assert_eq!(params.visibility.as_deref(), Some("public"));
        assert_eq!(params.limit, Some(50));
    }

    #[test]
    fn parses_boundary_check_params_from_json() {
        let json = serde_json::json!({
            "operation": "boundary_check",
            "project_path": "/workspace/repo",
            "rules": [
                {"from_glob": "server/src/**", "to_glob": "ui/**"},
                {"from_glob": "ui/**", "to_glob": "server/src/**"},
            ],
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        let rules = params.rules.as_ref().expect("rules set");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].from_glob, "server/src/**");
        assert_eq!(rules[1].to_glob, "server/src/**");
    }

    #[test]
    fn parses_hotspots_params_from_json() {
        let json = serde_json::json!({
            "operation": "hotspots",
            "project_path": "/workspace/repo",
            "window_days": 30,
            "file_glob": "server/src/**",
            "limit": 10,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "hotspots");
        assert_eq!(params.window_days, Some(30));
        assert_eq!(params.file_glob.as_deref(), Some("server/src/**"));
        assert_eq!(params.limit, Some(10));
    }

    #[test]
    fn parses_metrics_at_params_from_json() {
        let json = serde_json::json!({
            "operation": "metrics_at",
            "project_path": "/workspace/repo",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "metrics_at");
        assert_eq!(params.project_path, "/workspace/repo");
    }

    #[test]
    fn parses_dead_symbols_params_from_json() {
        let json = serde_json::json!({
            "operation": "dead_symbols",
            "project_path": "/workspace/repo",
            "confidence": "med",
            "limit": 75,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "dead_symbols");
        assert_eq!(params.confidence.as_deref(), Some("med"));
        assert_eq!(params.limit, Some(75));
    }

    #[test]
    fn parses_deprecated_callers_params_from_json() {
        let json = serde_json::json!({
            "operation": "deprecated_callers",
            "project_path": "/workspace/repo",
            "limit": 25,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "deprecated_callers");
        assert_eq!(params.limit, Some(25));
    }

    #[test]
    fn parses_touches_hot_path_params_from_json() {
        let json = serde_json::json!({
            "operation": "touches_hot_path",
            "project_path": "/workspace/repo",
            "seed_entries": ["scip-rust . . . entry#"],
            "seed_sinks": ["scip-rust . . . sink#"],
            "symbols": ["scip-rust . . . foo#", "scip-rust . . . bar#"],
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "touches_hot_path");
        assert_eq!(params.seed_entries.as_ref().unwrap().len(), 1);
        assert_eq!(params.seed_sinks.as_ref().unwrap().len(), 1);
        assert_eq!(params.symbols.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn parses_diff_touches_params_from_json() {
        let json = serde_json::json!({
            "operation": "diff_touches",
            "project_path": "/workspace/repo",
            "changed_ranges": [
                {"file": "src/a.rs", "start_line": 10, "end_line": 20},
                {"file": "src/b.rs", "start_line": 5},
            ],
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        let ranges = params.changed_ranges.as_ref().expect("changed_ranges set");
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].file, "src/a.rs");
        assert_eq!(ranges[0].start_line, 10);
        assert_eq!(ranges[0].end_line, Some(20));
        assert_eq!(ranges[1].file, "src/b.rs");
        assert_eq!(ranges[1].start_line, 5);
        assert!(ranges[1].end_line.is_none());
    }
}
