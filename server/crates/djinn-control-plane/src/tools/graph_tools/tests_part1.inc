    use super::*;

    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use djinn_core::events::EventBus;
    use djinn_db::Database;
    use djinn_provider::catalog::{CatalogService, HealthTracker};

    use crate::bridge::{
        ApiSurfaceEntry, BoundaryRule, BoundaryViolation, ChangedRange, ChurnEntry,
        ComplexityResult, CoupledPairEntry, CouplingEntry, CouplingHubEntry, CycleGroup,
        DeadSymbolEntry, DeprecatedHit, DetectedChangesResult, DiffTouchesResult, EdgeEntry,
        GraphStatus, HotPathHit, HotspotEntry, ImpactResult, MetricsAtResult, NeighborsResult,
        OrphanEntry, PathResult, ProjectCtx, QuerySubgraphBudget, QuerySubgraphEdge,
        QuerySubgraphNode, QuerySubgraphRequest, QuerySubgraphResult, QuerySubgraphSeedDebug,
        QuerySubgraphTraversalDebug, RankedNode, RefactorCandidate, RepoGraphOps, ResolveOutcome,
        SearchHit, SnapshotPayload, SymbolAtHit, SymbolContext, SymbolDescription,
    };
    use crate::server::DjinnMcpServer;
    use crate::state::McpState;

    #[derive(Clone)]
    struct RecordingQuerySubgraphOps {
        seen: Arc<Mutex<Vec<QuerySubgraphRequest>>>,
        response: QuerySubgraphResult,
    }

    impl RecordingQuerySubgraphOps {
        fn new(response: QuerySubgraphResult) -> Self {
            Self {
                seen: Arc::new(Mutex::new(Vec::new())),
                response,
            }
        }

        fn seen_requests(&self) -> Vec<QuerySubgraphRequest> {
            self.seen.lock().expect("seen requests lock").clone()
        }
    }

    #[async_trait]
    impl RepoGraphOps for RecordingQuerySubgraphOps {
        async fn neighbors(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<NeighborsResult, String> {
            unreachable!("query_subgraph tests should not call neighbors")
        }

        async fn ranked(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RankedNode>, String> {
            unreachable!("query_subgraph tests should not call ranked")
        }

        async fn implementations(&self, _: &ProjectCtx, _: &str) -> Result<Vec<String>, String> {
            unreachable!("query_subgraph tests should not call implementations")
        }

        async fn impact(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: &str,
            _: usize,
            _: Option<&str>,
            _: Option<f64>,
        ) -> Result<ImpactResult, String> {
            unreachable!("query_subgraph tests should not call impact")
        }

        async fn search(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<SearchHit>, String> {
            unreachable!("query_subgraph tests should not call search")
        }

        async fn query_subgraph(
            &self,
            _: &ProjectCtx,
            req: QuerySubgraphRequest,
        ) -> Result<QuerySubgraphResult, String> {
            self.seen.lock().expect("seen requests lock").push(req);
            Ok(self.response.clone())
        }

        async fn cycles(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<CycleGroup>, String> {
            unreachable!("query_subgraph tests should not call cycles")
        }

        async fn orphans(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<OrphanEntry>, String> {
            unreachable!("query_subgraph tests should not call orphans")
        }

        async fn path(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: &str,
            _: &str,
            _: Option<usize>,
        ) -> Result<Option<PathResult>, String> {
            unreachable!("query_subgraph tests should not call path")
        }

        async fn edges(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<EdgeEntry>, String> {
            unreachable!("query_subgraph tests should not call edges")
        }

        async fn describe(
            &self,
            _: &ProjectCtx,
            _: &str,
        ) -> Result<Option<SymbolDescription>, String> {
            unreachable!("query_subgraph tests should not call describe")
        }

        async fn context(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: bool,
        ) -> Result<Option<SymbolContext>, String> {
            unreachable!("query_subgraph tests should not call context")
        }

        async fn status(&self, _: &ProjectCtx) -> Result<GraphStatus, String> {
            unreachable!("query_subgraph tests should not call status")
        }

        async fn snapshot(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: SnapshotLevel,
            _: usize,
            _: &GraphExclusions,
        ) -> Result<SnapshotPayload, String> {
            unreachable!("query_subgraph tests should not call snapshot")
        }

        async fn symbols_at(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: u32,
            _: Option<u32>,
        ) -> Result<Vec<SymbolAtHit>, String> {
            unreachable!("query_subgraph tests should not call symbols_at")
        }

        async fn diff_touches(
            &self,
            _: &ProjectCtx,
            _: &[ChangedRange],
        ) -> Result<DiffTouchesResult, String> {
            unreachable!("query_subgraph tests should not call diff_touches")
        }

        async fn detect_changes(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: &[String],
        ) -> Result<DetectedChangesResult, String> {
            unreachable!("query_subgraph tests should not call detect_changes")
        }

        async fn api_surface(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<ApiSurfaceEntry>, String> {
            unreachable!("query_subgraph tests should not call api_surface")
        }

        async fn boundary_check(
            &self,
            _: &ProjectCtx,
            _: &[BoundaryRule],
        ) -> Result<Vec<BoundaryViolation>, String> {
            unreachable!("query_subgraph tests should not call boundary_check")
        }

        async fn hotspots(
            &self,
            _: &ProjectCtx,
            _: u32,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<HotspotEntry>, String> {
            unreachable!("query_subgraph tests should not call hotspots")
        }

        async fn complexity(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<ComplexityResult, String> {
            unreachable!("query_subgraph tests should not call complexity")
        }

        async fn refactor_candidates(
            &self,
            _: &ProjectCtx,
            _: Option<u32>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RefactorCandidate>, String> {
            unreachable!("query_subgraph tests should not call refactor_candidates")
        }

        async fn metrics_at(&self, _: &ProjectCtx) -> Result<MetricsAtResult, String> {
            unreachable!("query_subgraph tests should not call metrics_at")
        }

        async fn dead_symbols(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<DeadSymbolEntry>, String> {
            unreachable!("query_subgraph tests should not call dead_symbols")
        }

        async fn deprecated_callers(
            &self,
            _: &ProjectCtx,
            _: usize,
        ) -> Result<Vec<DeprecatedHit>, String> {
            unreachable!("query_subgraph tests should not call deprecated_callers")
        }

        async fn touches_hot_path(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: &[String],
            _: &[String],
            _: &[String],
        ) -> Result<Vec<HotPathHit>, String> {
            unreachable!("query_subgraph tests should not call touches_hot_path")
        }

        async fn coupling(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<CouplingEntry>, String> {
            unreachable!("query_subgraph tests should not call coupling")
        }

        async fn churn(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
        ) -> Result<Vec<ChurnEntry>, String> {
            unreachable!("query_subgraph tests should not call churn")
        }

        async fn coupling_hotspots(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CoupledPairEntry>, String> {
            unreachable!("query_subgraph tests should not call coupling_hotspots")
        }

        async fn coupling_hubs(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CouplingHubEntry>, String> {
            unreachable!("query_subgraph tests should not call coupling_hubs")
        }

        async fn resolve(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
        ) -> Result<ResolveOutcome, String> {
            Ok(ResolveOutcome::NotFound)
        }
    }

    fn query_subgraph_canned_response() -> QuerySubgraphResult {
        QuerySubgraphResult {
            query: "How is auth routed?".to_string(),
            nodes: vec![
                QuerySubgraphNode {
                    uid: "file:src/auth.rs".to_string(),
                    kind: "file".to_string(),
                    display_name: "src/auth.rs".to_string(),
                    file_path: Some("src/auth.rs".to_string()),
                    workspace: Some("server".to_string()),
                    is_seed: true,
                    is_hub: false,
                    degree: 3,
                },
                QuerySubgraphNode {
                    uid: "sym:login".to_string(),
                    kind: "symbol".to_string(),
                    display_name: "login".to_string(),
                    file_path: Some("src/auth.rs".to_string()),
                    workspace: Some("server".to_string()),
                    is_seed: false,
                    is_hub: false,
                    degree: 1,
                },
            ],
            edges: vec![QuerySubgraphEdge {
                from_uid: "file:src/auth.rs".to_string(),
                to_uid: "sym:login".to_string(),
                kind: "contains_definition".to_string(),
                confidence: 0.98,
                reason: Some("seed expansion".to_string()),
            }],
            seeds: vec![QuerySubgraphSeedDebug {
                uid: "file:src/auth.rs".to_string(),
                display_name: "src/auth.rs".to_string(),
                score: 0.91,
                source: "hybrid".to_string(),
                matched_text: Some("auth routed".to_string()),
                debug: vec!["rrf=0.91".to_string()],
            }],
            inferred_edge_kinds: vec!["calls".to_string(), "imports".to_string()],
            budget: QuerySubgraphBudget {
                requested_tokens: 2_048,
                estimated_tokens: 1_200,
                truncated: true,
                omitted_nodes: 4,
                omitted_edges: 7,
            },
            traversal: QuerySubgraphTraversalDebug {
                max_depth: 2,
                hub_degree_threshold: 99,
                hubs_blocked: vec!["sym:common".to_string()],
                skipped_edge_kinds: vec!["writes".to_string()],
            },
            narrowing_hints: vec!["narrow with context_filter=auth middleware".to_string()],
        }
    }

    fn query_subgraph_test_server(
        graph: RecordingQuerySubgraphOps,
    ) -> (DjinnMcpServer, RecordingQuerySubgraphOps) {
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
            Arc::new(graph.clone()),
        );
        (DjinnMcpServer::new(state), graph)
    }

    fn query_subgraph_ctx() -> ProjectCtx {
        ProjectCtx {
            id: "proj-query-subgraph".to_string(),
            clone_path: "/workspace/repo".to_string(),
            workspace: Some("server".to_string()),
            sub_path: None,
        }
    }

    #[tokio::test]
    async fn query_subgraph_dispatch_maps_request_fields() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_canned_response());
        let (server, graph) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.workspace = Some("server".to_string());
        params.query = Some("  How is auth routed?  ".to_string());
        params.context_filter = Some(" middleware ".to_string());
        params.file_filter = Some(" src/auth ".to_string());
        params.kind_filter = Some("symbol".to_string());
        params.edge_filters = Some(vec![" Calls ".to_string(), "IMPORTS".to_string()]);
        params.token_budget = Some(2_048);
        params.max_depth = Some(3);
        params.max_seeds = Some(4);

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("query_subgraph dispatch succeeds");

        match response {
            CodeGraphResponse::QuerySubgraph(resp) => {
                assert_eq!(resp.query_subgraph.query, "How is auth routed?");
            }
            other => panic!("expected query_subgraph response, got {other:?}"),
        }
        let seen = graph.seen_requests();
        assert_eq!(seen.len(), 1, "RepoGraphOps::query_subgraph called once");
        let req = &seen[0];
        assert_eq!(req.query, "How is auth routed?");
        assert_eq!(req.workspace.as_deref(), Some("server"));
        assert_eq!(req.context_filter.as_deref(), Some("middleware"));
        assert_eq!(req.file_filter.as_deref(), Some("src/auth"));
        assert_eq!(req.kind_filter.as_deref(), Some("symbol"));
        assert_eq!(req.edge_filter, vec!["calls", "imports"]);
        assert_eq!(req.token_budget, Some(2_048));
        assert_eq!(req.max_depth, Some(3));
        assert_eq!(req.max_seeds, Some(4));
    }

    #[tokio::test]
    async fn query_subgraph_dispatch_rejects_missing_and_blank_query() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_canned_response());
        let (server, graph) = query_subgraph_test_server(graph);
        let ctx = query_subgraph_ctx();

        let mut missing = test_params("query_subgraph");
        missing.project_id = ctx.id.clone();
        missing.project_path = ctx.clone_path.clone();
        let err = server
            .dispatch_code_graph(&ctx, &mut missing)
            .await
            .expect_err("missing query rejected through dispatch");
        assert!(
            err.contains("'query' is required for operation 'query_subgraph'"),
            "explicit query validation error: {err}"
        );

        let mut blank = test_params("query_subgraph");
        blank.project_id = ctx.id.clone();
        blank.project_path = ctx.clone_path.clone();
        blank.query = Some(" \t\n ".to_string());
        let err = server
            .dispatch_code_graph(&ctx, &mut blank)
            .await
            .expect_err("blank query rejected through dispatch");
        assert!(
            err.contains("'query' is required for operation 'query_subgraph'"),
            "explicit blank query validation error: {err}"
        );
        assert!(
            graph.seen_requests().is_empty(),
            "invalid requests must not reach RepoGraphOps::query_subgraph"
        );
    }

    #[tokio::test]
    async fn query_subgraph_dispatch_applies_token_budget_defaults_clamps_and_rejections() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_canned_response());
        let (server, graph) = query_subgraph_test_server(graph);
        let ctx = query_subgraph_ctx();

        let mut default_budget = test_params("query_subgraph");
        default_budget.project_id = ctx.id.clone();
        default_budget.project_path = ctx.clone_path.clone();
        default_budget.query = Some("default budget".to_string());
        server
            .dispatch_code_graph(&ctx, &mut default_budget)
            .await
            .expect("default budget dispatch succeeds");

        let mut clamped_low = test_params("query_subgraph");
        clamped_low.project_id = ctx.id.clone();
        clamped_low.project_path = ctx.clone_path.clone();
        clamped_low.query = Some("small budget".to_string());
        clamped_low.token_budget = Some(128);
        server
            .dispatch_code_graph(&ctx, &mut clamped_low)
            .await
            .expect("low budget dispatch succeeds with clamp");

        let mut clamped_high = test_params("query_subgraph");
        clamped_high.project_id = ctx.id.clone();
        clamped_high.project_path = ctx.clone_path.clone();
        clamped_high.query = Some("large budget".to_string());
        clamped_high.token_budget = Some(99_999);
        server
            .dispatch_code_graph(&ctx, &mut clamped_high)
            .await
            .expect("high budget dispatch succeeds with clamp");

        let requests = graph.seen_requests();
        assert_eq!(
            requests[0].token_budget, None,
            "omitted budget stays omitted"
        );
        assert_eq!(
            requests[1].token_budget,
            Some(1_024),
            "low budget clamps up"
        );
        assert_eq!(
            requests[2].token_budget,
            Some(32_000),
            "high budget clamps down"
        );

        let mut zero = test_params("query_subgraph");
        zero.project_id = ctx.id.clone();
        zero.project_path = ctx.clone_path.clone();
        zero.query = Some("zero budget".to_string());
        zero.token_budget = Some(0);
        let err = server
            .dispatch_code_graph(&ctx, &mut zero)
            .await
            .expect_err("zero budget rejected through dispatch");
        assert!(
            err.contains("invalid token_budget 0"),
            "invalid budget error should name token_budget: {err}"
        );
    }

    #[tokio::test]
    async fn query_subgraph_dispatch_serializes_full_response_shape() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_canned_response());
        let (server, _) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.query = Some("How is auth routed?".to_string());

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("query_subgraph dispatch succeeds");
        let json = serde_json::to_value(CodeGraphResponse::QuerySubgraph(match response {
            CodeGraphResponse::QuerySubgraph(resp) => resp,
            other => panic!("expected query_subgraph response, got {other:?}"),
        }))
        .expect("serialize query_subgraph response");
        let payload = json
            .get("query_subgraph")
            .and_then(|v| v.as_object())
            .expect("query_subgraph payload object");
        for required in [
            "query",
            "nodes",
            "edges",
            "seeds",
            "inferred_edge_kinds",
            "budget",
            "traversal",
            "narrowing_hints",
        ] {
            assert!(payload.contains_key(required), "missing {required}: {json}");
        }
        assert_eq!(payload["nodes"].as_array().expect("nodes array").len(), 2);
        assert_eq!(payload["edges"].as_array().expect("edges array").len(), 1);

        let seed = payload["seeds"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_object())
            .expect("seed debug object");
        for required in [
            "uid",
            "display_name",
            "score",
            "source",
            "matched_text",
            "debug",
        ] {
            assert!(
                seed.contains_key(required),
                "seed missing {required}: {seed:?}"
            );
        }

        let budget = payload["budget"].as_object().expect("budget object");
        for required in [
            "requested_tokens",
            "estimated_tokens",
            "truncated",
            "omitted_nodes",
            "omitted_edges",
        ] {
            assert!(
                budget.contains_key(required),
                "budget missing {required}: {budget:?}"
            );
        }
        assert_eq!(
            budget.get("truncated").and_then(|v| v.as_bool()),
            Some(true)
        );

        let traversal = payload["traversal"].as_object().expect("traversal object");
        for required in [
            "max_depth",
            "hub_degree_threshold",
            "hubs_blocked",
            "skipped_edge_kinds",
        ] {
            assert!(
                traversal.contains_key(required),
                "traversal missing {required}: {traversal:?}"
            );
        }
        assert_eq!(
            payload["narrowing_hints"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str()),
            Some("narrow with context_filter=auth middleware")
        );
    }

    #[test]
    fn workspaces_response_serializes_workspace_metadata() {
        let json = serde_json::to_value(CodeGraphResponse::Workspaces(WorkspacesResponse {
            result: WorkspacesResult {
                project_id: "proj-1".to_string(),
                workspaces: vec![crate::bridge::GraphWorkspaceEntry {
                    slug: "server".to_string(),
                    name: "server".to_string(),
                    node_count: 42,
                    commit_sha: Some("abc123".to_string()),
                    warmed_at: Some("2026-06-09T00:00:00.000Z".to_string()),
                    status: Some("ready".to_string()),
                }],
            },
            next_step: None,
        }))
        .expect("serialize workspaces response");

        assert_eq!(
            json.get("project_id").and_then(|v| v.as_str()),
            Some("proj-1")
        );
        let workspace = json
            .get("workspaces")
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .and_then(|v| v.as_object())
            .expect("workspace entry");
        for required in [
            "slug",
            "name",
            "node_count",
            "commit_sha",
            "warmed_at",
            "status",
        ] {
            assert!(
                workspace.contains_key(required),
                "missing {required}: {workspace:?}"
            );
        }
        assert_eq!(
            workspace.get("slug").and_then(|v| v.as_str()),
            Some("server")
        );
        assert_eq!(
            workspace.get("node_count").and_then(|v| v.as_u64()),
            Some(42)
        );
    }

    #[test]
    fn validates_operation_field() {
        let params = test_params("unknown_op");
        // Just test validation logic, not the async handler
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn validates_direction() {
        assert!(validate_direction(Some("incoming")).is_ok());
        assert!(validate_direction(Some("outgoing")).is_ok());
        assert!(validate_direction(None).is_ok());
        assert!(validate_direction(Some("both")).is_err());
    }

    #[test]
    fn validates_kind_filter() {
        assert!(validate_kind_filter(Some("file")).is_ok());
        assert!(validate_kind_filter(Some("symbol")).is_ok());
        assert!(validate_kind_filter(None).is_ok());
        assert!(validate_kind_filter(Some("unknown")).is_err());
    }

    /// PR B4: search-mode resolution layers caller intent on top of
    /// the `DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE` env var. Caller
    /// always wins; missing env var defaults to `name`.
    #[test]
    fn resolve_search_mode_caller_overrides_env() {
        // Caller pin → wins regardless of env var value.
        let prev = std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE").ok();
        unsafe {
            std::env::set_var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE", "hybrid");
        }
        assert_eq!(resolve_search_mode(Some("name")).unwrap(), SearchMode::Name);
        // Explicit `hybrid` also resolves.
        assert_eq!(
            resolve_search_mode(Some("hybrid")).unwrap(),
            SearchMode::Hybrid
        );
        // Unset → env var wins (`hybrid`).
        assert_eq!(resolve_search_mode(None).unwrap(), SearchMode::Hybrid);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE", v),
                None => std::env::remove_var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE"),
            }
        }
    }

    #[test]
    fn resolve_search_mode_default_is_name() {
        let prev = std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE").ok();
        unsafe {
            std::env::remove_var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE");
        }
        assert_eq!(resolve_search_mode(None).unwrap(), SearchMode::Name);
        unsafe {
            if let Some(v) = prev {
                std::env::set_var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE", v);
            }
        }
    }

    #[test]
    fn resolve_search_mode_rejects_unknown_value() {
        assert!(resolve_search_mode(Some("fuzzy")).is_err());
    }

    /// PR B4: `mode` field deserialises off the wire.
    #[test]
    fn parses_search_mode_from_json() {
        let json = serde_json::json!({
            "operation": "search",
            "project": "/workspace/repo",
            "query": "permissions check",
            "mode": "hybrid",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.mode.as_deref(), Some("hybrid"));
    }

    #[test]
    fn parses_workspace_from_json() {
        for (workspace, expected) in [
            ("", Some("")),
            ("nonexistent", Some("nonexistent")),
            ("server", Some("server")),
        ] {
            let json = serde_json::json!({
                "operation": "ranked",
                "project": "/workspace/repo",
                "workspace": workspace,
            });

            let params: CodeGraphParams = serde_json::from_value(json).unwrap();

            assert_eq!(params.workspace.as_deref(), expected);
        }
    }

    #[test]
    fn normalize_clears_empty_workspace() {
        let json = serde_json::json!({
            "operation": "ranked",
            "project": "/workspace/repo",
            "workspace": "",
        });
        let mut params: CodeGraphParams = serde_json::from_value(json).unwrap();

        params.normalize();

        assert_eq!(params.workspace, None);
    }

    // ── pb94: workspace scoping semantic tests ─────────────────────────────────
    //
    // The acceptance criteria require explicit coverage for:
    // - empty `workspace: ""` continuing to normalize to `None`
    // - valid slugs hard-filtering for listing/bounded ops
    // - valid slugs scoping only seed/endpoint for traversal ops
    // - unknown non-empty slugs returning full result + structured hint
    // - single-workspace graphs being a no-op (no surprising hard-empty)
    //
    // Each fixture is a thin `RepoGraphOps` impl that records the
    // workspace arg it sees (so the test can assert "what did the
    // handler pass downstream?") and returns canned data from the
    // workspace-aware methods. The fixture's `workspace_hint` is
    // programmable so the test can simulate single-vs-multi-workspace
    // graphs and known-vs-unknown slugs.

    /// Programmable workspace fixture: records calls, returns canned
    /// data, and answers `workspace_hint` according to a test-supplied
    /// plan. Mirrors the production `workspace_hint` semantics — single
    /// workspace projects never produce a hint, unknown slugs against
    /// multi-workspace projects surface the candidate list.
    #[derive(Clone)]
    struct WorkspaceFixtureOps {
        /// Every bridge method that takes a workspace records the slug
        /// it was called with into a parallel slot so the test can
        /// assert "the handler passed this to the bridge".
        seen_workspaces: Arc<Mutex<Vec<Option<String>>>>,
        /// What `workspace_hint` should return. `Some(candidates)`
        /// mimics an unknown slug on a multi-workspace graph; `None`
        /// mimics single-workspace / no-workspace / known-slug cases.
        hint: Option<Vec<String>>,
        ranked_nodes: Vec<RankedNode>,
        impact_result: ImpactResult,
    }

    impl WorkspaceFixtureOps {
        fn new(hint: Option<Vec<String>>) -> Self {
            Self {
                seen_workspaces: Arc::new(Mutex::new(Vec::new())),
                hint,
                ranked_nodes: Vec::new(),
                impact_result: ImpactResult::Detailed(Vec::new()),
            }
        }

        fn with_ranked_and_impact(
            hint: Option<Vec<String>>,
            ranked_nodes: Vec<RankedNode>,
            impact_result: ImpactResult,
        ) -> Self {
            Self {
                seen_workspaces: Arc::new(Mutex::new(Vec::new())),
                hint,
                ranked_nodes,
                impact_result,
            }
        }

        fn record_workspace(&self, workspace: Option<&str>) {
            let mut guard = self.seen_workspaces.lock().expect("seen_workspaces");
            guard.push(workspace.map(str::to_string));
        }

        fn seen_workspaces(&self) -> Vec<Option<String>> {
            self.seen_workspaces
                .lock()
                .expect("seen_workspaces")
                .clone()
        }
    }

    #[async_trait]
    impl RepoGraphOps for WorkspaceFixtureOps {
        async fn ranked(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RankedNode>, String> {
            self.record_workspace(workspace);
            Ok(self.ranked_nodes.clone())
        }

        async fn impact(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: &str,
            _: usize,
            _: Option<&str>,
            _: Option<f64>,
        ) -> Result<ImpactResult, String> {
            self.record_workspace(workspace);
            Ok(self.impact_result.clone())
        }

        async fn workspace_hint(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
        ) -> Result<Option<Vec<String>>, String> {
            Ok(self.hint.clone())
        }

        // The rest are no-ops — these tests don't exercise them. We
        // still have to satisfy the trait so the dispatcher doesn't
        // trip when it routes the call.
        async fn workspaces(
            &self,
            ctx: &ProjectCtx,
        ) -> Result<crate::bridge::WorkspacesResult, String> {
            Ok(crate::bridge::WorkspacesResult {
                project_id: ctx.id.clone(),
                workspaces: Vec::new(),
            })
        }

        async fn neighbors(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<NeighborsResult, String> {
            Ok(NeighborsResult::Detailed(Vec::new()))
        }
        async fn implementations(&self, _: &ProjectCtx, _: &str) -> Result<Vec<String>, String> {
            Ok(Vec::new())
        }
        async fn search(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<SearchHit>, String> {
            Ok(Vec::new())
        }
        async fn query_subgraph(
            &self,
            _: &ProjectCtx,
            req: QuerySubgraphRequest,
        ) -> Result<QuerySubgraphResult, String> {
            Ok(QuerySubgraphResult {
                query: req.query,
                nodes: Vec::new(),
                edges: Vec::new(),
                seeds: Vec::new(),
                inferred_edge_kinds: Vec::new(),
                budget: QuerySubgraphBudget {
                    requested_tokens: 0,
                    estimated_tokens: 0,
                    truncated: false,
                    omitted_nodes: 0,
                    omitted_edges: 0,
                },
                traversal: QuerySubgraphTraversalDebug {
                    max_depth: 0,
                    hub_degree_threshold: 0,
                    hubs_blocked: Vec::new(),
                    skipped_edge_kinds: Vec::new(),
                },
                narrowing_hints: Vec::new(),
            })
        }
        async fn cycles(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<CycleGroup>, String> {
            Ok(Vec::new())
        }
        async fn orphans(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<OrphanEntry>, String> {
            self.record_workspace(workspace);
            Ok(Vec::new())
        }
        async fn path(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: &str,
            _: &str,
            _: Option<usize>,
        ) -> Result<Option<PathResult>, String> {
            self.record_workspace(workspace);
            Ok(None)
        }
        async fn edges(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<EdgeEntry>, String> {
            Ok(Vec::new())
        }
        async fn describe(
            &self,
            _: &ProjectCtx,
            _: &str,
        ) -> Result<Option<SymbolDescription>, String> {
            Ok(None)
        }
        async fn context(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: bool,
        ) -> Result<Option<SymbolContext>, String> {
            Ok(None)
        }
        async fn status(&self, _: &ProjectCtx) -> Result<GraphStatus, String> {
            Ok(GraphStatus {
                project_id: "p".to_string(),
                warmed: false,
                last_warm_at: None,
                pinned_commit: None,
                commits_since_pin: None,
            })
        }
        async fn snapshot(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: SnapshotLevel,
            _: usize,
            _: &crate::tools::graph_exclusions::GraphExclusions,
        ) -> Result<SnapshotPayload, String> {
            self.record_workspace(workspace);
            Ok(SnapshotPayload {
                project_id: "p".to_string(),
                git_head: String::new(),
                generated_at: String::new(),
                truncated: false,
                total_nodes: 0,
                total_edges: 0,
                node_cap: 0,
                nodes: Vec::new(),
                edges: Vec::new(),
            })
        }
        async fn symbols_at(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: u32,
            _: Option<u32>,
        ) -> Result<Vec<SymbolAtHit>, String> {
            Ok(Vec::new())
        }
        async fn diff_touches(
            &self,
            _: &ProjectCtx,
            _: &[ChangedRange],
        ) -> Result<crate::bridge::DiffTouchesResult, String> {
            unimplemented!()
        }
        async fn detect_changes(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: &[String],
        ) -> Result<DetectedChangesResult, String> {
            unimplemented!()
        }
        async fn api_surface(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<ApiSurfaceEntry>, String> {
            self.record_workspace(workspace);
            Ok(Vec::new())
        }
        async fn boundary_check(
            &self,
            _: &ProjectCtx,
            _: &[BoundaryRule],
        ) -> Result<Vec<BoundaryViolation>, String> {
            Ok(Vec::new())
        }
        async fn hotspots(
            &self,
            _: &ProjectCtx,
            _: u32,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<HotspotEntry>, String> {
            Ok(Vec::new())
        }
        async fn complexity(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<ComplexityResult, String> {
            Ok(ComplexityResult::Functions(Vec::new()))
        }
        async fn refactor_candidates(
            &self,
            _: &ProjectCtx,
            _: Option<u32>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RefactorCandidate>, String> {
            Ok(Vec::new())
        }
        async fn metrics_at(&self, _: &ProjectCtx) -> Result<MetricsAtResult, String> {
            unimplemented!()
        }
        async fn dead_symbols(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<DeadSymbolEntry>, String> {
            Ok(Vec::new())
        }
        async fn deprecated_callers(
            &self,
            _: &ProjectCtx,
            _: usize,
        ) -> Result<Vec<DeprecatedHit>, String> {
            Ok(Vec::new())
        }
        async fn touches_hot_path(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            _: &[String],
            _: &[String],
            _: &[String],
        ) -> Result<Vec<HotPathHit>, String> {
            self.record_workspace(workspace);
            Ok(Vec::new())
        }
        async fn coupling(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<CouplingEntry>, String> {
            Ok(Vec::new())
        }
        async fn churn(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
        ) -> Result<Vec<ChurnEntry>, String> {
            Ok(Vec::new())
        }
        async fn coupling_hotspots(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CoupledPairEntry>, String> {
            Ok(Vec::new())
        }
        async fn coupling_hubs(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CouplingHubEntry>, String> {
            Ok(Vec::new())
        }
        async fn resolve(
            &self,
            _: &ProjectCtx,
            key: &str,
            _: Option<&str>,
        ) -> Result<crate::bridge::ResolveOutcome, String> {
            // The dispatcher pre-resolves the caller's `key` (or
            // `from`/`to` for `path`) before routing to the inner op.
            // Echoing the input back as the resolved uid keeps the
            // dispatcher from short-circuiting to `NotFound` while
            // still letting the inner handler see the test's `key`
            // string.
            Ok(crate::bridge::ResolveOutcome::Found(key.to_string()))
        }
    }

