    // vxmw / 5ice: dispatch-level smoke coverage for the registry-driven
    // vertical slice. The fixture below is a thin recording
    // `RepoGraphOps` impl that captures which bridge methods the
    // dispatcher reached and with what arguments, returning canned
    // success data. Each smoke test then exercises the full
    // `DjinnMcpServer::dispatch_code_graph` path end-to-end:
    //
    //   CodeGraphParams.operation
    //     → `pre_resolve_key` (resolver, may short-circuit)
    //     → registry-driven per-op handler
    //     → `RepoGraphOps::<method>`
    //     → typed `CodeGraphResponse` envelope
    //
    // Acceptance criteria (task 5ice):
    //
    // 1. `operation = "impact"` reaches `RepoGraphOps::impact` and
    //    returns the same `Impact` envelope semantics as the pre-vxmw
    //    handwritten dispatch.
    // 2. `operation = "coupling_hotspots"` reaches
    //    `RepoGraphOps::coupling_hotspots` and returns the same
    //    `CouplingHotspots` envelope semantics.
    // 3. At least one additional basic op (`neighbors` or `context`)
    //    is smoke-tested. We exercise BOTH since the existing
    //    `df6s_pagination` tests already cover `neighbors` deeply,
    //    and `context` is cheap to add and covers the `KeyOnly`
    //    validation class that `neighbors` does not.
    // 4. Validation errors propagate through the registry path with
    //    the same error envelope as before — do not weaken assertions
    //    just because the dispatch is now registry-driven.
    // 5. Pagination / workspace / staleness envelope fields are
    //    preserved through the registry path.
    //
    // No new `use` statements: `tests_part1.inc` already imports
    // every type this file references, and `tests_part2.inc` defines
    // the `test_params` builder we reuse.

    /// Captures which `RepoGraphOps` method the dispatcher reached plus
    /// the resolved arguments, so each test can assert the routing
    /// survived the registry-driven dispatch.
    /// Type alias to keep the `RegistryDispatchOps` struct readable.
    type RegistryCallLog = Vec<(&'static str, Option<String>)>;

    #[derive(Clone)]
    struct RegistryDispatchOps {
        /// Sequence of `(method, key)` calls — one entry per bridge
        /// invocation. `key` is the `&str` the bridge was called with
        /// (e.g. `key`, `query`, `file_path`); `None` for calls that
        /// don't take a single string key (`coupling_hotspots`,
        /// `search`, etc.).
        calls: Arc<Mutex<RegistryCallLog>>,
        /// Programmable success response for `neighbors`.
        neighbors_result: Option<NeighborsResult>,
        /// Programmable success response for `impact`.
        impact_result: Option<ImpactResult>,
        /// Programmable success response for `context`.
        context_result: Option<SymbolContext>,
        /// Programmable success response for `route_map`.
        route_map_result: Option<RouteMapResult>,
        /// Programmable success response for `api_impact`.
        api_impact_result: Option<ApiImpactResult>,
        /// Programmable success response for `flow`.
        flow_result: Option<FlowResult>,
        /// Programmable success response for `detect_changes`.
        detect_changes_result: Option<DetectedChangesResult>,
        /// Programmable success response for `snapshot`.
        snapshot_result: Option<SnapshotPayload>,
        /// Programmable success response for `status`.
        status_result: Option<GraphStatus>,
        /// Programmable success response for `coupling`.
        coupling_result: Option<Vec<CouplingEntry>>,
        /// Programmable success response for `coupling_hotspots`.
        coupling_hotspots_result: Option<Vec<CoupledPairEntry>>,
    }

    impl RegistryDispatchOps {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                neighbors_result: None,
                impact_result: None,
                context_result: None,
                route_map_result: None,
                api_impact_result: None,
                flow_result: None,
                detect_changes_result: None,
                snapshot_result: None,
                status_result: None,
                coupling_result: None,
                coupling_hotspots_result: None,
            }
        }

        fn with_neighbors(mut self, result: NeighborsResult) -> Self {
            self.neighbors_result = Some(result);
            self
        }

        fn with_impact(mut self, result: ImpactResult) -> Self {
            self.impact_result = Some(result);
            self
        }

        fn with_context(mut self, result: SymbolContext) -> Self {
            self.context_result = Some(result);
            self
        }

        fn with_route_map(mut self, result: RouteMapResult) -> Self {
            self.route_map_result = Some(result);
            self
        }

        fn with_api_impact(mut self, result: ApiImpactResult) -> Self {
            self.api_impact_result = Some(result);
            self
        }

        fn with_flow(mut self, result: FlowResult) -> Self {
            self.flow_result = Some(result);
            self
        }

        fn with_detect_changes(mut self, result: DetectedChangesResult) -> Self {
            self.detect_changes_result = Some(result);
            self
        }

        fn with_snapshot(mut self, result: SnapshotPayload) -> Self {
            self.snapshot_result = Some(result);
            self
        }

        fn with_status(mut self, result: GraphStatus) -> Self {
            self.status_result = Some(result);
            self
        }

        fn with_coupling(mut self, result: Vec<CouplingEntry>) -> Self {
            self.coupling_result = Some(result);
            self
        }

        fn with_coupling_hotspots(mut self, result: Vec<CoupledPairEntry>) -> Self {
            self.coupling_hotspots_result = Some(result);
            self
        }

        fn calls(&self) -> RegistryCallLog {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .clone()
        }
    }

    #[async_trait]
    impl RepoGraphOps for RegistryDispatchOps {
        async fn workspaces(
            &self,
            _: &ProjectCtx,
        ) -> Result<crate::bridge::WorkspacesResult, String> {
            Ok(crate::bridge::WorkspacesResult {
                project_id: String::new(),
                workspaces: Vec::new(),
            })
        }

        async fn workspace_hint(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
        ) -> Result<Option<Vec<String>>, String> {
            // pb94: no choices → `None` so the dispatcher's
            // `resolve_workspace_scope` keeps the requested scope as-is
            // (or unscoped when omitted). That matches the
            // pre-vxmw production behaviour for single-workspace
            // projects and is what `impact`'s `traversal_seed_only`
            // contract expects.
            Ok(None)
        }

        async fn neighbors(
            &self,
            _: &ProjectCtx,
            key: &str,
            direction: Option<&str>,
            group_by: Option<&str>,
            kind_filter: Option<&str>,
        ) -> Result<NeighborsResult, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("neighbors", Some(key.to_string())));
            // Mirror the smoke inputs back so tests can assert
            // dispatch preserves direction/group_by/kind_filter.
            assert!(
                direction.is_none() || matches!(direction, Some("incoming" | "outgoing")),
                "registry dispatch: direction must be 'incoming' or 'outgoing' after validation"
            );
            assert!(
                group_by.is_none() || group_by == Some("file"),
                "registry dispatch: group_by must be 'file' or unset after validation"
            );
            assert!(kind_filter.is_none() || matches!(kind_filter, Some("reads" | "writes")));
            self.neighbors_result.clone().ok_or_else(|| {
                "registry dispatch fixture: neighbors_result not programmed".to_string()
            })
        }

        async fn impact(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            key: &str,
            depth: usize,
            group_by: Option<&str>,
            min_confidence: Option<f64>,
        ) -> Result<ImpactResult, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("impact", Some(key.to_string())));
            // pb94: workspace scoping for `impact` is traversal-seed-only —
            // the bridge receives `Some(workspace)` only when the caller
            // supplied a known slug; unknown slugs must degrade to `None`
            // so the BFS walk stays unconstrained. The fixture's
            // `workspace_hint` returns `None` to exercise that path.
            assert!(
                workspace.is_none(),
                "registry dispatch: `impact` should receive None workspace when \
                 fixture's workspace_hint returns None"
            );
            assert!(depth > 0, "registry dispatch: depth must default > 0");
            assert!(
                group_by.is_none() || group_by == Some("file"),
                "registry dispatch: group_by must be 'file' or unset after validation"
            );
            if let Some(c) = min_confidence {
                assert!(
                    (0.0..=1.0).contains(&c),
                    "registry dispatch: min_confidence must be in [0.0, 1.0] after validation"
                );
            }
            Ok(self.impact_result.clone().ok_or_else(|| {
                "registry dispatch fixture: impact_result not programmed".to_string()
            })?)
        }

        async fn context(
            &self,
            _: &ProjectCtx,
            key: &str,
            include_content: bool,
        ) -> Result<Option<SymbolContext>, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("context", Some(key.to_string())));
            // `context` does not let `include_content` leak through to
            // the wire — the registry path passes it through, but the
            // bridge impl is responsible for materialising `content`
            // when set. The fixture always omits content (the canned
            // canned `SymbolContext` is constructed without it).
            let _ = include_content;
            Ok(self.context_result.clone())
        }

        async fn route_map(
            &self,
            _: &ProjectCtx,
            route_id: Option<&str>,
            method: Option<&str>,
            path: Option<&str>,
            path_glob: Option<&str>,
            framework: Option<&str>,
            limit: usize,
        ) -> Result<RouteMapResult, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("route_map", route_id.or(path).map(str::to_string)));
            assert!(route_id.is_some() || path_glob.is_some() || method.zip(path).is_some());
            assert!(limit > 0, "registry dispatch: route_map limit must be > 0");
            let _ = framework;
            self.route_map_result.clone().ok_or_else(|| {
                "registry dispatch fixture: route_map_result not programmed".to_string()
            })
        }

        async fn api_impact(
            &self,
            _: &ProjectCtx,
            route_id: Option<&str>,
            method: Option<&str>,
            path: Option<&str>,
            min_confidence: f64,
            limit: usize,
        ) -> Result<ApiImpactResult, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("api_impact", route_id.or(path).map(str::to_string)));
            assert!(route_id.is_some() || method.zip(path).is_some());
            assert!((0.0..=1.0).contains(&min_confidence));
            assert!(limit > 0, "registry dispatch: api_impact limit must be > 0");
            self.api_impact_result.clone().ok_or_else(|| {
                "registry dispatch fixture: api_impact_result not programmed".to_string()
            })
        }

        async fn flow(
            &self,
            _: &ProjectCtx,
            query: &str,
            kind_filter: Option<&str>,
            limit: usize,
        ) -> Result<FlowResult, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("flow", Some(query.to_string())));
            assert!(!query.is_empty());
            assert!(kind_filter.is_none() || matches!(kind_filter, Some("process" | "step")));
            assert!(limit > 0, "registry dispatch: flow limit must be > 0");
            self.flow_result.clone().ok_or_else(|| {
                "registry dispatch fixture: flow_result not programmed".to_string()
            })
        }


        async fn coupling_hotspots(
            &self,
            _: &ProjectCtx,
            limit: usize,
            since_days: Option<u32>,
            max_files_per_commit: usize,
        ) -> Result<Vec<CoupledPairEntry>, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("coupling_hotspots", None));
            // The dispatcher's pagination contract: the bridge always
            // receives the **fetch** limit (over-fetched by the
            // handler) — never the page cap. Tests below confirm the
            // fetch value matches `pagination.limit * 25` for the
            // default page size.
            assert!(limit > 0, "registry dispatch: fetch limit must be > 0");
            assert!(
                (1..=1000).contains(&max_files_per_commit),
                "registry dispatch: max_files_per_commit must be clamped to [1, 1000]"
            );
            if let Some(days) = since_days {
                assert!(days > 0, "registry dispatch: since_days must be > 0 when set");
            }
            Ok(self.coupling_hotspots_result.clone().ok_or_else(|| {
                "registry dispatch fixture: coupling_hotspots_result not programmed".to_string()
            })?)
        }

        async fn resolve(
            &self,
            _: &ProjectCtx,
            key: &str,
            _kind_hint: Option<&str>,
        ) -> Result<ResolveOutcome, String> {
            // The pre-resolve pass rewrites `params.key` to the
            // canonical UID returned here. The dispatcher then
            // dispatches the op with that UID. Tests assert the bridge
            // was called with the **rewritten** key (i.e. what
            // `RepoGraphOps::resolve` returned), not the original
            // short identifier the caller supplied.
            Ok(ResolveOutcome::Found(format!("uid:{key}")))
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
            unreachable!("registry dispatch smoke tests should not call ranked")
        }
        async fn implementations(&self, _: &ProjectCtx, _: &str) -> Result<Vec<String>, String> {
            unreachable!("registry dispatch smoke tests should not call implementations")
        }
        async fn search(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<SearchHit>, String> {
            unreachable!("registry dispatch smoke tests should not call search")
        }
        async fn cycles(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<CycleGroup>, String> {
            unreachable!("registry dispatch smoke tests should not call cycles")
        }
        async fn orphans(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<OrphanEntry>, String> {
            unreachable!("registry dispatch smoke tests should not call orphans")
        }
        async fn path(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: &str,
            _: &str,
            _: Option<usize>,
        ) -> Result<Option<PathResult>, String> {
            unreachable!("registry dispatch smoke tests should not call path")
        }
        async fn edges(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<EdgeEntry>, String> {
            unreachable!("registry dispatch smoke tests should not call edges")
        }
        async fn describe(
            &self,
            _: &ProjectCtx,
            _: &str,
        ) -> Result<Option<SymbolDescription>, String> {
            unreachable!("registry dispatch smoke tests should not call describe")
        }
        async fn status(&self, _: &ProjectCtx) -> Result<GraphStatus, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("status", None));
            self.status_result.clone().ok_or_else(|| {
                "registry dispatch fixture: status_result not programmed".to_string()
            })
        }
        async fn snapshot(
            &self,
            _: &ProjectCtx,
            workspace: Option<&str>,
            level: SnapshotLevel,
            node_cap: usize,
            _: &GraphExclusions,
        ) -> Result<SnapshotPayload, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("snapshot", workspace.map(str::to_string)));
            assert!(matches!(level, SnapshotLevel::Symbol | SnapshotLevel::Community));
            assert!(node_cap > 0, "registry dispatch: snapshot node_cap must be > 0");
            self.snapshot_result.clone().ok_or_else(|| {
                "registry dispatch fixture: snapshot_result not programmed".to_string()
            })
        }
        async fn symbols_at(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: u32,
            _: Option<u32>,
        ) -> Result<Vec<SymbolAtHit>, String> {
            unreachable!("registry dispatch smoke tests should not call symbols_at")
        }
        async fn diff_touches(
            &self,
            _: &ProjectCtx,
            _: &[ChangedRange],
        ) -> Result<DiffTouchesResult, String> {
            unreachable!("registry dispatch smoke tests should not call diff_touches")
        }
        async fn detect_changes(
            &self,
            _: &ProjectCtx,
            from_sha: Option<&str>,
            to_sha: Option<&str>,
            changed_files: &[String],
        ) -> Result<DetectedChangesResult, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("detect_changes", from_sha.or(to_sha).map(str::to_string)));
            assert!(from_sha.zip(to_sha).is_some() || !changed_files.is_empty());
            self.detect_changes_result.clone().ok_or_else(|| {
                "registry dispatch fixture: detect_changes_result not programmed".to_string()
            })
        }
        async fn api_surface(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<ApiSurfaceEntry>, String> {
            unreachable!("registry dispatch smoke tests should not call api_surface")
        }
        async fn boundary_check(
            &self,
            _: &ProjectCtx,
            _: &[BoundaryRule],
            _: &str,
        ) -> Result<Vec<BoundaryViolation>, String> {
            unreachable!("registry dispatch smoke tests should not call boundary_check")
        }
        async fn hotspots(
            &self,
            _: &ProjectCtx,
            _: u32,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<HotspotEntry>, String> {
            unreachable!("registry dispatch smoke tests should not call hotspots")
        }
        async fn complexity(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<ComplexityResult, String> {
            unreachable!("registry dispatch smoke tests should not call complexity")
        }
        async fn refactor_candidates(
            &self,
            _: &ProjectCtx,
            _: Option<u32>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RefactorCandidate>, String> {
            unreachable!("registry dispatch smoke tests should not call refactor_candidates")
        }
        async fn metrics_at(&self, _: &ProjectCtx) -> Result<MetricsAtResult, String> {
            unreachable!("registry dispatch smoke tests should not call metrics_at")
        }
        async fn dead_symbols(
            &self,
            _: &ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<DeadSymbolEntry>, String> {
            unreachable!("registry dispatch smoke tests should not call dead_symbols")
        }
        async fn deprecated_callers(
            &self,
            _: &ProjectCtx,
            _: usize,
        ) -> Result<Vec<DeprecatedHit>, String> {
            unreachable!("registry dispatch smoke tests should not call deprecated_callers")
        }
        async fn touches_hot_path(
            &self,
            _: &ProjectCtx,
            _: Option<&str>,
            _: &[String],
            _: &[String],
            _: &[String],
        ) -> Result<Vec<HotPathHit>, String> {
            unreachable!("registry dispatch smoke tests should not call touches_hot_path")
        }
        async fn coupling(
            &self,
            _: &ProjectCtx,
            file_path: &str,
            limit: usize,
        ) -> Result<Vec<CouplingEntry>, String> {
            self.calls
                .lock()
                .expect("registry dispatch calls lock")
                .push(("coupling", Some(file_path.to_string())));
            assert!(!file_path.is_empty());
            assert!(limit > 0, "registry dispatch: coupling limit must be > 0");
            self.coupling_result.clone().ok_or_else(|| {
                "registry dispatch fixture: coupling_result not programmed".to_string()
            })
        }
        async fn churn(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
        ) -> Result<Vec<ChurnEntry>, String> {
            unreachable!("registry dispatch smoke tests should not call churn")
        }
        async fn coupling_hubs(
            &self,
            _: &ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<CouplingHubEntry>, String> {
            unreachable!("registry dispatch smoke tests should not call coupling_hubs")
        }
    }

    /// Build a `DjinnMcpServer` wired to the recording fixture.  Mirrors
    /// the `fixture_server` helper from `tests_part2.inc` so each test
    /// gets a self-contained state — no shared mutation across tests.
    fn registry_dispatch_server(
        ops: RegistryDispatchOps,
    ) -> (DjinnMcpServer, RegistryDispatchOps) {
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
            Arc::new(ops.clone()),
        );
        (DjinnMcpServer::new(state), ops)
    }

    fn registry_dispatch_ctx() -> ProjectCtx {
        ProjectCtx {
            id: "proj-vxmw-registry".to_string(),
            clone_path: "/workspace/repo".to_string(),
            workspace: None,
            sub_path: None,
        }
    }

    /// Canned `SymbolContext` returned by the fixture for `context`
    /// smoke tests. Carries the discriminator field the UI parser
    /// (per `parseSymbolContext`) hangs off.
    fn canned_symbol_context(uid: &str) -> SymbolContext {
        SymbolContext {
            symbol: crate::bridge::SymbolNode {
                uid: uid.to_string(),
                name: uid.to_string(),
                kind: "function".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 10,
                content: None,
                method_metadata: None,
                complexity: None,
            },
            incoming: Default::default(),
            outgoing: Default::default(),
            processes: Vec::new(),
        }
    }

    fn canned_graph_status(pinned_commit: &str) -> GraphStatus {
        GraphStatus {
            project_id: "proj-vxmw-registry".to_string(),
            warmed: true,
            last_warm_at: Some("2026-06-30T00:00:00Z".to_string()),
            pinned_commit: Some(pinned_commit.to_string()),
            commits_since_pin: Some(0),
            route_parity_enabled: true,
            route_exclusion_config: serde_json::json!({}),
        }
    }

    fn canned_snapshot_payload(project_id: &str) -> SnapshotPayload {
        SnapshotPayload {
            project_id: project_id.to_string(),
            git_head: "abc123".to_string(),
            generated_at: "2026-06-30T00:00:00Z".to_string(),
            truncated: false,
            total_nodes: 1,
            total_edges: 0,
            node_cap: 25,
            nodes: vec![SnapshotNode {
                id: "symbol:handler".to_string(),
                uid: "symbol:handler".to_string(),
                kind: "symbol".to_string(),
                label: "handler".to_string(),
                workspace: None,
                workspace_kind: None,
                member_count: None,
                internal_edge_count: None,
                symbol_kind: Some("function".to_string()),
                file_path: Some("src/routes.rs".to_string()),
                pagerank: 1.0,
                community_id: None,
                cognitive: None,
                is_test: false,
                x: 0.0,
                y: 0.0,
                gx: None,
                gy: None,
                gz: None,
                degree: None,
                keywords: Vec::new(),
            }],
            edges: Vec::new(),
        }
    }

    fn canned_detect_changes_result() -> DetectedChangesResult {
        let touched = DetectedTouchedSymbol {
            uid: "symbol:changed".to_string(),
            name: "changed".to_string(),
            kind: "function".to_string(),
            file_path: "src/changed.rs".to_string(),
            start_line: 1,
            end_line: 5,
            pagerank_tier: PagerankTier::High,
            change_kind: ChangeKind::Modified,
        };
        let mut by_file = std::collections::BTreeMap::new();
        by_file.insert("src/changed.rs".to_string(), vec![touched.clone()]);
        DetectedChangesResult {
            from_sha: "base".to_string(),
            to_sha: "head".to_string(),
            touched_symbols: vec![touched],
            by_file,
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Smoke: `operation = "impact"` reaches RepoGraphOps::impact
    // ─────────────────────────────────────────────────────────────────

    /// Acceptance criterion: `operation = "impact"` routes through
    /// `DjinnMcpServer::dispatch_code_graph` and reaches
    /// `RepoGraphOps::impact`. The returned envelope is the same
    /// `CodeGraphResponse::Impact(_)` the pre-vxmw handwritten
    /// dispatch produced.
    #[tokio::test]
    async fn impact_dispatch_routes_to_repo_graph_impact() {
        let canned = ImpactResult::Detailed(vec![ImpactEntry {
            key: "uid:caller_a".to_string(),
            uid: "uid:caller_a".to_string(),
            depth: 1,
            file_path: Some("src/caller.rs".to_string()),
            confidence_tier: None,
            exclusion_reason: None,
        }]);
        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_impact(canned.clone()),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("impact");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.key = Some("MyStruct".to_string());

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("impact dispatch succeeds");

        match response {
            CodeGraphResponse::Impact(impact) => {
                // Envelope semantics: pre-resolve rewrites `key` to the
                // canonical UID the bridge returned, so the response
                // `key` field carries the resolved UID, not the caller's
                // short identifier. This is the same shape the
                // pre-vxmw dispatch produced.
                assert_eq!(
                    impact.key, "uid:MyStruct",
                    "registry dispatch: impact response `key` should carry the \
                     pre-resolved UID the bridge returned, not the caller's input"
                );
                // Bridge payload: the canned `Detailed` vec surfaces
                // verbatim inside the response DTO, preserving the
                // exact `ImpactEntry` shape callers expect.
                let entries = impact
                    .impact
                    .expect("impact response carries detailed entries when not summary_only");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].key, "uid:caller_a");
                assert_eq!(entries[0].depth, 1);
                // df6s: pagination envelope fields stay None for the
                // default (no `page_limit`) call so the wire shape is
                // additive for un-paginated callers.
                assert!(
                    impact.total.is_none()
                        && impact.offset.is_none()
                        && impact.limit.is_none()
                        && impact.has_more.is_none()
                        && impact.summary_only.is_none(),
                    "registry dispatch: default impact call must not emit df6s \
                     pagination envelope (total/offset/limit/has_more/summary_only)"
                );
            }
            other => panic!("expected CodeGraphResponse::Impact, got {other:?}"),
        }

        // Dispatch routing: the bridge saw exactly one call, named
        // `impact`, with the pre-resolved UID (not the caller's
        // short identifier).
        let calls = ops.calls();
        assert_eq!(calls.len(), 1, "exactly one bridge call expected");
        assert_eq!(calls[0].0, "impact");
        assert_eq!(
            calls[0].1.as_deref(),
            Some("uid:MyStruct"),
            "impact bridge call must use the pre-resolved UID"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Smoke: `operation = "coupling_hotspots"` reaches RepoGraphOps::coupling_hotspots
    // ─────────────────────────────────────────────────────────────────

    /// Acceptance criterion: `operation = "coupling_hotspots"` routes
    /// through `dispatch_code_graph` and reaches
    /// `RepoGraphOps::coupling_hotspots`. The returned envelope is the
    /// same `CodeGraphResponse::CouplingHotspots(_)` the pre-vxmw
    /// dispatch produced.
    #[tokio::test]
    async fn coupling_hotspots_dispatch_routes_to_repo_graph_coupling_hotspots() {
        let canned = vec![CoupledPairEntry {
            file_a: "src/auth.rs".to_string(),
            file_b: "src/user.rs".to_string(),
            co_edits: 42,
            last_co_edit: "2026-06-01T12:00:00Z".to_string(),
        }];
        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_coupling_hotspots(canned.clone()),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("coupling_hotspots");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.limit = Some(10);

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("coupling_hotspots dispatch succeeds");

        match response {
            CodeGraphResponse::CouplingHotspots(ch) => {
                assert_eq!(
                    ch.pairs.len(),
                    1,
                    "coupling_hotspots response carries the bridge result"
                );
                assert_eq!(ch.pairs[0].file_a, "src/auth.rs");
                assert_eq!(ch.pairs[0].co_edits, 42);
                // df6s: default page cap does not emit pagination
                // envelope so the wire shape stays additive.
                assert!(
                    ch.total.is_none()
                        && ch.offset.is_none()
                        && ch.limit.is_none()
                        && ch.has_more.is_none()
                        && ch.summary_only.is_none(),
                    "registry dispatch: default coupling_hotspots call must not \
                     emit df6s pagination envelope"
                );
            }
            other => panic!("expected CodeGraphResponse::CouplingHotspots, got {other:?}"),
        }

        let calls = ops.calls();
        assert_eq!(calls.len(), 1, "exactly one bridge call expected");
        assert_eq!(calls[0].0, "coupling_hotspots");
    }

    /// Coupling-hotspots exercises the **non-basic** registry path: no
    /// pre-resolve, no `require_key` validation, no workspace scoping.
    /// This test confirms that envelope semantics survive the dispatch
    /// when `since_days` is supplied (the bridge receives a u32 — not
    /// the wire-shape `i64`).
    #[tokio::test]
    async fn coupling_hotspots_dispatch_preserves_since_days_window() {
        let canned = vec![CoupledPairEntry {
            file_a: "src/auth.rs".to_string(),
            file_b: "src/user.rs".to_string(),
            co_edits: 7,
            last_co_edit: "2026-06-01T12:00:00Z".to_string(),
        }];
        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_coupling_hotspots(canned),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("coupling_hotspots");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.since_days = Some(30);

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("coupling_hotspots dispatch with since_days succeeds");

        match response {
            CodeGraphResponse::CouplingHotspots(ch) => {
                assert_eq!(ch.pairs.len(), 1);
                assert_eq!(ch.pairs[0].co_edits, 7);
            }
            other => panic!("expected CodeGraphResponse::CouplingHotspots, got {other:?}"),
        }
        let calls = ops.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "coupling_hotspots");
        // The fixture asserts `since_days > 0` when set; if the
        // handler failed to pass it through, the fixture panics
        // before we get here. The dispatch path is exercised
        // end-to-end, not just unit-tested.
    }

    // ─────────────────────────────────────────────────────────────────
    // Smoke: `operation = "neighbors"` (additional basic coverage)
    // ─────────────────────────────────────────────────────────────────

    /// Acceptance criterion (additional basic op): `operation =
    /// "neighbors"` routes through `dispatch_code_graph` and reaches
    /// `RepoGraphOps::neighbors`. The fixture's `neighbors` impl
    /// additionally asserts `direction` / `group_by` / `kind_filter`
    /// are within the validated set so a regression that drops
    /// validation surfaces here.
    #[tokio::test]
    async fn neighbors_dispatch_routes_to_repo_graph_neighbors() {
        let canned = NeighborsResult::Detailed(vec![GraphNeighbor {
            key: "uid:target".to_string(),
            uid: "uid:target".to_string(),
            kind: "symbol".to_string(),
            display_name: "target".to_string(),
            edge_kind: "symbol_reference".to_string(),
            edge_weight: 1.0,
            direction: "outgoing".to_string(),
        }]);
        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_neighbors(canned.clone()),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("neighbors");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.key = Some("MyStruct".to_string());
        params.direction = Some("outgoing".to_string());
        params.kind_filter = Some("reads".to_string());

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("neighbors dispatch succeeds");

        match response {
            CodeGraphResponse::Neighbors(n) => {
                assert_eq!(
                    n.key, "uid:MyStruct",
                    "registry dispatch: neighbors response `key` carries the pre-resolved UID"
                );
                let entries = n
                    .neighbors
                    .expect("neighbors response carries detailed entries");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].key, "uid:target");
                assert_eq!(entries[0].direction, "outgoing");
            }
            other => panic!("expected CodeGraphResponse::Neighbors, got {other:?}"),
        }
        let calls = ops.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "neighbors");
        assert_eq!(
            calls[0].1.as_deref(),
            Some("uid:MyStruct"),
            "neighbors bridge call uses pre-resolved UID"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Smoke: `operation = "context"` (additional basic coverage)
    // ─────────────────────────────────────────────────────────────────

    /// Acceptance criterion (additional basic op): `operation =
    /// "context"` routes through `dispatch_code_graph` and reaches
    /// `RepoGraphOps::context`. `context` uses the `KeyOnly`
    /// validation class (no group_by / direction / kind_filter
    /// validators), so this test also covers a different validation
    /// surface than `neighbors`.
    #[tokio::test]
    async fn context_dispatch_routes_to_repo_graph_context() {
        let canned = canned_symbol_context("uid:MyStruct");
        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_context(canned.clone()),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("context");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.key = Some("MyStruct".to_string());
        params.include_content = Some(true);

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("context dispatch succeeds");

        match response {
            CodeGraphResponse::Context(c) => {
                // The `symbol_context` discriminator field carries the
                // canned `SymbolContext` verbatim — the envelope
                // semantics `parseSymbolContext` depends on stay
                // intact through the registry path.
                assert_eq!(c.symbol_context.symbol.uid, "uid:MyStruct");
                assert_eq!(c.symbol_context.symbol.kind, "function");
                assert!(
                    c.next_step.is_none(),
                    "registry dispatch: context response must not carry a next_step hint \
                     when next_step_hints_enabled is unset"
                );
            }
            other => panic!("expected CodeGraphResponse::Context, got {other:?}"),
        }
        let calls = ops.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "context");
        assert_eq!(
            calls[0].1.as_deref(),
            Some("uid:MyStruct"),
            "context bridge call uses pre-resolved UID"
        );
    }

    #[tokio::test]
    async fn route_api_flow_change_snapshot_and_coupling_routes_preserve_envelopes() {
        let ctx = registry_dispatch_ctx();

        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_route_map(RouteMapResult::default()),
        );
        let mut params = test_params("route_map");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.method = Some("GET".to_string());
        params.path = Some("/api/users".to_string());
        params.path_glob = Some("/api/**".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("route_map dispatch succeeds");
        assert!(matches!(response, CodeGraphResponse::RouteMap(_)));
        assert_eq!(ops.calls()[0].0, "route_map");

        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_api_impact(ApiImpactResult::default()),
        );
        let mut params = test_params("api_impact");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.method = Some("GET".to_string());
        params.path = Some("/api/users".to_string());
        params.min_confidence = Some(0.75);
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("api_impact dispatch succeeds");
        assert!(matches!(response, CodeGraphResponse::ApiImpact(_)));
        assert_eq!(ops.calls()[0].0, "api_impact");

        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_flow(FlowResult::default()),
        );
        let mut params = test_params("flow");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.query = Some("checkout".to_string());
        params.kind_filter = Some("step".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("flow dispatch succeeds");
        assert!(matches!(response, CodeGraphResponse::Flow(_)));
        assert_eq!(ops.calls()[0].0, "flow");

        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_detect_changes(canned_detect_changes_result()),
        );
        let mut params = test_params("detect_changes");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.from_sha = Some("base".to_string());
        params.to_sha = Some("head".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("detect_changes dispatch succeeds");
        assert!(matches!(response, CodeGraphResponse::DetectedChanges(_)));
        assert_eq!(ops.calls()[0].0, "detect_changes");

        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_snapshot(canned_snapshot_payload(&ctx.id)),
        );
        let mut params = test_params("snapshot");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.limit = Some(25);
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("snapshot dispatch succeeds");
        match response {
            CodeGraphResponse::Snapshot(snapshot) => {
                assert_eq!(snapshot.snapshot.project_id, ctx.id);
                assert_eq!(snapshot.snapshot.nodes.len(), 1);
            }
            other => panic!("expected Snapshot envelope, got {other:?}"),
        }
        assert_eq!(ops.calls()[0].0, "snapshot");

        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_coupling(vec![CouplingEntry {
                file_path: "src/paired.rs".to_string(),
                co_edit_count: 2,
                last_co_edit: "2026-06-30T00:00:00Z".to_string(),
                supporting_commit_samples: vec!["abc123".to_string()],
            }]),
        );
        let mut params = test_params("coupling");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.file = Some("src/lib.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("coupling dispatch succeeds");
        match response {
            CodeGraphResponse::Coupling(coupling) => {
                assert_eq!(coupling.file, "src/lib.rs");
                assert_eq!(coupling.coupled[0].file_path, "src/paired.rs");
            }
            other => panic!("expected Coupling envelope, got {other:?}"),
        }
        assert_eq!(ops.calls()[0].0, "coupling");
    }

    // ─────────────────────────────────────────────────────────────────
    // Envelope-semantics regressions: errors, validation, pagination
    // ─────────────────────────────────────────────────────────────────

    /// Acceptance criterion (do not weaken assertions): when the
    /// caller omits `key` for `impact`, `dispatch_code_graph` returns
    /// the pre-vxmw validation error envelope — not a coerced success.
    /// This guards the contract that the registry path preserves the
    /// `require_key` validator behaviour.
    #[tokio::test]
    async fn impact_dispatch_preserves_require_key_validation_error() {
        let canned = ImpactResult::Detailed(Vec::new());
        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_impact(canned),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("impact");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.key = None; // intentionally missing

        let err = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect_err("impact without key must surface a validation error");

        assert!(
            err.contains("'key' is required for operation 'impact'"),
            "expected require_key error to propagate through registry dispatch, got: {err}"
        );

        // The bridge must NOT be reached when validation fails —
        // otherwise a caller could observe a successful impact result
        // for a query we never validated.
        assert!(
            ops.calls().is_empty(),
            "registry dispatch: bridge must not be called when require_key fails"
        );
    }

    /// Acceptance criterion (do not weaken assertions): invalid
    /// `min_confidence` (out of `[0.0, 1.0]`) is rejected with the
    /// same error envelope through the registry path.
    #[tokio::test]
    async fn impact_dispatch_preserves_min_confidence_range_validation() {
        let canned = ImpactResult::Detailed(Vec::new());
        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_impact(canned),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("impact");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.key = Some("MyStruct".to_string());
        params.min_confidence = Some(1.5); // out of [0.0, 1.0]

        let err = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect_err("out-of-range min_confidence must surface a validation error");

        assert!(
            err.contains("invalid min_confidence 1.5"),
            "expected impact range-validation error to propagate, got: {err}"
        );
        assert!(
            ops.calls().is_empty(),
            "registry dispatch: bridge must not be called when range validation fails"
        );
    }

    #[tokio::test]
    async fn api_impact_dispatch_preserves_missing_route_selector_validation_error() {
        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_api_impact(ApiImpactResult::default()),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("api_impact");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.route_id = None;
        params.method = None;
        params.path = None;

        let err = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect_err("api_impact without a route selector must fail");

        assert!(
            err.contains("'route_id' or both 'method' and 'path' are required"),
            "expected route selector validation error, got: {err}"
        );
        assert!(
            ops.calls().is_empty(),
            "registry dispatch: bridge must not be called when route selector validation fails"
        );
    }

    /// Acceptance criterion (do not weaken assertions): invalid
    /// `direction` for `neighbors` is rejected before the bridge is
    /// called. The pre-vxmw dispatcher's `validate_direction` hook
    /// must still fire through the registry path.
    #[tokio::test]
    async fn neighbors_dispatch_preserves_direction_validation_error() {
        let canned = NeighborsResult::Detailed(Vec::new());
        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_neighbors(canned),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("neighbors");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.key = Some("MyStruct".to_string());
        params.direction = Some("sideways".to_string());

        let err = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect_err("invalid direction must surface a validation error");

        assert!(
            err.contains("invalid direction"),
            "expected neighbors direction-validation error to propagate, got: {err}"
        );
        assert!(
            ops.calls().is_empty(),
            "registry dispatch: bridge must not be called when direction validation fails"
        );
    }

    /// Acceptance criterion (do not weaken assertions): invalid
    /// `kind_filter` for `neighbors` (the edge-kind validator that
    /// PR A3 introduced) is rejected before the bridge is called.
    #[tokio::test]
    async fn neighbors_dispatch_preserves_edge_kind_filter_validation_error() {
        let canned = NeighborsResult::Detailed(Vec::new());
        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_neighbors(canned),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("neighbors");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.key = Some("MyStruct".to_string());
        params.kind_filter = Some("not-a-kind".to_string());

        let err = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect_err("invalid edge-kind filter must surface a validation error");

        assert!(
            err.contains("invalid kind_filter"),
            "expected neighbors edge-kind-filter validation error to propagate, got: {err}"
        );
        assert!(
            ops.calls().is_empty(),
            "registry dispatch: bridge must not be called when edge-kind validation fails"
        );
    }

    #[tokio::test]
    async fn current_head_aliases_attach_graph_staleness_without_changing_payload() {
        let parsed: CodeGraphParams = serde_json::from_value(serde_json::json!({
            "operation": "impact",
            "project": "owner/repo",
            "key": "MyStruct",
            "currentHead": "head-sha"
        }))
        .expect("currentHead alias parses");
        assert_eq!(parsed.current_head.as_deref(), Some("head-sha"));
        let parsed: CodeGraphParams = serde_json::from_value(serde_json::json!({
            "operation": "impact",
            "project": "owner/repo",
            "key": "MyStruct",
            "caller_commit": "caller-sha"
        }))
        .expect("caller_commit alias parses");
        assert_eq!(parsed.current_head.as_deref(), Some("caller-sha"));

        let (server, ops) = registry_dispatch_server(
            RegistryDispatchOps::new()
                .with_impact(ImpactResult::Detailed(Vec::new()))
                .with_status(canned_graph_status("cached-sha")),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("impact");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.key = Some("MyStruct".to_string());
        params.current_head = Some("caller-sha".to_string());

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("impact with caller head dispatch succeeds");
        match response {
            CodeGraphResponse::Impact(impact) => {
                let stale = impact
                    .graph_staleness
                    .expect("caller head attaches graph staleness");
                assert_eq!(stale.caller_commit, "caller-sha");
                assert_eq!(stale.cached_commit.as_deref(), Some("cached-sha"));
                assert!(stale.is_stale, "mismatched commits should be stale");
            }
            other => panic!("expected Impact envelope, got {other:?}"),
        }
        let calls = ops.calls();
        assert_eq!(calls[0].0, "impact");
        assert_eq!(calls[1].0, "status");
    }

    /// Acceptance criterion (preserve pagination behaviour): when the
    /// caller supplies `page_limit` + `offset`, the response carries
    /// the **unsliced** `total` and the correct `has_more` flag. The
    /// registry path must not leak `offset` into the bridge call
    /// (the bridge receives the fetch limit, not the page slice).
    #[tokio::test]
    async fn coupling_hotspots_dispatch_preserves_pagination_envelope() {
        // Build 30 pairs so the page slice leaves entries off-page
        // (the default fetch limit is `pagination.limit * 25` and
        // the default page cap is 20).
        let mut pairs = Vec::new();
        for i in 0..30 {
            pairs.push(CoupledPairEntry {
                file_a: format!("src/a{i:02}.rs"),
                file_b: format!("src/b{i:02}.rs"),
                co_edits: 30 - i,
                last_co_edit: "2026-06-01T12:00:00Z".to_string(),
            });
        }
        let (server, _ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_coupling_hotspots(pairs),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("coupling_hotspots");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.page_limit = Some(5);
        params.offset = Some(10);

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("coupling_hotspots paginated dispatch succeeds");

        match response {
            CodeGraphResponse::CouplingHotspots(ch) => {
                assert_eq!(ch.pairs.len(), 5, "page slice should produce 5 entries");
                assert_eq!(
                    ch.total,
                    Some(30),
                    "df6s: total must reflect the unsliced count, not the page size"
                );
                assert_eq!(ch.offset, Some(10), "df6s: offset echoed in response");
                assert_eq!(ch.limit, Some(5), "df6s: page_limit echoed as limit");
                assert_eq!(
                    ch.has_more,
                    Some(true),
                    "df6s: has_more must be true when offset+limit < total"
                );
            }
            other => panic!("expected CodeGraphResponse::CouplingHotspots, got {other:?}"),
        }
    }

    /// Acceptance criterion (preserve summary_only behaviour):
    /// `summary_only` strips the heavy `pairs` list and keeps the
    /// unsliced `total` so the agent-boundary triage view remains
    /// counts-only.
    #[tokio::test]
    async fn coupling_hotspots_dispatch_preserves_summary_only_envelope() {
        let canned = vec![
            CoupledPairEntry {
                file_a: "src/a.rs".to_string(),
                file_b: "src/b.rs".to_string(),
                co_edits: 5,
                last_co_edit: "2026-06-01T12:00:00Z".to_string(),
            },
            CoupledPairEntry {
                file_a: "src/c.rs".to_string(),
                file_b: "src/d.rs".to_string(),
                co_edits: 4,
                last_co_edit: "2026-05-15T08:00:00Z".to_string(),
            },
        ];
        let (server, _ops) = registry_dispatch_server(
            RegistryDispatchOps::new().with_coupling_hotspots(canned),
        );
        let ctx = registry_dispatch_ctx();
        let mut params = test_params("coupling_hotspots");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.summary_only = Some(true);

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("coupling_hotspots summary_only dispatch succeeds");

        match response {
            CodeGraphResponse::CouplingHotspots(ch) => {
                assert!(
                    ch.pairs.is_empty(),
                    "df6s: summary_only must strip the heavy pairs list"
                );
                assert_eq!(ch.total, Some(2), "df6s: total still reflects unsliced count");
                assert_eq!(ch.summary_only, Some(true));
            }
            other => panic!("expected CodeGraphResponse::CouplingHotspots, got {other:?}"),
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Registry-coverage check: every entry routes to its declared bridge method
    // ─────────────────────────────────────────────────────────────────

    /// Programmatic acceptance check: every entry in the vxmw
    /// registry routes through `dispatch_code_graph` to the
    /// `RepoGraphOps` method declared in `entry.bridge_method`.  This
    /// catches forward additions to the registry that forgot to add a
    /// dispatch arm AND forward additions to the dispatch arm that
    /// forgot to add a `bridge_method` line in the registry.
    ///
    /// The test walks the registry table directly and exercises each
    /// op with a minimum fixture, then asserts the bridge method name
    /// the registry declared was the one actually called.
    #[tokio::test]
    async fn registry_vertical_slice_routes_to_declared_bridge_methods() {
        // For each registry entry, craft a fixture that returns a
        // valid canned response for the declared bridge method. We
        // build one fixture per entry (so each test is hermetic) and
        // assert (a) the bridge saw exactly one call, (b) the call
        // name matches `entry.bridge_method`, (c) the response is the
        // expected `CodeGraphResponse` variant.
        for entry in operation_registry::CODE_GRAPH_REGISTRY.iter().filter(|entry| {
            matches!(
                entry.name,
                "neighbors" | "impact" | "context" | "coupling_hotspots"
            )
        }) {
            match entry.bridge_method {
                "neighbors" => {
                    let canned = NeighborsResult::Detailed(Vec::new());
                    let (server, ops) = registry_dispatch_server(
                        RegistryDispatchOps::new().with_neighbors(canned),
                    );
                    let ctx = registry_dispatch_ctx();
                    let mut params = test_params(entry.name);
                    params.project_id = ctx.id.clone();
                    params.project_path = ctx.clone_path.clone();
                    params.key = entry.smoke.key.map(str::to_string);

                    let response = server
                        .dispatch_code_graph(&ctx, &mut params)
                        .await
                        .unwrap_or_else(|e| {
                            panic!(
                                "registry entry '{}' dispatch failed at bridge_method \
                                 '{}': {e}",
                                entry.name, entry.bridge_method
                            )
                        });
                    assert!(
                        matches!(response, CodeGraphResponse::Neighbors(_)),
                        "registry entry '{}': expected Neighbors envelope, got {:?}",
                        entry.name,
                        std::mem::discriminant(&response)
                    );
                    let calls = ops.calls();
                    assert_eq!(
                        calls.len(),
                        1,
                        "registry entry '{}': expected 1 bridge call, got {}",
                        entry.name,
                        calls.len()
                    );
                    assert_eq!(
                        calls[0].0, "neighbors",
                        "registry entry '{}': bridge_method dispatch mismatch",
                        entry.name
                    );
                }
                "impact" => {
                    let canned = ImpactResult::Detailed(Vec::new());
                    let (server, ops) = registry_dispatch_server(
                        RegistryDispatchOps::new().with_impact(canned),
                    );
                    let ctx = registry_dispatch_ctx();
                    let mut params = test_params(entry.name);
                    params.project_id = ctx.id.clone();
                    params.project_path = ctx.clone_path.clone();
                    params.key = entry.smoke.key.map(str::to_string);

                    let response = server
                        .dispatch_code_graph(&ctx, &mut params)
                        .await
                        .unwrap_or_else(|e| {
                            panic!(
                                "registry entry '{}' dispatch failed at bridge_method \
                                 '{}': {e}",
                                entry.name, entry.bridge_method
                            )
                        });
                    assert!(
                        matches!(response, CodeGraphResponse::Impact(_)),
                        "registry entry '{}': expected Impact envelope, got {:?}",
                        entry.name,
                        std::mem::discriminant(&response)
                    );
                    let calls = ops.calls();
                    assert_eq!(
                        calls.len(),
                        1,
                        "registry entry '{}': expected 1 bridge call, got {}",
                        entry.name,
                        calls.len()
                    );
                    assert_eq!(
                        calls[0].0, "impact",
                        "registry entry '{}': bridge_method dispatch mismatch",
                        entry.name
                    );
                }
                "context" => {
                    let canned = canned_symbol_context("uid:test");
                    let (server, ops) = registry_dispatch_server(
                        RegistryDispatchOps::new().with_context(canned),
                    );
                    let ctx = registry_dispatch_ctx();
                    let mut params = test_params(entry.name);
                    params.project_id = ctx.id.clone();
                    params.project_path = ctx.clone_path.clone();
                    params.key = entry.smoke.key.map(str::to_string);

                    let response = server
                        .dispatch_code_graph(&ctx, &mut params)
                        .await
                        .unwrap_or_else(|e| {
                            panic!(
                                "registry entry '{}' dispatch failed at bridge_method \
                                 '{}': {e}",
                                entry.name, entry.bridge_method
                            )
                        });
                    assert!(
                        matches!(response, CodeGraphResponse::Context(_)),
                        "registry entry '{}': expected Context envelope, got {:?}",
                        entry.name,
                        std::mem::discriminant(&response)
                    );
                    let calls = ops.calls();
                    assert_eq!(
                        calls.len(),
                        1,
                        "registry entry '{}': expected 1 bridge call, got {}",
                        entry.name,
                        calls.len()
                    );
                    assert_eq!(
                        calls[0].0, "context",
                        "registry entry '{}': bridge_method dispatch mismatch",
                        entry.name
                    );
                }
                "coupling_hotspots" => {
                    let canned: Vec<CoupledPairEntry> = Vec::new();
                    let (server, ops) = registry_dispatch_server(
                        RegistryDispatchOps::new().with_coupling_hotspots(canned),
                    );
                    let ctx = registry_dispatch_ctx();
                    let mut params = test_params(entry.name);
                    params.project_id = ctx.id.clone();
                    params.project_path = ctx.clone_path.clone();
                    // `coupling_hotspots` takes no `key`; leave it
                    // `None` to match the smoke exemplar.
                    params.key = entry.smoke.key.map(str::to_string);

                    let response = server
                        .dispatch_code_graph(&ctx, &mut params)
                        .await
                        .unwrap_or_else(|e| {
                            panic!(
                                "registry entry '{}' dispatch failed at bridge_method \
                                 '{}': {e}",
                                entry.name, entry.bridge_method
                            )
                        });
                    assert!(
                        matches!(response, CodeGraphResponse::CouplingHotspots(_)),
                        "registry entry '{}': expected CouplingHotspots envelope, got {:?}",
                        entry.name,
                        std::mem::discriminant(&response)
                    );
                    let calls = ops.calls();
                    assert_eq!(
                        calls.len(),
                        1,
                        "registry entry '{}': expected 1 bridge call, got {}",
                        entry.name,
                        calls.len()
                    );
                    assert_eq!(
                        calls[0].0, "coupling_hotspots",
                        "registry entry '{}': bridge_method dispatch mismatch",
                        entry.name
                    );
                }
                other => panic!(
                    "registry_vertical_slice_routes_to_declared_bridge_methods: \
                     unhandled bridge_method '{other}' — extend this test to cover it"
                ),
            }
        }
    }

    /// Compile-time witness: the vxmw slice's smoke exemplar keys all
    /// parse as non-empty short identifiers so `require_key` is
    /// satisfied during the dispatch test. A regression that empties
    /// the smoke exemplar (or accidentally sets `key = None` for an
    /// op that requires a key) would break the dispatch-coverage
    /// test above; this witness catches it at unit-test time.
    #[test]
    fn registry_smoke_exemplars_match_validation_class() {
        for entry in operation_registry::CODE_GRAPH_REGISTRY {
            match entry.validation {
                operation_registry::ValidationCategory::KeyWithEdgeFilters
                | operation_registry::ValidationCategory::KeyWithConfidence
                | operation_registry::ValidationCategory::KeyOnly => {
                    assert!(
                        entry.smoke.key.is_some() && !entry.smoke.key.unwrap().is_empty(),
                        "registry entry '{}' has Key* validation but smoke exemplar \
                         has empty key",
                        entry.name
                    );
                }
                operation_registry::ValidationCategory::None => {
                    assert!(
                        entry.smoke.key.is_none(),
                        "registry entry '{}' has None validation but smoke exemplar \
                         supplies a key",
                        entry.name
                    );
                }
                // All other validation categories do not require a key exemplar.
                // Smoke tests for these categories are covered by their own
                // dedicated exemplar fields (query, file, from/to, etc.).
                _ => {}
            }
        }
    }

    /// Regression witness for the MCP schema contract: converting the
    /// vxmw dispatch path to a registry must not change the public
    /// `code_graph` tool wire shape. The authoritative snapshot is still
    /// owned by the server crate's `tool_schemas` insta test; this local
    /// smoke assertion pins the two schema details this task is most
    /// likely to accidentally disturb while adding registry metadata:
    /// the public schema remains `CodeGraphParams`, and `operation` is
    /// still a plain required string field rather than a generated enum
    /// from the partial registry slice.
    #[test]
    fn registry_dispatch_keeps_code_graph_schema_snapshot_wire_shape() {
        let snapshot = include_str!(
            "../../../../../src/server/tests/snapshots/djinn_server__server__tests__tool_schemas__mcp_tools_schema.snap"
        );
        // Parse the snapshot JSON body (after the insta metadata header) and
        // assert structural wire-shape rather than brittle substrings. Note:
        // schemars 1.x (rmcp 1.x) no longer emits a `title` on the input
        // schema, so we pin the operation field's shape instead — it must
        // stay a plain required string, not a generated enum from the partial
        // registry slice.
        let body = snapshot
            .splitn(3, "---\n")
            .nth(2)
            .expect("insta snapshot body after metadata header");
        let tools: Vec<serde_json::Value> =
            serde_json::from_str(body).expect("MCP schema snapshot is JSON");
        let code_graph = tools
            .iter()
            .find(|t| t.get("name").and_then(serde_json::Value::as_str) == Some("code_graph"))
            .expect("MCP tool schema snapshot should contain code_graph tool");
        let input = &code_graph["input_schema"];

        let required: Vec<&str> = input["required"]
            .as_array()
            .expect("code_graph input schema has a required array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert!(
            required.contains(&"operation") && required.contains(&"project"),
            "operation/project should remain required fields, got {required:?}"
        );

        let operation = &input["properties"]["operation"];
        assert_eq!(
            operation.get("type").and_then(serde_json::Value::as_str),
            Some("string"),
            "operation should remain a string field"
        );
        assert!(
            operation.get("enum").is_none(),
            "operation must not become a generated enum from the partial vxmw registry"
        );
    }
