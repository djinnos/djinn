use super::*;

// ── Request types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CodeGraphParams {
    /// The operation to perform.
    /// One of: `neighbors`, `ranked`, `impact`, `implementations`,
    /// `search`, `query_subgraph`, `route_map`, `shape_check`,
    /// `api_impact`, `flow`, `cycles`, `orphans`, `path`, `edges`,
    /// `symbols_at`, `diff_touches`, `detect_changes`, `describe`,
    /// `context`, `status`, `snapshot`, `workspaces`, and the other
    /// graph analysis ops.
    pub operation: String,
    /// Project identifier — either the UUID (`project_id`) or the
    /// canonical `"owner/repo"` slug. The handler resolves it to the
    /// server-managed clone path via `djinn_core::paths::project_dir`
    /// before dispatching to the graph backend.
    pub project: String,
    /// Optional workspace slug. Empty string is normalized to omitted. Use
    /// `operation = "workspaces"` to enumerate valid slugs and metadata
    /// (`slug`, `name`, `node_count`, `commit_sha`, `warmed_at`, `status`).
    /// Known workspaces hard-scope listing/bounded ops such as `ranked`,
    /// `orphans`, `snapshot`, and `api_surface`. Traversal ops such as
    /// `impact`, `path`, `touches_hot_path`, and `query_subgraph` use the
    /// workspace only while resolving seeds/endpoints, then keep the traversal
    /// cross-workspace so blast radius stays visible. Unknown non-empty slugs
    /// return unscoped results with `workspace_hint` candidate slugs where the
    /// response type supports hints; single-workspace graphs treat the parameter
    /// as a no-op.
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
    /// - `flow`: flow-hit tier — `process` or `step`.
    #[serde(default)]
    pub kind_filter: Option<String>,
    /// Maximum results for list operations. Defaults are op-specific:
    /// `ranked`/`search`/`neighbors`/`flow` default 20, `route_map`/
    /// `api_impact` default 50, `edges` default 100. For `impact`, this is
    /// max traversal depth (default 3). Negative values are treated as zero by
    /// legacy ops; new route/API/flow ops reject negative limits.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Query text, op-specific:
    /// - `search`: substring/name lookup text.
    /// - `query_subgraph`: required nonblank natural-language question used
    ///   to pick relevant seeds and infer useful traversal edge kinds.
    /// - `flow`: required nonblank natural-language query re-ranked over
    ///   process/step matches.
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
    /// HTTP method for route-aware ops (`route_map`, `shape_check`,
    /// `api_impact`) when `route_id` is not supplied.
    #[serde(default)]
    pub method: Option<String>,
    /// Exact route path for `shape_check` / `api_impact` when `route_id` is
    /// not supplied.
    #[serde(default)]
    pub path: Option<String>,
    /// Source path glob for `edges`.
    #[serde(default)]
    pub from_glob: Option<String>,
    /// Destination path glob for `edges`.
    #[serde(default)]
    pub to_glob: Option<String>,
    /// Stable route id for route-aware ops. For `shape_check` and
    /// `api_impact`, callers must provide either `route_id` or both
    /// `method` and `path`.
    #[serde(default)]
    pub route_id: Option<String>,
    /// Route path glob for `route_map` discovery.
    #[serde(default)]
    pub path_glob: Option<String>,
    /// Optional route framework filter for `route_map` discovery.
    #[serde(default)]
    pub framework: Option<String>,
    /// Include optional response fields when computing `shape_check` drift.
    /// Defaults to `false`.
    #[serde(default)]
    pub include_optional: Option<bool>,
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
    /// df6s: page offset for paginated traversal ops (`neighbors`,
    /// `impact`, `coupling_hotspots`). Sliced **only** when
    /// constructing the agent-facing response DTO — the underlying
    /// `RepoDependencyGraph` traversal always runs to completion so
    /// `total` reflects the unsliced result count and pagination can
    /// never be misread as "the graph has no more nodes". Empty
    /// string is normalized to `None` via `normalize()`. Negative
    /// values are clamped to zero at the handler boundary.
    #[serde(default)]
    pub offset: Option<i64>,
    /// df6s: counts-only mode for paginated traversal ops
    /// (`neighbors`, `impact`, `coupling_hotspots`). When `Some(true)`,
    /// the response omits the large node/pair lists and instead ships
    /// `total` + the relevant counters (`by_depth_counts` for impact).
    /// Designed for triage: a model can ask "how big is the blast
    /// radius?" without paying the token cost of every impacted
    /// symbol. The internal traversal still runs to completion so
    /// group_by-file rollups report the full per-module count.
    #[serde(default, alias = "summaryOnly")]
    pub summary_only: Option<bool>,
    /// df6s: per-depth counts for `impact`. When `Some(true)`, the
    /// response ships a `by_depth_counts: { "1": 12, "2": 7 }` map
    /// alongside the (sliced) impact list so triage can read the
    /// depth distribution without walking every entry. Honoured by
    /// `impact` only; other ops ignore it. `summary_only=true`
    /// already implies this — passing both is fine.
    #[serde(default, alias = "byDepthCounts")]
    pub by_depth_counts: Option<bool>,
    /// df6s: distinct result cap for paginated traversal ops. For
    /// `neighbors` / `coupling_hotspots` this is the existing `limit`
    /// (re-aliased for clarity), so the field is **only** consumed by
    /// ops where `limit` means something else — currently `impact`,
    /// where `limit` is the BFS depth. `impact` therefore reads
    /// `page_limit` for its result cap and keeps `limit` as the
    /// traversal depth. Defaults to 100 when omitted; clamps to
    /// `[1, 1000]`. Sliced only at the response-DTO layer; the
    /// internal traversal still runs to completion.
    #[serde(default, alias = "pageLimit")]
    pub page_limit: Option<i64>,
    /// jc47: the caller's current HEAD / git commit SHA. When supplied,
    /// every successful `code_graph` response includes an additive
    /// `graph_staleness` object comparing this commit against the cached
    /// graph blob's pinned commit. Omit to keep the previous response
    /// shape (no `graph_staleness` field). Empty/whitespace values are
    /// normalized to `None` so clients that serialize every field as `""`
    /// don't accidentally trigger staleness computation. `caller_commit`
    /// and `currentHead` are accepted as aliases.
    #[serde(default, alias = "caller_commit", alias = "currentHead")]
    pub current_head: Option<String>,
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
        clear(&mut self.method);
        clear(&mut self.path);
        clear(&mut self.from_glob);
        clear(&mut self.to_glob);
        clear(&mut self.route_id);
        clear(&mut self.path_glob);
        clear(&mut self.framework);
        clear(&mut self.visibility);
        clear(&mut self.sort_by);
        clear(&mut self.group_by);
        clear(&mut self.edge_kind);
        clear(&mut self.file);
        clear(&mut self.module_glob);
        clear(&mut self.confidence);
        clear(&mut self.file_glob);
        clear(&mut self.route_id);
        clear(&mut self.method);
        clear(&mut self.path);
        clear(&mut self.path_glob);
        clear(&mut self.framework);
        clear(&mut self.kind_hint);
        clear(&mut self.from_sha);
        clear(&mut self.to_sha);
        clear(&mut self.mode);
        clear(&mut self.level);
        clear(&mut self.target);
        clear(&mut self.tests);
        clear(&mut self.current_head);
        clear(&mut self.route_id);
        clear(&mut self.method);
        clear(&mut self.path);
        clear(&mut self.path_glob);
        clear(&mut self.framework);
    }

    /// df6s: resolve a non-negative offset from `self.offset`. Negative
    /// and missing values both clamp to `0` so handlers can pass the
    /// result straight into `.skip()`.
    pub fn resolved_offset(&self) -> usize {
        self.offset.unwrap_or(0).max(0) as usize
    }

    /// df6s: resolve the result-cap for a paginated traversal op.
    /// Precedence: `page_limit` → `default`. The legacy `limit`
    /// field is **not** consulted here so the two semantics stay
    /// separated. Handlers that want to honour both `limit` and
    /// `page_limit` (e.g. `neighbors`, which uses `limit` for the
    /// result cap) should pass `params.limit.unwrap_or(default) as
    /// usize` as the `default` argument so a caller-supplied
    /// `limit` survives alongside the new `page_limit`.
    ///
    /// Clamps into `[1, 1000]` so a runaway caller can't dump the
    /// whole graph into a single page.
    pub fn resolved_page_limit(&self, default: usize) -> usize {
        let raw = self.page_limit.unwrap_or(default as i64).max(0) as usize;
        raw.clamp(1, 1000)
    }
}

/// df6s: the four pagination inputs a paginated traversal op resolves
/// from `CodeGraphParams`. Held by value (not a reference) so handlers
/// can pass it into the response DTO without further juggling.
///
/// `offset` is always populated (clamped to ≥ 0). `limit` is the
/// cap applied to the agent-facing slice. `summary_only` is `true`
/// when the response should ship counts only — the handler is
/// expected to drop the large list fields in that case.
/// `by_depth_counts` is `true` when the response should ship the
/// per-depth count map (currently `impact` only).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PaginationParams {
    pub offset: usize,
    pub limit: usize,
    pub summary_only: bool,
    pub by_depth_counts: bool,
}

impl PaginationParams {
    /// Resolve the four pagination inputs from `CodeGraphParams`,
    /// using `default_limit` as the page-cap fallback. `summary_only`
    /// is `true` only when the caller passed `Some(true)` — an omitted
    /// flag means "give me the full page".
    pub fn resolve(params: &CodeGraphParams, default_limit: usize) -> Self {
        Self {
            offset: params.resolved_offset(),
            limit: params.resolved_page_limit(default_limit),
            summary_only: params.summary_only.unwrap_or(false),
            by_depth_counts: params.by_depth_counts.unwrap_or(false),
        }
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
