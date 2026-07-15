use std::path::PathBuf;

use super::*;
use crate::canonical_graph::CrateMap;
use crate::repo_graph::{
    RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
};

/// Build a multi-crate test fixture with 3+ crates (alpha, beta, gamma),
/// each containing 3+ nodes, internal heavy edges, and thin cross-crate
/// bridges. Returns the graph plus a `CrateMap` that maps each crate's
/// root directory to its crate name.
fn multi_crate_fixture() -> (RepoDependencyGraph, CrateMap) {
    use crate::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
    };

    let mk_node = |name: &str, file: &str| RepoGraphNode {
        id: RepoNodeKey::Symbol(format!("symbol:{name}")),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(file)),
        symbol: Some(format!("symbol:{name}")),
        symbol_kind: None,
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
        // alpha crate (nodes 0..3)
        mk_node("alpha_one", "crates/alpha/src/one.rs"),
        mk_node("alpha_two", "crates/alpha/src/two.rs"),
        mk_node("alpha_three", "crates/alpha/src/three.rs"),
        // beta crate (nodes 3..6)
        mk_node("beta_one", "crates/beta/src/one.rs"),
        mk_node("beta_two", "crates/beta/src/two.rs"),
        mk_node("beta_three", "crates/beta/src/three.rs"),
        // gamma crate (nodes 6..9)
        mk_node("gamma_one", "crates/gamma/src/one.rs"),
        mk_node("gamma_two", "crates/gamma/src/two.rs"),
        mk_node("gamma_three", "crates/gamma/src/three.rs"),
    ];

    let edge = |s, t, w| RepoGraphArtifactEdge {
        source: s,
        target: t,
        kind: RepoGraphEdgeKind::SymbolReference,
        weight: w,
        evidence_count: 1,
        confidence: 0.9,
        reason: None,
        step: None,
    };

    let edges = vec![
        // alpha internal heavy edges (triangle + extra)
        edge(0, 1, 5.0),
        edge(1, 0, 5.0),
        edge(1, 2, 5.0),
        edge(2, 1, 5.0),
        edge(0, 2, 5.0),
        edge(2, 0, 5.0),
        // beta internal heavy edges
        edge(3, 4, 5.0),
        edge(4, 3, 5.0),
        edge(4, 5, 5.0),
        edge(5, 4, 5.0),
        edge(3, 5, 5.0),
        edge(5, 3, 5.0),
        // gamma internal heavy edges
        edge(6, 7, 5.0),
        edge(7, 6, 5.0),
        edge(7, 8, 5.0),
        edge(8, 7, 5.0),
        edge(6, 8, 5.0),
        edge(8, 6, 5.0),
        // thin cross-crate bridges
        edge(2, 3, 0.5),
        edge(3, 2, 0.5),
        edge(5, 6, 0.5),
        edge(6, 5, 0.5),
    ];

    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes,
        edges,
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: Vec::new(),
        route_exclusion_config: Default::default(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    };
    let graph = RepoDependencyGraph::from_artifact(&artifact);

    let mut crate_map = CrateMap::new();
    crate_map.insert(PathBuf::from("crates/alpha"), "alpha".to_string());
    crate_map.insert(PathBuf::from("crates/beta"), "beta".to_string());
    crate_map.insert(PathBuf::from("crates/gamma"), "gamma".to_string());

    (graph, crate_map)
}

/// Compute the dominant-crate fraction for a community.
/// For each member, resolve its crate via `resolve_crate_for_node`.
/// Returns (max_crate_count / total_members) as a fraction in [0.0, 1.0].
fn crate_purity(community: &Community, graph: &RepoDependencyGraph, crate_map: &CrateMap) -> f64 {
    let pg = graph.graph();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for &v in &community.member_ids {
        let node = &pg[NodeIndex::new(v)];
        if let Some(crate_name) = resolve_crate_for_node(node.file_path.as_deref(), crate_map) {
            *counts.entry(crate_name.to_string()).or_default() += 1;
        }
    }
    let total = community.member_ids.len();
    if total == 0 {
        return 0.0;
    }
    let max_count = counts.values().copied().max().unwrap_or(0);
    max_count as f64 / total as f64
}

/// Return the purity of the *best* community for a given crate — i.e. the
/// highest fraction of that crate's nodes that landed in any single
/// community.  This is the per-crate metric used by the acceptance tests.
fn best_crate_purity(
    crate_name: &str,
    graph: &RepoDependencyGraph,
    communities: &[Community],
    crate_map: &CrateMap,
) -> f64 {
    let pg = graph.graph();
    let mut crate_nodes: Vec<usize> = Vec::new();
    for v in 0..pg.node_count() {
        let node = &pg[NodeIndex::new(v)];
        if resolve_crate_for_node(node.file_path.as_deref(), crate_map) == Some(crate_name) {
            crate_nodes.push(v);
        }
    }
    if crate_nodes.is_empty() {
        return 0.0;
    }
    let mut best = 0.0_f64;
    for comm in communities {
        let in_comm = crate_nodes
            .iter()
            .filter(|&&v| comm.member_ids.contains(&v))
            .count();
        let frac = in_comm as f64 / crate_nodes.len() as f64;
        if frac > best {
            best = frac;
        }
    }
    best
}

#[test]
fn seeded_communities_respect_crate_boundaries() {
    let (graph, crate_map) = multi_crate_fixture();
    let communities = detect_communities_with_options(
        &graph,
        CommunityDetectionOptions {
            resolution: Resolution::Medium,
            seed_by_crate: Some(crate_map.clone()),
        },
    );

    // Assert per-crate: ≥80% of each crate's nodes land in one community.
    for crate_name in ["alpha", "beta", "gamma"] {
        let purity = best_crate_purity(crate_name, &graph, &communities, &crate_map);
        assert!(
            purity >= 0.80,
            "crate '{}' should have ≥80% of its nodes in one community with seeding, got {:.2}",
            crate_name,
            purity
        );
    }

    // Assert per-community: every community with ≥2 members has ≥80%
    // purity (i.e. the dominant crate accounts for ≥80% of members).
    for comm in &communities {
        if comm.symbol_count >= 2 {
            let purity = crate_purity(comm, &graph, &crate_map);
            assert!(
                purity >= 0.80,
                "community '{}' ({} members) should have ≥80% purity, got {:.2}",
                comm.label,
                comm.symbol_count,
                purity
            );
        }
    }
}

#[test]
fn seeded_outperforms_unseeded() {
    let (graph, crate_map) = multi_crate_fixture();

    let seeded = detect_communities_with_options(
        &graph,
        CommunityDetectionOptions {
            resolution: Resolution::Medium,
            seed_by_crate: Some(crate_map.clone()),
        },
    );
    let unseeded = detect_communities_with_options(
        &graph,
        CommunityDetectionOptions {
            resolution: Resolution::Medium,
            seed_by_crate: None,
        },
    );

    let seeded_avg = ["alpha", "beta", "gamma"]
        .iter()
        .map(|c| best_crate_purity(c, &graph, &seeded, &crate_map))
        .sum::<f64>()
        / 3.0;
    let unseeded_avg = ["alpha", "beta", "gamma"]
        .iter()
        .map(|c| best_crate_purity(c, &graph, &unseeded, &crate_map))
        .sum::<f64>()
        / 3.0;

    // The fixture is designed so that unseeded detection may also find
    // the optimal partition (tight clusters + thin bridges), so we
    // assert seeded is *at least as good* rather than strictly greater.
    assert!(
        seeded_avg >= unseeded_avg,
        "seeded detection should have >= average crate purity than unseeded: seeded={:.3}, unseeded={:.3}",
        seeded_avg,
        unseeded_avg
    );
}

/// Build a tiny manual graph with two clusters of 3 nodes each,
/// connected internally by tight edges and across by a single
/// thin edge. Modularity should partition them cleanly.
///
/// We bypass the SCIP builder and inject nodes/edges directly via
/// the artifact round-trip seam, since the SCIP-shaped builder
/// doesn't expose a low-level "add edge" hook.
fn two_cluster_graph() -> RepoDependencyGraph {
    use crate::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
    };

    let mk_symbol_node = |name: &str, file: &str| RepoGraphNode {
        id: RepoNodeKey::Symbol(format!("symbol:{name}")),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(file)),
        symbol: Some(format!("symbol:{name}")),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: None,
        // PR s6ch / cs4v: route metadata is not applicable to
        // these placeholder symbol nodes — defaults to `None`.
        route_framework: None,
        route_handler_symbol: None,
    };

    let nodes = vec![
        mk_symbol_node("auth_login", "src/auth/login.rs"),
        mk_symbol_node("auth_session", "src/auth/session.rs"),
        mk_symbol_node("auth_token", "src/auth/token.rs"),
        mk_symbol_node("billing_charge", "src/billing/charge.rs"),
        mk_symbol_node("billing_invoice", "src/billing/invoice.rs"),
        mk_symbol_node("billing_refund", "src/billing/refund.rs"),
    ];

    let edge = |s, t, w| RepoGraphArtifactEdge {
        source: s,
        target: t,
        kind: RepoGraphEdgeKind::SymbolReference,
        weight: w,
        evidence_count: 1,
        confidence: 0.9,
        reason: None,
        step: None,
    };
    let edges = vec![
        // auth cluster: 0 ↔ 1, 1 ↔ 2, 0 ↔ 2 (triangle, heavy)
        edge(0, 1, 5.0),
        edge(1, 0, 5.0),
        edge(1, 2, 5.0),
        edge(2, 1, 5.0),
        edge(0, 2, 5.0),
        edge(2, 0, 5.0),
        // billing cluster: 3 ↔ 4, 4 ↔ 5, 3 ↔ 5
        edge(3, 4, 5.0),
        edge(4, 3, 5.0),
        edge(4, 5, 5.0),
        edge(5, 4, 5.0),
        edge(3, 5, 5.0),
        edge(5, 3, 5.0),
        // Thin bridge between clusters: 2 ↔ 3
        edge(2, 3, 0.5),
        edge(3, 2, 0.5),
    ];

    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes,
        edges,
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: Vec::new(),
        route_exclusion_config: Default::default(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    };
    RepoDependencyGraph::from_artifact(&artifact)
}

#[test]
fn detect_communities_empty_graph_returns_empty() {
    use crate::repo_graph::{REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact};
    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![],
        edges: vec![],
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: Vec::new(),
        route_exclusion_config: Default::default(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    };
    let graph = RepoDependencyGraph::from_artifact(&artifact);
    assert!(detect_communities(&graph).is_empty());
}

#[test]
fn detect_communities_partitions_two_tight_clusters() {
    let graph = two_cluster_graph();
    let communities = detect_communities(&graph);
    assert!(
        communities.len() >= 2,
        "expected at least two communities (auth + billing), got {}: {:?}",
        communities.len(),
        communities
            .iter()
            .map(|c| (c.label.clone(), c.member_ids.clone()))
            .collect::<Vec<_>>()
    );

    // Every member of the auth cluster should share a community
    // distinct from the billing cluster.
    let auth_idx: Vec<usize> = (0..3).collect();
    let billing_idx: Vec<usize> = (3..6).collect();

    let comm_for = |target: usize| -> Option<&Community> {
        communities.iter().find(|c| c.member_ids.contains(&target))
    };

    let auth_comm = comm_for(0).expect("auth_login should live in some community");
    for v in &auth_idx {
        assert!(
            auth_comm.member_ids.contains(v),
            "auth member {v} not in shared community {:?}",
            auth_comm.member_ids
        );
    }
    let billing_comm = comm_for(3).expect("billing_charge should live in some community");
    for v in &billing_idx {
        assert!(
            billing_comm.member_ids.contains(v),
            "billing member {v} not in shared community {:?}",
            billing_comm.member_ids
        );
    }
    assert_ne!(
        auth_comm.id, billing_comm.id,
        "auth and billing should not share a community"
    );

    // Cohesion: each cluster has 3 internal edges (undirected,
    // weight 5 each = 15) and 1 outgoing edge (weight 0.5) →
    // cohesion ≈ 15 / 15.5 ≈ 0.967.
    assert!(
        auth_comm.cohesion > 0.9,
        "auth cohesion too low: {}",
        auth_comm.cohesion
    );
    assert!(
        billing_comm.cohesion > 0.9,
        "billing cohesion too low: {}",
        billing_comm.cohesion
    );

    // Labels should pick up the *distinguishing* path component.
    // Both clusters share "src" as the first component, so the
    // distinguishing index is 1: "auth" vs "billing".
    assert_eq!(auth_comm.label, "auth");
    assert_eq!(billing_comm.label, "billing");
}

#[test]
fn community_id_is_stable_across_calls() {
    let graph = two_cluster_graph();
    let a = detect_communities(&graph);
    let b = detect_communities(&graph);
    assert_eq!(
        a.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        b.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        "community ids should be deterministic"
    );
}

#[test]
fn tokenize_splits_camel_snake_path() {
    let toks = tokenize_identifier("MyClass_handle_request::do_thing");
    assert_eq!(
        toks,
        vec!["my", "class", "handle", "request", "do", "thing"],
    );
}

#[test]
fn tokenize_drops_scip_punctuation() {
    let toks = tokenize_identifier("`helper`().");
    assert_eq!(toks, vec!["helper"]);
}

/// Sanity check the modularity of a known-clean partition is
/// strictly positive — this is the quality target community
/// detection optimizes. Q ranges over [-0.5, 1]; well-separated
/// clusters give Q > 0.3.
#[test]
fn modularity_of_two_cluster_partition_is_positive() {
    let graph = two_cluster_graph();
    let communities = detect_communities(&graph);
    // Compute Q from the communities.
    let pg = graph.graph();
    let mut adjacency: HashMap<usize, HashMap<usize, f64>> = HashMap::new();
    let mut k: HashMap<usize, f64> = HashMap::new();
    let mut m = 0.0_f64;
    for er in pg.edge_references() {
        let s = er.source().index();
        let t = er.target().index();
        let w = er.weight().weight;
        *adjacency.entry(s).or_default().entry(t).or_default() += w;
        *adjacency.entry(t).or_default().entry(s).or_default() += w;
        *k.entry(s).or_default() += w;
        *k.entry(t).or_default() += w;
        m += w;
    }
    let mut comm_of: HashMap<usize, &Community> = HashMap::new();
    for c in &communities {
        for &v in &c.member_ids {
            comm_of.insert(v, c);
        }
    }
    let mut q = 0.0_f64;
    for u in 0..pg.node_count() {
        for v in 0..pg.node_count() {
            let cu = comm_of.get(&u).map(|c| c.id.as_str());
            let cv = comm_of.get(&v).map(|c| c.id.as_str());
            if cu.is_none() || cv.is_none() || cu != cv {
                continue;
            }
            let a_uv = adjacency
                .get(&u)
                .and_then(|n| n.get(&v))
                .copied()
                .unwrap_or(0.0);
            let ku = k.get(&u).copied().unwrap_or(0.0);
            let kv = k.get(&v).copied().unwrap_or(0.0);
            q += a_uv - (ku * kv) / (2.0 * m);
        }
    }
    q /= 2.0 * m;
    assert!(
        q > 0.3,
        "expected positive modularity for clean cluster split, got Q={q}"
    );
}

/// Build a monorepo-style fixture with workspace="server" paths.
/// Three clusters: djinn-graph (3 nodes), djinn-auth (3 nodes),
/// djinn-billing (3 nodes). Paths all start with
/// `server/crates/<crate>/src/...` and workspace is "server".
fn monorepo_cluster_graph() -> RepoDependencyGraph {
    use crate::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
    };

    let mk_node = |name: &str, file: &str, ws: Option<&str>| RepoGraphNode {
        id: RepoNodeKey::Symbol(format!("symbol:{name}")),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(file)),
        symbol: Some(format!("symbol:{name}")),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: ws.map(str::to_string),
        route_framework: None,
        route_handler_symbol: None,
    };

    let nodes = vec![
        // djinn-graph cluster
        mk_node(
            "detect_communities",
            "server/crates/djinn-graph/src/communities.rs",
            Some("server"),
        ),
        mk_node(
            "derive_label",
            "server/crates/djinn-graph/src/communities.rs",
            Some("server"),
        ),
        mk_node(
            "tokenize_identifier",
            "server/crates/djinn-graph/src/communities.rs",
            Some("server"),
        ),
        // djinn-auth cluster
        mk_node(
            "auth_login",
            "server/crates/djinn-auth/src/login.rs",
            Some("server"),
        ),
        mk_node(
            "auth_session",
            "server/crates/djinn-auth/src/session.rs",
            Some("server"),
        ),
        mk_node(
            "auth_token",
            "server/crates/djinn-auth/src/token.rs",
            Some("server"),
        ),
        // djinn-billing cluster
        mk_node(
            "billing_charge",
            "server/crates/djinn-billing/src/charge.rs",
            Some("server"),
        ),
        mk_node(
            "billing_invoice",
            "server/crates/djinn-billing/src/invoice.rs",
            Some("server"),
        ),
        mk_node(
            "billing_refund",
            "server/crates/djinn-billing/src/refund.rs",
            Some("server"),
        ),
    ];

    let edge = |s, t, w| RepoGraphArtifactEdge {
        source: s,
        target: t,
        kind: RepoGraphEdgeKind::SymbolReference,
        weight: w,
        evidence_count: 1,
        confidence: 0.9,
        reason: None,
        step: None,
    };
    let edges = vec![
        // graph cluster: triangle
        edge(0, 1, 5.0),
        edge(1, 0, 5.0),
        edge(1, 2, 5.0),
        edge(2, 1, 5.0),
        edge(0, 2, 5.0),
        edge(2, 0, 5.0),
        // auth cluster: triangle
        edge(3, 4, 5.0),
        edge(4, 3, 5.0),
        edge(4, 5, 5.0),
        edge(5, 4, 5.0),
        edge(3, 5, 5.0),
        edge(5, 3, 5.0),
        // billing cluster: triangle
        edge(6, 7, 5.0),
        edge(7, 6, 5.0),
        edge(7, 8, 5.0),
        edge(8, 7, 5.0),
        edge(6, 8, 5.0),
        edge(8, 6, 5.0),
        // Thin bridges
        edge(2, 3, 0.5),
        edge(3, 2, 0.5),
        edge(5, 6, 0.5),
        edge(6, 5, 0.5),
    ];

    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes,
        edges,
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: Vec::new(),
        route_exclusion_config: Default::default(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    };
    RepoDependencyGraph::from_artifact(&artifact)
}

#[test]
fn monorepo_labels_are_not_all_server() {
    let graph = monorepo_cluster_graph();
    let communities = detect_communities(&graph);
    assert!(
        communities.len() >= 2,
        "expected at least two communities, got {}",
        communities.len()
    );
    // No community should be labeled "server" — the workspace root
    // segment should be skipped.
    for c in &communities {
        assert_ne!(
            c.label, "server",
            "community should not be labeled 'server' (workspace root): {:?}",
            c.label
        );
    }
}

#[test]
fn monorepo_labels_are_pairwise_distinct() {
    let graph = monorepo_cluster_graph();
    let communities = detect_communities(&graph);
    let labels: Vec<&str> = communities.iter().map(|c| c.label.as_str()).collect();
    let mut unique: Vec<&str> = labels.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        labels.len(),
        unique.len(),
        "community labels should be pairwise distinct, got: {:?}",
        labels
    );
}

#[test]
fn label_falls_back_to_keywords_when_paths_are_not_distinguishing() {
    use crate::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
    };

    let mk_node = |name: &str| RepoGraphNode {
        id: RepoNodeKey::Symbol(format!("symbol:{name}")),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from("server/crates/djinn-payments/src/lib.rs")),
        symbol: Some(format!("symbol:{name}")),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some("server".to_string()),
        route_framework: None,
        route_handler_symbol: None,
    };

    let edge = |s, t| RepoGraphArtifactEdge {
        source: s,
        target: t,
        kind: RepoGraphEdgeKind::SymbolReference,
        weight: 5.0,
        evidence_count: 1,
        confidence: 0.9,
        reason: None,
        step: None,
    };

    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![
            mk_node("payments_processor"),
            mk_node("payments_gateway"),
            mk_node("payments_refund"),
        ],
        edges: vec![edge(0, 1), edge(1, 0), edge(1, 2), edge(2, 1)],
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: Vec::new(),
        route_exclusion_config: Default::default(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    };
    let graph = RepoDependencyGraph::from_artifact(&artifact);

    assert_eq!(derive_label(&graph, &[0, 1, 2]), "payments");
}

#[test]
fn community_count_not_collapsed() {
    let graph = monorepo_cluster_graph();
    let communities = detect_communities(&graph);
    // With 9 nodes in 3 tight clusters, we should get at least 3
    // communities (not collapsed into one or two giant communities).
    assert!(
        communities.len() >= 3,
        "community count should not collapse for a 3-crate fixture, got {}",
        communities.len()
    );
}

#[test]
fn resolution_fine_produces_more_communities() {
    let graph = two_cluster_graph();
    let coarse = detect_communities_with_resolution(&graph, Resolution::Coarse);
    let fine = detect_communities_with_resolution(&graph, Resolution::Fine);
    // Fine should produce at least as many communities as coarse
    // (min size 1 vs 4).
    assert!(
        fine.len() >= coarse.len(),
        "fine resolution should produce >= coarse communities: fine={}, coarse={}",
        fine.len(),
        coarse.len()
    );
}

#[test]
fn community_detection_options_default_is_unseeded_medium() {
    let opts = CommunityDetectionOptions::default();
    assert_eq!(opts.resolution, Resolution::Medium);
    assert!(opts.seed_by_crate.is_none());
}

#[test]
fn detect_communities_with_options_unseeded_matches_default() {
    let graph = two_cluster_graph();
    let legacy = detect_communities(&graph);
    let via_options = detect_communities_with_options(&graph, CommunityDetectionOptions::default());
    assert_eq!(
        legacy, via_options,
        "unseeded options path must be identical to the legacy entry point"
    );
}

#[test]
fn detect_communities_with_options_resolution_matches_legacy() {
    let graph = two_cluster_graph();
    for resolution in [Resolution::Fine, Resolution::Medium, Resolution::Coarse] {
        let legacy = detect_communities_with_resolution(&graph, resolution);
        let via_options = detect_communities_with_options(
            &graph,
            CommunityDetectionOptions {
                resolution,
                seed_by_crate: None,
            },
        );
        assert_eq!(
            legacy, via_options,
            "resolution {:?}: options path must match legacy wrapper",
            resolution
        );
    }
}

#[test]
fn resolve_crate_for_node_longest_prefix_wins() {
    let mut crate_map = BTreeMap::new();
    crate_map.insert(PathBuf::from("src"), "root".to_string());
    crate_map.insert(PathBuf::from("src/auth"), "auth".to_string());

    // Longer prefix wins over the shorter parent.
    let resolved =
        resolve_crate_for_node(Some(std::path::Path::new("src/auth/login.rs")), &crate_map);
    assert_eq!(resolved, Some("auth"));

    // No file_path → None (no resolution possible).
    assert_eq!(resolve_crate_for_node(None, &crate_map), None);

    // Path outside any known crate prefix → None.
    assert_eq!(
        resolve_crate_for_node(Some(std::path::Path::new("other/x.rs")), &crate_map),
        None,
    );
}

#[test]
fn seed_partition_by_crate_groups_crate_mates() {
    let graph = two_cluster_graph();
    let mut crate_map = BTreeMap::new();
    crate_map.insert(PathBuf::from("src/auth"), "auth".to_string());
    crate_map.insert(PathBuf::from("src/billing"), "billing".to_string());

    let partition = seed_partition_by_crate(&graph, 6, &crate_map);
    assert_eq!(partition.len(), 6);

    // auth cluster (nodes 0,1,2) share one community ...
    assert_eq!(partition[0], partition[1]);
    assert_eq!(partition[1], partition[2]);
    // billing cluster (nodes 3,4,5) share another ...
    assert_eq!(partition[3], partition[4]);
    assert_eq!(partition[4], partition[5]);
    // ... and the two crates are distinct.
    assert_ne!(partition[0], partition[3]);
}

#[test]
fn seed_partition_by_crate_merges_paths_sharing_a_crate_name() {
    let graph = two_cluster_graph();
    // Two different prefixes map to the SAME crate name → one seed
    // community, exercising the name-based grouping.
    let mut crate_map = BTreeMap::new();
    crate_map.insert(PathBuf::from("src/auth"), "monolib".to_string());
    crate_map.insert(PathBuf::from("src/billing"), "monolib".to_string());

    let partition = seed_partition_by_crate(&graph, 6, &crate_map);
    let first = partition[0];
    for (v, &community) in partition.iter().enumerate().take(6) {
        assert_eq!(
            community, first,
            "node {v} should share the single monolib seed community"
        );
    }
}

#[test]
fn seed_partition_by_crate_empty_map_leaves_singletons() {
    let graph = two_cluster_graph();
    // Empty crate_map → nothing matches → every node is its own
    // singleton community (degrades to the unseeded initial state).
    let crate_map = BTreeMap::new();
    let partition = seed_partition_by_crate(&graph, 6, &crate_map);

    let mut ids = partition.clone();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 6, "each node should be a singleton community");
}

#[test]
fn seed_partition_by_crate_nodes_outside_crate_are_singletons() {
    let graph = two_cluster_graph();
    // Map only auth; billing nodes (3,4,5) match no crate prefix.
    let mut crate_map = BTreeMap::new();
    crate_map.insert(PathBuf::from("src/auth"), "auth".to_string());

    let partition = seed_partition_by_crate(&graph, 6, &crate_map);

    // auth nodes grouped together.
    assert_eq!(partition[0], partition[1]);
    assert_eq!(partition[1], partition[2]);

    // Each billing node gets a distinct singleton id, all different
    // from the auth community and from each other.
    assert_ne!(partition[3], partition[0]);
    assert_ne!(partition[4], partition[0]);
    assert_ne!(partition[5], partition[0]);
    assert_ne!(partition[3], partition[4]);
    assert_ne!(partition[4], partition[5]);
    assert_ne!(partition[3], partition[5]);
}

#[test]
fn detect_communities_with_options_seeded_keeps_crate_purity() {
    let graph = two_cluster_graph();
    let mut crate_map = BTreeMap::new();
    crate_map.insert(PathBuf::from("src/auth"), "auth".to_string());
    crate_map.insert(PathBuf::from("src/billing"), "billing".to_string());

    let communities = detect_communities_with_options(
        &graph,
        CommunityDetectionOptions {
            resolution: Resolution::Medium,
            seed_by_crate: Some(crate_map),
        },
    );

    // Seeding should keep the auth cluster predominantly together.
    let auth_comm = communities
        .iter()
        .find(|c| c.member_ids.contains(&0))
        .expect("auth_login should belong to some community");
    let auth_in_same = (0..3).filter(|v| auth_comm.member_ids.contains(v)).count();
    assert!(
        auth_in_same >= 2,
        "seeding should keep ≥2/3 auth nodes together, got {auth_in_same}: {:?}",
        auth_comm.member_ids
    );
}
