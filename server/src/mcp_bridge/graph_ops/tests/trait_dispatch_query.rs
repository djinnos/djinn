//! Trait-dispatch query regression tests.
//!
//! Builds compact in-memory `RepoDependencyGraph` fixtures with trait
//! methods, concrete impl methods, caller symbols, and the finalized
//! trait-dispatch edge semantics from epic 5wyo. Exercises `context`
//! and `neighbors` query surfaces to verify that caller relationships
//! surface correctly for both the direct trait-method and the concrete
//! impl-method nodes.

use super::*;

use crate::mcp_bridge::graph_neighbors::edge_category_for;
use djinn_graph::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphEdgeKind,
    RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
};
use djinn_graph::scip_parser::{ScipSymbolKind, ScipVisibility};
use std::path::PathBuf;

// ── Fixture helpers ──────────────────────────────────────────────────────

const CALLER_SYM: &str = "scip-rust pkg src/order.rs `process_order`().";
const TRAIT_METHOD_SYM: &str = "scip-rust pkg src/repo.rs `Repository`#`find`().";
const IMPL_METHOD_SYM: &str = "scip-rust pkg src/pg.rs `PostgresRepo`#`find`().";

fn mk_symbol_node(sym: &str, display: &str, file: &str, kind: ScipSymbolKind) -> RepoGraphNode {
    RepoGraphNode {
        id: RepoNodeKey::Symbol(sym.to_string()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: display.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(file)),
        symbol: Some(sym.to_string()),
        symbol_kind: Some(kind),
        is_external: false,
        visibility: Some(ScipVisibility::Public),
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some("root".to_string()),
        route_framework: None,
        route_handler_symbol: None,
    }
}

fn mk_artifact_edge(
    src: usize,
    tgt: usize,
    kind: RepoGraphEdgeKind,
    confidence: f64,
    reason: Option<&str>,
) -> RepoGraphArtifactEdge {
    RepoGraphArtifactEdge {
        source: src,
        target: tgt,
        kind,
        weight: 1.0,
        evidence_count: 1,
        confidence,
        reason: reason.map(String::from),
        step: None,
    }
}

/// Builds a compact fixture graph exercising the canonical 5wyo
/// trait-dispatch semantics:
///
/// ```text
///  [0] caller ──TraitDispatchCall──▸ [1] trait_method
///   │                                  ▲ Implements
///   │                                  │
///   └────TraitDispatchCall (fanout)──▸ [2] impl_method
/// ```
///
/// - `caller → trait_method`: direct TraitDispatchCall
/// - `impl_method → trait_method`: Implements (relationship edge)
/// - `caller → impl_method`: TraitDispatchCall fan-out
fn trait_dispatch_fixture() -> djinn_graph::repo_graph::RepoDependencyGraph {
    let trait_dispatch_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);
    let implements_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::Implements);

    let nodes = vec![
        // [0] caller
        mk_symbol_node(
            CALLER_SYM,
            "process_order",
            "src/order.rs",
            ScipSymbolKind::Function,
        ),
        // [1] trait_method
        mk_symbol_node(
            TRAIT_METHOD_SYM,
            "find",
            "src/repo.rs",
            ScipSymbolKind::Method,
        ),
        // [2] impl_method
        mk_symbol_node(IMPL_METHOD_SYM, "find", "src/pg.rs", ScipSymbolKind::Method),
    ];

    let edges = vec![
        // Direct: caller → trait_method
        mk_artifact_edge(
            0,
            1,
            RepoGraphEdgeKind::TraitDispatchCall,
            trait_dispatch_conf,
            Some("trait-dispatch-call"),
        ),
        // Relationship: impl_method → trait_method
        mk_artifact_edge(2, 1, RepoGraphEdgeKind::Implements, implements_conf, None),
        // Fan-out: caller → impl_method
        mk_artifact_edge(
            0,
            2,
            RepoGraphEdgeKind::TraitDispatchCall,
            trait_dispatch_conf,
            Some("trait-dispatch-fanout"),
        ),
    ];

    djinn_graph::repo_graph::RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes,
        edges,
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: Default::default(),
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    })
}

/// Like [`trait_dispatch_fixture`] but WITHOUT the caller → impl_method
/// fan-out edge, simulating the case where the builder suppresses
/// fan-out because the trait has more impls than
/// `TRAIT_DISPATCH_FANOUT_CAP`.
///
/// ```text
///  [0] caller ──TraitDispatchCall──▸ [1] trait_method
///                                     ▲ Implements
///                                     │
///                                   [2] impl_method
/// ```
fn trait_dispatch_no_fanout_fixture() -> djinn_graph::repo_graph::RepoDependencyGraph {
    let trait_dispatch_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);
    let implements_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::Implements);

    let nodes = vec![
        // [0] caller
        mk_symbol_node(
            CALLER_SYM,
            "process_order",
            "src/order.rs",
            ScipSymbolKind::Function,
        ),
        // [1] trait_method
        mk_symbol_node(
            TRAIT_METHOD_SYM,
            "find",
            "src/repo.rs",
            ScipSymbolKind::Method,
        ),
        // [2] impl_method
        mk_symbol_node(IMPL_METHOD_SYM, "find", "src/pg.rs", ScipSymbolKind::Method),
    ];

    let edges = vec![
        // Direct: caller → trait_method
        mk_artifact_edge(
            0,
            1,
            RepoGraphEdgeKind::TraitDispatchCall,
            trait_dispatch_conf,
            Some("trait-dispatch-call"),
        ),
        // Relationship: impl_method → trait_method
        mk_artifact_edge(2, 1, RepoGraphEdgeKind::Implements, implements_conf, None),
        // NO fan-out edge from caller to impl_method.
    ];

    djinn_graph::repo_graph::RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes,
        edges,
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: Default::default(),
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    })
}

// ── Fixture sanity ───────────────────────────────────────────────────────

/// Verify the fixture itself is well-formed: all three nodes resolve,
/// the expected edges are present, and edge kinds/categories match
/// the canonical 5wyo semantics.
#[test]
fn fixture_sanity_trait_dispatch_edges_present() {
    let graph = trait_dispatch_fixture();

    let caller_idx = graph
        .symbol_node(CALLER_SYM)
        .expect("caller should resolve");
    let trait_idx = graph
        .symbol_node(TRAIT_METHOD_SYM)
        .expect("trait method should resolve");
    let impl_idx = graph
        .symbol_node(IMPL_METHOD_SYM)
        .expect("impl method should resolve");

    // caller → trait_method: TraitDispatchCall
    let direct_edges: Vec<_> = graph
        .graph()
        .edges_connecting(caller_idx, trait_idx)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
        .collect();
    assert_eq!(direct_edges.len(), 1);

    // impl_method → trait_method: Implements
    let impl_edges: Vec<_> = graph
        .graph()
        .edges_connecting(impl_idx, trait_idx)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::Implements)
        .collect();
    assert_eq!(impl_edges.len(), 1);

    // caller → impl_method: TraitDispatchCall (fan-out)
    let fanout_edges: Vec<_> = graph
        .graph()
        .edges_connecting(caller_idx, impl_idx)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
        .collect();
    assert_eq!(fanout_edges.len(), 1);

    // Confidence matches floor.
    let expected_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);
    assert!(
        (direct_edges[0].weight().confidence - expected_conf).abs() < f64::EPSILON,
        "direct TraitDispatchCall confidence should equal floor {expected_conf}"
    );

    // Categories.
    let trait_node = graph.node(trait_idx);
    assert_eq!(
        edge_category_for(Some(direct_edges[0].weight()), trait_node),
        EdgeCategory::Calls,
        "TraitDispatchCall should classify as EdgeCategory::Calls"
    );
    assert_eq!(
        edge_category_for(Some(impl_edges[0].weight()), trait_node),
        EdgeCategory::Implements,
        "Implements edge should classify as EdgeCategory::Implements"
    );
}

// ── Context: trait method ────────────────────────────────────────────────

/// AC2: `code_graph context` for the trait-method node returns the
/// caller in an incoming `Calls` bucket with the synthesized edge's
/// confidence (0.70 floor for TraitDispatchCall).
#[test]
fn trait_method_context_includes_caller_in_calls_bucket() {
    let graph = trait_dispatch_fixture();
    let node_index = graph
        .symbol_node(TRAIT_METHOD_SYM)
        .expect("trait_method should resolve in fixture");
    let (incoming, _outgoing) = collect_context_buckets(&graph, node_index);

    let calls = incoming
        .get(&EdgeCategory::Calls)
        .cloned()
        .unwrap_or_default();
    let caller_entry = calls
        .iter()
        .find(|r| r.name == "process_order")
        .expect("caller 'process_order' should appear in incoming.calls for trait method");

    let expected_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);
    assert!(
        (caller_entry.confidence - expected_conf).abs() < f64::EPSILON,
        "caller confidence {} should equal TraitDispatchCall floor {}",
        caller_entry.confidence,
        expected_conf,
    );
}

/// The trait method's outgoing Implements bucket should be empty —
/// the Implements edge is directed impl → trait (incoming from the
/// trait method's perspective).
#[test]
fn trait_method_context_outgoing_implements_is_empty() {
    let graph = trait_dispatch_fixture();
    let node_index = graph
        .symbol_node(TRAIT_METHOD_SYM)
        .expect("trait_method should resolve in fixture");
    let (_incoming, outgoing) = collect_context_buckets(&graph, node_index);

    let implements = outgoing
        .get(&EdgeCategory::Implements)
        .cloned()
        .unwrap_or_default();
    assert!(
        implements.is_empty(),
        "trait method's outgoing.implements should be empty (Implements edge is impl→trait); got {implements:?}"
    );
}

// ── Context: impl method (with fan-out) ──────────────────────────────────

/// AC3: With canonical fan-out, the impl-method's incoming Calls
/// bucket includes the caller via the fan-out TraitDispatchCall edge.
#[test]
fn impl_method_context_with_fanout_includes_caller_in_calls() {
    let graph = trait_dispatch_fixture();
    let node_index = graph
        .symbol_node(IMPL_METHOD_SYM)
        .expect("impl_method should resolve in fixture");
    let (incoming, _outgoing) = collect_context_buckets(&graph, node_index);

    let calls = incoming
        .get(&EdgeCategory::Calls)
        .cloned()
        .unwrap_or_default();
    assert!(
        calls.iter().any(|r| r.name == "process_order"),
        "with fan-out, impl_method should have 'process_order' in incoming.calls; got {calls:?}"
    );
}

/// With fan-out, the impl-method's outgoing Implements bucket includes
/// the trait method.
#[test]
fn impl_method_context_outgoing_implements_has_trait() {
    let graph = trait_dispatch_fixture();
    let node_index = graph
        .symbol_node(IMPL_METHOD_SYM)
        .expect("impl_method should resolve in fixture");
    let (_incoming, outgoing) = collect_context_buckets(&graph, node_index);

    let implements = outgoing
        .get(&EdgeCategory::Implements)
        .cloned()
        .unwrap_or_default();
    assert!(
        implements.iter().any(|r| r.uid.contains("repo.rs")),
        "impl_method should have trait_method in outgoing.implements; got {implements:?}"
    );
}

// ── Context: impl method (no fan-out) ────────────────────────────────────

/// AC3: When fan-out is suppressed, the impl-method does NOT have
/// the caller in its incoming Calls bucket — the query does not
/// fabricate caller unions.
#[test]
fn impl_method_context_without_fanout_has_no_caller_union() {
    let graph = trait_dispatch_no_fanout_fixture();
    let node_index = graph
        .symbol_node(IMPL_METHOD_SYM)
        .expect("impl_method should resolve in fixture");
    let (incoming, outgoing) = collect_context_buckets(&graph, node_index);

    let calls = incoming
        .get(&EdgeCategory::Calls)
        .cloned()
        .unwrap_or_default();
    assert!(
        calls.is_empty(),
        "without fan-out, impl_method should NOT fabricate caller unions in incoming.calls; got {calls:?}"
    );

    // The Implements hop to the trait method is still present.
    let implements = outgoing
        .get(&EdgeCategory::Implements)
        .cloned()
        .unwrap_or_default();
    assert!(
        !implements.is_empty(),
        "impl_method should still surface trait_method via outgoing.implements; got {implements:?}"
    );
}

// ── Neighbors: trait method ──────────────────────────────────────────────

/// AC3: `neighbors` for the trait-method node surfaces the caller as
/// an incoming TraitDispatchCall neighbor.
#[test]
fn trait_method_neighbors_include_caller_via_dispatch() {
    let graph = trait_dispatch_fixture();
    let node_index = graph
        .symbol_node(TRAIT_METHOD_SYM)
        .expect("trait_method should resolve in fixture");

    let incoming: Vec<(String, RepoGraphEdgeKind)> = graph
        .graph()
        .edges_directed(node_index, petgraph::Direction::Incoming)
        .map(|e| {
            let src = graph.node(e.source());
            (src.display_name.clone(), e.weight().kind)
        })
        .collect();

    assert!(
        incoming.iter().any(|(name, kind)| {
            name == "process_order" && *kind == RepoGraphEdgeKind::TraitDispatchCall
        }),
        "trait method should have caller as incoming TraitDispatchCall neighbor; got {incoming:?}"
    );
}

/// The trait-method node's incoming Implements neighbor is the
/// impl_method.
#[test]
fn trait_method_neighbors_include_impl_via_implements() {
    let graph = trait_dispatch_fixture();
    let node_index = graph
        .symbol_node(TRAIT_METHOD_SYM)
        .expect("trait_method should resolve in fixture");

    let incoming: Vec<(String, RepoGraphEdgeKind)> = graph
        .graph()
        .edges_directed(node_index, petgraph::Direction::Incoming)
        .map(|e| {
            let src = graph.node(e.source());
            (src.display_name.clone(), e.weight().kind)
        })
        .collect();

    assert!(
        incoming
            .iter()
            .any(|(name, kind)| *kind == RepoGraphEdgeKind::Implements
                && graph
                    .symbol_node(IMPL_METHOD_SYM)
                    .is_some_and(|idx| graph.node(idx).display_name == *name)),
        "trait method should have impl_method as incoming Implements neighbor; got {incoming:?}"
    );
}

// ── Neighbors: impl method (with fan-out) ────────────────────────────────

/// AC3: With canonical fan-out, the impl-method node surfaces the
/// caller as an incoming TraitDispatchCall neighbor.
#[test]
fn impl_method_neighbors_with_fanout_include_caller() {
    let graph = trait_dispatch_fixture();
    let node_index = graph
        .symbol_node(IMPL_METHOD_SYM)
        .expect("impl_method should resolve in fixture");

    let incoming: Vec<(String, RepoGraphEdgeKind)> = graph
        .graph()
        .edges_directed(node_index, petgraph::Direction::Incoming)
        .map(|e| {
            let src = graph.node(e.source());
            (src.display_name.clone(), e.weight().kind)
        })
        .collect();

    assert!(
        incoming.iter().any(|(name, kind)| {
            name == "process_order" && *kind == RepoGraphEdgeKind::TraitDispatchCall
        }),
        "with fan-out, impl_method should have caller as incoming TraitDispatchCall neighbor; got {incoming:?}"
    );
}

/// The impl-method node surfaces the trait method as an outgoing
/// Implements neighbor.
#[test]
fn impl_method_neighbors_outgoing_implements_to_trait() {
    let graph = trait_dispatch_fixture();
    let node_index = graph
        .symbol_node(IMPL_METHOD_SYM)
        .expect("impl_method should resolve in fixture");

    let outgoing: Vec<(String, RepoGraphEdgeKind)> = graph
        .graph()
        .edges_directed(node_index, petgraph::Direction::Outgoing)
        .map(|e| {
            let tgt = graph.node(e.target());
            (tgt.display_name.clone(), e.weight().kind)
        })
        .collect();

    assert!(
        outgoing
            .iter()
            .any(|(_, kind)| *kind == RepoGraphEdgeKind::Implements),
        "impl_method should have trait_method as outgoing Implements neighbor; got {outgoing:?}"
    );
}

// ── Neighbors: impl method (no fan-out) ──────────────────────────────────

/// AC3: When fan-out is suppressed, the impl-method node does NOT
/// have the caller as a neighbor. Only the explicit trait↔impl hop
/// is visible.
#[test]
fn impl_method_neighbors_without_fanout_exposes_only_impl_hop() {
    let graph = trait_dispatch_no_fanout_fixture();
    let node_index = graph
        .symbol_node(IMPL_METHOD_SYM)
        .expect("impl_method should resolve in fixture");

    let incoming: Vec<(String, RepoGraphEdgeKind)> = graph
        .graph()
        .edges_directed(node_index, petgraph::Direction::Incoming)
        .map(|e| {
            let src = graph.node(e.source());
            (src.display_name.clone(), e.weight().kind)
        })
        .collect();

    assert!(
        incoming.is_empty(),
        "without fan-out, impl_method should have NO incoming edges; got {incoming:?}"
    );

    let outgoing: Vec<(String, RepoGraphEdgeKind)> = graph
        .graph()
        .edges_directed(node_index, petgraph::Direction::Outgoing)
        .map(|e| {
            let tgt = graph.node(e.target());
            (tgt.display_name.clone(), e.weight().kind)
        })
        .collect();

    assert!(
        outgoing
            .iter()
            .any(|(_, kind)| *kind == RepoGraphEdgeKind::Implements),
        "impl_method should expose trait_method via outgoing Implements; got {outgoing:?}"
    );
}
