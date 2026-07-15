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
            graph_staleness: None,
        })
    }

    fn empty_search() -> CodeGraphResponse {
        CodeGraphResponse::Search(SearchResponse {
            query: "auth".to_string(),
            hits: vec![],
            workspace_hint: None,
            next_step: None,
            graph_staleness: None,
        })
    }

    fn empty_ranked() -> CodeGraphResponse {
        CodeGraphResponse::Ranked(RankedResponse {
            nodes: vec![],
            workspace_hint: None,
            next_step: None,
            graph_staleness: None,
        })
    }

    fn empty_cycles() -> CodeGraphResponse {
        CodeGraphResponse::Cycles(CyclesResponse {
            cycles: vec![],
            workspace_hint: None,
            next_step: None,
            graph_staleness: None,
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
            graph_staleness: None,
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
            graph_staleness: None,
        })
    }

    fn empty_describe() -> CodeGraphResponse {
        CodeGraphResponse::Describe(DescribeResponse {
            description: None,
            next_step: None,
            graph_staleness: None,
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
            graph_staleness: None,
        })
    }

    fn empty_orphans() -> CodeGraphResponse {
        CodeGraphResponse::Orphans(OrphansResponse {
            orphans: vec![],
            workspace_hint: None,
            next_step: None,
            graph_staleness: None,
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
            CodeGraphResponse::CrateGraph(r) => r.next_step.as_deref(),
            CodeGraphResponse::ImpactCheck(r) => r.next_step.as_deref(),
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
                    graph_staleness: None,
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
            graph_staleness: None,
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
            gx: None,
            gy: None,
            gz: None,
            degree: None,
            keywords: Vec::new(),
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
            gx: None,
            gy: None,
            gz: None,
            degree: None,
            keywords: Vec::new(),
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

    // ── jc47: per-query staleness metadata tests ─────────────────────────────

    /// Extract `graph_staleness` from any `CodeGraphResponse` variant.
    fn graph_staleness_of(response: &CodeGraphResponse) -> Option<&GraphStaleness> {
        graph_staleness_slot_const(response).as_ref()
    }

    // Const-access mirror of `graph_staleness_slot` for read-only tests.
    fn graph_staleness_slot_const(response: &CodeGraphResponse) -> &Option<GraphStaleness> {
        match response {
            CodeGraphResponse::Neighbors(r) => &r.graph_staleness,
            CodeGraphResponse::Ranked(r) => &r.graph_staleness,
            CodeGraphResponse::Implementations(r) => &r.graph_staleness,
            CodeGraphResponse::Impact(r) => &r.graph_staleness,
            CodeGraphResponse::Search(r) => &r.graph_staleness,
            CodeGraphResponse::Cycles(r) => &r.graph_staleness,
            CodeGraphResponse::Orphans(r) => &r.graph_staleness,
            CodeGraphResponse::Path(r) => &r.graph_staleness,
            CodeGraphResponse::Edges(r) => &r.graph_staleness,
            CodeGraphResponse::Describe(r) => &r.graph_staleness,
            CodeGraphResponse::Context(r) => &r.graph_staleness,
            CodeGraphResponse::Status(r) => &r.graph_staleness,
            CodeGraphResponse::Workspaces(r) => &r.graph_staleness,
            CodeGraphResponse::SymbolsAt(r) => &r.graph_staleness,
            CodeGraphResponse::DiffTouches(r) => &r.graph_staleness,
            CodeGraphResponse::ApiSurface(r) => &r.graph_staleness,
            CodeGraphResponse::BoundaryCheck(r) => &r.graph_staleness,
            CodeGraphResponse::Hotspots(r) => &r.graph_staleness,
            CodeGraphResponse::Complexity(r) => &r.graph_staleness,
            CodeGraphResponse::RefactorCandidates(r) => &r.graph_staleness,
            CodeGraphResponse::MetricsAt(r) => &r.graph_staleness,
            CodeGraphResponse::DeadSymbols(r) => &r.graph_staleness,
            CodeGraphResponse::DeprecatedCallers(r) => &r.graph_staleness,
            CodeGraphResponse::TouchesHotPath(r) => &r.graph_staleness,
            CodeGraphResponse::Coupling(r) => &r.graph_staleness,
            CodeGraphResponse::Churn(r) => &r.graph_staleness,
            CodeGraphResponse::CouplingHotspots(r) => &r.graph_staleness,
            CodeGraphResponse::CouplingHubs(r) => &r.graph_staleness,
            CodeGraphResponse::Ambiguous(r) => &r.graph_staleness,
            CodeGraphResponse::NotFound(r) => &r.graph_staleness,
            CodeGraphResponse::DetectedChanges(r) => &r.graph_staleness,
            CodeGraphResponse::Snapshot(r) => &r.graph_staleness,
            CodeGraphResponse::QuerySubgraph(r) => &r.graph_staleness,
            CodeGraphResponse::RouteMap(r) => &r.graph_staleness,
            CodeGraphResponse::ShapeCheck(r) => &r.graph_staleness,
            CodeGraphResponse::ApiImpact(r) => &r.graph_staleness,
            CodeGraphResponse::Flow(r) => &r.graph_staleness,
            CodeGraphResponse::CrateGraph(r) => &r.graph_staleness,
            CodeGraphResponse::ImpactCheck(r) => &r.graph_staleness,
        }
    }

    /// AC#1+AC#3: when no caller commit is supplied, the response has
    /// **no** `graph_staleness` field — existing clients see an
    /// unchanged shape.
    #[tokio::test]
    async fn code_graph_staleness_absent_when_no_caller_commit() {
        let ops = WorkspaceFixtureOps::new(None).with_pinned_commit("abc123");
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(None);
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        // current_head is None by default via test_params

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("ranked dispatch succeeds");

        assert!(
            graph_staleness_of(&response).is_none(),
            "graph_staleness must be absent when no caller commit is supplied"
        );
    }

    /// AC#2: matching caller commit → `is_stale=false`, cached_commit
    /// echoes the graph cache's pinned commit.
    #[tokio::test]
    async fn code_graph_staleness_false_for_matching_commit() {
        let ops = WorkspaceFixtureOps::new(None).with_pinned_commit("abc123");
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(None);
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.current_head = Some("abc123".to_string());

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("ranked dispatch succeeds");

        let staleness = graph_staleness_of(&response)
            .expect("graph_staleness must be present when caller supplies a commit");
        assert!(
            !staleness.is_stale,
            "is_stale must be false when commits match"
        );
        assert_eq!(
            staleness.cached_commit.as_deref(),
            Some("abc123"),
            "cached_commit must echo the graph cache's pinned commit"
        );
        assert_eq!(
            staleness.caller_commit, "abc123",
            "caller_commit must echo the trimmed caller value"
        );
    }

    /// AC#2: differing caller commit → `is_stale=true`.
    #[tokio::test]
    async fn code_graph_staleness_true_for_differing_commit() {
        let ops = WorkspaceFixtureOps::new(None).with_pinned_commit("abc123");
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(None);
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.current_head = Some("def456".to_string());

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("ranked dispatch succeeds");

        let staleness = graph_staleness_of(&response)
            .expect("graph_staleness must be present when caller supplies a commit");
        assert!(
            staleness.is_stale,
            "is_stale must be true when commits differ"
        );
        assert_eq!(
            staleness.cached_commit.as_deref(),
            Some("abc123"),
            "cached_commit must echo the graph cache's pinned commit"
        );
        assert_eq!(
            staleness.caller_commit, "def456",
            "caller_commit must echo the trimmed caller value"
        );
    }

    /// AC#3: when caller supplies a commit but the cache has no pinned
    /// commit (un-warmed graph), `graph_staleness` is still populated
    /// with `is_stale=false` (non-stale-safe) and `cached_commit=None`.
    #[tokio::test]
    async fn code_graph_staleness_non_stale_when_cache_unwarmed() {
        let ops = WorkspaceFixtureOps::new(None); // pinned_commit: None
        let (server, _) = fixture_server(ops);
        let ctx = fixture_ctx(None);
        let mut params = test_params("ranked");
        params.project_id = ctx.id.clone();
        params.project_path = ctx.clone_path.clone();
        params.current_head = Some("abc123".to_string());

        let response = server
            .dispatch_code_graph(&ctx, &mut params)
            .await
            .expect("ranked dispatch succeeds");

        let staleness = graph_staleness_of(&response)
            .expect("graph_staleness must be present when caller supplies a commit");
        assert!(
            !staleness.is_stale,
            "is_stale must be false (non-stale-safe) when cached commit is missing"
        );
        assert!(
            staleness.cached_commit.is_none(),
            "cached_commit must be None when graph cache has no pinned commit"
        );
        assert_eq!(staleness.caller_commit, "abc123");
    }

    // ── kfgh / epic z3en: `check_impact_staleness` (strict primitive) ─────

    /// kfgh: matching caller/cached → not stale.
    #[test]
    fn check_impact_staleness_matching_returns_false() {
        let (is_stale, caller, cached) = check_impact_staleness("abc123", Some("abc123"));
        assert!(!is_stale, "matching commits must not be flagged stale");
        assert_eq!(caller, "abc123");
        assert_eq!(cached.as_deref(), Some("abc123"));
    }

    /// kfgh: differing caller/cached → stale (strict).
    #[test]
    fn check_impact_staleness_differing_returns_true() {
        let (is_stale, caller, cached) = check_impact_staleness("def456", Some("abc123"));
        assert!(is_stale, "differing commits must be flagged stale");
        assert_eq!(caller, "def456");
        assert_eq!(cached.as_deref(), Some("abc123"));
    }

    /// kfgh: missing cached → stale (strict; diverges from
    /// `GraphStaleness::compute`'s lenient `is_stale=false` default).
    #[test]
    fn check_impact_staleness_missing_cached_returns_true() {
        let (is_stale, caller, cached) = check_impact_staleness("abc123", None);
        assert!(
            is_stale,
            "missing cached commit MUST be flagged stale for impact preflight"
        );
        assert_eq!(caller, "abc123");
        assert!(cached.is_none(), "cached_commit must round-trip as None");
    }

    /// kfgh: blank cached (whitespace) → stale.
    #[test]
    fn check_impact_staleness_blank_cached_returns_true() {
        let (is_stale, _, cached) = check_impact_staleness("abc123", Some("   \t\n"));
        assert!(
            is_stale,
            "blank/whitespace cached commit MUST be flagged stale"
        );
        assert!(
            cached.is_none(),
            "blank cached_commit must normalize to None"
        );
    }

    /// kfgh: caller whitespace is trimmed before equality check.
    #[test]
    fn check_impact_staleness_trims_caller_whitespace() {
        let (is_stale, caller, _) = check_impact_staleness("  abc123  ", Some("abc123"));
        assert!(
            !is_stale,
            "caller head whitespace must be trimmed before equality check"
        );
        assert_eq!(
            caller, "abc123",
            "caller_commit echo must be trimmed"
        );
    }

    /// kfgh: `GraphStaleness::compute` must keep its lenient missing-cache
    /// contract (non-stale-safe default) even after delegating to the
    /// shared strict primitive. This pins the asymmetry between the two
    /// flows — `code_graph` ops must not start blocking on un-warmed
    /// graphs just because the impact_check flow wants strict semantics.
    #[test]
    fn graph_staleness_compute_keeps_lenient_missing_default() {
        let s = GraphStaleness::compute("abc123", None);
        assert!(
            !s.is_stale,
            "GraphStaleness must remain lenient when cached_commit is None"
        );
        assert!(s.cached_commit.is_none());
        assert_eq!(s.caller_commit, "abc123");

        let s = GraphStaleness::compute("abc123", Some("   "));
        assert!(
            !s.is_stale,
            "GraphStaleness must remain lenient when cached_commit is blank"
        );

        // Same input through the strict helper should diverge.
        let (strict_is_stale, _, _) = check_impact_staleness("abc123", None);
        assert!(
            strict_is_stale,
            "check_impact_staleness must surface missing as stale"
        );
    }

    // ── kfgh / epic z3en: `check_impact_check_staleness` (bridge flow) ────

    /// kfgh: the helper reads `pinned_commit` via the bridge's `status`
    /// peek and returns the strict staleness snapshot for the caller.
    /// When the bridge reports a matching commit, the snapshot is fresh.
    #[tokio::test]
    async fn check_impact_check_staleness_fresh_when_commits_match() {
        let ops = WorkspaceFixtureOps::new(None).with_pinned_commit("abc123");
        let ctx = fixture_ctx(None);
        let snap = check_impact_check_staleness(&ops, &ctx, "abc123").await;
        assert!(
            !snap.is_stale,
            "matching pinned_commit must not be flagged stale"
        );
        assert_eq!(snap.caller_commit, "abc123");
        assert_eq!(snap.cached_commit.as_deref(), Some("abc123"));
        assert!(
            !snap.is_completely_unanchored(),
            "anchored commits must not be unanchored"
        );
    }

    /// kfgh: when the bridge reports a different commit, the snapshot is
    /// stale — `impact_check` must short-circuit with `needs_spike`.
    #[tokio::test]
    async fn check_impact_check_staleness_stale_when_commits_differ() {
        let ops = WorkspaceFixtureOps::new(None).with_pinned_commit("abc123");
        let ctx = fixture_ctx(None);
        let snap = check_impact_check_staleness(&ops, &ctx, "def456").await;
        assert!(
            snap.is_stale,
            "differing pinned_commit must be flagged stale"
        );
        assert_eq!(snap.caller_commit, "def456");
        assert_eq!(snap.cached_commit.as_deref(), Some("abc123"));
    }

    /// kfgh: an un-warmed graph (no pinned commit) yields a strict-stale
    /// snapshot — `impact_check` MUST spike rather than trust the empty
    /// answer that an unanchored graph would produce.
    #[tokio::test]
    async fn check_impact_check_staleness_stale_when_graph_unwarmed() {
        let ops = WorkspaceFixtureOps::new(None); // pinned_commit: None
        let ctx = fixture_ctx(None);
        let snap = check_impact_check_staleness(&ops, &ctx, "abc123").await;
        assert!(
            snap.is_stale,
            "un-warmed graph MUST be flagged stale (strict semantic)"
        );
        assert!(snap.cached_commit.is_none());
    }

    /// kfgh: both sides missing (no caller commit, no pinned commit) is
    /// `is_completely_unanchored` — the impact_check caller must surface
    /// this distinctly because the user can recover by supplying a HEAD.
    #[tokio::test]
    async fn check_impact_check_staleness_marks_fully_unanchored() {
        let ops = WorkspaceFixtureOps::new(None); // pinned_commit: None
        let ctx = fixture_ctx(None);
        let snap = check_impact_check_staleness(&ops, &ctx, "").await;
        assert!(
            snap.is_stale,
            "no anchor on either side must be flagged stale"
        );
        assert!(
            snap.is_completely_unanchored(),
            "empty caller + empty cache must be flagged fully unanchored"
        );
    }

    /// AC#1: `current_head` accepts aliases `caller_commit` and
    /// `currentHead`.
    #[test]
    fn code_graph_params_current_head_accepts_aliases() {
        let json = serde_json::json!({"operation":"ranked","project":"o/r","caller_commit":"deadbeef"});
        let params: CodeGraphParams = serde_json::from_value(json).expect("deserialize");
        assert_eq!(params.current_head.as_deref(), Some("deadbeef"));

        let json = serde_json::json!({"operation":"ranked","project":"o/r","currentHead":"cafef00d"});
        let params: CodeGraphParams = serde_json::from_value(json).expect("deserialize");
        assert_eq!(params.current_head.as_deref(), Some("cafef00d"));
    }

    /// AC#1: blank `current_head` is normalized to `None` by `normalize()`.
    #[test]
    fn code_graph_params_current_head_blank_normalized_to_none() {
        let mut params = CodeGraphParams {
            operation: "ranked".to_string(),
            project: "o/r".to_string(),
            current_head: Some(String::new()),
            ..test_params("ranked")
        };
        params.normalize();
        assert!(
            params.current_head.is_none(),
            "empty current_head must normalize to None"
        );
    }

    // ── epic uajf / task 1zcr: focused unit tests for the helpers
    //    extracted from `code_graph_impact_check`.  Each helper is
    //    exercised directly with no I/O, so the tests are O(few) and
    //    pin the pre-refactor contract on the error strings, the
    //    recommendation vocabulary, the `<external>` exclusion, and
    //    the file→crate mapping.

    /// AC#2: `validate_impact_check_request` must reject a request
    /// missing `impact_targets` with the pre-refactor error string
    /// verbatim.
    #[tokio::test]
    async fn validate_impact_check_request_rejects_missing_targets() {
        let (server, _ops) = fixture_server(WorkspaceFixtureOps::new(None));
        let params = test_params("impact_check");
        let err = server
            .validate_impact_check_request(&params)
            .expect_err("missing impact_targets must error");
        assert!(
            err.contains("impact_check requires `impact_targets`"),
            "missing-targets error must keep the pre-refactor wording, got {err}"
        );
    }

    /// AC#2: an empty `impact_targets` list must error with the
    /// pre-refactor "at least one entry" string.
    #[tokio::test]
    async fn validate_impact_check_request_rejects_empty_targets() {
        let (server, _ops) = fixture_server(WorkspaceFixtureOps::new(None));
        let mut params = test_params("impact_check");
        params.impact_targets = Some(Vec::new());
        let err = server
            .validate_impact_check_request(&params)
            .expect_err("empty impact_targets must error");
        assert!(
            err.contains("at least one entry"),
            "empty-targets error must keep the pre-refactor wording, got {err}"
        );
    }

    /// AC#2: a valid request returns the targets slice and the
    /// `scope_crates` set.  `scope_crates` is preserved exactly (the
    /// handler relies on set membership for the
    /// `safe_independent_slice` derivation).
    #[tokio::test]
    async fn validate_impact_check_request_passes_through() {
        let (server, _ops) = fixture_server(WorkspaceFixtureOps::new(None));
        let mut params = test_params("impact_check");
        params.impact_targets = Some(vec!["alpha".to_string(), "beta".to_string()]);
        params.scope_crates = Some(vec!["alpha".to_string(), "gamma".to_string()]);
        let (targets, scope) = server
            .validate_impact_check_request(&params)
            .expect("valid request must succeed");
        assert_eq!(targets, &["alpha".to_string(), "beta".to_string()]);
        assert_eq!(scope.len(), 2);
        assert!(scope.contains("alpha"));
        assert!(scope.contains("gamma"));
    }

    /// AC#3: `CrateIndex::from_crate_graph` excludes the synthetic
    /// `<external>` crate from `crate_dirs` (file→crate mapping must
    /// not produce a phantom mapping for a path outside any real
    /// crate) but keeps it in `known_crates` so the dispatch loop
    /// still routes it to the symbol/file path consistently.
    #[test]
    fn crate_index_excludes_external_from_dirs() {
        let resp = CrateGraphResponse {
            crates: vec![
                CrateNodeEntry {
                    name: "alpha".to_string(),
                    manifest_path: "/workspace/server/crates/alpha/Cargo.toml".to_string(),
                    loc: 0,
                    node_count: 0,
                    fan_in: 0.0,
                    fan_out: 0.0,
                    inbound_weight: 0.0,
                    outbound_weight: 0.0,
                },
                CrateNodeEntry {
                    name: "<external>".to_string(),
                    manifest_path: "/dev/null/Cargo.toml".to_string(),
                    loc: 0,
                    node_count: 0,
                    fan_in: 0.0,
                    fan_out: 0.0,
                    inbound_weight: 0.0,
                    outbound_weight: 0.0,
                },
            ],
            edges: Vec::new(),
            message: None,
        };
        let idx = CrateIndex::from_crate_graph(&resp);
        assert!(
            idx.is_known_crate("alpha"),
            "real crate must be in the known set"
        );
        assert!(
            idx.is_known_crate("<external>"),
            "<external> stays in the known set so callers see it explicitly"
        );
        let alpha_hits: Vec<&String> = idx
            .crates_for_file("/workspace/server/crates/alpha/src/lib.rs")
            .collect();
        assert_eq!(alpha_hits, vec![&"alpha".to_string()]);
        let external_hits: Vec<&String> = idx.crates_for_file("/dev/null").collect();
        assert!(
            external_hits.is_empty(),
            "<external> must not contribute a directory mapping"
        );
    }

    /// AC#3: `ImpactAggregator::add_crate_consumers` mirrors the
    /// pre-refactor inlined edge walk — only edges whose source is
    /// not `<external>` contribute.  The synthetic crate is later
    /// dropped in `finalized()` so the inlined-equivalence is exact.
    #[test]
    fn aggregator_add_crate_consumers_skips_external_source() {
        let mut agg = ImpactAggregator::new();
        let edges = vec![
            CrateEdgeEntry {
                source: "beta".to_string(),
                target: "alpha".to_string(),
                weight: 1.0,
                edge_count: 1,
            },
            CrateEdgeEntry {
                source: "<external>".to_string(),
                target: "alpha".to_string(),
                weight: 1.0,
                edge_count: 1,
            },
        ];
        agg.add_crate_consumers(&edges, "alpha");
        // Direct: only beta is added (the `<external>` source is skipped).
        assert_eq!(
            agg.affected_crates.iter().collect::<Vec<_>>(),
            vec![&"beta".to_string()]
        );

        // finalized() must also drop `<external>` if it ever leaks in.
        agg.affected_crates.insert("<external>".to_string());
        let (crates, _, _) = agg.finalized();
        assert!(
            !crates.contains(&"<external>".to_string()),
            "finalized() must drop the synthetic <external> crate"
        );
    }

    /// AC#3: `ImpactAggregator::add_detailed_entries` collects
    /// every symbol key and every file_path, then maps the file
    /// paths back to the owning crate via the `CrateIndex`.  A file
    /// with no `file_path` contributes no crate mapping.
    #[test]
    fn aggregator_add_detailed_entries_maps_files_to_crates() {
        let resp = CrateGraphResponse {
            crates: vec![CrateNodeEntry {
                name: "alpha".to_string(),
                manifest_path: "/workspace/server/crates/alpha/Cargo.toml".to_string(),
                loc: 0,
                node_count: 0,
                fan_in: 0.0,
                fan_out: 0.0,
                inbound_weight: 0.0,
                outbound_weight: 0.0,
            }],
            edges: Vec::new(),
            message: None,
        };
        let idx = CrateIndex::from_crate_graph(&resp);
        let mut agg = ImpactAggregator::new();
        agg.add_detailed_entries(
            &[
                ImpactEntry {
                    key: "symbol:foo".to_string(),
                    uid: String::new(),
                    depth: 1,
                    file_path: Some(
                        "/workspace/server/crates/alpha/src/lib.rs".to_string(),
                    ),
                    confidence_tier: None,
                    exclusion_reason: None,
                },
                ImpactEntry {
                    key: "symbol:bar".to_string(),
                    uid: String::new(),
                    depth: 2,
                    file_path: None,
                    confidence_tier: None,
                    exclusion_reason: None,
                },
            ],
            &idx,
        );
        let (crates, files, symbols) = agg.finalized();
        assert!(symbols.contains(&"symbol:foo".to_string()));
        assert!(symbols.contains(&"symbol:bar".to_string()));
        assert!(files.iter().any(|f| f.ends_with("alpha/src/lib.rs")));
        assert_eq!(
            crates,
            vec!["alpha".to_string()],
            "file_path → crate mapping must surface alpha"
        );
    }

    /// AC#3: `ImpactAggregator::add_grouped_files` collects every
    /// group file path and maps it to the owning crate.  A path
    /// that doesn't fall under any crate contributes no mapping.
    #[test]
    fn aggregator_add_grouped_files_maps_files_to_crates() {
        let resp = CrateGraphResponse {
            crates: vec![CrateNodeEntry {
                name: "alpha".to_string(),
                manifest_path: "/workspace/server/crates/alpha/Cargo.toml".to_string(),
                loc: 0,
                node_count: 0,
                fan_in: 0.0,
                fan_out: 0.0,
                inbound_weight: 0.0,
                outbound_weight: 0.0,
            }],
            edges: Vec::new(),
            message: None,
        };
        let idx = CrateIndex::from_crate_graph(&resp);
        let mut agg = ImpactAggregator::new();
        agg.add_grouped_files(
            &[FileGroupEntry {
                file: "/workspace/server/crates/alpha/src/g.rs".to_string(),
                occurrence_count: 3,
                max_depth: 2,
                sample_keys: vec!["s1".to_string()],
            }],
            &idx,
        );
        let (crates, files, symbols) = agg.finalized();
        assert!(files.iter().any(|f| f.ends_with("alpha/src/g.rs")));
        assert_eq!(crates, vec!["alpha".to_string()]);
        assert!(symbols.is_empty());
    }

    /// AC#2 / AC#3: `derive_safe_slice_and_recommendation` keeps the
    /// pre-refactor exact strings for every branch — `ok_independent`,
    /// `chain_tasks`, `atomic_cutover` — and the `safe_independent_slice`
    /// contract.
    #[test]
    fn derive_safe_slice_and_recommendation_no_consumers_yields_ok_independent() {
        let (safe, rec) =
            derive_safe_slice_and_recommendation(&[], &std::collections::HashSet::new());
        assert!(safe, "no consumers must always be safe");
        assert_eq!(rec, "ok_independent");
    }

    /// AC#3: consumers exist but caller supplied no `scope_crates` —
    /// `safe_independent_slice` is `false` and the recommendation is
    /// `atomic_cutover` (matches the pre-refactor branch ordering).
    #[test]
    fn derive_safe_slice_and_recommendation_consumers_no_scope_yields_atomic_cutover() {
        let affected = vec!["alpha".to_string(), "beta".to_string()];
        let scope = std::collections::HashSet::new();
        let (safe, rec) = derive_safe_slice_and_recommendation(&affected, &scope);
        assert!(!safe, "missing scope must be unsafe when consumers exist");
        assert_eq!(rec, "atomic_cutover");
    }

    /// AC#3: every consumer is inside the supplied scope — the
    /// recommendation is `chain_tasks` (consumers exist but are
    /// within the slice, so explicit ordering suffices).
    #[test]
    fn derive_safe_slice_and_recommendation_consumers_in_scope_yields_chain_tasks() {
        let affected = vec!["alpha".to_string(), "beta".to_string()];
        let mut scope = std::collections::HashSet::new();
        scope.insert("alpha".to_string());
        scope.insert("beta".to_string());
        let (safe, rec) = derive_safe_slice_and_recommendation(&affected, &scope);
        assert!(safe, "all consumers within scope must be safe");
        assert_eq!(rec, "chain_tasks");
    }

    /// AC#3: some consumers are outside the supplied scope — the
    /// recommendation is `atomic_cutover`.
    #[test]
    fn derive_safe_slice_and_recommendation_consumers_outside_scope_yields_atomic_cutover() {
        let affected = vec!["alpha".to_string(), "gamma".to_string()];
        let mut scope = std::collections::HashSet::new();
        scope.insert("alpha".to_string());
        let (safe, rec) = derive_safe_slice_and_recommendation(&affected, &scope);
        assert!(!safe, "any consumer outside scope must be unsafe");
        assert_eq!(rec, "atomic_cutover");
    }

