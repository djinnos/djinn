//! Declarative registry for `code_graph` operations.
//!
//! This module is the **single source of truth** for operation metadata.
//! `handler.rs` derives dispatch and pre-resolve tables from here so
//! adding a new operation requires one registry entry plus handler
//! logic — no independent handwritten operation-name lists.
//!
//! The registry is purely data — no build scripts, no runtime graph
//! cache access, no database or network dependencies.  Each entry is
//! a `const`-compatible [`OpEntry`] struct collected into the
//! [`CODE_GRAPH_REGISTRY`] static slice.
//!
//! Some fields (e.g. [`SmokeExemplar`], [`OpEntry::handler_fn`],
//! [`OpEntry::aliases`]) are test/documentary infrastructure not
//! referenced by production dispatch code; dead_code is suppressed
//! at the module boundary.
#![allow(dead_code)]
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
    /// `require_route_selector` (route_id or method+path).
    RouteSelector,
    /// `require_route_selector` + min_confidence range check.
    RouteSelectorConfidence,
    /// `require_query` + kind_filter validator.
    QueryKindFilter,
    /// `require_query` + flow kind_filter validator.
    QueryFlowFilter,
    /// `require_from_to` (dual key).
    FromTo,
    /// `require_globs` (from_glob + to_glob).
    Globs,
    /// kind_filter + sort_by (no key/query requirement).
    KindFilterSortBy,
    /// kind_filter only (no key/query requirement).
    KindFilterOnly,
    /// kind_filter + visibility.
    KindFilterVisibility,
    /// visibility only.
    VisibilityOnly,
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
            Self::RouteSelector => f.write_str("route_selector"),
            Self::RouteSelectorConfidence => f.write_str("route_selector_confidence"),
            Self::QueryKindFilter => f.write_str("query_kind_filter"),
            Self::QueryFlowFilter => f.write_str("query_flow_filter"),
            Self::FromTo => f.write_str("from_to"),
            Self::Globs => f.write_str("globs"),
            Self::KindFilterSortBy => f.write_str("kind_filter_sort_by"),
            Self::KindFilterOnly => f.write_str("kind_filter_only"),
            Self::KindFilterVisibility => f.write_str("kind_filter_visibility"),
            Self::VisibilityOnly => f.write_str("visibility_only"),
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
    /// Accepted aliases.  Empty for the current set.
    pub aliases: &'static [&'static str],
    /// Pre-resolve classification.
    pub pre_resolve: PreResolveCategory,
    /// Validation classification.
    pub validation: ValidationCategory,
    /// Name of the handler method on `DjinnMcpServer` (e.g.
    /// `"code_graph_neighbors"`).  Purely documentary; follow-up
    /// tasks may use it for compile-time dispatch.
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
/// metadata.  The full catalog covers every operation supported by
/// `dispatch_code_graph_op`.
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
    // ── ranked ───────────────────────────────────────────────────────
    //
    // Handler  : code_graph_ranked  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::ranked
    // Pre-res  : none
    // Validation: validate_kind_filter + validate_sort_by
    // Workspace: hard-scoped — workspace scopes the result set
    OpEntry {
        name: "ranked",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::KindFilterSortBy,
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
            note: "ranked nodes by pagerank",
        },
    },
    // ── implementations ──────────────────────────────────────────────
    //
    // Handler  : code_graph_implementations  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::implementations
    // Pre-res  : single_key
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
            note: "list implementations of a trait",
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
    // ── search ───────────────────────────────────────────────────────
    //
    // Handler  : code_graph_search  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::search (or hybrid_search via mode)
    // Pre-res  : none — `query` is a search string, not a node key
    // Validation: require_query + validate_kind_filter
    // Workspace: ignored — search index is workspace-agnostic
    OpEntry {
        name: "search",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::QueryKindFilter,
        handler_fn: "code_graph_search",
        bridge_method: "search",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "search",
            key: None,
            query: Some("MyStruct"),
            file: None,
            from: None,
            to: None,
            note: "name-index search for a symbol",
        },
    },
    // ── query_subgraph ───────────────────────────────────────────────
    //
    // Handler  : code_graph_query_subgraph  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::query_subgraph
    // Pre-res  : none
    // Validation: require_query + validate_kind_filter + inline bounded fields
    // Workspace: ignored — query_subgraph reads workspace from ctx directly
    OpEntry {
        name: "query_subgraph",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::QueryKindFilter,
        handler_fn: "code_graph_query_subgraph",
        bridge_method: "query_subgraph",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "query_subgraph",
            key: None,
            query: Some("auth middleware"),
            file: None,
            from: None,
            to: None,
            note: "subgraph extraction around a query",
        },
    },
    // ── route_map ────────────────────────────────────────────────────
    //
    // Handler  : code_graph_route_map  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::route_map
    // Pre-res  : none
    // Validation: none beyond bounded_required_limit
    // Workspace: ignored
    OpEntry {
        name: "route_map",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
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
            note: "list API routes",
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
            note: "check request/response shape for a route",
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
        validation: ValidationCategory::RouteSelectorConfidence,
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
            note: "impact of an API route change",
        },
    },
    // ── flow ─────────────────────────────────────────────────────────
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
        validation: ValidationCategory::QueryFlowFilter,
        handler_fn: "code_graph_flow",
        bridge_method: "flow",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "flow",
            key: None,
            query: Some("handle_request"),
            file: None,
            from: None,
            to: None,
            note: "process/step flow from an entry point",
        },
    },
    // ── cycles ───────────────────────────────────────────────────────
    //
    // Handler  : code_graph_cycles  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::cycles
    // Pre-res  : none
    // Validation: validate_kind_filter
    // Workspace: ignored — SCC cache is workspace-agnostic; hint only
    OpEntry {
        name: "cycles",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::KindFilterOnly,
        handler_fn: "code_graph_cycles",
        bridge_method: "cycles",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "cycles",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "detect dependency cycles",
        },
    },
    // ── orphans ──────────────────────────────────────────────────────
    //
    // Handler  : code_graph_orphans  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::orphans
    // Pre-res  : none
    // Validation: validate_kind_filter + validate_visibility
    // Workspace: hard-scoped
    OpEntry {
        name: "orphans",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::KindFilterVisibility,
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
            note: "find orphan nodes in the graph",
        },
    },
    // ── path ─────────────────────────────────────────────────────────
    //
    // Handler  : code_graph_path  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::path
    // Pre-res  : dual_key — `from` + `to` resolved individually
    // Validation: require_from_to
    // Workspace: traversal_seed_only
    OpEntry {
        name: "path",
        aliases: &[],
        pre_resolve: PreResolveCategory::DualKey,
        validation: ValidationCategory::FromTo,
        handler_fn: "code_graph_path",
        bridge_method: "path",
        workspace: WorkspaceBehavior::TraversalSeedOnly,
        smoke: SmokeExemplar {
            operation: "path",
            key: None,
            query: None,
            file: None,
            from: Some("MyStruct"),
            to: Some("OtherStruct"),
            note: "shortest path between two symbols",
        },
    },
    // ── edges ────────────────────────────────────────────────────────
    //
    // Handler  : code_graph_edges  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::edges
    // Pre-res  : none
    // Validation: require_globs
    // Workspace: ignored — edges are workspace-agnostic
    OpEntry {
        name: "edges",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::Globs,
        handler_fn: "code_graph_edges",
        bridge_method: "edges",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "edges",
            key: None,
            query: None,
            file: None,
            from: Some("src/**/*.rs"),
            to: Some("tests/**/*.rs"),
            note: "edges matching a glob pair",
        },
    },
    // ── describe ─────────────────────────────────────────────────────
    //
    // Handler  : code_graph_describe  (handler_basic_ops.rs)
    // Bridge   : RepoGraphOps::describe
    // Pre-res  : single_key
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
            note: "describe a symbol's role and edges",
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
            note: "360\u{b0} view of a symbol's incoming/outgoing edges",
        },
    },
    // ── status ───────────────────────────────────────────────────────
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
            note: "graph index status and freshness",
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
            note: "list workspaces in the project",
        },
    },
    // ── symbols_at ───────────────────────────────────────────────────
    //
    // Handler  : code_graph_symbols_at  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::symbols_at
    // Pre-res  : none
    // Validation: inline (require file + start_line)
    // Workspace: ignored
    OpEntry {
        name: "symbols_at",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
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
            note: "symbols at a line range in a file",
        },
    },
    // ── diff_touches ─────────────────────────────────────────────────
    //
    // Handler  : code_graph_diff_touches  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::diff_touches
    // Pre-res  : none
    // Validation: inline (require changed_ranges)
    // Workspace: ignored
    OpEntry {
        name: "diff_touches",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
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
            note: "symbols touched by line-range changes",
        },
    },
    // ── detect_changes ───────────────────────────────────────────────
    //
    // Handler  : code_graph_detect_changes  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::detect_changes
    // Pre-res  : none
    // Validation: inline (from_sha+to_sha or changed_files)
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
            note: "detect changed symbols between commits or files",
        },
    },
    // ── api_surface ──────────────────────────────────────────────────
    //
    // Handler  : code_graph_api_surface  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::api_surface
    // Pre-res  : none
    // Validation: validate_visibility
    // Workspace: hard-scoped
    OpEntry {
        name: "api_surface",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::VisibilityOnly,
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
            note: "public API surface of the project",
        },
    },
    // ── boundary_check ───────────────────────────────────────────────
    //
    // Handler  : code_graph_boundary_check  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::boundary_check
    // Pre-res  : none
    // Validation: inline (require rules)
    // Workspace: ignored
    OpEntry {
        name: "boundary_check",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
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
            note: "check module boundary violations",
        },
    },
    // ── hotspots ─────────────────────────────────────────────────────
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
            note: "high-churn hotspot files",
        },
    },
    // ── complexity ───────────────────────────────────────────────────
    //
    // Handler  : code_graph_complexity  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::complexity
    // Pre-res  : none
    // Validation: none
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
            note: "complexity rankings for functions or files",
        },
    },
    // ── refactor_candidates ──────────────────────────────────────────
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
            note: "composite complexity+churn+pagerank ranking",
        },
    },
    // ── metrics_at ───────────────────────────────────────────────────
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
            note: "graph metrics snapshot at current index",
        },
    },
    // ── dead_symbols ─────────────────────────────────────────────────
    //
    // Handler  : code_graph_dead_symbols  (handler_change_ops.rs)
    // Bridge   : RepoGraphOps::dead_symbols
    // Pre-res  : none
    // Validation: inline (confidence validation)
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
            note: "find symbols with no callers",
        },
    },
    // ── deprecated_callers ───────────────────────────────────────────
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
            note: "callers of deprecated symbols",
        },
    },
    // ── touches_hot_path ─────────────────────────────────────────────
    //
    // Handler  : code_graph_touches_hot_path  (handler_coupling_ops.rs)
    // Bridge   : RepoGraphOps::touches_hot_path
    // Pre-res  : none
    // Validation: inline (require seed_entries, seed_sinks, symbols)
    // Workspace: traversal_seed_only
    OpEntry {
        name: "touches_hot_path",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
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
            note: "check if symbols touch a hot path",
        },
    },
    // ── coupling ─────────────────────────────────────────────────────
    //
    // Handler  : code_graph_coupling  (handler_coupling_ops.rs)
    // Bridge   : RepoGraphOps::coupling
    // Pre-res  : none
    // Validation: inline (require file)
    // Workspace: ignored
    OpEntry {
        name: "coupling",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
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
            note: "files coupled with a given file",
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
    // ── coupling_hubs ────────────────────────────────────────────────
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
            note: "hub files with highest coupling degree",
        },
    },
    // ── churn ────────────────────────────────────────────────────────
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
            note: "highest-churn files by commit count",
        },
    },
    // ── snapshot ─────────────────────────────────────────────────────
    //
    // Handler  : code_graph_snapshot  (handler_coupling_ops.rs)
    // Bridge   : RepoGraphOps::snapshot
    // Pre-res  : none
    // Validation: inline (SnapshotLevel::parse + TestFilter::parse)
    // Workspace: hard-scoped
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
            note: "full repo graph snapshot for visualization",
        },
    },
    // ── crate_graph ──────────────────────────────────────────────────
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
            note: "inter-crate dependency graph",
        },
    },
    // ── impact_check ─────────────────────────────────────────────────
    //
    // Handler  : code_graph_impact_check  (handler_impact_check.rs)
    // Bridge   : composite — uses RepoGraphOps::impact + crate_graph
    // Pre-res  : none
    // Validation: inline (validate_impact_check_request)
    // Workspace: ignored
    OpEntry {
        name: "impact_check",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_impact_check",
        bridge_method: "impact",
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "impact_check",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "advisory impact preflight for proposed removals",
        },
    },
];

// ── Lookup helpers ───────────────────────────────────────────────────────────

/// Find a registry entry by canonical name or alias.
///
/// Returns `None` if the operation string doesn't match any entry.
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

/// Known `RepoGraphOps` trait method names that correspond to
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
pub const KNOWN_BRIDGE_METHODS: &[&str] = &[
    "neighbors",
    "ranked",
    "implementations",
    "impact",
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
    "describe",
    "context",
    "status",
    "workspaces",
    "symbols_at",
    "diff_touches",
    "detect_changes",
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
    "coupling_hotspots",
    "coupling_hubs",
    "churn",
    "snapshot",
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

    #[test]
    fn registry_contains_full_catalog() {
        let names = registered_names();
        for expected in &[
            "neighbors",
            "ranked",
            "implementations",
            "impact",
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
            "describe",
            "context",
            "status",
            "workspaces",
            "symbols_at",
            "diff_touches",
            "detect_changes",
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
            "coupling_hotspots",
            "coupling_hubs",
            "churn",
            "snapshot",
            "crate_graph",
            "impact_check",
        ] {
            assert!(
                names.contains(expected),
                "registry missing expected operation '{expected}'"
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
    fn pre_resolve_classification_sanity() {
        // Single-key ops must all be in the expected set
        let single_key: Vec<&str> = CODE_GRAPH_REGISTRY
            .iter()
            .filter(|e| e.pre_resolve == PreResolveCategory::SingleKey)
            .map(|e| e.name)
            .collect();
        for expected in &[
            "neighbors",
            "impact",
            "implementations",
            "describe",
            "context",
        ] {
            assert!(
                single_key.contains(expected),
                "{expected} should be SingleKey"
            );
        }
        assert!(
            !single_key.contains(&"search"),
            "search should NOT be SingleKey"
        );

        // Dual-key ops
        let dual_key: Vec<&str> = CODE_GRAPH_REGISTRY
            .iter()
            .filter(|e| e.pre_resolve == PreResolveCategory::DualKey)
            .map(|e| e.name)
            .collect();
        assert!(dual_key.contains(&"path"), "path should be DualKey");
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

    /// All validation categories are exercised by at least one registry entry.
    #[test]
    fn all_validation_categories_exercised() {
        let categories: Vec<ValidationCategory> =
            CODE_GRAPH_REGISTRY.iter().map(|e| e.validation).collect();
        let expected = [
            ValidationCategory::KeyWithEdgeFilters,
            ValidationCategory::KeyWithConfidence,
            ValidationCategory::KeyOnly,
            ValidationCategory::RouteSelector,
            ValidationCategory::RouteSelectorConfidence,
            ValidationCategory::QueryKindFilter,
            ValidationCategory::QueryFlowFilter,
            ValidationCategory::FromTo,
            ValidationCategory::Globs,
            ValidationCategory::KindFilterSortBy,
            ValidationCategory::KindFilterOnly,
            ValidationCategory::KindFilterVisibility,
            ValidationCategory::VisibilityOnly,
            ValidationCategory::None,
        ];
        for cat in expected {
            assert!(
                categories.contains(&cat),
                "validation category {cat:?} is not exercised by any registry entry"
            );
        }
    }

    /// All workspace behaviors are exercised by at least one registry entry.
    #[test]
    fn all_workspace_behaviors_exercised() {
        let behaviors: Vec<WorkspaceBehavior> =
            CODE_GRAPH_REGISTRY.iter().map(|e| e.workspace).collect();
        for expected in &[
            WorkspaceBehavior::Ignored,
            WorkspaceBehavior::TraversalSeedOnly,
            WorkspaceBehavior::HardScoped,
        ] {
            assert!(
                behaviors.contains(expected),
                "workspace behavior {expected:?} is not exercised by any registry entry"
            );
        }
    }
}
