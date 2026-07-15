use super::*;

// ── Response types ──────────────────────────────────────────────────────────────

/// jc47: per-query staleness indicator comparing the caller-supplied HEAD
/// against the graph blob's pinned commit. Only populated when the caller
/// passes `current_head` (see [`CodeGraphParams`]); absent otherwise so
/// existing clients retain their previous response shape.
///
/// `is_stale` is `true` when the caller's commit differs from the cached
/// graph commit (exact comparison after trimming). When the cached graph
/// commit is missing or the status lookup fails, `is_stale` defaults to
/// `false` (non-stale-safe) and `cached_commit` is `None` so a missing
/// graph never blocks the query.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GraphStaleness {
    /// The commit the cached graph blob was built from (`None` when the
    /// graph cache has no pinned commit, e.g. un-warmed or status lookup
    /// failed). Compare against `caller_commit` to determine staleness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_commit: Option<String>,
    /// Echo of the caller-supplied commit (trimmed).
    pub caller_commit: String,
    /// `true` when `cached_commit` is present and differs from
    /// `caller_commit`. `false` when they match or when `cached_commit`
    /// is unknown (non-stale-safe default).
    pub is_stale: bool,
}

impl GraphStaleness {
    /// Compute staleness metadata from a caller-supplied commit and the
    /// optional cached graph commit. The caller commit is trimmed; a
    /// missing/blank cached commit yields `is_stale=false` (non-stale-safe).
    ///
    /// Delegates the comparison primitive to
    /// [`check_impact_staleness`] so the strict `impact_check` flow
    /// (epic z3en) and the lenient `code_graph` flow share a single
    /// trim+equality implementation. The lenient `is_stale=false` default
    /// for missing cached commits is applied AFTER the strict check so
    /// a missing graph never blocks an unrelated query.
    pub(crate) fn compute(caller_commit: &str, cached_commit: Option<&str>) -> Self {
        let (strict_is_stale, caller_commit, cached_commit) =
            check_impact_staleness(caller_commit, cached_commit);
        // Lenient override: when the graph has no pinned commit at all,
        // the strict helper returns `is_stale=true`, but `code_graph`
        // responses should not block callers on an un-warmed graph — the
        // caller is going to get an empty result anyway, and we want the
        // staleness signal to be additive, not load-bearing.
        let is_stale = if cached_commit.is_none() {
            false
        } else {
            strict_is_stale
        };
        GraphStaleness {
            cached_commit,
            caller_commit,
            is_stale,
        }
    }
}

/// Strict staleness primitive shared by the `impact_check` MCP tool and
/// the `code_graph` staleness flow (epic z3en, kfgh).
///
/// Returns `(is_stale, caller_commit, cached_commit)` where:
///
/// - `is_stale` is `true` when:
///   - `cached_commit` is missing/blank (un-warmed or un-pinned graph), OR
///   - the trimmed `cached_commit` and `caller_commit` differ.
/// - `caller_commit` is the trimmed caller commit (always present).
/// - `cached_commit` is the trimmed cached commit, or `None` when the
///   graph has no pinned commit.
///
/// This is the strict counterpart of `GraphStaleness::compute`'s lenient
/// default (which returns `is_stale=false` for missing cached commits to
/// keep unrelated `code_graph` queries flowing). For impact preflight we
/// MUST surface missing as stale — silently answering from unanchored
/// data would defeat the entire point of the freshness signal.
///
/// Mirrors [`djinn_graph::canonical_graph::git_head_is_strictly_stale`]
/// semantically: same trim + missing-blank→stale + equality rules. The
/// duplication is intentional: the control-plane does not depend on
/// `djinn-graph` directly, and the canonical bridge boundary already
/// returns `pinned_commit` as a `String`. Keep both implementations in
/// sync; the unit tests in `canonical_graph::tests` pin the semantics.
pub(crate) fn check_impact_staleness(
    caller_commit: &str,
    cached_commit: Option<&str>,
) -> (bool, String, Option<String>) {
    let trimmed_caller = caller_commit.trim();
    let trimmed_cached = cached_commit.map(str::trim).filter(|c| !c.is_empty());
    let is_stale = match &trimmed_cached {
        Some(cached) => *cached != trimmed_caller,
        None => true,
    };
    (
        is_stale,
        trimmed_caller.to_string(),
        trimmed_cached.map(str::to_string),
    )
}

// NOTE: previously `result: NeighborsResult` was `#[serde(flatten)]`, but
// `NeighborsResult` is an untagged enum of `Vec<_>` variants — serde's flatten
// adapter only accepts map-like types, so serialization failed at runtime with
// "can only flatten structs and maps (got a sequence)". We now emit the list
// under a named field that matches the desktop client parsers (`neighbors` for
// the detailed shape, `file_groups` for the `group_by=file` rollup).
//
// df6s: `total` / `offset` / `limit` / `has_more` are added so callers
// can distinguish "the graph has no more neighbors" from "you asked
// for page 3 and only got an empty list back". `summary_only` is set
// when the caller asked for a counts-only response — every field is
// `Option`-skipped-on-`None` so the wire shape stays additive for
// clients that haven't migrated yet.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NeighborsResponse {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neighbors: Option<Vec<GraphNeighbor>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_groups: Option<Vec<FileGroupEntry>>,
    /// df6s: total entries in the **unsliced** result set (post-exclusion,
    /// pre-`offset`/`limit`). `None` for full (non-paginated) responses so
    /// the wire shape is additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// df6s: page offset that was applied. `None` for the first page
    /// (`offset == 0`) and for full responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// df6s: page cap that was applied. `None` for full responses and
    /// for single-page (no `limit` cap) calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// df6s: `true` when more pages remain after the current one
    /// (`offset + limit < total`). `None` for full responses and when
    /// the result is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    /// df6s: `true` when the caller asked for a counts-only response.
    /// When `Some(true)`, the `neighbors` / `file_groups` lists are
    /// omitted (or empty) and `total` carries the count signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RankedResponse {
    pub nodes: Vec<RankedNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImplementationsResponse {
    pub symbol: String,
    pub implementations: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
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
//
// df6s: `total` / `offset` / `limit` / `has_more` follow the same
// contract as `NeighborsResponse` — `total` is the unsliced result
// count so a `LIMIT 50` page can never be misread as "the impact
// set only has 50 nodes". `summary_only` and `by_depth_counts` are
// the new df6s fields that mirror the request params and let
// triage callers skip the full payload.
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
    /// df6s: total entries in the **unsliced** result set
    /// (post-exclusion, pre-`offset`/`page_limit`). `None` for full
    /// (non-paginated) responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// df6s: page offset that was applied. `None` for the first page
    /// (`offset == 0`) and for full responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// df6s: page cap that was applied (`page_limit` for `impact`,
    /// since `limit` is the BFS depth there). `None` for full
    /// responses and for single-page calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// df6s: `true` when more pages remain after the current one
    /// (`offset + limit < total`). `None` for full responses and
    /// when the result is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    /// df6s: `true` when the caller asked for a counts-only response.
    /// When `Some(true)`, `impact` / `file_groups` are omitted (or
    /// empty) and `total` + `summary` carry the count signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_only: Option<bool>,
    /// df6s: per-depth impact count, e.g. `{ "1": 12, "2": 7 }`.
    /// Computed from the unsliced detailed set, so it always reflects
    /// the full impact distribution even when the page is capped.
    /// `None` when the caller didn't ask for it and didn't request
    /// `summary_only` (which implies the breakdown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_depth_counts: Option<std::collections::BTreeMap<String, usize>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<crate::tools::graph_tools::CoverageAdvisory>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<crate::tools::graph_tools::CoverageAdvisory>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OrphansResponse {
    pub orphans: Vec<OrphanEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<crate::tools::graph_tools::CoverageAdvisory>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PathResponse {
    pub path: Option<PathResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DescribeResponse {
    pub description: Option<SymbolDescription>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusResponse {
    #[serde(flatten)]
    pub status: GraphStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `workspaces` op — graph-observed workspace slugs joined
/// with per-workspace freshness rows.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspacesResponse {
    #[serde(flatten)]
    pub result: WorkspacesResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `symbols_at` op — the queried file and every symbol
/// hit whose definition range encloses the requested line window.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolsAtResponse {
    pub file: String,
    pub hits: Vec<SymbolAtHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `api_surface` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiSurfaceResponse {
    pub symbols: Vec<ApiSurfaceEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<crate::tools::graph_tools::CoverageAdvisory>,
}

/// Response for the `boundary_check` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BoundaryCheckResponse {
    pub violations: Vec<BoundaryViolation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `hotspots` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HotspotsResponse {
    pub hotspots: Vec<HotspotEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `metrics_at` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MetricsAtResponse {
    #[serde(flatten)]
    pub metrics: MetricsAtResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `dead_symbols` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeadSymbolsResponse {
    pub symbols: Vec<DeadSymbolEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<crate::tools::graph_tools::CoverageAdvisory>,
}

/// Response for the `deprecated_callers` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeprecatedCallersResponse {
    pub hits: Vec<DeprecatedHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `touches_hot_path` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TouchesHotPathResponse {
    pub hits: Vec<HotPathHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `coupling` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CouplingResponse {
    pub file: String,
    pub coupled: Vec<CouplingEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `churn` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChurnResponse {
    pub files: Vec<ChurnEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `coupling_hotspots` op — top file pairs ranked by
/// distinct-commit co-edit count.
///
/// df6s: `total` / `offset` / `limit` / `has_more` follow the
/// `NeighborsResponse` contract — `total` reflects the
/// coupling-index fetch, the page is the offset+limit slice, and
/// `summary_only` (when `Some(true)`) drops the `pairs` list in
/// favour of a count signal. All four pagination fields are
/// `Option`-skipped-on-`None` so the wire shape stays additive.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CouplingHotspotsResponse {
    pub pairs: Vec<CoupledPairEntry>,
    /// df6s: total entries in the **unsliced** result set
    /// (post-exclusion, pre-`offset`/`limit`). `None` for full
    /// (non-paginated) responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// df6s: page offset that was applied. `None` for the first page
    /// (`offset == 0`) and for full responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// df6s: page cap that was applied. `None` for full responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// df6s: `true` when more pages remain after the current one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    /// df6s: `true` when the caller asked for a counts-only response.
    /// When `Some(true)`, `pairs` is empty and `total` carries the
    /// count signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `coupling_hubs` op — files by cumulative coupling
/// across all partners (change-propagation risk map).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CouplingHubsResponse {
    pub hubs: Vec<CouplingHubEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct QuerySubgraphResponse {
    pub query_subgraph: QuerySubgraphResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RouteMapResponse {
    pub route_map: RouteMapResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShapeCheckResponse {
    pub shape_check: ShapeCheckResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiImpactResponse {
    pub api_impact: ApiImpactResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FlowResponse {
    pub flow: FlowResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Response for the `crate_graph` op — workspace crates as nodes with
/// aggregated cross-crate edges.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrateGraphOpResponse {
    pub crates: Vec<CrateNodeEntry>,
    pub edges: Vec<CrateEdgeEntry>,
    /// Present when the graph is empty (e.g. not a Rust workspace).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// Advisory impact preflight response: mechanical analysis of which
/// crates, files, and symbols would break if proposed removals/renames
/// land, along with a recommendation for how to slice the work.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImpactCheckResponse {
    /// Crate names whose code depends on the proposed targets.
    pub affected_crates: Vec<String>,
    /// Repository-relative file paths that consume proposed targets.
    pub affected_files: Vec<String>,
    /// Symbol keys that consume proposed targets.
    pub affected_symbols: Vec<String>,
    /// `true` when every affected consumer crate is inside the
    /// caller-supplied `scope_crates` (or when no consumers were
    /// found). A `true` value means the proposed slice can ship
    /// without breaking external consumers.
    pub safe_independent_slice: bool,
    /// Advisory recommendation:
    /// - `ok_independent` — safe to ship as independent tasks.
    /// - `chain_tasks` — consumers are within the slice but need
    ///   explicit ordering (task B blocked_by task A).
    /// - `atomic_cutover` — consumers outside the proposed slice
    ///   require a single atomic PR.
    /// - `needs_spike` — graph is stale/missing; results are
    ///   unreliable; run a tech spike first.
    pub recommendation: String,
    /// `true` when the graph cache is missing or stale relative to
    /// the caller's HEAD, making results unreliable.
    #[serde(default)]
    pub low_confidence: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<crate::tools::graph_tools::CoverageAdvisory>,
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
    Flow(FlowResponse),
    /// Crate-level dependency graph: workspace crates as nodes,
    /// aggregated cross-crate references as edges.
    CrateGraph(CrateGraphOpResponse),
    /// Advisory impact preflight: which crates/files/symbols would
    /// break if proposed removals/renames land, and whether the
    /// proposed slice is safe to ship independently.
    ImpactCheck(ImpactCheckResponse),
    /// glqk: index-coverage table — per-(workspace, language) outcome +
    /// extent, per-language rollup, and discovered-but-unindexed source
    /// roots. Read from `project_workspace_coverage` WITHOUT loading the
    /// graph blob. Discriminator field `coverage`.
    Coverage(CoverageResponse),
}

// ── glqk: index-coverage contract ────────────────────────────────────────────

/// One row of the coverage table: the outcome + extent of one indexer against
/// one workspace. Mirrors a `project_workspace_coverage` row.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoverageWorkspaceEntry {
    pub workspace_slug: String,
    /// Stable language key (rust/typescript/python/…).
    pub language: String,
    /// Coverage enum: indexed | indexer_failed | timed_out |
    /// unsupported_language | excluded.
    pub status: String,
    /// `true` when `status` is a genuine gap (not `indexed`/`excluded`).
    pub is_gap: bool,
    /// Indexer exit detail (stderr tail / exit code / timeout reason).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Workspace root relative to the project root (empty for the repo root).
    pub workspace_root: String,
    /// Marker file(s) whose presence caused workspace detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker_evidence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovered_files: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_files: Option<i64>,
    pub commit_sha: String,
    pub warmed_at: String,
}

/// Per-language rollup across all workspaces.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoverageLanguageRollup {
    pub language: String,
    pub workspaces_total: usize,
    pub workspaces_indexed: usize,
    pub workspaces_gap: usize,
    /// Sum of `discovered_files` across this language's workspaces (when known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovered_files: Option<i64>,
    /// Sum of `indexed_files` across this language's workspaces (when known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_files: Option<i64>,
}

/// A workspace root that was discovered but produced no index (the coverage gap
/// stated as a source root, for direct UI/agent consumption).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UnindexedSourceRoot {
    pub workspace_slug: String,
    pub language: String,
    pub workspace_root: String,
    pub status: String,
}

/// Response for the `coverage` op. Cheap — never loads the graph blob.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoverageResponse {
    /// Discriminator so the untagged enum resolves to this variant.
    pub coverage: bool,
    /// `true` when at least one in-scope workspace is a genuine coverage gap.
    pub has_gaps: bool,
    pub workspaces: Vec<CoverageWorkspaceEntry>,
    pub language_rollup: Vec<CoverageLanguageRollup>,
    pub unindexed_source_roots: Vec<UnindexedSourceRoot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_staleness: Option<crate::tools::graph_tools::GraphStaleness>,
}

/// glqk: compact coverage advisory attached to `dead_symbols`, `orphans`,
/// `impact`, `impact_check`, `search`, and `api_surface` when a genuine gap
/// exists — mirroring how `graph_staleness` rides along. Absent when coverage
/// is clean or every non-indexed workspace is intentionally `excluded`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoverageAdvisory {
    /// Human-readable one-liner naming the gap, safe to lift into a prompt.
    pub message: String,
    /// The workspaces that are not indexed (gaps only).
    pub unindexed_workspaces: Vec<CoverageAdvisoryWorkspace>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoverageAdvisoryWorkspace {
    pub workspace_slug: String,
    pub language: String,
    /// indexer_failed | timed_out | unsupported_language.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
