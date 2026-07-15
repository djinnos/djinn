// PR njpa: impact BFS tests for synthesized trait-dispatch edges.
//
// These tests pin the `impact_bfs_with_policy` behavior when the
// graph contains synthesized `TraitDispatchCall` edges:
//
// 1. The synthesized caller → trait-method edge at the standard
//    0.70 confidence floor participates in blast-radius traversal
//    when `min_confidence` is at or below 0.70.
// 2. The same edge is excluded when `min_confidence` exceeds 0.70,
//    proving existing confidence filtering applies uniformly.
// 3. Directly extracted SCIP relationship edges (`Implements`,
//    `Defines`) retain their original confidence values and are
//    not downgraded by the query-layer confidence filter.
// 4. Default `min_confidence` (None → 0.85) is above the 0.70
//    synthesized floor, so callers that want trait-dispatch
//    callers in the blast radius must pass an explicit lower
//    threshold (e.g. `Some(0.70)` or `Some(0.0)`).

use djinn_graph::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
    RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
};
use djinn_graph::scip_parser::ScipSymbolKind;

use crate::mcp_bridge::shared;

/// Hardcoded confidence floor for `TraitDispatchCall` edges,
/// matching `EDGE_CONFIDENCE_TRAIT_DISPATCH_CALL` (0.70).
/// Kept as a local constant so the test documents the value
/// without importing a `pub(crate)` constant from djinn-graph.
const TRAIT_DISPATCH_CONFIDENCE: f64 = 0.70;

/// Build a fixture graph with the following structure:
///
/// ```text
/// [caller] ──TraitDispatchCall(0.70)──► [trait_method] ◄──Implements(0.90)── [impl_method]
///                                              ▲
///                                              │
///                                      TypeDefines(0.85)
///                                              │
///                                       [impl_type]
/// ```
///
/// Key design decisions for the fixture:
///
/// - `caller` → `trait_method` is the ONLY incoming edge to
///   `trait_method` that arrives via the `TraitDispatchCall` kind.
///   The caller is ONLY reachable from trait_method through this
///   edge — there are no alternative paths (e.g., no
///   `caller → impl_method → trait_method` chain).
///
/// - `impl_method` → `trait_method` via `Implements(0.90)` is a
///   high-confidence directly extracted edge. This lets us verify
///   that `Implements` edges survive filtering at thresholds where
///   `TraitDispatchCall` does not, and that their confidence value
///   is not altered by query-layer processing.
///
/// - `impl_type` → `trait_method` via `TypeDefines(0.85)` adds
///   another high-confidence incoming edge for diversity.
///
/// Returns `(graph, trait_method_idx)` plus formatted keys for each
/// symbol node.
fn build_trait_dispatch_impact_fixture() -> (
    RepoDependencyGraph,
    petgraph::graph::NodeIndex,
    String, // caller key
    String, // trait_method key
    String, // impl_method key
    String, // impl_type key
) {
    // RepoNodeKey values: the inner string should NOT include the
    // "symbol:" prefix because `format_node_key` prepends it.
    let caller_nk = RepoNodeKey::Symbol("caller_fn".to_string());
    let trait_method_nk = RepoNodeKey::Symbol("Trait#method".to_string());
    let impl_method_nk = RepoNodeKey::Symbol("impl_method".to_string());
    let impl_type_nk = RepoNodeKey::Symbol("TraitImpl#".to_string());

    let mk_node = |key: RepoNodeKey, name: &str, kind: ScipSymbolKind| RepoGraphNode {
        id: key.clone(),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(std::path::PathBuf::from("src/fixture.rs")),
        symbol: Some(match &key {
            RepoNodeKey::Symbol(s) => s.clone(),
            _ => String::new(),
        }),
        symbol_kind: Some(kind),
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: None,
        route_framework: None,
        route_handler_symbol: None,
    };

    let nodes = vec![
        mk_node(caller_nk.clone(), "caller_fn", ScipSymbolKind::Function), // [0]
        mk_node(trait_method_nk.clone(), "method", ScipSymbolKind::Method), // [1]
        mk_node(
            impl_method_nk.clone(),
            "impl_method",
            ScipSymbolKind::Method,
        ), // [2]
        mk_node(impl_type_nk.clone(), "TraitImpl", ScipSymbolKind::Type),  // [3]
    ];

    let mk_edge = |source: usize, target: usize, kind: RepoGraphEdgeKind, confidence: f64| {
        RepoGraphArtifactEdge {
            source,
            target,
            kind,
            weight: 1.0,
            evidence_count: 1,
            confidence,
            reason: None,
            step: None,
        }
    };

    let edges = vec![
        // [0]caller → [1]trait_method: synthesized trait-dispatch
        // caller edge at the standard 0.70 confidence floor.
        mk_edge(
            0,
            1,
            RepoGraphEdgeKind::TraitDispatchCall,
            TRAIT_DISPATCH_CONFIDENCE,
        ),
        // [2]impl_method → [1]trait_method: directly extracted
        // Implements edge at high confidence (0.90).
        mk_edge(2, 1, RepoGraphEdgeKind::Implements, 0.90),
        // [3]impl_type → [1]trait_method: directly extracted
        // TypeDefines edge at high confidence (0.85).
        mk_edge(3, 1, RepoGraphEdgeKind::TypeDefines, 0.85),
    ];

    let artifact = RepoGraphArtifact {
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
    };
    let graph = RepoDependencyGraph::from_artifact(&artifact);

    let trait_method_idx = graph
        .symbol_node("Trait#method")
        .expect("trait method node should resolve");

    (
        graph,
        trait_method_idx,
        "symbol:caller_fn".to_string(), // format_node_key output
        "symbol:Trait#method".to_string(),
        "symbol:impl_method".to_string(),
        "symbol:TraitImpl#".to_string(),
    )
}

/// AC1: `impact_bfs` includes the in-repo caller in the blast
/// radius for a trait-method node when `min_confidence` is at or
/// below the synthesized trait-dispatch edge confidence (0.70).
#[tokio::test]
async fn impact_includes_trait_dispatch_caller_when_confidence_at_floor() {
    let (graph, trait_method_idx, caller_key, _, _, _) = build_trait_dispatch_impact_fixture();

    // min_confidence = 0.70 (exactly at the trait-dispatch floor)
    let result = shared::impact_bfs(&graph, trait_method_idx, 3, Some(TRAIT_DISPATCH_CONFIDENCE));
    let keys: Vec<&str> = result.iter().map(|(_, e)| e.key.as_str()).collect();

    assert!(
        keys.iter().any(|k| *k == caller_key),
        "caller must appear in blast radius when min_confidence == trait-dispatch floor (0.70); got {keys:?}"
    );
}

/// AC1 (extreme): passing `Some(0.0)` opts into the full edge set,
/// which must include the trait-dispatch caller.
#[tokio::test]
async fn impact_includes_trait_dispatch_caller_when_confidence_zero() {
    let (graph, trait_method_idx, caller_key, _, _, _) = build_trait_dispatch_impact_fixture();

    let result = shared::impact_bfs(&graph, trait_method_idx, 3, Some(0.0));
    let keys: Vec<&str> = result.iter().map(|(_, e)| e.key.as_str()).collect();

    assert!(
        keys.iter().any(|k| *k == caller_key),
        "caller must appear in blast radius when min_confidence=0.0; got {keys:?}"
    );
}

/// AC1 (just below): even a threshold slightly below the floor
/// must include the trait-dispatch edge.
#[tokio::test]
async fn impact_includes_trait_dispatch_caller_when_confidence_just_below() {
    let (graph, trait_method_idx, caller_key, _, _, _) = build_trait_dispatch_impact_fixture();

    // Just below the floor should still include the caller.
    let result = shared::impact_bfs(
        &graph,
        trait_method_idx,
        3,
        Some(TRAIT_DISPATCH_CONFIDENCE - 0.01),
    );
    let keys: Vec<&str> = result.iter().map(|(_, e)| e.key.as_str()).collect();

    assert!(
        keys.iter().any(|k| *k == caller_key),
        "caller must appear when min_confidence is below the trait-dispatch floor; got {keys:?}"
    );
}

/// AC2: The same synthesized caller edge is excluded when
/// `min_confidence` is set above the edge's confidence (0.70).
/// This proves the existing confidence filtering semantics apply
/// uniformly to trait-dispatch edges.
#[tokio::test]
async fn impact_excludes_trait_dispatch_caller_when_confidence_above_floor() {
    let (graph, trait_method_idx, caller_key, _, impl_method_key, impl_type_key) =
        build_trait_dispatch_impact_fixture();

    // Use a threshold above 0.70 but at/below 0.85 to isolate the
    // trait-dispatch edge filtering while the Implements(0.90) and
    // TypeDefines(0.85) edges still pass.
    let result = shared::impact_bfs(&graph, trait_method_idx, 3, Some(0.75));
    let keys: Vec<&str> = result.iter().map(|(_, e)| e.key.as_str()).collect();

    // The caller is reachable from trait_method ONLY via the
    // TraitDispatchCall(0.70) incoming edge. With threshold 0.75,
    // that edge is filtered out, so the caller must NOT appear.
    assert!(
        !keys.iter().any(|k| *k == caller_key),
        "caller must be excluded when min_confidence (0.75) > trait-dispatch confidence (0.70); got {keys:?}"
    );

    // The Implements(0.90) and TypeDefines(0.85) edges are still
    // above 0.75, so impl_method and impl_type must remain.
    assert!(
        keys.iter().any(|k| *k == impl_method_key),
        "impl_method must remain at threshold 0.75 (Implements 0.90 >= 0.75); got {keys:?}"
    );
    assert!(
        keys.iter().any(|k| *k == impl_type_key),
        "impl_type must remain at threshold 0.75 (TypeDefines 0.85 >= 0.75); got {keys:?}"
    );
}

/// AC2 (extreme): threshold above 1.0 must exclude everything.
#[tokio::test]
async fn impact_excludes_everything_when_confidence_above_one() {
    let (graph, trait_method_idx, _, _, _, _) = build_trait_dispatch_impact_fixture();

    let result = shared::impact_bfs(&graph, trait_method_idx, 3, Some(1.5));
    assert!(
        result.is_empty(),
        "min_confidence above 1.0 must collapse the frontier to empty; got {} entries",
        result.len()
    );
}

/// AC3: Directly extracted SCIP relationship edges (`Implements`
/// and `TypeDefines`) in the fixture retain their original
/// confidence values and are not changed by query-layer filtering.
///
/// We verify this by running the BFS at thresholds that sit exactly
/// on the boundary of each edge's confidence and confirming the
/// behavior matches the original confidence (not any altered value).
#[tokio::test]
async fn directly_extracted_edges_retain_confidence_values() {
    let (graph, trait_method_idx, caller_key, _, impl_method_key, impl_type_key) =
        build_trait_dispatch_impact_fixture();

    // ── Threshold 0.85 (default): Implements(0.90) and
    // TypeDefines(0.85) both pass; TraitDispatchCall(0.70) fails.
    let result_default = shared::impact_bfs(&graph, trait_method_idx, 3, None);
    let keys_default: Vec<&str> = result_default.iter().map(|(_, e)| e.key.as_str()).collect();

    assert!(
        keys_default.iter().any(|k| *k == impl_method_key),
        "impl_method must be in blast radius at default threshold (Implements 0.90 >= 0.85); got {keys_default:?}"
    );
    assert!(
        keys_default.iter().any(|k| *k == impl_type_key),
        "impl_type must be in blast radius at default threshold (TypeDefines 0.85 >= 0.85); got {keys_default:?}"
    );
    // Caller is NOT reachable at default threshold (TraitDispatchCall
    // 0.70 < 0.85, and there is no alternative path in this fixture).
    assert!(
        !keys_default.iter().any(|k| *k == caller_key),
        "caller must be excluded at default threshold (TraitDispatchCall 0.70 < 0.85); got {keys_default:?}"
    );

    // ── Threshold 0.90: Implements(0.90) passes (0.90 >= 0.90),
    // TypeDefines(0.85) fails (0.85 < 0.90).
    let result_90 = shared::impact_bfs(&graph, trait_method_idx, 3, Some(0.90));
    let keys_90: Vec<&str> = result_90.iter().map(|(_, e)| e.key.as_str()).collect();

    assert!(
        keys_90.iter().any(|k| *k == impl_method_key),
        "impl_method reachable at threshold 0.90 (Implements 0.90 == 0.90); got {keys_90:?}"
    );
    assert!(
        !keys_90.iter().any(|k| *k == impl_type_key),
        "impl_type must drop out at threshold 0.90 (TypeDefines 0.85 < 0.90); got {keys_90:?}"
    );

    // ── Threshold 0.91: Implements(0.90) now fails (0.90 < 0.91).
    let result_91 = shared::impact_bfs(&graph, trait_method_idx, 3, Some(0.91));
    let keys_91: Vec<&str> = result_91.iter().map(|(_, e)| e.key.as_str()).collect();

    assert!(
        !keys_91.iter().any(|k| *k == impl_method_key),
        "impl_method must drop out at threshold 0.91 (Implements 0.90 < 0.91); got {keys_91:?}"
    );
    assert!(
        !keys_91.iter().any(|k| *k == impl_type_key),
        "impl_type must stay out at threshold 0.91; got {keys_91:?}"
    );

    // ── Threshold 0.89: Implements(0.90) passes, TypeDefines(0.85)
    // still fails.
    let result_89 = shared::impact_bfs(&graph, trait_method_idx, 3, Some(0.89));
    let keys_89: Vec<&str> = result_89.iter().map(|(_, e)| e.key.as_str()).collect();

    assert!(
        keys_89.iter().any(|k| *k == impl_method_key),
        "impl_method reachable at threshold 0.89 (Implements 0.90 >= 0.89); got {keys_89:?}"
    );

    // The key AC3 assertion: the confidence boundary is exactly
    // where the original edge confidence says it should be. The
    // Implements(0.90) edge passes at 0.89/0.90 and fails at 0.91.
    // TypeDefines(0.85) passes at 0.85 and fails at 0.86. Neither
    // confidence has been shifted by query-layer processing.
}

/// AC3 (edge confidence check): verify the edges in the built graph
/// carry exactly the expected confidence values, proving no
/// mutation occurs during graph construction or query traversal.
#[test]
fn graph_edges_carry_original_confidence_values() {
    let (graph, trait_method_idx, _, _, _, _) = build_trait_dispatch_impact_fixture();

    let edges: Vec<_> = graph
        .graph()
        .edges_directed(trait_method_idx, petgraph::Direction::Incoming)
        .map(|e| (e.weight().kind, e.weight().confidence))
        .collect();

    // Each incoming edge to trait_method must carry the exact
    // confidence value from the fixture.
    let td = edges
        .iter()
        .find(|(k, _)| *k == RepoGraphEdgeKind::TraitDispatchCall)
        .expect("TraitDispatchCall edge must be present");
    assert!(
        (td.1 - TRAIT_DISPATCH_CONFIDENCE).abs() < f64::EPSILON,
        "TraitDispatchCall confidence must be exactly {TRAIT_DISPATCH_CONFIDENCE}, got {}",
        td.1
    );

    let imp = edges
        .iter()
        .find(|(k, _)| *k == RepoGraphEdgeKind::Implements)
        .expect("Implements edge must be present");
    assert!(
        (imp.1 - 0.90).abs() < f64::EPSILON,
        "Implements confidence must be exactly 0.90, got {}",
        imp.1
    );

    let td2 = edges
        .iter()
        .find(|(k, _)| *k == RepoGraphEdgeKind::TypeDefines)
        .expect("TypeDefines edge must be present");
    assert!(
        (td2.1 - 0.85).abs() < f64::EPSILON,
        "TypeDefines confidence must be exactly 0.85, got {}",
        td2.1
    );
}

/// AC4: Document the default `min_confidence` behavior for the
/// synthesized trait-dispatch confidence floor.
///
/// The finalized 5wyo epic chose `EDGE_CONFIDENCE_TRAIT_DISPATCH_CALL
/// = 0.70` as the synthesized confidence floor. This is deliberately
/// below the default `min_confidence` threshold of 0.85 used by
/// `impact_bfs_with_policy` when the caller passes `None`.
///
/// **Consequence:** callers that want trait-dispatch edges to
/// participate in the default blast-radius traversal must pass an
/// explicit `min_confidence` at or below 0.70. The default path
/// (None → 0.85) intentionally excludes these synthesized edges
/// because they carry an `Inferred` confidence tier — a deliberate
/// signal that the call site's concrete resolution is uncertain.
///
/// This test pins that contract: the default threshold excludes the
/// trait-dispatch caller, and lowering to exactly the floor includes
/// it.
#[tokio::test]
async fn default_min_confidence_excludes_trait_dispatch_by_design() {
    let (graph, trait_method_idx, caller_key, _, impl_method_key, impl_type_key) =
        build_trait_dispatch_impact_fixture();

    // Default (None → 0.85): TraitDispatchCall(0.70) is below the
    // threshold, so the caller is NOT in the blast radius.
    let result_default = shared::impact_bfs(&graph, trait_method_idx, 3, None);
    let keys_default: Vec<&str> = result_default.iter().map(|(_, e)| e.key.as_str()).collect();

    assert!(
        !keys_default.iter().any(|k| *k == caller_key),
        "default threshold (0.85) must exclude trait-dispatch caller \
         (edge confidence 0.70 < 0.85) — callers must pass explicit \
         lower min_confidence to include synthesized edges; got {keys_default:?}"
    );

    // But impl_method IS reachable at the default threshold via
    // Implements(0.90 >= 0.85).
    assert!(
        keys_default.iter().any(|k| *k == impl_method_key),
        "impl_method must remain at default threshold (Implements 0.90 >= 0.85); got {keys_default:?}"
    );

    // And impl_type IS reachable at the default threshold via
    // TypeDefines(0.85 >= 0.85).
    assert!(
        keys_default.iter().any(|k| *k == impl_type_key),
        "impl_type must remain at default threshold (TypeDefines 0.85 >= 0.85); got {keys_default:?}"
    );

    // Lowering to the floor: caller appears.
    let result_floor =
        shared::impact_bfs(&graph, trait_method_idx, 3, Some(TRAIT_DISPATCH_CONFIDENCE));
    let keys_floor: Vec<&str> = result_floor.iter().map(|(_, e)| e.key.as_str()).collect();

    assert!(
        keys_floor.iter().any(|k| *k == caller_key),
        "at min_confidence=0.70 (trait-dispatch floor), the caller must be included; got {keys_floor:?}"
    );
    // impl_method still present.
    assert!(
        keys_floor.iter().any(|k| *k == impl_method_key),
        "impl_method must remain at min_confidence=0.70 (Implements 0.90 >= 0.70); got {keys_floor:?}"
    );
    // impl_type still present.
    assert!(
        keys_floor.iter().any(|k| *k == impl_type_key),
        "impl_type must remain at min_confidence=0.70 (TypeDefines 0.85 >= 0.70); got {keys_floor:?}"
    );
}

/// Verify that the `impact_bfs_with_policy` variant (with policy
/// parameter) behaves identically for trait-dispatch edges as the
/// plain `impact_bfs` — the policy only affects `Fetches`→`Route`
/// edges, not `TraitDispatchCall`.
#[tokio::test]
async fn impact_bfs_with_policy_treats_trait_dispatch_same_as_plain() {
    let (graph, trait_method_idx, caller_key, _, _, _) = build_trait_dispatch_impact_fixture();

    // With policy=None (no route-exclusion config), behavior must
    // match impact_bfs.
    let result_with_policy = shared::impact_bfs_with_policy(
        &graph,
        trait_method_idx,
        3,
        Some(TRAIT_DISPATCH_CONFIDENCE),
        None,
    );
    let result_plain =
        shared::impact_bfs(&graph, trait_method_idx, 3, Some(TRAIT_DISPATCH_CONFIDENCE));

    let keys_policy: Vec<&str> = result_with_policy
        .iter()
        .map(|(_, e)| e.key.as_str())
        .collect();
    let keys_plain: Vec<&str> = result_plain.iter().map(|(_, e)| e.key.as_str()).collect();

    assert_eq!(
        keys_policy, keys_plain,
        "impact_bfs_with_policy with policy=None must match impact_bfs for trait-dispatch edges"
    );

    assert!(
        keys_policy.iter().any(|k| *k == caller_key),
        "both variants must include the trait-dispatch caller at floor confidence; got {keys_policy:?}"
    );
}
