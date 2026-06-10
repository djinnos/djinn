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
}

impl RepoGraphNode {
    pub fn key(&self) -> RepoNodeKey {
        self.id.clone()
    }

    pub fn kind(&self) -> RepoGraphNodeKind {
        self.kind
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
        }
    }
}
