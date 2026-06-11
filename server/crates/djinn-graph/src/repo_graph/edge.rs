//! [`RepoGraphEdge`] and friends — the edge-side types of the in-memory
//! repo graph.

use serde::{Deserialize, Serialize};

use super::constants::{
    EDGE_CONFIDENCE_CONTAINS_DEFINITION, EDGE_CONFIDENCE_DECLARED_IN_FILE, EDGE_CONFIDENCE_DEFINES,
    EDGE_CONFIDENCE_ENTRY_POINT_OF, EDGE_CONFIDENCE_EXTENDS, EDGE_CONFIDENCE_FETCHES,
    EDGE_CONFIDENCE_FILE_REFERENCE, EDGE_CONFIDENCE_HANDLES_ROUTE, EDGE_CONFIDENCE_IMPLEMENTS,
    EDGE_CONFIDENCE_MEMBER_OF, EDGE_CONFIDENCE_READS, EDGE_CONFIDENCE_STEP_IN_PROCESS,
    EDGE_CONFIDENCE_SYMBOL_REFERENCE, EDGE_CONFIDENCE_TYPE_DEFINES, EDGE_CONFIDENCE_WRITES,
    EDGE_WEIGHT_DEFINES, EDGE_WEIGHT_DEFINITION_TO_FILE, EDGE_WEIGHT_ENTRY_POINT_OF,
    EDGE_WEIGHT_EXTENDS, EDGE_WEIGHT_FETCHES, EDGE_WEIGHT_FILE_REFERENCE,
    EDGE_WEIGHT_FILE_TO_DEFINITION, EDGE_WEIGHT_HANDLES_ROUTE, EDGE_WEIGHT_IMPLEMENTS,
    EDGE_WEIGHT_MEMBER_OF, EDGE_WEIGHT_STEP_IN_PROCESS, EDGE_WEIGHT_SYMBOL_REFERENCE,
    EDGE_WEIGHT_TYPE_DEFINES,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoGraphEdgeKind {
    ContainsDefinition,
    DeclaredInFile,
    FileReference,
    /// Generic symbol reference. PR A3 carves out `Reads` and `Writes`
    /// from this kind based on SCIP `SymbolRole::ReadAccess` /
    /// `WriteAccess`; this catch-all variant is still emitted for
    /// occurrences that carry neither role (e.g. `Import`, type-only
    /// references). Matches the `references` `EdgeCategory` per the
    /// inter-PR contract.
    SymbolReference,
    /// PR A3: SCIP `SymbolRole::ReadAccess` reference. Occurrences
    /// where the symbol is loaded/used without being written.
    Reads,
    /// PR A3: SCIP `SymbolRole::WriteAccess` reference. Occurrences
    /// where the symbol is assigned to or otherwise mutated.
    Writes,
    /// SCIP `Relationship.is_reference` — subtype-of / supertype-of.
    /// Used by scip-typescript for `class Foo extends Bar`, by
    /// rust-analyzer for supertrait references, and as a generic
    /// upward-typing pointer for cross-symbol relationships that aren't
    /// covered by the more specific variants below. Renamed from
    /// `SymbolRelationshipReference` in artifact v6 (PR clarity rename).
    Extends,
    /// SCIP `Relationship.is_implementation` — interface / trait
    /// implementation. `impl Trait for Struct` in Rust, `class Foo
    /// implements Bar` in TypeScript / Java, `class Child(Parent)` for
    /// ABC implementations in Python. Renamed from
    /// `SymbolRelationshipImplementation` in artifact v6.
    Implements,
    /// SCIP `Relationship.is_type_definition` — variable / parameter /
    /// return type, type alias target, generic bound. The receiver
    /// symbol's *type* is the target. Renamed from
    /// `SymbolRelationshipTypeDefinition` in artifact v6.
    TypeDefines,
    /// SCIP `Relationship.is_definition` — canonical-definition
    /// relationship. Rare; emitted when a symbol's definition is part of
    /// another symbol's defining region (e.g. a property defined inside
    /// a class without its own definition site). Renamed from
    /// `SymbolRelationshipDefinition` in artifact v6.
    Defines,
    /// PR F1: synthetic edge marking that the *target* symbol is an
    /// entry point of the *source* file (e.g. `src/main.rs ─EntryPointOf→
    /// fn main`). Stamped by [`crate::entry_points::detect_entry_points`]
    /// during graph build. `dead_symbols` excludes any node with an
    /// incoming `EntryPointOf` edge so test/main/HTTP-route symbols
    /// don't get false-positive flagged as dead. The edge carries the
    /// detector's per-hit confidence (0.6 – 0.95) and a `reason` string
    /// describing the matching heuristic (e.g. `"rust-main"`,
    /// `"scip-test-role"`, `"py-dunder-main"`).
    EntryPointOf,
    /// PR F3: synthesized "node X is a member of community Y" edge.
    /// Currently surfaced only via the per-graph
    /// [`crate::communities::Community`] sidecar — the variant exists in
    /// the enum (and in [`edge_confidence_floor`] / [`edge_weight`]) so
    /// downstream tools that iterate by edge kind have a stable kind
    /// name to dispatch on, even when no `MemberOf` edges are
    /// materialized into the petgraph.
    MemberOf,
    /// PR F2: synthetic edge linking a [`RepoGraphNodeKind::Process`]
    /// node to each [`RepoGraphNode`] along the deterministic call
    /// chain it traced. The 0-indexed step ordinal lives on
    /// [`RepoGraphEdge::step`] (the entry point is `step=0`, the
    /// terminal node is `step=step_count-1`). Confidence floor is 0.95 —
    /// process membership is computed from SCIP-derived edges, so it's
    /// as deterministic as the source graph.
    StepInProcess,
    /// PR s6ch / ykcg: synthetic edge from a first-class HTTP `Route`
    /// node to the `Symbol` node that handles it. Stamped by future
    /// server-side route extractors with per-edge confidence and a
    /// detector reason such as `"axum-route-attr"`.
    HandlesRoute,
    /// PR s6ch / ykcg: synthetic edge from a client/caller `Symbol` to
    /// the HTTP `Route` it fetches. Consumer-side route matching is
    /// inferential, so its confidence floor is lower than
    /// [`RepoGraphEdgeKind::HandlesRoute`].
    Fetches,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoGraphEdge {
    pub kind: RepoGraphEdgeKind,
    pub weight: f64,
    pub evidence_count: usize,
    /// Edge confidence in [0, 1]. Comes from a per-kind floor (PR A2 plan)
    /// optionally adjusted by the visibility heuristic. `min_confidence`
    /// filters in `code_graph.impact` use this value.
    pub confidence: f64,
    /// Optional human-readable explanation for the confidence value
    /// (e.g. `"local-prefix"` when one of the involved symbols is
    /// document-local). `None` means "default floor for kind, no
    /// adjustments applied".
    pub reason: Option<String>,
    /// PR F2: 0-indexed step ordinal — only populated on
    /// [`RepoGraphEdgeKind::StepInProcess`] edges. `None` for every
    /// other kind. Stored as a dedicated field rather than reusing
    /// `weight` so PageRank / shortest-path scoring stays oblivious to
    /// the process side-channel.
    #[serde(default)]
    pub step: Option<i32>,
}

/// Initial confidence floor for an edge of the given kind.
///
/// See the constants block at the top of this module for the table; the
/// values are tuned to put high-trust SCIP-derived edges (definitions,
/// declarations) above 0.9 and looser cross-symbol relationship edges in
/// the 0.8 band.
pub fn edge_confidence_floor(kind: RepoGraphEdgeKind) -> f64 {
    match kind {
        RepoGraphEdgeKind::ContainsDefinition => EDGE_CONFIDENCE_CONTAINS_DEFINITION,
        RepoGraphEdgeKind::DeclaredInFile => EDGE_CONFIDENCE_DECLARED_IN_FILE,
        RepoGraphEdgeKind::FileReference => EDGE_CONFIDENCE_FILE_REFERENCE,
        RepoGraphEdgeKind::SymbolReference => EDGE_CONFIDENCE_SYMBOL_REFERENCE,
        RepoGraphEdgeKind::Reads => EDGE_CONFIDENCE_READS,
        RepoGraphEdgeKind::Writes => EDGE_CONFIDENCE_WRITES,
        RepoGraphEdgeKind::Extends => EDGE_CONFIDENCE_EXTENDS,
        RepoGraphEdgeKind::Implements => EDGE_CONFIDENCE_IMPLEMENTS,
        RepoGraphEdgeKind::TypeDefines => EDGE_CONFIDENCE_TYPE_DEFINES,
        RepoGraphEdgeKind::Defines => EDGE_CONFIDENCE_DEFINES,
        RepoGraphEdgeKind::EntryPointOf => EDGE_CONFIDENCE_ENTRY_POINT_OF,
        RepoGraphEdgeKind::MemberOf => EDGE_CONFIDENCE_MEMBER_OF,
        RepoGraphEdgeKind::StepInProcess => EDGE_CONFIDENCE_STEP_IN_PROCESS,
        RepoGraphEdgeKind::HandlesRoute => EDGE_CONFIDENCE_HANDLES_ROUTE,
        RepoGraphEdgeKind::Fetches => EDGE_CONFIDENCE_FETCHES,
    }
}

/// PR F1: public wrapper around the per-kind weight table so the
/// entry-point detector (which lives in a sibling module and assembles
/// `EntryPointOf` edges by hand) can stay in sync with the build-time
/// weight assignments.
pub(crate) fn edge_weight_for(kind: RepoGraphEdgeKind) -> f64 {
    edge_weight(kind)
}

pub(crate) fn edge_weight(kind: RepoGraphEdgeKind) -> f64 {
    match kind {
        RepoGraphEdgeKind::ContainsDefinition => EDGE_WEIGHT_DEFINITION_TO_FILE,
        RepoGraphEdgeKind::DeclaredInFile => EDGE_WEIGHT_FILE_TO_DEFINITION,
        RepoGraphEdgeKind::FileReference => EDGE_WEIGHT_FILE_REFERENCE,
        // PR A3: `Reads` and `Writes` are refinements of `SymbolReference`;
        // they reuse the same structural weight so PageRank / shortest-path
        // results are stable across the split.
        RepoGraphEdgeKind::SymbolReference
        | RepoGraphEdgeKind::Reads
        | RepoGraphEdgeKind::Writes => EDGE_WEIGHT_SYMBOL_REFERENCE,
        RepoGraphEdgeKind::Extends => EDGE_WEIGHT_EXTENDS,
        RepoGraphEdgeKind::Implements => EDGE_WEIGHT_IMPLEMENTS,
        RepoGraphEdgeKind::TypeDefines => EDGE_WEIGHT_TYPE_DEFINES,
        RepoGraphEdgeKind::Defines => EDGE_WEIGHT_DEFINES,
        RepoGraphEdgeKind::EntryPointOf => EDGE_WEIGHT_ENTRY_POINT_OF,
        RepoGraphEdgeKind::MemberOf => EDGE_WEIGHT_MEMBER_OF,
        RepoGraphEdgeKind::StepInProcess => EDGE_WEIGHT_STEP_IN_PROCESS,
        RepoGraphEdgeKind::HandlesRoute => EDGE_WEIGHT_HANDLES_ROUTE,
        RepoGraphEdgeKind::Fetches => EDGE_WEIGHT_FETCHES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_edge_kinds_have_snake_case_serde_names() {
        assert_eq!(
            serde_json::to_string(&RepoGraphEdgeKind::HandlesRoute).expect("serialize kind"),
            "\"handles_route\""
        );
        assert_eq!(
            serde_json::to_string(&RepoGraphEdgeKind::Fetches).expect("serialize kind"),
            "\"fetches\""
        );
    }

    #[test]
    fn route_edge_confidence_and_weight_rows_are_pinned() {
        let handles_confidence = edge_confidence_floor(RepoGraphEdgeKind::HandlesRoute);
        let fetches_confidence = edge_confidence_floor(RepoGraphEdgeKind::Fetches);

        assert!(handles_confidence > fetches_confidence);
        assert!((handles_confidence - EDGE_CONFIDENCE_HANDLES_ROUTE).abs() < f64::EPSILON);
        assert!((fetches_confidence - EDGE_CONFIDENCE_FETCHES).abs() < f64::EPSILON);
        assert!(
            (edge_weight(RepoGraphEdgeKind::HandlesRoute) - EDGE_WEIGHT_HANDLES_ROUTE).abs()
                < f64::EPSILON
        );
        assert!(
            (edge_weight(RepoGraphEdgeKind::Fetches) - EDGE_WEIGHT_FETCHES).abs() < f64::EPSILON
        );
    }
}
