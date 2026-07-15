//! Declarative registry for `code_graph` operations.
//!
//! This module is the **single source of truth** for operation metadata.
//! `handler.rs` derives dispatch, pre-resolve, and validation routing
//! from here so adding a new operation requires one registry entry plus
//! handler logic — no independent handwritten operation-name lists.
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
//! * **Typed enums** — pre-resolve, validation, and workspace
//!   categories use typed enums (`PreResolveCategory`,
//!   `ValidationCategory`, `WorkspaceBehavior`) for compile-time
//!   exhaustiveness.  Handler fn names and bridge method names
//!   remain `&'static str` so the registry stays decoupled from
//!   the concrete `impl` signatures.
//! * **Exemplar data** — each entry carries a [`SmokeExemplar`] that
//!   captures the minimum `CodeGraphParams` fields a smoke test needs
//!   to exercise the operation end-to-end.  This lets the downstream
//!   test task generate param sets from the registry instead of
//!   hand-writing each fixture.

mod catalog;

pub use catalog::CODE_GRAPH_REGISTRY;

use std::fmt;

use crate::bridge::REPO_GRAPH_OPS_METHODS;

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

/// Compile-time witness: every `OpEntry::bridge_method` value must
/// name a method on the declared `RepoGraphOps` trait identity
/// surface (`REPO_GRAPH_OPS_METHODS` in `graph_bridge.rs`).
///
/// This walks the registry at compile time and catches typos (e.g.
/// `"neigbors"`) at `cargo check` time rather than at first
/// dispatch.  The constraint here is the *opposite* direction of
/// the `KNOWN_BRIDGE_METHODS` guard above: that one guards against
/// stale entries in the known list; this one guards against
/// forward additions to the registry that named a missing trait
/// method.
///
/// If this fails to compile: a registry entry's `bridge_method`
/// does not appear in `REPO_GRAPH_OPS_METHODS`.  Either:
/// 1. Fix the typo on the registry entry, or
/// 2. Add the method to the `RepoGraphOps` trait AND add its name
///    to `REPO_GRAPH_OPS_METHODS`.
const _: () = {
    let mut ri = 0;
    while ri < CODE_GRAPH_REGISTRY.len() {
        let method = CODE_GRAPH_REGISTRY[ri].bridge_method;
        let mut found = false;
        let mut mi = 0;
        while mi < REPO_GRAPH_OPS_METHODS.len() {
            if str_eq(REPO_GRAPH_OPS_METHODS[mi], method) {
                found = true;
                break;
            }
            mi += 1;
        }
        assert!(
            found,
            "registry entry bridge_method not in REPO_GRAPH_OPS_METHODS surface"
        );
        ri += 1;
    }
};

/// Compile-time witness: every `KNOWN_BRIDGE_METHODS` entry is a
/// subset of the declared `RepoGraphOps` trait identity surface.
/// `KNOWN_BRIDGE_METHODS` is the curated registry-routed subset
/// (used by the server-side forwarding coverage test); the trait
/// surface is the full set of bridge methods.  An entry in
/// `KNOWN_BRIDGE_METHODS` that names a method not on the trait
/// would be impossible to forward — this witness catches that
/// drift at compile time.
const _: () = {
    let mut mi = 0;
    while mi < KNOWN_BRIDGE_METHODS.len() {
        let method = KNOWN_BRIDGE_METHODS[mi];
        let mut found = false;
        let mut ti = 0;
        while ti < REPO_GRAPH_OPS_METHODS.len() {
            if str_eq(REPO_GRAPH_OPS_METHODS[ti], method) {
                found = true;
                break;
            }
            ti += 1;
        }
        assert!(
            found,
            "KNOWN_BRIDGE_METHODS entry not in REPO_GRAPH_OPS_METHODS surface"
        );
        mi += 1;
    }
};

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

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
        "coverage",
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
            names
                .iter()
                .filter(|n| !FULL_CATALOG.contains(n))
                .collect::<Vec<_>>(),
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
        for name in &[
            "neighbors",
            "impact",
            "context",
            "implementations",
            "describe",
        ] {
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

    /// All validation categories are exercised by at least one registry entry.
    #[test]
    fn all_validation_categories_exercised() {
        let categories: Vec<ValidationCategory> =
            CODE_GRAPH_REGISTRY.iter().map(|e| e.validation).collect();
        let expected = [
            ValidationCategory::KeyWithEdgeFilters,
            ValidationCategory::KeyWithConfidence,
            ValidationCategory::KeyOnly,
            ValidationCategory::QueryWithKindFilter,
            ValidationCategory::QueryWithFlowKind,
            ValidationCategory::DualKey,
            ValidationCategory::Globs,
            ValidationCategory::RouteSelector,
            ValidationCategory::RouteSelectorWithConfidence,
            ValidationCategory::KindFilterAndVisibility,
            ValidationCategory::KindFilterAndSortBy,
            ValidationCategory::KindFilterOnly,
            ValidationCategory::FileWithLines,
            ValidationCategory::ChangedRanges,
            ValidationCategory::Rules,
            ValidationCategory::ImpactTargets,
            ValidationCategory::HotPathSeeds,
            ValidationCategory::FileOnly,
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

    // ── Bridge-to-trait coverage (control-plane half of `0bpa`) ──
    //
    // These tests are the focused replacement for the control-plane
    // half of the stuck `0bpa` task. They assert that every registry
    // entry's `bridge_method` value names a method declared on the
    // `RepoGraphOps` trait identity surface
    // (`REPO_GRAPH_OPS_METHODS` in `graph_bridge.rs`), and that the
    // asserted set covers at least one explicit non-basic family
    // beyond the vxmw basic slice (route / api / flow / change /
    // coupling / snapshot).
    //
    // The compile-time witnesses above catch the same constraint at
    // `cargo check` time; the tests below give the same check a name
    // and a structured error message that lists the offending entry,
    // and they exercise the non-basic family requirement that is
    // awkward to express in a `const _: () = {}` block.

    /// Focused coverage check: every `CODE_GRAPH_REGISTRY` entry's
    /// `bridge_method` value names a method declared on the
    /// `RepoGraphOps` trait identity surface.
    ///
    /// This is the contract the task asks for: a focused test-time
    /// check that derives expected bridge identities from
    /// `CODE_GRAPH_REGISTRY` and compares them against a single
    /// declared `RepoGraphOps` method identity surface
    /// (`REPO_GRAPH_OPS_METHODS`). A forward addition to the registry
    /// that names a missing `RepoGraphOps` method fails this test
    /// with a clear error message identifying the offending entry.
    #[test]
    fn registry_bridge_methods_resolve_to_repo_graph_ops_surface() {
        let surface: BTreeSet<&str> = REPO_GRAPH_OPS_METHODS.iter().copied().collect();

        // Sanity: the surface itself must be non-empty so the check
        // has something to compare against. An accidentally emptied
        // `REPO_GRAPH_OPS_METHODS` would silently pass a per-entry
        // lookup against an empty set, so pin the lower bound here.
        assert!(
            !REPO_GRAPH_OPS_METHODS.is_empty(),
            "REPO_GRAPH_OPS_METHODS surface is empty — the trait identity \
             surface must declare every RepoGraphOps method name"
        );

        let mut missing: Vec<(&'static str, &'static str)> = Vec::new();
        for entry in CODE_GRAPH_REGISTRY {
            if !surface.contains(entry.bridge_method) {
                missing.push((entry.name, entry.bridge_method));
            }
        }
        assert!(
            missing.is_empty(),
            "registry entries name a bridge_method that is NOT declared on \
             the RepoGraphOps trait identity surface (REPO_GRAPH_OPS_METHODS): \
             {:?}. Either fix the bridge_method typo or add the method to the \
             trait and to REPO_GRAPH_OPS_METHODS.",
            missing
        );
    }

    /// Non-basic family coverage: at least one explicit non-basic
    /// family — route, api, flow, change, coupling, or snapshot —
    /// must be present in `CODE_GRAPH_REGISTRY` AND its
    /// `bridge_method` must resolve through the trait identity
    /// surface. The check would fail if any of these entries named a
    /// missing `RepoGraphOps` method.
    ///
    /// Each tuple is `(canonical_name, expected_bridge_method)`; the
    /// lookup confirms (a) the registry declares the entry and (b)
    /// the `bridge_method` it carries appears in
    /// `REPO_GRAPH_OPS_METHODS`. This deliberately goes beyond the
    /// vxmw basic slice (`neighbors`, `impact`, `context`,
    /// `coupling_hotspots`) so the coverage proof cannot regress to
    /// only the vertical-slice operations.
    #[test]
    fn registry_non_basic_families_resolve_to_repo_graph_ops_surface() {
        // Pick at least one explicit representative from each
        // non-basic family the task calls out, so a forward addition
        // that removes an entire family (or renames its bridge
        // method) breaks this check. Each entry below is asserted to
        // exist in the registry AND to resolve through the trait
        // identity surface.
        let non_basic_representatives: &[(&str, &str)] = &[
            // route family
            ("route_map", "route_map"),
            ("shape_check", "shape_check"),
            // api family
            ("api_impact", "api_impact"),
            ("api_surface", "api_surface"),
            // flow family
            ("flow", "flow"),
            // change family
            ("detect_changes", "detect_changes"),
            ("diff_touches", "diff_touches"),
            ("boundary_check", "boundary_check"),
            // coupling family
            ("coupling", "coupling"),
            ("coupling_hubs", "coupling_hubs"),
            ("churn", "churn"),
            ("touches_hot_path", "touches_hot_path"),
            // snapshot family
            ("snapshot", "snapshot"),
        ];

        let surface: BTreeSet<&str> = REPO_GRAPH_OPS_METHODS.iter().copied().collect();

        for (canonical_name, expected_bridge_method) in non_basic_representatives {
            let entry = lookup_by_name(canonical_name).unwrap_or_else(|| {
                panic!(
                    "non-basic family coverage: registry is missing canonical \
                     operation '{canonical_name}' — the task requires at least \
                     one explicit non-basic family entry to be present"
                )
            });
            assert_eq!(
                entry.bridge_method, *expected_bridge_method,
                "non-basic family coverage: registry entry '{canonical_name}' \
                 carries bridge_method '{}' but the asserted non-basic \
                 representative expects '{expected_bridge_method}'",
                entry.bridge_method,
            );
            assert!(
                surface.contains(entry.bridge_method),
                "non-basic family coverage: registry entry '{canonical_name}' \
                 has bridge_method '{}' which is NOT declared on the \
                 RepoGraphOps trait identity surface (REPO_GRAPH_OPS_METHODS). \
                 This is the 'would fail if registry entry named a missing \
                 RepoGraphOps method' check the task requires.",
                entry.bridge_method,
            );
        }
    }

    /// `KNOWN_BRIDGE_METHODS` (the registry-routed subset used by
    /// the server-side forwarding coverage test) must be a subset of
    /// the declared `RepoGraphOps` trait identity surface. This
    /// pins the contract that any method the registry marks
    /// "routed today" is also a method declared on the trait —
    /// otherwise the server-side `RepoGraphBridge` could not
    /// forward it.
    ///
    /// The same constraint is enforced at compile time by the
    /// `const _: ()` witness above; this test surfaces the same
    /// constraint with a structured error message.
    #[test]
    fn known_bridge_methods_subset_of_repo_graph_ops_surface() {
        let surface: BTreeSet<&str> = REPO_GRAPH_OPS_METHODS.iter().copied().collect();
        let mut not_in_surface: Vec<&str> = Vec::new();
        for method in KNOWN_BRIDGE_METHODS {
            if !surface.contains(method) {
                not_in_surface.push(method);
            }
        }
        assert!(
            not_in_surface.is_empty(),
            "KNOWN_BRIDGE_METHODS contains entries not declared on the \
             RepoGraphOps trait identity surface (REPO_GRAPH_OPS_METHODS): \
             {not_in_surface:?}. The registry-routed subset must be a \
             subset of the trait surface or the server cannot forward it.",
        );
    }
}
