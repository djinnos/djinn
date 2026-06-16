    fn search_with_top_hit(name: &str) -> CodeGraphResponse {
        CodeGraphResponse::Search(SearchResponse {
            query: "auth".to_string(),
            hits: vec![SearchHit {
                key: format!("scip-rust . . . {name}#"),
                uid: format!("scip-rust . . . {name}#"),
                kind: "function".to_string(),
                display_name: name.to_string(),
                score: 0.9,
                file: Some("src/auth.rs".to_string()),
                match_kind: None,
            }],
            workspace_hint: None,
            next_step: None,
        })
    }

    fn empty_search() -> CodeGraphResponse {
        CodeGraphResponse::Search(SearchResponse {
            query: "auth".to_string(),
            hits: vec![],
            workspace_hint: None,
            next_step: None,
        })
    }

    fn empty_ranked() -> CodeGraphResponse {
        CodeGraphResponse::Ranked(RankedResponse {
            nodes: vec![],
            workspace_hint: None,
            next_step: None,
        })
    }

    fn empty_cycles() -> CodeGraphResponse {
        CodeGraphResponse::Cycles(CyclesResponse {
            cycles: vec![],
            workspace_hint: None,
            next_step: None,
        })
    }

    fn empty_impact() -> CodeGraphResponse {
        CodeGraphResponse::Impact(ImpactResponse {
            key: "Foo".to_string(),
            impact: Some(vec![]),
            file_groups: None,
            risk: Some(ImpactRisk::Low),
            summary: Some("no direct callers in current graph snapshot".to_string()),
            workspace_hint: None,
            // df6s: pagination fields default to `None` for
            // non-paginated responses, matching the pre-pagination
            // wire shape.
            total: None,
            offset: None,
            limit: None,
            has_more: None,
            summary_only: None,
            by_depth_counts: None,
            next_step: None,
        })
    }

    /// PR C3 helper: build an ImpactResponse with N synthetic direct
    /// callers spread across `modules` two-segment buckets. Used to
    /// exercise risk thresholds without a full graph fixture.
    fn impact_with_callers(direct: usize, modules: usize) -> CodeGraphResponse {
        let entries: Vec<ImpactEntry> = (0..direct)
            .map(|i| {
                let bucket = i % modules.max(1);
                ImpactEntry {
                    // uid: test-only synthetic symbol key mirrors key.
                    uid: format!("symbol:scip-rust pkg src/m{bucket}/f{i}.rs `caller{i}`()."),
                    key: format!("symbol:scip-rust pkg src/m{bucket}/f{i}.rs `caller{i}`()."),
                    depth: 1,
                    file_path: Some(format!("src/m{bucket}/f{i}.rs")),
                    confidence_tier: None,
                    exclusion_reason: None,
                }
            })
            .collect();
        let metrics = metrics_from_detailed(&entries);
        let risk = ImpactRisk::classify(metrics.direct, metrics.total, metrics.modules);
        let summary = impact_summary(metrics);
        CodeGraphResponse::Impact(ImpactResponse {
            key: "Foo".to_string(),
            impact: Some(entries),
            file_groups: None,
            risk: Some(risk),
            summary: Some(summary),
            workspace_hint: None,
            // df6s: the new pagination fields default to `None` for
            // non-paginated responses, matching the pre-pagination
            // wire shape.
            total: None,
            offset: None,
            limit: None,
            has_more: None,
            summary_only: None,
            by_depth_counts: None,
            next_step: None,
        })
    }

    fn empty_describe() -> CodeGraphResponse {
        CodeGraphResponse::Describe(DescribeResponse {
            description: None,
            next_step: None,
        })
    }

    /// PR C1: synthetic empty `Context` response used by hint tests and
    /// the snapshot serialization tests.
    fn empty_context() -> CodeGraphResponse {
        use crate::bridge::{SymbolContext, SymbolNode};
        CodeGraphResponse::Context(ContextResponse {
            symbol_context: SymbolContext {
                symbol: SymbolNode {
                    uid: "symbol:foo".to_string(),
                    name: "foo".to_string(),
                    kind: "function".to_string(),
                    file_path: "src/lib.rs".to_string(),
                    start_line: 0,
                    end_line: 0,
                    content: None,
                    method_metadata: None,
                    complexity: None,
                },
                incoming: std::collections::BTreeMap::new(),
                outgoing: std::collections::BTreeMap::new(),
                processes: vec![],
            },
            next_step: None,
        })
    }

    fn empty_orphans() -> CodeGraphResponse {
        CodeGraphResponse::Orphans(OrphansResponse {
            orphans: vec![],
            workspace_hint: None,
            next_step: None,
        })
    }

    /// Reads the `next_step` slot regardless of variant.
    fn read_next_step(response: &CodeGraphResponse) -> Option<&str> {
        match response {
            CodeGraphResponse::Neighbors(r) => r.next_step.as_deref(),
            CodeGraphResponse::Ranked(r) => r.next_step.as_deref(),
            CodeGraphResponse::Implementations(r) => r.next_step.as_deref(),
            CodeGraphResponse::Impact(r) => r.next_step.as_deref(),
            CodeGraphResponse::Search(r) => r.next_step.as_deref(),
            CodeGraphResponse::Cycles(r) => r.next_step.as_deref(),
            CodeGraphResponse::Orphans(r) => r.next_step.as_deref(),
            CodeGraphResponse::Path(r) => r.next_step.as_deref(),
            CodeGraphResponse::Edges(r) => r.next_step.as_deref(),
            CodeGraphResponse::Describe(r) => r.next_step.as_deref(),
            CodeGraphResponse::Context(r) => r.next_step.as_deref(),
            CodeGraphResponse::Status(r) => r.next_step.as_deref(),
            CodeGraphResponse::Workspaces(r) => r.next_step.as_deref(),
            CodeGraphResponse::SymbolsAt(r) => r.next_step.as_deref(),
            CodeGraphResponse::DiffTouches(r) => r.next_step.as_deref(),
            CodeGraphResponse::ApiSurface(r) => r.next_step.as_deref(),
            CodeGraphResponse::BoundaryCheck(r) => r.next_step.as_deref(),
            CodeGraphResponse::Hotspots(r) => r.next_step.as_deref(),
            CodeGraphResponse::Complexity(r) => r.next_step.as_deref(),
            CodeGraphResponse::RefactorCandidates(r) => r.next_step.as_deref(),
            CodeGraphResponse::MetricsAt(r) => r.next_step.as_deref(),
            CodeGraphResponse::DeadSymbols(r) => r.next_step.as_deref(),
            CodeGraphResponse::DeprecatedCallers(r) => r.next_step.as_deref(),
            CodeGraphResponse::TouchesHotPath(r) => r.next_step.as_deref(),
            CodeGraphResponse::Coupling(r) => r.next_step.as_deref(),
            CodeGraphResponse::Churn(r) => r.next_step.as_deref(),
            CodeGraphResponse::CouplingHotspots(r) => r.next_step.as_deref(),
            CodeGraphResponse::CouplingHubs(r) => r.next_step.as_deref(),
            CodeGraphResponse::Ambiguous(r) => r.next_step.as_deref(),
            CodeGraphResponse::NotFound(r) => r.next_step.as_deref(),
            CodeGraphResponse::DetectedChanges(r) => r.next_step.as_deref(),
            CodeGraphResponse::Snapshot(r) => r.next_step.as_deref(),
            CodeGraphResponse::QuerySubgraph(r) => r.next_step.as_deref(),
            CodeGraphResponse::RouteMap(r) => r.next_step.as_deref(),
            CodeGraphResponse::ShapeCheck(r) => r.next_step.as_deref(),
            CodeGraphResponse::ApiImpact(r) => r.next_step.as_deref(),
            CodeGraphResponse::Flow(r) => r.next_step.as_deref(),
        }
    }

    #[test]
    fn search_hint_uses_top_hit_display_name() {
        let mut response = search_with_top_hit("authenticate_user");
        attach_next_step_hint("search", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(
            hint.contains("authenticate_user"),
            "hint should reference top hit name: {hint}"
        );
        assert!(hint.contains("code_graph context"), "hint: {hint}");
    }

    #[test]
    fn search_hint_falls_back_when_no_hits() {
        let mut response = empty_search();
        attach_next_step_hint("search", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert_eq!(hint, FALLBACK_NEXT_STEP);
    }

    #[test]
    fn ranked_hint_mentions_pagerank() {
        let mut response = empty_ranked();
        attach_next_step_hint("ranked", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(hint.contains("PageRank"), "hint: {hint}");
        assert!(hint.contains("context"), "hint: {hint}");
    }

    #[test]
    fn cycles_hint_mentions_path() {
        let mut response = empty_cycles();
        attach_next_step_hint("cycles", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(hint.contains("path"), "hint: {hint}");
    }

    #[test]
    fn impact_hint_low_risk_uses_fallback() {
        // PR C3: LOW-risk impact stays on the generic fallback hint —
        // no need to nudge the agent toward `dead_symbols` for a
        // 0-caller change.
        let mut response = empty_impact();
        attach_next_step_hint("impact", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert_eq!(hint, FALLBACK_NEXT_STEP);
    }

    #[test]
    fn impact_hint_high_risk_recommends_cleanup_ops() {
        // PR C3: HIGH (>=10 direct callers) flips the hint to the
        // dead_symbols / deprecated_callers cleanup nudge.
        let mut response = impact_with_callers(12, 3);
        // Sanity-check the synthetic fixture lands on HIGH.
        match &response {
            CodeGraphResponse::Impact(r) => {
                assert_eq!(r.risk, Some(ImpactRisk::High), "fixture risk: {:?}", r.risk);
            }
            other => panic!("expected Impact, got {other:?}"),
        }
        attach_next_step_hint("impact", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(
            hint.contains("dead_symbols"),
            "high-risk hint should mention dead_symbols: {hint}"
        );
        assert!(
            hint.contains("deprecated_callers"),
            "high-risk hint should mention deprecated_callers: {hint}"
        );
    }

    #[test]
    fn impact_hint_critical_risk_recommends_cleanup_ops() {
        // PR C3: CRITICAL (>=20 direct callers) also emits the
        // cleanup hint.
        let mut response = impact_with_callers(25, 6);
        match &response {
            CodeGraphResponse::Impact(r) => {
                assert_eq!(r.risk, Some(ImpactRisk::Critical));
            }
            other => panic!("expected Impact, got {other:?}"),
        }
        attach_next_step_hint("impact", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert_eq!(hint, HIGH_IMPACT_NEXT_STEP);
    }

    #[test]
    fn context_hint_points_to_impact() {
        // PR C1: the dedicated Context variant routes through the
        // `("context" | "describe", _)` arm and emits the
        // blast-radius nudge.
        let mut response = empty_context();
        attach_next_step_hint("context", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(hint.contains("impact"), "hint: {hint}");
        // Sanity-check the discriminator field stays intact post-hint.
        let json = serde_json::to_value(&response).expect("serialize");
        assert!(
            json.get("symbol_context").is_some(),
            "Context discriminator dropped: {json}"
        );
    }

    #[test]
    fn context_response_serializes_symbol_context_discriminator_pr_c1() {
        // The untagged-enum contract pins the discriminator field to
        // `symbol_context`. UI parsers in `pulseTypes.ts` hang off
        // exactly that name; renaming would silently break them.
        let response = empty_context();
        let json = serde_json::to_value(&response).expect("serialize");
        let inner = json
            .get("symbol_context")
            .expect("symbol_context discriminator missing");
        assert!(inner.get("symbol").is_some(), "no nested symbol: {json}");
        assert!(inner.get("incoming").is_some(), "no incoming map: {json}");
        assert!(inner.get("outgoing").is_some(), "no outgoing map: {json}");
        assert!(
            inner.get("processes").is_some(),
            "no processes list: {json}"
        );
    }

    #[test]
    fn describe_hint_points_to_impact_until_c1() {
        let mut response = empty_describe();
        attach_next_step_hint("describe", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert!(hint.contains("impact"), "hint: {hint}");
    }

    #[test]
    fn unknown_op_gets_fallback_hint() {
        let mut response = empty_orphans();
        attach_next_step_hint("orphans", &mut response);
        let hint = read_next_step(&response).expect("hint set");
        assert_eq!(hint, FALLBACK_NEXT_STEP);
    }

    #[test]
    fn next_step_hint_always_non_empty() {
        // Plan acceptance criterion: every code_graph.* op response
        // ends with a non-empty `next_step` field.
        let cases: Vec<(&str, CodeGraphResponse)> = vec![
            ("search", empty_search()),
            ("ranked", empty_ranked()),
            ("cycles", empty_cycles()),
            ("impact", empty_impact()),
            ("orphans", empty_orphans()),
            ("describe", empty_describe()),
            ("context", empty_context()),
            (
                "metrics_at",
                CodeGraphResponse::MetricsAt(MetricsAtResponse {
                    metrics: MetricsAtResult {
                        commit: "abc123".to_string(),
                        node_count: 0,
                        edge_count: 0,
                        cycle_count: 0,
                        cycle_count_symbol_only: 0,
                        cycle_count_file_only: 0,
                        cycles_by_size_histogram: Default::default(),
                        god_object_count: 0,
                        orphan_count: 0,
                        public_api_count: 0,
                        doc_coverage_pct: 0.0,
                    },
                    next_step: None,
                }),
            ),
        ];
        for (op, mut response) in cases {
            attach_next_step_hint(op, &mut response);
            let hint = read_next_step(&response).unwrap_or("");
            assert!(!hint.is_empty(), "op={op} produced empty hint");
        }
    }

    #[test]
    fn search_response_serializes_next_step() {
        let mut response = search_with_top_hit("login");
        attach_next_step_hint("search", &mut response);
        let json = serde_json::to_value(&response).expect("serialize");
        let next_step = json.get("next_step").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            next_step.contains("login"),
            "serialized next_step missing top hit: {json}"
        );
        // Untagged-enum discriminator field stays intact.
        assert!(json.get("hits").is_some(), "discriminator dropped: {json}");
    }

    #[test]
    fn next_step_omitted_when_none_via_skip_serializing() {
        // When the hint is not attached the field is `None` and the
        // serde `skip_serializing_if` rule keeps it out of the wire
        // payload entirely — important so the additive change
        // doesn't pollute every existing snapshot.
        let response = empty_ranked();
        let json = serde_json::to_value(&response).expect("serialize");
        assert!(
            json.get("next_step").is_none(),
            "next_step should be omitted when None: {json}"
        );
    }

    #[test]
    fn flag_disabled_suppresses_emission_path() {
        // Direct env-var test: setting the flag to "0" must short-circuit
        // the dispatcher's `next_step_hints_enabled()` gate. Use a unique
        // value-restore to keep this hermetic across other tests in the
        // module that read the same env var.
        let prev = std::env::var("DJINN_CODE_GRAPH_NEXT_STEP_HINTS").ok();
        // SAFETY: the test binary is single-threaded for env mutations
        // here; cargo runs each `#[test]` in its own thread but env is
        // process-global. The matched `prev` restore at the end keeps
        // the test idempotent for the assertion we care about: when
        // *we* set "0", the helper returns false.
        unsafe {
            std::env::set_var("DJINN_CODE_GRAPH_NEXT_STEP_HINTS", "0");
        }
        assert!(!next_step_hints_enabled());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DJINN_CODE_GRAPH_NEXT_STEP_HINTS", v),
                None => std::env::remove_var("DJINN_CODE_GRAPH_NEXT_STEP_HINTS"),
            }
        }
    }

    // ── PR C3 risk classification ──────────────────────────────────────────

    #[test]
    fn module_bucket_takes_first_two_path_segments() {
        // Per PR C3 plan: bucket on the first two segments after the
        // repo root.
        assert_eq!(module_bucket("src/auth/User.rs"), "src/auth");
        assert_eq!(
            module_bucket("crates/djinn-control-plane/src/lib.rs"),
            "crates/djinn-control-plane"
        );
        // Single-segment path stays as-is so the count remains 1.
        assert_eq!(module_bucket("Cargo.toml"), "Cargo.toml");
        // Backslashes get normalized so Windows-formatted paths still
        // bucket the same way.
        assert_eq!(module_bucket("src\\auth\\User.rs"), "src/auth");
    }

    #[test]
    fn module_bucket_collapses_two_paths_in_same_dir() {
        assert_eq!(
            module_bucket("src/auth/User.rs"),
            module_bucket("src/auth/Session.rs")
        );
    }

    #[test]
    fn risk_thresholds_critical_at_20_direct() {
        // Boundary: direct == 20 lands in CRITICAL.
        assert_eq!(ImpactRisk::classify(20, 0, 0), ImpactRisk::Critical);
        assert_eq!(ImpactRisk::classify(19, 0, 0), ImpactRisk::High);
        // Boundary: total == 200, modules == 10 also push to CRITICAL.
        assert_eq!(ImpactRisk::classify(0, 200, 0), ImpactRisk::Critical);
        assert_eq!(ImpactRisk::classify(0, 0, 10), ImpactRisk::Critical);
    }

    #[test]
    fn risk_thresholds_high_at_10_direct() {
        // Boundary: direct == 10 lands in HIGH.
        assert_eq!(ImpactRisk::classify(10, 0, 0), ImpactRisk::High);
        assert_eq!(ImpactRisk::classify(9, 0, 0), ImpactRisk::Medium);
        // Other axes too.
        assert_eq!(ImpactRisk::classify(0, 80, 0), ImpactRisk::High);
        assert_eq!(ImpactRisk::classify(0, 0, 5), ImpactRisk::High);
    }

    #[test]
    fn risk_thresholds_medium_at_3_direct() {
        // Boundary: direct == 3 lands in MEDIUM.
        assert_eq!(ImpactRisk::classify(3, 0, 0), ImpactRisk::Medium);
        assert_eq!(ImpactRisk::classify(2, 0, 0), ImpactRisk::Low);
        // Other axes.
        assert_eq!(ImpactRisk::classify(0, 20, 0), ImpactRisk::Medium);
        assert_eq!(ImpactRisk::classify(0, 0, 2), ImpactRisk::Medium);
    }

    #[test]
    fn risk_thresholds_low_for_zero() {
        assert_eq!(ImpactRisk::classify(0, 0, 0), ImpactRisk::Low);
        assert_eq!(ImpactRisk::classify(1, 1, 1), ImpactRisk::Low);
    }

    #[test]
    fn metrics_from_detailed_counts_direct_only_at_depth_one() {
        // depth-1 entries are "direct callers"; deeper entries roll up
        // into `total` only. Modules dedupe by two-segment bucket.
        let entries = vec![
            ImpactEntry {
                // uid: test-only synthetic symbol key mirrors key.
                uid: "symbol:a".into(),
                key: "symbol:a".into(),
                depth: 1,
                file_path: Some("src/auth/A.rs".into()),
                confidence_tier: None,
                exclusion_reason: None,
            },
            ImpactEntry {
                // uid: test-only synthetic symbol key mirrors key.
                uid: "symbol:b".into(),
                key: "symbol:b".into(),
                depth: 1,
                file_path: Some("src/auth/B.rs".into()),
                confidence_tier: None,
                exclusion_reason: None,
            },
            ImpactEntry {
                // uid: test-only synthetic symbol key mirrors key.
                uid: "symbol:c".into(),
                key: "symbol:c".into(),
                depth: 2,
                file_path: Some("src/billing/C.rs".into()),
                confidence_tier: None,
                exclusion_reason: None,
            },
        ];
        let m = metrics_from_detailed(&entries);
        assert_eq!(m.direct, 2);
        assert_eq!(m.total, 3);
        assert_eq!(m.modules, 2, "src/auth + src/billing => 2 buckets");
    }

    #[test]
    fn metrics_skip_entries_without_file_path() {
        // External symbols with no file_path don't contribute to the
        // module count but still hit `direct`/`total`.
        let entries = vec![
            ImpactEntry {
                // uid: test-only synthetic symbol key mirrors key.
                uid: "symbol:a".into(),
                key: "symbol:a".into(),
                depth: 1,
                file_path: Some("src/auth/A.rs".into()),
                confidence_tier: None,
                exclusion_reason: None,
            },
            ImpactEntry {
                // uid: test-only synthetic symbol key mirrors key.
                uid: "symbol:ext".into(),
                key: "symbol:ext".into(),
                depth: 1,
                file_path: None,
                confidence_tier: None,
                exclusion_reason: None,
            },
        ];
        let m = metrics_from_detailed(&entries);
        assert_eq!(m.direct, 2);
        assert_eq!(m.total, 2);
        assert_eq!(m.modules, 1);
    }

    #[test]
    fn impact_summary_uses_plan_phrasing() {
        let m = ImpactMetrics {
            direct: 12,
            total: 30,
            modules: 3,
        };
        assert_eq!(impact_summary(m), "12 direct caller(s) across 3 module(s)");
    }

    #[test]
    fn impact_summary_zero_callers_uses_snapshot_phrasing() {
        let m = ImpactMetrics {
            direct: 0,
            total: 0,
            modules: 0,
        };
        assert_eq!(
            impact_summary(m),
            "no direct callers in current graph snapshot"
        );
    }

    #[test]
    fn impact_response_serializes_risk_screaming_snake() {
        // Plan acceptance: `risk: "HIGH"` on the wire — confirm
        // SCREAMING_SNAKE_CASE serialization survives the untagged
        // CodeGraphResponse envelope.
        let response = impact_with_callers(12, 3);
        let json = serde_json::to_value(&response).expect("serialize");
        assert_eq!(
            json.get("risk").and_then(|v| v.as_str()),
            Some("HIGH"),
            "risk should serialize as 'HIGH': {json}"
        );
        let summary = json
            .get("summary")
            .and_then(|v| v.as_str())
            .expect("summary present");
        assert!(
            summary.contains("12 direct caller"),
            "summary phrasing: {summary}"
        );
    }

    #[test]
    fn impact_acceptance_twelve_direct_callers_three_modules() {
        // The plan's literal acceptance test: a 12-direct-caller change
        // returns risk == HIGH and a summary like the example string.
        let response = impact_with_callers(12, 3);
        match response {
            CodeGraphResponse::Impact(r) => {
                assert_eq!(r.risk, Some(ImpactRisk::High));
                let summary = r.summary.expect("summary set");
                assert!(
                    summary.starts_with("12 direct caller"),
                    "summary: {summary}"
                );
                assert!(
                    summary.contains("3 module"),
                    "summary should report 3 modules: {summary}"
                );
            }
            other => panic!("expected Impact, got {other:?}"),
        }
    }

    #[test]
    fn parses_detect_changes_params_with_sha_range() {
        let json = serde_json::json!({
            "operation": "detect_changes",
            "project": "owner/repo",
            "from_sha": "abc123",
            "to_sha": "HEAD",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "detect_changes");
        assert_eq!(params.from_sha.as_deref(), Some("abc123"));
        assert_eq!(params.to_sha.as_deref(), Some("HEAD"));
        assert!(params.changed_files.is_none());
    }

    #[test]
    fn parses_detect_changes_params_with_changed_files() {
        let json = serde_json::json!({
            "operation": "detect_changes",
            "project": "owner/repo",
            "changed_files": ["src/a.rs", "src/b.rs"],
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        let files = params.changed_files.as_ref().expect("changed_files set");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], "src/a.rs");
        assert_eq!(files[1], "src/b.rs");
    }

    #[test]
    fn pick_next_step_target_prefers_high_tier() {
        use crate::bridge::{ChangeKind, DetectedTouchedSymbol, PagerankTier};
        let symbols = vec![
            DetectedTouchedSymbol {
                uid: "sym:low".to_string(),
                name: "z_low".to_string(),
                kind: "function".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 1,
                pagerank_tier: PagerankTier::Low,
                change_kind: ChangeKind::Modified,
            },
            DetectedTouchedSymbol {
                uid: "sym:high".to_string(),
                name: "a_high".to_string(),
                kind: "function".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 10,
                end_line: 20,
                pagerank_tier: PagerankTier::High,
                change_kind: ChangeKind::Modified,
            },
            DetectedTouchedSymbol {
                uid: "sym:medium".to_string(),
                name: "m_medium".to_string(),
                kind: "function".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 5,
                end_line: 5,
                pagerank_tier: PagerankTier::Medium,
                change_kind: ChangeKind::Modified,
            },
        ];
        let target = super::pick_next_step_target(&symbols);
        assert_eq!(target.as_deref(), Some("sym:high"));
    }

    #[test]
    fn pick_next_step_target_returns_none_for_empty() {
        assert!(super::pick_next_step_target(&[]).is_none());
    }

    // ── PR D2: snapshot wire-shape tests ───────────────────────────────────

    fn empty_snapshot_response() -> SnapshotResponse {
        SnapshotResponse {
            snapshot: SnapshotPayload {
                project_id: "proj-test".to_string(),
                git_head: "deadbeef".to_string(),
                generated_at: "2026-04-28T00:00:00Z".to_string(),
                truncated: false,
                total_nodes: 0,
                total_edges: 0,
                node_cap: 2_000,
                nodes: vec![],
                edges: vec![],
            },
            workspace_hint: None,
            next_step: None,
        }
    }

    #[test]
    fn snapshot_variant_uses_unique_discriminator_pr_d2() {
        // The `CodeGraphResponse` enum is `#[serde(untagged)]`; UI
        // parsers disambiguate on a top-level field name. The plan
        // pins `snapshot` as the PR D2 discriminator — assert it
        // doesn't collide with any other variant's discriminator.
        let response = CodeGraphResponse::Snapshot(empty_snapshot_response());
        let json = serde_json::to_value(&response).expect("serialize");
        let obj = json
            .as_object()
            .expect("snapshot variant should be an object");
        assert!(
            obj.contains_key("snapshot"),
            "snapshot variant must surface the `snapshot` field: {json}"
        );
        // Existing taken field names per the inter-PR contract — none
        // may appear at the top level of the snapshot variant.
        for forbidden in [
            "nodes",
            "orphans",
            "cycles",
            "hits",
            "neighbors",
            "file_groups",
            "hotspots",
            "pairs",
            "hubs",
            "coupled",
            "edges",
            "symbols",
            "violations",
            "description",
            "path",
            "status",
            "warmed",
            "deprecated_symbol",
            "witness_path",
            "members",
            "symbol_context",
            "candidates",
            "not_found",
            "detected_changes",
        ] {
            assert!(
                !obj.contains_key(forbidden),
                "snapshot variant must not surface the `{forbidden}` field at top level: {json}"
            );
        }
    }

    #[test]
    fn snapshot_response_serializes_full_contract_pr_d2() {
        // Pin the wire shape spec'd in the inter-PR contract: the
        // payload sits under `snapshot`, with all required fields
        // present and the right types.
        let mut response = empty_snapshot_response();
        response.snapshot.total_nodes = 3;
        response.snapshot.total_edges = 5;
        response.snapshot.truncated = true;
        response.snapshot.nodes.push(crate::bridge::SnapshotNode {
            id: "symbol:scip-rust . . . main()".to_string(),
            uid: "symbol:scip-rust . . . main()".to_string(),
            kind: "symbol".to_string(),
            label: "main".to_string(),
            workspace: Some("server".to_string()),
            workspace_kind: None,
            member_count: None,
            internal_edge_count: None,
            symbol_kind: Some("function".to_string()),
            file_path: Some("src/main.rs".to_string()),
            pagerank: 0.42,
            community_id: None,
            cognitive: None,
            is_test: false,
            x: 0.0,
            y: 0.0,
        });
        response.snapshot.edges.push(crate::bridge::SnapshotEdge {
            from: "file:src/main.rs".to_string(),
            to: "symbol:scip-rust . . . main()".to_string(),
            kind: "ContainsDefinition".to_string(),
            confidence: 0.95,
            reason: None,
        });
        let json = serde_json::to_value(CodeGraphResponse::Snapshot(response)).expect("serialize");
        let snapshot = json
            .get("snapshot")
            .and_then(|s| s.as_object())
            .expect("snapshot object present");
        for required in [
            "project_id",
            "git_head",
            "generated_at",
            "truncated",
            "total_nodes",
            "total_edges",
            "node_cap",
            "nodes",
            "edges",
        ] {
            assert!(
                snapshot.contains_key(required),
                "snapshot payload missing required field `{required}`: {json}"
            );
        }
        assert_eq!(
            snapshot.get("truncated").and_then(|v| v.as_bool()),
            Some(true),
            "truncated round-trips as bool: {json}"
        );

        // Nodes / edges shape spot-check.
        let node = snapshot
            .get("nodes")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_object())
            .expect("first node object");
        for required in ["id", "kind", "label", "workspace", "pagerank", "x", "y"] {
            assert!(
                node.contains_key(required),
                "node missing `{required}`: {node:?}"
            );
        }
        // 7e6o: coordinates serialize as explicit numeric fields.
        assert_eq!(
            node.get("x").and_then(|v| v.as_f64()),
            Some(0.0),
            "x should serialize as a number: {node:?}"
        );
        assert_eq!(
            node.get("y").and_then(|v| v.as_f64()),
            Some(0.0),
            "y should serialize as a number: {node:?}"
        );
        assert_eq!(
            node.get("workspace").and_then(|v| v.as_str()),
            Some("server"),
            "node workspace slug should serialize from SnapshotNode.workspace: {node:?}"
        );
        let edge = snapshot
            .get("edges")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_object())
            .expect("first edge object");
        for required in ["from", "to", "kind", "confidence"] {
            assert!(
                edge.contains_key(required),
                "edge missing `{required}`: {edge:?}"
            );
        }
    }

    #[test]
    fn parses_snapshot_params_from_json_pr_d2() {
        let json = serde_json::json!({
            "operation": "snapshot",
            "project": "owner/repo",
            "limit": 1500,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "snapshot");
        assert_eq!(params.limit, Some(1500));
    }

    #[test]
    fn parses_workspaces_params_without_key_or_query() {
        let json = serde_json::json!({
            "operation": "workspaces",
            "project": "owner/repo",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "workspaces");
        assert!(params.key.is_none());
        assert!(params.query.is_none());
    }

    #[test]
    fn snapshot_node_omits_absent_workspace() {
        let node = crate::bridge::SnapshotNode {
            id: "file:src/lib.rs".to_string(),
            uid: "file:src/lib.rs".to_string(),
            kind: "file".to_string(),
            label: "src/lib.rs".to_string(),
            workspace: None,
            workspace_kind: None,
            member_count: None,
            internal_edge_count: None,
            symbol_kind: None,
            file_path: Some("src/lib.rs".to_string()),
            pagerank: 0.1,
            community_id: None,
            cognitive: None,
            is_test: false,
            x: 0.0,
            y: 0.0,
        };
        let json = serde_json::to_value(node).expect("serialize node");
        assert!(
            json.as_object()
                .expect("node object")
                .get("workspace")
                .is_none(),
            "workspace should be omitted when absent: {json}"
        );
    }
