use super::*;
use crate::mcp_bridge::graph_ops::flow::test_helpers;
use djinn_control_plane::bridge::SearchHit;
use djinn_db::CodeChunkSearchHit;
use djinn_graph::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
    RepoGraphArtifactProcess, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
};
use djinn_graph::scip_parser::{ScipSymbolKind, ScipVisibility};

const ENTRY_SYM: &str = "scip-rust pkg src/checkout.rs `checkout`().";
const CHARGE_SYM: &str = "scip-rust pkg src/payments.rs `charge_card`().";
const EMAIL_SYM: &str = "scip-rust pkg src/email.rs `send_receipt`().";

fn symbol_node(symbol: &str, name: &str, file: &str) -> RepoGraphNode {
    RepoGraphNode {
        id: RepoNodeKey::Symbol(symbol.to_string()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(file)),
        symbol: Some(symbol.to_string()),
        symbol_kind: Some(ScipSymbolKind::Function),
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

fn process_node(id: &str, label: &str) -> RepoGraphNode {
    RepoGraphNode {
        id: RepoNodeKey::Process(id.to_string()),
        kind: RepoGraphNodeKind::Process,
        display_name: label.to_string(),
        language: None,
        file_path: None,
        symbol: None,
        symbol_kind: None,
        is_external: false,
        visibility: None,
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

fn edge(
    source: usize,
    target: usize,
    kind: RepoGraphEdgeKind,
    step: Option<i32>,
) -> RepoGraphArtifactEdge {
    RepoGraphArtifactEdge {
        source,
        target,
        kind,
        weight: 1.0,
        evidence_count: 1,
        confidence: 0.95,
        reason: None,
        step,
    }
}

fn flow_fixture_graph() -> RepoDependencyGraph {
    let process_id = "checkout-flow";
    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![
            process_node(process_id, "checkout process"),
            symbol_node(ENTRY_SYM, "checkout", "src/checkout.rs"),
            symbol_node(CHARGE_SYM, "charge_card", "src/payments.rs"),
            symbol_node(EMAIL_SYM, "send_receipt", "src/email.rs"),
        ],
        edges: vec![
            edge(0, 1, RepoGraphEdgeKind::StepInProcess, Some(0)),
            edge(0, 2, RepoGraphEdgeKind::StepInProcess, Some(1)),
            edge(0, 3, RepoGraphEdgeKind::StepInProcess, Some(2)),
        ],
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![],
        processes: vec![RepoGraphArtifactProcess {
            id: process_id.to_string(),
            label: "checkout process".to_string(),
            process_node: 0,
            entry_point: 1,
            terminal: 3,
            steps: vec![1, 2, 3],
        }],
        route_exclusion_config: Default::default(),
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    };
    RepoDependencyGraph::from_artifact(&artifact)
}

fn search_hit(key: &str, name: &str, score: f64) -> SearchHit {
    SearchHit {
        key: key.to_string(),
        uid: key.to_string(),
        kind: "function".to_string(),
        display_name: name.to_string(),
        score,
        file: Some("src/fixture.rs".to_string()),
        match_kind: Some("hybrid".to_string()),
    }
}

fn lexical_chunk_hit(symbol_key: &str, score: f64) -> CodeChunkSearchHit {
    CodeChunkSearchHit {
        chunk_id: "chunk-charge-card".to_string(),
        file_path: "src/payments.rs".to_string(),
        symbol_key: Some(symbol_key.to_string()),
        kind: "function".to_string(),
        start_line: 10,
        end_line: 24,
        score,
    }
}

#[test]
fn flow_maps_hybrid_symbol_hit_to_process_membership() {
    let graph = flow_fixture_graph();
    let result = test_helpers::flow_for_graph_from_hits(
        &graph,
        vec![search_hit(
            &format!("symbol:{CHARGE_SYM}"),
            "charge_card",
            0.42,
        )],
        None,
        20,
    )
    .expect("flow helper should succeed");

    assert_eq!(result.hits.len(), 1);
    let hit = &result.hits[0];
    assert_eq!(hit.process.id, "checkout-flow");
    assert_eq!(hit.process.label, "checkout process");
    assert_eq!(hit.process.role, "step");
    assert_eq!(hit.matched_step.name, "charge_card");
    assert_eq!(hit.matched_step.uid, format!("symbol:{CHARGE_SYM}"));
    assert_eq!(hit.matched_step_index, 1);
    assert_eq!(hit.rrf_score, 0.42);
}

#[test]
fn flow_surfaces_process_from_lexical_hit_fused_by_hybrid_pipeline() {
    let graph = flow_fixture_graph();

    // Fixture the DB/Qdrant-facing layer at the existing hybrid pipeline's
    // signal boundary: the lexical DB search found the charge step, while
    // semantic/Qdrant and structural search are empty. This proves `flow` can
    // consume the same RRF-fused `SearchHit` shape produced by
    // `hybrid_search::run` without duplicating BM25/semantic/RRF logic.
    let hybrid_hits = crate::mcp_bridge::hybrid_search::fuse_signals(
        vec![lexical_chunk_hit(CHARGE_SYM, 7.0)],
        vec![],
        vec![],
        20,
    );

    assert_eq!(hybrid_hits.len(), 1);
    assert_eq!(hybrid_hits[0].key, CHARGE_SYM);
    assert_eq!(hybrid_hits[0].match_kind.as_deref(), Some("lexical"));

    let result = test_helpers::flow_for_graph_from_hits(&graph, hybrid_hits, None, 20)
        .expect("flow helper should consume hybrid output");

    assert_eq!(result.hits.len(), 1);
    let hit = &result.hits[0];
    assert_eq!(hit.process.id, "checkout-flow");
    assert_eq!(hit.matched_step.uid, format!("symbol:{CHARGE_SYM}"));
    assert_eq!(hit.matched_step_index, 1);
    assert!(hit.rrf_score > 0.0, "RRF score from hybrid pipeline");
}

#[test]
fn flow_dedupes_and_sorts_by_score_then_step_then_process() {
    let graph = flow_fixture_graph();
    let result = test_helpers::flow_for_graph_from_hits(
        &graph,
        vec![
            search_hit(&format!("symbol:{EMAIL_SYM}"), "send_receipt", 0.50),
            search_hit(&format!("symbol:{CHARGE_SYM}"), "charge_card", 0.75),
            search_hit(&format!("symbol:{CHARGE_SYM}"), "charge_card", 0.25),
            search_hit(&format!("symbol:{ENTRY_SYM}"), "checkout", 0.50),
        ],
        Some("step"),
        2,
    )
    .expect("flow helper should succeed");

    assert_eq!(result.hits.len(), 2);
    assert_eq!(result.hits[0].matched_step.name, "charge_card");
    assert_eq!(result.hits[0].rrf_score, 0.75);
    assert_eq!(result.hits[1].matched_step.name, "checkout");
    assert_eq!(result.hits[1].matched_step_index, 0);
}

#[test]
fn flow_process_filter_returns_process_level_hits() {
    let graph = flow_fixture_graph();
    let result = test_helpers::flow_for_graph_from_hits(
        &graph,
        vec![search_hit(
            "process:checkout-flow",
            "checkout process",
            0.33,
        )],
        Some("process"),
        20,
    )
    .expect("flow helper should succeed");

    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].process.role, "process");
    assert_eq!(result.hits[0].matched_step.name, "checkout");
    assert_eq!(result.hits[0].matched_step_index, 0);
}

#[test]
fn flow_invalid_filter_and_empty_graphs_are_deterministic() {
    let graph = flow_fixture_graph();
    let err = test_helpers::flow_for_graph_from_hits(&graph, vec![], Some("symbol"), 20)
        .expect_err("invalid kind_filter should be rejected");
    assert!(err.contains("invalid kind_filter 'symbol' for flow"));

    let empty = RepoDependencyGraph::build(&[]);
    let result = test_helpers::flow_for_graph_from_hits(
        &empty,
        vec![search_hit(
            &format!("symbol:{CHARGE_SYM}"),
            "charge_card",
            0.42,
        )],
        None,
        20,
    )
    .expect("empty graph is a successful empty result");
    assert!(result.hits.is_empty());

    let no_match = test_helpers::flow_for_graph_from_hits(
        &graph,
        vec![search_hit(
            "symbol:scip-rust pkg src/other.rs `other`().",
            "other",
            0.9,
        )],
        None,
        20,
    )
    .expect("non-matching symbol hit is a successful empty result");
    assert!(no_match.hits.is_empty());
}
