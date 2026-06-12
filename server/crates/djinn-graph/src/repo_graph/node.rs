//! [`RepoGraphNode`] and friends — the node-side types of the in-memory
//! repo graph.

use std::path::PathBuf;

use petgraph::graph::NodeIndex;
use serde::{Deserialize, Serialize};

use crate::complexity::ComplexityMetrics;
use crate::scip_parser::{ScipSymbolKind, ScipVisibility};

use super::constants::{
    SYMBOL_KIND_DEFAULT_MULTIPLIER, SYMBOL_KIND_FUNCTION_MULTIPLIER, SYMBOL_KIND_METHOD_MULTIPLIER,
    SYMBOL_KIND_TYPE_MULTIPLIER, SYMBOL_KIND_VARIABLE_MULTIPLIER,
};

/// Internal hit returned by `search_by_name`. The bridge converts this to the
/// public `SearchHit` data type.
#[derive(Debug, Clone)]
pub struct RepoGraphSearchHit {
    pub node_index: NodeIndex,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RepoNodeKey {
    File(PathBuf),
    Symbol(String),
    /// PR F2: synthetic node identifying a deterministic execution
    /// flow traced from an entry point. The string is the stable
    /// process id (sha256 of `entry_point_uid || step_count` truncated
    /// to 16 hex chars) — see [`crate::processes::Process::id`].
    Process(String),
    /// Synthetic node identifying a database table referenced by raw
    /// SQL or ORM access in source code. The string is the lowercased,
    /// schema-qualified table name (`"public.users"`, or just `"users"`
    /// when no schema is present). Materialized by
    /// [`crate::db_access::detect_db_access`]; receivers of `Reads` /
    /// `Writes` edges from the enclosing function/method symbol.
    /// Kept under the same enum so name-index / search / impact ops
    /// surface tables transparently alongside symbols.
    Table(String),
    /// PR s6ch / cs4v: synthetic node identifying a server-side HTTP
    /// route (axum / actix / etc.). The string is the stable route id
    /// shaped as `"<METHOD> <path> (<framework>)"`, e.g.
    /// `"GET /api/agents (axum)"`. Used as the anchor for
    /// `HandlesRoute` / `Fetches` edges; materialized by the route
    /// extractor (out of scope for cs4v — the variant lands first so
    /// the model is forward-compatible with proposal ykcg).
    Route(String),
    /// PR s6ch / cs4v: synthetic node identifying an MCP / RPC tool
    /// handler. The string is the stable tool id (e.g.
    /// `"agents.list"`). No extractor ships in cs4v — the variant
    /// exists so the model can carry tool nodes without a follow-up
    /// schema migration when the proposal lands.
    Tool(String),
}

impl RepoNodeKey {
    /// Return the deterministic canonical UID for this repo-graph node key.
    ///
    /// The UID is derived only from the stable payload carried by the enum
    /// variant, never from `NodeIndex` or other per-build graph state, so the
    /// same logical node receives the same UID across graph rebuilds.
    pub fn stable_uid(&self) -> String {
        match self {
            RepoNodeKey::File(path) => format!("file:{}", path.display()),
            RepoNodeKey::Symbol(symbol) => format!("symbol:{symbol}"),
            RepoNodeKey::Process(id) => format!("process:{id}"),
            RepoNodeKey::Table(name) => format!("table:{name}"),
            RepoNodeKey::Route(id) => format!("route:{id}"),
            RepoNodeKey::Tool(id) => format!("tool:{id}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoGraphNodeKind {
    File,
    Symbol,
    /// PR F2: synthetic execution-flow node materialized by
    /// [`crate::processes::detect_processes`]. Carries no SCIP-derived
    /// metadata of its own (no `file_path`, no `symbol_kind`); the
    /// node's identity lives entirely in [`RepoNodeKey::Process`]. Hung
    /// off the canonical graph by a chain of `StepInProcess` edges.
    Process,
    /// Synthetic database-table node materialized by
    /// [`crate::db_access::detect_db_access`]. Identity in
    /// [`RepoNodeKey::Table`]; carries `display_name` only. Receives
    /// `Reads` / `Writes` edges from enclosing function symbols whose
    /// bodies contain raw SQL touching the table.
    Table,
    /// PR s6ch / cs4v: synthetic HTTP-route node. Identity lives in
    /// [`RepoNodeKey::Route`]; the display name is the human-readable
    /// `"<METHOD> <path> (<framework>)"` form, and the optional
    /// `route_framework` / `route_handler_symbol` metadata on the
    /// owning [`RepoGraphNode`] carries the framework slug and the
    /// handler symbol id. Anchor for `HandlesRoute` (Route → Symbol)
    /// and `Fetches` (Symbol → Route) edges.
    Route,
    /// PR s6ch / cs4v: synthetic tool node (MCP / RPC handler). Mirror
    /// of `Route` for tool-style surfaces. Identity in
    /// [`RepoNodeKey::Tool`]; metadata in the `display_name` /
    /// `language` / `workspace` fields on the owning [`RepoGraphNode`].
    Tool,
}

impl RepoGraphNodeKind {
    /// Returns true for synthetic route/tool affordance nodes.
    ///
    /// These nodes are useful anchors for route/tool-specific graph ops, but
    /// they are not architecture hubs in their own right and should be filtered
    /// out of centrality/god-object ranking surfaces.
    pub fn is_route_or_tool(self) -> bool {
        matches!(self, Self::Route | Self::Tool)
    }
}

/// Return the deterministic canonical UID for a repo-graph node.
pub fn stable_node_uid(node: &RepoGraphNode) -> String {
    node.stable_uid()
}

/// Reusable ranking/noise-filter predicate for synthetic route/tool nodes.
///
/// Checks both `kind` and `id` so future/confidence-tier migrations that load
/// older artifacts with partially widened metadata still treat `Route`/`Tool`
/// affordances consistently at the graph/ranking boundary.
pub fn is_route_or_tool_node(node: &RepoGraphNode) -> bool {
    node.kind.is_route_or_tool() || matches!(node.id, RepoNodeKey::Route(_) | RepoNodeKey::Tool(_))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoGraphNode {
    pub id: RepoNodeKey,
    pub kind: RepoGraphNodeKind,
    pub display_name: String,
    pub language: Option<String>,
    pub file_path: Option<PathBuf>,
    pub symbol: Option<String>,
    pub symbol_kind: Option<ScipSymbolKind>,
    pub is_external: bool,
    /// Visibility of the underlying SCIP symbol, when known. `None` for file
    /// nodes and for synthetic placeholder symbols.
    #[serde(default)]
    pub visibility: Option<ScipVisibility>,
    /// Symbol signature, copied from `ScipSymbol::signature` when present.
    #[serde(default)]
    pub signature: Option<String>,
    /// Symbol documentation, copied from `ScipSymbol::documentation`.
    #[serde(default)]
    pub documentation: Vec<String>,
    /// PR C1: structured signature parts when SCIP populates them.
    /// Propagated from `ScipSymbol::signature_parts`. `None` for indexers
    /// that emit only the markdown signature blob — `code_graph context`
    /// surfaces this as `method_metadata: None` rather than regexing.
    #[serde(default)]
    pub signature_parts: Option<crate::scip_parser::ScipSignatureParts>,
    /// PR F1: SCIP-derived test marker. `true` when at least one of the
    /// symbol's definition occurrences carries the SCIP `Test` role
    /// (`SymbolRole::Test`, bit 32). Used by
    /// [`crate::entry_points::detect_entry_points`] as the high-confidence
    /// signal for `EntryPointKind::Test` (0.95) before falling back to
    /// the file-path / name-prefix heuristics. `false` for file nodes,
    /// for symbols whose indexer doesn't stamp the role bit, and for
    /// symbols restored from pre-PR-F1 (v2 or earlier) artifacts.
    #[serde(default)]
    pub is_test: bool,
    /// Iteration 26: per-function complexity metrics (cyclomatic, cognitive,
    /// nloc, max_nesting, param_count) computed by
    /// [`crate::complexity::ComplexityWalker`] over the host file's tree-
    /// sitter AST. Populated only for function-like SCIP symbols
    /// (`Function` / `Method` / `Constructor`) and only when the file's
    /// language is supported by the walker AND a tree-sitter range can be
    /// matched against the SCIP definition. `None` for file nodes,
    /// non-function symbols, unsupported languages, and for any node
    /// restored from a pre-iteration-26 (v8 or earlier) artifact —
    /// `#[serde(default)]` keeps the deserialization tolerant in that
    /// case, but the version bump forces a re-warm.
    #[serde(default)]
    pub complexity: Option<ComplexityMetrics>,
    /// Workspace slug for the SCIP artifact that produced this node. Kept at
    /// the end of the struct and defaulted so existing v10 bincode artifacts,
    /// which stop after `complexity`, can deserialize with `None` without a
    /// repo-graph artifact version bump or cluster-wide re-warm.
    #[serde(default)]
    pub workspace: Option<String>,
    /// PR s6ch / cs4v: framework slug for a `Route` node (e.g. `"axum"`,
    /// `"actix-web"`). `None` for File / Symbol / Process / Table /
    /// Tool nodes — the field is shared across kinds so the `Route`
    /// metadata can ride on the same struct without widening the enum
    /// shape. Defaulted to `None` so existing v10 bincode artifacts
    /// deserialize unchanged.
    #[serde(default)]
    pub route_framework: Option<String>,
    /// PR s6ch / cs4v: stable SCIP-style symbol id of the handler
    /// function that backs a `Route` node (e.g.
    /// `"scip-rust pkg src/handlers/agents.rs `list_agents`()."`). The
    /// handler's full graph node is reachable via [`RepoNodeKey::Symbol`]
    /// — this field is a denormalized back-reference for cheap lookups
    /// without an extra graph walk. `None` for File / Symbol / Process
    /// / Table / Tool nodes. Defaulted to `None` so existing v10
    /// bincode artifacts deserialize unchanged.
    #[serde(default)]
    pub route_handler_symbol: Option<String>,
}

impl RepoGraphNode {
    pub fn key(&self) -> RepoNodeKey {
        self.id.clone()
    }

    /// Return the deterministic canonical UID for this graph node.
    pub fn stable_uid(&self) -> String {
        self.id.stable_uid()
    }

    pub fn kind(&self) -> RepoGraphNodeKind {
        self.kind
    }

    pub fn is_route_or_tool(&self) -> bool {
        is_route_or_tool_node(self)
    }

    pub(crate) fn intrinsic_weight(&self) -> f64 {
        match self.kind {
            RepoGraphNodeKind::File => 1.0,
            RepoGraphNodeKind::Symbol => match self.symbol_kind {
                Some(ScipSymbolKind::Type)
                | Some(ScipSymbolKind::Struct)
                | Some(ScipSymbolKind::Interface)
                | Some(ScipSymbolKind::Enum) => SYMBOL_KIND_TYPE_MULTIPLIER,
                Some(ScipSymbolKind::Method) | Some(ScipSymbolKind::Constructor) => {
                    SYMBOL_KIND_METHOD_MULTIPLIER
                }
                Some(ScipSymbolKind::Function) => SYMBOL_KIND_FUNCTION_MULTIPLIER,
                Some(ScipSymbolKind::Variable)
                | Some(ScipSymbolKind::Field)
                | Some(ScipSymbolKind::Property)
                | Some(ScipSymbolKind::Constant) => SYMBOL_KIND_VARIABLE_MULTIPLIER,
                _ => SYMBOL_KIND_DEFAULT_MULTIPLIER,
            },
            // PR F2: process nodes are synthetic side-channel metadata.
            // Give them the lowest tier (variable-class) so PageRank
            // doesn't promote them above real symbols just because
            // they fan out to many steps.
            RepoGraphNodeKind::Process => SYMBOL_KIND_VARIABLE_MULTIPLIER,
            // Database tables are pure sinks — they only receive
            // `Reads`/`Writes` edges from caller symbols. Same tier as
            // `Process` so PageRank doesn't promote them just because
            // many functions touch the same table.
            RepoGraphNodeKind::Table => SYMBOL_KIND_VARIABLE_MULTIPLIER,
            // PR s6ch / cs4v: HTTP routes are synthetic side-channel
            // metadata. They fan out to a single handler symbol and may
            // be hit by many `Fetches` edges from TS call sites; the
            // variable-class tier keeps them from outscoring the real
            // symbol handlers in PageRank. Mirrors `Process` / `Table`.
            RepoGraphNodeKind::Route => SYMBOL_KIND_VARIABLE_MULTIPLIER,
            // PR s6ch / cs4v: tool nodes (MCP / RPC handlers) sit in
            // the same role as `Route` — synthetic side-channel
            // metadata whose importance should be inherited from the
            // implementing symbol rather than a standalone PageRank
            // boost. Variable-class tier for symmetry with `Route`.
            RepoGraphNodeKind::Tool => SYMBOL_KIND_VARIABLE_MULTIPLIER,
        }
    }
}
