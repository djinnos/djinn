//! Declarative registry for `code_graph` operations.
//!
//! This module is the **single source of truth** for operation metadata.
//! Follow-up tasks will derive dispatch and pre-resolve tables from here
//! instead of maintaining separate handwritten match arms.
//!
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
pub(crate) enum PreResolveCategory {
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
pub(crate) enum ValidationCategory {
    /// `require_key` + direction/kind_filter/group_by validators.
    KeyWithEdgeFilters,
    /// `require_key` + group_by + min_confidence range check.
    KeyWithConfidence,
    /// `require_key` only; no extra param validators.
    KeyOnly,
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
pub(crate) enum WorkspaceBehavior {
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
pub(crate) struct SmokeExemplar {
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
pub(crate) struct OpEntry {
    /// Canonical operation string that appears on the wire
    /// (`params.operation`).
    pub name: &'static str,
    /// Accepted aliases.  Empty for the current vertical slice.
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
/// metadata.  The current vertical slice covers four operations;
/// follow-up tasks will extend this slice to cover the full set.
///
/// **Lookup helpers** — [`lookup_by_name`] performs a linear scan
/// (the table is small; a `HashMap` would cost more in `lazy_static`
/// machinery than it saves in lookup time).
pub(crate) const CODE_GRAPH_REGISTRY: &[OpEntry] = &[
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
            note: "360\u{b0} view of a symbol's incoming/outgoing edges",
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
];

// ── Lookup helpers ───────────────────────────────────────────────────────────

/// Find a registry entry by canonical name or alias.
///
/// Returns `None` if the operation string doesn't match any entry
/// (including operations that exist in `dispatch_code_graph_op` but
/// haven't been registered yet).
pub(crate) fn lookup_by_name(op: &str) -> Option<&'static OpEntry> {
    CODE_GRAPH_REGISTRY
        .iter()
        .find(|e| e.name == op || e.aliases.contains(&op))
}

/// Return the list of all registered canonical operation names.
///
/// Useful for error messages and test assertions.
pub(crate) fn registered_names() -> Vec<&'static str> {
    CODE_GRAPH_REGISTRY.iter().map(|e| e.name).collect()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_vertical_slice_ops() {
        let names = registered_names();
        for expected in &["neighbors", "impact", "context", "coupling_hotspots"] {
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
        for name in &["neighbors", "impact", "context"] {
            let entry = lookup_by_name(name).unwrap();
            assert_eq!(
                entry.pre_resolve,
                PreResolveCategory::SingleKey,
                "{name}: expected SingleKey pre-resolve"
            );
        }
        // coupling_hotspots is NOT in single_key_ops
        let ch = lookup_by_name("coupling_hotspots").unwrap();
        assert_eq!(ch.pre_resolve, PreResolveCategory::None);
    }
}
