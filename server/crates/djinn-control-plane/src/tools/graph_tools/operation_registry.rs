//! Declarative registry for `code_graph` operations.
//!
//! This module is the **single source of truth** for operation metadata.
//! Follow-up tasks will derive dispatch and pre-resolve tables from here
//! instead of maintaining separate handwritten match arms.
//!
//! The registry is wired into runtime dispatch in `handler.rs` for the
//! vertical slice (`neighbors`, `impact`, `context`, `coupling_hotspots`).
//! Smoke exemplars and bridge coverage helpers are still only exercised
//! by unit tests, so `dead_code` is suppressed at the module boundary.
#![allow(dead_code)]
//! The registry is purely data — no build scripts, no runtime graph
//! cache access, no database or network dependencies.  Each entry is
//! a `const`-compatible [`OpEntry`] struct collected into the
//! [`CODE_GRAPH_REGISTRY`] static slice.
//!
//! # Design choices
//!
//! * **Const slice** — zero runtime cost; the compiler folds every
//!   entry into `.rodata`.  A `HashMap` would require a
//!   `lazy_static`/`OnceLock` for no benefit (the set is < 50
//!   entries and iteration is fine for lookups).
//! * **Stringly-typed identifiers** — handler fn names, bridge method
//!   names, and validation categories are `&'static str` so the
//!   registry stays decoupled from the concrete `impl` signatures.
//!   Future PRs can upgrade these to typed enums once the full set
//!   is stable.
//! * **Exemplar data** — each entry carries a [`SmokeExemplar`] that
//!   captures the minimum `CodeGraphParams` fields a smoke test needs
//!   to exercise the operation end-to-end.  This lets the downstream
//!   test task generate param sets from the registry instead of
//!   hand-writing each fixture.

use std::fmt;

// ── Types ────────────────────────────────────────────────────────────────────

/// Classifies how a `code_graph` operation participates in the
/// pre-resolve step (`pre_resolve_key` in `handler.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreResolveCategory {
    /// No pre-resolve — the op's input key is a query/glob, not a
    /// node identifier (e.g. `search`, `cycles`, `hotspots`).
    None,
    /// Single `key` field resolved through `RepoGraphOps::resolve`.
    SingleKey,
    /// Two keys (`from` + `to`) resolved individually.
    DualKey,
}

impl fmt::Display for PreResolveCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("none"),
            Self::SingleKey => f.write_str("single_key"),
            Self::DualKey => f.write_str("dual_key"),
        }
    }
}

/// Identifies which validation functions are applied to the
/// operation's params before the handler runs.
///
/// Values are stringly typed so the registry doesn't depend on the
/// `validation` module's concrete function signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationCategory {
    /// `require_key` + direction/kind_filter/group_by validators.
    KeyWithEdgeFilters,
    /// `require_key` + group_by + min_confidence range check.
    KeyWithConfidence,
    /// `require_key` only; no extra param validators.
    KeyOnly,
    /// `require_query` + validate_kind_filter.
    QueryWithKindFilter,
    /// `require_query` + validate_flow_kind_filter.
    QueryWithFlowKind,
    /// `require_from_to` only.
    DualKey,
    /// `require_globs` only.
    Globs,
    /// `require_route_selector` only.
    RouteSelector,
    /// `require_route_selector` + validate_min_confidence_value.
    RouteSelectorWithConfidence,
    /// `validate_kind_filter` + `validate_visibility`.
    KindFilterAndVisibility,
    /// `validate_kind_filter` + `validate_sort_by`.
    KindFilterAndSortBy,
    /// `validate_kind_filter` only.
    KindFilterOnly,
    /// `file` + `start_line` required (symbols_at).
    FileWithLines,
    /// `changed_ranges` required (diff_touches).
    ChangedRanges,
    /// `rules` required (boundary_check).
    Rules,
    /// `impact_targets` required (impact_check).
    ImpactTargets,
    /// `seed_entries` + `seed_sinks` + `symbols` required (touches_hot_path).
    HotPathSeeds,
    /// `file` required (coupling).
    FileOnly,
    /// No operation-specific validation beyond the generic
    /// normalization applied by `CodeGraphParams::normalize()`.
    None,
}

impl fmt::Display for ValidationCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyWithEdgeFilters => f.write_str("key_with_edge_filters"),
            Self::KeyWithConfidence => f.write_str("key_with_confidence"),
            Self::KeyOnly => f.write_str("key_only"),
            Self::QueryWithKindFilter => f.write_str("query_with_kind_filter"),
            Self::QueryWithFlowKind => f.write_str("query_with_flow_kind"),
            Self::DualKey => f.write_str("dual_key"),
            Self::Globs => f.write_str("globs"),
            Self::RouteSelector => f.write_str("route_selector"),
            Self::RouteSelectorWithConfidence => f.write_str("route_selector_with_confidence"),
            Self::KindFilterAndVisibility => f.write_str("kind_filter_and_visibility"),
            Self::KindFilterAndSortBy => f.write_str("kind_filter_and_sort_by"),
            Self::KindFilterOnly => f.write_str("kind_filter_only"),
            Self::FileWithLines => f.write_str("file_with_lines"),
            Self::ChangedRanges => f.write_str("changed_ranges"),
            Self::Rules => f.write_str("rules"),
            Self::ImpactTargets => f.write_str("impact_targets"),
            Self::HotPathSeeds => f.write_str("hot_path_seeds"),
            Self::FileOnly => f.write_str("file_only"),
            Self::None => f.write_str("none"),
        }
    }
}

/// Classifies the workspace-scoping behaviour of the operation.
///
/// This captures the distinction the pb94 epic draws between
/// listing/bounded ops (workspace hard-scopes the result set) and
/// traversal ops (workspace scopes seed resolution only, the walk
/// stays unconstrained).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceBehavior {
    /// Operation does not consult `workspace` at all.
    Ignored,
    /// Workspace is resolved via `resolve_workspace_scope` for seed
    /// resolution; the traversal itself is unconstrained.
    TraversalSeedOnly,
    /// Workspace hard-scopes the returned result set.
    HardScoped,
}

impl fmt::Display for WorkspaceBehavior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ignored => f.write_str("ignored"),
            Self::TraversalSeedOnly => f.write_str("traversal_seed_only"),
            Self::HardScoped => f.write_str("hard_scoped"),
        }
    }
}

/// Minimum `CodeGraphParams` fields needed for a smoke-test call.
///
/// All fields are `Option<&'static str>` so the exemplar can omit
/// fields that aren't relevant to the operation (the resolver
/// fills in `project` / `project_id` / `project_path` at dispatch
/// time regardless).
#[derive(Debug)]
pub struct SmokeExemplar {
    /// The `operation` string (redundant with `OpEntry::name` but
    /// explicit so the test generator can build a full
    /// `CodeGraphParams` without cross-referencing).
    pub operation: &'static str,
    /// Value for `params.key`, if the op requires one.
    pub key: Option<&'static str>,
    /// Value for `params.query`, if the op requires one.
    pub query: Option<&'static str>,
    /// Value for `params.file`, if the op requires one.
    pub file: Option<&'static str>,
    /// Value for `params.from`, if the op requires one.
    pub from: Option<&'static str>,
    /// Value for `params.to`, if the op requires one.
    pub to: Option<&'static str>,
    /// Human-readable description of what this exemplar exercises.
    pub note: &'static str,
}

/// A single registry entry describing one `code_graph` operation.
///
/// Fields are intentionally stringly typed so the registry module
/// has zero import dependencies on handler, bridge, or validation
/// internals.  Future PRs may tighten selected fields to typed
/// enums once the full operation set is stable.
#[derive(Debug)]
pub struct OpEntry {
    /// Canonical operation string that appears on the wire
    /// (`params.operation`).
    pub name: &'static str,
    /// Accepted aliases.  Empty when no aliases are defined.
    pub aliases: &'static [&'static str],
    /// Pre-resolve classification.
    pub pre_resolve: PreResolveCategory,
    /// Validation classification.
    pub validation: ValidationCategory,
    /// Name of the handler method on `DjinnMcpServer` (e.g.
    /// `"code_graph_neighbors"`).  Purely documentary for now;
    /// follow-up tasks may use it for compile-time dispatch.
    pub handler_fn: &'static str,
    /// Name of the corresponding `RepoGraphOps` trait method (e.g.
    /// `"neighbors"`).
    pub bridge_method: &'static str,
    /// Workspace-scoping behaviour.
    pub workspace: WorkspaceBehavior,
    /// Smoke-test exemplar data.
    pub smoke: SmokeExemplar,
}

// ── Registry ─────────────────────────────────────────────────────────────────

/// The canonical `code_graph` operation registry.
///
/// Each entry maps one wire-level operation name to its dispatch
/// metadata.  The full catalog covers every operation currently
/// supported by the `code_graph` tool.
///
/// **Lookup helpers** — [`lookup_by_name`] performs a linear scan
/// (the table is small; a `HashMap` would cost more in `lazy_static`
/// machinery than it saves in lookup time).
pub const CODE_GRAPH_REGISTRY: &[OpEntry] = &[
    // ── neighbors ────────────────────────────────────────────────────
    //
    // Handler  : code_graph_neighbors  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::neighbors
    // Pre-res  : single_key — `params.key` resolved via `RepoGraphOps::resolve`
    // Validation: require_key + validate_direction + validate_group_by
    //            + validate_edge_kind_filter (reads/writes)
    // Workspace: ignored — neighbors are not workspace-scoped
    OpEntry {
        name: "neighbors",
        aliases: &[],
        pre_resolve: PreResolveCategory::SingleKey,
        validation: ValidationCategory::KeyWithEdgeFilters,
        handler_fn: "code_graph_neighbors",
        bridge_method: "neighbors",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "neighbors",
            key: Some("MyStruct"),
            query: None,
            file: None,
            from: None,
            to: None,
            note: "basic neighbor lookup for a struct symbol",
        },
    },
    // ── impact ───────────────────────────────────────────────────────
    //
    // Handler  : code_graph_impact  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::impact
    // Pre-res  : single_key — `params.key` resolved via `RepoGraphOps::resolve`
    // Validation: require_key + validate_group_by + min_confidence range
    // Workspace: traversal_seed_only — workspace scopes only seed
    //            resolution; the BFS walk is unconstrained (pb94)
    OpEntry {
        name: "impact",
        aliases: &[],
        pre_resolve: PreResolveCategory::SingleKey,
        validation: ValidationCategory::KeyWithConfidence,
        handler_fn: "code_graph_impact",
        bridge_method: "impact",
        workspace: WorkspaceBehavior::TraversalSeedOnly,
        smoke: SmokeExemplar {
            operation: "impact",
            key: Some("MyStruct"),
            query: None,
            file: None,
            from: None,
            to: None,
            note: "blast-radius BFS from a struct symbol",
        },
    },
    // ── context ──────────────────────────────────────────────────────
    //
    // Handler  : code_graph_context  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::context
    // Pre-res  : single_key — `params.key` resolved via `RepoGraphOps::resolve`
    // Validation: require_key only
    // Workspace: ignored
    OpEntry {
        name: "context",
        aliases: &[],
        pre_resolve: PreResolveCategory::SingleKey,
        validation: ValidationCategory::KeyOnly,
        handler_fn: "code_graph_context",
        bridge_method: "context",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "context",
            key: Some("MyStruct"),
            query: None,
            file: None,
            from: None,
            to: None,
            note: "360° view of a symbol's incoming/outgoing edges",
        },
    },
    // ── coupling_hotspots ────────────────────────────────────────────
    //
    // Handler  : code_graph_coupling_hotspots  (handler_coupling_ops.rs)
    // Bridge   : RepoGraphOps::coupling_hotspots
    // Pre-res  : none — no caller-supplied node key; the op scans
    //            the coupling index project-wide
    // Validation: none beyond CodeGraphParams::normalize()
    // Workspace: ignored — coupling_hotspots is project-global
    OpEntry {
        name: "coupling_hotspots",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_coupling_hotspots",
        bridge_method: "coupling_hotspots",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "coupling_hotspots",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "top file pairs by co-edit frequency",
        },
    },
    // ── workspaces ───────────────────────────────────────────────────
    //
    // Handler  : code_graph_workspaces  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::workspaces
    // Pre-res  : none
    // Validation: none
    // Workspace: ignored
    OpEntry {
        name: "workspaces",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_workspaces",
        bridge_method: "workspaces",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "workspaces",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "enumerate graph workspaces with freshness metadata",
        },
    },
    // ── ranked ───────────────────────────────────────────────────────
    //
    // Handler  : code_graph_ranked  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::ranked
    // Pre-res  : none — `key` is not a node identifier for this op
    // Validation: validate_kind_filter + validate_sort_by
    // Workspace: hard_scoped — workspace bounds the returned node set
    OpEntry {
        name: "ranked",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::KindFilterAndSortBy,
        handler_fn: "code_graph_ranked",
        bridge_method: "ranked",
        workspace: WorkspaceBehavior::HardScoped,
        smoke: SmokeExemplar {
            operation: "ranked",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "top-ranked nodes by PageRank or degree",
        },
    },
    // ── implementations ──────────────────────────────────────────────
    //
    // Handler  : code_graph_implementations  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::implementations
    // Pre-res  : single_key — `params.key` resolved via `RepoGraphOps::resolve`
    // Validation: require_key only
    // Workspace: ignored
    OpEntry {
        name: "implementations",
        aliases: &[],
        pre_resolve: PreResolveCategory::SingleKey,
        validation: ValidationCategory::KeyOnly,
        handler_fn: "code_graph_implementations",
        bridge_method: "implementations",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "implementations",
            key: Some("MyTrait"),
            query: None,
            file: None,
            from: None,
            to: None,
            note: "symbols that implement a given trait",
        },
    },
    // ── search ───────────────────────────────────────────────────────
    //
    // Handler  : code_graph_search  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::search / hybrid_search
    // Pre-res  : none — `query` is a text search, not a node key
    // Validation: require_query + validate_kind_filter + resolve_search_mode
    // Workspace: hard_scoped (hint envelope only today)
    OpEntry {
        name: "search",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::QueryWithKindFilter,
        handler_fn: "code_graph_search",
        bridge_method: "search",
        workspace: WorkspaceBehavior::HardScoped,
        smoke: SmokeExemplar {
            operation: "search",
            key: None,
            query: Some("helper"),
            file: None,
            from: None,
            to: None,
            note: "name-based symbol search",
        },
    },
    // ── query_subgraph ─────────────────────────────────────────────────
    //
    // Handler  : code_graph_query_subgraph  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::query_subgraph
    // Pre-res  : none — natural-language query, not a node key
    // Validation: none (kind_filter validated inline; query required inline)
    // Workspace: ignored (workspace passed via QuerySubgraphRequest)
    OpEntry {
        name: "query_subgraph",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_query_subgraph",
        bridge_method: "query_subgraph",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "query_subgraph",
            key: None,
            query: Some("how is auth handled"),
            file: None,
            from: None,
            to: None,
            note: "budgeted natural-language subgraph query",
        },
    },
    // ── route_map ────────────────────────────────────────────────────
    //
    // Handler  : code_graph_route_map  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::route_map
    // Pre-res  : none
    // Validation: require_route_selector
    // Workspace: ignored
    OpEntry {
        name: "route_map",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::RouteSelector,
        handler_fn: "code_graph_route_map",
        bridge_method: "route_map",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "route_map",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "route graph surface discovery",
        },
    },
    // ── shape_check ──────────────────────────────────────────────────
    //
    // Handler  : code_graph_shape_check  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::shape_check
    // Pre-res  : none
    // Validation: require_route_selector
    // Workspace: ignored
    OpEntry {
        name: "shape_check",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::RouteSelector,
        handler_fn: "code_graph_shape_check",
        bridge_method: "shape_check",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "shape_check",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "route response-shape drift surface",
        },
    },
    // ── api_impact ───────────────────────────────────────────────────
    //
    // Handler  : code_graph_api_impact  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::api_impact
    // Pre-res  : none
    // Validation: require_route_selector + validate_min_confidence_value
    // Workspace: ignored
    OpEntry {
        name: "api_impact",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::RouteSelectorWithConfidence,
        handler_fn: "code_graph_api_impact",
        bridge_method: "api_impact",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "api_impact",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "route API-impact surface",
        },
    },
    // ── flow ───────────────────────────────────────────────────────────
    //
    // Handler  : code_graph_flow  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::flow
    // Pre-res  : none
    // Validation: require_query + validate_flow_kind_filter
    // Workspace: ignored
    OpEntry {
        name: "flow",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::QueryWithFlowKind,
        handler_fn: "code_graph_flow",
        bridge_method: "flow",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "flow",
            key: None,
            query: Some("user registration"),
            file: None,
            from: None,
            to: None,
            note: "execution-flow search",
        },
    },
    // ── cycles ───────────────────────────────────────────────────────
    //
    // Handler  : code_graph_cycles  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::cycles
    // Pre-res  : none
    // Validation: validate_kind_filter
    // Workspace: hard_scoped (hint envelope only; SCC cache is workspace-agnostic)
    OpEntry {
        name: "cycles",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::KindFilterOnly,
        handler_fn: "code_graph_cycles",
        bridge_method: "cycles",
        workspace: WorkspaceBehavior::HardScoped,
        smoke: SmokeExemplar {
            operation: "cycles",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "strongly-connected components",
        },
    },
    // ── orphans ──────────────────────────────────────────────────────
    //
    // Handler  : code_graph_orphans  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::orphans
    // Pre-res  : none
    // Validation: validate_kind_filter + validate_visibility
    // Workspace: hard_scoped
    OpEntry {
        name: "orphans",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::KindFilterAndVisibility,
        handler_fn: "code_graph_orphans",
        bridge_method: "orphans",
        workspace: WorkspaceBehavior::HardScoped,
        smoke: SmokeExemplar {
            operation: "orphans",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "dead-symbol enumeration (zero incoming references)",
        },
    },
    // ── path ───────────────────────────────────────────────────────────
    //
    // Handler  : code_graph_path  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::path
    // Pre-res  : dual_key — `from` and `to` resolved individually
    // Validation: require_from_to
    // Workspace: traversal_seed_only — workspace scopes endpoint resolution only
    OpEntry {
        name: "path",
        aliases: &[],
        pre_resolve: PreResolveCategory::DualKey,
        validation: ValidationCategory::DualKey,
        handler_fn: "code_graph_path",
        bridge_method: "path",
        workspace: WorkspaceBehavior::TraversalSeedOnly,
        smoke: SmokeExemplar {
            operation: "path",
            key: None,
            query: None,
            file: None,
            from: Some("SourceStruct"),
            to: Some("TargetStruct"),
            note: "shortest dependency path between two nodes",
        },
    },
    // ── edges ──────────────────────────────────────────────────────────
    //
    // Handler  : code_graph_edges  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::edges
    // Pre-res  : none
    // Validation: require_globs
    // Workspace: hard_scoped (hint envelope only)
    OpEntry {
        name: "edges",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::Globs,
        handler_fn: "code_graph_edges",
        bridge_method: "edges",
        workspace: WorkspaceBehavior::HardScoped,
        smoke: SmokeExemplar {
            operation: "edges",
            key: None,
            query: None,
            file: None,
            from: Some("src/**/*.rs"),
            to: Some("src/**/*.rs"),
            note: "enumerate edges matching path globs",
        },
    },
    // ── symbols_at ───────────────────────────────────────────────────
    //
    // Handler  : code_graph_symbols_at  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::symbols_at
    // Pre-res  : none
    // Validation: file + start_line required (inline in handler)
    // Workspace: ignored
    OpEntry {
        name: "symbols_at",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::FileWithLines,
        handler_fn: "code_graph_symbols_at",
        bridge_method: "symbols_at",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "symbols_at",
            key: None,
            query: None,
            file: Some("src/lib.rs"),
            from: None,
            to: None,
            note: "symbols enclosing a file line range",
        },
    },
    // ── diff_touches ───────────────────────────────────────────────────
    //
    // Handler  : code_graph_diff_touches  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::diff_touches
    // Pre-res  : none
    // Validation: changed_ranges required (inline in handler)
    // Workspace: ignored
    OpEntry {
        name: "diff_touches",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::ChangedRanges,
        handler_fn: "code_graph_diff_touches",
        bridge_method: "diff_touches",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "diff_touches",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "map changed line ranges to touched symbols",
        },
    },
    // ── detect_changes ─────────────────────────────────────────────────
    //
    // Handler  : code_graph_detect_changes  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::detect_changes
    // Pre-res  : none
    // Validation: from_sha+to_sha OR changed_files required (inline)
    // Workspace: ignored
    OpEntry {
        name: "detect_changes",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_detect_changes",
        bridge_method: "detect_changes",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "detect_changes",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "detect touched symbols from SHA range or file list",
        },
    },
    // ── describe ───────────────────────────────────────────────────────
    //
    // Handler  : code_graph_describe  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::describe
    // Pre-res  : single_key — `params.key` resolved via `RepoGraphOps::resolve`
    // Validation: require_key only
    // Workspace: ignored
    OpEntry {
        name: "describe",
        aliases: &[],
        pre_resolve: PreResolveCategory::SingleKey,
        validation: ValidationCategory::KeyOnly,
        handler_fn: "code_graph_describe",
        bridge_method: "describe",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "describe",
            key: Some("MyStruct"),
            query: None,
            file: None,
            from: None,
            to: None,
            note: "detailed description of a single symbol",
        },
    },
    // ── status ─────────────────────────────────────────────────────────
    //
    // Handler  : code_graph_status  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::status
    // Pre-res  : none
    // Validation: none
    // Workspace: ignored
    OpEntry {
        name: "status",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_status",
        bridge_method: "status",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "status",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "peek at in-memory graph cache status",
        },
    },
    // ── snapshot ───────────────────────────────────────────────────────
    //
    // Handler  : code_graph_snapshot  (handler_coupling_ops.rs)
    // Bridge   : RepoGraphOps::snapshot
    // Pre-res  : none
    // Validation: none (level parsed but not validated as closed set)
    // Workspace: hard_scoped
    OpEntry {
        name: "snapshot",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_snapshot",
        bridge_method: "snapshot",
        workspace: WorkspaceBehavior::HardScoped,
        smoke: SmokeExemplar {
            operation: "snapshot",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "full-graph snapshot capped by PageRank tier",
        },
    },
    // ── api_surface ────────────────────────────────────────────────────
    //
    // Handler  : code_graph_api_surface  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::api_surface
    // Pre-res  : none
    // Validation: validate_visibility
    // Workspace: hard_scoped
    OpEntry {
        name: "api_surface",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_api_surface",
        bridge_method: "api_surface",
        workspace: WorkspaceBehavior::HardScoped,
        smoke: SmokeExemplar {
            operation: "api_surface",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "public/private/any symbol listing with fan-in/fan-out",
        },
    },
    // ── boundary_check ─────────────────────────────────────────────────
    //
    // Handler  : code_graph_boundary_check  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::boundary_check
    // Pre-res  : none
    // Validation: rules required (inline in handler)
    // Workspace: ignored
    OpEntry {
        name: "boundary_check",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::Rules,
        handler_fn: "code_graph_boundary_check",
        bridge_method: "boundary_check",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "boundary_check",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "match edges against boundary rules and return violations",
        },
    },
    // ── hotspots ─────────────────────────────────────────────────────────
    //
    // Handler  : code_graph_hotspots  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::hotspots
    // Pre-res  : none
    // Validation: none
    // Workspace: ignored
    OpEntry {
        name: "hotspots",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_hotspots",
        bridge_method: "hotspots",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "hotspots",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "churn × centrality ranking over files",
        },
    },
    // ── complexity ─────────────────────────────────────────────────────
    //
    // Handler  : code_graph_complexity  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::complexity
    // Pre-res  : none
    // Validation: none (target/sort_by parsed but not validated as closed set)
    // Workspace: ignored
    OpEntry {
        name: "complexity",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_complexity",
        bridge_method: "complexity",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "complexity",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "rank function-like symbols or files by complexity metrics",
        },
    },
    // ── refactor_candidates ────────────────────────────────────────────
    //
    // Handler  : code_graph_refactor_candidates  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::refactor_candidates
    // Pre-res  : none
    // Validation: none
    // Workspace: ignored
    OpEntry {
        name: "refactor_candidates",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_refactor_candidates",
        bridge_method: "refactor_candidates",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "refactor_candidates",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "composite refactor-priority ranking",
        },
    },
    // ── metrics_at ───────────────────────────────────────────────────────
    //
    // Handler  : code_graph_metrics_at  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::metrics_at
    // Pre-res  : none
    // Validation: none
    // Workspace: ignored
    OpEntry {
        name: "metrics_at",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_metrics_at",
        bridge_method: "metrics_at",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "metrics_at",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "scalar graph snapshot of the canonical graph",
        },
    },
    // ── dead_symbols ───────────────────────────────────────────────────
    //
    // Handler  : code_graph_dead_symbols  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::dead_symbols
    // Pre-res  : none
    // Validation: confidence tier validated inline (high/med/low)
    // Workspace: ignored
    OpEntry {
        name: "dead_symbols",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_dead_symbols",
        bridge_method: "dead_symbols",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "dead_symbols",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "symbols with zero incoming edges from entry-point set",
        },
    },
    // ── deprecated_callers ───────────────────────────────────────────────
    //
    // Handler  : code_graph_deprecated_callers  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::deprecated_callers
    // Pre-res  : none
    // Validation: none
    // Workspace: ignored
    OpEntry {
        name: "deprecated_callers",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_deprecated_callers",
        bridge_method: "deprecated_callers",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "deprecated_callers",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "callers of symbols marked deprecated",
        },
    },
    // ── touches_hot_path ─────────────────────────────────────────────────
    //
    // Handler  : code_graph_touches_hot_path  (handler_coupling_ops.rs)
    // Bridge   : RepoGraphOps::touches_hot_path
    // Pre-res  : none
    // Validation: seed_entries + seed_sinks + symbols required (inline)
    // Workspace: traversal_seed_only
    OpEntry {
        name: "touches_hot_path",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::HotPathSeeds,
        handler_fn: "code_graph_touches_hot_path",
        bridge_method: "touches_hot_path",
        workspace: WorkspaceBehavior::TraversalSeedOnly,
        smoke: SmokeExemplar {
            operation: "touches_hot_path",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "which queried symbols sit on entry→sink shortest paths",
        },
    },
    // ── coupling ─────────────────────────────────────────────────────────
    //
    // Handler  : code_graph_coupling  (handler_coupling_ops.rs)
    // Bridge   : RepoGraphOps::coupling
    // Pre-res  : none
    // Validation: file required (inline in handler)
    // Workspace: ignored
    OpEntry {
        name: "coupling",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::FileOnly,
        handler_fn: "code_graph_coupling",
        bridge_method: "coupling",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "coupling",
            key: None,
            query: None,
            file: Some("src/lib.rs"),
            from: None,
            to: None,
            note: "files most frequently co-edited with a given file",
        },
    },
    // ── churn ────────────────────────────────────────────────────────────
    //
    // Handler  : code_graph_churn  (handler_coupling_ops.rs)
    // Bridge   : RepoGraphOps::churn
    // Pre-res  : none
    // Validation: none
    // Workspace: ignored
    OpEntry {
        name: "churn",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_churn",
        bridge_method: "churn",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "churn",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "top files by distinct-commit count",
        },
    },
    // ── coupling_hubs ──────────────────────────────────────────────────
    //
    // Handler  : code_graph_coupling_hubs  (handler_coupling_ops.rs)
    // Bridge   : RepoGraphOps::coupling_hubs
    // Pre-res  : none
    // Validation: none
    // Workspace: ignored
    OpEntry {
        name: "coupling_hubs",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_coupling_hubs",
        bridge_method: "coupling_hubs",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "coupling_hubs",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "top files by cumulative coupling across all partners",
        },
    },
    // ── crate_graph ────────────────────────────────────────────────────
    //
    // Handler  : code_graph_crate_graph  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::crate_graph
    // Pre-res  : none
    // Validation: none
    // Workspace: ignored
    OpEntry {
        name: "crate_graph",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_crate_graph",
        bridge_method: "crate_graph",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "crate_graph",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "crate-level dependency graph with aggregated cross-crate references",
        },
    },
    // ── impact_check ─────────────────────────────────────────────────────
    //
    // Handler  : code_graph_impact_check  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::impact + crate_graph (composite)
    // Pre-res  : none
    // Validation: impact_targets required (inline in handler)
    // Workspace: ignored
    OpEntry {
        name: "impact_check",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::ImpactTargets,
        handler_fn: "code_graph_impact_check",
        bridge_method: "impact", // composite: uses impact + crate_graph
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "impact_check",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "advisory impact preflight for proposed removals/renames",
        },
    },
];

// ── Lookup helpers ───────────────────────────────────────────────────────────

/// Find a registry entry by canonical name or alias.
///
/// Returns `None` if the operation string doesn't match any entry
/// (including operations that exist in `dispatch_code_graph_op` but
/// haven't been registered yet).
pub fn lookup_by_name(op: &str) -> Option<&'static OpEntry> {
    CODE_GRAPH_REGISTRY
        .iter()
        .find(|e| e.name == op || e.aliases.contains(&op))
}

/// Return the list of all registered canonical operation names.
///
/// Useful for error messages and test assertions.
pub fn registered_names() -> Vec<&'static str> {
    CODE_GRAPH_REGISTRY.iter().map(|e| e.name).collect()
}

// ── Bridge coverage ──────────────────────────────────────────────────────────

/// Known `RepoGraphOps` trait method names that correspond to converted
/// registry entries.  The bridge coverage tests and the compile-time
/// `const _` guard below verify that every registered `bridge_method`
/// appears here and vice-versa.
///
/// This is the canonical mapping between registry metadata and the
/// `RepoGraphOps` trait surface in
/// `server/crates/djinn-control-plane/src/bridge/graph_bridge.rs`.
/// When a new operation is registered, its `bridge_method` **must**
/// appear here and have a corresponding `async fn` on `RepoGraphOps`
/// plus a forwarding stub on `RepoGraphBridge` in the server crate.
///
/// Note: `impact_check` is a composite handler that delegates to
/// `impact` and `crate_graph`; it is listed under `impact` for
/// bridge-coverage purposes.
pub const KNOWN_BRIDGE_METHODS: &[&str] = &[
    "neighbors",
    "impact",
    "context",
    "coupling_hotspots",
    "workspaces",
    "ranked",
    "implementations",
    "search",
    "query_subgraph",
    "route_map",
    "shape_check",
    "api_impact",
    "flow",
    "cycles",
    "orphans",
    "path",
    "edges",
    "symbols_at",
    "diff_touches",
    "detect_changes",
    "describe",
    "status",
    "snapshot",
    "api_surface",
    "boundary_check",
    "hotspots",
    "complexity",
    "refactor_candidates",
    "metrics_at",
    "dead_symbols",
    "deprecated_callers",
    "touches_hot_path",
    "coupling",
    "churn",
    "coupling_hubs",
    "crate_graph",
];

/// Byte-equality helper usable in `const`-context.
const fn str_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a.as_bytes()[i] != b.as_bytes()[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Compile-time bridge coverage guard.
///
/// For each entry in `KNOWN_BRIDGE_METHODS`, walk the registry and
/// confirm at least one `OpEntry` references it.  This catches
/// stale entries in `KNOWN_BRIDGE_METHODS` (method removed from
/// registry) at compile time.
const _: () = {
    let mut mi = 0;
    while mi < KNOWN_BRIDGE_METHODS.len() {
        let method = KNOWN_BRIDGE_METHODS[mi];
        let mut found = false;
        let mut ri = 0;
        while ri < CODE_GRAPH_REGISTRY.len() {
            if str_eq(CODE_GRAPH_REGISTRY[ri].bridge_method, method) {
                found = true;
                break;
            }
            ri += 1;
        }
        // If this fails to compile: KNOWN_BRIDGE_METHODS has an entry
        // not referenced by any registry OpEntry.  Either add the
        // entry to the registry or remove it from the known list.
        assert!(found);
        mi += 1;
    }
};

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Full catalog of operations supported by the `code_graph` tool.
    /// This is the source of truth for the registry coverage test.
    const FULL_CATALOG: &[&str] = &[
        "neighbors",
        "impact",
        "context",
        "coupling_hotspots",
        "workspaces",
        "ranked",
        "implementations",
        "search",
        "query_subgraph",
        "route_map",
        "shape_check",
        "api_impact",
        "flow",
        "cycles",
        "orphans",
        "path",
        "edges",
        "symbols_at",
        "diff_touches",
        "detect_changes",
        "describe",
        "status",
        "snapshot",
        "api_surface",
        "boundary_check",
        "hotspots",
        "complexity",
        "refactor_candidates",
        "metrics_at",
        "dead_symbols",
        "deprecated_callers",
        "touches_hot_path",
        "coupling",
        "churn",
        "coupling_hubs",
        "crate_graph",
        "impact_check",
    ];

    #[test]
    fn registry_contains_full_catalog() {
        let names = registered_names();
        for expected in FULL_CATALOG {
            assert!(
                names.contains(expected),
                "registry missing expected operation '{expected}'"
            );
        }
    }

    #[test]
    fn registry_has_no_extra_ops() {
        let names = registered_names();
        assert_eq!(
            names.len(),
            FULL_CATALOG.len(),
            "registry has {} entries but full catalog has {}. \
             Extra: {:?}",
            names.len(),
            FULL_CATALOG.len(),
            names.iter().filter(|n| !FULL_CATALOG.contains(n)).collect::<Vec<_>>(),
        );
        for name in &names {
            assert!(
                FULL_CATALOG.contains(name),
                "registry has unexpected extra operation '{name}'"
            );
        }
    }

    #[test]
    fn lookup_finds_all_registered() {
        for entry in CODE_GRAPH_REGISTRY {
            let found = lookup_by_name(entry.name);
            assert!(found.is_some(), "lookup failed for '{}'", entry.name);
            assert_eq!(found.unwrap().name, entry.name);
        }
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        assert!(lookup_by_name("no_such_op").is_none());
    }

    #[test]
    fn vertical_slice_metadata_sanity() {
        // neighbors: single-key, edge-filter validation
        let n = lookup_by_name("neighbors").unwrap();
        assert_eq!(n.pre_resolve, PreResolveCategory::SingleKey);
        assert_eq!(n.validation, ValidationCategory::KeyWithEdgeFilters);
        assert_eq!(n.handler_fn, "code_graph_neighbors");
        assert_eq!(n.bridge_method, "neighbors");
        assert!(n.smoke.key.is_some());

        // impact: single-key, confidence validation, traversal seed
        let i = lookup_by_name("impact").unwrap();
        assert_eq!(i.pre_resolve, PreResolveCategory::SingleKey);
        assert_eq!(i.validation, ValidationCategory::KeyWithConfidence);
        assert_eq!(i.workspace, WorkspaceBehavior::TraversalSeedOnly);
        assert_eq!(i.bridge_method, "impact");

        // context: single-key, key-only validation
        let c = lookup_by_name("context").unwrap();
        assert_eq!(c.pre_resolve, PreResolveCategory::SingleKey);
        assert_eq!(c.validation, ValidationCategory::KeyOnly);
        assert_eq!(c.bridge_method, "context");

        // coupling_hotspots: no pre-resolve, no validation
        let ch = lookup_by_name("coupling_hotspots").unwrap();
        assert_eq!(ch.pre_resolve, PreResolveCategory::None);
        assert_eq!(ch.validation, ValidationCategory::None);
        assert_eq!(ch.bridge_method, "coupling_hotspots");
    }

    #[test]
    fn smoke_exemplars_have_operations() {
        for entry in CODE_GRAPH_REGISTRY {
            assert_eq!(
                entry.smoke.operation, entry.name,
                "smoke.exemplar.operation != name for '{}'",
                entry.name,
            );
        }
    }

    #[test]
    fn pre_resolve_matches_handler_dispatch() {
        // Operations that appear in `single_key_ops` in handler.rs
        // must be `SingleKey` here.
        for name in &["neighbors", "impact", "context", "implementations", "describe"] {
            let entry = lookup_by_name(name).unwrap();
            assert_eq!(
                entry.pre_resolve,
                PreResolveCategory::SingleKey,
                "{name}: expected SingleKey pre-resolve"
            );
        }
        // path is DualKey
        let path = lookup_by_name("path").unwrap();
        assert_eq!(path.pre_resolve, PreResolveCategory::DualKey);
        // coupling_hotspots and most others are NOT in single_key_ops
        let ch = lookup_by_name("coupling_hotspots").unwrap();
        assert_eq!(ch.pre_resolve, PreResolveCategory::None);
    }

    /// Every converted registry entry's `bridge_method` must appear
    /// in `KNOWN_BRIDGE_METHODS`.  This is the dynamic counterpart
    /// of the compile-time `const _` guard — it catches forward
    /// additions to the registry that forgot to extend the
    /// known-methods list.
    #[test]
    fn all_registered_bridge_methods_are_known() {
        for entry in CODE_GRAPH_REGISTRY {
            assert!(
                KNOWN_BRIDGE_METHODS.contains(&entry.bridge_method),
                "registry entry '{}' has bridge_method '{}' not in KNOWN_BRIDGE_METHODS",
                entry.name,
                entry.bridge_method,
            );
        }
    }

    /// Every `KNOWN_BRIDGE_METHODS` entry is referenced by at least
    /// one registry entry (guards against stale entries in the list).
    #[test]
    fn no_orphaned_known_bridge_methods() {
        let registry_methods: Vec<&str> = CODE_GRAPH_REGISTRY
            .iter()
            .map(|e| e.bridge_method)
            .collect();
        for method in KNOWN_BRIDGE_METHODS {
            assert!(
                registry_methods.contains(method),
                "KNOWN_BRIDGE_METHODS has '{method}' not referenced by any registry entry",
            );
        }
    }

    /// Every operation in the full catalog has a non-empty handler_fn.
    #[test]
    fn all_entries_have_handler_fn() {
        for entry in CODE_GRAPH_REGISTRY {
            assert!(
                !entry.handler_fn.is_empty(),
                "registry entry '{}' has empty handler_fn",
                entry.name,
            );
        }
    }

    /// Every operation in the full catalog has a non-empty bridge_method.
    #[test]
    fn all_entries_have_bridge_method() {
        for entry in CODE_GRAPH_REGISTRY {
            assert!(
                !entry.bridge_method.is_empty(),
                "registry entry '{}' has empty bridge_method",
                entry.name,
            );
        }
    }
}
