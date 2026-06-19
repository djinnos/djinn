// Tests for the bounded-memory `try_build_with_scip_files` and
// `try_build_with_scip_file_iter` entry points. These verify that
// the new file-iteration builder produces graphs structurally identical
// to the original `try_build_with_source` path on the same input.

use super::*;

/// `try_build_with_scip_files` must produce the same graph as
/// `try_build_with_source` on the same test fixture.
#[test]
fn scip_file_iter_parity_with_try_build_with_source() {
    let index = fixture_index();

    // Build using the existing in-memory path.
    let source_graph =
        RepoDependencyGraph::try_build_with_source(std::slice::from_ref(&index), None)
            .expect("try_build_with_source must succeed");

    // Build using the new bounded-memory file iterator path.
    let iter_graph = RepoDependencyGraph::try_build_with_scip_files(
        index.files.iter(),
        &index.workspace_slug,
        &index.external_symbols,
        None,
    )
    .expect("try_build_with_scip_files must succeed");

    // Both graphs must have the same structural shape.
    assert_eq!(
        source_graph.node_count(),
        iter_graph.node_count(),
        "node counts must match between source and file-iter builders"
    );
    assert_eq!(
        source_graph.edge_count(),
        iter_graph.edge_count(),
        "edge counts must match between source and file-iter builders"
    );

    // Use the graph_parity harness for a comprehensive structural
    // comparison (nodes, edges, edge kinds, weights, display names).
    crate::graph_parity::assert_graph_parity(&source_graph, &iter_graph)
        .expect("graph parity must hold between source and file-iter builders");
}

/// `try_build_with_scip_file_iter` (fallible variant) must produce the
/// same graph as `try_build_with_source` when given `Ok` items.
#[test]
fn scip_file_iter_fallible_parity_with_try_build_with_source() {
    let index = fixture_index();

    // Build using the existing in-memory path.
    let source_graph =
        RepoDependencyGraph::try_build_with_source(std::slice::from_ref(&index), None)
            .expect("try_build_with_source must succeed");

    // Build using the fallible file iterator path (all items are Ok).
    let iter_graph = RepoDependencyGraph::try_build_with_scip_file_iter(
        index.files.iter().map(Ok::<_, String>),
        &index.workspace_slug,
        &index.external_symbols,
        None,
    )
    .expect("try_build_with_scip_file_iter must succeed");

    assert_eq!(
        source_graph.node_count(),
        iter_graph.node_count(),
        "node counts must match"
    );
    assert_eq!(
        source_graph.edge_count(),
        iter_graph.edge_count(),
        "edge counts must match"
    );

    // Full parity check.
    crate::graph_parity::assert_graph_parity(&source_graph, &iter_graph)
        .expect("graph parity must hold between source and fallible file-iter builders");
}

/// `try_build_with_scip_file_iter` must propagate errors from the
/// iterator and produce a meaningful error message.
#[test]
fn scip_file_iter_fallible_propagates_errors() {
    let index = fixture_index();

    // Create an iterator that yields one Ok then an Err.
    let files_with_error: Vec<Result<&ScipFile, String>> = vec![
        Ok(&index.files[0]),
        Err("simulated I/O failure".to_string()),
    ];

    let result = RepoDependencyGraph::try_build_with_scip_file_iter(
        files_with_error.into_iter(),
        &index.workspace_slug,
        &index.external_symbols,
        None,
    );

    assert!(result.is_err(), "fallible iterator must propagate errors");
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("simulated I/O failure"),
        "error message must contain the original error text, got: {err_msg}"
    );
}

/// The new entry points must handle external symbols correctly,
/// creating external symbol nodes just like the source path.
#[test]
fn scip_file_iter_preserves_external_symbols() {
    let mut index = fixture_index();

    // Add an external symbol (one that is NOT defined in any file).
    let external_sym = ScipSymbol {
        symbol: "scip-rust pkg external/lib.rs `ExternalTrait`#".to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("ExternalTrait".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    index.external_symbols.push(external_sym.clone());

    // Build using the source path.
    let source_graph =
        RepoDependencyGraph::try_build_with_source(std::slice::from_ref(&index), None)
            .expect("try_build_with_source must succeed");

    // Build using the file iterator path.
    let iter_graph = RepoDependencyGraph::try_build_with_scip_files(
        index.files.iter(),
        &index.workspace_slug,
        &index.external_symbols,
        None,
    )
    .expect("try_build_with_scip_files must succeed");

    // The external symbol must exist in both graphs.
    assert!(
        source_graph.symbol_node(&external_sym.symbol).is_some(),
        "external symbol must exist in source graph"
    );
    assert!(
        iter_graph.symbol_node(&external_sym.symbol).is_some(),
        "external symbol must exist in iter graph"
    );

    // Full parity.
    crate::graph_parity::assert_graph_parity(&source_graph, &iter_graph)
        .expect("graph parity must hold with external symbols");
}
