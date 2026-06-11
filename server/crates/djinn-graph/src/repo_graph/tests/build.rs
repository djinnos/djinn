use super::*;

#[test]
fn builds_dependency_graph_with_file_and_symbol_metadata() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);

    // PR F2 bumped the count from 5 to 6: the entry-point detector
    // tags `main` as `EntryPointKind::Main`, and the process
    // detector traces a (short) call chain from it, materializing
    // one synthetic `Process` node. The five SCIP-derived nodes
    // (2 files + 3 symbols) are still all present; the extra node
    // is the synthetic process.
    assert_eq!(graph.node_count(), 6);
    assert!(graph.edge_count() >= 8);

    let app_file = graph
        .file_node("src/app.rs")
        .expect("app file node should exist");
    let app_node = graph.node(app_file);
    assert_eq!(app_node.kind, RepoGraphNodeKind::File);
    assert_eq!(app_node.language.as_deref(), Some("rust"));
    assert_eq!(app_node.file_path.as_deref(), Some(Path::new("src/app.rs")));
    assert_eq!(app_node.workspace.as_deref(), Some("root"));

    let helper_symbol = graph
        .symbol_node("scip-rust pkg src/helper.rs `helper`().")
        .expect("helper symbol node should exist");
    let helper_node = graph.node(helper_symbol);
    assert_eq!(helper_node.kind, RepoGraphNodeKind::Symbol);
    assert_eq!(helper_node.symbol_kind, Some(ScipSymbolKind::Function));
    assert_eq!(
        helper_node.file_path.as_deref(),
        Some(Path::new("src/helper.rs"))
    );
    assert_eq!(helper_node.workspace.as_deref(), Some("root"));

    let has_file_reference = graph.graph().edges(app_file).any(|edge| {
        edge.target() == helper_symbol && edge.weight().kind == RepoGraphEdgeKind::FileReference
    });
    assert!(has_file_reference, "expected file->symbol reference edge");
}

/// v10: the build stamps `is_test` on File nodes (and the symbols
/// defined in them) by the file-path convention, while leaving
/// production files/symbols unmarked.
#[test]
fn build_stamps_is_test_from_file_path_convention() {
    let test_symbol_name = "scip-rust pkg tests/login_test.rs `it_logs_in`().".to_string();
    let test_symbol = ScipSymbol {
        symbol: test_symbol_name.clone(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("it_logs_in".to_string()),
        signature: Some("fn it_logs_in()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let prod_symbol_name = "scip-rust pkg src/login.rs `login`().".to_string();
    let prod_symbol = ScipSymbol {
        symbol: prod_symbol_name.clone(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("login".to_string()),
        signature: Some("fn login()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo".to_string()),
            tool_name: Some("rust-analyzer".to_string()),
            tool_version: Some("1.0.0".to_string()),
        },
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("tests/login_test.rs"),
                definitions: vec![definition_occurrence(&test_symbol_name)],
                references: vec![],
                occurrences: vec![definition_occurrence(&test_symbol_name)],
                symbols: vec![test_symbol],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/login.rs"),
                definitions: vec![definition_occurrence(&prod_symbol_name)],
                references: vec![],
                occurrences: vec![definition_occurrence(&prod_symbol_name)],
                symbols: vec![prod_symbol],
            },
        ],
        external_symbols: vec![],
    };
    let graph = RepoDependencyGraph::build(&[index]);

    let test_file = graph
        .file_node("tests/login_test.rs")
        .expect("test file node should exist");
    assert!(
        graph.node(test_file).is_test,
        "file under tests/ must be marked is_test"
    );
    let test_sym = graph
        .symbol_node(&test_symbol_name)
        .expect("test symbol node should exist");
    assert!(
        graph.node(test_sym).is_test,
        "symbol defined in a test file must inherit is_test"
    );

    let prod_file = graph
        .file_node("src/login.rs")
        .expect("prod file node should exist");
    assert!(
        !graph.node(prod_file).is_test,
        "production file must not be marked is_test"
    );
    let prod_sym = graph
        .symbol_node(&prod_symbol_name)
        .expect("prod symbol node should exist");
    assert!(
        !graph.node(prod_sym).is_test,
        "production symbol must not be marked is_test"
    );
}
