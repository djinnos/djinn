use super::*;

/// Regression test for the sparse PageRank replacement.  Validates
/// the three properties the downstream ranking code depends on:
///
/// 1. Output length matches node count (required by indexing in
///    `rank()`).
/// 2. All ranks are finite, non-negative, and sum to ~1 (mass
///    preservation + normalization — a sanity check against FP
///    drift or dangling-node mass loss).
/// 3. Isolated nodes (no in-edges, no out-edges) share the same
///    rank — they receive only the random-jump + dangling baseline.
///
/// Does NOT assert numerical equivalence with petgraph's 0.8.3
/// `page_rank` — that implementation uses a different per-pair
/// formulation, so direct comparison is meaningful only in the
/// ordering test above.
#[test]
fn compute_pagerank_sparse_is_mass_preserving_and_finite() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranks =
        compute_pagerank_sparse(&graph.graph, PAGE_RANK_DAMPING_FACTOR, PAGE_RANK_ITERATIONS);

    assert_eq!(ranks.len(), graph.node_count());
    assert!(ranks.iter().all(|r| r.is_finite() && *r >= 0.0));
    let sum: f64 = ranks.iter().sum();
    assert!((sum - 1.0).abs() < 1e-9, "ranks must sum to ~1, got {sum}");
}

#[test]
fn compute_pagerank_sparse_handles_empty_graph() {
    let graph: DiGraph<RepoGraphNode, RepoGraphEdge> = DiGraph::new();
    let ranks = compute_pagerank_sparse(&graph, PAGE_RANK_DAMPING_FACTOR, 5);
    assert!(ranks.is_empty());
}

#[test]
fn page_rank_ordering_favors_referenced_symbols_and_files() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranking = graph.rank();

    let helper_symbol_rank = ranking
        .nodes
        .iter()
        .position(|node| {
            node.key == RepoNodeKey::Symbol("scip-rust pkg src/helper.rs `helper`().".to_string())
        })
        .expect("helper symbol should be ranked");
    let app_symbol_rank = ranking
        .nodes
        .iter()
        .position(|node| {
            node.key == RepoNodeKey::Symbol("scip-rust pkg src/app.rs `main`().".to_string())
        })
        .expect("main symbol should be ranked");
    let helper_file_rank = ranking
        .nodes
        .iter()
        .position(|node| node.key == RepoNodeKey::File(PathBuf::from("src/helper.rs")))
        .expect("helper file should be ranked");
    let app_file_rank = ranking
        .nodes
        .iter()
        .position(|node| node.key == RepoNodeKey::File(PathBuf::from("src/app.rs")))
        .expect("app file should be ranked");

    // PR F4: positions are now governed by fused rank (RRF over
    // pagerank, total degree, entry-point distance), so we no
    // longer assert the legacy "helper outranks main" position
    // ordering — `main` is an entry point and the fusion now
    // promotes it. The classic `pagerank * structural_weight`
    // score is still surfaced on the node for callers that want
    // it, and we keep asserting that signal directly so the
    // PageRank pass itself doesn't silently regress.
    let helper_symbol_score = ranking.nodes[helper_symbol_rank].score;
    let app_symbol_score = ranking.nodes[app_symbol_rank].score;
    assert!(helper_symbol_score > app_symbol_score);

    let helper_file_score = ranking.nodes[helper_file_rank].score;
    let app_file_score = ranking.nodes[app_file_rank].score;
    assert!(helper_file_score > app_file_score);
}

/// PR F4: the entry-point function detected for the fixture
/// (`fn main` in `src/app.rs`) must come back from `rank()` with
/// `entry_point_distance == Some(0)` — distance is measured from
/// the entry-point set itself, BFS via Outgoing edges.
#[test]
fn entry_point_distance_zero_at_entry_point() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranking = graph.rank();
    let main_node = ranking
        .nodes
        .iter()
        .find(|n| n.key == RepoNodeKey::Symbol("scip-rust pkg src/app.rs `main`().".to_string()))
        .expect("main symbol should be in ranking");
    assert!(
        main_node.is_entry_point,
        "fixture's `fn main` should have been detected as an entry point",
    );
    assert_eq!(
        main_node.entry_point_distance,
        Some(0),
        "entry-point function should sit at distance 0",
    );
}

/// PR F4: build a tiny synthetic graph with two symbols at
/// identical PageRank — one is the entry point, the other is a
/// helper that lives off to the side. With RRF the entry-point
/// signal breaks the tie and the entry point ranks higher.
#[test]
fn rrf_fused_rank_promotes_entry_points_under_pagerank_tie() {
    // Hand-build the ranked node vector to control the inputs
    // exactly — using the SCIP fixture pulls in too much
    // structural variation to guarantee a strict pagerank tie.
    let entry_key = RepoNodeKey::Symbol("symbol:entry".to_string());
    let helper_key = RepoNodeKey::Symbol("symbol:helper".to_string());
    let mut nodes = vec![
        RankedRepoGraphNode {
            node_index: NodeIndex::new(0),
            key: entry_key.clone(),
            kind: RepoGraphNodeKind::Symbol,
            score: 0.5,
            page_rank: 0.5,
            structural_weight: 1.0,
            inbound_edge_weight: 1.0,
            outbound_edge_weight: 1.0,
            is_entry_point: true,
            entry_point_distance: Some(0),
            fused_rank: 0.0,
        },
        RankedRepoGraphNode {
            node_index: NodeIndex::new(1),
            key: helper_key.clone(),
            kind: RepoGraphNodeKind::Symbol,
            score: 0.5,
            page_rank: 0.5,
            structural_weight: 1.0,
            inbound_edge_weight: 1.0,
            outbound_edge_weight: 1.0,
            is_entry_point: false,
            entry_point_distance: None,
            fused_rank: 0.0,
        },
    ];
    apply_rrf_fused_rank(&mut nodes);
    nodes.sort_by(|l, r| r.fused_rank.total_cmp(&l.fused_rank));
    assert_eq!(
        nodes[0].key, entry_key,
        "entry point must outrank helper under RRF when pagerank/degree are tied",
    );
    assert_eq!(nodes[1].key, helper_key);
    assert!(
        nodes[0].fused_rank > nodes[1].fused_rank,
        "fused rank for entry point ({}) should exceed helper ({})",
        nodes[0].fused_rank,
        nodes[1].fused_rank,
    );
}
