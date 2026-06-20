    // ── crate_graph integration tests ────────────────────────────────────────
    //
    // Verify that the `crate_graph` op is dispatchable through the MCP
    // server handler and returns the expected crate-nodes + cross-crate
    // edges shape.

    /// Helper: build a `DjinnMcpServer` with a `WorkspaceFixtureOps`
    /// that returns non-empty crate graph data.
    fn crate_graph_fixture_server() -> DjinnMcpServer {
        let crate_graph_resp = CrateGraphResponse {
            crates: vec![
                CrateNodeEntry {
                    name: "djinn-db".to_string(),
                    manifest_path: "crates/djinn-db/Cargo.toml".to_string(),
                    loc: 4200,
                    node_count: 85,
                    fan_in: 3.0,
                    fan_out: 1.0,
                    inbound_weight: 12.0,
                    outbound_weight: 4.0,
                },
                CrateNodeEntry {
                    name: "djinn-graph".to_string(),
                    manifest_path: "crates/djinn-graph/Cargo.toml".to_string(),
                    loc: 6100,
                    node_count: 120,
                    fan_in: 1.0,
                    fan_out: 3.0,
                    inbound_weight: 4.0,
                    outbound_weight: 12.0,
                },
            ],
            edges: vec![
                CrateEdgeEntry {
                    source: "djinn-graph".to_string(),
                    target: "djinn-db".to_string(),
                    weight: 8.0,
                    edge_count: 15,
                },
            ],
            message: None,
        };
        let ops = WorkspaceFixtureOps::new(None).with_crate_graph(crate_graph_resp);
        let db = Database::open_in_memory().expect("open in-memory db");
        let state = McpState::new(
            db,
            EventBus::noop(),
            CatalogService::new(),
            HealthTracker::new(),
            None,
            None,
            None,
            None,
            Arc::new(crate::state::stubs::StubLspOps),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(ops),
        );
        DjinnMcpServer::new(state)
    }

    /// Acceptance criterion: the `crate_graph` op returns a response
    /// with at least one crate for a warmed test workspace.
    #[tokio::test]
    async fn crate_graph_returns_non_empty_crates() {
        let server = crate_graph_fixture_server();
        let ctx = fixture_ctx(None);
        let mut params = test_params("crate_graph");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("crate_graph dispatch succeeds");

        match response {
            CodeGraphResponse::CrateGraph(cg) => {
                assert!(
                    !cg.crates.is_empty(),
                    "expected at least one crate in the response"
                );
                assert_eq!(cg.crates.len(), 2, "expected 2 crates in fixture");
                assert_eq!(cg.crates[0].name, "djinn-db");
                assert_eq!(cg.crates[1].name, "djinn-graph");
            }
            other => panic!("expected CrateGraph response, got {other:?}"),
        }
    }

    /// Acceptance criterion: the `crate_graph` op reports at least one
    /// cross-crate edge (asserting the djinn-graph → djinn-db edge from
    /// the fixture).
    #[tokio::test]
    async fn crate_graph_reports_cross_crate_edges() {
        let server = crate_graph_fixture_server();
        let ctx = fixture_ctx(None);
        let mut params = test_params("crate_graph");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("crate_graph dispatch succeeds");

        match response {
            CodeGraphResponse::CrateGraph(cg) => {
                assert!(
                    !cg.edges.is_empty(),
                    "expected at least one cross-crate edge"
                );
                let edge = &cg.edges[0];
                assert_eq!(
                    edge.source, "djinn-graph",
                    "expected edge source to be djinn-graph"
                );
                assert_eq!(
                    edge.target, "djinn-db",
                    "expected edge target to be djinn-db"
                );
                assert!(
                    edge.weight > 0.0,
                    "expected positive edge weight"
                );
                assert!(
                    edge.edge_count > 0,
                    "expected positive edge_count"
                );
            }
            other => panic!("expected CrateGraph response, got {other:?}"),
        }
    }

    /// Acceptance criterion: the `crate_graph` op uses the default
    /// trait implementation (empty graph) when the warmed graph has
    /// no crate map.
    #[tokio::test]
    async fn crate_graph_default_returns_empty_when_no_crate_map() {
        let ops = WorkspaceFixtureOps::new(None);
        let db = Database::open_in_memory().expect("open in-memory db");
        let state = McpState::new(
            db,
            EventBus::noop(),
            CatalogService::new(),
            HealthTracker::new(),
            None,
            None,
            None,
            None,
            Arc::new(crate::state::stubs::StubLspOps),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(ops),
        );
        let server = DjinnMcpServer::new(state);
        let ctx = fixture_ctx(None);
        let mut params = test_params("crate_graph");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("crate_graph dispatch succeeds with default impl");

        match response {
            CodeGraphResponse::CrateGraph(cg) => {
                assert!(
                    cg.crates.is_empty(),
                    "default impl should return empty crates"
                );
                assert!(
                    cg.edges.is_empty(),
                    "default impl should return empty edges"
                );
            }
            other => panic!("expected CrateGraph response, got {other:?}"),
        }
    }
