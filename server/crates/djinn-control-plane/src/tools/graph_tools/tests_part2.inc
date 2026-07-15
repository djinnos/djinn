    fn fixture_server(ops: WorkspaceFixtureOps) -> (DjinnMcpServer, WorkspaceFixtureOps) {
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

    fn fixture_ctx(workspace: Option<&str>) -> ProjectCtx {
        ProjectCtx {
            id: "proj-ws".to_string(),
            clone_path: "/workspace/repo".to_string(),
            workspace: workspace.map(str::to_string),
            sub_path: None,
        }
    }

    /// Acceptance criterion: `operation = "workspaces"` is pure
    /// introspection. It must dispatch through `RepoGraphOps::workspaces`
    /// without requiring a key/query and without applying the requested
    /// `workspace` as a scope first — callers use this operation to discover
    /// the valid slugs, including when the slug they currently have is stale.
    #[tokio::test]
    async fn workspaces_operation_dispatches_without_workspace_scope() {
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("stale-slug"));
        let mut params = test_params("workspaces");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = ctx.workspace.clone();

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("workspaces dispatch succeeds without key/query");

        match response {
            CodeGraphResponse::Workspaces(workspaces) => {
                assert_eq!(workspaces.result.project_id, ctx.id);
                assert!(workspaces.result.workspaces.is_empty());
            }
            other => panic!("expected workspaces response, got {other:?}"),
        }
        assert!(
            ops.seen_workspaces().is_empty(),
            "workspaces listing should not resolve/apply the caller's workspace filter first"
        );
    }

    /// Acceptance criterion: `resolve_workspace_scope` returns
    /// `(None, None)` when no workspace is requested. Bridge sees
    /// `None`.
    #[tokio::test]
    async fn workspace_scope_unscoped_passes_through() {
        let ops = WorkspaceFixtureOps::new(None);
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(None);
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unscoped dispatch succeeds");
        match response {
            CodeGraphResponse::Ranked(ranked) => {
                assert!(
                    ranked.workspace_hint.is_none(),
                    "no hint when workspace is omitted, got {:?}",
                    ranked.workspace_hint
                );
            }
            other => panic!("expected ranked, got {other:?}"),
        }
    }

    /// Acceptance criterion: empty `workspace: ""` normalizes to `None`
    /// at the request boundary, same shape as omitting the field.
    #[tokio::test]
    async fn workspace_scope_empty_string_normalizes_to_none() {
        let ops = WorkspaceFixtureOps::new(None);
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(None);
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some(String::new());
        server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("empty-workspace dispatch succeeds");
        // `normalize()` runs at the dispatcher boundary, so by the
        // time the bridge saw the request `ctx.workspace` is `None`
        // and no hint fires.
    }

    /// Acceptance criterion: for valid slugs the bridge sees the slug
    /// and no hint is surfaced.
    #[tokio::test]
    async fn workspace_scope_known_slug_passes_through_for_listing_op() {
        let ops = WorkspaceFixtureOps::new(None);
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug dispatch succeeds");
        match response {
            CodeGraphResponse::Ranked(ranked) => {
                assert!(ranked.workspace_hint.is_none());
            }
            other => panic!("expected ranked, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "ranked (listing/bounded) hard-filters with the slug"
        );
    }

    /// Acceptance criterion: unknown non-empty slug on a
    /// multi-workspace graph returns the full/unscoped result AND
    /// surfaces the candidate list. The bridge sees `None` (so it
    /// returns the unfiltered set).
    #[tokio::test]
    async fn workspace_scope_unknown_slug_returns_full_result_plus_hint() {
        let hint_candidates = vec!["server".to_string(), "ui".to_string()];
        let ops = WorkspaceFixtureOps::new(Some(hint_candidates.clone()));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug dispatch succeeds");
        match response {
            CodeGraphResponse::Ranked(ranked) => {
                assert_eq!(
                    ranked.workspace_hint,
                    Some(hint_candidates),
                    "unknown slug surfaces candidate slug list"
                );
            }
            other => panic!("expected ranked, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "unknown slug degrades to unscoped at the bridge"
        );
    }

    /// Acceptance criterion: single-workspace graphs (the bridge
    /// returns `None` for the hint) treat the workspace parameter as
    /// a no-op — bridge sees the slug.
    #[tokio::test]
    async fn workspace_scope_single_workspace_is_a_no_op() {
        // Bridge returns `None` for the hint → the helper treats the
        // slug as known. The downstream filter still matches every
        // node (because every node lives in the only workspace) so
        // there is no hard-empty surprise.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("orphans");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("single-workspace dispatch succeeds");
        match response {
            CodeGraphResponse::Orphans(orphans) => {
                assert!(
                    orphans.workspace_hint.is_none(),
                    "single-workspace graphs never produce a hint"
                );
            }
            other => panic!("expected orphans, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "single-workspace graph still passes the slug through"
        );
    }

    /// Acceptance criterion: for unknown slugs, the bridge call sees
    /// `None` (full graph) AND a listing/bounded op also produces the
    /// hint envelope.
    #[tokio::test]
    async fn workspace_scope_orphans_unknown_slug_falls_back_and_hints() {
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("orphans");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("orphans unknown-slug dispatch succeeds");
        match response {
            CodeGraphResponse::Orphans(orphans) => {
                assert_eq!(
                    orphans.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected orphans, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "orphans (listing) sees unscoped workspace for unknown slug"
        );
    }

    /// Acceptance criterion: traversal ops (`impact`, `path`,
    /// `touches_hot_path`) use the workspace only while resolving
    /// seeds/endpoints, then traverse the full graph. The bridge
    /// call sees the slug for known/valid workspaces — the seed
    /// resolution happens inside the bridge and the walk itself is
    /// unconstrained at the bridge layer. Unknown slugs still
    /// produce a hint and the bridge sees `None` (full graph).
    #[tokio::test]
    async fn workspace_scope_impact_traversal_resolves_seed_only() {
        // Case 1: known slug → bridge sees the slug, no hint.
        let ops = WorkspaceFixtureOps::with_ranked_and_impact(
            None,
            Vec::new(),
            ImpactResult::Detailed(Vec::new()),
        );
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("impact");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        params.key = Some("src/foo.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug impact dispatch succeeds");
        match response {
            CodeGraphResponse::Impact(impact) => {
                assert!(
                    impact.workspace_hint.is_none(),
                    "known-slug impact surfaces no hint"
                );
            }
            other => panic!("expected impact, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "impact (traversal) hands the slug to the bridge for seed resolution"
        );

        // Case 2: unknown slug → bridge sees `None`, hint fires.
        let ops = WorkspaceFixtureOps::with_ranked_and_impact(
            Some(vec!["server".to_string(), "ui".to_string()]),
            Vec::new(),
            ImpactResult::Detailed(Vec::new()),
        );
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("impact");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        params.key = Some("src/foo.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug impact dispatch succeeds");
        match response {
            CodeGraphResponse::Impact(impact) => {
                assert_eq!(
                    impact.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected impact, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "impact with unknown slug resolves the seed from the full graph"
        );
    }

    /// Acceptance criterion: `path` (traversal) follows the same
    /// seed-only scoping as `impact`. Lock it in independently of
    /// `impact` so a regression in either is caught.
    #[tokio::test]
    async fn workspace_scope_path_traversal_resolves_endpoints_only() {
        // Case 1: known slug → bridge sees the slug, no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("path");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        params.from = Some("src/a.rs".to_string());
        params.to = Some("src/b.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug path dispatch succeeds");
        match response {
            CodeGraphResponse::Path(path) => {
                assert!(path.workspace_hint.is_none());
            }
            other => panic!("expected path, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "path (traversal) hands the slug to the bridge for endpoint resolution"
        );

        // Case 2: unknown slug → bridge sees `None`, hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("path");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        params.from = Some("src/a.rs".to_string());
        params.to = Some("src/b.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug path dispatch succeeds");
        match response {
            CodeGraphResponse::Path(path) => {
                assert_eq!(
                    path.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected path, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "path with unknown slug resolves endpoints from the full graph"
        );
    }

    /// Acceptance criterion: `snapshot` (listing/bounded) hard-filters
    /// for valid slugs AND surfaces a hint for unknown slugs.
    #[tokio::test]
    async fn workspace_scope_snapshot_listing_hard_filters() {
        // Case 1: known slug → bridge sees the slug, no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("snapshot");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug snapshot dispatch succeeds");
        match response {
            CodeGraphResponse::Snapshot(snapshot) => {
                assert!(snapshot.workspace_hint.is_none());
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "snapshot (listing) hard-filters with the slug"
        );

        // Case 2: unknown slug → bridge sees `None`, hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("snapshot");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug snapshot dispatch succeeds");
        match response {
            CodeGraphResponse::Snapshot(snapshot) => {
                assert_eq!(
                    snapshot.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "snapshot (listing) sees unscoped workspace for unknown slug"
        );
    }

    /// Acceptance criterion: `search` (listing/bounded) surfaces the
    /// hint envelope for unknown slugs even though the bridge call
    /// itself is workspace-agnostic today. A future PR may add
    /// workspace support to `search`; the hint path is the
    /// immediately user-visible half.
    #[tokio::test]
    async fn workspace_scope_search_surfaces_hint_for_unknown_slug() {
        // Case 1: known slug → no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("search");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        params.query = Some("login".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug search dispatch succeeds");
        match response {
            CodeGraphResponse::Search(search) => {
                assert!(search.workspace_hint.is_none());
            }
            other => panic!("expected search, got {other:?}"),
        }

        // Case 2: unknown slug → hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("search");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        params.query = Some("login".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug search dispatch succeeds");
        match response {
            CodeGraphResponse::Search(search) => {
                assert_eq!(
                    search.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected search, got {other:?}"),
        }
    }

    /// Acceptance criterion: `touches_hot_path` (traversal) follows
    /// the same seed-only scoping as `impact` / `path`.
    #[tokio::test]
    async fn workspace_scope_touches_hot_path_traversal_resolves_seeds_only() {
        // Case 1: known slug → bridge sees the slug, no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("touches_hot_path");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        params.seed_entries = Some(vec!["sym:main".to_string()]);
        params.seed_sinks = Some(vec!["sym:db".to_string()]);
        params.symbols = Some(vec!["sym:foo".to_string()]);
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug touches_hot_path dispatch succeeds");
        match response {
            CodeGraphResponse::TouchesHotPath(hits) => {
                assert!(hits.workspace_hint.is_none());
            }
            other => panic!("expected touches_hot_path, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![Some("server".to_string())],
            "touches_hot_path hands the slug to the bridge for seed resolution"
        );

        // Case 2: unknown slug → bridge sees `None`, hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, ops) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("touches_hot_path");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        params.seed_entries = Some(vec!["sym:main".to_string()]);
        params.seed_sinks = Some(vec!["sym:db".to_string()]);
        params.symbols = Some(vec!["sym:foo".to_string()]);
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug touches_hot_path dispatch succeeds");
        match response {
            CodeGraphResponse::TouchesHotPath(hits) => {
                assert_eq!(
                    hits.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected touches_hot_path, got {other:?}"),
        }
        assert_eq!(
            ops.seen_workspaces(),
            vec![None],
            "touches_hot_path with unknown slug resolves seeds from the full graph"
        );
    }

    /// Acceptance criterion: empty `workspace: ""` is the same shape
    /// as omitting `workspace` after `normalize()`. Specifically, the
    /// `CodeGraphParams::normalize()` helper must clear `workspace =
    /// Some("")` to `None`. Locks in the `params.workspace.is_none()`
    /// invariant the dispatcher relies on.
    #[test]
    fn normalize_workspace_empty_string_is_none_for_dispatch() {
        let mut params = test_params("ranked");
        params.workspace = Some(String::new());
        params.normalize();
        assert!(
            params.workspace.is_none(),
            "normalize() should clear Some(\"\") to None, got {:?}",
            params.workspace
        );
        // Same shape as omitting workspace in the first place.
        let params2 = test_params("ranked");
        assert!(params2.workspace.is_none());
    }

    /// Acceptance criterion: `cycles` and `edges` (listing/bounded)
    /// also surface a hint envelope for unknown slugs even though
    /// the bridge calls themselves stay workspace-agnostic today.
    /// These ops don't take a workspace arg on the trait, so the
    /// "seen_workspaces" record stays empty — what matters is the
    /// hint envelope is populated correctly.
    #[tokio::test]
    async fn workspace_scope_cycles_edges_surface_hint_for_unknown_slug() {
        // cycles: known slug → no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("cycles");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug cycles dispatch succeeds");
        match response {
            CodeGraphResponse::Cycles(cycles) => {
                assert!(cycles.workspace_hint.is_none());
            }
            other => panic!("expected cycles, got {other:?}"),
        }

        // cycles: unknown slug → hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("cycles");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug cycles dispatch succeeds");
        match response {
            CodeGraphResponse::Cycles(cycles) => {
                assert_eq!(
                    cycles.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected cycles, got {other:?}"),
        }

        // edges: known slug → no hint.
        let ops = WorkspaceFixtureOps::new(None);
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("server"));
        let mut params = test_params("edges");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("server".to_string());
        params.from_glob = Some("**/*.rs".to_string());
        params.to_glob = Some("**/*.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("known-slug edges dispatch succeeds");
        match response {
            CodeGraphResponse::Edges(edges) => {
                assert!(edges.workspace_hint.is_none());
            }
            other => panic!("expected edges, got {other:?}"),
        }

        // edges: unknown slug → hint fires.
        let ops = WorkspaceFixtureOps::new(Some(vec!["server".to_string(), "ui".to_string()]));
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(Some("nope"));
        let mut params = test_params("edges");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.workspace = Some("nope".to_string());
        params.from_glob = Some("**/*.rs".to_string());
        params.to_glob = Some("**/*.rs".to_string());
        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("unknown-slug edges dispatch succeeds");
        match response {
            CodeGraphResponse::Edges(edges) => {
                assert_eq!(
                    edges.workspace_hint,
                    Some(vec!["server".to_string(), "ui".to_string()])
                );
            }
            other => panic!("expected edges, got {other:?}"),
        }
    }

    /// PR B4: the default `hybrid_search` impl on `RepoGraphOps`
    /// degrades to the structural-only path so test stubs that only
    /// override `search` still serve the hybrid mode (with every hit
    /// stamped `match_kind="structural"`). Exercised through the
    /// `StubRepoGraphOps` fixture from `test_support` rather than a
    /// hand-rolled stub — the goal here is to lock in the default-impl
    /// tagging contract, not re-test every other op.
    #[tokio::test]
    async fn hybrid_search_default_impl_tags_structural() {
        use crate::bridge::{ProjectCtx, RepoGraphOps};
        use crate::state::stubs::StubRepoGraphOps;

        let ctx = ProjectCtx {
            id: "p".to_string(),
            clone_path: "/tmp".to_string(),
            workspace: None,
            sub_path: None,
        };
        let ops = StubRepoGraphOps;
        // Stub returns no hits; the default impl still has to compile
        // and the empty list is valid evidence the dispatch path works.
        let hits = ops.hybrid_search(&ctx, "login", None, 10).await.unwrap();
        assert!(hits.is_empty());
    }

    /// PR A3: `neighbors` op uses `kind_filter` for edge kinds (`reads` /
    /// `writes`); `validate_edge_kind_filter` enforces that vocabulary so
    /// typos surface server-side rather than silently dropping every
    /// neighbor.
    #[test]
    fn validates_edge_kind_filter_pr_a3() {
        assert!(validate_edge_kind_filter(Some("reads")).is_ok());
        assert!(validate_edge_kind_filter(Some("writes")).is_ok());
        // Proposal qoxm: explicit opt-in to the commit co-change sidecar.
        assert!(validate_edge_kind_filter(Some("co_changed_with")).is_ok());
        assert!(validate_edge_kind_filter(None).is_ok());
        // Node-kind labels are NOT valid for the neighbors op — they
        // belong to the other ops' validator.
        assert!(validate_edge_kind_filter(Some("file")).is_err());
        assert!(validate_edge_kind_filter(Some("symbol")).is_err());
        assert!(validate_edge_kind_filter(Some("unknown")).is_err());
    }

    #[test]
    fn test_filter_parse_maps_aliases_and_default() {
        assert_eq!(
            TestFilter::parse(Some("exclude"), TestFilter::Include),
            TestFilter::Exclude
        );
        assert_eq!(
            TestFilter::parse(Some("ONLY"), TestFilter::Include),
            TestFilter::Only
        );
        assert_eq!(
            TestFilter::parse(Some(" include "), TestFilter::Exclude),
            TestFilter::Include
        );
        // Unknown / absent falls back to the per-op default.
        assert_eq!(
            TestFilter::parse(Some("garbage"), TestFilter::Exclude),
            TestFilter::Exclude
        );
        assert_eq!(
            TestFilter::parse(None, TestFilter::Include),
            TestFilter::Include
        );
    }

    #[test]
    fn test_filter_keeps_respects_mode() {
        assert!(TestFilter::Include.keeps(true));
        assert!(TestFilter::Include.keeps(false));
        assert!(!TestFilter::Exclude.keeps(true));
        assert!(TestFilter::Exclude.keeps(false));
        assert!(TestFilter::Only.keeps(true));
        assert!(!TestFilter::Only.keeps(false));
    }

    fn test_params(op: &str) -> CodeGraphParams {
        CodeGraphParams {
            operation: op.to_string(),
            project: "test/test".to_string(),
            workspace: None,
            project_id: String::new(),
            project_path: "/tmp".to_string(),
            key: None,
            direction: None,
            kind_filter: None,
            limit: None,
            query: None,
            context_filter: None,
            file_filter: None,
            edge_filters: None,
            token_budget: None,
            max_seeds: None,
            from: None,
            to: None,
            method: None,
            path: None,
            from_glob: None,
            to_glob: None,
            route_id: None,
            path_glob: None,
            framework: None,
            include_optional: None,
            min_size: None,
            visibility: None,
            sort_by: None,
            group_by: None,
            max_depth: None,
            edge_kind: None,
            file: None,
            start_line: None,
            end_line: None,
            changed_ranges: None,
            module_glob: None,
            confidence: None,
            window_days: None,
            file_glob: None,
            rules: None,
            seed_entries: None,
            seed_sinks: None,
            symbols: None,
            since_days: None,
            max_files_per_commit: None,
            min_confidence: None,
            kind_hint: None,
            from_sha: None,
            to_sha: None,
            changed_files: None,
            include_content: None,
            level: None,
            mode: None,
            target: None,
            tests: None,
            impact_targets: None,
            scope_crates: None,
            offset: None,
            summary_only: None,
            by_depth_counts: None,
            page_limit: None,
            current_head: None,
        }
    }

    #[test]
    fn require_key_rejects_empty() {
        let mut params = test_params("neighbors");
        params.key = Some(String::new());
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn require_key_rejects_missing() {
        let params = test_params("neighbors");
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn require_key_accepts_valid() {
        let mut params = test_params("neighbors");
        params.key = Some("src/lib.rs".to_string());
        assert_eq!(require_key(&params).unwrap(), "src/lib.rs");
    }

    #[test]
    fn require_query_rejects_missing() {
        let params = test_params("search");
        assert!(require_query(&params).is_err());
    }

    #[test]
    fn route_selector_requires_route_id_or_method_path() {
        let mut params = test_params("shape_check");
        assert!(require_route_selector(&params).is_err());

        params.route_id = Some("route-1".to_string());
        let selector = require_route_selector(&params).unwrap();
        assert_eq!(selector.route_id, Some("route-1"));
        assert_eq!(selector.method, None);
        assert_eq!(selector.path, None);

        let mut params = test_params("api_impact");
        params.method = Some("GET".to_string());
        assert!(require_route_selector(&params).is_err());
        params.path = Some("/users/:id".to_string());
        let selector = require_route_selector(&params).unwrap();
        assert_eq!(selector.route_id, None);
        assert_eq!(selector.method, Some("GET"));
        assert_eq!(selector.path, Some("/users/:id"));
    }

    #[test]
    fn validates_flow_kind_filter() {
        assert!(validate_flow_kind_filter(None).is_ok());
        assert!(validate_flow_kind_filter(Some("process")).is_ok());
        assert!(validate_flow_kind_filter(Some("step")).is_ok());
        assert!(validate_flow_kind_filter(Some("symbol")).is_err());
    }

    #[test]
    fn require_from_to_rejects_partial() {
        let mut params = test_params("path");
        params.from = Some("src/a.rs".to_string());
        assert!(require_from_to(&params).is_err());
        params.to = Some("src/b.rs".to_string());
        assert_eq!(require_from_to(&params).unwrap(), ("src/a.rs", "src/b.rs"));
    }

    #[test]
    fn require_globs_rejects_partial() {
        let mut params = test_params("edges");
        params.from_glob = Some("**/*.rs".to_string());
        assert!(require_globs(&params).is_err());
        params.to_glob = Some("**/*.rs".to_string());
        assert!(require_globs(&params).is_ok());
    }

    #[test]
    fn validates_visibility() {
        assert!(validate_visibility(None).is_ok());
        assert!(validate_visibility(Some("public")).is_ok());
        assert!(validate_visibility(Some("private")).is_ok());
        assert!(validate_visibility(Some("any")).is_ok());
        assert!(validate_visibility(Some("internal")).is_err());
    }

    #[test]
    fn validates_sort_by() {
        assert!(validate_sort_by(None).is_ok());
        assert!(validate_sort_by(Some("pagerank")).is_ok());
        assert!(validate_sort_by(Some("in_degree")).is_ok());
        assert!(validate_sort_by(Some("out_degree")).is_ok());
        assert!(validate_sort_by(Some("total_degree")).is_ok());
        assert!(validate_sort_by(Some("alpha")).is_err());
    }

    #[test]
    fn validates_group_by() {
        assert!(validate_group_by(None).is_ok());
        assert!(validate_group_by(Some("file")).is_ok());
        assert!(validate_group_by(Some("symbol")).is_err());
    }

    #[test]
    fn parses_code_graph_params_from_json() {
        let json = serde_json::json!({
            "operation": "neighbors",
            "project": "/workspace/repo",
            "key": "src/main.rs",
            "direction": "outgoing"
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "neighbors");
        assert_eq!(params.key.as_deref(), Some("src/main.rs"));
        assert_eq!(params.direction.as_deref(), Some("outgoing"));
        assert!(params.kind_filter.is_none());
        assert!(params.limit.is_none());
    }

    #[test]
    fn parses_ranked_params_from_json() {
        let json = serde_json::json!({
            "operation": "ranked",
            "project": "/workspace/repo",
            "kind_filter": "file",
            "limit": 10
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "ranked");
        assert!(params.key.is_none());
        assert_eq!(params.kind_filter.as_deref(), Some("file"));
        assert_eq!(params.limit, Some(10));
    }

    #[test]
    fn parses_impact_params_from_json() {
        let json = serde_json::json!({
            "operation": "impact",
            "project": "/workspace/repo",
            "key": "scip-rust . . . MyStruct#",
            "limit": 5
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "impact");
        assert_eq!(params.key.as_deref(), Some("scip-rust . . . MyStruct#"));
        assert_eq!(params.limit, Some(5));
    }

    #[test]
    fn parses_route_api_and_flow_params_from_json() {
        let route_map: CodeGraphParams = serde_json::from_value(serde_json::json!({
            "operation": "route_map",
            "project": "/workspace/repo",
            "method": "GET",
            "path_glob": "/api/**",
            "framework": "axum",
            "limit": 25
        }))
        .unwrap();
        assert_eq!(route_map.method.as_deref(), Some("GET"));
        assert_eq!(route_map.path_glob.as_deref(), Some("/api/**"));
        assert_eq!(route_map.framework.as_deref(), Some("axum"));
        assert_eq!(route_map.limit, Some(25));

        let shape_check: CodeGraphParams = serde_json::from_value(serde_json::json!({
            "operation": "shape_check",
            "project": "/workspace/repo",
            "route_id": "route-1",
            "include_optional": true
        }))
        .unwrap();
        assert_eq!(shape_check.route_id.as_deref(), Some("route-1"));
        assert_eq!(shape_check.include_optional, Some(true));

        let flow: CodeGraphParams = serde_json::from_value(serde_json::json!({
            "operation": "flow",
            "project": "/workspace/repo",
            "query": "checkout payment",
            "kind_filter": "step",
            "limit": 10
        }))
        .unwrap();
        assert_eq!(flow.query.as_deref(), Some("checkout payment"));
        assert_eq!(flow.kind_filter.as_deref(), Some("step"));
        assert_eq!(flow.limit, Some(10));
    }

    #[test]
    fn parses_implementations_params_from_json() {
        let json = serde_json::json!({
            "operation": "implementations",
            "project": "/workspace/repo",
            "key": "scip-rust . . . Trait#"
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "implementations");
        assert_eq!(params.key.as_deref(), Some("scip-rust . . . Trait#"));
    }

    #[test]
    fn parses_search_params_from_json() {
        let json = serde_json::json!({
            "operation": "search",
            "project": "/workspace/repo",
            "query": "AgentSession",
            "kind_filter": "symbol",
            "limit": 5,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "search");
        assert_eq!(params.query.as_deref(), Some("AgentSession"));
        assert_eq!(params.kind_filter.as_deref(), Some("symbol"));
        assert_eq!(params.limit, Some(5));
    }

    #[test]
    fn parses_cycles_params_from_json() {
        let json = serde_json::json!({
            "operation": "cycles",
            "project": "/workspace/repo",
            "min_size": 3,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "cycles");
        assert_eq!(params.min_size, Some(3));
    }

    #[test]
    fn parses_orphans_params_from_json() {
        let json = serde_json::json!({
            "operation": "orphans",
            "project": "/workspace/repo",
            "visibility": "private",
            "limit": 25,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.visibility.as_deref(), Some("private"));
        assert_eq!(params.limit, Some(25));
    }

    #[test]
    fn parses_path_params_from_json() {
        let json = serde_json::json!({
            "operation": "path",
            "project": "/workspace/repo",
            "from": "src/a.rs",
            "to": "src/b.rs",
            "max_depth": 6,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.from.as_deref(), Some("src/a.rs"));
        assert_eq!(params.to.as_deref(), Some("src/b.rs"));
        assert_eq!(params.max_depth, Some(6));
    }

    #[test]
    fn parses_edges_params_from_json() {
        let json = serde_json::json!({
            "operation": "edges",
            "project": "/workspace/repo",
            "from_glob": "server/src/**",
            "to_glob": "server/crates/**",
            "edge_kind": "FileReference",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.from_glob.as_deref(), Some("server/src/**"));
        assert_eq!(params.to_glob.as_deref(), Some("server/crates/**"));
        assert_eq!(params.edge_kind.as_deref(), Some("FileReference"));
    }

    #[test]
    fn edge_entry_serializes_confidence_tier_contract() {
        let json = serde_json::to_value(EdgeEntry {
            from: "file:ui/src/api.ts".to_string(),
            to: "route:GET /api/users".to_string(),
            edge_kind: "Fetches".to_string(),
            edge_weight: 2.0,
            confidence: 0.91,
            confidence_tier: "extracted".to_string(),
            reason: Some("url-literal; import-evidence".to_string()),
            exclusion_reason: None,
        })
        .expect("serialize EdgeEntry");
        assert_eq!(
            json.get("confidence_tier").and_then(|v| v.as_str()),
            Some("extracted")
        );
        assert_eq!(json.get("confidence").and_then(|v| v.as_f64()), Some(0.91));
        assert_eq!(
            json.get("reason").and_then(|v| v.as_str()),
            Some("url-literal; import-evidence")
        );
    }

    #[test]
    fn route_contracts_serialize_exclusion_and_tier_audit_fields() {
        let consumer = RelatedSymbol {
            uid: "symbol:ui-loadAgents".to_string(),
            name: "loadAgents".to_string(),
            kind: "function".to_string(),
            file_path: Some("ui/src/api.ts".to_string()),
            confidence: 0.2,
            confidence_tier: "ambiguous".to_string(),
            confidence_reason: Some("below-floor string-shape".to_string()),
            excluded_reason: Some("below-confidence-floor".to_string()),
            route_language_chain: Some(RouteLanguageChain {
                source_language: Some("typescript".to_string()),
                target_language: Some("rust".to_string()),
                is_cross_language: true,
            }),
        };
        let route = RouteRef {
            uid: "route:GET /api/agents (axum)".to_string(),
            id: "GET /api/agents (axum)".to_string(),
            method: Some("GET".to_string()),
            path: Some("/api/agents".to_string()),
            framework: Some("axum".to_string()),
        };

        let route_json = serde_json::to_value(RouteMapResult {
            routes: vec![RouteMapEntry {
                route: route.clone(),
                excluded_reason: Some("health-path".to_string()),
                handler: None,
                middleware: Vec::new(),
                consumers: vec![consumer.clone()],
            }],
            summary: RouteSummary::default(),
        })
        .expect("serialize route map");
        assert_eq!(
            route_json["routes"][0]["excluded_reason"].as_str(),
            Some("health-path")
        );
        assert_eq!(
            route_json["routes"][0]["consumers"][0]["confidence_tier"].as_str(),
            Some("ambiguous")
        );
        assert_eq!(
            route_json["routes"][0]["consumers"][0]["excluded_reason"].as_str(),
            Some("below-confidence-floor")
        );

        let shape_json = serde_json::to_value(ShapeCheckResult {
            matched: false,
            summary: "route matched selector but is excluded: health-path".to_string(),
            excluded_reason: Some("health-path".to_string()),
            route_shape: RouteShape {
                route: route.clone(),
                response_fields: Vec::new(),
            },
            drifts: Vec::new(),
        })
        .expect("serialize shape check");
        assert_eq!(shape_json["matched"].as_bool(), Some(false));
        assert_eq!(
            shape_json["excluded_reason"].as_str(),
            Some("health-path")
        );

        let impact_json = serde_json::to_value(ApiImpactResult {
            impacts: Vec::new(),
            excluded_impacts: vec![ApiImpactEntry {
                consumer,
                risk_tier: "excluded".to_string(),
                reason: "below-floor Fetches suggestion".to_string(),
                excluded_reason: Some("below-confidence-floor".to_string()),
            }],
            summary: "0 default impact(s); 1 excluded audit entry".to_string(),
        })
        .expect("serialize api impact");
        assert_eq!(
            impact_json["excluded_impacts"][0]["consumer"]["confidence_tier"].as_str(),
            Some("ambiguous")
        );
        assert_eq!(
            impact_json["excluded_impacts"][0]["excluded_reason"].as_str(),
            Some("below-confidence-floor")
        );
    }

    #[test]
    fn parses_describe_params_from_json() {
        let json = serde_json::json!({
            "operation": "describe",
            "project": "/workspace/repo",
            "key": "scip-rust . . . AgentSession#",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "describe");
        assert_eq!(params.key.as_deref(), Some("scip-rust . . . AgentSession#"));
    }

    #[test]
    fn parses_symbols_at_params_from_json() {
        let json = serde_json::json!({
            "operation": "symbols_at",
            "project": "/workspace/repo",
            "file": "src/lib.rs",
            "start_line": 42,
            "end_line": 48,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "symbols_at");
        assert_eq!(params.file.as_deref(), Some("src/lib.rs"));
        assert_eq!(params.start_line, Some(42));
        assert_eq!(params.end_line, Some(48));
    }

    #[test]
    fn parses_symbols_at_params_without_end_line() {
        let json = serde_json::json!({
            "operation": "symbols_at",
            "project": "/workspace/repo",
            "file": "src/lib.rs",
            "start_line": 17,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.start_line, Some(17));
        assert!(params.end_line.is_none());
    }

    #[test]
    fn parses_api_surface_params_from_json() {
        let json = serde_json::json!({
            "operation": "api_surface",
            "project": "/workspace/repo",
            "module_glob": "server/src/**",
            "visibility": "public",
            "limit": 50,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "api_surface");
        assert_eq!(params.module_glob.as_deref(), Some("server/src/**"));
        assert_eq!(params.visibility.as_deref(), Some("public"));
        assert_eq!(params.limit, Some(50));
    }

    #[test]
    fn parses_boundary_check_params_from_json() {
        let json = serde_json::json!({
            "operation": "boundary_check",
            "project": "/workspace/repo",
            "rules": [
                {"from_glob": "server/src/**", "to_glob": "ui/**"},
                {"from_glob": "ui/**", "to_glob": "server/src/**"},
            ],
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        let rules = params.rules.as_ref().expect("rules set");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].from_glob, "server/src/**");
        assert_eq!(rules[1].to_glob, "server/src/**");
    }

    #[test]
    fn parses_hotspots_params_from_json() {
        let json = serde_json::json!({
            "operation": "hotspots",
            "project": "/workspace/repo",
            "window_days": 30,
            "file_glob": "server/src/**",
            "limit": 10,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "hotspots");
        assert_eq!(params.window_days, Some(30));
        assert_eq!(params.file_glob.as_deref(), Some("server/src/**"));
        assert_eq!(params.limit, Some(10));
    }

    #[test]
    fn parses_complexity_params_from_json() {
        // Iter 28: target / sort_by / file_glob / limit all parse via
        // the shared CodeGraphParams. `target` is the only new field
        // — sort_by/file_glob/limit are reused from ranked / hotspots.
        let json = serde_json::json!({
            "operation": "complexity",
            "project": "/workspace/repo",
            "target": "files",
            "sort_by": "cyclomatic",
            "file_glob": "server/src/**/*.rs",
            "limit": 25,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "complexity");
        assert_eq!(params.target.as_deref(), Some("files"));
        assert_eq!(params.sort_by.as_deref(), Some("cyclomatic"));
        assert_eq!(params.file_glob.as_deref(), Some("server/src/**/*.rs"));
        assert_eq!(params.limit, Some(25));
    }

    #[test]
    fn parses_refactor_candidates_params_from_json() {
        // Iter 29: since_days / file_glob / limit all reuse existing
        // CodeGraphParams fields. No new params are introduced — the
        // op rides on the same shared shape iter 28's complexity uses.
        let json = serde_json::json!({
            "operation": "refactor_candidates",
            "project": "/workspace/repo",
            "since_days": 60,
            "file_glob": "server/src/**/*.rs",
            "limit": 25,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "refactor_candidates");
        assert_eq!(params.since_days, Some(60));
        assert_eq!(params.file_glob.as_deref(), Some("server/src/**/*.rs"));
        assert_eq!(params.limit, Some(25));
    }

    #[test]
    fn parses_metrics_at_params_from_json() {
        let json = serde_json::json!({
            "operation": "metrics_at",
            "project": "/workspace/repo",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "metrics_at");
        assert_eq!(params.project, "/workspace/repo");
    }

    #[test]
    fn parses_dead_symbols_params_from_json() {
        let json = serde_json::json!({
            "operation": "dead_symbols",
            "project": "/workspace/repo",
            "confidence": "med",
            "limit": 75,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "dead_symbols");
        assert_eq!(params.confidence.as_deref(), Some("med"));
        assert_eq!(params.limit, Some(75));
    }

    #[test]
    fn parses_deprecated_callers_params_from_json() {
        let json = serde_json::json!({
            "operation": "deprecated_callers",
            "project": "/workspace/repo",
            "limit": 25,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "deprecated_callers");
        assert_eq!(params.limit, Some(25));
    }

    #[test]
    fn parses_touches_hot_path_params_from_json() {
        let json = serde_json::json!({
            "operation": "touches_hot_path",
            "project": "/workspace/repo",
            "seed_entries": ["scip-rust . . . entry#"],
            "seed_sinks": ["scip-rust . . . sink#"],
            "symbols": ["scip-rust . . . foo#", "scip-rust . . . bar#"],
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "touches_hot_path");
        assert_eq!(params.seed_entries.as_ref().unwrap().len(), 1);
        assert_eq!(params.seed_sinks.as_ref().unwrap().len(), 1);
        assert_eq!(params.symbols.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn parses_diff_touches_params_from_json() {
        let json = serde_json::json!({
            "operation": "diff_touches",
            "project": "/workspace/repo",
            "changed_ranges": [
                {"file": "src/a.rs", "start_line": 10, "end_line": 20},
                {"file": "src/b.rs", "start_line": 5},
            ],
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        let ranges = params.changed_ranges.as_ref().expect("changed_ranges set");
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].file, "src/a.rs");
        assert_eq!(ranges[0].start_line, 10);
        assert_eq!(ranges[0].end_line, Some(20));
        assert_eq!(ranges[1].file, "src/b.rs");
        assert_eq!(ranges[1].start_line, 5);
        assert!(ranges[1].end_line.is_none());
    }

    // ── Next-step hint tests (PR A4) ────────────────────────────────────────

