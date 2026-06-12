use super::*;
use djinn_graph::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphEdgeKind,
    RepoGraphNode, RepoGraphNodeKind, RouteExclusionConfig,
};
use djinn_graph::scip_parser::{ScipSymbolKind, ScipVisibility};

fn symbol_node(symbol: &str, name: &str, file: &str, language: &str) -> RepoGraphNode {
    RepoGraphNode {
        id: RepoNodeKey::Symbol(symbol.to_string()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some(language.to_string()),
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

fn route_fixture_graph() -> RepoDependencyGraph {
    let mut route = RepoGraphNode {
        id: RepoNodeKey::Route("GET /api/agents (axum)".to_string()),
        kind: RepoGraphNodeKind::Route,
        display_name: "GET /api/agents (axum)".to_string(),
        language: Some("rust".to_string()),
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
        route_framework: Some("axum".to_string()),
        route_handler_symbol: Some(
            "scip-rust pkg server/src/routes.rs `list_agents`().".to_string(),
        ),
    };
    route.signature = Some("route GET /api/agents".to_string());

    let mut handler = symbol_node(
        "scip-rust pkg server/src/routes.rs `list_agents`().",
        "list_agents",
        "server/src/routes.rs",
        "rust",
    );
    handler.signature =
        Some("async fn list_agents() -> Json<{ id: string, name: string }>".to_string());
    handler.documentation = vec!["response_shape: { id: string, name: string }".to_string()];

    let mut consumer = symbol_node(
        "scip-typescript pkg ui/src/api.ts `loadAgents`().",
        "loadAgents",
        "ui/src/api.ts",
        "typescript",
    );
    consumer.documentation =
        vec!["fetches /api/agents; uses response: { id: number, missing: string }".to_string()];

    let middleware = symbol_node(
        "scip-rust pkg server/src/middleware.rs `auth`().",
        "auth",
        "server/src/middleware.rs",
        "rust",
    );

    let edge = |source, target, kind| RepoGraphArtifactEdge {
        source,
        target,
        kind,
        weight: 1.0,
        evidence_count: 1,
        confidence: 0.95,
        reason: None,
        step: None,
    };
    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![route, handler, consumer, middleware],
        edges: vec![
            edge(0, 1, RepoGraphEdgeKind::HandlesRoute),
            edge(2, 0, RepoGraphEdgeKind::Fetches),
            edge(3, 1, RepoGraphEdgeKind::EntryPointOf),
        ],
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: Default::default(),
    };
    RepoDependencyGraph::from_artifact(&artifact)
}

fn multi_route_fixture_graph() -> RepoDependencyGraph {
    let mut artifact = route_fixture_graph().to_artifact();
    artifact.nodes.push(RepoGraphNode {
        id: RepoNodeKey::Route("POST /api/tasks (actix-web)".to_string()),
        kind: RepoGraphNodeKind::Route,
        display_name: "POST /api/tasks (actix-web)".to_string(),
        language: Some("rust".to_string()),
        file_path: None,
        symbol: None,
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: Some("route POST /api/tasks".to_string()),
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some("root".to_string()),
        route_framework: Some("actix-web".to_string()),
        route_handler_symbol: None,
    });
    artifact.nodes.push(RepoGraphNode {
        id: RepoNodeKey::Route("GET /health (axum)".to_string()),
        kind: RepoGraphNodeKind::Route,
        display_name: "GET /health (axum)".to_string(),
        language: Some("rust".to_string()),
        file_path: None,
        symbol: None,
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: Some("route GET /health".to_string()),
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some("root".to_string()),
        route_framework: Some("axum".to_string()),
        route_handler_symbol: None,
    });
    RepoDependencyGraph::from_artifact(&artifact)
}

#[test]
fn route_map_returns_handler_consumers_middleware_and_summary() {
    let graph = route_fixture_graph();
    let result = routes::test_helpers::route_map_for_graph(&graph);
    assert_eq!(result.routes.len(), 1);
    let entry = &result.routes[0];
    assert_eq!(entry.route.method.as_deref(), Some("GET"));
    assert_eq!(entry.route.path.as_deref(), Some("/api/agents"));
    assert_eq!(
        entry.handler.as_ref().map(|h| h.name.as_str()),
        Some("list_agents")
    );
    assert_eq!(entry.consumers[0].name, "loadAgents");
    assert_eq!(entry.consumers[0].confidence_tier, "inferred");
    assert!(entry.excluded_reason.is_none());
    let chain = entry.consumers[0]
        .route_language_chain
        .as_ref()
        .expect("route-map consumer includes route language chain");
    assert_eq!(chain.source_language.as_deref(), Some("typescript"));
    assert_eq!(chain.target_language.as_deref(), Some("rust"));
    assert!(chain.is_cross_language);
    assert_eq!(entry.middleware[0].name, "auth");
    assert_eq!(result.summary.total_routes, 1);
    assert_eq!(result.summary.framework_counts.get("axum"), Some(&1));
}

#[test]
fn route_map_resolves_seed_filters_and_applies_limit_deterministically() {
    let graph = multi_route_fixture_graph();

    let by_id = routes::test_helpers::route_map_for_graph_with_filters(
        &graph,
        Some("GET /api/agents (axum)"),
        None,
        None,
        None,
        None,
        20,
    );
    assert_eq!(by_id.routes.len(), 1);
    assert_eq!(by_id.routes[0].route.path.as_deref(), Some("/api/agents"));

    let by_method_path_glob = routes::test_helpers::route_map_for_graph_with_filters(
        &graph,
        None,
        Some("get"),
        None,
        Some("/api/*"),
        None,
        20,
    );
    assert_eq!(by_method_path_glob.routes.len(), 1);
    assert_eq!(
        by_method_path_glob.routes[0].route.id,
        "GET /api/agents (axum)"
    );

    let by_framework_limited = routes::test_helpers::route_map_for_graph_with_filters(
        &graph,
        None,
        None,
        None,
        None,
        Some("axum"),
        1,
    );
    assert_eq!(by_framework_limited.routes.len(), 1);
    assert_eq!(
        by_framework_limited.routes[0].route.id, "GET /api/agents (axum)",
        "limit ordering follows stable route node keys"
    );
}

#[test]
fn route_map_no_match_returns_empty_routes_with_summary() {
    let graph = route_fixture_graph();
    let result = routes::test_helpers::route_map_for_graph_with_filters(
        &graph,
        None,
        Some("POST"),
        None,
        Some("/api/missing*"),
        Some("axum"),
        20,
    );

    assert!(result.routes.is_empty());
    assert_eq!(result.summary.total_routes, 1);
    assert_eq!(result.summary.framework_counts.get("axum"), Some(&1));
    assert_eq!(result.summary.handler_counts.get("list_agents"), Some(&1));
}

#[test]
fn shape_check_detects_missing_and_extra_response_keys() {
    let graph = route_fixture_graph();
    let result = routes::test_helpers::shape_check_for_graph(&graph);
    assert_eq!(
        result.route_shape.route.path.as_deref(),
        Some("/api/agents")
    );
    assert!(
        result
            .route_shape
            .response_fields
            .iter()
            .any(|field| field.name == "id")
    );
    assert_eq!(result.drifts.len(), 1);
    let drift = &result.drifts[0];
    assert!(drift.missing_keys.iter().any(|k| k == "missing"));
    assert!(drift.extra_keys.iter().any(|k| k == "name"));
    assert!(
        drift
            .type_mismatches
            .iter()
            .any(|m| { m.key == "id" && m.server_type == "string" && m.consumer_type == "number" })
    );

    let by_method_path = routes::test_helpers::shape_check_for_graph_with_route(
        &graph,
        None,
        Some("get"),
        Some("/api/agents"),
    );
    assert_eq!(
        by_method_path.route_shape.route.id,
        result.route_shape.route.id
    );
    assert_eq!(by_method_path.drifts.len(), 1);
}

#[test]
fn below_floor_fetches_are_audit_only_for_shape_and_api_impact() {
    let graph = route_fixture_graph();
    let mut artifact = graph.to_artifact();
    for edge in &mut artifact.edges {
        if edge.kind == RepoGraphEdgeKind::Fetches {
            edge.confidence = 0.2;
            edge.reason = Some("below-floor string-shape".to_string());
        }
    }
    let graph = RepoDependencyGraph::from_artifact(&artifact);

    let route_map = routes::test_helpers::route_map_for_graph(&graph);
    let consumer = &route_map.routes[0].consumers[0];
    assert_eq!(consumer.name, "loadAgents");
    assert_eq!(consumer.confidence, 0.2);
    assert_eq!(consumer.confidence_tier, "ambiguous");
    assert_eq!(
        consumer.confidence_reason.as_deref(),
        Some("below-floor string-shape")
    );
    assert_eq!(
        consumer.excluded_reason.as_deref(),
        Some("below-confidence-floor")
    );

    let shape = routes::test_helpers::shape_check_for_graph(&graph);
    assert!(shape.matched);
    assert!(
        shape.drifts.is_empty(),
        "below-floor consumers are excluded from shape drift"
    );

    let impact = routes::test_helpers::api_impact_for_graph(&graph);
    assert!(
        impact
            .impacts
            .iter()
            .all(|entry| entry.consumer.name != "loadAgents")
    );
    assert_eq!(impact.excluded_impacts.len(), 1);
    assert_eq!(
        impact.excluded_impacts[0].excluded_reason.as_deref(),
        Some("below-confidence-floor")
    );
}

#[test]
fn route_exclusions_mark_route_map_and_skip_shape_check() {
    let graph = route_fixture_graph();
    let mut artifact = graph.to_artifact();
    artifact.route_exclusion_config = RouteExclusionConfig {
        health_path_globs: vec!["/api/*".to_string()],
        ..RouteExclusionConfig::default()
    };
    let graph = RepoDependencyGraph::from_artifact(&artifact);

    let route_map = routes::test_helpers::route_map_for_graph(&graph);
    assert_eq!(
        route_map.routes[0].excluded_reason.as_deref(),
        Some("health-path")
    );

    let shape = routes::test_helpers::shape_check_for_graph(&graph);
    assert!(!shape.matched);
    assert_eq!(shape.excluded_reason.as_deref(), Some("health-path"));
    assert!(shape.summary.contains("excluded"));

    let impact = routes::test_helpers::api_impact_for_graph(&graph);
    assert!(impact.impacts.is_empty());
    assert_eq!(impact.excluded_impacts.len(), 1);
    assert_eq!(
        impact.excluded_impacts[0].excluded_reason.as_deref(),
        Some("health-path")
    );
}

#[test]
fn api_impact_prioritizes_shape_drift_and_empty_graphs_succeed() {
    let graph = route_fixture_graph();
    let result = routes::test_helpers::api_impact_for_graph(&graph);
    assert!(!result.impacts.is_empty());
    assert_eq!(result.impacts[0].consumer.name, "loadAgents");
    assert_eq!(result.impacts[0].risk_tier, "high");

    let empty = RepoDependencyGraph::build(&[]);
    assert!(
        routes::test_helpers::route_map_for_graph(&empty)
            .routes
            .is_empty()
    );
    assert!(
        routes::test_helpers::shape_check_for_graph(&empty)
            .drifts
            .is_empty()
    );
    assert!(
        routes::test_helpers::api_impact_for_graph(&empty)
            .impacts
            .is_empty()
    );
}
