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

    // ------------------------------------------------------------------ //
    // boundary_check crate-level tests (r7iw — layering guards)
    // ------------------------------------------------------------------ //

    /// Helper: build a `DjinnMcpServer` with a `WorkspaceFixtureOps` that
    /// returns pre-canned `boundary_check` violations.
    fn boundary_check_fixture_server(violations: Vec<BoundaryViolation>) -> DjinnMcpServer {
        let ops = WorkspaceFixtureOps::new(None).with_boundary_check(violations);
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

    /// 1. db→memory inversion: boundary_check with level=crate catches the
    /// known `djinn-db → djinn-memory` edge.
    #[tokio::test]
    async fn boundary_check_crate_level_catches_db_memory_inversion() {
        let violations = vec![BoundaryViolation {
            rule_index: 2,
            from_key: "djinn-db".to_string(),
            to_key: "djinn-memory".to_string(),
            edge_kind: "crate_depends_on".to_string(),
            from_file: None,
            to_file: None,
            witness_path: Some(vec!["djinn-db".to_string(), "djinn-memory".to_string()]),
        }];
        let server = boundary_check_fixture_server(violations);
        let ctx = fixture_ctx(None);
        let mut params = test_params("boundary_check");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.level = Some("crate".to_string());
        params.rules = Some(vec![BoundaryRule {
            from_glob: "**/djinn-db/**".to_string(),
            to_glob: "**/djinn-memory/**".to_string(),
            description: Some("Known inversion".to_string()),
        }]);

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("boundary_check dispatch succeeds");

        match response {
            CodeGraphResponse::BoundaryCheck(bc) => {
                assert_eq!(
                    bc.violations.len(),
                    1,
                    "should catch exactly one db→memory violation"
                );
                assert_eq!(bc.violations[0].from_key, "djinn-db");
                assert_eq!(bc.violations[0].to_key, "djinn-memory");
                assert_eq!(bc.violations[0].edge_kind, "crate_depends_on");
            }
            other => panic!("expected BoundaryCheck response, got {other:?}"),
        }
    }

    /// 2. Agent high fan-out: boundary_check with level=crate catches
    /// `djinn-agent → djinn-db` or `djinn-agent → djinn-graph` edges.
    #[tokio::test]
    async fn boundary_check_crate_level_catches_agent_high_fan_out() {
        let violations = vec![
            BoundaryViolation {
                rule_index: 0,
                from_key: "djinn-agent".to_string(),
                to_key: "djinn-db".to_string(),
                edge_kind: "crate_depends_on".to_string(),
                from_file: None,
                to_file: None,
                witness_path: Some(vec!["djinn-agent".to_string(), "djinn-db".to_string()]),
            },
            BoundaryViolation {
                rule_index: 1,
                from_key: "djinn-agent".to_string(),
                to_key: "djinn-graph".to_string(),
                edge_kind: "crate_depends_on".to_string(),
                from_file: None,
                to_file: None,
                witness_path: Some(vec!["djinn-agent".to_string(), "djinn-graph".to_string()]),
            },
        ];
        let server = boundary_check_fixture_server(violations);
        let ctx = fixture_ctx(None);
        let mut params = test_params("boundary_check");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.level = Some("crate".to_string());
        params.rules = Some(vec![
            BoundaryRule {
                from_glob: "**/djinn-agent/**".to_string(),
                to_glob: "**/djinn-db/**".to_string(),
                description: Some("Agent must not import db".to_string()),
            },
            BoundaryRule {
                from_glob: "**/djinn-agent/**".to_string(),
                to_glob: "**/djinn-graph/**".to_string(),
                description: Some("Agent must not import graph".to_string()),
            },
        ]);

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("boundary_check dispatch succeeds");

        match response {
            CodeGraphResponse::BoundaryCheck(bc) => {
                assert_eq!(
                    bc.violations.len(),
                    2,
                    "should catch both agent→db and agent→graph violations"
                );
                let from_keys: Vec<_> = bc.violations.iter().map(|v| v.from_key.as_str()).collect();
                let to_keys: Vec<_> = bc.violations.iter().map(|v| v.to_key.as_str()).collect();
                assert!(from_keys.iter().all(|k| *k == "djinn-agent"));
                assert!(to_keys.contains(&"djinn-db"));
                assert!(to_keys.contains(&"djinn-graph"));
            }
            other => panic!("expected BoundaryCheck response, got {other:?}"),
        }
    }

    /// 3. Clean pass: boundary_check returns empty violations when rules
    /// don't match any edges.
    #[tokio::test]
    async fn boundary_check_crate_level_returns_empty_when_clean() {
        let server = boundary_check_fixture_server(vec![]);
        let ctx = fixture_ctx(None);
        let mut params = test_params("boundary_check");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.level = Some("crate".to_string());
        params.rules = Some(vec![BoundaryRule {
            from_glob: "**/crate-c/**".to_string(),
            to_glob: "**/crate-d/**".to_string(),
            description: Some("Non-existent edge".to_string()),
        }]);

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("boundary_check dispatch succeeds");

        match response {
            CodeGraphResponse::BoundaryCheck(bc) => {
                assert!(
                    bc.violations.is_empty(),
                    "expected no violations for clean graph, got {bc:?}"
                );
            }
            other => panic!("expected BoundaryCheck response, got {other:?}"),
        }
    }

    /// 4. Synthetic violation: adding a violating edge to a test fixture
    /// causes boundary_check to catch it.
    #[tokio::test]
    async fn boundary_check_crate_level_catches_synthetic_violation() {
        let violations = vec![BoundaryViolation {
            rule_index: 0,
            from_key: "synthetic-upper".to_string(),
            to_key: "synthetic-lower".to_string(),
            edge_kind: "crate_depends_on".to_string(),
            from_file: None,
            to_file: None,
            witness_path: Some(vec![
                "synthetic-upper".to_string(),
                "synthetic-lower".to_string(),
            ]),
        }];
        let server = boundary_check_fixture_server(violations);
        let ctx = fixture_ctx(None);
        let mut params = test_params("boundary_check");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.level = Some("crate".to_string());
        params.rules = Some(vec![BoundaryRule {
            from_glob: "**/synthetic-upper/**".to_string(),
            to_glob: "**/synthetic-lower/**".to_string(),
            description: Some("Synthetic test rule".to_string()),
        }]);

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("boundary_check dispatch succeeds");

        match response {
            CodeGraphResponse::BoundaryCheck(bc) => {
                assert_eq!(
                    bc.violations.len(),
                    1,
                    "should catch the synthetic violating edge"
                );
                assert_eq!(bc.violations[0].from_key, "synthetic-upper");
                assert_eq!(bc.violations[0].to_key, "synthetic-lower");
            }
            other => panic!("expected BoundaryCheck response, got {other:?}"),
        }
    }

    /// 5. Rule format validation: empty rules list is rejected by the handler.
    #[tokio::test]
    async fn boundary_check_rejects_empty_rules() {
        let server = boundary_check_fixture_server(vec![]);
        let ctx = fixture_ctx(None);
        let mut params = test_params("boundary_check");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.level = Some("crate".to_string());
        params.rules = Some(vec![]);

        let result = server.dispatch_code_graph(&ctx, &mut params).await;
        assert!(result.is_err(), "expected error for empty rules");
        let err = result.unwrap_err();
        assert!(
            err.contains("must not be empty"),
            "error should mention empty rules: {err}"
        );
    }

    /// 6. Rule format validation: missing rules field is rejected by the handler.
    #[tokio::test]
    async fn boundary_check_rejects_missing_rules() {
        let server = boundary_check_fixture_server(vec![]);
        let ctx = fixture_ctx(None);
        let mut params = test_params("boundary_check");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.level = Some("crate".to_string());
        params.rules = None;

        let result = server.dispatch_code_graph(&ctx, &mut params).await;
        assert!(result.is_err(), "expected error for missing rules");
        let err = result.unwrap_err();
        assert!(
            err.contains("'rules' is required"),
            "error should mention missing rules: {err}"
        );
    }
