use std::path::Path;

use super::*;
use crate::repo_graph::{RepoGraphEdge, RepoGraphEdgeKind, RepoGraphNodeKind};

const AXUM_ONLY_ROUTE: &str = include_str!("fixtures/axum_only/server/src/routes.rs");
const TS_ONLY_UNKNOWN: &str = include_str!("fixtures/ts_only_no_match/ui/src/api/unknown.ts");
const FULL_E2E_ROUTE: &str = include_str!("fixtures/full_e2e/server/src/routes.rs");
const FULL_E2E_FETCH: &str = include_str!("fixtures/full_e2e/ui/src/api/fixture.ts");

#[derive(Debug)]
struct RouteCounts {
    route_nodes: usize,
    handles: Vec<RepoGraphEdge>,
    fetches: Vec<RepoGraphEdge>,
}

fn write_fixture(root: &Path, axum_source: Option<&str>, ts_file: Option<(&str, &str)>) {
    if let Some(source) = axum_source {
        std::fs::create_dir_all(root.join("server/src")).expect("create server fixture dir");
        std::fs::write(root.join("server/src/routes.rs"), source).expect("write rust fixture");
    }

    if let Some((relative_path, source)) = ts_file {
        let path = root.join(relative_path);
        std::fs::create_dir_all(path.parent().expect("fixture TS parent"))
            .expect("create TS fixture dir");
        std::fs::write(path, source).expect("write TS fixture");
    }
}

/// End-to-end route-extraction harness that exercises the same source-backed
/// canonical graph post-processing pass that `ensure_canonical_graph` invokes:
/// build a file/symbol graph for the fixture, run route extraction against the
/// fixture project root, then inspect the materialized canonical graph shape.
fn ensure_canonical_graph_route_extraction_fixture(
    axum_source: Option<&str>,
    ts_file: Option<(&str, &str)>,
    include_axum_graph: bool,
    include_ts_graph: bool,
) -> (
    RouteExtractionReport,
    crate::repo_graph::RepoDependencyGraph,
) {
    let temp = tempfile::tempdir().expect("create route extraction e2e fixture dir");
    write_fixture(temp.path(), axum_source, ts_file);

    let mut graph = route_fixture_graph(include_axum_graph, include_ts_graph);
    let report = detect_routes(&mut graph, temp.path());
    (report, graph)
}

fn counts(graph: &crate::repo_graph::RepoDependencyGraph) -> RouteCounts {
    RouteCounts {
        route_nodes: graph
            .graph()
            .node_weights()
            .filter(|node| node.kind == RepoGraphNodeKind::Route)
            .count(),
        handles: graph
            .graph()
            .edge_weights()
            .filter(|edge| edge.kind == RepoGraphEdgeKind::HandlesRoute)
            .cloned()
            .collect(),
        fetches: graph
            .graph()
            .edge_weights()
            .filter(|edge| edge.kind == RepoGraphEdgeKind::Fetches)
            .cloned()
            .collect(),
    }
}

#[test]
fn axum_only_round_trips_to_route_and_handler_edge_without_fetches() {
    let (report, graph) =
        ensure_canonical_graph_route_extraction_fixture(Some(AXUM_ONLY_ROUTE), None, true, false);
    let counts = counts(&graph);

    assert_eq!(report.route_nodes_added, 1);
    assert_eq!(report.handles_route_edges_added, 1);
    assert_eq!(report.fetches_edges_added, 0);
    assert_eq!(report.unmatched_fetch_count, 0);
    assert_eq!(counts.route_nodes, 1);
    assert_eq!(counts.handles.len(), 1);
    assert_eq!(counts.fetches.len(), 0);
}

#[test]
fn ts_only_no_match_records_unmatched_fetch_without_graph_pollution() {
    let (report, graph) = ensure_canonical_graph_route_extraction_fixture(
        None,
        Some(("ui/src/api/agents.ts", TS_ONLY_UNKNOWN)),
        false,
        true,
    );
    let counts = counts(&graph);

    assert_eq!(report.route_nodes_added, 0);
    assert_eq!(report.handles_route_edges_added, 0);
    assert_eq!(report.fetches_edges_added, 0);
    assert_eq!(report.unmatched_fetch_count, 1);
    assert_eq!(counts.route_nodes, 0);
    assert_eq!(counts.handles.len(), 0);
    assert_eq!(counts.fetches.len(), 0);
}

#[test]
fn full_e2e_round_trips_to_route_handler_and_fetch_edges() {
    let (report, graph) = ensure_canonical_graph_route_extraction_fixture(
        Some(FULL_E2E_ROUTE),
        Some(("ui/src/api/agents.ts", FULL_E2E_FETCH)),
        true,
        true,
    );
    let counts = counts(&graph);

    assert_eq!(report.route_nodes_added, 1);
    assert_eq!(report.handles_route_edges_added, 1);
    assert_eq!(report.fetches_edges_added, 1);
    assert_eq!(report.unmatched_fetch_count, 0);
    assert_eq!(counts.route_nodes, 1);

    let [handles] = counts.handles.as_slice() else {
        panic!(
            "expected exactly one HandlesRoute edge, got {:?}",
            counts.handles
        );
    };
    assert_eq!(handles.confidence, 0.90);
    assert_eq!(handles.reason.as_deref(), Some("axum-router-new"));

    let [fetches] = counts.fetches.as_slice() else {
        panic!(
            "expected exactly one Fetches edge, got {:?}",
            counts.fetches
        );
    };
    assert_eq!(fetches.confidence, 0.70);
    assert_eq!(fetches.reason.as_deref(), Some("ts-fetch-literal"));
}
