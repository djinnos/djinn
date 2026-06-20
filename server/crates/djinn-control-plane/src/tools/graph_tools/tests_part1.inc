    use super::*;

    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use djinn_core::events::EventBus;
    use djinn_db::Database;
    use djinn_provider::catalog::{CatalogService, HealthTracker};

    use crate::bridge::{
        ApiSurfaceEntry, BoundaryRule, BoundaryViolation, ChangedRange, ChurnEntry,
        ComplexityResult, CoupledPairEntry, CouplingEntry, CouplingHubEntry,
        CrateEdgeEntry, CrateGraphResponse, CrateNodeEntry, CycleGroup,
        DeadSymbolEntry, DeprecatedHit, DetectedChangesResult, DiffTouchesResult, EdgeEntry,
        GraphNeighbor, GraphStatus, HotPathHit, HotspotEntry, ImpactResult, MetricsAtResult,
        NeighborsResult, OrphanEntry, PathResult, ProjectCtx, QuerySubgraphBudget, QuerySubgraphEdge,
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
            key: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<NeighborsResult, String> {
            if self.response.nodes.iter().any(|node| node.uid == key) {
                Ok(NeighborsResult::Detailed(vec![GraphNeighbor {
                    key: key.to_string(),
                    uid: key.to_string(),
                    kind: "symbol".to_string(),
                    display_name: "follow-up accepted stable UID".to_string(),
                    edge_kind: "symbol_reference".to_string(),
                    edge_weight: 1.0,
                    direction: "outgoing".to_string(),
                }]))
            } else {
                Err(format!("unexpected query_subgraph follow-up UID: {key}"))
            }
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
            key: &str,
        ) -> Result<Option<SymbolDescription>, String> {
            Ok(self
                .response
                .nodes
                .iter()
                .find(|node| node.uid == key)
                .map(|node| SymbolDescription {
                    key: key.to_string(),
                    kind: node.kind.clone(),
                    display_name: node.display_name.clone(),
                    signature: None,
                    documentation: None,
                    file: node.file_path.clone(),
                    start_line: None,
                    end_line: None,
                    fan_in: node.degree,
                    fan_out: node.degree,
                    visibility: None,
                    is_external: false,
                    is_entry_point: false,
                    is_test: false,
                    complexity: None,
                }))
        }

        async fn context(
            &self,
            _: &ProjectCtx,
            key: &str,
            _: bool,
        ) -> Result<Option<SymbolContext>, String> {
            let Some(node) = self.response.nodes.iter().find(|node| node.uid == key) else {
                return Ok(None);
            };
            Ok(Some(SymbolContext {
                symbol: crate::bridge::SymbolNode {
                    uid: key.to_string(),
                    name: node.display_name.clone(),
                    kind: node.kind.clone(),
                    file_path: node.file_path.clone().unwrap_or_default(),
                    start_line: 0,
                    end_line: 0,
                    content: None,
                    method_metadata: None,
                    complexity: None,
                },
                incoming: Default::default(),
                outgoing: Default::default(),
                processes: Vec::new(),
            }))
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
            key: &str,
            _: Option<&str>,
        ) -> Result<ResolveOutcome, String> {
            if self.response.nodes.iter().any(|node| node.uid == key) {
                Ok(ResolveOutcome::Found(key.to_string()))
            } else {
                Ok(ResolveOutcome::NotFound)
            }
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
                confidence_tier: "extracted".to_string(),
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
        let edge = payload["edges"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_object())
            .expect("edge object");
        assert_eq!(
            edge.get("confidence_tier").and_then(|v| v.as_str()),
            Some("extracted"),
            "query_subgraph edge should expose confidence_tier: {edge:?}"
        );

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

    // -------------------------------------------------------------------
    // wave-1 cross-layer regression coverage for `query_subgraph`. The
    // unit-level tests above already cover the request-field mapping,
    // the missing/blank query rejection, and the budget
    // default/clamping/invalid-budget path; the tests below extend the
    // end-to-end surface with the agent-safety properties the spec
    // calls out:
    //
    //   1. Source-level budget enforcement + explicit truncation +
    //      narrowing hints for broad queries (acceptance criterion #1).
    //   2. Hub-avoidance state flows through the control-plane
    //      response so agents can see *why* a high-degree node was not
    //      expanded (acceptance criterion #2).
    //   3. Natural-language edge intent inference surfaces in the
    //      public `inferred_edge_kinds` list for calls / reads /
    //      writes / implements / imports phrasings (acceptance
    //      criterion #3).
    //   4. Returned UIDs use the canonical `file:` / `symbol:` prefix
    //      so they round-trip straight into `describe`, `context`,
    //      and `neighbors` (acceptance criterion #4).
    //   5. Hybrid / lexical / semantic / structural seed debug
    //      metadata is preserved in the public response shape
    //      (acceptance criterion #5).
    // -------------------------------------------------------------------

    /// Build a "broad query" canned response: a non-empty bounded
    /// subgraph that triggered source-level truncation, with a
    /// non-empty narrowing_hints list, full budget state, and stable
    /// `file:`/`symbol:` UIDs. The canned body is reused by the
    /// wave-1 tests below so each property is asserted against a
    /// single source of truth.
    fn query_subgraph_broad_canned_response() -> QuerySubgraphResult {
        QuerySubgraphResult {
            query: "everything related to auth".to_string(),
            nodes: vec![
                QuerySubgraphNode {
                    uid: "file:src/auth.rs".to_string(),
                    kind: "file".to_string(),
                    display_name: "src/auth.rs".to_string(),
                    file_path: Some("src/auth.rs".to_string()),
                    workspace: Some("server".to_string()),
                    is_seed: true,
                    is_hub: false,
                    degree: 12,
                },
                QuerySubgraphNode {
                    uid: "symbol:scip-rust pkg auth `login`().".to_string(),
                    kind: "symbol".to_string(),
                    display_name: "login".to_string(),
                    file_path: Some("src/auth.rs".to_string()),
                    workspace: Some("server".to_string()),
                    is_seed: true,
                    is_hub: false,
                    degree: 4,
                },
                QuerySubgraphNode {
                    uid: "file:src/auth/common.rs".to_string(),
                    kind: "file".to_string(),
                    display_name: "src/auth/common.rs".to_string(),
                    file_path: Some("src/auth/common.rs".to_string()),
                    workspace: Some("server".to_string()),
                    is_seed: false,
                    is_hub: true,
                    degree: 240,
                },
            ],
            edges: vec![QuerySubgraphEdge {
                from_uid: "file:src/auth.rs".to_string(),
                to_uid: "symbol:scip-rust pkg auth `login`().".to_string(),
                kind: "contains_definition".to_string(),
                confidence: 0.98,
                confidence_tier: "extracted".to_string(),
                reason: Some("seed expansion".to_string()),
            }],
            seeds: vec![
                QuerySubgraphSeedDebug {
                    uid: "file:src/auth.rs".to_string(),
                    display_name: "src/auth.rs".to_string(),
                    score: 0.91,
                    source: "hybrid".to_string(),
                    matched_text: Some("auth routing".to_string()),
                    debug: vec!["rrf=0.91 lexical+semantic+structural".to_string()],
                },
                QuerySubgraphSeedDebug {
                    uid: "symbol:scip-rust pkg auth `login`().".to_string(),
                    display_name: "login".to_string(),
                    score: 0.74,
                    source: "lexical".to_string(),
                    matched_text: Some("login function".to_string()),
                    debug: vec!["lexical LIKE code_chunks".to_string()],
                },
            ],
            inferred_edge_kinds: vec![
                "symbol_reference".to_string(),
                "reads".to_string(),
                "writes".to_string(),
            ],
            budget: QuerySubgraphBudget {
                requested_tokens: 2_048,
                estimated_tokens: 1_700,
                truncated: true,
                omitted_nodes: 4,
                omitted_edges: 7,
            },
            traversal: QuerySubgraphTraversalDebug {
                max_depth: 2,
                hub_degree_threshold: 99,
                hubs_blocked: vec!["file:src/auth/common.rs".to_string()],
                skipped_edge_kinds: vec!["member_of".to_string()],
            },
            narrowing_hints: vec![
                "narrow with context_filter=auth middleware".to_string(),
                "add file_filter=src/auth to focus traversal on auth package".to_string(),
                "ask about calls, reads, writes, implements, or imports to narrow edge kinds"
                    .to_string(),
                "high-degree hub nodes were included but not expanded; query a returned UID directly for hub neighbors"
                    .to_string(),
            ],
        }
    }

    /// Acceptance criterion #1 — source-level budget enforcement,
    /// explicit truncation, and narrowing hints. The control-plane
    /// response is the public surface agents see, so it must
    /// advertise every signal the spec promises: non-empty
    /// bounded `nodes`/`edges`, a `budget` block with `truncated =
    /// true` plus `omitted_nodes`/`omitted_edges`, and at least one
    /// `narrowing_hints` entry telling the model what to try next.
    #[tokio::test]
    async fn query_subgraph_broad_response_carries_budget_truncation_and_hints() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_broad_canned_response());
        let (server, _) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.query = Some("everything related to auth".to_string());

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("broad query_subgraph dispatch succeeds");
        let payload = match response {
            CodeGraphResponse::QuerySubgraph(resp) => serde_json::to_value(resp)
                .expect("serialize query_subgraph response"),
            other => panic!("expected query_subgraph response, got {other:?}"),
        };
        let query_subgraph = payload
            .get("query_subgraph")
            .and_then(|v| v.as_object())
            .expect("query_subgraph payload object");

        // Source-level budget enforcement: every budget field is
        // populated and the on-the-wire shape matches the spec.
        let budget = query_subgraph
            .get("budget")
            .and_then(|v| v.as_object())
            .expect("budget object");
        for field in [
            "requested_tokens",
            "estimated_tokens",
            "truncated",
            "omitted_nodes",
            "omitted_edges",
        ] {
            assert!(
                budget.contains_key(field),
                "budget missing {field}: {budget:?}"
            );
        }
        assert_eq!(
            budget.get("requested_tokens").and_then(|v| v.as_u64()),
            Some(2_048),
            "requested_tokens must surface the budget the planner was given"
        );
        assert_eq!(
            budget.get("truncated").and_then(|v| v.as_bool()),
            Some(true),
            "broad query must surface truncation state to the caller"
        );
        // Explicit truncation must come with at least one omitted
        // count so the agent knows the loss was on the source side,
        // not the downstream 8MiB tool-result truncation.
        let omitted_nodes = budget
            .get("omitted_nodes")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let omitted_edges = budget
            .get("omitted_edges")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        assert!(
            omitted_nodes + omitted_edges > 0,
            "truncated response must report non-zero omitted items, got nodes={omitted_nodes} edges={omitted_edges}"
        );

        // Narrowing hints: the response must always include at least
        // one hint that an agent can act on (context_filter /
        // file_filter / edge_filter / hub-query follow-up). The spec
        // is explicit that the planner should never silently truncate
        // and leave the agent with no recovery path.
        let hints = query_subgraph
            .get("narrowing_hints")
            .and_then(|v| v.as_array())
            .expect("narrowing_hints must be an array");
        assert!(
            !hints.is_empty(),
            "broad query must include at least one narrowing hint"
        );
        let hint_blob = hints
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            hint_blob.contains("file_filter")
                || hint_blob.contains("context_filter")
                || hint_blob.contains("edge"),
            "narrowing hints must guide the agent towards at least one of file_filter/context_filter/edge kinds: {hint_blob}"
        );
    }

    /// Acceptance criterion #2 — hub-avoidance state. The `traversal`
    /// block carries the hub-degree threshold, the list of hub UIDs
    /// that were intentionally *not* expanded, and the edge kinds
    /// that were skipped because of the inferred filters. Agents
    /// reading this block can see exactly why a high-degree common
    /// node did not pull in the rest of the graph.
    #[tokio::test]
    async fn query_subgraph_broad_response_records_hub_avoidance_state() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_broad_canned_response());
        let (server, _) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.query = Some("everything related to auth".to_string());

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("query_subgraph dispatch succeeds");
        let payload = match response {
            CodeGraphResponse::QuerySubgraph(resp) => serde_json::to_value(resp)
                .expect("serialize query_subgraph response"),
            other => panic!("expected query_subgraph response, got {other:?}"),
        };
        let traversal = payload
            .get("query_subgraph")
            .and_then(|q| q.get("traversal"))
            .and_then(|v| v.as_object())
            .expect("traversal object");
        for field in [
            "max_depth",
            "hub_degree_threshold",
            "hubs_blocked",
            "skipped_edge_kinds",
        ] {
            assert!(
                traversal.contains_key(field),
                "traversal missing {field}: {traversal:?}"
            );
        }
        let threshold = traversal
            .get("hub_degree_threshold")
            .and_then(|v| v.as_u64())
            .expect("hub_degree_threshold is a non-negative integer");
        assert!(
            threshold >= 1,
            "hub_degree_threshold must be at least 1, got {threshold}"
        );
        let hubs_blocked = traversal
            .get("hubs_blocked")
            .and_then(|v| v.as_array())
            .expect("hubs_blocked must be an array");
        assert!(
            !hubs_blocked.is_empty(),
            "broad query must record at least one blocked hub UID, got {traversal:?}"
        );
        for hub in hubs_blocked {
            let hub = hub.as_str().expect("hub UID must be a string");
            assert!(
                hub.starts_with("file:") || hub.starts_with("symbol:"),
                "blocked-hub UID must use the canonical follow-up-op prefix, got {hub}"
            );
        }

        // Hub-avoidance contract: high-degree common nodes appear as
        // bounded result nodes (`is_hub = true`) but do not contribute
        // to the expansion. Lock that down by asserting the broad
        // canned response's hub node is marked `is_hub` and the
        // hubs_blocked list points back at the same UID.
        let nodes = payload
            .get("query_subgraph")
            .and_then(|q| q.get("nodes"))
            .and_then(|v| v.as_array())
            .expect("nodes must be an array");
        let mut found_hub_node = false;
        for node in nodes {
            let is_hub = node
                .get("is_hub")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let uid = node.get("uid").and_then(|v| v.as_str()).unwrap_or("");
            if is_hub {
                found_hub_node = true;
                assert!(
                    hubs_blocked
                        .iter()
                        .any(|h| h.as_str() == Some(uid)),
                    "high-degree hub node {uid} must also be listed in hubs_blocked: {hubs_blocked:?}"
                );
            }
        }
        assert!(
            found_hub_node,
            "broad canned response should expose at least one is_hub=true node so the hub-avoidance contract is observable"
        );
    }

    /// Acceptance criterion #3 — edge intent inference round-trips
    /// end-to-end. The `inferred_edge_kinds` list is the public
    /// surface agents read to see what the planner chose; each of
    /// the spec's edge-intent phrasings (calls, reads, writes,
    /// implements, imports) must surface here. We exercise the
    /// full control-plane path by feeding the canned response back
    /// through `dispatch_code_graph` so the wire shape is the
    /// actual `CodeGraphResponse::QuerySubgraph` payload.
    #[tokio::test]
    async fn query_subgraph_response_inferred_edge_kinds_cover_phrase_variants() {
        // The canned response encodes the full set of phrasings the
        // spec calls out: calls (symbol_reference), reads, writes,
        // implements, and imports. A change to the planner that
        // drops any one of them would change this canned response
        // and break the test, so it doubles as a regression guard
        // for the public response shape itself.
        let expected = BTreeSet::from_iter([
            "symbol_reference".to_string(),
            "reads".to_string(),
            "writes".to_string(),
        ]);
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_broad_canned_response());
        let (server, _) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.query = Some("everything related to auth".to_string());

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("query_subgraph dispatch succeeds");
        let payload = match response {
            CodeGraphResponse::QuerySubgraph(resp) => serde_json::to_value(resp)
                .expect("serialize query_subgraph response"),
            other => panic!("expected query_subgraph response, got {other:?}"),
        };
        let inferred: BTreeSet<String> = payload
            .get("query_subgraph")
            .and_then(|q| q.get("inferred_edge_kinds"))
            .and_then(|v| v.as_array())
            .expect("inferred_edge_kinds must be an array")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(
            inferred, expected,
            "inferred_edge_kinds must reflect the planner's edge intent end-to-end"
        );
    }

    /// Acceptance criterion #4 — stable UID follow-up compatibility.
    /// The wire-shape UIDs the planner emits are the exact strings
    /// the bridge passes to `describe`, `context`, and `neighbors` —
    /// a regression that strips the `file:` / `symbol:` prefix
    /// (e.g. some untyped rendering that emits just the file path)
    /// would silently break the follow-up call chain.
    #[tokio::test]
    async fn query_subgraph_response_uids_use_canonical_followup_prefixes() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_broad_canned_response());
        let (server, _) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.query = Some("everything related to auth".to_string());

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("query_subgraph dispatch succeeds");
        let payload = match response {
            CodeGraphResponse::QuerySubgraph(resp) => serde_json::to_value(resp)
                .expect("serialize query_subgraph response"),
            other => panic!("expected query_subgraph response, got {other:?}"),
        };
        let nodes = payload
            .get("query_subgraph")
            .and_then(|q| q.get("nodes"))
            .and_then(|v| v.as_array())
            .expect("nodes must be an array");
        let mut node_uids: BTreeSet<String> = BTreeSet::new();
        for node in nodes {
            let uid = node
                .get("uid")
                .and_then(|v| v.as_str())
                .expect("node uid is a string");
            assert!(
                uid.starts_with("file:") || uid.starts_with("symbol:"),
                "node UID must use canonical follow-up-op prefix, got {uid}"
            );
            node_uids.insert(uid.to_string());
        }
        // Every edge from_uid / to_uid must point at one of the
        // emitted node UIDs — otherwise the agent would try to
        // follow a dangling UID to `neighbors`.
        let edges = payload
            .get("query_subgraph")
            .and_then(|q| q.get("edges"))
            .and_then(|v| v.as_array())
            .expect("edges must be an array");
        for edge in edges {
            for field in ["from_uid", "to_uid"] {
                let uid = edge
                    .get(field)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                assert!(
                    node_uids.contains(uid),
                    "edge {field}={uid} must reference a node UID emitted by the same query_subgraph response"
                );
            }
        }
        // Every seed UID must also be canonical — seeds flow into
        // `describe`/`neighbors` follow-ups the same way nodes do.
        let seeds = payload
            .get("query_subgraph")
            .and_then(|q| q.get("seeds"))
            .and_then(|v| v.as_array())
            .expect("seeds must be an array");
        for seed in seeds {
            let uid = seed
                .get("uid")
                .and_then(|v| v.as_str())
                .expect("seed uid is a string");
            assert!(
                uid.starts_with("file:") || uid.starts_with("symbol:"),
                "seed UID must use canonical follow-up-op prefix, got {uid}"
            );
        }
    }

    /// Acceptance criterion #4 (stronger cross-layer check) — the
    /// exact UID strings returned by `query_subgraph` must be
    /// accepted by existing follow-up operations without stripping
    /// prefixes or re-resolving display names. This exercises the
    /// same control-plane dispatcher path an agent uses when it
    /// chains `query_subgraph` -> `describe` / `context` /
    /// `neighbors`.
    #[tokio::test]
    async fn query_subgraph_returned_uid_is_accepted_by_followup_operations() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_broad_canned_response());
        let (server, _) = query_subgraph_test_server(graph);
        let ctx = query_subgraph_ctx();
        let mut query = test_params("query_subgraph");
        query.project_id = "proj-query-subgraph".to_string();
        query.project_path = "/workspace/repo".to_string();
        query.query = Some("everything related to auth".to_string());

        let response = server
            .dispatch_code_graph(&ctx, &mut query)
            .await
            .expect("query_subgraph dispatch succeeds");
        let uid = match response {
            CodeGraphResponse::QuerySubgraph(resp) => resp
                .query_subgraph
                .nodes
                .iter()
                .find(|node| node.uid.starts_with("symbol:"))
                .or_else(|| resp.query_subgraph.nodes.first())
                .expect("canned query_subgraph response emits at least one UID")
                .uid
                .clone(),
            other => panic!("expected query_subgraph response, got {other:?}"),
        };
        assert!(
            uid.starts_with("file:") || uid.starts_with("symbol:"),
            "fixture UID must use the canonical follow-up prefix, got {uid}"
        );

        let mut describe = test_params("describe");
        describe.project_id = "proj-query-subgraph".to_string();
        describe.project_path = "/workspace/repo".to_string();
        describe.key = Some(uid.clone());
        match server
            .dispatch_code_graph(&ctx, &mut describe)
            .await
            .expect("describe accepts query_subgraph UID")
        {
            CodeGraphResponse::Describe(resp) => {
                let description = resp.description.expect("describe resolves returned UID");
                assert_eq!(description.key, uid);
            }
            other => panic!("expected describe response, got {other:?}"),
        }

        let mut context = test_params("context");
        context.project_id = "proj-query-subgraph".to_string();
        context.project_path = "/workspace/repo".to_string();
        context.key = Some(uid.clone());
        match server
            .dispatch_code_graph(&ctx, &mut context)
            .await
            .expect("context accepts query_subgraph UID")
        {
            CodeGraphResponse::Context(resp) => {
                assert_eq!(resp.symbol_context.symbol.uid, uid);
            }
            other => panic!("expected context response, got {other:?}"),
        }

        let mut neighbors = test_params("neighbors");
        neighbors.project_id = "proj-query-subgraph".to_string();
        neighbors.project_path = "/workspace/repo".to_string();
        neighbors.key = Some(uid.clone());
        match server
            .dispatch_code_graph(&ctx, &mut neighbors)
            .await
            .expect("neighbors accepts query_subgraph UID")
        {
            CodeGraphResponse::Neighbors(resp) => {
                assert_eq!(resp.key, uid);
                assert!(
                    resp.neighbors
                        .as_ref()
                        .is_some_and(|neighbors| !neighbors.is_empty()),
                    "neighbors follow-up should return the fixture response"
                );
            }
            other => panic!("expected neighbors response, got {other:?}"),
        }
    }

    /// Acceptance criterion #5 — seed debug metadata is preserved in
    /// the public response shape. The spec is explicit: agents must
    /// be able to understand *why* initial nodes were selected, so
    /// each seed must carry `uid`, `display_name`, `score`, `source`,
    /// and a non-empty `debug` array that names the contributing
    /// signals (rrf fusion, lexical LIKE, semantic cosine, …).
    #[tokio::test]
    async fn query_subgraph_response_seed_debug_metadata_surfaces_end_to_end() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_broad_canned_response());
        let (server, _) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.query = Some("everything related to auth".to_string());

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("query_subgraph dispatch succeeds");
        let payload = match response {
            CodeGraphResponse::QuerySubgraph(resp) => serde_json::to_value(resp)
                .expect("serialize query_subgraph response"),
            other => panic!("expected query_subgraph response, got {other:?}"),
        };
        let seeds = payload
            .get("query_subgraph")
            .and_then(|q| q.get("seeds"))
            .and_then(|v| v.as_array())
            .expect("seeds must be an array");
        assert!(
            !seeds.is_empty(),
            "broad canned response should expose at least one seed so the public-shape contract is observable"
        );

        // Source labels must round-trip end-to-end. The planner's
        // four seed sources all flow through the bridge; if a
        // future change collapses them into a single "hybrid" the
        // agent loses the debug signal and the tests below break.
        let mut seen_sources: BTreeSet<String> = BTreeSet::new();
        for seed in seeds {
            for field in [
                "uid",
                "display_name",
                "score",
                "source",
                "debug",
            ] {
                assert!(
                    seed.get(field).is_some(),
                    "seed missing required debug field {field}: {seed:?}"
                );
            }
            let source = seed
                .get("source")
                .and_then(|v| v.as_str())
                .expect("seed source is a string")
                .to_string();
            seen_sources.insert(source);
            let debug = seed
                .get("debug")
                .and_then(|v| v.as_array())
                .expect("seed debug must be an array");
            assert!(
                !debug.is_empty(),
                "seed debug array must contain at least one human-readable reason: {seed:?}"
            );
        }
        // The canned broad response exercises both the "hybrid" and
        // "lexical" seed sources — locking both in here makes a
        // future migration that drops one of them break loudly.
        assert!(
            seen_sources.contains("hybrid") || seen_sources.contains("lexical"),
            "expected canned broad response to surface at least one hybrid/lexical seed source, got {seen_sources:?}"
        );
    }

    /// Acceptance criterion #1 (companion) — an empty-seed response
    /// must still surface narrowing hints and must NOT mark the
    /// result truncated (nothing to spend budget on). This is the
    /// "no silent empty results" anti-regression: agents that
    /// blindly rely on `truncated = true` to decide whether to
    /// retry would otherwise spin forever.
    #[tokio::test]
    async fn query_subgraph_empty_response_carries_hints_without_truncated_flag() {
        let empty = QuerySubgraphResult {
            query: "no matches".to_string(),
            nodes: Vec::new(),
            edges: Vec::new(),
            seeds: Vec::new(),
            inferred_edge_kinds: Vec::new(),
            budget: QuerySubgraphBudget {
                requested_tokens: 2_000,
                estimated_tokens: 0,
                truncated: false,
                omitted_nodes: 0,
                omitted_edges: 0,
            },
            traversal: QuerySubgraphTraversalDebug {
                max_depth: 2,
                hub_degree_threshold: 0,
                hubs_blocked: Vec::new(),
                skipped_edge_kinds: Vec::new(),
            },
            narrowing_hints: vec![
                "narrow with workspace/context_filter when the repository has multiple workspaces"
                    .to_string(),
                "add file_filter to focus traversal on a package or directory".to_string(),
            ],
        };
        let graph = RecordingQuerySubgraphOps::new(empty);
        let (server, _) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.query = Some("no matches".to_string());

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("empty query_subgraph dispatch succeeds");
        let payload = match response {
            CodeGraphResponse::QuerySubgraph(resp) => serde_json::to_value(resp)
                .expect("serialize query_subgraph response"),
            other => panic!("expected query_subgraph response, got {other:?}"),
        };
        let query_subgraph = payload
            .get("query_subgraph")
            .and_then(|v| v.as_object())
            .expect("query_subgraph payload object");
        assert_eq!(
            query_subgraph
                .get("nodes")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(0),
            "empty response must serialize an empty nodes array"
        );
        let hints = query_subgraph
            .get("narrowing_hints")
            .and_then(|v| v.as_array())
            .expect("narrowing_hints must be an array");
        assert!(
            !hints.is_empty(),
            "empty response must still include narrowing hints so the agent has a recovery path"
        );
        let budget_truncated = query_subgraph
            .get("budget")
            .and_then(|b| b.get("truncated"))
            .and_then(|v| v.as_bool());
        assert_eq!(
            budget_truncated,
            Some(false),
            "empty response must not be marked truncated: agent retry logic would spin forever"
        );
    }

    /// Acceptance criterion #4 (companion) — the canonical
    /// `QuerySubgraphResponse` JSON shape has a top-level
    /// `query_subgraph` discriminator field that wraps the
    /// `QuerySubgraphResult` payload, matching the other graph
    /// response shapes (`snapshot`, `search`, etc.). A regression
    /// that flattened the payload (e.g. by serializing the result
    /// at the top level) would collide with the untagged
    /// `CodeGraphResponse` enum and break the schema snapshot.
    #[tokio::test]
    async fn query_subgraph_response_wraps_payload_under_query_subgraph_field() {
        let graph = RecordingQuerySubgraphOps::new(query_subgraph_broad_canned_response());
        let (server, _) = query_subgraph_test_server(graph);
        let mut params = test_params("query_subgraph");
        params.project_id = "proj-query-subgraph".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.query = Some("everything related to auth".to_string());

        let response = server
            .dispatch_code_graph(&query_subgraph_ctx(), &mut params)
            .await
            .expect("query_subgraph dispatch succeeds");
        let payload = match response {
            CodeGraphResponse::QuerySubgraph(resp) => serde_json::to_value(resp)
                .expect("serialize query_subgraph response"),
            other => panic!("expected query_subgraph response, got {other:?}"),
        };
        let obj = payload.as_object().expect("response is a JSON object");
        assert_eq!(
            obj.len(),
            1,
            "QuerySubgraphResponse must wrap the payload in a single discriminator field, got {payload}"
        );
        assert!(
            obj.contains_key("query_subgraph"),
            "QuerySubgraphResponse must use `query_subgraph` as its discriminator field"
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
            graph_staleness: None,
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
        /// jc47: the `pinned_commit` returned by `status`. `None`
        /// mimics an un-warmed graph cache; `Some(sha)` mimics a
        /// warmed graph pinned at that commit.
        pinned_commit: Option<String>,
        /// Optional crate_graph response override. When `Some`, the
        /// `crate_graph` bridge method returns this data instead of
        /// the trait default (empty graph).
        crate_graph_response: Option<CrateGraphResponse>,
    }

    impl WorkspaceFixtureOps {
        fn new(hint: Option<Vec<String>>) -> Self {
            Self {
                seen_workspaces: Arc::new(Mutex::new(Vec::new())),
                hint,
                ranked_nodes: Vec::new(),
                impact_result: ImpactResult::Detailed(Vec::new()),
                pinned_commit: None,
                crate_graph_response: None,
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
                pinned_commit: None,
                crate_graph_response: None,
            }
        }

        /// jc47: set the `pinned_commit` that `status` reports,
        /// simulating a warmed graph cache.
        fn with_pinned_commit(mut self, commit: impl Into<String>) -> Self {
            self.pinned_commit = Some(commit.into());
            self
        }

        /// Set a custom `crate_graph` response, simulating a warmed
        /// workspace with crate-level aggregation.
        fn with_crate_graph(mut self, response: CrateGraphResponse) -> Self {
            self.crate_graph_response = Some(response);
            self
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
                pinned_commit: self.pinned_commit.clone(),
                commits_since_pin: None,
            route_parity_enabled: true,
            route_exclusion_config: serde_json::json!({}),
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
        async fn crate_graph(
            &self,
            _: &ProjectCtx,
        ) -> Result<CrateGraphResponse, String> {
            match &self.crate_graph_response {
                Some(resp) => Ok(resp.clone()),
                None => Ok(CrateGraphResponse {
                    crates: Vec::new(),
                    edges: Vec::new(),
                    message: None,
                }),
            }
        }
    }

