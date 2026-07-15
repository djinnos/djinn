//! Operation catalog data for the `code_graph` registry.
//!
//! This submodule holds the large `CODE_GRAPH_REGISTRY` const slice so that
//! the parent [`operation_registry`] module stays under the file-size guard.

use super::{OpEntry, PreResolveCategory, SmokeExemplar, ValidationCategory, WorkspaceBehavior};

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
    //            + validate_edge_kind_filter (reads/writes/co_changed_with)
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
    // ── coverage (glqk) ──────────────────────────────────────────────────
    //
    // Handler  : code_graph_coverage  (handler_basic_ops.rs)
    // Bridge   : none — reads `project_workspace_coverage` from the DB. The
    //            `bridge_method` is a formality to satisfy the compile-time
    //            KNOWN_BRIDGE_METHODS guard; `workspaces` is the closest read
    //            (both are cheap, no-blob per-workspace status surfaces).
    // Pre-res  : none
    // Validation: none
    // Workspace: ignored
    OpEntry {
        name: "coverage",
        aliases: &[],
        pre_resolve: PreResolveCategory::None,
        validation: ValidationCategory::None,
        handler_fn: "code_graph_coverage",
        bridge_method: "workspaces", // DB-only; reuses a known bridge key
        workspace: WorkspaceBehavior::Ignored,
        smoke: SmokeExemplar {
            operation: "coverage",
            key: None,
            query: None,
            file: None,
            from: None,
            to: None,
            note: "per-workspace/per-language index coverage table + gaps",
        },
    },
];
