use super::*;

fn route(
    graph: &mut RepoDependencyGraph,
    id: &str,
    source: Option<&str>,
    handler: Option<&str>,
) -> NodeIndex {
    graph.ensure_route_node(
        id,
        id,
        Some("rust"),
        Some("api"),
        source.map(Path::new),
        Some("axum"),
        handler,
    )
}

#[test]
fn route_dedup_is_scoped_to_source_file() {
    let mut graph = RepoDependencyGraph::build(&[]);
    let first = route(
        &mut graph,
        "GET /health (axum)",
        Some("src/routes_a.rs"),
        Some("handler_a"),
    );
    let same_file = route(
        &mut graph,
        "GET /health (axum)",
        Some("src/routes_a.rs"),
        Some("handler_a"),
    );
    let other_file = route(
        &mut graph,
        "GET /health (axum)",
        Some("src/routes_b.rs"),
        Some("handler_b"),
    );
    assert_eq!(first, same_file, "same route in same source may merge");
    assert_ne!(
        first, other_file,
        "routes from different source files must not merge"
    );
}

#[test]
fn low_entropy_same_file_routes_require_handler_discriminator() {
    let mut graph = RepoDependencyGraph::build(&[]);
    let first = route(&mut graph, "/", Some("src/routes.rs"), Some("index"));
    let second = route(&mut graph, "/", Some("src/routes.rs"), Some("fallback"));
    assert_ne!(
        first, second,
        "low-entropy same-label route sets should not collapse"
    );
}

#[test]
fn routes_without_handler_source_do_not_merge() {
    let mut graph = RepoDependencyGraph::build(&[]);
    let first = route(&mut graph, "GET /api/status (axum)", None, None);
    let second = route(&mut graph, "GET /api/status (axum)", None, None);
    assert_ne!(
        first, second,
        "route merges require a known handler source_file"
    );
}

#[test]
fn tool_dedup_is_scoped_to_source_file_and_workspace() {
    let mut graph = RepoDependencyGraph::build(&[]);
    let first = graph.ensure_tool_node(
        "agents.list",
        "agents.list",
        Some("rust"),
        Some("api"),
        Some(Path::new("src/tools/agents.rs")),
    );
    let same_source = graph.ensure_tool_node(
        "agents.list",
        "agents.list",
        Some("rust"),
        Some("api"),
        Some(Path::new("src/tools/agents.rs")),
    );
    let other_source = graph.ensure_tool_node(
        "agents.list",
        "agents.list",
        Some("rust"),
        Some("api"),
        Some(Path::new("src/tools/admin.rs")),
    );
    let other_workspace = graph.ensure_tool_node(
        "agents.list",
        "agents.list",
        Some("rust"),
        Some("worker"),
        Some(Path::new("src/tools/agents.rs")),
    );
    assert_eq!(
        first, same_source,
        "same tool in same source/workspace may merge"
    );
    assert_ne!(
        first, other_source,
        "tools from different source files must not merge"
    );
    assert_ne!(
        first, other_workspace,
        "tools from different workspaces/projects must never merge"
    );
}

#[test]
fn tools_without_workspace_do_not_merge_across_unknown_projects() {
    let mut graph = RepoDependencyGraph::build(&[]);
    let first = graph.ensure_tool_node(
        "agents.list",
        "agents.list",
        Some("rust"),
        None,
        Some(Path::new("src/tools/agents.rs")),
    );
    let second = graph.ensure_tool_node(
        "agents.list",
        "agents.list",
        Some("rust"),
        None,
        Some(Path::new("src/tools/agents.rs")),
    );
    assert_ne!(
        first, second,
        "tool merges require an explicit same-workspace/project scope"
    );
}

#[test]
fn tool_additions_use_ykcg_parity_allowlist_path() {
    let baseline = RepoDependencyGraph::build(&[fixture_index()]);
    let mut live = baseline.clone();
    live.ensure_tool_node(
        "agents.list",
        "agents.list",
        Some("rust"),
        Some("root"),
        Some(Path::new("src/tools/agents.rs")),
    );

    let allowlisted = crate::ykcg_parity::YkcgExtractorParityConfig::new(
        "tool-extractor",
        [RepoGraphNodeKind::Tool],
        [],
    );
    let report =
        crate::ykcg_parity::assert_ykcg_extractor_graph_parity(&baseline, &live, &allowlisted)
            .expect("explicitly allowlisted Tool additions should pass");

    assert!(report.passed);
    assert_eq!(
        report.allowed_added_nodes[&RepoGraphNodeKind::Tool].count,
        1
    );
    assert!(
        report.allowed_added_nodes[&RepoGraphNodeKind::Tool]
            .samples
            .iter()
            .any(|sample| sample.contains("agents.list")),
        "structured report should sample the allowed Tool addition: {report:#?}"
    );
    assert!(report.render_for_ci().contains("allowed added nodes"));

    let strict =
        crate::ykcg_parity::YkcgExtractorParityConfig::new("tool-extractor-strict", [], []);
    let err = crate::ykcg_parity::assert_ykcg_extractor_graph_parity(&baseline, &live, &strict)
        .expect_err("unallowlisted Tool additions must fail");
    let crate::ykcg_parity::YkcgExtractorParityError::Diff(report) = err;
    assert!(!report.passed);
    assert!(report.allowed_added_nodes.is_empty());
    let diff = report
        .failing_diff
        .expect("strict report should retain the diff");
    assert_eq!(diff.nodes.added_counts_by_kind[&RepoGraphNodeKind::Tool], 1);
}

fn symbol_node(symbol: &str, display_name: &str) -> RepoGraphNode {
    RepoGraphNode {
        id: RepoNodeKey::Symbol(symbol.to_string()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: display_name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from("src/routes.rs")),
        symbol: Some(symbol.to_string()),
        symbol_kind: Some(ScipSymbolKind::Function),
        is_external: false,
        visibility: None,
        signature: None,
        documentation: Vec::new(),
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some("root".to_string()),
        route_framework: None,
        route_handler_symbol: None,
    }
}

#[test]
fn route_edge_helpers_stamp_reason_and_clamped_confidence() {
    let mut graph = RepoDependencyGraph::build(&[]);
    let route = graph.ensure_route_node(
        "GET /api/agents (axum)",
        "GET /api/agents",
        Some("rust"),
        Some("root"),
        Some(Path::new("src/routes.rs")),
        Some("axum"),
        Some("scip-rust pkg handlers `list_agents`()."),
    );
    let handler = graph.graph_mut_unchecked().add_node(symbol_node(
        "scip-rust pkg handlers `list_agents`().",
        "list_agents",
    ));
    let caller = graph
        .graph_mut_unchecked()
        .add_node(symbol_node("scip-ts pkg ui `loadAgents`().", "loadAgents"));
    graph.graph_mut_unchecked()[caller].language = Some("typescript".to_string());

    graph.add_handles_route_edge(route, handler, "axum-router-new", Some(1.25));
    graph.add_fetches_edge(caller, route, "ts-fetch-literal", None);

    let handles = graph
        .graph()
        .edges_connecting(route, handler)
        .find(|edge| edge.weight().kind == RepoGraphEdgeKind::HandlesRoute)
        .expect("handles-route edge");
    assert_eq!(handles.weight().reason.as_deref(), Some("axum-router-new"));
    assert_eq!(handles.weight().confidence, 1.0);
    assert_eq!(
        handles.weight().weight,
        edge_weight(RepoGraphEdgeKind::HandlesRoute)
    );

    let fetches = graph
        .graph()
        .edges_connecting(caller, route)
        .find(|edge| edge.weight().kind == RepoGraphEdgeKind::Fetches)
        .expect("fetches edge");
    assert_eq!(fetches.weight().reason.as_deref(), Some("ts-fetch-literal"));
    assert_eq!(
        fetches.weight().confidence,
        edge_confidence_floor(RepoGraphEdgeKind::Fetches)
    );
    assert_eq!(
        fetches.weight().weight,
        edge_weight(RepoGraphEdgeKind::Fetches)
    );
    let language_chain = graph
        .route_edge_language_chain(caller, route, RepoGraphEdgeKind::Fetches)
        .expect("route edge language chain");
    assert_eq!(
        language_chain.source_language.as_deref(),
        Some("typescript")
    );
    assert_eq!(language_chain.target_language.as_deref(), Some("rust"));
    assert!(language_chain.is_cross_language);
    assert!(graph.is_cross_language_route_edge(caller, route, RepoGraphEdgeKind::Fetches));
}
