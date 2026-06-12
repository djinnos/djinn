use std::collections::BTreeSet;
use std::path::Path;

use super::*;
use crate::repo_graph::{RepoDependencyGraph, RepoGraphEdge, RepoGraphEdgeKind, RepoGraphNodeKind};
use crate::scip_parser::{
    ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipSymbol, ScipSymbolKind,
    ScipSymbolRole, ScipVisibility,
};

const AXUM_ONLY_ROUTE: &str = include_str!("fixtures/axum_only/server/src/routes.rs");
const TS_ONLY_UNKNOWN: &str = include_str!("fixtures/ts_only_no_match/ui/src/api/unknown.ts");
const FULL_E2E_ROUTE: &str = include_str!("fixtures/full_e2e/server/src/routes.rs");
const FULL_E2E_FETCH: &str = include_str!("fixtures/full_e2e/ui/src/api/fixture.ts");

#[derive(Debug)]
struct RouteCounts {
    route_display_names: Vec<String>,
    handles: Vec<RepoGraphEdge>,
    fetches: Vec<RepoGraphEdge>,
}

fn assert_edge_metadata(edge: &RepoGraphEdge, confidence: f64, reason: &str) {
    assert_eq!(edge.confidence, confidence);
    assert_eq!(edge.reason.as_deref(), Some(reason));
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

fn definition_occurrence(symbol: &str, start_line: u32, end_line: u32) -> ScipOccurrence {
    ScipOccurrence {
        symbol: symbol.to_string(),
        range: ScipRange {
            start_line: start_line.saturating_sub(1) as i32,
            start_character: 0,
            end_line: start_line.saturating_sub(1) as i32,
            end_character: 1,
        },
        enclosing_range: Some(ScipRange {
            start_line: start_line.saturating_sub(1) as i32,
            start_character: 0,
            end_line: end_line.saturating_sub(1) as i32,
            end_character: 1,
        }),
        roles: BTreeSet::from([ScipSymbolRole::Definition]),
        syntax_kind: None,
        override_documentation: Vec::new(),
    }
}

fn reference_occurrence(symbol: &str) -> ScipOccurrence {
    ScipOccurrence {
        symbol: symbol.to_string(),
        range: ScipRange {
            start_line: 0,
            start_character: 0,
            end_line: 0,
            end_character: 1,
        },
        enclosing_range: None,
        roles: BTreeSet::from([ScipSymbolRole::Import]),
        syntax_kind: None,
        override_documentation: Vec::new(),
    }
}

fn scip_function_symbol(symbol: String, display_name: String) -> ScipSymbol {
    ScipSymbol {
        symbol,
        kind: Some(ScipSymbolKind::Function),
        display_name: Some(display_name),
        signature: None,
        documentation: Vec::new(),
        relationships: Vec::new(),
        visibility: Some(ScipVisibility::Public),
        signature_parts: None,
    }
}

fn function_names(source: &str) -> Vec<(String, u32)> {
    source
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let function_pos = line
                .find("fn ")
                .map(|pos| pos + "fn ".len())
                .or_else(|| line.find("function ").map(|pos| pos + "function ".len()))?;
            let name: String = line[function_pos..]
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            (!name.is_empty()).then_some((name, (idx + 1) as u32))
        })
        .collect()
}

fn rust_fixture_file(root: &Path, rel_path: &Path) -> Option<ScipFile> {
    let source = std::fs::read_to_string(root.join(rel_path)).ok()?;
    let end_line = source.lines().count().max(1) as u32;
    let mut symbols = Vec::new();
    let mut definitions = Vec::new();
    for (name, line) in function_names(&source) {
        let symbol = format!("test {}/{name}().", rel_path.display());
        symbols.push(scip_function_symbol(symbol.clone(), name));
        definitions.push(definition_occurrence(&symbol, line, end_line));
    }

    Some(ScipFile {
        language: "rust".to_string(),
        relative_path: rel_path.to_path_buf(),
        definitions,
        references: vec![reference_occurrence("axum::Router")],
        occurrences: Vec::new(),
        symbols,
    })
}

fn typescript_fixture_file(root: &Path, rel_path: &Path) -> Option<ScipFile> {
    let source = std::fs::read_to_string(root.join(rel_path)).ok()?;
    let end_line = source.lines().count().max(1) as u32;
    let mut symbols = Vec::new();
    let mut definitions = Vec::new();
    for (name, line) in function_names(&source) {
        let symbol = format!("ts {}/{name}().", rel_path.display());
        symbols.push(scip_function_symbol(symbol.clone(), name));
        definitions.push(definition_occurrence(&symbol, line, end_line));
    }

    Some(ScipFile {
        language: "typescript".to_string(),
        relative_path: rel_path.to_path_buf(),
        definitions,
        references: Vec::new(),
        occurrences: Vec::new(),
        symbols,
    })
}

fn source_backed_fixture_graph(root: &Path, ts_path: &Path) -> RepoDependencyGraph {
    let mut files = Vec::new();
    if let Some(file) = rust_fixture_file(root, Path::new("server/src/routes.rs")) {
        files.push(file);
    }
    if let Some(file) = typescript_fixture_file(root, ts_path) {
        files.push(file);
    }

    RepoDependencyGraph::build(&[ParsedScipIndex {
        workspace_slug: "route-e2e-fixture".to_string(),
        metadata: ScipMetadata::default(),
        files,
        external_symbols: vec![ScipSymbol {
            symbol: "axum::Router".to_string(),
            kind: Some(ScipSymbolKind::Type),
            display_name: Some("Router".to_string()),
            signature: None,
            documentation: Vec::new(),
            relationships: Vec::new(),
            visibility: Some(ScipVisibility::Public),
            signature_parts: None,
        }],
    }])
}

/// End-to-end route-extraction harness that exercises the same source-backed
/// canonical graph post-processing pass that `ensure_canonical_graph` invokes:
/// write a fixture project, build a file/symbol graph from those files, run
/// route extraction against the fixture project root, then inspect the
/// materialized canonical graph shape.
fn ensure_canonical_graph_route_extraction_fixture(
    axum_source: Option<&str>,
    ts_file: Option<(&str, &str)>,
) -> (RouteExtractionReport, RepoDependencyGraph) {
    let temp = tempfile::tempdir().expect("create route extraction e2e fixture dir");
    write_fixture(temp.path(), axum_source, ts_file);

    let ts_path = ts_file
        .map(|(relative_path, _source)| Path::new(relative_path))
        .unwrap_or_else(|| Path::new("ui/src/api/agents.ts"));
    let mut graph = source_backed_fixture_graph(temp.path(), ts_path);
    let report = detect_routes(&mut graph, temp.path());
    (report, graph)
}

fn counts(graph: &RepoDependencyGraph) -> RouteCounts {
    RouteCounts {
        route_display_names: graph
            .graph()
            .node_weights()
            .filter(|node| node.kind == RepoGraphNodeKind::Route)
            .map(|node| node.display_name.clone())
            .collect(),
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
        ensure_canonical_graph_route_extraction_fixture(Some(AXUM_ONLY_ROUTE), None);
    let counts = counts(&graph);

    assert!(report.skipped_files.is_empty());
    assert!(report.file_failures.is_empty());
    assert_eq!(report.route_nodes_added, 1);
    assert_eq!(report.handles_route_edges_added, 1);
    assert_eq!(report.fetches_edges_added, 0);
    assert_eq!(report.unmatched_fetch_count, 0);
    assert_eq!(counts.route_display_names, ["GET /api/fixture (axum)"]);
    let [handles] = counts.handles.as_slice() else {
        panic!(
            "expected exactly one HandlesRoute edge, got {:?}",
            counts.handles
        );
    };
    assert_edge_metadata(handles, 0.90, "axum-router-new");
    assert_eq!(counts.fetches.len(), 0);
}

#[test]
fn ts_only_no_match_records_unmatched_fetch_without_graph_pollution() {
    let (report, graph) = ensure_canonical_graph_route_extraction_fixture(
        None,
        Some(("ui/src/api/unknown.ts", TS_ONLY_UNKNOWN)),
    );
    let counts = counts(&graph);

    assert!(report.skipped_files.is_empty());
    assert!(report.file_failures.is_empty());
    assert_eq!(report.route_nodes_added, 0);
    assert_eq!(report.handles_route_edges_added, 0);
    assert_eq!(report.fetches_edges_added, 0);
    assert_eq!(report.unmatched_fetch_count, 1);
    assert!(counts.route_display_names.is_empty());
    assert_eq!(counts.handles.len(), 0);
    assert_eq!(counts.fetches.len(), 0);
}

#[test]
fn full_e2e_round_trips_to_route_handler_and_fetch_edges() {
    let (report, graph) = ensure_canonical_graph_route_extraction_fixture(
        Some(FULL_E2E_ROUTE),
        Some(("ui/src/api/fixture.ts", FULL_E2E_FETCH)),
    );
    let counts = counts(&graph);

    assert!(report.skipped_files.is_empty());
    assert!(report.file_failures.is_empty());
    assert_eq!(report.route_nodes_added, 1);
    assert_eq!(report.handles_route_edges_added, 1);
    assert_eq!(report.fetches_edges_added, 1);
    assert_eq!(report.unmatched_fetch_count, 0);
    assert_eq!(counts.route_display_names, ["GET /api/fixture (axum)"]);

    let [handles] = counts.handles.as_slice() else {
        panic!(
            "expected exactly one HandlesRoute edge, got {:?}",
            counts.handles
        );
    };
    assert_edge_metadata(handles, 0.90, "axum-router-new");

    let [fetches] = counts.fetches.as_slice() else {
        panic!(
            "expected exactly one Fetches edge, got {:?}",
            counts.fetches
        );
    };
    assert_edge_metadata(fetches, 0.70, "ts-fetch-literal");
}
