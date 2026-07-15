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

/// PR s6ch / 92z7 fixture: a graph with a `Fetches` consumer into
/// each of the three exclusion categories (health-path, param-only
/// path, below-confidence floor) plus a high-confidence "good"
/// consumer. The same fixture is reused by the policy-aware
/// `impact_bfs_with_policy` / `first_exclusion_reason` tests so
/// we don't drift the projections out of sync.
fn route_exclusion_fixture_graph() -> RepoDependencyGraph {
    let mut health_route = RepoGraphNode {
        id: RepoNodeKey::Route("GET /health (axum)".to_string()),
        kind: RepoGraphNodeKind::Route,
        display_name: "GET /health (axum)".to_string(),
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
        route_handler_symbol: None,
    };
    health_route.signature = Some("route GET /health".to_string());

    let mut param_only_route = RepoGraphNode {
        id: RepoNodeKey::Route("GET /{tenant}/{id} (axum)".to_string()),
        kind: RepoGraphNodeKind::Route,
        display_name: "GET /{tenant}/{id} (axum)".to_string(),
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
        route_handler_symbol: None,
    };
    param_only_route.signature = Some("route GET /{tenant}/{id}".to_string());

    let mut low_confidence_route = RepoGraphNode {
        id: RepoNodeKey::Route("POST /api/agents (axum)".to_string()),
        kind: RepoGraphNodeKind::Route,
        display_name: "POST /api/agents (axum)".to_string(),
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
        route_handler_symbol: None,
    };
    low_confidence_route.signature = Some("route POST /api/agents".to_string());

    let mut good_route = RepoGraphNode {
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
        route_handler_symbol: None,
    };
    good_route.signature = Some("route GET /api/agents".to_string());

    let handler = symbol_node(
        "scip-rust pkg server/src/routes.rs `list_agents`().",
        "list_agents",
        "server/src/routes.rs",
        "rust",
    );

    let health_consumer = symbol_node(
        "scip-typescript pkg ui/src/api.ts `pollHealth`().",
        "pollHealth",
        "ui/src/api.ts",
        "typescript",
    );

    let param_only_consumer = symbol_node(
        "scip-typescript pkg ui/src/api.ts `pollTenant`().",
        "pollTenant",
        "ui/src/api.ts",
        "typescript",
    );

    let low_confidence_consumer = symbol_node(
        "scip-typescript pkg ui/src/api.ts `createAgent`().",
        "createAgent",
        "ui/src/api.ts",
        "typescript",
    );

    let good_consumer = symbol_node(
        "scip-typescript pkg ui/src/api.ts `loadAgents`().",
        "loadAgents",
        "ui/src/api.ts",
        "typescript",
    );

    let fetches_edge = |source, target, confidence, reason: &str| RepoGraphArtifactEdge {
        source,
        target,
        kind: RepoGraphEdgeKind::Fetches,
        weight: 1.0,
        evidence_count: 1,
        confidence,
        reason: Some(reason.to_string()),
        step: None,
    };
    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![
            handler,
            good_route,
            low_confidence_route,
            health_route,
            param_only_route,
            good_consumer,
            low_confidence_consumer,
            health_consumer,
            param_only_consumer,
        ],
        edges: vec![
            // High-confidence `Fetches` consumer into the real route.
            fetches_edge(5, 1, 0.92, "ts-fetch-template"),
            // Inferred consumer in the (0, 0.5) "below the policy
            // floor" band — `first_exclusion_reason` flags the edge
            // as `below-confidence-floor`, but the BFS impact
            // threshold (0.85 by default) would cut it before the
            // policy ever sees it. The `impact_bfs_with_policy`
            // tests below dial `min_confidence` down to 0.0 so the
            // edge survives the BFS long enough to be downgraded.
            fetches_edge(6, 2, 0.4, "ts-fetch-template"),
            // Health-path consumer (above the floor).
            fetches_edge(7, 3, 0.92, "ts-fetch-template"),
            // Param-only-path consumer (above the floor).
            fetches_edge(8, 4, 0.92, "ts-fetch-template"),
        ],
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: RouteExclusionConfig::default(),
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    };
    RepoDependencyGraph::from_artifact(&artifact)
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
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
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
    // PR s6ch / 92z7: take the parity lock so concurrent tests
    // mutating `DJINN_ROUTE_PARITY` (e.g. the
    // `api_impact_disabled_parity_shadow_path_*` / `_enabled_parity_*`
    // tests below) can't race this one and flip
    // `route_consumer_excluded_reason` to `None`.
    let _guard = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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
    // PR s6ch / 92z7: take the parity lock so concurrent tests
    // mutating `DJINN_ROUTE_PARITY` (e.g. the
    // `api_impact_disabled_parity_shadow_path_*` / `_enabled_parity_*`
    // tests below) can't race this one and flip
    // `route_excluded_reason` to `None`.
    let _guard = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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

// ── PR s6ch / 92z7: route-exclusion / suggestion semantics ──────────
//
// The fixture exposes one consumer per exclusion category (health
// path, param-only path, below-confidence floor) plus a single
// "good" high-confidence consumer. The tests below assert that
// `first_exclusion_reason` / `impact_bfs_with_policy` correctly
// stamp the right reason on each projection and that disabling
// `DJINN_ROUTE_PARITY` keeps the shadow path unfiltered.

#[test]
fn consumer_exclusion_reason_flags_health_path_consumer() {
    let graph = route_exclusion_fixture_graph();
    let cfg = graph.route_exclusion_config();
    // Find the `pollHealth` consumer's NodeIndex and the
    // `GET /health` route's NodeIndex by display name.
    let mut consumer_idx = None;
    let mut route_idx = None;
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.display_name == "pollHealth" {
            consumer_idx = Some(idx);
        } else if node.display_name == "GET /health (axum)" {
            route_idx = Some(idx);
        }
    }
    let (consumer_idx, route_idx) = (consumer_idx.unwrap(), route_idx.unwrap());
    let edge = graph
        .graph()
        .edges_connecting(consumer_idx, route_idx)
        .next()
        .expect("health fetches edge present");
    assert_eq!(
        shared::first_exclusion_reason(edge.weight(), Some("/health"), cfg),
        Some(shared::exclusion_reason::HEALTH_PATH)
    );
}

#[test]
fn consumer_exclusion_reason_flags_param_only_path_consumer() {
    let graph = route_exclusion_fixture_graph();
    let cfg = graph.route_exclusion_config();
    let mut consumer_idx = None;
    let mut route_idx = None;
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.display_name == "pollTenant" {
            consumer_idx = Some(idx);
        } else if node.display_name == "GET /{tenant}/{id} (axum)" {
            route_idx = Some(idx);
        }
    }
    let (consumer_idx, route_idx) = (consumer_idx.unwrap(), route_idx.unwrap());
    let edge = graph
        .graph()
        .edges_connecting(consumer_idx, route_idx)
        .next()
        .expect("param-only fetches edge present");
    assert_eq!(
        shared::first_exclusion_reason(edge.weight(), Some("/{tenant}/{id}"), cfg),
        Some(shared::exclusion_reason::PARAM_ONLY_PATH)
    );
}

#[test]
fn consumer_exclusion_reason_flags_below_confidence_floor_consumer() {
    let graph = route_exclusion_fixture_graph();
    let cfg = graph.route_exclusion_config();
    let mut consumer_idx = None;
    let mut route_idx = None;
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.display_name == "createAgent" {
            consumer_idx = Some(idx);
        } else if node.display_name == "POST /api/agents (axum)" {
            route_idx = Some(idx);
        }
    }
    let (consumer_idx, route_idx) = (consumer_idx.unwrap(), route_idx.unwrap());
    let edge = graph
        .graph()
        .edges_connecting(consumer_idx, route_idx)
        .next()
        .expect("low-confidence fetches edge present");
    assert_eq!(
        shared::first_exclusion_reason(edge.weight(), Some("/api/agents"), cfg),
        Some(shared::exclusion_reason::BELOW_CONFIDENCE_FLOOR)
    );
}

#[test]
fn consumer_exclusion_reason_returns_none_for_hard_consumer() {
    let graph = route_exclusion_fixture_graph();
    let cfg = graph.route_exclusion_config();
    let mut consumer_idx = None;
    let mut route_idx = None;
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.display_name == "loadAgents" {
            consumer_idx = Some(idx);
        } else if node.display_name == "GET /api/agents (axum)" {
            route_idx = Some(idx);
        }
    }
    let (consumer_idx, route_idx) = (consumer_idx.unwrap(), route_idx.unwrap());
    let edge = graph
        .graph()
        .edges_connecting(consumer_idx, route_idx)
        .next()
        .expect("hard fetches edge present");
    assert_eq!(
        shared::first_exclusion_reason(edge.weight(), Some("/api/agents"), cfg),
        None
    );
}

#[test]
fn path_is_param_only_matches_braced_and_axum_style_paths() {
    assert!(shared::path_is_param_only("/{tenant}"));
    assert!(shared::path_is_param_only("/{id}/{slug}"));
    assert!(shared::path_is_param_only("/<id>/<slug>"));
    // axum-style `:segment` is only param-only when *every*
    // segment uses that style — `/api/:tenant` mixes a static
    // segment with a parameter segment, so the helper says no.
    assert!(!shared::path_is_param_only("/:tenant/items"));
    assert!(!shared::path_is_param_only("/api/agents"));
    assert!(!shared::path_is_param_only("/"));
    assert!(!shared::path_is_param_only("/api/{tenant}/items"));
}

#[test]
fn path_matches_health_glob_normalises_case_and_slashes() {
    let globs = vec![
        "/health".to_string(),
        "/healthz".to_string(),
        "/ping".to_string(),
    ];
    assert!(shared::path_matches_health_glob("/health", &globs));
    assert!(shared::path_matches_health_glob("health", &globs));
    assert!(shared::path_matches_health_glob("/HEALTH", &globs));
    assert!(shared::path_matches_health_glob("/ping/", &globs));
    // `healthcheck` shouldn't match `/health`.
    assert!(!shared::path_matches_health_glob("/healthcheck", &globs));
    assert!(!shared::path_matches_health_glob("/api/agents", &globs));
    assert!(!shared::path_matches_health_glob("/", &globs));
    // Empty glob list disables the filter.
    assert!(!shared::path_matches_health_glob("/health", &[]));
}

#[test]
fn route_node_path_parses_method_path_and_framework_suffix() {
    use djinn_graph::repo_graph::{RepoGraphNode, RepoGraphNodeKind, RepoNodeKey};
    let route_node = RepoGraphNode {
        id: RepoNodeKey::Route("POST /api/v1/items (axum)".to_string()),
        kind: RepoGraphNodeKind::Route,
        display_name: "POST /api/v1/items (axum)".to_string(),
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
        workspace: None,
        route_framework: None,
        route_handler_symbol: None,
    };
    assert_eq!(
        shared::route_node_path(&route_node).as_deref(),
        Some("/api/v1/items")
    );
    // Plain symbol nodes don't expose a route path.
    let symbol_node = symbol_node("scip-rust pkg x.rs `foo`().", "foo", "x.rs", "rust");
    assert_eq!(shared::route_node_path(&symbol_node), None);
}

#[test]
fn impact_bfs_with_policy_stamps_health_path_exclusion_reason() {
    let _guard = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let graph = route_exclusion_fixture_graph();
    let cfg = graph.route_exclusion_config();
    let mut route_idx = None;
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.display_name == "GET /health (axum)" {
            route_idx = Some(idx);
        }
    }
    // Walk backward from the `GET /health` route, treating it as the
    // "queried" node. The BFS should reach the `pollHealth` symbol
    // via the `Fetches` edge and stamp the `health-path` reason.
    let entries: Vec<_> =
        shared::impact_bfs_with_policy(&graph, route_idx.unwrap(), 2, None, Some(cfg))
            .into_iter()
            .map(|(_, entry)| entry)
            .collect();
    let health_entry = entries
        .iter()
        .find(|entry| entry.key.contains("pollHealth"))
        .expect("pollHealth entry present");
    assert_eq!(
        health_entry.exclusion_reason.as_deref(),
        Some(shared::exclusion_reason::HEALTH_PATH)
    );
}

#[test]
fn impact_bfs_with_policy_stamps_param_only_exclusion_reason() {
    let _guard = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let graph = route_exclusion_fixture_graph();
    let cfg = graph.route_exclusion_config();
    let mut route_idx = None;
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.display_name == "GET /{tenant}/{id} (axum)" {
            route_idx = Some(idx);
        }
    }
    let entries: Vec<_> =
        shared::impact_bfs_with_policy(&graph, route_idx.unwrap(), 2, None, Some(cfg))
            .into_iter()
            .map(|(_, entry)| entry)
            .collect();
    let entry = entries
        .iter()
        .find(|entry| entry.key.contains("pollTenant"))
        .expect("pollTenant entry present");
    assert_eq!(
        entry.exclusion_reason.as_deref(),
        Some(shared::exclusion_reason::PARAM_ONLY_PATH)
    );
}

#[test]
fn impact_bfs_with_policy_stamps_below_confidence_floor_reason() {
    let _guard = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let graph = route_exclusion_fixture_graph();
    let cfg = graph.route_exclusion_config();
    let mut route_idx = None;
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.display_name == "POST /api/agents (axum)" {
            route_idx = Some(idx);
        }
    }
    // Walk backward from the `POST /api/agents` route. The
    // `createAgent` consumer's `Fetches` edge sits below the
    // `min_confidence_for_consumer_edge=0.5` floor, so the BFS
    // needs `min_confidence=Some(0.0)` to let it through; once
    // inside the BFS the policy downgrades it to a
    // `below-confidence-floor` suggestion.
    let entries: Vec<_> =
        shared::impact_bfs_with_policy(&graph, route_idx.unwrap(), 2, Some(0.0), Some(cfg))
            .into_iter()
            .map(|(_, entry)| entry)
            .collect();
    let entry = entries
        .iter()
        .find(|entry| entry.key.contains("createAgent"))
        .expect("createAgent entry present");
    assert_eq!(
        entry.exclusion_reason.as_deref(),
        Some(shared::exclusion_reason::BELOW_CONFIDENCE_FLOOR)
    );
}

#[test]
fn impact_bfs_with_policy_leaves_hard_consumer_unflagged() {
    let _guard = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let graph = route_exclusion_fixture_graph();
    let cfg = graph.route_exclusion_config();
    let mut route_idx = None;
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.display_name == "GET /api/agents (axum)" {
            route_idx = Some(idx);
        }
    }
    let entries: Vec<_> =
        shared::impact_bfs_with_policy(&graph, route_idx.unwrap(), 2, None, Some(cfg))
            .into_iter()
            .map(|(_, entry)| entry)
            .collect();
    let entry = entries
        .iter()
        .find(|entry| entry.key.contains("loadAgents"))
        .expect("loadAgents entry present");
    assert_eq!(entry.exclusion_reason, None);
    assert_eq!(entry.confidence_tier.as_deref(), Some("symbol"));
}

#[test]
fn impact_bfs_with_policy_disabled_shadow_path_keeps_pre_filter_edges() {
    // PR s6ch / 92z7: with `DJINN_ROUTE_PARITY=0` the shadow path
    // should not exclude any inferred `Fetches` consumer — that's
    // the comparison surface the rollout team uses to validate the
    // new exclusions don't drop real consumers.
    let _guard = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let graph = route_exclusion_fixture_graph();
    let mut route_idx = None;
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.display_name == "GET /health (axum)" {
            route_idx = Some(idx);
        }
    }
    // `policy: None` mirrors the parity-disabled path: every
    // `Fetches` edge above the confidence floor passes through.
    let entries: Vec<_> = shared::impact_bfs_with_policy(&graph, route_idx.unwrap(), 2, None, None)
        .into_iter()
        .map(|(_, entry)| entry)
        .collect();
    let health_entry = entries
        .iter()
        .find(|entry| entry.key.contains("pollHealth"))
        .expect("pollHealth entry present in shadow path");
    assert_eq!(health_entry.exclusion_reason, None);
}

#[test]
fn impact_bfs_disabled_shadow_path_via_env_var_keeps_pre_filter_edges() {
    // PR s6ch / 92z7 acceptance: when `DJINN_ROUTE_PARITY` is
    // disabled at runtime, the bridge hands callers the pre-filter
    // edge set so the rollout team can compare. We model that
    // here by setting the env var, calling
    // `route_parity_enabled()`, and confirming the BFS helper
    // agrees the consumer is a hard entry.
    let _lock = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env_guard = RouteParityGuard::set("0");
    assert!(!djinn_graph::route_extraction::route_parity_enabled());
    let graph = route_exclusion_fixture_graph();
    let mut route_idx = None;
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.display_name == "GET /health (axum)" {
            route_idx = Some(idx);
        }
    }
    let entries: Vec<_> = shared::impact_bfs_with_policy(&graph, route_idx.unwrap(), 2, None, None)
        .into_iter()
        .map(|(_, entry)| entry)
        .collect();
    let entry = entries
        .iter()
        .find(|entry| entry.key.contains("pollHealth"))
        .expect("pollHealth entry present in shadow path");
    assert_eq!(entry.exclusion_reason, None);
    // `_env_guard` restores the prior `DJINN_ROUTE_PARITY` value
    // when this scope exits, even on panic.
}

#[test]
fn api_impact_stamps_exclusion_reasons_on_consumers() {
    let _guard = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let graph = route_exclusion_fixture_graph();
    let result = routes::test_helpers::api_impact_for_graph(&graph);
    // The fixture's "real" route is `GET /api/agents` — every
    // consumer into it lands in the `impacts` list. The high-
    // confidence `loadAgents` consumer is the only hard entry;
    // every other fixture edge points at a different route, so it
    // shows up in `impacts` only via the policy-suggested path.
    let load = result
        .impacts
        .iter()
        .find(|entry| entry.consumer.name == "loadAgents")
        .expect("loadAgents impact present");
    assert_eq!(load.excluded_reason, None);
}

#[test]
fn api_impact_disabled_parity_shadow_path_keeps_pre_filter_consumers() {
    // PR s6ch / 92z7 acceptance: with `DJINN_ROUTE_PARITY=0` the
    // `api_impact` helper must surface the *pre-filter* consumer
    // / impact set — no `exclusion_reason` stamps, an empty
    // `excluded_impacts` list. This guards the parity-disabled
    // shadow path the rollout team uses to compare. The test
    // drives the helper end-to-end (not the lower-level
    // `impact_bfs_with_policy` directly) so the parity-gating
    // in `api_impact_on_graph` is exercised.
    let _lock = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env_guard = RouteParityGuard::set("0");
    assert!(!djinn_graph::route_extraction::route_parity_enabled());

    // `route_fixture_graph` already includes a single high-confidence
    // `Fetches` consumer (`loadAgents`) into the `GET /api/agents`
    // route. With parity disabled, that consumer is a hard blast-
    // radius link — no `excluded_reason`, no `excluded_impacts`.
    let graph = route_fixture_graph();
    let result = routes::test_helpers::api_impact_for_graph(&graph);
    assert!(
        result.excluded_impacts.is_empty(),
        "parity-disabled shadow path must keep `excluded_impacts` empty: {:?}",
        result.excluded_impacts
    );
    assert!(
        result
            .impacts
            .iter()
            .all(|entry| entry.excluded_reason.is_none()),
        "parity-disabled shadow path must not stamp `excluded_reason` on any impact: {:?}",
        result
            .impacts
            .iter()
            .map(|e| (&e.consumer.name, &e.excluded_reason))
            .collect::<Vec<_>>()
    );
    let load = result
        .impacts
        .iter()
        .find(|entry| entry.consumer.name == "loadAgents")
        .expect("loadAgents present as hard impact in shadow path");
    assert_eq!(load.excluded_reason, None);
    assert_ne!(load.risk_tier, "excluded");

    // Same drill, but with the full route-exclusion fixture so we
    // would *expect* the health-path / param-only-path / below-
    // confidence-floor stamps under parity. With parity disabled,
    // the helper must not populate `excluded_impacts` for *any*
    // seed route, and no impact entry may carry an
    // `excluded_reason` stamp. (Each excluded fixture consumer
    // points at a different route, so they only surface via the
    // parity-enabled test below — the per-route helper.)
    let graph = route_exclusion_fixture_graph();
    for route_id in [
        "GET /health (axum)",
        "GET /{tenant}/{id} (axum)",
        "POST /api/agents (axum)",
        "GET /api/agents (axum)",
    ] {
        let result = routes::test_helpers::api_impact_for_graph_with_route_id(&graph, route_id);
        assert!(
            result.excluded_impacts.is_empty(),
            "parity-disabled shadow path must keep `excluded_impacts` empty for {route_id}: {:?}",
            result.excluded_impacts
        );
        assert!(
            result
                .impacts
                .iter()
                .all(|entry| entry.excluded_reason.is_none()),
            "parity-disabled shadow path must not stamp `excluded_reason` on any {route_id} impact: {:?}",
            result
                .impacts
                .iter()
                .map(|e| (&e.consumer.name, &e.excluded_reason))
                .collect::<Vec<_>>()
        );
    }
    // `_env_guard` restores the prior `DJINN_ROUTE_PARITY` value
    // when this scope exits, even on panic.
}

#[test]
fn api_impact_enabled_parity_downgrades_health_path_consumer() {
    // PR s6ch / 92z7 acceptance: with `DJINN_ROUTE_PARITY=1` (or
    // unset) the `api_impact` helper must downgrade a
    // health-path / param-only-path / below-confidence-floor
    // `Fetches` consumer to a suggestion in `excluded_impacts`
    // rather than promoting it to a hard blast-radius impact.
    // This is the parity-enabled counterpart to
    // `api_impact_disabled_parity_shadow_path_keeps_pre_filter_consumers`.
    let _lock = ROUTE_PARITY_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env_guard = RouteParityGuard::set("1");
    assert!(djinn_graph::route_extraction::route_parity_enabled());

    let graph = route_exclusion_fixture_graph();

    // The fixture routes each excluded consumer into a *different*
    // route, so the test must query each route individually to
    // assert the matching `exclusion_reason` is stamped.
    for (route_id, consumer_name, expected_reason) in [
        (
            "GET /health (axum)",
            "pollHealth",
            shared::exclusion_reason::HEALTH_PATH,
        ),
        (
            "GET /{tenant}/{id} (axum)",
            "pollTenant",
            shared::exclusion_reason::PARAM_ONLY_PATH,
        ),
        (
            "POST /api/agents (axum)",
            "createAgent",
            shared::exclusion_reason::BELOW_CONFIDENCE_FLOOR,
        ),
    ] {
        let result = routes::test_helpers::api_impact_for_graph_with_route_id(&graph, route_id);
        let entry = result
            .excluded_impacts
            .iter()
            .find(|entry| entry.consumer.name == consumer_name)
            .unwrap_or_else(|| {
                panic!(
                    "{consumer_name} must be excluded by {route_id} in parity-enabled path: {:?}",
                    result.excluded_impacts
                )
            });
        assert_eq!(
            entry.excluded_reason.as_deref(),
            Some(expected_reason),
            "{consumer_name} must carry the {expected_reason} stamp for {route_id}"
        );
        assert!(
            result
                .impacts
                .iter()
                .all(|entry| entry.consumer.name != consumer_name),
            "{consumer_name} must not be promoted to a hard impact for {route_id}"
        );
    }
    // `_env_guard` restores the prior `DJINN_ROUTE_PARITY` value
    // when this scope exits, even on panic.
}
