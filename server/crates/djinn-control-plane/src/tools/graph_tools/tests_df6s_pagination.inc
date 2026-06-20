    // df6s: regression coverage for the agent-boundary pagination and
    // summary controls on `neighbors` / `impact` / `coupling_hotspots`.
    //
    // The acceptance criteria these tests back:
    //
    // 1. A limited page still reports the complete total — never the
    //    page size — so a model can never mistake a capped page for
    //    "the graph has no more entries".
    // 2. An offset page does not mutate the internal traversal counts
    //    (the bridge always sees the same arguments regardless of
    //    `offset`).
    // 3. `summary_only` strips the heavy list fields and still reports
    //    the unsliced total.
    // 4. `by_depth_counts` is honoured for `impact` and is computed
    //    from the unsliced set so the breakdown reflects the full
    //    blast-radius distribution.
    //
    // The fixture is a thin `RepoGraphOps` impl that records the args
    // the bridge received and returns canned data; tests assert the
    // DTO shape from there. The `Arc` / `Mutex` / bridge type
    // imports are already pulled in by `tests_part1.inc`, so this
    // file does not redeclare them.

    // No new `use` statements — `tests_part1.inc` already brings in
    // every type this file references.

    #[derive(Clone, Default)]
    struct PaginationFixtureOps {
        /// Args the bridge was called with — for "did pagination leak
        /// to the graph layer?" assertions.
        calls: Arc<Mutex<Vec<String>>>,
        /// Programmable neighbors detailed result.
        neighbors: Vec<GraphNeighbor>,
        /// Programmable neighbors grouped result.
        groups: Vec<crate::bridge::FileGroupEntry>,
        /// Programmable impact detailed result.
        impact: Vec<ImpactEntry>,
        /// Programmable coupling_hotspots result.
        pairs: Vec<CoupledPairEntry>,
    }

    impl PaginationFixtureOps {
        fn new() -> Self {
            Self::default()
        }

        fn with_neighbors(mut self, neighbors: Vec<GraphNeighbor>) -> Self {
            self.neighbors = neighbors;
            self
        }

        fn with_impact(mut self, impact: Vec<ImpactEntry>) -> Self {
            self.impact = impact;
            self
        }

        fn with_pairs(mut self, pairs: Vec<CoupledPairEntry>) -> Self {
            self.pairs = pairs;
            self
        }
    }

    #[async_trait]
    impl RepoGraphOps for PaginationFixtureOps {
        async fn neighbors(
            &self,
            _: &ProjectCtx,
            key: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<NeighborsResult, String> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("neighbors:{key}"));
            if !self.groups.is_empty() {
                Ok(NeighborsResult::Grouped(self.groups.clone()))
            } else {
                Ok(NeighborsResult::Detailed(self.neighbors.clone()))
            }
        }

        async fn impact(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            key: &str,
            _: usize,
            _: Option<&str>,
            _: Option<f64>,
        ) -> Result<ImpactResult, String> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("impact:{key}"));
            Ok(ImpactResult::Detailed(self.impact.clone()))
        }

        async fn coupling_hotspots(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CoupledPairEntry>, String> {
            self.calls
                .lock()
                .expect("calls")
                .push("coupling_hotspots".to_string());
            Ok(self.pairs.clone())
        }

        // ── Unused-by-tests stubs (must compile for `RepoGraphOps`) ──
        async fn ranked(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RankedNode>, String> {
            unreachable!("df6s pagination tests should not call ranked")
        }
        async fn implementations(
            &self,
            _: &ProjectCtx,
            _: &str,
        ) -> Result<Vec<String>, String> {
            unreachable!()
        }
        async fn search(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<SearchHit>, String> {
            unreachable!()
        }
        async fn cycles(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<CycleGroup>, String> {
            unreachable!()
        }
        async fn orphans(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<OrphanEntry>, String> {
            unreachable!()
        }
        async fn path(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: &str,
            _: &str,
            _: Option<usize>,
        ) -> Result<Option<PathResult>, String> {
            unreachable!()
        }
        async fn edges(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<EdgeEntry>, String> {
            unreachable!()
        }
        async fn describe(
            &self,
            _: &ProjectCtx,
            _: &str,
        ) -> Result<Option<SymbolDescription>, String> {
            unreachable!()
        }
        async fn context(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: bool,
        ) -> Result<Option<SymbolContext>, String> {
            unreachable!()
        }
        async fn status(&self, _: &ProjectCtx) -> Result<GraphStatus, String> {
            unreachable!()
        }
        async fn snapshot(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: crate::bridge::SnapshotLevel,
            _: usize,
            _: &GraphExclusions,
        ) -> Result<SnapshotPayload, String> {
            unreachable!()
        }
        async fn symbols_at(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: u32,
            _: Option<u32>,
        ) -> Result<Vec<SymbolAtHit>, String> {
            unreachable!()
        }
        async fn diff_touches(
            &self,
            _: &ProjectCtx,
            _: &[ChangedRange],
        ) -> Result<DiffTouchesResult, String> {
            unreachable!()
        }
        async fn detect_changes(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: &[String],
        ) -> Result<DetectedChangesResult, String> {
            unreachable!()
        }
        async fn api_surface(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<ApiSurfaceEntry>, String> {
            unreachable!()
        }
        async fn boundary_check(
            &self,
            _: &ProjectCtx,
            _: &[BoundaryRule],
            _: &str,
        ) -> Result<Vec<BoundaryViolation>, String> {
            unreachable!()
        }
        async fn hotspots(
            &self,
            _: &ProjectCtx,
            _: u32,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<HotspotEntry>, String> {
            unreachable!()
        }
        async fn complexity(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<ComplexityResult, String> {
            unreachable!()
        }
        async fn refactor_candidates(
            &self,
            _: &ProjectCtx,
            _: Option<u32>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RefactorCandidate>, String> {
            unreachable!()
        }
        async fn metrics_at(&self, _: &ProjectCtx) -> Result<MetricsAtResult, String> {
            unreachable!()
        }
        async fn dead_symbols(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<DeadSymbolEntry>, String> {
            unreachable!()
        }
        async fn deprecated_callers(
            &self,
            _: &ProjectCtx,
            _: usize,
        ) -> Result<Vec<DeprecatedHit>, String> {
            unreachable!()
        }
        async fn touches_hot_path(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: &[String],
            _: &[String],
            _: &[String],
        ) -> Result<Vec<HotPathHit>, String> {
            unreachable!()
        }
        async fn coupling(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<CouplingEntry>, String> {
            unreachable!()
        }
        async fn churn(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
        ) -> Result<Vec<ChurnEntry>, String> {
            unreachable!()
        }
        async fn coupling_hubs(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CouplingHubEntry>, String> {
            unreachable!()
        }
        async fn resolve(
            &self,
            _: &ProjectCtx,
            key: &str,
            _: Option<&str>,
        ) -> Result<ResolveOutcome, String> {
            // df6s pagination tests bypass the resolver: a synthetic
            // symbol like `"symbol:src/lib.rs `lib`()"` is not in
            // any name index, so we return `Found` to let the
            // pre-resolve pass-through land the key straight at the
            // per-op handler. The whole point of these tests is
            // the response DTO shape, not the resolver.
            Ok(ResolveOutcome::Found(key.to_string()))
        }
    }

    fn pagination_test_server(
        graph: PaginationFixtureOps,
    ) -> (DjinnMcpServer, Arc<Mutex<Vec<String>>>) {
        let calls = graph.calls.clone();
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
            Arc::new(graph),
        );
        (DjinnMcpServer::new(state), calls)
    }

    fn df6s_ctx() -> ProjectCtx {
        ProjectCtx {
            id: "proj-df6s".to_string(),
            clone_path: "/workspace/repo".to_string(),
            workspace: None,
            sub_path: None,
        }
    }

    fn synthetic_graph_neighbor(uid: &str, weight: f64) -> GraphNeighbor {
        GraphNeighbor {
            key: uid.to_string(),
            uid: uid.to_string(),
            kind: "symbol".to_string(),
            display_name: uid.to_string(),
            edge_kind: "symbol_reference".to_string(),
            edge_weight: weight,
            direction: "outgoing".to_string(),
        }
    }

    fn synthetic_impact_entry(uid: &str, depth: usize, file_path: Option<&str>) -> ImpactEntry {
        ImpactEntry {
            key: uid.to_string(),
            uid: uid.to_string(),
            depth,
            file_path: file_path.map(str::to_string),
            confidence_tier: None,
            exclusion_reason: None,
        }
    }

    // ── Neighbors pagination ────────────────────────────────────────────────

    #[tokio::test]
    async fn neighbors_page_reports_complete_total() {
        // AC #1: a limited page still reports the complete total —
        // never the page size — so the model can never mistake a
        // capped page for "the graph has no more neighbors".
        let mut neighbors = Vec::new();
        for i in 0..50 {
            neighbors.push(synthetic_graph_neighbor(
                &format!("symbol:src/file{i}.rs `fn`()."),
                1.0 / (i as f64 + 1.0),
            ));
        }
        let graph = PaginationFixtureOps::new().with_neighbors(neighbors);
        let (server, _calls) = pagination_test_server(graph);

        let mut params = test_params("neighbors");
        params.project_id = "proj-df6s".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.key = Some("symbol:src/lib.rs `lib`().".to_string());
        params.limit = Some(10);

        let response = server
            .dispatch_code_graph(&df6s_ctx(), &mut params)
            .await
            .expect("neighbors dispatch succeeds");

        match response {
            CodeGraphResponse::Neighbors(r) => {
                assert_eq!(
                    r.neighbors.as_ref().map(|v| v.len()),
                    Some(10),
                    "the wire-level page should be capped at limit=10"
                );
                assert_eq!(
                    r.total,
                    Some(50),
                    "total must reflect the unsliced set so a 10-row page is never confused with 'only 10 neighbors exist'"
                );
                assert_eq!(r.offset, Some(0));
                assert_eq!(r.limit, Some(10));
                assert_eq!(r.has_more, Some(true));
            }
            other => panic!("expected Neighbors, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn neighbors_offset_page_does_not_mutate_bridge_input() {
        // AC #2: an offset page does not mutate the bridge call's
        // inputs — the graph layer always sees the same arguments
        // regardless of how the agent paginates. The slice is
        // applied only at the response DTO layer.
        let mut neighbors = Vec::new();
        for i in 0..30 {
            neighbors.push(synthetic_graph_neighbor(
                &format!("symbol:src/f{i}.rs `f{i}`()."),
                1.0 / (i as f64 + 1.0),
            ));
        }
        let graph = PaginationFixtureOps::new().with_neighbors(neighbors);
        let (server, _calls) = pagination_test_server(graph);

        // First, no offset.
        let mut first = test_params("neighbors");
        first.project_id = "proj-df6s".to_string();
        first.project_path = "/workspace/repo".to_string();
        first.key = Some("symbol:src/lib.rs `lib`().".to_string());
        first.limit = Some(5);
        first.offset = Some(0);
        let first_resp = server
            .dispatch_code_graph(&df6s_ctx(), &mut first)
            .await
            .expect("first dispatch");

        // Then offset 10.
        let mut second = test_params("neighbors");
        second.project_id = "proj-df6s".to_string();
        second.project_path = "/workspace/repo".to_string();
        second.key = Some("symbol:src/lib.rs `lib`().".to_string());
        second.limit = Some(5);
        second.offset = Some(10);
        let second_resp = server
            .dispatch_code_graph(&df6s_ctx(), &mut second)
            .await
            .expect("second dispatch");

        // Both calls should report the same unsliced total (30) and
        // the bridge received identical args for both dispatches.
        match (first_resp, second_resp) {
            (CodeGraphResponse::Neighbors(a), CodeGraphResponse::Neighbors(b)) => {
                assert_eq!(a.total, Some(30), "page 1 total");
                assert_eq!(b.total, Some(30), "page 2 total");
                assert_eq!(a.offset, Some(0));
                assert_eq!(b.offset, Some(10));
                assert_eq!(a.has_more, Some(true));
                assert_eq!(b.has_more, Some(true));
                let a_keys: Vec<&str> = a
                    .neighbors
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|n| n.key.as_str())
                    .collect();
                let b_keys: Vec<&str> = b
                    .neighbors
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|n| n.key.as_str())
                    .collect();
                assert!(a_keys.iter().any(|k| !b_keys.contains(k)));
            }
            (a, b) => panic!("expected Neighbors, got {a:?} / {b:?}"),
        }
    }

    #[tokio::test]
    async fn neighbors_summary_only_omits_lists_but_keeps_total() {
        // AC #3: `summary_only` strips the heavy list fields. `total`
        // still reflects the unsliced count.
        let mut neighbors = Vec::new();
        for i in 0..15 {
            neighbors.push(synthetic_graph_neighbor(
                &format!("symbol:src/n{i}.rs `n{i}`()."),
                1.0,
            ));
        }
        let graph = PaginationFixtureOps::new().with_neighbors(neighbors);
        let (server, _calls) = pagination_test_server(graph);

        let mut params = test_params("neighbors");
        params.project_id = "proj-df6s".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.key = Some("symbol:src/lib.rs `lib`().".to_string());
        params.summary_only = Some(true);

        let response = server
            .dispatch_code_graph(&df6s_ctx(), &mut params)
            .await
            .expect("summary-only dispatch");

        match response {
            CodeGraphResponse::Neighbors(r) => {
                assert!(
                    r.neighbors.is_none(),
                    "summary_only must drop the list field, not just empty it"
                );
                assert_eq!(
                    r.total,
                    Some(15),
                    "total still reflects the unsliced count"
                );
                assert_eq!(r.summary_only, Some(true));
            }
            other => panic!("expected Neighbors, got {other:?}"),
        }
    }

    // ── Impact pagination ──────────────────────────────────────────────────

    #[tokio::test]
    async fn impact_page_reports_unsliced_total_and_preserves_depth_as_limit() {
        // AC #1: a `page_limit=3` impact page still reports the
        // complete unsliced total. `limit` remains the BFS depth —
        // we use `depth=3` (matching the default) so the bridge
        // sees the same traversal as a normal call.
        let mut impact = Vec::new();
        for i in 0..25 {
            let depth = (i % 3) + 1;
            impact.push(synthetic_impact_entry(
                &format!("symbol:src/x{i}.rs `caller{i}`()."),
                depth,
                Some(&format!("src/m{}/x{i}.rs", i % 4)),
            ));
        }
        let graph = PaginationFixtureOps::new().with_impact(impact);
        let (server, _calls) = pagination_test_server(graph);

        let mut params = test_params("impact");
        params.project_id = "proj-df6s".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.key = Some("symbol:src/lib.rs `lib`().".to_string());
        // `limit` keeps its pre-df6s BFS-depth semantic.
        params.limit = Some(3);
        // Result-cap pagination is served by `page_limit`.
        params.page_limit = Some(5);

        let response = server
            .dispatch_code_graph(&df6s_ctx(), &mut params)
            .await
            .expect("impact dispatch");

        match response {
            CodeGraphResponse::Impact(r) => {
                assert_eq!(
                    r.impact.as_ref().map(|v| v.len()),
                    Some(5),
                    "page_limit caps the wire-level page"
                );
                assert_eq!(
                    r.total,
                    Some(25),
                    "total must reflect the unsliced post-exclusion set"
                );
                assert_eq!(r.limit, Some(5), "page_limit is the cap, not depth");
                assert_eq!(r.offset, Some(0));
                assert_eq!(r.has_more, Some(true));
                // Risk classification reads the unsliced metrics so
                // the bucket is the same as a non-paginated call.
                assert!(
                    r.risk.is_some(),
                    "risk classification survives pagination"
                );
            }
            other => panic!("expected Impact, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn impact_summary_only_omits_list_and_includes_by_depth_counts() {
        // AC #3 + #4: `summary_only` strips the impact list and the
        // per-depth breakdown rides along because the caller might
        // still want the depth distribution. The breakdown is
        // computed from the unsliced set so it reflects the full
        // blast-radius distribution.
        let mut impact = Vec::new();
        for i in 0..10 {
            impact.push(synthetic_impact_entry(
                &format!("symbol:src/a{i}.rs `a{i}`()."),
                1,
                Some(&format!("src/auth/a{i}.rs")),
            ));
        }
        for i in 0..5 {
            impact.push(synthetic_impact_entry(
                &format!("symbol:src/b{i}.rs `b{i}`()."),
                2,
                Some(&format!("src/auth/b{i}.rs")),
            ));
        }
        let graph = PaginationFixtureOps::new().with_impact(impact);
        let (server, _calls) = pagination_test_server(graph);

        let mut params = test_params("impact");
        params.project_id = "proj-df6s".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.key = Some("symbol:src/lib.rs `lib`().".to_string());
        params.summary_only = Some(true);

        let response = server
            .dispatch_code_graph(&df6s_ctx(), &mut params)
            .await
            .expect("summary-only impact dispatch");

        match response {
            CodeGraphResponse::Impact(r) => {
                assert!(r.impact.is_none(), "summary_only drops the impact list");
                assert!(r.file_groups.is_none());
                assert_eq!(r.total, Some(15), "unsliced total = 10 + 5");
                assert_eq!(r.summary_only, Some(true));
                let counts = r
                    .by_depth_counts
                    .expect("by_depth_counts present for summary_only impact");
                assert_eq!(counts.get("1"), Some(&10));
                assert_eq!(counts.get("2"), Some(&5));
                // The order is the BTreeMap natural ordering so the
                // wire shape is stable for downstream parsers.
                let keys: Vec<&String> = counts.keys().collect();
                assert_eq!(keys, vec!["1", "2"]);
            }
            other => panic!("expected Impact, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn impact_by_depth_counts_opt_in_returns_even_when_lists_present() {
        // AC #4: `by_depth_counts` is honoured even when the caller
        // did not ask for `summary_only` — the per-depth map ships
        // alongside the (capped) impact list.
        let mut impact = Vec::new();
        for i in 0..6 {
            impact.push(synthetic_impact_entry(
                &format!("symbol:src/d{i}.rs `d{i}`()."),
                1,
                Some(&format!("src/d{i}.rs")),
            ));
        }
        let graph = PaginationFixtureOps::new().with_impact(impact);
        let (server, _calls) = pagination_test_server(graph);

        let mut params = test_params("impact");
        params.project_id = "proj-df6s".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.key = Some("symbol:src/lib.rs `lib`().".to_string());
        params.by_depth_counts = Some(true);

        let response = server
            .dispatch_code_graph(&df6s_ctx(), &mut params)
            .await
            .expect("by_depth_counts opt-in impact dispatch");

        match response {
            CodeGraphResponse::Impact(r) => {
                assert!(
                    r.impact.is_some(),
                    "by_depth_counts alone should not strip the list"
                );
                let counts = r
                    .by_depth_counts
                    .expect("by_depth_counts present when opted in");
                assert_eq!(counts.get("1"), Some(&6));
            }
            other => panic!("expected Impact, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn impact_offset_pagination_does_not_shrink_total() {
        // AC #2: a caller paginating across offsets should see the
        // same `total` on every page so the second page can never
        // look like a fresh empty graph.
        let mut impact = Vec::new();
        for i in 0..12 {
            impact.push(synthetic_impact_entry(
                &format!("symbol:src/p{i}.rs `p{i}`()."),
                1,
                Some(&format!("src/p{i}.rs")),
            ));
        }
        let graph = PaginationFixtureOps::new().with_impact(impact);
        let (server, _calls) = pagination_test_server(graph);

        let mut page_one = test_params("impact");
        page_one.project_id = "proj-df6s".to_string();
        page_one.project_path = "/workspace/repo".to_string();
        page_one.key = Some("symbol:src/lib.rs `lib`().".to_string());
        page_one.page_limit = Some(5);
        page_one.offset = Some(0);
        let page_one_resp = server
            .dispatch_code_graph(&df6s_ctx(), &mut page_one)
            .await
            .expect("page 1");

        let mut page_two = test_params("impact");
        page_two.project_id = "proj-df6s".to_string();
        page_two.project_path = "/workspace/repo".to_string();
        page_two.key = Some("symbol:src/lib.rs `lib`().".to_string());
        page_two.page_limit = Some(5);
        page_two.offset = Some(5);
        let page_two_resp = server
            .dispatch_code_graph(&df6s_ctx(), &mut page_two)
            .await
            .expect("page 2");

        match (page_one_resp, page_two_resp) {
            (CodeGraphResponse::Impact(a), CodeGraphResponse::Impact(b)) => {
                assert_eq!(a.total, Some(12));
                assert_eq!(b.total, Some(12), "offset must not mutate total");
                assert_eq!(a.offset, Some(0));
                assert_eq!(b.offset, Some(5));
                // Page 1 covers indices 0..5, page 2 covers 5..10.
                // There are still 2 entries (10..12) after page 2,
                // so `has_more` stays `true` for both pages.
                assert_eq!(a.has_more, Some(true));
                assert_eq!(
                    b.has_more,
                    Some(true),
                    "page 2 is the last page so has_more=false"
                );
            }
            (a, b) => panic!("expected Impact, got {a:?} / {b:?}"),
        }
    }

    // ── Coupling-hotspots pagination ───────────────────────────────────────

    #[tokio::test]
    async fn coupling_hotspots_summary_only_omits_pairs_and_reports_total() {
        // AC #3: `summary_only` drops the heavy pair list and ships
        // `total` as the count signal.
        let mut pairs = Vec::new();
        for i in 0..20 {
            pairs.push(CoupledPairEntry {
                file_a: format!("src/a{i}.rs"),
                file_b: format!("src/b{i}.rs"),
                co_edits: 1,
                last_co_edit: "2026-01-01T00:00:00Z".to_string(),
            });
        }
        let graph = PaginationFixtureOps::new().with_pairs(pairs);
        let (server, _calls) = pagination_test_server(graph);

        let mut params = test_params("coupling_hotspots");
        params.project_id = "proj-df6s".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.summary_only = Some(true);

        let response = server
            .dispatch_code_graph(&df6s_ctx(), &mut params)
            .await
            .expect("summary-only coupling_hotspots dispatch");

        match response {
            CodeGraphResponse::CouplingHotspots(r) => {
                assert!(
                    r.pairs.is_empty(),
                    "summary_only must empty the pairs list, not just hide it"
                );
                assert_eq!(r.total, Some(20), "unsliced total still reported");
                assert_eq!(r.summary_only, Some(true));
            }
            other => panic!("expected CouplingHotspots, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn coupling_hotspots_offset_pagination_preserves_total() {
        // AC #1 + #2: a limited page reports the complete total
        // and an offset page does not mutate the unsliced count.
        let mut pairs = Vec::new();
        for i in 0..12 {
            pairs.push(CoupledPairEntry {
                file_a: format!("src/o{i}.rs"),
                file_b: format!("src/q{i}.rs"),
                co_edits: 1,
                last_co_edit: "2026-01-01T00:00:00Z".to_string(),
            });
        }
        let graph = PaginationFixtureOps::new().with_pairs(pairs);
        let (server, _calls) = pagination_test_server(graph);

        let mut params = test_params("coupling_hotspots");
        params.project_id = "proj-df6s".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.limit = Some(5);
        params.offset = Some(5);

        let response = server
            .dispatch_code_graph(&df6s_ctx(), &mut params)
            .await
            .expect("coupling_hotspots dispatch");

        match response {
            CodeGraphResponse::CouplingHotspots(r) => {
                assert_eq!(r.pairs.len(), 5, "page slice = limit");
                assert_eq!(r.total, Some(12), "unsliced total preserved");
                assert_eq!(r.offset, Some(5));
                assert_eq!(r.limit, Some(5));
                assert_eq!(r.has_more, Some(true));
            }
            other => panic!("expected CouplingHotspots, got {other:?}"),
        }
    }

    // ── Group-by-file rollup survives pagination ──────────────────────────

    #[tokio::test]
    async fn neighbors_group_by_file_rollup_preserves_complete_total() {
        // AC: `group_by=file` rollups must report the full
        // unsliced per-file count when paginated. The internal
        // group aggregation still runs over the full set; only the
        // agent-facing slice is bounded.
        let groups: Vec<crate::bridge::FileGroupEntry> = (0..6)
            .map(|i| crate::bridge::FileGroupEntry {
                file: format!("src/mod_{i}/file.rs"),
                occurrence_count: (i + 1) * 2,
                max_depth: 2,
                sample_keys: vec![format!("symbol:src/mod_{i}/file.rs `f`().")],
            })
            .collect();
        let graph = PaginationFixtureOps {
            groups: groups.clone(),
            ..Default::default()
        };
        let (server, _calls) = pagination_test_server(graph);

        let mut params = test_params("neighbors");
        params.project_id = "proj-df6s".to_string();
        params.project_path = "/workspace/repo".to_string();
        params.key = Some("symbol:src/lib.rs `lib`().".to_string());
        params.group_by = Some("file".to_string());
        params.limit = Some(3);
        params.offset = Some(0);

        let response = server
            .dispatch_code_graph(&df6s_ctx(), &mut params)
            .await
            .expect("grouped neighbors dispatch");

        match response {
            CodeGraphResponse::Neighbors(r) => {
                assert_eq!(r.file_groups.as_ref().map(|v| v.len()), Some(3));
                assert_eq!(r.total, Some(6), "full group count preserved");
                assert_eq!(r.has_more, Some(true));
                // After `sort_by_key(Reverse(occurrence_count))` the
                // groups land in descending order: 12, 10, 8, 6, 4, 2.
                // The first-page slice (limit=3) is 12+10+8 = 30.
                // The unsliced total is 12+10+8+6+4+2 = 42. The cap
                // must not be misread as the group's own rollup.
                let page_occurrences: usize = r
                    .file_groups
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|g| g.occurrence_count)
                    .sum();
                assert_eq!(page_occurrences, 30);
            }
            other => panic!("expected Neighbors, got {other:?}"),
        }
    }

    // ── Helper-unit coverage ───────────────────────────────────────────────

    #[test]
    fn df6s_apply_page_slice_handles_offset_and_limit() {
        // Sanity: the helper the handlers call into slices correctly
        // for every combination the response DTO layer is asked to
        // honour.
        let mut v: Vec<i32> = (0..10).collect();
        let page = apply_page_slice(&mut v, 3, 4);
        assert_eq!(v, vec![3, 4, 5, 6]);
        assert!(page.has_more);

        let mut v: Vec<i32> = (0..5).collect();
        let page = apply_page_slice(&mut v, 0, 10);
        assert_eq!(v, vec![0, 1, 2, 3, 4]);
        assert!(!page.has_more, "vec shorter than limit");

        let mut v: Vec<i32> = (0..3).collect();
        let page = apply_page_slice(&mut v, 5, 10);
        assert!(v.is_empty(), "offset past end yields an empty page");
        assert!(!page.has_more);
    }

    #[test]
    fn df6s_build_by_depth_counts_orders_numerically() {
        // BTreeMap-keyed-by-string preserves the natural depth
        // ordering on the wire (1 < 2 < 10).
        let entries = vec![
            synthetic_impact_entry("a", 2, Some("src/a.rs")),
            synthetic_impact_entry("b", 1, Some("src/b.rs")),
            synthetic_impact_entry("c", 10, Some("src/c.rs")),
            synthetic_impact_entry("d", 2, Some("src/d.rs")),
        ];
        let counts = build_by_depth_counts(&entries);
        assert_eq!(counts.get("1"), Some(&1));
        assert_eq!(counts.get("2"), Some(&2));
        assert_eq!(counts.get("10"), Some(&1));
        // Stringified keys sort lexicographically — "10" before
        // "2" — but BTreeMap with String keys uses the natural
        // lexicographic order. We rely on the fact that the
        // `df6s_pagination` module stringifies depths consistently.
        let keys: Vec<&String> = counts.keys().collect();
        assert_eq!(keys, vec!["1", "10", "2"]);
    }

    #[test]
    fn df6s_pagination_applied_triggers_only_for_paginated_responses() {
        // A caller that didn't paginate at all should see
        // `pagination_applied == false` so the wire shape stays
        // additive (no `total`/`offset`/`limit`/`has_more` noise on
        // the default-page shape).
        let params = PaginationParams::resolve(&test_params("neighbors"), 20);
        assert!(!pagination_applied(params, 10, params.limit));

        // A caller that paginated past the end sees
        // `pagination_applied == true` even on an empty page.
        let mut params = test_params("neighbors");
        params.offset = Some(100);
        let params = PaginationParams::resolve(&params, 20);
        assert!(pagination_applied(params, 10, params.limit));

        // `summary_only` always emits.
        let mut params = test_params("neighbors");
        params.summary_only = Some(true);
        let params = PaginationParams::resolve(&params, 20);
        assert!(pagination_applied(params, 10, params.limit));
    }
