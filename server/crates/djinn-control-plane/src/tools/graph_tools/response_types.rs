use super::*;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RankedResponse {
    pub nodes: Vec<RankedNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImplementationsResponse {
    pub symbol: String,
    pub implementations: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// PR C3 risk bucket for an `impact` query, derived from `direct_count`,
/// `total_impacted`, and `module_count`. Serialized in SCREAMING_SNAKE
/// (`"LOW" | "MEDIUM" | "HIGH" | "CRITICAL"`) so reviewer prompts and
/// dashboards can string-match without round-tripping through the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ImpactRisk {
    Low,
    Medium,
    High,
    Critical,
}

impl ImpactRisk {
    /// PR C3 thresholds. The `direct`/`total`/`modules` triple is
    /// OR-combined within each tier, then evaluated top-down (critical
    /// first) so the highest matching bucket wins.
    pub(crate) fn classify(direct: usize, total: usize, modules: usize) -> Self {
        if direct >= 20 || total >= 200 || modules >= 10 {
            ImpactRisk::Critical
        } else if direct >= 10 || total >= 80 || modules >= 5 {
            ImpactRisk::High
        } else if direct >= 3 || total >= 20 || modules >= 2 {
            ImpactRisk::Medium
        } else {
            ImpactRisk::Low
        }
    }

    /// PR C3 hint gating: HIGH/CRITICAL impacts deserve a follow-up
    /// nudge toward `dead_symbols` + `deprecated_callers` so reviewers
    /// pre-clean the blast radius before the change lands.
    pub(crate) fn is_high_or_critical(self) -> bool {
        matches!(self, ImpactRisk::High | ImpactRisk::Critical)
    }
}

// See NeighborsResponse above — same flatten-on-sequence bug. Impact emits
// its detailed list under `impact` and its file rollup under `file_groups`.
//
// PR C3 additions (`risk`, `summary`) are skipped when `None` so the wire
// stays additive: callers that don't ask for risk classification (e.g.
// `group_by=file` rollup with no risk computation) still serialize as
// before.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImpactResponse {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<Vec<ImpactEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_groups: Option<Vec<FileGroupEntry>>,
    /// PR C3: blast-radius bucket (`LOW`/`MEDIUM`/`HIGH`/`CRITICAL`).
    /// Populated for both detailed and grouped responses; absent when
    /// classification was skipped (e.g. fixture-only test paths).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<ImpactRisk>,
    /// PR C3: 1-line human summary, e.g. `"12 direct caller(s) across
    /// 3 module(s)"`. Stable phrasing so chat UIs and reviewer prompts
    /// can lift it verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResponse {
    pub query: String,
    pub hits: Vec<SearchHit>,
    /// Populated when the caller passed a non-empty `workspace` slug that
    /// did not match any node in the graph. Contains the available
    /// workspace slugs so the caller can recover from a typo or stale
    /// slug. Absent when the workspace was omitted, known, or the
    /// project exposes a single workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CyclesResponse {
    pub cycles: Vec<CycleGroup>,
    /// Populated when the caller passed a non-empty `workspace` slug that
    /// did not match any node in the graph. Contains the available
    /// workspace slugs so the caller can recover. Absent when the
    /// workspace was omitted, known, or the project exposes a single
    /// workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OrphansResponse {
    pub orphans: Vec<OrphanEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PathResponse {
    pub path: Option<PathResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EdgesResponse {
    pub edges: Vec<EdgeEntry>,
    /// Populated when the caller passed a non-empty `workspace` slug that
    /// did not match any node in the graph. Contains the available
    /// workspace slugs so the caller can recover. Absent when the
    /// workspace was omitted, known, or the project exposes a single
    /// workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DescribeResponse {
    pub description: Option<SymbolDescription>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// PR C1: 360° symbol view emitted by `code_graph context`. The
/// discriminator field per the inter-PR contract is `symbol_context`,
/// which carries `{symbol, incoming, outgoing, processes}`. UI parsers
/// (`parseSymbolContext` in `pulseTypes.ts`) hang off that field name.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ContextResponse {
    pub symbol_context: SymbolContext,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusResponse {
    #[serde(flatten)]
    pub status: GraphStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `workspaces` op — graph-observed workspace slugs joined
/// with per-workspace freshness rows.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspacesResponse {
    #[serde(flatten)]
    pub result: WorkspacesResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `symbols_at` op — the queried file and every symbol
/// hit whose definition range encloses the requested line window.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolsAtResponse {
    pub file: String,
    pub hits: Vec<SymbolAtHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `diff_touches` op — touched-symbol rollup plus the
/// affected/unknown-file partition.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiffTouchesResponse {
    pub touched_symbols: Vec<TouchedSymbol>,
    pub affected_files: Vec<String>,
    pub unknown_files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `detect_changes` op (PR C4). The discriminator field
/// is `detected_changes` (matching the `CodeGraphResponse` untagged-enum
/// contract); a `next_step` hint nudges the caller toward an `impact`
/// follow-up on the highest-tier touched symbol.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DetectedChangesResponse {
    pub detected_changes: DetectedChangesResult,
    /// Human-readable suggestion for the next MCP call. Always present
    /// (matches the A4 next-step convention).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `api_surface` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiSurfaceResponse {
    pub symbols: Vec<ApiSurfaceEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `boundary_check` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BoundaryCheckResponse {
    pub violations: Vec<BoundaryViolation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `hotspots` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HotspotsResponse {
    pub hotspots: Vec<HotspotEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Iter 28: response for the `complexity` op. The result is itself an
/// untagged union (`Functions` | `Files`), so the discriminator on the
/// outer `CodeGraphResponse` enum is a unique top-level field name —
/// `complexity` — and we wrap rather than `#[serde(flatten)]` to avoid
/// the same flatten-on-sequence pitfall noted on `NeighborsResponse`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ComplexityResponse {
    pub complexity: ComplexityResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Iter 29: response for the `refactor_candidates` op. The discriminator
/// is `refactor_candidates` so the untagged enum stays disambiguable
/// from every other variant. Wrapping rather than flattening matches
/// the iter-28 `complexity` convention.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefactorCandidatesResponse {
    pub refactor_candidates: Vec<RefactorCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `metrics_at` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MetricsAtResponse {
    #[serde(flatten)]
    pub metrics: MetricsAtResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `dead_symbols` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeadSymbolsResponse {
    pub symbols: Vec<DeadSymbolEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `deprecated_callers` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeprecatedCallersResponse {
    pub hits: Vec<DeprecatedHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `touches_hot_path` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TouchesHotPathResponse {
    pub hits: Vec<HotPathHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `coupling` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CouplingResponse {
    pub file: String,
    pub coupled: Vec<CouplingEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `churn` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChurnResponse {
    pub files: Vec<ChurnEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `coupling_hotspots` op — top file pairs ranked by
/// distinct-commit co-edit count.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CouplingHotspotsResponse {
    pub pairs: Vec<CoupledPairEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// Response for the `coupling_hubs` op — files by cumulative coupling
/// across all partners (change-propagation risk map).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CouplingHubsResponse {
    pub hubs: Vec<CouplingHubEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// PR C2: emitted when the dispatcher's pre-resolve pass returns
/// multiple plausible nodes for a caller-supplied `key`. The wire shape
/// hangs on the `candidates` discriminator so the untagged enum stays
/// disambiguable from every other `CodeGraphResponse` variant.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AmbiguousResponse {
    pub candidates: Vec<Candidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// PR C2: emitted when neither the exact-match nor the name-search
/// fallback turns up any node for the supplied `key`. The body is an
/// object (not a bare string) so the discriminator is unambiguous and
/// callers can read `query` for telemetry / surfaces.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NotFoundResponse {
    pub not_found: NotFoundDetail,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NotFoundDetail {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind_hint: Option<String>,
}

/// PR D2: full-graph snapshot for the `/code-graph` UI. The discriminator
/// field per the inter-PR contract is `snapshot`, which carries the
/// shape spec'd in the plan (`{project_id, git_head, generated_at,
/// truncated, total_nodes, total_edges, node_cap, nodes, edges}`). We
/// wrap the payload under that field rather than flattening to avoid
/// colliding with `Ranked.nodes` and `Edges.edges` — the
/// `CodeGraphResponse` is `#[serde(untagged)]`, so a unique top-level
/// field name is the disambiguator.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SnapshotResponse {
    pub snapshot: SnapshotPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct QuerySubgraphResponse {
    pub query_subgraph: QuerySubgraphResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RouteMapResponse {
    pub route_map: RouteMapResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShapeCheckResponse {
    pub shape_check: ShapeCheckResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiImpactResponse {
    pub api_impact: ApiImpactResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
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
    /// PR C1: 360° symbol view (incoming/outgoing categorized neighbors
    /// + method metadata). Discriminator field `symbol_context`.
    Context(ContextResponse),
    Status(StatusResponse),
    Workspaces(WorkspacesResponse),
    SymbolsAt(SymbolsAtResponse),
    DiffTouches(DiffTouchesResponse),
    DetectedChanges(DetectedChangesResponse),
    ApiSurface(ApiSurfaceResponse),
    BoundaryCheck(BoundaryCheckResponse),
    Hotspots(HotspotsResponse),
    /// Iter 28: complexity ranking (functions or files).
    Complexity(ComplexityResponse),
    /// Iter 29: composite refactor-priority ranking (cognitive × churn ×
    /// pagerank z-scores). Discriminator field `refactor_candidates`.
    RefactorCandidates(RefactorCandidatesResponse),
    MetricsAt(MetricsAtResponse),
    DeadSymbols(DeadSymbolsResponse),
    DeprecatedCallers(DeprecatedCallersResponse),
    TouchesHotPath(TouchesHotPathResponse),
    Coupling(CouplingResponse),
    Churn(ChurnResponse),
    CouplingHotspots(CouplingHotspotsResponse),
    CouplingHubs(CouplingHubsResponse),
    /// PR C2: multi-match disambiguation list.
    Ambiguous(AmbiguousResponse),
    /// PR C2: hard miss — neither exact nor name-index resolution
    /// produced any hit for the caller's key.
    NotFound(NotFoundResponse),
    /// PR D2: full-graph snapshot for the `/code-graph` UI render.
    /// Discriminator field `snapshot`.
    Snapshot(SnapshotResponse),
    QuerySubgraph(QuerySubgraphResponse),
    RouteMap(RouteMapResponse),
    ShapeCheck(ShapeCheckResponse),
    ApiImpact(ApiImpactResponse),
}
