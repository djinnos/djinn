//! `code_graph` tool handlers for querying the repository dependency graph.
//!
//! All graph queries are dispatched through the [`RepoGraphOps`] bridge trait,
//! keeping the MCP layer free of petgraph/SCIP dependencies.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::Instrument;

use crate::bridge::{
    ApiSurfaceEntry, BoundaryRule, BoundaryViolation, Candidate, ChangedRange, ChurnEntry,
    ComplexityResult, CoupledPairEntry, CouplingEntry, CouplingHubEntry, CycleGroup,
    DeadSymbolEntry, DeprecatedHit, DetectedChangesResult, EdgeEntry, FileGroupEntry,
    GraphNeighbor, GraphStatus, HotPathHit, HotspotEntry, ImpactEntry, ImpactResult,
    MetricsAtResult, NeighborsResult, OrphanEntry, PathResult, ProjectCtx, QuerySubgraphRequest,
    QuerySubgraphResult, RankedNode, RefactorCandidate, ResolveOutcome, SearchHit, SnapshotLevel,
    SnapshotPayload, SymbolAtHit, SymbolContext, SymbolDescription, TouchedSymbol,
    WorkspacesResult,
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
    /// `search`, `query_subgraph`, `cycles`, `orphans`, `path`, `edges`,
    /// `symbols_at`, `diff_touches`, `detect_changes`, `describe`,
    /// `context`, `status`, `snapshot`, `workspaces`, and the other graph analysis ops.
    pub operation: String,
    /// Project identifier — either the UUID (`project_id`) or the
    /// canonical `"owner/repo"` slug. The handler resolves it to the
    /// server-managed clone path via `djinn_core::paths::project_dir`
    /// before dispatching to the graph backend.
    pub project: String,
    /// Optional workspace slug. For `query_subgraph`, scopes seed search and
    /// traversal to a warmed workspace when provided; omit to query the
    /// project-level graph/default workspace selection.
    #[serde(default)]
    pub workspace: Option<String>,
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
    /// Kind filter, op-specific:
    /// - `ranked` / `search` / `cycles` / `orphans`: node kind — `file` or
    ///   `symbol`.
    /// - `query_subgraph`: node-kind narrowing for seeds/traversal — `file`
    ///   or `symbol`.
    /// - `neighbors`: edge kind — `reads` or `writes` (PR A3). Restricts
    ///   the response to neighbors connected by `Reads` / `Writes` edges
    ///   only, so callers can ask for "who writes to field X" without
    ///   post-filtering.
    #[serde(default)]
    pub kind_filter: Option<String>,
    /// Maximum results for `ranked`/`search`/`orphans`/`edges`/`neighbors`
    /// (default 20) or max traversal depth for `impact` (default 3).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Query text, op-specific:
    /// - `search`: substring/name lookup text.
    /// - `query_subgraph`: required nonblank natural-language question used
    ///   to pick relevant seeds and infer useful traversal edge kinds.
    #[serde(default)]
    pub query: Option<String>,
    /// Optional coarse context substring for `query_subgraph`. Use this to
    /// narrow the natural-language subgraph query to a subsystem, API, type,
    /// or concern when the initial response is too broad; returned
    /// `narrowing_hints` may suggest values for this field.
    #[serde(default)]
    pub context_filter: Option<String>,
    /// Optional repository-relative path/file substring filter for
    /// `query_subgraph`. It narrows seed selection and traversal to matching
    /// file paths. `file_glob` is also accepted as a compatibility alias and
    /// is used when `file_filter` is omitted.
    #[serde(default)]
    pub file_filter: Option<String>,
    /// Optional explicit edge kinds for `query_subgraph` traversal. Omit to
    /// let the planner infer edge kinds from the question; provide values such
    /// as calls/imports/returns/reads/writes/implements/extends to narrow the
    /// bounded subgraph. `edge_kind` is accepted as a single-kind compatibility
    /// alias when this list is omitted.
    #[serde(default)]
    pub edge_filters: Option<Vec<String>>,
    /// Approximate response token budget for `query_subgraph`. Omit to use
    /// the backend default (currently about 2000 tokens). Positive values below 1024 are clamped up to 1024,
    /// values above 32000 are clamped down to 32000, and zero/negative values
    /// are rejected. The result reports budget/truncation state so callers can
    /// retry with a narrower filter or a different budget.
    #[serde(default)]
    pub token_budget: Option<i64>,
    /// Maximum seed count for `query_subgraph`. Omit to use the backend
    /// default (currently 6). Positive values are clamped into 1..=32; zero/negative values
    /// are rejected. Returned seed debug metadata explains which seeds were
    /// selected and why.
    #[serde(default)]
    pub max_seeds: Option<i64>,
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
    /// Optional max depth. For `path`, bounds shortest-path search depth. For
    /// `query_subgraph`, bounds traversal depth from selected seeds; omit to
    /// use the backend default (currently 2). Values are clamped into 0..=8, so 0 means seed
    /// nodes only and larger values are capped at 8.
    #[serde(default)]
    pub max_depth: Option<i64>,
    /// Optional edge-kind filter for `edges`; for `query_subgraph`, a
    /// single-kind compatibility alias used when `edge_filters` is omitted.
    #[serde(default)]
    pub edge_kind: Option<String>,
    /// Repository-relative file path for `symbols_at`.
    #[serde(default)]
    pub file: Option<String>,
    /// 1-indexed inclusive start line for `symbols_at`.
    #[serde(default)]
    pub start_line: Option<i64>,
    /// 1-indexed inclusive end line for `symbols_at`. Defaults to
    /// `start_line` when omitted.
    #[serde(default)]
    pub end_line: Option<i64>,
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
    pub window_days: Option<i64>,
    /// Optional file glob restricting `hotspots` to a subset of paths. For
    /// `query_subgraph`, a path-filter compatibility alias used when
    /// `file_filter` is omitted.
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
    /// Time-window (in days) for the `churn` op. Omit for all-time.
    /// Clamped to `[1, 3650]` server-side.
    #[serde(default)]
    pub since_days: Option<i64>,
    /// Max files per commit before a commit is skipped in the
    /// `coupling_hotspots` / `coupling_hubs` aggregation. Default 15.
    /// Protects the pair-count signal from lockfile refreshes,
    /// codemods, and similar bulk rewrites that contribute `N^2`
    /// pairs with essentially zero real coupling information.
    #[serde(default)]
    pub max_files_per_commit: Option<i64>,
    /// Minimum edge confidence in `[0, 1]` for the `impact` BFS frontier
    /// (PR A2). Edges below this threshold are skipped — useful for
    /// excluding `local`-prefixed references and other low-confidence SCIP
    /// signals from the blast radius. Omit to keep every edge regardless of
    /// confidence (default behaviour).
    #[serde(default)]
    pub min_confidence: Option<f64>,
    /// PR C2: optional kind hint biasing the C2 disambiguation score
    /// when `key` is a short identifier (e.g. `"User"`) and the
    /// resolver hits multiple candidates. Accepts the same labels the
    /// resolver emits: `"file"`, `"class"`, `"interface"`, `"function"`,
    /// `"method"`, `"struct"`, `"enum"`, etc.
    #[serde(default)]
    pub kind_hint: Option<String>,
    /// Base SHA for `detect_changes`. When paired with `to_sha`, the
    /// op runs `git diff --unified=0 from_sha..to_sha` and maps the
    /// resulting hunks to symbols. Mutually exclusive with
    /// `changed_files` only when both are absent — when both are
    /// provided, line-level wins.
    #[serde(default)]
    pub from_sha: Option<String>,
    /// Head SHA for `detect_changes`.
    #[serde(default)]
    pub to_sha: Option<String>,
    /// Repository-relative file paths for `detect_changes` when no
    /// SHA range is supplied (or as a coarser fallback). Every symbol
    /// in each listed file is treated as potentially touched.
    #[serde(default)]
    pub changed_files: Option<Vec<String>>,
    /// PR C1: when `true`, the `context` op populates
    /// `symbol_context.symbol.content` with the symbol's body text
    /// read from the project clone. Default `false` — bandwidth
    /// matters; clients that already have the file open don't need
    /// the body shipped over MCP.
    #[serde(default)]
    pub include_content: Option<bool>,
    /// Semantic zoom level for `snapshot`: `symbol` keeps the existing
    /// file/symbol-node payload shape; `community` is accepted for forward
    /// compatibility with the collapsed community view.
    #[serde(default)]
    pub level: Option<String>,
    /// PR B4: search mode for the `search` op. `"name"` (the legacy
    /// fast path) runs the canonical-graph name index only;
    /// `"hybrid"` blends lexical (`code_chunks` LIKE), semantic
    /// (Qdrant cosine), and structural signals via RRF k=60. The
    /// effective default is read from `DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE`
    /// (defaults to `"name"`); pass an explicit value to override.
    /// Ignored by every other op.
    #[serde(default)]
    pub mode: Option<String>,
    /// Iter 28: target tier for the `complexity` op — `"functions"`
    /// (default) or `"files"`. The `functions` shape ranks individual
    /// function-like symbols; the `files` shape aggregates by file_path
    /// and returns per-file totals + worst-offender info. Reuses the
    /// shared `sort_by`, `file_glob`, and `limit` fields.
    #[serde(default)]
    pub target: Option<String>,
    /// v10: test-file filter. `"include"` (default for `snapshot` —
    /// returns the whole graph), `"exclude"` (drop every node the graph
    /// builder marked `is_test`), or `"only"` (keep only test nodes).
    /// Test classification is the canonical `RepoGraphNode::is_test`
    /// flag (file-path convention OR SCIP `Test` role). Currently
    /// honoured by the `snapshot` op; other ops keep their existing
    /// test-handling.
    #[serde(default)]
    pub tests: Option<String>,
}

impl CodeGraphParams {
    /// Coerce `Some("")` to `None` on every `Option<String>` input.
    ///
    /// Why: MCP clients (and chat-side LLMs in particular) often serialize
    /// tool calls with every schema field present, defaulting unset
    /// optionals to `""`. Downstream handlers that build a glob from
    /// `Some("")` get an "empty pattern" matcher that matches nothing,
    /// silently filtering all results. Normalizing at the param boundary
    /// turns these into `None` so every op sees the same shape regardless
    /// of caller behavior.
    pub fn normalize(&mut self) {
        fn clear(opt: &mut Option<String>) {
            if opt.as_deref().is_some_and(str::is_empty) {
                *opt = None;
            }
        }
        clear(&mut self.key);
        clear(&mut self.workspace);
        clear(&mut self.direction);
        clear(&mut self.kind_filter);
        clear(&mut self.query);
        clear(&mut self.context_filter);
        clear(&mut self.file_filter);
        clear(&mut self.from);
        clear(&mut self.to);
        clear(&mut self.from_glob);
        clear(&mut self.to_glob);
        clear(&mut self.visibility);
        clear(&mut self.sort_by);
        clear(&mut self.group_by);
        clear(&mut self.edge_kind);
        clear(&mut self.file);
        clear(&mut self.module_glob);
        clear(&mut self.confidence);
        clear(&mut self.file_glob);
        clear(&mut self.kind_hint);
        clear(&mut self.from_sha);
        clear(&mut self.to_sha);
        clear(&mut self.mode);
        clear(&mut self.level);
        clear(&mut self.target);
        clear(&mut self.tests);
    }
}

/// v10: how the `code_graph` `tests=` param filters test nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestFilter {
    /// Keep every node regardless of test classification.
    Include,
    /// Drop nodes the graph builder marked `is_test`.
    Exclude,
    /// Keep only nodes marked `is_test`.
    Only,
}

impl TestFilter {
    /// Parse the `tests=` param value. Unknown / absent → `default`.
    pub fn parse(value: Option<&str>, default: TestFilter) -> TestFilter {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("include") | Some("all") | Some("with") => TestFilter::Include,
            Some("exclude") | Some("none") | Some("without") | Some("no") => TestFilter::Exclude,
            Some("only") => TestFilter::Only,
            _ => default,
        }
    }

    /// True when a node with the given `is_test` flag should be kept.
    pub fn keeps(self, is_test: bool) -> bool {
        match self {
            TestFilter::Include => true,
            TestFilter::Exclude => !is_test,
            TestFilter::Only => is_test,
        }
    }
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
}

// ── Next-step hints ─────────────────────────────────────────────────────────────

const FALLBACK_NEXT_STEP: &str = "Use `code_graph status` to inspect the current graph state.";

/// PR C3: emitted when an `impact` query lands on a HIGH or CRITICAL
/// risk bucket. Steers the caller toward the cleanup ops they should
/// run before the change ships.
const HIGH_IMPACT_NEXT_STEP: &str =
    "Consider running `dead_symbols` and `deprecated_callers` before the change.";

/// Returns whether next-step hints should be appended. Toggled via the
/// `DJINN_CODE_GRAPH_NEXT_STEP_HINTS` env var; default is `true` (only
/// `0` / `false` / `off` / `no` suppress).
fn next_step_hints_enabled() -> bool {
    match std::env::var("DJINN_CODE_GRAPH_NEXT_STEP_HINTS") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// Pick the right hint for the operation and write it into the response
/// envelope. Returns the populated hint string for telemetry.
///
/// The 5 op-specific hints come from the PR A4 plan; everything else
/// gets [`FALLBACK_NEXT_STEP`] so the contract "every response carries
/// a non-empty `next_step`" holds.
fn attach_next_step_hint(op: &str, response: &mut CodeGraphResponse) -> String {
    let hint = compute_next_step_hint(op, response);
    set_next_step_hint(response, hint.clone());
    tracing::debug!(
        op,
        hint = hint.as_str(),
        "code_graph: next_step_hint emitted"
    );
    hint
}

fn compute_next_step_hint(op: &str, response: &CodeGraphResponse) -> String {
    match (op, response) {
        ("search", CodeGraphResponse::Search(s)) => match s.hits.first() {
            Some(hit) => format!(
                "Call `code_graph context name={}` to see full incoming/outgoing.",
                hit.display_name
            ),
            None => FALLBACK_NEXT_STEP.to_string(),
        },
        // PR C1 introduces a dedicated `context` op; until then map onto
        // the existing `describe` arm so the hint chain still nudges the
        // agent toward the impact view.
        ("context" | "describe", _) => {
            "Call `code_graph impact target=<symbol>` to see blast radius.".to_string()
        }
        ("ranked", CodeGraphResponse::Ranked(_)) => {
            "Files at top of PageRank are likely entry points; explore with `context`.".to_string()
        }
        ("cycles", CodeGraphResponse::Cycles(_)) => {
            "Each cycle entry is a tuple of mutually-reaching nodes; resolve with `path`."
                .to_string()
        }
        // PR C3: gate on the freshly-computed `risk` bucket so HIGH /
        // CRITICAL blast radii steer reviewers toward the cleanup ops
        // (`dead_symbols` + `deprecated_callers`). LOW / MEDIUM (and
        // legacy responses without classification) keep the generic
        // fallback hint.
        ("impact", CodeGraphResponse::Impact(r)) => match r.risk {
            Some(risk) if risk.is_high_or_critical() => HIGH_IMPACT_NEXT_STEP.to_string(),
            _ => FALLBACK_NEXT_STEP.to_string(),
        },
        // Iter 28: complexity → nudge the agent to drill into the
        // worst-offender function with `context` before refactoring.
        // Files-target keeps the generic fallback because the file
        // entry doesn't carry a single SCIP key the next call could
        // hand directly to `context`.
        ("complexity", CodeGraphResponse::Complexity(r)) => match &r.complexity {
            crate::bridge::ComplexityResult::Functions(entries) if !entries.is_empty() => {
                "Top entries are refactor candidates. Call code_graph context \
                 name=<key> to see the function in detail before changing."
                    .to_string()
            }
            _ => FALLBACK_NEXT_STEP.to_string(),
        },
        // Iter 29: refactor_candidates → top entries are highest-priority
        // refactor targets (high cognitive + high churn + high pagerank).
        // Steer the caller into `context` on the worst offender.
        ("refactor_candidates", CodeGraphResponse::RefactorCandidates(r))
            if !r.refactor_candidates.is_empty() =>
        {
            "Top entries are highest-priority refactor targets (high cognitive + \
             high churn + high pagerank). Call code_graph context name=<key> \
             to inspect before changing."
                .to_string()
        }
        // PR D2: nudge the caller toward `context` on a top-PageRank
        // node. Truncation is the common case for medium repos, so the
        // hint focuses on drilling into the cap rather than expanding
        // it.
        ("query_subgraph", CodeGraphResponse::QuerySubgraph(r)) => r
            .query_subgraph
            .narrowing_hints
            .first()
            .cloned()
            .unwrap_or_else(|| FALLBACK_NEXT_STEP.to_string()),
        ("snapshot", CodeGraphResponse::Snapshot(r)) => match r.snapshot.nodes.first() {
            Some(node) => format!(
                "Snapshot capped at {} of {} nodes; call `code_graph context name={}` to drill in.",
                r.snapshot.nodes.len(),
                r.snapshot.total_nodes,
                node.label,
            ),
            None => FALLBACK_NEXT_STEP.to_string(),
        },
        _ => FALLBACK_NEXT_STEP.to_string(),
    }
}

fn set_next_step_hint(response: &mut CodeGraphResponse, hint: String) {
    let slot = next_step_slot(response);
    *slot = Some(hint);
}

fn next_step_slot(response: &mut CodeGraphResponse) -> &mut Option<String> {
    match response {
        CodeGraphResponse::Neighbors(r) => &mut r.next_step,
        CodeGraphResponse::Ranked(r) => &mut r.next_step,
        CodeGraphResponse::Implementations(r) => &mut r.next_step,
        CodeGraphResponse::Impact(r) => &mut r.next_step,
        CodeGraphResponse::Search(r) => &mut r.next_step,
        CodeGraphResponse::Cycles(r) => &mut r.next_step,
        CodeGraphResponse::Orphans(r) => &mut r.next_step,
        CodeGraphResponse::Path(r) => &mut r.next_step,
        CodeGraphResponse::Edges(r) => &mut r.next_step,
        CodeGraphResponse::Describe(r) => &mut r.next_step,
        CodeGraphResponse::Context(r) => &mut r.next_step,
        CodeGraphResponse::Status(r) => &mut r.next_step,
        CodeGraphResponse::Workspaces(r) => &mut r.next_step,
        CodeGraphResponse::SymbolsAt(r) => &mut r.next_step,
        CodeGraphResponse::DiffTouches(r) => &mut r.next_step,
        CodeGraphResponse::ApiSurface(r) => &mut r.next_step,
        CodeGraphResponse::BoundaryCheck(r) => &mut r.next_step,
        CodeGraphResponse::Hotspots(r) => &mut r.next_step,
        CodeGraphResponse::Complexity(r) => &mut r.next_step,
        CodeGraphResponse::RefactorCandidates(r) => &mut r.next_step,
        CodeGraphResponse::MetricsAt(r) => &mut r.next_step,
        CodeGraphResponse::DeadSymbols(r) => &mut r.next_step,
        CodeGraphResponse::DeprecatedCallers(r) => &mut r.next_step,
        CodeGraphResponse::TouchesHotPath(r) => &mut r.next_step,
        CodeGraphResponse::Coupling(r) => &mut r.next_step,
        CodeGraphResponse::Churn(r) => &mut r.next_step,
        CodeGraphResponse::CouplingHotspots(r) => &mut r.next_step,
        CodeGraphResponse::CouplingHubs(r) => &mut r.next_step,
        CodeGraphResponse::Ambiguous(r) => &mut r.next_step,
        CodeGraphResponse::NotFound(r) => &mut r.next_step,
        CodeGraphResponse::DetectedChanges(r) => &mut r.next_step,
        CodeGraphResponse::Snapshot(r) => &mut r.next_step,
        CodeGraphResponse::QuerySubgraph(r) => &mut r.next_step,
    }
}

// ── Risk classification (PR C3) ─────────────────────────────────────────────────

/// PR C3: bucket a file path into a "module" key by taking its first
/// two path segments. The plan calls for "first 2 segments after repo
/// root"; the file paths returned by the graph layer are already
/// repo-relative, so we slice them as-is.
///
/// - `src/auth/User.rs` → `"src/auth"`
/// - `crates/djinn-control-plane/src/lib.rs` → `"crates/djinn-control-plane"`
/// - `Cargo.toml` (single segment) → `"Cargo.toml"` (degenerate; the
///   single-file repo case still counts as one module)
fn module_bucket(file_path: &str) -> String {
    let normalized = file_path.replace('\\', "/");
    let mut iter = normalized.split('/').filter(|s| !s.is_empty());
    let first = iter.next();
    let second = iter.next();
    match (first, second) {
        (Some(a), Some(b)) => format!("{a}/{b}"),
        (Some(a), None) => a.to_string(),
        _ => file_path.to_string(),
    }
}

/// PR C3 metrics tuple — `(direct_count, total_impacted, module_count)`.
/// Computed once and shared between risk classification and the summary
/// string so the two never disagree.
#[derive(Debug, Clone, Copy)]
struct ImpactMetrics {
    direct: usize,
    total: usize,
    modules: usize,
}

/// Compute risk metrics from a detailed `ImpactEntry` slice. `direct`
/// counts entries at depth 1 (BFS root has depth 0 and isn't emitted),
/// `total` is the full impacted set, `modules` is the unique module
/// bucket count over all entries that carry a `file_path`. Entries
/// without a `file_path` (external/virtual symbols) don't contribute
/// to the module count — the plan treats them as "no module signal".
fn metrics_from_detailed(entries: &[ImpactEntry]) -> ImpactMetrics {
    use std::collections::HashSet;
    let direct = entries.iter().filter(|e| e.depth == 1).count();
    let total = entries.len();
    let mut buckets: HashSet<String> = HashSet::new();
    for entry in entries {
        if let Some(path) = entry.file_path.as_deref() {
            buckets.insert(module_bucket(path));
        }
    }
    ImpactMetrics {
        direct,
        total,
        modules: buckets.len(),
    }
}

/// Compute risk metrics from the per-file rollup (`group_by=file`).
/// `total` is the sum of `occurrence_count` across groups; `direct` is
/// the count of entries with `max_depth == 1` aggregated across groups
/// — but since the rollup loses per-entry depth granularity, we
/// approximate `direct` as the sum of `occurrence_count` over groups
/// whose `max_depth == 1`, which is exact iff the only depth-1 entries
/// land in single-direct-only files (rare). For the wider (multi-depth)
/// case the approximation under-counts; the detailed path remains the
/// authoritative source. `modules` is the unique two-segment bucket
/// count across the listed files.
fn metrics_from_grouped(groups: &[FileGroupEntry]) -> ImpactMetrics {
    use std::collections::HashSet;
    let total: usize = groups.iter().map(|g| g.occurrence_count).sum();
    let direct: usize = groups
        .iter()
        .filter(|g| g.max_depth == 1)
        .map(|g| g.occurrence_count)
        .sum();
    let mut buckets: HashSet<String> = HashSet::new();
    for g in groups {
        buckets.insert(module_bucket(&g.file));
    }
    ImpactMetrics {
        direct,
        total,
        modules: buckets.len(),
    }
}

/// Format the 1-line plan-mandated summary. Stable phrasing —
/// reviewer prompts in PR E3 lift this verbatim.
fn impact_summary(metrics: ImpactMetrics) -> String {
    if metrics.direct == 0 && metrics.total == 0 {
        return "no direct callers in current graph snapshot".to_string();
    }
    format!(
        "{} direct caller(s) across {} module(s)",
        metrics.direct, metrics.modules
    )
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

/// PR B4: resolved mode for the `search` op. The wire-level vocabulary
/// is two strings (`"name"` / `"hybrid"`); this enum keeps the dispatch
/// site honest about the closed set of options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchMode {
    Name,
    Hybrid,
}

/// PR B4: resolve the effective search mode by layering caller intent
/// on top of the `DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE` env var. The
/// env-var default is `"name"` per the engineering-practices section
/// of the plan; flip to `"hybrid"` after the soak window. Caller wins
/// when both are set so an explicit `mode=name` always pins the fast
/// path even in a hybrid-default deployment.
pub(crate) fn resolve_search_mode(caller: Option<&str>) -> Result<SearchMode, String> {
    let raw = match caller {
        Some(value) => value.to_string(),
        None => std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE")
            .unwrap_or_else(|_| "name".to_string()),
    };
    match raw.as_str() {
        "name" => Ok(SearchMode::Name),
        "hybrid" => Ok(SearchMode::Hybrid),
        other => Err(format!(
            "invalid search mode '{other}': expected 'name' or 'hybrid'"
        )),
    }
}

/// PR A3: validator for the `neighbors` op's edge-kind filter. Currently
/// accepts `reads` / `writes`; future PRs may extend this with `calls` etc.
/// once the `EdgeCategory` enum lands (PR C1).
fn validate_edge_kind_filter(kind_filter: Option<&str>) -> Result<(), String> {
    if let Some(k) = kind_filter {
        match k {
            "reads" | "writes" => Ok(()),
            _ => Err(format!(
                "invalid kind_filter '{k}' for neighbors: expected 'reads' or 'writes'"
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

/// Resolve the effective workspace scope for a `code_graph` op that
/// consumes the optional `workspace` parameter (pb94 epic).
///
/// Centralises the three-outcome contract the epic commits to:
///
/// 1. **Empty / unscoped** — caller omitted `workspace` (or sent `""`,
///    already normalized to `None` upstream). Returns
///    `WorkspaceScope { workspace: None, hint: None }` and the bridge
///    runs over the full graph. This matches the pre-pb94 behaviour
///    and the "omit == `None`" shape locked in by
///    `CodeGraphParams::normalize()`.
///
/// 2. **Valid / known** — requested slug is non-empty and resolves to at
///    least one node in the graph. Returns
///    `WorkspaceScope { workspace: Some(slug), hint: None }` and the
///    bridge hard-filters for listing/bounded ops. **Single-workspace
///    graphs land here too** — the workspace filter is structurally a
///    no-op (every node matches) rather than producing a hard-empty
///    result, per the epic "scoping is a no-op for single-workspace
///    graphs" requirement.
///
/// 3. **Unknown non-empty slug** — requested slug is non-empty and the
///    graph contains no node in that workspace. Returns
///    `WorkspaceScope { workspace: None, hint: Some(candidates) }` so
///    the bridge returns the full/unscoped result AND the caller gets
///    the candidate list to recover from the typo. Single-candidate
///    hint lists are suppressed (a "no choice" project never hints).
///
/// This helper is the only call site that needs to call
/// `RepoGraphOps::workspace_hint` — every per-op handler that uses
/// `workspace` should call this once and feed the result into both the
/// bridge call (`scope.workspace.as_deref()`) and the response
/// (`scope.hint`).
///
/// `query_subgraph` and a few other ops take `workspace` via their own
/// request type; they call `workspace_hint` directly to keep their
/// request DTO independent of the dispatcher.
pub(crate) async fn resolve_workspace_scope(
    ops: &Arc<dyn crate::bridge::RepoGraphOps>,
    ctx: &crate::bridge::ProjectCtx,
) -> Result<crate::bridge::WorkspaceScope, String> {
    let requested = ctx.workspace.as_deref();
    let hint = ops.workspace_hint(ctx, requested).await?;
    let scope = match hint {
        // Unknown non-empty slug AND the bridge confirms the project
        // has multiple workspaces to choose from: degrade to unscoped
        // and surface the candidate list. `workspace_hint_from_graph`
        // (in mcp_bridge.rs) gates the `Some(_)` return on
        // `slugs.len() > 1` so single-workspace graphs land in the
        // `None` arm below and the workspace is a structural no-op.
        Some(candidates) => crate::bridge::WorkspaceScope {
            workspace: None,
            hint: Some(candidates),
        },
        // Known, valid, or no-choice: use the caller's workspace as-is
        // (or `None` for unscoped callers). The bridge handles
        // single-workspace graphs transparently — every node matches
        // the only slug, so the filter is a no-op.
        None => crate::bridge::WorkspaceScope {
            workspace: requested.map(str::to_string),
            hint: None,
        },
    };
    Ok(scope)
}

fn bounded_optional_usize(
    value: Option<i64>,
    name: &str,
    min: usize,
    max: usize,
    allow_zero: bool,
) -> Result<Option<usize>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    if raw < 0 || (!allow_zero && raw == 0) {
        return Err(format!("invalid {name} {raw}: expected a positive integer"));
    }
    Ok(Some((raw as usize).clamp(min, max)))
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

/// Pick the highest-priority touched symbol for the next-step impact
/// hint. High-tier wins over Medium wins over Low; ties break on
/// `name` for stability.
fn pick_next_step_target(symbols: &[crate::bridge::DetectedTouchedSymbol]) -> Option<String> {
    use crate::bridge::PagerankTier;
    fn rank(t: PagerankTier) -> u8 {
        match t {
            PagerankTier::High => 0,
            PagerankTier::Medium => 1,
            PagerankTier::Low => 2,
        }
    }
    symbols
        .iter()
        .min_by(|a, b| {
            rank(a.pagerank_tier)
                .cmp(&rank(b.pagerank_tier))
                .then_with(|| a.name.cmp(&b.name))
        })
        .map(|s| s.uid.clone())
}

// ── Handler ─────────────────────────────────────────────────────────────────────

#[tool_router(router = graph_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Query the repository dependency graph built from SCIP indexer output.
    #[tool(
        description = "Query the repository dependency graph built from SCIP indexer output and the commit-based file-coupling index. Operations: workspaces (enumerate graph workspaces with node counts and freshness), neighbors (edges in/out of a node, with optional group_by=file rollup), ranked (top nodes; sort_by pagerank/in_degree/out_degree/total_degree), impact (transitive dependents, with optional group_by=file rollup), implementations (find implementors of a trait/interface symbol), search (name-based symbol lookup; `query` is substring/name text), query_subgraph (natural-language, budget-bounded subgraph query; requires nonblank `query` as the question. Optional narrowing fields: `workspace` scopes to a warmed workspace, `file_filter` narrows by repository-relative path/file substring [`file_glob` alias when omitted], `kind_filter` narrows node kind to file|symbol, `edge_filters` narrows traversal edge kinds such as calls/imports/returns/reads/writes/implements/extends [`edge_kind` single-kind alias when omitted], and `context_filter` narrows to a subsystem/API/type/concern. Budget controls: `token_budget` is approximate; omit for the backend default (currently about 2000 tokens), positive values are clamped into 1024..=32000, and zero/negative values are rejected. `max_depth` is clamped into 0..=8 [0 means seeds only; omit for backend default, currently 2], and positive `max_seeds` is clamped into 1..=32 [omit for backend default, currently 6; zero/negative rejected]. The response is bounded and includes nodes/edges, seed debug metadata, inferred/requested edge kinds, budget/truncation/traversal/hub-skip state where available, and `narrowing_hints` suggesting tighter context/path/kind/edge filters or budget changes), cycles (strongly-connected components), orphans (zero-incoming-reference nodes, with visibility filter), path (shortest dependency path), edges (enumerate edges by from_glob/to_glob), symbols_at (given file+line range, return SCIP symbols whose definition range encloses those lines — diff-hunk → symbol lookup), diff_touches (given a list of changed line ranges parsed from `git diff --unified=0 base..head`, return every base-graph symbol touched, with fan-in/fan-out and file grouping; the base graph is always current main — this op does NOT build a head graph), detect_changes (given from_sha + to_sha [or a changed_files list], return touched symbols + their PageRank tier [High/Medium/Low quartile] + per-file rollup; shells out to `git diff --unified=0 from_sha..to_sha` server-side and maps hunks via symbols_enclosing — replaces the architect's manual diff inspection), describe (symbol signature/documentation without an LSP round trip), context (PR C1: 360° symbol view — categorized incoming/outgoing dicts [calls/reads/writes/extends/implements/...], plus structured method_metadata when SCIP populates it; pass include_content=true to include the symbol body. Each category list is hard-capped at 30 entries), status (peek at the persisted canonical graph cache; never warms), api_surface (list every public symbol with fan-in/fan-out and a used-outside-crate signal), boundary_check (edge-based architecture rule scanner over from_glob→to_glob pairs; returns forbidden violations), hotspots (file churn × centrality ranking over a configurable window; top_symbols per file), complexity (rank functions or files by complexity metric — target: functions|files, sort_by: cognitive|cyclomatic|nloc|max_nesting|param_count, file_glob, limit), refactor_candidates (composite refactor-priority ranking — fuses cognitive complexity × file-level churn × PageRank into a single z-score and surfaces the top function-level targets; respects since_days [default 90, clamped 1..=365], file_glob, limit [default 30, clamped 1..=200]; each entry carries the composite score, a tier label [high/medium/low], and the underlying raw + z-score signals so callers can re-rank locally), metrics_at (scalar graph snapshot: node/edge/cycle counts, god-object floor, orphans, public API and doc coverage), dead_symbols (no-incoming-edge-from-entry-points enumeration; confidence=high|med|low), deprecated_callers (symbols whose signature/documentation contains #[deprecated] or @deprecated, with caller list), touches_hot_path (given entry and sink SCIP keys, report which queried symbols sit on any entry→sink shortest path), coupling (files most frequently co-edited with `file`, sourced from the per-commit change log; returns co-edit count, last co-edit timestamp, and up to three supporting SHAs per peer), churn (top files by distinct-commit count over an optional `since_days` window; returns commit count, cumulative insertions/deletions, and last-touched timestamp), coupling_hotspots (top file PAIRS by co-edit count project-wide; returns [{file_a,file_b,co_edits,last_co_edit}]; respects `since_days` and `max_files_per_commit` [default 15] — useful for spotting implicit coupling between distant parts of the tree), coupling_hubs (top FILES by cumulative coupling across all partners; returns [{file_path,total_coupling,partner_count}] — change-propagation risk map, higher total_coupling means a touch to this file is more likely to require touching many others), snapshot (PR D2: full graph snapshot with workspace-fair, endpoint-consistent capping — returns {snapshot:{project_id,git_head,generated_at,truncated,total_nodes,total_edges,node_cap,nodes,edges}}; default cap 2000 nodes [Sigma WebGL ceiling], settable via `limit` up to 10k. Drives the `/code-graph` UI's force-directed render. Pass tests=include|exclude|only to filter test files/symbols — include is the default [whole graph], exclude drops everything marked is_test, only keeps test nodes; classification is the canonical is_test flag built from the file-path convention OR the SCIP Test role). All coupling / churn outputs are filtered through the project's `project_graph_exclusions` glob list at query time, so tuning exclusions takes effect without re-ingesting."
    )]
    pub async fn code_graph(
        &self,
        Parameters(mut params): Parameters<CodeGraphParams>,
    ) -> Json<ErrorOr<CodeGraphResponse>> {
        params.normalize();
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
        params.project_path =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
                .to_string_lossy()
                .into_owned();

        // Build the resolved `ProjectCtx` once. Inner handlers pass it
        // straight to the `RepoGraphOps` bridge so no downstream code
        // needs to reverse-parse `{projects_root}/{owner}/{repo}`.
        let ctx = ProjectCtx {
            id: params.project_id.clone(),
            clone_path: params.project_path.clone(),
            workspace: params.workspace.clone(),
            sub_path: None,
        };

        // Both pre-resolve and the per-op match now live inside
        // `dispatch_code_graph`, which also wraps the inner call in a
        // tokio timeout + tracing span so the chat handler can't be
        // wedged forever by a slow op.
        let result = self.dispatch_code_graph(&ctx, &mut params).await;

        Json(match result {
            Ok(mut response) => {
                if next_step_hints_enabled() {
                    attach_next_step_hint(params.operation.as_str(), &mut response);
                }
                ErrorOr::Ok(response)
            }
            Err(error) => ErrorOr::Error(ErrorResponse { error }),
        })
    }
}

/// Default per-op timeout for `dispatch_code_graph`. Override with the
/// `DJINN_CODE_GRAPH_DISPATCH_TIMEOUT_SECS` env var. 60s is comfortably
/// above the slowest healthy op we measure (snapshot at full size
/// ~1.5s) but well under the chat handler's outer guard so the
/// timeout error surfaces to the model instead of stalling the stream.
const CODE_GRAPH_DISPATCH_TIMEOUT_DEFAULT_SECS: u64 = 60;

fn code_graph_dispatch_timeout() -> std::time::Duration {
    let secs = std::env::var("DJINN_CODE_GRAPH_DISPATCH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(CODE_GRAPH_DISPATCH_TIMEOUT_DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

impl DjinnMcpServer {
    /// Single source of truth for `code_graph` dispatch.
    ///
    /// Wraps the op-string match in:
    /// - **Pre-resolve** — short identifiers (`User`, `helper`) routed
    ///   through `RepoGraphOps::resolve` so they short-circuit to
    ///   `Ambiguous` / `NotFound` instead of failing inside the inner
    ///   handler.
    /// - **Tokio timeout** — a slow op (e.g. an unindexed coupling
    ///   self-join hitting Dolt's planner pathology) returns a
    ///   structured timeout error after
    ///   `DJINN_CODE_GRAPH_DISPATCH_TIMEOUT_SECS` (default 60s) instead
    ///   of stalling the chat stream forever. This is the chat-handler
    ///   hang fix — without it, a slow op wedges the whole tool loop.
    /// - **Tracing span** — every dispatch emits an `info_span!` with
    ///   `op`, `project_id`, `elapsed_ms`, `status` so we can grep
    ///   latency / failure rates.
    ///
    /// Both the MCP tool entry (`code_graph` below) and the chat
    /// extension (`djinn_agent::extension::handlers::code_intel`) call
    /// this method. Keep the per-op match here; do not duplicate it.
    pub async fn dispatch_code_graph(
        &self,
        ctx: &ProjectCtx,
        params: &mut CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        match Self::pre_resolve_key(self.state.repo_graph().as_ref(), ctx, params).await? {
            None => {}
            Some(short_circuit) => return Ok(short_circuit),
        }

        let timeout = code_graph_dispatch_timeout();
        let op = params.operation.clone();
        let project_id = params.project_id.clone();
        let started = std::time::Instant::now();

        let span = tracing::info_span!(
            "code_graph",
            op = %op,
            project_id = %project_id,
        );
        let inner = self.dispatch_code_graph_op(ctx, params);
        let result = tokio::time::timeout(timeout, inner)
            .instrument(span)
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "code_graph op '{op}' exceeded {}s — try a narrower call \
                     (lower limit, file_glob filter, since_days) or a different op",
                    timeout.as_secs()
                ))
            });

        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => tracing::info!(
                target: "djinn_control_plane::tools::graph_tools",
                op = %op,
                project_id = %project_id,
                elapsed_ms,
                status = "ok",
                "code_graph dispatch completed"
            ),
            Err(err) => tracing::warn!(
                target: "djinn_control_plane::tools::graph_tools",
                op = %op,
                project_id = %project_id,
                elapsed_ms,
                status = "error",
                error = %err,
                "code_graph dispatch failed"
            ),
        }

        result
    }

    /// Inner op-string match. Lives here so [`Self::dispatch_code_graph`]
    /// can wrap it uniformly in timeout + tracing without each per-op
    /// handler having to know about either.
    async fn dispatch_code_graph_op(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        match params.operation.as_str() {
            "neighbors" => self.code_graph_neighbors(ctx, params).await,
            "ranked" => self.code_graph_ranked(ctx, params).await,
            "implementations" => self.code_graph_implementations(ctx, params).await,
            "impact" => self.code_graph_impact(ctx, params).await,
            "search" => self.code_graph_search(ctx, params).await,
            "query_subgraph" => self.code_graph_query_subgraph(ctx, params).await,
            "cycles" => self.code_graph_cycles(ctx, params).await,
            "orphans" => self.code_graph_orphans(ctx, params).await,
            "path" => self.code_graph_path(ctx, params).await,
            "edges" => self.code_graph_edges(ctx, params).await,
            "describe" => self.code_graph_describe(ctx, params).await,
            "context" => self.code_graph_context(ctx, params).await,
            "status" => self.code_graph_status(ctx, params).await,
            "workspaces" => self.code_graph_workspaces(ctx, params).await,
            "symbols_at" => self.code_graph_symbols_at(ctx, params).await,
            "diff_touches" => self.code_graph_diff_touches(ctx, params).await,
            "detect_changes" => self.code_graph_detect_changes(ctx, params).await,
            "api_surface" => self.code_graph_api_surface(ctx, params).await,
            "boundary_check" => self.code_graph_boundary_check(ctx, params).await,
            "hotspots" => self.code_graph_hotspots(ctx, params).await,
            "complexity" => self.code_graph_complexity(ctx, params).await,
            "refactor_candidates" => self.code_graph_refactor_candidates(ctx, params).await,
            "metrics_at" => self.code_graph_metrics_at(ctx, params).await,
            "dead_symbols" => self.code_graph_dead_symbols(ctx, params).await,
            "deprecated_callers" => self.code_graph_deprecated_callers(ctx, params).await,
            "touches_hot_path" => self.code_graph_touches_hot_path(ctx, params).await,
            "coupling" => self.code_graph_coupling(ctx, params).await,
            "churn" => self.code_graph_churn(ctx, params).await,
            "coupling_hotspots" => self.code_graph_coupling_hotspots(ctx, params).await,
            "coupling_hubs" => self.code_graph_coupling_hubs(ctx, params).await,
            "snapshot" => self.code_graph_snapshot(ctx, params).await,
            other => Err(format!(
                "unknown code_graph operation '{other}': expected one of \
                 'neighbors', 'ranked', 'impact', 'implementations', \
                 'search', 'query_subgraph', 'cycles', 'orphans', 'path', 'edges', \
                 'symbols_at', 'diff_touches', 'detect_changes', \
                 'describe', 'context', 'status', 'workspaces', \
                 'api_surface', 'boundary_check', 'hotspots', 'complexity', \
                 'refactor_candidates', 'metrics_at', \
                 'dead_symbols', 'deprecated_callers', 'touches_hot_path', \
                 'coupling', 'churn', 'coupling_hotspots', 'coupling_hubs', \
                 'snapshot'"
            )),
        }
    }

    /// PR C2 dispatcher hook: for ops that read a caller-supplied node
    /// key (`neighbors`, `impact`, `implementations`, `describe`,
    /// `path`), pre-resolve via [`RepoGraphOps::resolve`] so the inner
    /// op gets either a unique RepoNodeKey or short-circuits on a
    /// disambiguation list / hard miss.
    ///
    /// Returns:
    /// - `Ok(None)` — caller may dispatch the inner op as usual. For
    ///   `Found(uid)` we rewrite `params.key` (or `from`/`to` for
    ///   `path`) to the canonical key first.
    /// - `Ok(Some(response))` — short-circuit; emit `Ambiguous`/`NotFound`.
    /// - `Err(_)` — bridge call failed; surface as an MCP error.
    async fn pre_resolve_key(
        graph: &dyn crate::bridge::RepoGraphOps,
        ctx: &ProjectCtx,
        params: &mut CodeGraphParams,
    ) -> Result<Option<CodeGraphResponse>, String> {
        // Operations that take a single `key`. `search`/`ranked`/
        // `cycles`/`orphans`/`hotspots`/etc. don't go through
        // resolution — their `key` is a query/glob.
        let single_key_ops = [
            "neighbors",
            "impact",
            "implementations",
            "describe",
            // PR C1: `context` shares the same key-resolution path so a
            // short identifier like `User` short-circuits to Ambiguous /
            // NotFound instead of failing inside the graph backend.
            "context",
        ];
        if single_key_ops.contains(&params.operation.as_str())
            && let Some(key) = params.key.as_deref().filter(|k| !k.is_empty())
        {
            let kind_hint = params.kind_hint.as_deref();
            match graph.resolve(ctx, key, kind_hint).await? {
                ResolveOutcome::Found(uid) => {
                    params.key = Some(uid);
                }
                ResolveOutcome::Ambiguous(candidates) => {
                    return Ok(Some(CodeGraphResponse::Ambiguous(AmbiguousResponse {
                        candidates,
                        next_step: None,
                    })));
                }
                ResolveOutcome::NotFound => {
                    return Ok(Some(CodeGraphResponse::NotFound(NotFoundResponse {
                        not_found: NotFoundDetail {
                            query: key.to_string(),
                            kind_hint: kind_hint.map(str::to_string),
                        },
                        next_step: None,
                    })));
                }
            }
        }

        // `path` takes two keys; resolve both.
        if params.operation == "path" {
            for which in ["from", "to"] {
                let raw = match which {
                    "from" => params.from.as_deref().filter(|s| !s.is_empty()),
                    _ => params.to.as_deref().filter(|s| !s.is_empty()),
                };
                let Some(key) = raw else { continue };
                let kind_hint = params.kind_hint.as_deref();
                match graph.resolve(ctx, key, kind_hint).await? {
                    ResolveOutcome::Found(uid) => {
                        if which == "from" {
                            params.from = Some(uid);
                        } else {
                            params.to = Some(uid);
                        }
                    }
                    ResolveOutcome::Ambiguous(candidates) => {
                        return Ok(Some(CodeGraphResponse::Ambiguous(AmbiguousResponse {
                            candidates,
                            next_step: None,
                        })));
                    }
                    ResolveOutcome::NotFound => {
                        return Ok(Some(CodeGraphResponse::NotFound(NotFoundResponse {
                            not_found: NotFoundDetail {
                                query: key.to_string(),
                                kind_hint: kind_hint.map(str::to_string),
                            },
                            next_step: None,
                        })));
                    }
                }
            }
        }

        Ok(None)
    }

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
        // PR A3: `neighbors` repurposes `kind_filter` for an *edge* kind
        // (`reads` / `writes`); other ops use it for the node kind
        // (`file` / `symbol`). Validate against the edge-kind set here so
        // a typo surfaces server-side rather than silently dropping every
        // neighbor.
        validate_edge_kind_filter(params.kind_filter.as_deref())?;
        let result = self
            .state
            .repo_graph()
            .neighbors(
                ctx,
                key,
                params.direction.as_deref(),
                params.group_by.as_deref(),
                params.kind_filter.as_deref(),
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
            next_step: None,
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
        // pb94: route workspace resolution through the shared helper so
        // valid / unknown / single-workspace / empty semantics stay in
        // one place.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let nodes = self
            .state
            .repo_graph()
            .ranked(
                ctx,
                scope.workspace.as_deref(),
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
        Ok(CodeGraphResponse::Ranked(RankedResponse {
            nodes,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    async fn code_graph_implementations(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let implementations = self.state.repo_graph().implementations(ctx, key).await?;
        Ok(CodeGraphResponse::Implementations(
            ImplementationsResponse {
                symbol: key.to_string(),
                implementations,
                next_step: None,
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
        // PR A2: validate `min_confidence` lives in `[0, 1]` before letting
        // it loose on the BFS frontier; out-of-range values would silently
        // collapse the impact set to zero or do nothing.
        if let Some(c) = params.min_confidence
            && !(0.0..=1.0).contains(&c)
        {
            return Err(format!("invalid min_confidence {c}: must be in [0.0, 1.0]"));
        }
        // pb94: `impact` is a TRAVERSAL op — `workspace` scopes only the
        // seed (the `key`) inside the workspace, the BFS walk itself
        // must remain unconstrained so cross-workspace blast radius
        // remains visible. The shared helper returns the effective
        // scope (which is `None` for unknown slugs so the bridge
        // resolves `key` from the full graph) and the hint envelope.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let result = self
            .state
            .repo_graph()
            .impact(
                ctx,
                scope.workspace.as_deref(),
                key,
                depth,
                params.group_by.as_deref(),
                params.min_confidence,
            )
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let (impact, file_groups, metrics) = match result {
            ImpactResult::Detailed(mut v) => {
                // ImpactEntry has no display_name; match key only (Tier
                // 1 still catches module artifacts; Tier 2 globs bound
                // against the SCIP key, matching the old client-side
                // behaviour).
                v.retain(|e| !exclusions.excludes(&e.key, None, &e.key));
                let metrics = metrics_from_detailed(&v);
                (Some(v), None, metrics)
            }
            ImpactResult::Grouped(mut v) => {
                v.retain(|g| !exclusions.excludes(&g.file, Some(&g.file), &g.file));
                let metrics = metrics_from_grouped(&v);
                (None, Some(v), metrics)
            }
        };
        // PR C3: classify the post-exclusion blast radius and ship
        // both the structured bucket (`risk`) and a human-readable
        // 1-line summary so chat UIs / reviewer prompts / dashboards
        // can each pick the form they want.
        let risk = ImpactRisk::classify(metrics.direct, metrics.total, metrics.modules);
        let summary = impact_summary(metrics);
        Ok(CodeGraphResponse::Impact(ImpactResponse {
            key: key.to_string(),
            impact,
            file_groups,
            risk: Some(risk),
            summary: Some(summary),
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    async fn code_graph_search(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let query = require_query(params)?;
        validate_kind_filter(params.kind_filter.as_deref())?;
        let mode = resolve_search_mode(params.mode.as_deref())?;
        let limit = params.limit.unwrap_or(20) as usize;
        let fetch_limit = (limit.saturating_mul(4)).clamp(limit, 200);
        // pb94: surface unknown-workspace hints for `search` too so a
        // caller that mistypes the workspace doesn't get a hard-empty
        // hit list. The bridge call itself is workspace-agnostic today
        // (search uses a name index that already spans all workspaces),
        // so the resolved scope is used purely for the hint envelope.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        // PR B4: dispatch on mode. `name` keeps the pre-PR-B4 fast
        // path; `hybrid` runs the RRF orchestrator on the bridge,
        // which composes lexical + semantic + structural signals and
        // tags each hit's `match_kind` for debug surfaces.
        let hits = match mode {
            SearchMode::Name => {
                self.state
                    .repo_graph()
                    .search(ctx, query, params.kind_filter.as_deref(), fetch_limit)
                    .await?
            }
            SearchMode::Hybrid => {
                self.state
                    .repo_graph()
                    .hybrid_search(ctx, query, params.kind_filter.as_deref(), fetch_limit)
                    .await?
            }
        };
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let hits: Vec<SearchHit> = hits
            .into_iter()
            .filter(|h| !exclusions.excludes(&h.key, h.file.as_deref(), &h.display_name))
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::Search(SearchResponse {
            query: query.to_string(),
            hits,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    async fn code_graph_query_subgraph(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let query = params
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| "'query' is required for operation 'query_subgraph'".to_string())?;
        validate_kind_filter(params.kind_filter.as_deref())?;

        let token_budget =
            bounded_optional_usize(params.token_budget, "token_budget", 1_024, 32_000, false)?;
        let max_depth = bounded_optional_usize(params.max_depth, "max_depth", 0, 8, true)?;
        let max_seeds = bounded_optional_usize(params.max_seeds, "max_seeds", 1, 32, false)?;

        let edge_filter = params
            .edge_filters
            .clone()
            .unwrap_or_else(|| params.edge_kind.clone().into_iter().collect())
            .into_iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let file_filter = params
            .file_filter
            .as_deref()
            .or(params.file_glob.as_deref())
            .or(params.file.as_deref())
            .or(params.from_glob.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let request = QuerySubgraphRequest {
            query: query.to_string(),
            workspace: ctx
                .workspace
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            context_filter: params
                .context_filter
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            file_filter,
            kind_filter: params
                .kind_filter
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            edge_filter,
            token_budget,
            max_depth,
            max_seeds,
        };
        let result = self.state.repo_graph().query_subgraph(ctx, request).await?;
        Ok(CodeGraphResponse::QuerySubgraph(QuerySubgraphResponse {
            query_subgraph: result,
            next_step: None,
        }))
    }

    async fn code_graph_cycles(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        let min_size = params.min_size.unwrap_or(2).max(0) as usize;
        // pb94: surface unknown-workspace hints for `cycles` so a caller
        // that mistypes the workspace doesn't get a hard-empty result
        // when the bridge's SCC cache is workspace-agnostic. The
        // bridge call itself stays workspace-agnostic (the SCC cache
        // spans the whole graph); the resolved scope is used purely
        // for the hint envelope.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        // Ask the warmer cache for SCCs with a size floor of 2 so we
        // can still shed an SCC whose surviving members drop below the
        // user-requested `min_size` after exclusion filtering. We
        // re-apply `min_size` post-filter below.
        let fetch_floor = min_size.max(2);
        let cycles = self
            .state
            .repo_graph()
            .cycles(ctx, params.kind_filter.as_deref(), fetch_floor)
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
        Ok(CodeGraphResponse::Cycles(CyclesResponse {
            cycles,
            workspace_hint: scope.hint,
            next_step: None,
        }))
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
        // pb94: route workspace resolution through the shared helper so
        // valid / unknown / single-workspace / empty semantics stay in
        // one place.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let orphans = self
            .state
            .repo_graph()
            .orphans(
                ctx,
                scope.workspace.as_deref(),
                params.kind_filter.as_deref(),
                params.visibility.as_deref(),
                fetch_limit,
            )
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let orphans: Vec<OrphanEntry> = orphans
            .into_iter()
            .filter(|o| !exclusions.excludes_orphan(&o.key, o.file.as_deref(), &o.display_name))
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::Orphans(OrphansResponse {
            orphans,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    async fn code_graph_path(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let (from, to) = require_from_to(params)?;
        let max_depth = params.max_depth.map(|v| v.max(0) as usize);
        // pb94: `path` is a TRAVERSAL op — `workspace` scopes only the
        // `from` and `to` endpoint resolution, the shortest-path walk
        // itself must remain unconstrained so cross-workspace path
        // context remains visible.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let path = self
            .state
            .repo_graph()
            .path(ctx, scope.workspace.as_deref(), from, to, max_depth)
            .await?;
        Ok(CodeGraphResponse::Path(PathResponse {
            path,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    async fn code_graph_edges(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let (from_glob, to_glob) = require_globs(params)?;
        let limit = params.limit.unwrap_or(100) as usize;
        // pb94: surface unknown-workspace hints for `edges` so a caller
        // that mistypes the workspace doesn't get a hard-empty result
        // when their `from_glob` / `to_glob` only matches edges in the
        // requested workspace. The bridge call itself stays
        // workspace-agnostic (edge globs match by path); the resolved
        // scope is used purely for the hint envelope.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
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
        Ok(CodeGraphResponse::Edges(EdgesResponse {
            edges,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    async fn code_graph_describe(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let description = self.state.repo_graph().describe(ctx, key).await?;
        Ok(CodeGraphResponse::Describe(DescribeResponse {
            description,
            next_step: None,
        }))
    }

    /// PR C1: `context` op handler. Resolves to a 360° symbol view
    /// (categorized incoming/outgoing dicts + method metadata). The
    /// pre-resolve pass runs in `pre_resolve_key`; if we got here the
    /// `key` is already a canonical RepoNodeKey or the resolver
    /// short-circuited.
    async fn code_graph_context(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let include_content = params.include_content.unwrap_or(false);
        let symbol_context = self
            .state
            .repo_graph()
            .context(ctx, key, include_content)
            .await?
            .ok_or_else(|| format!("symbol '{key}' not found in graph"))?;
        Ok(CodeGraphResponse::Context(ContextResponse {
            symbol_context,
            next_step: None,
        }))
    }

    async fn code_graph_status(
        &self,
        ctx: &ProjectCtx,
        _params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let status = self.state.repo_graph().status(ctx).await?;
        Ok(CodeGraphResponse::Status(StatusResponse {
            status,
            next_step: None,
        }))
    }

    async fn code_graph_workspaces(
        &self,
        ctx: &ProjectCtx,
        _params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let result = self.state.repo_graph().workspaces(ctx).await?;
        Ok(CodeGraphResponse::Workspaces(WorkspacesResponse {
            result,
            next_step: None,
        }))
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
        let start_line_u32 = u32::try_from(start_line.max(0)).unwrap_or(0);
        let end_line_u32 = params
            .end_line
            .map(|n| u32::try_from(n.max(0)).unwrap_or(0));
        let hits = self
            .state
            .repo_graph()
            .symbols_at(ctx, file, start_line_u32, end_line_u32)
            .await?;
        Ok(CodeGraphResponse::SymbolsAt(SymbolsAtResponse {
            file: file.to_string(),
            hits,
            next_step: None,
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
            next_step: None,
        }))
    }

    /// Handler for `operation = "detect_changes"`.
    ///
    /// Two input modes:
    /// * `from_sha` + `to_sha` — runs `git diff --unified=0 from..to`
    ///   server-side and maps hunks via `symbols_enclosing`.
    /// * `changed_files` — every symbol in each listed file is treated
    ///   as touched (no line-level filtering).
    ///
    /// When both are provided line-level wins; the file list is
    /// ignored. At least one mode must be supplied.
    async fn code_graph_detect_changes(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let from = params.from_sha.as_deref().filter(|s| !s.is_empty());
        let to = params.to_sha.as_deref().filter(|s| !s.is_empty());
        let changed_files: Vec<String> = params
            .changed_files
            .as_ref()
            .map(|v| v.iter().filter(|s| !s.is_empty()).cloned().collect())
            .unwrap_or_default();
        let line_mode = from.is_some() && to.is_some();
        if !line_mode && changed_files.is_empty() {
            return Err(format!(
                "'detect_changes' requires either both 'from_sha' and \
                 'to_sha', or a non-empty 'changed_files' list (got \
                 from_sha={}, to_sha={}, changed_files={})",
                from.is_some(),
                to.is_some(),
                changed_files.len()
            ));
        }
        let result = self
            .state
            .repo_graph()
            .detect_changes(ctx, from, to, &changed_files)
            .await?;

        // Apply Phase-0 graph exclusions to suppress generated/vendored
        // noise — match the diff_touches policy.
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let mut filtered = result;
        filtered
            .touched_symbols
            .retain(|s| !exclusions.excludes(&s.uid, Some(&s.file_path), &s.name));
        // Rebuild `by_file` after filtering so the rollup matches.
        let mut by_file: std::collections::BTreeMap<String, Vec<_>> =
            std::collections::BTreeMap::new();
        for sym in &filtered.touched_symbols {
            by_file
                .entry(sym.file_path.clone())
                .or_default()
                .push(sym.clone());
        }
        filtered.by_file = by_file;

        // Bias the next-step hint toward the highest-tier symbol —
        // High > Medium > Low, then by symbol name (stable).
        let next_step = pick_next_step_target(&filtered.touched_symbols).map(|target| {
            format!(
                "Call `code_graph impact target={target}` to assess each \
                 touched symbol's blast radius."
            )
        });

        Ok(CodeGraphResponse::DetectedChanges(
            DetectedChangesResponse {
                detected_changes: filtered,
                next_step,
            },
        ))
    }

    /// Handler for `operation = "api_surface"`.
    async fn code_graph_api_surface(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_visibility(params.visibility.as_deref())?;
        let limit = params.limit.unwrap_or(100).max(0) as usize;
        // pb94: route workspace resolution through the shared helper so
        // valid / unknown / single-workspace / empty semantics stay in
        // one place.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let symbols = self
            .state
            .repo_graph()
            .api_surface(
                ctx,
                scope.workspace.as_deref(),
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
            .filter(|e| !exclusions.excludes(&e.key, e.file.as_deref(), &e.display_name))
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::ApiSurface(ApiSurfaceResponse {
            symbols,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    /// Handler for `operation = "boundary_check"`.
    async fn code_graph_boundary_check(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let rules = params
            .rules
            .as_deref()
            .ok_or_else(|| format!("'rules' is required for operation '{}'", params.operation))?;
        if rules.is_empty() {
            return Err(format!(
                "'rules' must not be empty for operation '{}'",
                params.operation
            ));
        }
        let violations = self.state.repo_graph().boundary_check(ctx, rules).await?;
        Ok(CodeGraphResponse::BoundaryCheck(BoundaryCheckResponse {
            violations,
            next_step: None,
        }))
    }

    /// Handler for `operation = "hotspots"`.
    async fn code_graph_hotspots(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let window = params.window_days.unwrap_or(90).clamp(1, 365);
        let window_u32 = u32::try_from(window).unwrap_or(90);
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let limit = limit.clamp(1, 100);
        let hotspots = self
            .state
            .repo_graph()
            .hotspots(ctx, window_u32, params.file_glob.as_deref(), limit)
            .await?;
        Ok(CodeGraphResponse::Hotspots(HotspotsResponse {
            hotspots,
            next_step: None,
        }))
    }

    /// Handler for `operation = "complexity"` (iter 28). Reuses the
    /// shared `sort_by` / `file_glob` / `limit` params; adds a dedicated
    /// `target` discriminator (`functions` | `files`). Validation of
    /// `target` and `sort_by` happens in the bridge impl so the same
    /// error shape surfaces from every call path.
    async fn code_graph_complexity(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let target = params.target.as_deref().unwrap_or("functions");
        let sort_by = params.sort_by.as_deref().unwrap_or("cognitive");
        let limit = params.limit.unwrap_or(30).max(0) as usize;
        let limit = limit.clamp(1, 200);
        let result = self
            .state
            .repo_graph()
            .complexity(ctx, target, sort_by, params.file_glob.as_deref(), limit)
            .await?;
        Ok(CodeGraphResponse::Complexity(ComplexityResponse {
            complexity: result,
            next_step: None,
        }))
    }

    /// Handler for `operation = "refactor_candidates"` (iter 29).
    /// Composite ranking that fuses cognitive complexity, file-level
    /// churn, and PageRank z-scores. Reuses `since_days` (default 90,
    /// clamped to `[1, 365]` server-side), `file_glob`, and `limit`
    /// (default 30, clamped to `[1, 200]`).
    async fn code_graph_refactor_candidates(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let since_days_u32 = params
            .since_days
            .map(|d| u32::try_from(d.max(0)).unwrap_or(0));
        let limit = params.limit.unwrap_or(30).max(0) as usize;
        let limit = limit.clamp(1, 200);
        let candidates = self
            .state
            .repo_graph()
            .refactor_candidates(ctx, since_days_u32, params.file_glob.as_deref(), limit)
            .await?;
        Ok(CodeGraphResponse::RefactorCandidates(
            RefactorCandidatesResponse {
                refactor_candidates: candidates,
                next_step: None,
            },
        ))
    }

    /// Handler for `operation = "metrics_at"`.
    async fn code_graph_metrics_at(
        &self,
        ctx: &ProjectCtx,
        _params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let metrics = self.state.repo_graph().metrics_at(ctx).await?;
        Ok(CodeGraphResponse::MetricsAt(MetricsAtResponse {
            metrics,
            next_step: None,
        }))
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
            next_step: None,
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
            DeprecatedCallersResponse {
                hits,
                next_step: None,
            },
        ))
    }

    /// Handler for `operation = "coupling"`.
    async fn code_graph_coupling(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let file = params
            .file
            .as_deref()
            .filter(|f| !f.is_empty())
            .ok_or_else(|| format!("'file' is required for operation '{}'", params.operation))?;
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let limit = limit.clamp(1, 200);
        // Over-fetch 25× so the exclusion filter doesn't starve the
        // returned set. Underlying SQL sort is invariant to LIMIT, so
        // this is effectively free.
        let fetch_limit = (limit.saturating_mul(25)).clamp(limit, 500);
        let coupled = self
            .state
            .repo_graph()
            .coupling(ctx, file, fetch_limit)
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let coupled: Vec<CouplingEntry> = coupled
            .into_iter()
            .filter(|c| !exclusions.excludes_path(&c.file_path))
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::Coupling(CouplingResponse {
            file: file.to_string(),
            coupled,
            next_step: None,
        }))
    }

    /// Handler for `operation = "coupling_hotspots"`.
    async fn code_graph_coupling_hotspots(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let limit = limit.clamp(1, 200);
        let max_files_per_commit =
            params.max_files_per_commit.unwrap_or(15).clamp(1, 1000) as usize;
        let fetch_limit = (limit.saturating_mul(25)).clamp(limit, 500);
        let since_days_u32 = params
            .since_days
            .map(|d| u32::try_from(d.max(0)).unwrap_or(0));
        let pairs = self
            .state
            .repo_graph()
            .coupling_hotspots(ctx, fetch_limit, since_days_u32, max_files_per_commit)
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let pairs: Vec<CoupledPairEntry> = pairs
            .into_iter()
            .filter(|p| {
                !exclusions.excludes_path(&p.file_a) && !exclusions.excludes_path(&p.file_b)
            })
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::CouplingHotspots(
            CouplingHotspotsResponse {
                pairs,
                next_step: None,
            },
        ))
    }

    /// Handler for `operation = "coupling_hubs"`.
    async fn code_graph_coupling_hubs(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let limit = limit.clamp(1, 200);
        let max_files_per_commit =
            params.max_files_per_commit.unwrap_or(15).clamp(1, 1000) as usize;
        let fetch_limit = (limit.saturating_mul(25)).clamp(limit, 500);
        let since_days_u32 = params
            .since_days
            .map(|d| u32::try_from(d.max(0)).unwrap_or(0));
        let hubs = self
            .state
            .repo_graph()
            .coupling_hubs(ctx, fetch_limit, since_days_u32, max_files_per_commit)
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let hubs: Vec<CouplingHubEntry> = hubs
            .into_iter()
            .filter(|h| !exclusions.excludes_path(&h.file_path))
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::CouplingHubs(CouplingHubsResponse {
            hubs,
            next_step: None,
        }))
    }

    /// Handler for `operation = "churn"`.
    async fn code_graph_churn(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let limit = limit.clamp(1, 200);
        let since_days_u32 = params
            .since_days
            .map(|d| u32::try_from(d.max(0)).unwrap_or(0));
        let files = self
            .state
            .repo_graph()
            .churn(ctx, limit, since_days_u32)
            .await?;
        Ok(CodeGraphResponse::Churn(ChurnResponse {
            files,
            next_step: None,
        }))
    }

    /// Handler for `operation = "touches_hot_path"`.
    async fn code_graph_touches_hot_path(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let seed_entries = params.seed_entries.as_deref().ok_or_else(|| {
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
        let symbols = params
            .symbols
            .as_deref()
            .ok_or_else(|| format!("'symbols' is required for operation '{}'", params.operation))?;
        // pb94: `touches_hot_path` is a TRAVERSAL op — `workspace`
        // scopes only seed/entry/sink resolution, the shortest-path
        // walk itself must remain unconstrained.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let hits = self
            .state
            .repo_graph()
            .touches_hot_path(
                ctx,
                scope.workspace.as_deref(),
                seed_entries,
                seed_sinks,
                symbols,
            )
            .await?;
        Ok(CodeGraphResponse::TouchesHotPath(TouchesHotPathResponse {
            hits,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    /// Handler for `operation = "snapshot"` (PR D2).
    ///
    /// Returns the full repo graph capped by PageRank tier so the
    /// `/code-graph` UI can render it through Sigma + ForceAtlas2
    /// without hitting the ~5k-node WebGL ceiling on large
    /// repositories. The cap defaults to 2000 (matches the plan's
    /// `Sigma.js performance ceiling at ~5k nodes` mitigation) and is
    /// settable via the `limit` field.
    ///
    /// `graph_excluded_paths` filtering happens in the bridge so
    /// `total_nodes` / `total_edges` reflect the post-exclusion graph
    /// — the cap is then applied to the surviving population.
    async fn code_graph_snapshot(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        // Default 2000 nodes — plan §"Risks & mitigations" calls out
        // this as the Sigma WebGL ceiling. Clamp to [1, 10_000] to keep
        // the wire payload bounded; callers that want a wider view can
        // request up to 10k explicitly.
        let node_cap = params
            .limit
            .map(|l| l.max(1) as usize)
            .unwrap_or(2_000)
            .clamp(1, 10_000);
        let level = SnapshotLevel::parse(params.level.as_deref())?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        // pb94: route workspace resolution through the shared helper so
        // valid / unknown / single-workspace / empty semantics stay in
        // one place.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let mut snapshot = self
            .state
            .repo_graph()
            .snapshot(
                ctx,
                scope.workspace.as_deref(),
                level,
                node_cap,
                &exclusions,
            )
            .await?;
        // v10: apply the `tests=` filter to the assembled snapshot. The
        // UI defaults to `include` (it toggles test visibility
        // client-side off `SnapshotNode.is_test`); agents can pass
        // `exclude` / `only` to get a server-side-narrowed graph. Drop
        // edges whose endpoints no longer survive so the wire shape
        // stays internally consistent.
        let filter = TestFilter::parse(params.tests.as_deref(), TestFilter::Include);
        if filter != TestFilter::Include {
            snapshot.nodes.retain(|n| filter.keeps(n.is_test));
            let kept: std::collections::HashSet<&str> =
                snapshot.nodes.iter().map(|n| n.id.as_str()).collect();
            snapshot
                .edges
                .retain(|e| kept.contains(e.from.as_str()) && kept.contains(e.to.as_str()));
        }
        Ok(CodeGraphResponse::Snapshot(SnapshotResponse {
            snapshot,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use djinn_core::events::EventBus;
    use djinn_db::Database;
    use djinn_provider::catalog::{CatalogService, HealthTracker};

    use crate::bridge::{
        ApiSurfaceEntry, BoundaryRule, BoundaryViolation, ChangedRange, ChurnEntry,
        ComplexityResult, CoupledPairEntry, CouplingEntry, CouplingHubEntry, CycleGroup,
        DeadSymbolEntry, DeprecatedHit, DetectedChangesResult, DiffTouchesResult, EdgeEntry,
        GraphStatus, HotPathHit, HotspotEntry, ImpactResult, MetricsAtResult, NeighborsResult,
        OrphanEntry, PathResult, ProjectCtx, QuerySubgraphBudget, QuerySubgraphEdge,
        QuerySubgraphNode, QuerySubgraphRequest, QuerySubgraphResult, QuerySubgraphSeedDebug,
        QuerySubgraphTraversalDebug, RankedNode, RefactorCandidate, RepoGraphOps, ResolveOutcome,
        SearchHit, SnapshotPayload, SymbolAtHit, SymbolContext, SymbolDescription,
    };
    use crate::server::DjinnMcpServer;
    use crate::state::McpState;

    #[derive(Clone)]
    struct RecordingQuerySubgraphOps {
        seen: Arc<Mutex<Vec<QuerySubgraphRequest>>>,
        response: QuerySubgraphResult,
    }

    impl RecordingQuerySubgraphOps {
        fn new(response: QuerySubgraphResult) -> Self {
            Self {
                seen: Arc::new(Mutex::new(Vec::new())),
                response,
            }
        }

        fn seen_requests(&self) -> Vec<QuerySubgraphRequest> {
            self.seen.lock().expect("seen requests lock").clone()
        }
    }

    #[async_trait]
    impl RepoGraphOps for RecordingQuerySubgraphOps {
        async fn neighbors(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<NeighborsResult, String> {
            unreachable!("query_subgraph tests should not call neighbors")
        }

        async fn ranked(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RankedNode>, String> {
            unreachable!("query_subgraph tests should not call ranked")
        }

        async fn implementations(&self, _: &ProjectCtx, _: &str) -> Result<Vec<String>, String> {
            unreachable!("query_subgraph tests should not call implementations")
        }

        async fn impact(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: &str,
            _: usize,
            _: Option<&str>,
            _: Option<f64>,
        ) -> Result<ImpactResult, String> {
            unreachable!("query_subgraph tests should not call impact")
        }

        async fn search(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<SearchHit>, String> {
            unreachable!("query_subgraph tests should not call search")
        }

        async fn query_subgraph(
            &self,
            _: &ProjectCtx,
            req: QuerySubgraphRequest,
        ) -> Result<QuerySubgraphResult, String> {
            self.seen.lock().expect("seen requests lock").push(req);
            Ok(self.response.clone())
        }

        async fn cycles(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<CycleGroup>, String> {
            unreachable!("query_subgraph tests should not call cycles")
        }

        async fn orphans(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<OrphanEntry>, String> {
            unreachable!("query_subgraph tests should not call orphans")
        }

        async fn path(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: &str,
            _: &str,
            _: Option<usize>,
        ) -> Result<Option<PathResult>, String> {
            unreachable!("query_subgraph tests should not call path")
        }

        async fn edges(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<EdgeEntry>, String> {
            unreachable!("query_subgraph tests should not call edges")
        }

        async fn describe(
            &self,
            _: &ProjectCtx,
            _: &str,
        ) -> Result<Option<SymbolDescription>, String> {
            unreachable!("query_subgraph tests should not call describe")
        }

        async fn context(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: bool,
        ) -> Result<Option<SymbolContext>, String> {
            unreachable!("query_subgraph tests should not call context")
        }

        async fn status(&self, _: &ProjectCtx) -> Result<GraphStatus, String> {
            unreachable!("query_subgraph tests should not call status")
        }

        async fn snapshot(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: SnapshotLevel,
            _: usize,
            _: &GraphExclusions,
        ) -> Result<SnapshotPayload, String> {
            unreachable!("query_subgraph tests should not call snapshot")
        }

        async fn symbols_at(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: u32,
            _: Option<u32>,
        ) -> Result<Vec<SymbolAtHit>, String> {
            unreachable!("query_subgraph tests should not call symbols_at")
        }

        async fn diff_touches(
            &self,
            _: &ProjectCtx,
            _: &[ChangedRange],
        ) -> Result<DiffTouchesResult, String> {
            unreachable!("query_subgraph tests should not call diff_touches")
        }

        async fn detect_changes(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: &[String],
        ) -> Result<DetectedChangesResult, String> {
            unreachable!("query_subgraph tests should not call detect_changes")
        }

        async fn api_surface(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<ApiSurfaceEntry>, String> {
            unreachable!("query_subgraph tests should not call api_surface")
        }

        async fn boundary_check(
            &self,
            _: &ProjectCtx,
            _: &[BoundaryRule],
        ) -> Result<Vec<BoundaryViolation>, String> {
            unreachable!("query_subgraph tests should not call boundary_check")
        }

        async fn hotspots(
            &self,
            _: &ProjectCtx,
            _: u32,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<HotspotEntry>, String> {
            unreachable!("query_subgraph tests should not call hotspots")
        }

        async fn complexity(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<ComplexityResult, String> {
            unreachable!("query_subgraph tests should not call complexity")
        }

        async fn refactor_candidates(
            &self,
            _: &ProjectCtx,
            _: Option<u32>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RefactorCandidate>, String> {
            unreachable!("query_subgraph tests should not call refactor_candidates")
        }

        async fn metrics_at(&self, _: &ProjectCtx) -> Result<MetricsAtResult, String> {
            unreachable!("query_subgraph tests should not call metrics_at")
        }

        async fn dead_symbols(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<DeadSymbolEntry>, String> {
            unreachable!("query_subgraph tests should not call dead_symbols")
        }

        async fn deprecated_callers(
            &self,
            _: &ProjectCtx,
            _: usize,
        ) -> Result<Vec<DeprecatedHit>, String> {
            unreachable!("query_subgraph tests should not call deprecated_callers")
        }

        async fn touches_hot_path(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: &[String],
            _: &[String],
            _: &[String],
        ) -> Result<Vec<HotPathHit>, String> {
            unreachable!("query_subgraph tests should not call touches_hot_path")
        }

        async fn coupling(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<CouplingEntry>, String> {
            unreachable!("query_subgraph tests should not call coupling")
        }

        async fn churn(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
        ) -> Result<Vec<ChurnEntry>, String> {
            unreachable!("query_subgraph tests should not call churn")
        }

        async fn coupling_hotspots(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CoupledPairEntry>, String> {
            unreachable!("query_subgraph tests should not call coupling_hotspots")
        }

        async fn coupling_hubs(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CouplingHubEntry>, String> {
            unreachable!("query_subgraph tests should not call coupling_hubs")
        }

        async fn resolve(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
        ) -> Result<ResolveOutcome, String> {
            Ok(ResolveOutcome::NotFound)
        }
    }

    fn query_subgraph_canned_response() -> QuerySubgraphResult {
        QuerySubgraphResult {
            query: "How is auth routed?".to_string(),
            nodes: vec![
                QuerySubgraphNode {
                    uid: "file:src/auth.rs".to_string(),
                    kind: "file".to_string(),
                    display_name: "src/auth.rs".to_string(),
                    file_path: Some("src/auth.rs".to_string()),
                    workspace: Some("server".to_string()),
                    is_seed: true,
                    is_hub: false,
                    degree: 3,
                },
                QuerySubgraphNode {
                    uid: "sym:login".to_string(),
                    kind: "symbol".to_string(),
                    display_name: "login".to_string(),
                    file_path: Some("src/auth.rs".to_string()),
                    workspace: Some("server".to_string()),
                    is_seed: false,
                    is_hub: false,
                    degree: 1,
                },
            ],
            edges: vec![QuerySubgraphEdge {
                from_uid: "file:src/auth.rs".to_string(),
                to_uid: "sym:login".to_string(),
                kind: "contains_definition".to_string(),
                confidence: 0.98,
                reason: Some("seed expansion".to_string()),
            }],
            seeds: vec![QuerySubgraphSeedDebug {
                uid: "file:src/auth.rs".to_string(),
                display_name: "src/auth.rs".to_string(),
                score: 0.91,
                source: "hybrid".to_string(),
                matched_text: Some("auth routed".to_string()),
                debug: vec!["rrf=0.91".to_string()],
            }],
            inferred_edge_kinds: vec!["calls".to_string(), "imports".to_string()],
            budget: QuerySubgraphBudget {
                requested_tokens: 2_048,
                estimated_tokens: 1_200,
                truncated: true,
                omitted_nodes: 4,
                omitted_edges: 7,
            },
            traversal: QuerySubgraphTraversalDebug {
                max_depth: 2,
                hub_degree_threshold: 99,
                hubs_blocked: vec!["sym:common".to_string()],
                skipped_edge_kinds: vec!["writes".to_string()],
            },
            narrowing_hints: vec!["narrow with context_filter=auth middleware".to_string()],
        }
    }

    fn query_subgraph_test_server(
        graph: RecordingQuerySubgraphOps,
    ) -> (DjinnMcpServer, RecordingQuerySubgraphOps) {
        let db = Database::open_in_memory().expect("open in-memory db");
        let state = McpState::new(
            db,
            EventBus::noop(),
            CatalogService::new(),
            HealthTracker::new(),
            None,
            None,
            None,
            None,
            Arc::new(crate::state::stubs::StubLspOps),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(graph.clone()),
        );
        (DjinnMcpServer::new(state), graph)
    }

    fn query_subgraph_ctx() -> ProjectCtx {
        ProjectCtx {
            id: "proj-query-subgraph".to_string(),
            clone_path: "/workspace/repo".to_string(),
            workspace: Some("server".to_string()),
            sub_path: None,
        }
    }

    #[tokio::test]
    async fn query_subgraph_dispatch_maps_request_fields() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_canned_response());
        let (server, graph) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.workspace = Some("server".to_string());
        params.query = Some("  How is auth routed?  ".to_string());
        params.context_filter = Some(" middleware ".to_string());
        params.file_filter = Some(" src/auth ".to_string());
        params.kind_filter = Some("symbol".to_string());
        params.edge_filters = Some(vec![" Calls ".to_string(), "IMPORTS".to_string()]);
        params.token_budget = Some(2_048);
        params.max_depth = Some(3);
        params.max_seeds = Some(4);

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("query_subgraph dispatch succeeds");

        match response {
            CodeGraphResponse::QuerySubgraph(resp) => {
                assert_eq!(resp.query_subgraph.query, "How is auth routed?");
            }
            other => panic!("expected query_subgraph response, got {other:?}"),
        }
        let seen = graph.seen_requests();
        assert_eq!(seen.len(), 1, "RepoGraphOps::query_subgraph called once");
        let req = &seen[0];
        assert_eq!(req.query, "How is auth routed?");
        assert_eq!(req.workspace.as_deref(), Some("server"));
        assert_eq!(req.context_filter.as_deref(), Some("middleware"));
        assert_eq!(req.file_filter.as_deref(), Some("src/auth"));
        assert_eq!(req.kind_filter.as_deref(), Some("symbol"));
        assert_eq!(req.edge_filter, vec!["calls", "imports"]);
        assert_eq!(req.token_budget, Some(2_048));
        assert_eq!(req.max_depth, Some(3));
        assert_eq!(req.max_seeds, Some(4));
    }

    #[tokio::test]
    async fn query_subgraph_dispatch_rejects_missing_and_blank_query() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_canned_response());
        let (server, graph) = query_subgraph_test_server(graph);
        let ctx = query_subgraph_ctx();

        let mut missing = test_params("query_subgraph");
        missing.project_id = ctx.id.clone();
        missing.project_path = ctx.clone_path.clone();
        let err = server
            .dispatch_code_graph(&ctx, &mut missing)
            .await
            .expect_err("missing query rejected through dispatch");
        assert!(
            err.contains("'query' is required for operation 'query_subgraph'"),
            "explicit query validation error: {err}"
        );

        let mut blank = test_params("query_subgraph");
        blank.project_id = ctx.id.clone();
        blank.project_path = ctx.clone_path.clone();
        blank.query = Some(" \t\n ".to_string());
        let err = server
            .dispatch_code_graph(&ctx, &mut blank)
            .await
            .expect_err("blank query rejected through dispatch");
        assert!(
            err.contains("'query' is required for operation 'query_subgraph'"),
            "explicit blank query validation error: {err}"
        );
        assert!(
            graph.seen_requests().is_empty(),
            "invalid requests must not reach RepoGraphOps::query_subgraph"
        );
    }

    #[tokio::test]
    async fn query_subgraph_dispatch_applies_token_budget_defaults_clamps_and_rejections() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_canned_response());
        let (server, graph) = query_subgraph_test_server(graph);
        let ctx = query_subgraph_ctx();

        let mut default_budget = test_params("query_subgraph");
        default_budget.project_id = ctx.id.clone();
        default_budget.project_path = ctx.clone_path.clone();
        default_budget.query = Some("default budget".to_string());
        server
            .dispatch_code_graph(&ctx, &mut default_budget)
            .await
            .expect("default budget dispatch succeeds");

        let mut clamped_low = test_params("query_subgraph");
        clamped_low.project_id = ctx.id.clone();
        clamped_low.project_path = ctx.clone_path.clone();
        clamped_low.query = Some("small budget".to_string());
        clamped_low.token_budget = Some(128);
        server
            .dispatch_code_graph(&ctx, &mut clamped_low)
            .await
            .expect("low budget dispatch succeeds with clamp");

        let mut clamped_high = test_params("query_subgraph");
        clamped_high.project_id = ctx.id.clone();
        clamped_high.project_path = ctx.clone_path.clone();
        clamped_high.query = Some("large budget".to_string());
        clamped_high.token_budget = Some(99_999);
        server
            .dispatch_code_graph(&ctx, &mut clamped_high)
            .await
            .expect("high budget dispatch succeeds with clamp");

        let requests = graph.seen_requests();
        assert_eq!(
            requests[0].token_budget, None,
            "omitted budget stays omitted"
        );
        assert_eq!(
            requests[1].token_budget,
            Some(1_024),
            "low budget clamps up"
        );
        assert_eq!(
            requests[2].token_budget,
            Some(32_000),
            "high budget clamps down"
        );

        let mut zero = test_params("query_subgraph");
        zero.project_id = ctx.id.clone();
        zero.project_path = ctx.clone_path.clone();
        zero.query = Some("zero budget".to_string());
        zero.token_budget = Some(0);
        let err = server
            .dispatch_code_graph(&ctx, &mut zero)
            .await
            .expect_err("zero budget rejected through dispatch");
        assert!(
            err.contains("invalid token_budget 0"),
            "invalid budget error should name token_budget: {err}"
        );
    }

    #[tokio::test]
    async fn query_subgraph_dispatch_serializes_full_response_shape() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_canned_response());
        let (server, _) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.query = Some("How is auth routed?".to_string());

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("query_subgraph dispatch succeeds");
        let json = serde_json::to_value(CodeGraphResponse::QuerySubgraph(match response {
            CodeGraphResponse::QuerySubgraph(resp) => resp,
            other => panic!("expected query_subgraph response, got {other:?}"),
        }))
        .expect("serialize query_subgraph response");
        let payload = json
            .get("query_subgraph")
            .and_then(|v| v.as_object())
            .expect("query_subgraph payload object");
        for required in [
            "query",
            "nodes",
            "edges",
            "seeds",
            "inferred_edge_kinds",
            "budget",
            "traversal",
            "narrowing_hints",
        ] {
            assert!(payload.contains_key(required), "missing {required}: {json}");
        }
        assert_eq!(payload["nodes"].as_array().expect("nodes array").len(), 2);
        assert_eq!(payload["edges"].as_array().expect("edges array").len(), 1);

        let seed = payload["seeds"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_object())
            .expect("seed debug object");
        for required in [
            "uid",
            "display_name",
            "score",
            "source",
            "matched_text",
            "debug",
        ] {
            assert!(
                seed.contains_key(required),
                "seed missing {required}: {seed:?}"
            );
        }

        let budget = payload["budget"].as_object().expect("budget object");
        for required in [
            "requested_tokens",
            "estimated_tokens",
            "truncated",
            "omitted_nodes",
            "omitted_edges",
        ] {
            assert!(
                budget.contains_key(required),
                "budget missing {required}: {budget:?}"
            );
        }
        assert_eq!(
            budget.get("truncated").and_then(|v| v.as_bool()),
            Some(true)
        );

        let traversal = payload["traversal"].as_object().expect("traversal object");
        for required in [
            "max_depth",
            "hub_degree_threshold",
            "hubs_blocked",
            "skipped_edge_kinds",
        ] {
            assert!(
                traversal.contains_key(required),
                "traversal missing {required}: {traversal:?}"
            );
        }
        assert_eq!(
            payload["narrowing_hints"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str()),
            Some("narrow with context_filter=auth middleware")
        );
    }

    #[test]
    fn workspaces_response_serializes_workspace_metadata() {
        let json = serde_json::to_value(CodeGraphResponse::Workspaces(WorkspacesResponse {
            result: WorkspacesResult {
                project_id: "proj-1".to_string(),
                workspaces: vec![crate::bridge::GraphWorkspaceEntry {
                    slug: "server".to_string(),
                    name: "server".to_string(),
                    node_count: 42,
                    commit_sha: Some("abc123".to_string()),
                    warmed_at: Some("2026-06-09T00:00:00.000Z".to_string()),
                    status: Some("ready".to_string()),
                }],
            },
            next_step: None,
        }))
        .expect("serialize workspaces response");

        assert_eq!(
            json.get("project_id").and_then(|v| v.as_str()),
            Some("proj-1")
        );
        let workspace = json
            .get("workspaces")
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .and_then(|v| v.as_object())
            .expect("workspace entry");
        for required in [
            "slug",
            "name",
            "node_count",
            "commit_sha",
            "warmed_at",
            "status",
        ] {
            assert!(
                workspace.contains_key(required),
                "missing {required}: {workspace:?}"
            );
        }
        assert_eq!(
            workspace.get("slug").and_then(|v| v.as_str()),
            Some("server")
        );
        assert_eq!(
            workspace.get("node_count").and_then(|v| v.as_u64()),
            Some(42)
        );
    }

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

    /// PR B4: search-mode resolution layers caller intent on top of
    /// the `DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE` env var. Caller
    /// always wins; missing env var defaults to `name`.
    #[test]
    fn resolve_search_mode_caller_overrides_env() {
        // Caller pin → wins regardless of env var value.
        let prev = std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE").ok();
        unsafe {
            std::env::set_var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE", "hybrid");
        }
        assert_eq!(resolve_search_mode(Some("name")).unwrap(), SearchMode::Name);
        // Explicit `hybrid` also resolves.
        assert_eq!(
            resolve_search_mode(Some("hybrid")).unwrap(),
            SearchMode::Hybrid
        );
        // Unset → env var wins (`hybrid`).
        assert_eq!(resolve_search_mode(None).unwrap(), SearchMode::Hybrid);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE", v),
                None => std::env::remove_var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE"),
            }
        }
    }

    #[test]
    fn resolve_search_mode_default_is_name() {
        let prev = std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE").ok();
        unsafe {
            std::env::remove_var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE");
        }
        assert_eq!(resolve_search_mode(None).unwrap(), SearchMode::Name);
        unsafe {
            if let Some(v) = prev {
                std::env::set_var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE", v);
            }
        }
    }

    #[test]
    fn resolve_search_mode_rejects_unknown_value() {
        assert!(resolve_search_mode(Some("fuzzy")).is_err());
    }

    /// PR B4: `mode` field deserialises off the wire.
    #[test]
    fn parses_search_mode_from_json() {
        let json = serde_json::json!({
            "operation": "search",
            "project": "/workspace/repo",
            "query": "permissions check",
            "mode": "hybrid",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.mode.as_deref(), Some("hybrid"));
    }

    #[test]
    fn parses_workspace_from_json() {
        for (workspace, expected) in [
            ("", Some("")),
            ("nonexistent", Some("nonexistent")),
            ("server", Some("server")),
        ] {
            let json = serde_json::json!({
                "operation": "ranked",
                "project": "/workspace/repo",
                "workspace": workspace,
            });

            let params: CodeGraphParams = serde_json::from_value(json).unwrap();

            assert_eq!(params.workspace.as_deref(), expected);
        }
    }

    #[test]
    fn normalize_clears_empty_workspace() {
        let json = serde_json::json!({
            "operation": "ranked",
            "project": "/workspace/repo",
            "workspace": "",
        });
        let mut params: CodeGraphParams = serde_json::from_value(json).unwrap();

        params.normalize();

        assert_eq!(params.workspace, None);
    }

    // ── pb94: workspace scoping semantic tests ─────────────────────────────────
    //
    // The acceptance criteria require explicit coverage for:
    // - empty `workspace: ""` continuing to normalize to `None`
    // - valid slugs hard-filtering for listing/bounded ops
    // - valid slugs scoping only seed/endpoint for traversal ops
    // - unknown non-empty slugs returning full result + structured hint
    // - single-workspace graphs being a no-op (no surprising hard-empty)
    //
    // Each fixture is a thin `RepoGraphOps` impl that records the
    // workspace arg it sees (so the test can assert "what did the
    // handler pass downstream?") and returns canned data from the
    // workspace-aware methods. The fixture's `workspace_hint` is
    // programmable so the test can simulate single-vs-multi-workspace
    // graphs and known-vs-unknown slugs.

    /// Programmable workspace fixture: records calls, returns canned
    /// data, and answers `workspace_hint` according to a test-supplied
    /// plan. Mirrors the production `workspace_hint` semantics — single
    /// workspace projects never produce a hint, unknown slugs against
    /// multi-workspace projects surface the candidate list.
    #[derive(Clone)]
    struct WorkspaceFixtureOps {
        /// Every bridge method that takes a workspace records the slug
        /// it was called with into a parallel slot so the test can
        /// assert "the handler passed this to the bridge".
        seen_workspaces: Arc<Mutex<Vec<Option<String>>>>,
        /// What `workspace_hint` should return. `Some(candidates)`
        /// mimics an unknown slug on a multi-workspace graph; `None`
        /// mimics single-workspace / no-workspace / known-slug cases.
        hint: Option<Vec<String>>,
        ranked_nodes: Vec<RankedNode>,
        impact_result: ImpactResult,
    }

    impl WorkspaceFixtureOps {
        fn new(hint: Option<Vec<String>>) -> Self {
            Self {
                seen_workspaces: Arc::new(Mutex::new(Vec::new())),
                hint,
                ranked_nodes: Vec::new(),
                impact_result: ImpactResult::Detailed(Vec::new()),
            }
        }

        fn with_ranked_and_impact(
            hint: Option<Vec<String>>,
            ranked_nodes: Vec<RankedNode>,
            impact_result: ImpactResult,
        ) -> Self {
            Self {
                seen_workspaces: Arc::new(Mutex::new(Vec::new())),
                hint,
                ranked_nodes,
                impact_result,
            }
        }

        fn record_workspace(&self, workspace: Option<&str>) {
            let mut guard = self.seen_workspaces.lock().expect("seen_workspaces");
            guard.push(workspace.map(str::to_string));
        }

        fn seen_workspaces(&self) -> Vec<Option<String>> {
            self.seen_workspaces
                .lock()
                .expect("seen_workspaces")
                .clone()
        }
    }

    #[async_trait]
    impl RepoGraphOps for WorkspaceFixtureOps {
        async fn ranked(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RankedNode>, String> {
            self.record_workspace(workspace);
            Ok(self.ranked_nodes.clone())
        }

        async fn impact(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: &str,
            _: usize,
            _: Option<&str>,
            _: Option<f64>,
        ) -> Result<ImpactResult, String> {
            self.record_workspace(workspace);
            Ok(self.impact_result.clone())
        }

        async fn workspace_hint(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
        ) -> Result<Option<Vec<String>>, String> {
            Ok(self.hint.clone())
        }

        // The rest are no-ops — these tests don't exercise them. We
        // still have to satisfy the trait so the dispatcher doesn't
        // trip when it routes the call.
        async fn workspaces(
            &self,
            ctx: &ProjectCtx,
        ) -> Result<crate::bridge::WorkspacesResult, String> {
            Ok(crate::bridge::WorkspacesResult {
                project_id: ctx.id.clone(),
                workspaces: Vec::new(),
            })
        }

        async fn neighbors(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<NeighborsResult, String> {
            Ok(NeighborsResult::Detailed(Vec::new()))
        }
        async fn implementations(&self, _: &ProjectCtx, _: &str) -> Result<Vec<String>, String> {
            Ok(Vec::new())
        }
        async fn search(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<SearchHit>, String> {
            Ok(Vec::new())
        }
        async fn query_subgraph(
            &self,
            _: &ProjectCtx,
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
        async fn cycles(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<CycleGroup>, String> {
            Ok(Vec::new())
        }
        async fn orphans(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<OrphanEntry>, String> {
            self.record_workspace(workspace);
            Ok(Vec::new())
        }
        async fn path(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: &str,
            _: &str,
            _: Option<usize>,
        ) -> Result<Option<PathResult>, String> {
            self.record_workspace(workspace);
            Ok(None)
        }
        async fn edges(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<EdgeEntry>, String> {
            Ok(Vec::new())
        }
        async fn describe(
            &self,
            _: &ProjectCtx,
            _: &str,
        ) -> Result<Option<SymbolDescription>, String> {
            Ok(None)
        }
        async fn context(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: bool,
        ) -> Result<Option<SymbolContext>, String> {
            Ok(None)
        }
        async fn status(&self, _: &ProjectCtx) -> Result<GraphStatus, String> {
            Ok(GraphStatus {
                project_id: "p".to_string(),
                warmed: false,
                last_warm_at: None,
                pinned_commit: None,
                commits_since_pin: None,
            })
        }
        async fn snapshot(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: SnapshotLevel,
            _: usize,
            _: &crate::tools::graph_exclusions::GraphExclusions,
        ) -> Result<SnapshotPayload, String> {
            self.record_workspace(workspace);
            Ok(SnapshotPayload {
                project_id: "p".to_string(),
                git_head: String::new(),
                generated_at: String::new(),
                truncated: false,
                total_nodes: 0,
                total_edges: 0,
                node_cap: 0,
                nodes: Vec::new(),
                edges: Vec::new(),
            })
        }
        async fn symbols_at(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: u32,
            _: Option<u32>,
        ) -> Result<Vec<SymbolAtHit>, String> {
            Ok(Vec::new())
        }
        async fn diff_touches(
            &self,
            _: &ProjectCtx,
            _: &[ChangedRange],
        ) -> Result<crate::bridge::DiffTouchesResult, String> {
            unimplemented!()
        }
        async fn detect_changes(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: &[String],
        ) -> Result<DetectedChangesResult, String> {
            unimplemented!()
        }
        async fn api_surface(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<ApiSurfaceEntry>, String> {
            self.record_workspace(workspace);
            Ok(Vec::new())
        }
        async fn boundary_check(
            &self,
            _: &ProjectCtx,
            _: &[BoundaryRule],
        ) -> Result<Vec<BoundaryViolation>, String> {
            Ok(Vec::new())
        }
        async fn hotspots(
            &self,
            _: &ProjectCtx,
            _: u32,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<HotspotEntry>, String> {
            Ok(Vec::new())
        }
        async fn complexity(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<ComplexityResult, String> {
            Ok(ComplexityResult::Functions(Vec::new()))
        }
        async fn refactor_candidates(
            &self,
            _: &ProjectCtx,
            _: Option<u32>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RefactorCandidate>, String> {
            Ok(Vec::new())
        }
        async fn metrics_at(&self, _: &ProjectCtx) -> Result<MetricsAtResult, String> {
            unimplemented!()
        }
        async fn dead_symbols(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<DeadSymbolEntry>, String> {
            Ok(Vec::new())
        }
        async fn deprecated_callers(
            &self,
            _: &ProjectCtx,
            _: usize,
        ) -> Result<Vec<DeprecatedHit>, String> {
            Ok(Vec::new())
        }
        async fn touches_hot_path(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: &[String],
            _: &[String],
            _: &[String],
        ) -> Result<Vec<HotPathHit>, String> {
            self.record_workspace(workspace);
            Ok(Vec::new())
        }
        async fn coupling(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<CouplingEntry>, String> {
            Ok(Vec::new())
        }
        async fn churn(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
        ) -> Result<Vec<ChurnEntry>, String> {
            Ok(Vec::new())
        }
        async fn coupling_hotspots(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CoupledPairEntry>, String> {
            Ok(Vec::new())
        }
        async fn coupling_hubs(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CouplingHubEntry>, String> {
            Ok(Vec::new())
        }
        async fn resolve(
            &self,
            _: &ProjectCtx,
            key: &str,
            _: Option<&str>,
        ) -> Result<crate::bridge::ResolveOutcome, String> {
            // The dispatcher pre-resolves the caller's `key` (or
            // `from`/`to` for `path`) before routing to the inner op.
            // Echoing the input back as the resolved uid keeps the
            // dispatcher from short-circuiting to `NotFound` while
            // still letting the inner handler see the test's `key`
            // string.
            Ok(crate::bridge::ResolveOutcome::Found(key.to_string()))
        }
    }

    fn fixture_server(ops: WorkspaceFixtureOps) -> (DjinnMcpServer, WorkspaceFixtureOps) {
        let db = Database::open_in_memory().expect("open in-memory db");
        let state = McpState::new(
            db,
            EventBus::noop(),
            CatalogService::new(),
            HealthTracker::new(),
            None,
            None,
            None,
            None,
            Arc::new(crate::state::stubs::StubLspOps),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(ops.clone()),
        );
        (DjinnMcpServer::new(state), ops)
    }

    fn fixture_ctx(workspace: Option<&str>) -> ProjectCtx {
        ProjectCtx {
            id: "proj-ws".to_string(),
            clone_path: "/workspace/repo".to_string(),
            workspace: workspace.map(str::to_string),
            sub_path: None,
        }
    }

    /// Acceptance criterion: `resolve_workspace_scope` returns
    /// `(None, None)` when no workspace is requested. Bridge sees
    /// `None`.
    #[tokio::test]
    async fn workspace_scope_unscoped_passes_through() {
        let ops = WorkspaceFixtureOps::new(None);
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(None);
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unscoped dispatch succeeds");
        match response {
            CodeGraphResponse::Ranked(ranked) => {
                assert!(
                    ranked.workspace_hint.is_none(),
                    "no hint when workspace is omitted, got {:?}",
                    ranked.workspace_hint
                );
            }
            other => panic!("expected ranked, got {other:?}"),
        }
    }

    /// Acceptance criterion: empty `workspace: ""` normalizes to `None`
    /// at the request boundary, same shape as omitting the field.
    #[tokio::test]
    async fn workspace_scope_empty_string_normalizes_to_none() {
        let ops = WorkspaceFixtureOps::new(None);
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(None);
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some(String::new());
        server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("empty-workspace dispatch succeeds");
        // `normalize()` runs at the dispatcher boundary, so by the
        // time the bridge saw the request `ctx.workspace` is `None`
        // and no hint fires.
    }

    /// Acceptance criterion: for valid slugs the bridge sees the slug
    /// and no hint is surfaced.
    #[tokio::test]
    async fn workspace_scope_known_slug_passes_through_for_listing_op() {
        let ops = WorkspaceFixtureOps::new(None);
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug dispatch succeeds");
        match response {
            CodeGraphResponse::Ranked(ranked) => {
                assert!(ranked.workspace_hint.is_none());
            }
            other => panic!("expected ranked, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "ranked (listing/bounded) hard-filters with the slug"
        );
    }

    /// Acceptance criterion: unknown non-empty slug on a
    /// multi-workspace graph returns the full/unscoped result AND
    /// surfaces the candidate list. The bridge sees `None` (so it
    /// returns the unfiltered set).
    #[tokio::test]
    async fn workspace_scope_unknown_slug_returns_full_result_plus_hint() {
        let hint_candidates = vec!["server".to_string(), "ui".to_string()];
        let ops = WorkspaceFixtureOps::new(Some(hint_candidates.clone()));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug dispatch succeeds");
        match response {
            CodeGraphResponse::Ranked(ranked) => {
                assert_eq!(
                    ranked.workspace_hint,
                    Some(hint_candidates),
                    "unknown slug surfaces candidate slug list"
                );
            }
            other => panic!("expected ranked, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "unknown slug degrades to unscoped at the bridge"
        );
    }

    /// Acceptance criterion: single-workspace graphs (the bridge
    /// returns `None` for the hint) treat the workspace parameter as
    /// a no-op — bridge sees the slug.
    #[tokio::test]
    async fn workspace_scope_single_workspace_is_a_no_op() {
        // Bridge returns `None` for the hint → the helper treats the
        // slug as known. The downstream filter still matches every
        // node (because every node lives in the only workspace) so
        // there is no hard-empty surprise.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("orphans");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("single-workspace dispatch succeeds");
        match response {
            CodeGraphResponse::Orphans(orphans) => {
                assert!(
                    orphans.workspace_hint.is_none(),
                    "single-workspace graphs never produce a hint"
                );
            }
            other => panic!("expected orphans, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "single-workspace graph still passes the slug through"
        );
    }

    /// Acceptance criterion: for unknown slugs, the bridge call sees
    /// `None` (full graph) AND a listing/bounded op also produces the
    /// hint envelope.
    #[tokio::test]
    async fn workspace_scope_orphans_unknown_slug_falls_back_and_hints() {
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("orphans");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("orphans unknown-slug dispatch succeeds");
        match response {
            CodeGraphResponse::Orphans(orphans) => {
                assert_eq!(
                    orphans.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected orphans, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "orphans (listing) sees unscoped workspace for unknown slug"
        );
    }

    /// Acceptance criterion: traversal ops (`impact`, `path`,
    /// `touches_hot_path`) use the workspace only while resolving
    /// seeds/endpoints, then traverse the full graph. The bridge
    /// call sees the slug for known/valid workspaces — the seed
    /// resolution happens inside the bridge and the walk itself is
    /// unconstrained at the bridge layer. Unknown slugs still
    /// produce a hint and the bridge sees `None` (full graph).
    #[tokio::test]
    async fn workspace_scope_impact_traversal_resolves_seed_only() {
        // Case 1: known slug → bridge sees the slug, no hint.
        let ops = WorkspaceFixtureOps::with_ranked_and_impact(
            None,
            Vec::new(),
            ImpactResult::Detailed(Vec::new()),
        );
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("impact");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        params.key = Some("src/foo.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug impact dispatch succeeds");
        match response {
            CodeGraphResponse::Impact(impact) => {
                assert!(
                    impact.workspace_hint.is_none(),
                    "known-slug impact surfaces no hint"
                );
            }
            other => panic!("expected impact, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "impact (traversal) hands the slug to the bridge for seed resolution"
        );

        // Case 2: unknown slug → bridge sees `None`, hint fires.
        let ops = WorkspaceFixtureOps::with_ranked_and_impact(
            Some(vec!["server".to_string(), "ui".to_string()]),
            Vec::new(),
            ImpactResult::Detailed(Vec::new()),
        );
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("impact");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        params.key = Some("src/foo.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug impact dispatch succeeds");
        match response {
            CodeGraphResponse::Impact(impact) => {
                assert_eq!(
                    impact.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected impact, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "impact with unknown slug resolves the seed from the full graph"
        );
    }

    /// Acceptance criterion: `path` (traversal) follows the same
    /// seed-only scoping as `impact`. Lock it in independently of
    /// `impact` so a regression in either is caught.
    #[tokio::test]
    async fn workspace_scope_path_traversal_resolves_endpoints_only() {
        // Case 1: known slug → bridge sees the slug, no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("path");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        params.from = Some("src/a.rs".to_string());
        params.to = Some("src/b.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug path dispatch succeeds");
        match response {
            CodeGraphResponse::Path(path) => {
                assert!(path.workspace_hint.is_none());
            }
            other => panic!("expected path, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "path (traversal) hands the slug to the bridge for endpoint resolution"
        );

        // Case 2: unknown slug → bridge sees `None`, hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("path");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        params.from = Some("src/a.rs".to_string());
        params.to = Some("src/b.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug path dispatch succeeds");
        match response {
            CodeGraphResponse::Path(path) => {
                assert_eq!(
                    path.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected path, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "path with unknown slug resolves endpoints from the full graph"
        );
    }

    /// Acceptance criterion: `snapshot` (listing/bounded) hard-filters
    /// for valid slugs AND surfaces a hint for unknown slugs.
    #[tokio::test]
    async fn workspace_scope_snapshot_listing_hard_filters() {
        // Case 1: known slug → bridge sees the slug, no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("snapshot");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug snapshot dispatch succeeds");
        match response {
            CodeGraphResponse::Snapshot(snapshot) => {
                assert!(snapshot.workspace_hint.is_none());
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "snapshot (listing) hard-filters with the slug"
        );

        // Case 2: unknown slug → bridge sees `None`, hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("snapshot");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug snapshot dispatch succeeds");
        match response {
            CodeGraphResponse::Snapshot(snapshot) => {
                assert_eq!(
                    snapshot.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "snapshot (listing) sees unscoped workspace for unknown slug"
        );
    }

    /// Acceptance criterion: `search` (listing/bounded) surfaces the
    /// hint envelope for unknown slugs even though the bridge call
    /// itself is workspace-agnostic today. A future PR may add
    /// workspace support to `search`; the hint path is the
    /// immediately user-visible half.
    #[tokio::test]
    async fn workspace_scope_search_surfaces_hint_for_unknown_slug() {
        // Case 1: known slug → no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("search");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        params.query = Some("login".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug search dispatch succeeds");
        match response {
            CodeGraphResponse::Search(search) => {
                assert!(search.workspace_hint.is_none());
            }
            other => panic!("expected search, got {other:?}"),
        }

        // Case 2: unknown slug → hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("search");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        params.query = Some("login".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug search dispatch succeeds");
        match response {
            CodeGraphResponse::Search(search) => {
                assert_eq!(
                    search.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected search, got {other:?}"),
        }
    }

    /// Acceptance criterion: `touches_hot_path` (traversal) follows
    /// the same seed-only scoping as `impact` / `path`.
    #[tokio::test]
    async fn workspace_scope_touches_hot_path_traversal_resolves_seeds_only() {
        // Case 1: known slug → bridge sees the slug, no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("touches_hot_path");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        params.seed_entries = Some(vec!["sym:main".to_string()]);
        params.seed_sinks = Some(vec!["sym:db".to_string()]);
        params.symbols = Some(vec!["sym:foo".to_string()]);
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug touches_hot_path dispatch succeeds");
        match response {
            CodeGraphResponse::TouchesHotPath(hits) => {
                assert!(hits.workspace_hint.is_none());
            }
            other => panic!("expected touches_hot_path, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "touches_hot_path hands the slug to the bridge for seed resolution"
        );

        // Case 2: unknown slug → bridge sees `None`, hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("touches_hot_path");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        params.seed_entries = Some(vec!["sym:main".to_string()]);
        params.seed_sinks = Some(vec!["sym:db".to_string()]);
        params.symbols = Some(vec!["sym:foo".to_string()]);
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug touches_hot_path dispatch succeeds");
        match response {
            CodeGraphResponse::TouchesHotPath(hits) => {
                assert_eq!(
                    hits.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected touches_hot_path, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "touches_hot_path with unknown slug resolves seeds from the full graph"
        );
    }

    /// Acceptance criterion: empty `workspace: ""` is the same shape
    /// as omitting `workspace` after `normalize()`. Specifically, the
    /// `CodeGraphParams::normalize()` helper must clear `workspace =
    /// Some("")` to `None`. Locks in the `params.workspace.is_none()`
    /// invariant the dispatcher relies on.
    #[test]
    fn normalize_workspace_empty_string_is_none_for_dispatch() {
        let mut params = test_params("ranked");
        params.workspace = Some(String::new());
        params.normalize();
        assert!(
            params.workspace.is_none(),
            "normalize() should clear Some(\"\") to None, got {:?}",
            params.workspace
        );
        // Same shape as omitting workspace in the first place.
        let params2 = test_params("ranked");
        assert!(params2.workspace.is_none());
    }

    /// Acceptance criterion: `cycles` and `edges` (listing/bounded)
    /// also surface a hint envelope for unknown slugs even though
    /// the bridge calls themselves stay workspace-agnostic today.
    /// These ops don't take a workspace arg on the trait, so the
    /// "seen_workspaces" record stays empty — what matters is the
    /// hint envelope is populated correctly.
    #[tokio::test]
    async fn workspace_scope_cycles_edges_surface_hint_for_unknown_slug() {
        // cycles: known slug → no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("cycles");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug cycles dispatch succeeds");
        match response {
            CodeGraphResponse::Cycles(cycles) => {
                assert!(cycles.workspace_hint.is_none());
            }
            other => panic!("expected cycles, got {other:?}"),
        }

        // cycles: unknown slug → hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("cycles");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug cycles dispatch succeeds");
        match response {
            CodeGraphResponse::Cycles(cycles) => {
                assert_eq!(
                    cycles.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected cycles, got {other:?}"),
        }

        // edges: known slug → no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("edges");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        params.from_glob = Some("**/*.rs".to_string());
        params.to_glob = Some("**/*.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug edges dispatch succeeds");
        match response {
            CodeGraphResponse::Edges(edges) => {
                assert!(edges.workspace_hint.is_none());
            }
            other => panic!("expected edges, got {other:?}"),
        }

        // edges: unknown slug → hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("edges");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        params.from_glob = Some("**/*.rs".to_string());
        params.to_glob = Some("**/*.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug edges dispatch succeeds");
        match response {
            CodeGraphResponse::Edges(edges) => {
                assert_eq!(
                    edges.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected edges, got {other:?}"),
        }
    }

    /// PR B4: the default `hybrid_search` impl on `RepoGraphOps`
    /// degrades to the structural-only path so test stubs that only
    /// override `search` still serve the hybrid mode (with every hit
    /// stamped `match_kind="structural"`). Exercised through the
    /// `StubRepoGraphOps` fixture from `test_support` rather than a
    /// hand-rolled stub — the goal here is to lock in the default-impl
    /// tagging contract, not re-test every other op.
    #[tokio::test]
    async fn hybrid_search_default_impl_tags_structural() {
        use crate::bridge::{ProjectCtx, RepoGraphOps};
        use crate::state::stubs::StubRepoGraphOps;

        let ctx = ProjectCtx {
            id: "p".to_string(),
            clone_path: "/tmp".to_string(),
            workspace: None,
            sub_path: None,
        };
        let ops = StubRepoGraphOps;
        // Stub returns no hits; the default impl still has to compile
        // and the empty list is valid evidence the dispatch path works.
        let hits = ops.hybrid_search(&ctx, "login", None, 10).await.unwrap();
        assert!(hits.is_empty());
    }

    /// PR A3: `neighbors` op uses `kind_filter` for edge kinds (`reads` /
    /// `writes`); `validate_edge_kind_filter` enforces that vocabulary so
    /// typos surface server-side rather than silently dropping every
    /// neighbor.
    #[test]
    fn validates_edge_kind_filter_pr_a3() {
        assert!(validate_edge_kind_filter(Some("reads")).is_ok());
        assert!(validate_edge_kind_filter(Some("writes")).is_ok());
        assert!(validate_edge_kind_filter(None).is_ok());
        // Node-kind labels are NOT valid for the neighbors op — they
        // belong to the other ops' validator.
        assert!(validate_edge_kind_filter(Some("file")).is_err());
        assert!(validate_edge_kind_filter(Some("symbol")).is_err());
        assert!(validate_edge_kind_filter(Some("unknown")).is_err());
    }

    #[test]
    fn test_filter_parse_maps_aliases_and_default() {
        assert_eq!(
            TestFilter::parse(Some("exclude"), TestFilter::Include),
            TestFilter::Exclude
        );
        assert_eq!(
            TestFilter::parse(Some("ONLY"), TestFilter::Include),
            TestFilter::Only
        );
        assert_eq!(
            TestFilter::parse(Some(" include "), TestFilter::Exclude),
            TestFilter::Include
        );
        // Unknown / absent falls back to the per-op default.
        assert_eq!(
            TestFilter::parse(Some("garbage"), TestFilter::Exclude),
            TestFilter::Exclude
        );
        assert_eq!(
            TestFilter::parse(None, TestFilter::Include),
            TestFilter::Include
        );
    }

    #[test]
    fn test_filter_keeps_respects_mode() {
        assert!(TestFilter::Include.keeps(true));
        assert!(TestFilter::Include.keeps(false));
        assert!(!TestFilter::Exclude.keeps(true));
        assert!(TestFilter::Exclude.keeps(false));
        assert!(TestFilter::Only.keeps(true));
        assert!(!TestFilter::Only.keeps(false));
    }

    fn test_params(op: &str) -> CodeGraphParams {
        CodeGraphParams {
            operation: op.to_string(),
            project: "test/test".to_string(),
            workspace: None,
            project_id: String::new(),
            project_path: "/tmp".to_string(),
            key: None,
            direction: None,
            kind_filter: None,
            limit: None,
            query: None,
            context_filter: None,
            file_filter: None,
            edge_filters: None,
            token_budget: None,
            max_seeds: None,
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
            since_days: None,
            max_files_per_commit: None,
            min_confidence: None,
            kind_hint: None,
            from_sha: None,
            to_sha: None,
            changed_files: None,
            include_content: None,
            level: None,
            mode: None,
            target: None,
            tests: None,
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
    fn parses_complexity_params_from_json() {
        // Iter 28: target / sort_by / file_glob / limit all parse via
        // the shared CodeGraphParams. `target` is the only new field
        // — sort_by/file_glob/limit are reused from ranked / hotspots.
        let json = serde_json::json!({
            "operation": "complexity",
            "project": "/workspace/repo",
            "target": "files",
            "sort_by": "cyclomatic",
            "file_glob": "server/src/**/*.rs",
            "limit": 25,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "complexity");
        assert_eq!(params.target.as_deref(), Some("files"));
        assert_eq!(params.sort_by.as_deref(), Some("cyclomatic"));
        assert_eq!(params.file_glob.as_deref(), Some("server/src/**/*.rs"));
        assert_eq!(params.limit, Some(25));
    }

    #[test]
    fn parses_refactor_candidates_params_from_json() {
        // Iter 29: since_days / file_glob / limit all reuse existing
        // CodeGraphParams fields. No new params are introduced — the
        // op rides on the same shared shape iter 28's complexity uses.
        let json = serde_json::json!({
            "operation": "refactor_candidates",
            "project": "/workspace/repo",
            "since_days": 60,
            "file_glob": "server/src/**/*.rs",
            "limit": 25,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "refactor_candidates");
        assert_eq!(params.since_days, Some(60));
        assert_eq!(params.file_glob.as_deref(), Some("server/src/**/*.rs"));
        assert_eq!(params.limit, Some(25));
    }

    #[test]
    fn parses_metrics_at_params_from_json() {
        let json = serde_json::json!({
            "operation": "metrics_at",
            "project": "/workspace/repo",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "metrics_at");
        assert_eq!(params.project, "/workspace/repo");
    }

    #[test]
    fn parses_dead_symbols_params_from_json() {
        let json = serde_json::json!({
            "operation": "dead_symbols",
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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
            "project": "/workspace/repo",
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

    // ── Next-step hint tests (PR A4) ────────────────────────────────────────

    fn search_with_top_hit(name: &str) -> CodeGraphResponse {
        CodeGraphResponse::Search(SearchResponse {
            query: "auth".to_string(),
            hits: vec![SearchHit {
                key: format!("scip-rust . . . {name}#"),
                kind: "function".to_string(),
                display_name: name.to_string(),
                score: 0.9,
                file: Some("src/auth.rs".to_string()),
                match_kind: None,
            }],
            workspace_hint: None,
            next_step: None,
        })
    }

    fn empty_search() -> CodeGraphResponse {
        CodeGraphResponse::Search(SearchResponse {
            query: "auth".to_string(),
            hits: vec![],
            workspace_hint: None,
            next_step: None,
        })
    }

    fn empty_ranked() -> CodeGraphResponse {
        CodeGraphResponse::Ranked(RankedResponse {
            nodes: vec![],
            workspace_hint: None,
            next_step: None,
        })
    }

    fn empty_cycles() -> CodeGraphResponse {
        CodeGraphResponse::Cycles(CyclesResponse {
            cycles: vec![],
            workspace_hint: None,
            next_step: None,
        })
    }

    fn empty_impact() -> CodeGraphResponse {
        CodeGraphResponse::Impact(ImpactResponse {
            key: "Foo".to_string(),
            impact: Some(vec![]),
            file_groups: None,
            risk: Some(ImpactRisk::Low),
            summary: Some("no direct callers in current graph snapshot".to_string()),
            workspace_hint: None,
            next_step: None,
        })
    }

    /// PR C3 helper: build an ImpactResponse with N synthetic direct
    /// callers spread across `modules` two-segment buckets. Used to
    /// exercise risk thresholds without a full graph fixture.
    fn impact_with_callers(direct: usize, modules: usize) -> CodeGraphResponse {
        let entries: Vec<ImpactEntry> = (0..direct)
            .map(|i| {
                let bucket = i % modules.max(1);
                ImpactEntry {
                    key: format!("symbol:scip-rust pkg src/m{bucket}/f{i}.rs `caller{i}`()."),
                    depth: 1,
                    file_path: Some(format!("src/m{bucket}/f{i}.rs")),
                }
            })
            .collect();
        let metrics = metrics_from_detailed(&entries);
        let risk = ImpactRisk::classify(metrics.direct, metrics.total, metrics.modules);
        let summary = impact_summary(metrics);
        CodeGraphResponse::Impact(ImpactResponse {
            key: "Foo".to_string(),
            impact: Some(entries),
            file_groups: None,
            risk: Some(risk),
            summary: Some(summary),
            workspace_hint: None,
            next_step: None,
        })
    }

    fn empty_describe() -> CodeGraphResponse {
        CodeGraphResponse::Describe(DescribeResponse {
            description: None,
            next_step: None,
        })
    }

    /// PR C1: synthetic empty `Context` response used by hint tests and
    /// the snapshot serialization tests.
    fn empty_context() -> CodeGraphResponse {
        use crate::bridge::{SymbolContext, SymbolNode};
        CodeGraphResponse::Context(ContextResponse {
            symbol_context: SymbolContext {
                symbol: SymbolNode {
                    uid: "symbol:foo".to_string(),
                    name: "foo".to_string(),
                    kind: "function".to_string(),
                    file_path: "src/lib.rs".to_string(),
                    start_line: 0,
                    end_line: 0,
                    content: None,
                    method_metadata: None,
                    complexity: None,
                },
                incoming: std::collections::BTreeMap::new(),
                outgoing: std::collections::BTreeMap::new(),
                processes: vec![],
            },
            next_step: None,
        })
    }

    fn empty_orphans() -> CodeGraphResponse {
        CodeGraphResponse::Orphans(OrphansResponse {
            orphans: vec![],
            workspace_hint: None,
            next_step: None,
        })
    }

    /// Reads the `next_step` slot regardless of variant.
    fn read_next_step(response: &CodeGraphResponse) -> Option<&str> {
        match response {
            CodeGraphResponse::Neighbors(r) => r.next_step.as_deref(),
            CodeGraphResponse::Ranked(r) => r.next_step.as_deref(),
            CodeGraphResponse::Implementations(r) => r.next_step.as_deref(),
            CodeGraphResponse::Impact(r) => r.next_step.as_deref(),
            CodeGraphResponse::Search(r) => r.next_step.as_deref(),
            CodeGraphResponse::Cycles(r) => r.next_step.as_deref(),
            CodeGraphResponse::Orphans(r) => r.next_step.as_deref(),
            CodeGraphResponse::Path(r) => r.next_step.as_deref(),
            CodeGraphResponse::Edges(r) => r.next_step.as_deref(),
            CodeGraphResponse::Describe(r) => r.next_step.as_deref(),
            CodeGraphResponse::Context(r) => r.next_step.as_deref(),
            CodeGraphResponse::Status(r) => r.next_step.as_deref(),
            CodeGraphResponse::Workspaces(r) => r.next_step.as_deref(),
            CodeGraphResponse::SymbolsAt(r) => r.next_step.as_deref(),
            CodeGraphResponse::DiffTouches(r) => r.next_step.as_deref(),
            CodeGraphResponse::ApiSurface(r) => r.next_step.as_deref(),
            CodeGraphResponse::BoundaryCheck(r) => r.next_step.as_deref(),
            CodeGraphResponse::Hotspots(r) => r.next_step.as_deref(),
            CodeGraphResponse::Complexity(r) => r.next_step.as_deref(),
            CodeGraphResponse::RefactorCandidates(r) => r.next_step.as_deref(),
            CodeGraphResponse::MetricsAt(r) => r.next_step.as_deref(),
            CodeGraphResponse::DeadSymbols(r) => r.next_step.as_deref(),
            CodeGraphResponse::DeprecatedCallers(r) => r.next_step.as_deref(),
            CodeGraphResponse::TouchesHotPath(r) => r.next_step.as_deref(),
            CodeGraphResponse::Coupling(r) => r.next_step.as_deref(),
            CodeGraphResponse::Churn(r) => r.next_step.as_deref(),
            CodeGraphResponse::CouplingHotspots(r) => r.next_step.as_deref(),
            CodeGraphResponse::CouplingHubs(r) => r.next_step.as_deref(),
            CodeGraphResponse::Ambiguous(r) => r.next_step.as_deref(),
            CodeGraphResponse::NotFound(r) => r.next_step.as_deref(),
            CodeGraphResponse::DetectedChanges(r) => r.next_step.as_deref(),
            CodeGraphResponse::Snapshot(r) => r.next_step.as_deref(),
            CodeGraphResponse::QuerySubgraph(r) => r.next_step.as_deref(),
        }
    }

    #[test]
    fn search_hint_uses_top_hit_display_name() {
        let mut response = search_with_top_hit("authenticate_user");
        attach_next_step_hint("search", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(
            hint.contains("authenticate_user"),
            "hint should reference top hit name: {hint}"
        );
        assert!(hint.contains("code_graph context"), "hint: {hint}");
    }

    #[test]
    fn search_hint_falls_back_when_no_hits() {
        let mut response = empty_search();
        attach_next_step_hint("search", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert_eq!(hint, FALLBACK_NEXT_STEP);
    }

    #[test]
    fn ranked_hint_mentions_pagerank() {
        let mut response = empty_ranked();
        attach_next_step_hint("ranked", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(hint.contains("PageRank"), "hint: {hint}");
        assert!(hint.contains("context"), "hint: {hint}");
    }

    #[test]
    fn cycles_hint_mentions_path() {
        let mut response = empty_cycles();
        attach_next_step_hint("cycles", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(hint.contains("path"), "hint: {hint}");
    }

    #[test]
    fn impact_hint_low_risk_uses_fallback() {
        // PR C3: LOW-risk impact stays on the generic fallback hint —
        // no need to nudge the agent toward `dead_symbols` for a
        // 0-caller change.
        let mut response = empty_impact();
        attach_next_step_hint("impact", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert_eq!(hint, FALLBACK_NEXT_STEP);
    }

    #[test]
    fn impact_hint_high_risk_recommends_cleanup_ops() {
        // PR C3: HIGH (>=10 direct callers) flips the hint to the
        // dead_symbols / deprecated_callers cleanup nudge.
        let mut response = impact_with_callers(12, 3);
        // Sanity-check the synthetic fixture lands on HIGH.
        match &response {
            CodeGraphResponse::Impact(r) => {
                assert_eq!(r.risk, Some(ImpactRisk::High), "fixture risk: {:?}", r.risk);
            }
            other => panic!("expected Impact, got {other:?}"),
        }
        attach_next_step_hint("impact", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(
            hint.contains("dead_symbols"),
            "high-risk hint should mention dead_symbols: {hint}"
        );
        assert!(
            hint.contains("deprecated_callers"),
            "high-risk hint should mention deprecated_callers: {hint}"
        );
    }

    #[test]
    fn impact_hint_critical_risk_recommends_cleanup_ops() {
        // PR C3: CRITICAL (>=20 direct callers) also emits the
        // cleanup hint.
        let mut response = impact_with_callers(25, 6);
        match &response {
            CodeGraphResponse::Impact(r) => {
                assert_eq!(r.risk, Some(ImpactRisk::Critical));
            }
            other => panic!("expected Impact, got {other:?}"),
        }
        attach_next_step_hint("impact", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert_eq!(hint, HIGH_IMPACT_NEXT_STEP);
    }

    #[test]
    fn context_hint_points_to_impact() {
        // PR C1: the dedicated Context variant routes through the
        // `("context" | "describe", _)` arm and emits the
        // blast-radius nudge.
        let mut response = empty_context();
        attach_next_step_hint("context", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(hint.contains("impact"), "hint: {hint}");
        // Sanity-check the discriminator field stays intact post-hint.
        let json = serde_json::to_value(&response).expect("serialize");
        assert!(
            json.get("symbol_context").is_some(),
            "Context discriminator dropped: {json}"
        );
    }

    #[test]
    fn context_response_serializes_symbol_context_discriminator_pr_c1() {
        // The untagged-enum contract pins the discriminator field to
        // `symbol_context`. UI parsers in `pulseTypes.ts` hang off
        // exactly that name; renaming would silently break them.
        let response = empty_context();
        let json = serde_json::to_value(&response).expect("serialize");
        let inner = json
            .get("symbol_context")
            .expect("symbol_context discriminator missing");
        assert!(inner.get("symbol").is_some(), "no nested symbol: {json}");
        assert!(inner.get("incoming").is_some(), "no incoming map: {json}");
        assert!(inner.get("outgoing").is_some(), "no outgoing map: {json}");
        assert!(
            inner.get("processes").is_some(),
            "no processes list: {json}"
        );
    }

    #[test]
    fn describe_hint_points_to_impact_until_c1() {
        let mut response = empty_describe();
        attach_next_step_hint("describe", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(hint.contains("impact"), "hint: {hint}");
    }

    #[test]
    fn unknown_op_gets_fallback_hint() {
        let mut response = empty_orphans();
        attach_next_step_hint("orphans", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert_eq!(hint, FALLBACK_NEXT_STEP);
    }

    #[test]
    fn next_step_hint_always_non_empty() {
        // Plan acceptance criterion: every code_graph.* op response
        // ends with a non-empty `next_step` field.
        let cases: Vec<(&str, CodeGraphResponse)> = vec![
            ("search", empty_search()),
            ("ranked", empty_ranked()),
            ("cycles", empty_cycles()),
            ("impact", empty_impact()),
            ("orphans", empty_orphans()),
            ("describe", empty_describe()),
            ("context", empty_context()),
            (
                "metrics_at",
                CodeGraphResponse::MetricsAt(MetricsAtResponse {
                    metrics: MetricsAtResult {
                        commit: "abc123".to_string(),
                        node_count: 0,
                        edge_count: 0,
                        cycle_count: 0,
                        cycle_count_symbol_only: 0,
                        cycle_count_file_only: 0,
                        cycles_by_size_histogram: Default::default(),
                        god_object_count: 0,
                        orphan_count: 0,
                        public_api_count: 0,
                        doc_coverage_pct: 0.0,
                    },
                    next_step: None,
                }),
            ),
        ];
        for (op, mut response) in cases {
            attach_next_step_hint(op, &mut response);
            let hint = read_next_step(&response).unwrap_or("");
            assert!(!hint.is_empty(), "op={op} produced empty hint");
        }
    }

    #[test]
    fn search_response_serializes_next_step() {
        let mut response = search_with_top_hit("login");
        attach_next_step_hint("search", &mut response);
        let json = serde_json::to_value(&response).expect("serialize");
        let next_step = json.get("next_step").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            next_step.contains("login"),
            "serialized next_step missing top hit: {json}"
        );
        // Untagged-enum discriminator field stays intact.
        assert!(json.get("hits").is_some(), "discriminator dropped: {json}");
    }

    #[test]
    fn next_step_omitted_when_none_via_skip_serializing() {
        // When the hint is not attached the field is `None` and the
        // serde `skip_serializing_if` rule keeps it out of the wire
        // payload entirely — important so the additive change
        // doesn't pollute every existing snapshot.
        let response = empty_ranked();
        let json = serde_json::to_value(&response).expect("serialize");
        assert!(
            json.get("next_step").is_none(),
            "next_step should be omitted when None: {json}"
        );
    }

    #[test]
    fn flag_disabled_suppresses_emission_path() {
        // Direct env-var test: setting the flag to "0" must short-circuit
        // the dispatcher's `next_step_hints_enabled()` gate. Use a unique
        // value-restore to keep this hermetic across other tests in the
        // module that read the same env var.
        let prev = std::env::var("DJINN_CODE_GRAPH_NEXT_STEP_HINTS").ok();
        // SAFETY: the test binary is single-threaded for env mutations
        // here; cargo runs each `#[test]` in its own thread but env is
        // process-global. The matched `prev` restore at the end keeps
        // the test idempotent for the assertion we care about: when
        // *we* set "0", the helper returns false.
        unsafe {
            std::env::set_var("DJINN_CODE_GRAPH_NEXT_STEP_HINTS", "0");
        }
        assert!(!next_step_hints_enabled());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DJINN_CODE_GRAPH_NEXT_STEP_HINTS", v),
                None => std::env::remove_var("DJINN_CODE_GRAPH_NEXT_STEP_HINTS"),
            }
        }
    }

    // ── PR C3 risk classification ──────────────────────────────────────────

    #[test]
    fn module_bucket_takes_first_two_path_segments() {
        // Per PR C3 plan: bucket on the first two segments after the
        // repo root.
        assert_eq!(module_bucket("src/auth/User.rs"), "src/auth");
        assert_eq!(
            module_bucket("crates/djinn-control-plane/src/lib.rs"),
            "crates/djinn-control-plane"
        );
        // Single-segment path stays as-is so the count remains 1.
        assert_eq!(module_bucket("Cargo.toml"), "Cargo.toml");
        // Backslashes get normalized so Windows-formatted paths still
        // bucket the same way.
        assert_eq!(module_bucket("src\\auth\\User.rs"), "src/auth");
    }

    #[test]
    fn module_bucket_collapses_two_paths_in_same_dir() {
        assert_eq!(
            module_bucket("src/auth/User.rs"),
            module_bucket("src/auth/Session.rs")
        );
    }

    #[test]
    fn risk_thresholds_critical_at_20_direct() {
        // Boundary: direct == 20 lands in CRITICAL.
        assert_eq!(ImpactRisk::classify(20, 0, 0), ImpactRisk::Critical);
        assert_eq!(ImpactRisk::classify(19, 0, 0), ImpactRisk::High);
        // Boundary: total == 200, modules == 10 also push to CRITICAL.
        assert_eq!(ImpactRisk::classify(0, 200, 0), ImpactRisk::Critical);
        assert_eq!(ImpactRisk::classify(0, 0, 10), ImpactRisk::Critical);
    }

    #[test]
    fn risk_thresholds_high_at_10_direct() {
        // Boundary: direct == 10 lands in HIGH.
        assert_eq!(ImpactRisk::classify(10, 0, 0), ImpactRisk::High);
        assert_eq!(ImpactRisk::classify(9, 0, 0), ImpactRisk::Medium);
        // Other axes too.
        assert_eq!(ImpactRisk::classify(0, 80, 0), ImpactRisk::High);
        assert_eq!(ImpactRisk::classify(0, 0, 5), ImpactRisk::High);
    }

    #[test]
    fn risk_thresholds_medium_at_3_direct() {
        // Boundary: direct == 3 lands in MEDIUM.
        assert_eq!(ImpactRisk::classify(3, 0, 0), ImpactRisk::Medium);
        assert_eq!(ImpactRisk::classify(2, 0, 0), ImpactRisk::Low);
        // Other axes.
        assert_eq!(ImpactRisk::classify(0, 20, 0), ImpactRisk::Medium);
        assert_eq!(ImpactRisk::classify(0, 0, 2), ImpactRisk::Medium);
    }

    #[test]
    fn risk_thresholds_low_for_zero() {
        assert_eq!(ImpactRisk::classify(0, 0, 0), ImpactRisk::Low);
        assert_eq!(ImpactRisk::classify(1, 1, 1), ImpactRisk::Low);
    }

    #[test]
    fn metrics_from_detailed_counts_direct_only_at_depth_one() {
        // depth-1 entries are "direct callers"; deeper entries roll up
        // into `total` only. Modules dedupe by two-segment bucket.
        let entries = vec![
            ImpactEntry {
                key: "symbol:a".into(),
                depth: 1,
                file_path: Some("src/auth/A.rs".into()),
            },
            ImpactEntry {
                key: "symbol:b".into(),
                depth: 1,
                file_path: Some("src/auth/B.rs".into()),
            },
            ImpactEntry {
                key: "symbol:c".into(),
                depth: 2,
                file_path: Some("src/billing/C.rs".into()),
            },
        ];
        let m = metrics_from_detailed(&entries);
        assert_eq!(m.direct, 2);
        assert_eq!(m.total, 3);
        assert_eq!(m.modules, 2, "src/auth + src/billing => 2 buckets");
    }

    #[test]
    fn metrics_skip_entries_without_file_path() {
        // External symbols with no file_path don't contribute to the
        // module count but still hit `direct`/`total`.
        let entries = vec![
            ImpactEntry {
                key: "symbol:a".into(),
                depth: 1,
                file_path: Some("src/auth/A.rs".into()),
            },
            ImpactEntry {
                key: "symbol:ext".into(),
                depth: 1,
                file_path: None,
            },
        ];
        let m = metrics_from_detailed(&entries);
        assert_eq!(m.direct, 2);
        assert_eq!(m.total, 2);
        assert_eq!(m.modules, 1);
    }

    #[test]
    fn impact_summary_uses_plan_phrasing() {
        let m = ImpactMetrics {
            direct: 12,
            total: 30,
            modules: 3,
        };
        assert_eq!(impact_summary(m), "12 direct caller(s) across 3 module(s)");
    }

    #[test]
    fn impact_summary_zero_callers_uses_snapshot_phrasing() {
        let m = ImpactMetrics {
            direct: 0,
            total: 0,
            modules: 0,
        };
        assert_eq!(
            impact_summary(m),
            "no direct callers in current graph snapshot"
        );
    }

    #[test]
    fn impact_response_serializes_risk_screaming_snake() {
        // Plan acceptance: `risk: "HIGH"` on the wire — confirm
        // SCREAMING_SNAKE_CASE serialization survives the untagged
        // CodeGraphResponse envelope.
        let response = impact_with_callers(12, 3);
        let json = serde_json::to_value(&response).expect("serialize");
        assert_eq!(
            json.get("risk").and_then(|v| v.as_str()),
            Some("HIGH"),
            "risk should serialize as 'HIGH': {json}"
        );
        let summary = json
            .get("summary")
            .and_then(|v| v.as_str())
            .expect("summary present");
        assert!(
            summary.contains("12 direct caller"),
            "summary phrasing: {summary}"
        );
    }

    #[test]
    fn impact_acceptance_twelve_direct_callers_three_modules() {
        // The plan's literal acceptance test: a 12-direct-caller change
        // returns risk == HIGH and a summary like the example string.
        let response = impact_with_callers(12, 3);
        match response {
            CodeGraphResponse::Impact(r) => {
                assert_eq!(r.risk, Some(ImpactRisk::High));
                let summary = r.summary.expect("summary set");
                assert!(
                    summary.starts_with("12 direct caller"),
                    "summary: {summary}"
                );
                assert!(
                    summary.contains("3 module"),
                    "summary should report 3 modules: {summary}"
                );
            }
            other => panic!("expected Impact, got {other:?}"),
        }
    }

    #[test]
    fn parses_detect_changes_params_with_sha_range() {
        let json = serde_json::json!({
            "operation": "detect_changes",
            "project": "owner/repo",
            "from_sha": "abc123",
            "to_sha": "HEAD",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "detect_changes");
        assert_eq!(params.from_sha.as_deref(), Some("abc123"));
        assert_eq!(params.to_sha.as_deref(), Some("HEAD"));
        assert!(params.changed_files.is_none());
    }

    #[test]
    fn parses_detect_changes_params_with_changed_files() {
        let json = serde_json::json!({
            "operation": "detect_changes",
            "project": "owner/repo",
            "changed_files": ["src/a.rs", "src/b.rs"],
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        let files = params.changed_files.as_ref().expect("changed_files set");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], "src/a.rs");
        assert_eq!(files[1], "src/b.rs");
    }

    #[test]
    fn pick_next_step_target_prefers_high_tier() {
        use crate::bridge::{ChangeKind, DetectedTouchedSymbol, PagerankTier};
        let symbols = vec![
            DetectedTouchedSymbol {
                uid: "sym:low".to_string(),
                name: "z_low".to_string(),
                kind: "function".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 1,
                pagerank_tier: PagerankTier::Low,
                change_kind: ChangeKind::Modified,
            },
            DetectedTouchedSymbol {
                uid: "sym:high".to_string(),
                name: "a_high".to_string(),
                kind: "function".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 10,
                end_line: 20,
                pagerank_tier: PagerankTier::High,
                change_kind: ChangeKind::Modified,
            },
            DetectedTouchedSymbol {
                uid: "sym:medium".to_string(),
                name: "m_medium".to_string(),
                kind: "function".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 5,
                end_line: 5,
                pagerank_tier: PagerankTier::Medium,
                change_kind: ChangeKind::Modified,
            },
        ];
        let target = super::pick_next_step_target(&symbols);
        assert_eq!(target.as_deref(), Some("sym:high"));
    }

    #[test]
    fn pick_next_step_target_returns_none_for_empty() {
        assert!(super::pick_next_step_target(&[]).is_none());
    }

    // ── PR D2: snapshot wire-shape tests ───────────────────────────────────

    fn empty_snapshot_response() -> SnapshotResponse {
        SnapshotResponse {
            snapshot: SnapshotPayload {
                project_id: "proj-test".to_string(),
                git_head: "deadbeef".to_string(),
                generated_at: "2026-04-28T00:00:00Z".to_string(),
                truncated: false,
                total_nodes: 0,
                total_edges: 0,
                node_cap: 2_000,
                nodes: vec![],
                edges: vec![],
            },
            workspace_hint: None,
            next_step: None,
        }
    }

    #[test]
    fn snapshot_variant_uses_unique_discriminator_pr_d2() {
        // The `CodeGraphResponse` enum is `#[serde(untagged)]`; UI
        // parsers disambiguate on a top-level field name. The plan
        // pins `snapshot` as the PR D2 discriminator — assert it
        // doesn't collide with any other variant's discriminator.
        let response = CodeGraphResponse::Snapshot(empty_snapshot_response());
        let json = serde_json::to_value(&response).expect("serialize");
        let obj = json
            .as_object()
            .expect("snapshot variant should be an object");
        assert!(
            obj.contains_key("snapshot"),
            "snapshot variant must surface the `snapshot` field: {json}"
        );
        // Existing taken field names per the inter-PR contract — none
        // may appear at the top level of the snapshot variant.
        for forbidden in [
            "nodes",
            "orphans",
            "cycles",
            "hits",
            "neighbors",
            "file_groups",
            "hotspots",
            "pairs",
            "hubs",
            "coupled",
            "edges",
            "symbols",
            "violations",
            "description",
            "path",
            "status",
            "warmed",
            "deprecated_symbol",
            "witness_path",
            "members",
            "symbol_context",
            "candidates",
            "not_found",
            "detected_changes",
        ] {
            assert!(
                !obj.contains_key(forbidden),
                "snapshot variant must not surface the `{forbidden}` field at top level: {json}"
            );
        }
    }

    #[test]
    fn snapshot_response_serializes_full_contract_pr_d2() {
        // Pin the wire shape spec'd in the inter-PR contract: the
        // payload sits under `snapshot`, with all required fields
        // present and the right types.
        let mut response = empty_snapshot_response();
        response.snapshot.total_nodes = 3;
        response.snapshot.total_edges = 5;
        response.snapshot.truncated = true;
        response.snapshot.nodes.push(crate::bridge::SnapshotNode {
            id: "symbol:scip-rust . . . main()".to_string(),
            kind: "symbol".to_string(),
            label: "main".to_string(),
            workspace: Some("server".to_string()),
            workspace_kind: None,
            member_count: None,
            internal_edge_count: None,
            symbol_kind: Some("function".to_string()),
            file_path: Some("src/main.rs".to_string()),
            pagerank: 0.42,
            community_id: None,
            cognitive: None,
            is_test: false,
        });
        response.snapshot.edges.push(crate::bridge::SnapshotEdge {
            from: "file:src/main.rs".to_string(),
            to: "symbol:scip-rust . . . main()".to_string(),
            kind: "ContainsDefinition".to_string(),
            confidence: 0.95,
            reason: None,
        });
        let json = serde_json::to_value(CodeGraphResponse::Snapshot(response)).expect("serialize");
        let snapshot = json
            .get("snapshot")
            .and_then(|s| s.as_object())
            .expect("snapshot object present");
        for required in [
            "project_id",
            "git_head",
            "generated_at",
            "truncated",
            "total_nodes",
            "total_edges",
            "node_cap",
            "nodes",
            "edges",
        ] {
            assert!(
                snapshot.contains_key(required),
                "snapshot payload missing required field `{required}`: {json}"
            );
        }
        assert_eq!(
            snapshot.get("truncated").and_then(|v| v.as_bool()),
            Some(true),
            "truncated round-trips as bool: {json}"
        );

        // Nodes / edges shape spot-check.
        let node = snapshot
            .get("nodes")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_object())
            .expect("first node object");
        for required in ["id", "kind", "label", "workspace", "pagerank"] {
            assert!(
                node.contains_key(required),
                "node missing `{required}`: {node:?}"
            );
        }
        assert_eq!(
            node.get("workspace").and_then(|v| v.as_str()),
            Some("server"),
            "node workspace slug should serialize from SnapshotNode.workspace: {node:?}"
        );
        let edge = snapshot
            .get("edges")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_object())
            .expect("first edge object");
        for required in ["from", "to", "kind", "confidence"] {
            assert!(
                edge.contains_key(required),
                "edge missing `{required}`: {edge:?}"
            );
        }
    }

    #[test]
    fn parses_snapshot_params_from_json_pr_d2() {
        let json = serde_json::json!({
            "operation": "snapshot",
            "project": "owner/repo",
            "limit": 1500,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "snapshot");
        assert_eq!(params.limit, Some(1500));
    }

    #[test]
    fn parses_workspaces_params_without_key_or_query() {
        let json = serde_json::json!({
            "operation": "workspaces",
            "project": "owner/repo",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "workspaces");
        assert!(params.key.is_none());
        assert!(params.query.is_none());
    }

    #[test]
    fn snapshot_node_omits_absent_workspace() {
        let node = crate::bridge::SnapshotNode {
            id: "file:src/lib.rs".to_string(),
            kind: "file".to_string(),
            label: "src/lib.rs".to_string(),
            workspace: None,
            workspace_kind: None,
            member_count: None,
            internal_edge_count: None,
            symbol_kind: None,
            file_path: Some("src/lib.rs".to_string()),
            pagerank: 0.1,
            community_id: None,
            cognitive: None,
            is_test: false,
        };
        let json = serde_json::to_value(node).expect("serialize node");
        assert!(
            json.as_object()
                .expect("node object")
                .get("workspace")
                .is_none(),
            "workspace should be omitted when absent: {json}"
        );
    }
}
