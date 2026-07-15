//! [`RepoGraphEdge`] and friends — the edge-side types of the in-memory
//! repo graph.

use serde::{Deserialize, Serialize};

use super::constants::{
    EDGE_CONFIDENCE_CO_CHANGED_WITH, EDGE_CONFIDENCE_CONTAINS_DEFINITION,
    EDGE_CONFIDENCE_DECLARED_IN_FILE, EDGE_CONFIDENCE_DEFINES,
    EDGE_CONFIDENCE_ENTRY_POINT_OF, EDGE_CONFIDENCE_EXTENDS, EDGE_CONFIDENCE_FETCHES,
    EDGE_CONFIDENCE_FILE_REFERENCE, EDGE_CONFIDENCE_HANDLES_ROUTE, EDGE_CONFIDENCE_IMPLEMENTS,
    EDGE_CONFIDENCE_MEMBER_OF, EDGE_CONFIDENCE_READS, EDGE_CONFIDENCE_ROUTE,
    EDGE_CONFIDENCE_STEP_IN_PROCESS, EDGE_CONFIDENCE_SYMBOL_REFERENCE,
    EDGE_CONFIDENCE_TRAIT_DISPATCH_CALL, EDGE_CONFIDENCE_TYPE_DEFINES, EDGE_CONFIDENCE_WRITES,
    EDGE_WEIGHT_CO_CHANGED_WITH, EDGE_WEIGHT_DEFINES, EDGE_WEIGHT_DEFINITION_TO_FILE,
    EDGE_WEIGHT_ENTRY_POINT_OF,
    EDGE_WEIGHT_EXTENDS, EDGE_WEIGHT_FETCHES, EDGE_WEIGHT_FILE_REFERENCE,
    EDGE_WEIGHT_FILE_TO_DEFINITION, EDGE_WEIGHT_HANDLES_ROUTE, EDGE_WEIGHT_IMPLEMENTS,
    EDGE_WEIGHT_MEMBER_OF, EDGE_WEIGHT_ROUTE, EDGE_WEIGHT_STEP_IN_PROCESS,
    EDGE_WEIGHT_SYMBOL_REFERENCE, EDGE_WEIGHT_TRAIT_DISPATCH_CALL, EDGE_WEIGHT_TYPE_DEFINES,
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
    /// node to the `Symbol` node that handles it. Stamped by route
    /// extractors with per-edge confidence and a detector reason such as
    /// `"axum-route-attr"`.
    HandlesRoute,
    /// PR s6ch / ykcg: synthetic edge from a client/caller `Symbol` to
    /// the HTTP `Route` it fetches. Plain URL-literal matches are suggestions;
    /// matches stamped with explicit import evidence are promoted by
    /// [`promote_fetches_confidence_with_import_evidence`] and classify as
    /// [`EdgeConfidenceTier::Extracted`].
    Fetches,
    /// Inferred route relationship used by string/shape-based route matchers.
    /// Ordinary route string-shape matches classify as [`EdgeConfidenceTier::Inferred`].
    /// Appended after the v10 route-extraction variants to preserve existing
    /// bincode discriminants for `HandlesRoute` and `Fetches`.
    Route,
    /// Synthesized trait-dispatch caller edge: the *source* symbol calls a
    /// trait method (the *target*) via dynamic dispatch. The edge is stamped
    /// by the graph builder when a SCIP occurrence resolves to a trait-method
    /// symbol; the concrete implementation resolved at runtime is unknown at
    /// graph-build time. Carries [`REASON_TRAIT_DISPATCH_CALL`] as its
    /// `reason` and classifies as [`EdgeConfidenceTier::Inferred`] unless
    /// the confidence drops below the floor (→ `Ambiguous`).
    ///
    /// Appended after the v10 `Route` variant to preserve bincode
    /// discriminants for all pre-v11 variants. New in artifact v11.
    TraitDispatchCall,
    /// Proposal qoxm: commit co-change coupling. An undirected file↔file
    /// relationship (stored one direction, `file_a < file_b`) asserting the
    /// two files have historically changed together across commits. The edge
    /// carries `evidence_count` = distinct co-change commit count and
    /// `confidence` = the coupling *score* (a saturating function of the
    /// count); the temporal `last_co_change` epoch day rides in the `reason`
    /// (`"cochange;last_day=<n>"`). Because co-change is circumstantial
    /// evidence rather than SCIP proof, [`edge_confidence_tier`] never
    /// classifies it above [`EdgeConfidenceTier::Inferred`].
    ///
    /// These edges are materialized during warm into a dedicated sidecar
    /// (`RepoDependencyGraph::cochange_edges`) that lives OUTSIDE the
    /// PageRank/traversal petgraph, so they never silently inflate impact
    /// blast radii, degree, or rank. They round-trip through the artifact via
    /// the existing `edges` vec (partitioned back out on load). Appended last
    /// to preserve bincode discriminants for all earlier variants.
    CoChangedWith,
}

/// Model-level confidence tier for graph edges.
///
/// This is deliberately computed from persisted `kind` / `confidence` /
/// `reason` rather than stored on [`RepoGraphEdge`] or
/// [`crate::repo_graph::RepoGraphArtifactEdge`], so existing bincode artifacts
/// remain readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeConfidenceTier {
    Extracted,
    Inferred,
    Ambiguous,
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

impl RepoGraphEdge {
    pub fn confidence_tier(&self) -> EdgeConfidenceTier {
        edge_confidence_tier(self.kind, self.confidence, self.reason.as_deref())
    }
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
        RepoGraphEdgeKind::Route => EDGE_CONFIDENCE_ROUTE,
        RepoGraphEdgeKind::TraitDispatchCall => EDGE_CONFIDENCE_TRAIT_DISPATCH_CALL,
        RepoGraphEdgeKind::CoChangedWith => EDGE_CONFIDENCE_CO_CHANGED_WITH,
    }
}

pub fn edge_confidence_tier(
    kind: RepoGraphEdgeKind,
    confidence: f64,
    reason: Option<&str>,
) -> EdgeConfidenceTier {
    let floor = edge_confidence_floor(kind);
    let reason_lc = reason.unwrap_or_default().to_ascii_lowercase();
    if confidence + f64::EPSILON < floor
        || reason_lc.contains("ambiguous")
        || reason_lc.contains("suppressed")
        || reason_lc.contains("below-floor")
    {
        return EdgeConfidenceTier::Ambiguous;
    }

    match kind {
        RepoGraphEdgeKind::Fetches
            if is_fetches_import_evidence_reason(&reason_lc) && confidence >= 0.9 =>
        {
            EdgeConfidenceTier::Extracted
        }
        RepoGraphEdgeKind::Route
        | RepoGraphEdgeKind::Fetches
        | RepoGraphEdgeKind::TraitDispatchCall => EdgeConfidenceTier::Inferred,
        // Proposal qoxm: co-change is circumstantial history, never SCIP
        // proof — cap it at `Inferred`. The generic below-floor check above
        // already downgrades weak-score co-change edges to `Ambiguous`, so a
        // qualifying edge lands here as `Inferred` and can NEVER be
        // `Extracted`.
        RepoGraphEdgeKind::CoChangedWith => EdgeConfidenceTier::Inferred,
        RepoGraphEdgeKind::ContainsDefinition
        | RepoGraphEdgeKind::DeclaredInFile
        | RepoGraphEdgeKind::SymbolReference
        | RepoGraphEdgeKind::Writes
        | RepoGraphEdgeKind::EntryPointOf
        | RepoGraphEdgeKind::MemberOf
        | RepoGraphEdgeKind::StepInProcess
        | RepoGraphEdgeKind::HandlesRoute
            if confidence >= 0.9 =>
        {
            EdgeConfidenceTier::Extracted
        }
        _ => EdgeConfidenceTier::Inferred,
    }
}

fn is_fetches_import_evidence_reason(reason_lc: &str) -> bool {
    reason_lc.contains("import-evidence")
        || reason_lc.contains("import evidence")
        || reason_lc.contains("explicit-import")
}

/// Apply the import-evidence promotion contract for `Fetches` edges.
///
/// Callers that match a URL literal to a known imported route/client symbol
/// should use this helper before materializing the edge. It lifts the match
/// above 0.9 and stamps a reason that [`edge_confidence_tier`] recognizes as
/// extracted while leaving plain string-shape `Fetches` matches inferred.
pub fn promote_fetches_confidence_with_import_evidence(
    confidence: f64,
    reason: Option<&str>,
) -> (f64, String) {
    let promoted = confidence.max(0.91);
    let reason = match reason.filter(|value| !value.trim().is_empty()) {
        Some(value) if is_fetches_import_evidence_reason(&value.to_ascii_lowercase()) => {
            value.to_string()
        }
        Some(value) => format!("{value}; import-evidence"),
        None => "import-evidence".to_string(),
    };
    (promoted, reason)
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
        RepoGraphEdgeKind::Route => EDGE_WEIGHT_ROUTE,
        RepoGraphEdgeKind::TraitDispatchCall => EDGE_WEIGHT_TRAIT_DISPATCH_CALL,
        RepoGraphEdgeKind::CoChangedWith => EDGE_WEIGHT_CO_CHANGED_WITH,
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
        assert_eq!(
            serde_json::to_string(&RepoGraphEdgeKind::Route).expect("serialize kind"),
            "\"route\""
        );
        assert_eq!(
            serde_json::to_string(&RepoGraphEdgeKind::TraitDispatchCall).expect("serialize kind"),
            "\"trait_dispatch_call\""
        );
    }

    #[test]
    fn route_edge_confidence_and_weight_rows_are_pinned() {
        let handles_confidence = edge_confidence_floor(RepoGraphEdgeKind::HandlesRoute);
        let route_confidence = edge_confidence_floor(RepoGraphEdgeKind::Route);
        let fetches_confidence = edge_confidence_floor(RepoGraphEdgeKind::Fetches);
        let trait_dispatch_confidence = edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);

        assert!(handles_confidence > fetches_confidence);
        assert!(handles_confidence > route_confidence);
        // Trait-dispatch is lower-trust than route edges.
        assert!(route_confidence > trait_dispatch_confidence);
        assert!((handles_confidence - EDGE_CONFIDENCE_HANDLES_ROUTE).abs() < f64::EPSILON);
        assert!((fetches_confidence - EDGE_CONFIDENCE_FETCHES).abs() < f64::EPSILON);
        assert!((route_confidence - EDGE_CONFIDENCE_ROUTE).abs() < f64::EPSILON);
        assert!(
            (trait_dispatch_confidence - EDGE_CONFIDENCE_TRAIT_DISPATCH_CALL).abs() < f64::EPSILON
        );
        assert!(
            (edge_weight(RepoGraphEdgeKind::HandlesRoute) - EDGE_WEIGHT_HANDLES_ROUTE).abs()
                < f64::EPSILON
        );
        assert!(
            (edge_weight(RepoGraphEdgeKind::Fetches) - EDGE_WEIGHT_FETCHES).abs() < f64::EPSILON
        );
        assert!((edge_weight(RepoGraphEdgeKind::Route) - EDGE_WEIGHT_ROUTE).abs() < f64::EPSILON);
        assert!(
            (edge_weight(RepoGraphEdgeKind::TraitDispatchCall) - EDGE_WEIGHT_TRAIT_DISPATCH_CALL)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn confidence_tier_classifies_extracted_inferred_and_ambiguous_edges() {
        assert_eq!(
            edge_confidence_tier(RepoGraphEdgeKind::ContainsDefinition, 0.95, None),
            EdgeConfidenceTier::Extracted
        );
        assert_eq!(
            edge_confidence_tier(
                RepoGraphEdgeKind::HandlesRoute,
                0.90,
                Some("axum-router-new")
            ),
            EdgeConfidenceTier::Extracted
        );
        assert_eq!(
            edge_confidence_tier(RepoGraphEdgeKind::Route, 0.78, Some("string-shape")),
            EdgeConfidenceTier::Inferred
        );
        assert_eq!(
            edge_confidence_tier(RepoGraphEdgeKind::Fetches, 0.80, Some("url-literal")),
            EdgeConfidenceTier::Inferred
        );
        assert_eq!(
            edge_confidence_tier(RepoGraphEdgeKind::Route, 0.70, Some("string-shape")),
            EdgeConfidenceTier::Ambiguous
        );
        assert_eq!(
            edge_confidence_tier(RepoGraphEdgeKind::Fetches, 0.80, Some("ambiguous-path")),
            EdgeConfidenceTier::Ambiguous
        );
        // Trait-dispatch edges classify as Inferred at the floor and
        // Ambiguous when confidence drops below the floor.
        assert_eq!(
            edge_confidence_tier(
                RepoGraphEdgeKind::TraitDispatchCall,
                EDGE_CONFIDENCE_TRAIT_DISPATCH_CALL,
                Some("trait-dispatch-call")
            ),
            EdgeConfidenceTier::Inferred
        );
        assert_eq!(
            edge_confidence_tier(
                RepoGraphEdgeKind::TraitDispatchCall,
                0.50,
                Some("trait-dispatch-call")
            ),
            EdgeConfidenceTier::Ambiguous
        );
        assert_eq!(
            edge_confidence_tier(
                RepoGraphEdgeKind::TraitDispatchCall,
                EDGE_CONFIDENCE_TRAIT_DISPATCH_CALL,
                Some("below-floor-reason")
            ),
            EdgeConfidenceTier::Ambiguous
        );
    }

    #[test]
    fn co_changed_with_serde_name_and_tables_are_pinned() {
        assert_eq!(
            serde_json::to_string(&RepoGraphEdgeKind::CoChangedWith).expect("serialize kind"),
            "\"co_changed_with\""
        );
        // Confidence floor is the Inferred/Ambiguous band boundary.
        assert!(
            (edge_confidence_floor(RepoGraphEdgeKind::CoChangedWith)
                - EDGE_CONFIDENCE_CO_CHANGED_WITH)
                .abs()
                < f64::EPSILON
        );
        assert!(
            (edge_weight(RepoGraphEdgeKind::CoChangedWith) - EDGE_WEIGHT_CO_CHANGED_WITH).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn co_changed_with_is_inferred_or_ambiguous_never_extracted() {
        let reason = Some("cochange;last_day=20000");
        // At/above the score band → Inferred.
        assert_eq!(
            edge_confidence_tier(RepoGraphEdgeKind::CoChangedWith, 0.5, reason),
            EdgeConfidenceTier::Inferred
        );
        assert_eq!(
            edge_confidence_tier(RepoGraphEdgeKind::CoChangedWith, 0.95, reason),
            EdgeConfidenceTier::Inferred
        );
        // A very high score still never promotes to Extracted — co-change is
        // evidence, not proof.
        assert_ne!(
            edge_confidence_tier(RepoGraphEdgeKind::CoChangedWith, 1.0, reason),
            EdgeConfidenceTier::Extracted
        );
        // Below the band → Ambiguous.
        assert_eq!(
            edge_confidence_tier(RepoGraphEdgeKind::CoChangedWith, 0.3, reason),
            EdgeConfidenceTier::Ambiguous
        );
    }

    #[test]
    fn fetches_import_evidence_promotes_to_extracted() {
        let (confidence, reason) = promote_fetches_confidence_with_import_evidence(
            EDGE_CONFIDENCE_FETCHES,
            Some("url-literal"),
        );
        assert!(confidence > 0.9);
        assert!(reason.contains("import-evidence"));
        assert_eq!(
            edge_confidence_tier(RepoGraphEdgeKind::Fetches, confidence, Some(&reason)),
            EdgeConfidenceTier::Extracted
        );
    }

    #[test]
    fn confidence_tier_serde_uses_stable_snake_case_names() {
        assert_eq!(
            serde_json::to_string(&EdgeConfidenceTier::Extracted).expect("serialize"),
            "\"extracted\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeConfidenceTier::Inferred).expect("serialize"),
            "\"inferred\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeConfidenceTier::Ambiguous).expect("serialize"),
            "\"ambiguous\""
        );
    }
}
