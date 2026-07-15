// P4 tribunal regression tests — extracted from signoff_tests.rs to
// meet the 1500-line file-size guard.  Behavior and expectations are
// unchanged.

mod p4_tribunal_regression_tests {
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, EffectiveCreatorProvenance, ProjectRepository, ProposalCreateInput,
        ProposalDebateTrailCreateInput, ProposalRepository, TaskRepository, UserRepository,
    };

    /// A well-formed body that passes all deterministic readiness checks.
    fn ready_body() -> &'static str {
        r#"
# Problem
Users cannot do X.

# Scope
In scope: Y. Out of scope: Z.

# Objectives
- Deliver A
- Deliver B

## File map
```file-map
    src/main.rs
    src/lib.rs
```

# Dependencies
Blocked by service C.

# Open Questions
What happens if D fails?
"#
    }

    /// A body that fails DoR checks (missing all sections).
    fn failing_body() -> &'static str {
        "Just some random text without required sections."
    }

    async fn setup_test_server_and_user() -> (DjinnMcpServer, Database, String) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user = UserRepository::new(db.clone())
            .upsert_from_github(999_800, "p4-test-user", None, None)
            .await
            .unwrap();
        UserRepository::new(db.clone())
            .set_role(&user.id, "engineer")
            .await
            .unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db, user.id)
    }

    /// Create a proposal with a target project.
    async fn create_proposal_with_target(
        repo: &ProposalRepository,
        project_repo: &ProjectRepository,
        user_id: &str,
        title: &str,
        body: &str,
        ac: Option<&str>,
    ) -> djinn_core::models::proposal::Proposal {
        let project = project_repo
            .create(
                &format!("svc-p4-{}", uuid::Uuid::now_v7()),
                "test",
                &format!("svc-p4-{}", uuid::Uuid::now_v7()),
            )
            .await
            .unwrap();
        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.to_string()), async {
                repo.create(ProposalCreateInput {
                    title,
                    body,
                    acceptance_criteria: ac,
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();
        repo.add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();
        proposal
    }

    async fn force_approved(db: &Database, proposal_id: &str) {
        ProposalRepository::new(db.clone(), EventBus::noop())
            .set_status(proposal_id, "approved")
            .await
            .unwrap();
    }

    // ── AC1: Composed-gate blocked transition messages ──────────────────────

    /// draft → in_review is blocked when DoR checks fail, with a deterministic
    /// message naming the missing coverage.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn draft_to_in_review_blocked_by_dor_failures() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_target(
            &repo,
            &project_repo,
            &user_id,
            "DOR Block Test",
            failing_body(),
            Some(r#"[{"criterion":"API returns 200","met":false}]"#),
        )
        .await;

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_update",
                        serde_json::json!({
                            "id": proposal.id,
                            "status": "in_review",
                        }),
                    )
                    .await
            })
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(error.is_some(), "expected DoR error, got: {response:?}");
        let error = error.unwrap();
        assert!(
            error.contains("Missing required coverage: problem"),
            "error should name missing problem section: {error}"
        );

        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "draft");
    }

    /// draft → in_review is blocked when a needs-work judge verdict is present.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn draft_to_in_review_blocked_by_needs_work_verdict() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_target(
            &repo,
            &project_repo,
            &user_id,
            "Tribunal Block Test",
            ready_body(),
            Some(r#"[{"criterion":"API returns 200","met":false}]"#),
        )
        .await;

        let verdict = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "verdict",
                body: "needs-work: missing error handling section",
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: Some("test-judge"),
                source_task_id: None,
                against_revision_seq: proposal.latest_revision_seq,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap();

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_update",
                        serde_json::json!({
                            "id": proposal.id,
                            "status": "in_review",
                        }),
                    )
                    .await
            })
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(
            error.is_some(),
            "expected tribunal error, got: {response:?}"
        );
        let error = error.unwrap();
        assert!(
            error.contains("judge returned needs-work"),
            "error should mention needs-work: {error}"
        );
        assert!(
            error.contains(&verdict.id),
            "error should name the verdict id: {error}"
        );
    }

    // ── AC2: Needs-evidence spike parking/resume ────────────────────────────

    /// Spike parking blocks graduation; after clearing the spike, graduation
    /// succeeds. The spike finding is visible in the debate trail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_evidence_spike_parking_resume_and_graduation() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_target(
            &repo,
            &project_repo,
            &user_id,
            "Spike Resume Test",
            ready_body(),
            Some(r#"[{"criterion":"API returns 200","met":false}]"#),
        )
        .await;

        let targets = repo.targets(&proposal.id).await.unwrap();
        let target_project_id = &targets[0].project_id;
        let spike = task_repo
            .create_in_project_with_provenance(
                target_project_id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&user_id),
                    source_task_id: None,
                    proposal_id: Some(&proposal.id),
                },
                "Spike: feasibility of X",
                "Research whether X is feasible",
                "Research whether X is feasible",
                "spike",
                djinn_core::models::task::PRIORITY_CRITICAL,
                "planner",
                Some("open"),
                None,
            )
            .await
            .unwrap();

        repo.set_needs_evidence_spike(&proposal.id, &spike.id, "X is load-bearing")
            .await
            .unwrap();

        // set_needs_evidence_spike parks the proposal in draft.
        // Force to approved AFTER parking so the gate blocks on the spike.
        force_approved(&db, &proposal.id).await;

        // Graduation blocked while spike is open.
        let blocked_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_graduate",
                        serde_json::json!({ "id": proposal.id }),
                    )
                    .await
            })
            .await
            .unwrap();
        assert!(
            blocked_resp.get("error").is_some(),
            "graduation should be blocked while spike is open"
        );
        let err = blocked_resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            err.contains("proposal parked on needs-evidence spike"),
            "error should mention needs-evidence: {err}"
        );

        // Close the spike.
        TaskRepository::new(db.clone(), EventBus::noop())
            .set_status(&spike.id, "done")
            .await
            .unwrap();

        // Write the spike finding as a debate-trail entry.
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &proposal.id,
            kind: "rebuttal",
            body: "Spike finding: X is feasible with approach Y",
            blocking: false,
            agent_role: "advocate",
            author_kind: "agent",
            author_model: Some("test-advocate"),
            source_task_id: Some(&spike.id),
            against_revision_seq: proposal.latest_revision_seq,
            round: 1,
            body_metadata: None,
        })
        .await
        .unwrap();

        // Clear needs-evidence parking.
        repo.clear_needs_evidence_spike(&proposal.id).await.unwrap();

        // Graduation should now succeed.
        let ok_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_graduate",
                        serde_json::json!({ "id": proposal.id }),
                    )
                    .await
            })
            .await
            .unwrap();
        assert!(
            ok_resp.get("error").is_none(),
            "graduation after spike resume should succeed: {:?}",
            ok_resp.get("error")
        );

        // Verify the spike finding is in the debate trail.
        let entries = repo.debate_trail(&proposal.id).await.unwrap();
        let finding = entries
            .iter()
            .find(|e| e.body.contains("X is feasible with approach Y"));
        assert!(
            finding.is_some(),
            "spike finding should be visible in debate trail"
        );
        let finding = finding.unwrap();
        assert_eq!(finding.agent_role, "advocate");
        assert_eq!(finding.source_task_id.as_deref(), Some(spike.id.as_str()));
    }

    // ── AC1 (continued): Valid human override path through graduation ───────

    /// A human verdict override allows graduation past a needs-work judge
    /// verdict: verdict → override → signoff → graduation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graduation_succeeds_with_human_verdict_override() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_target(
            &repo,
            &project_repo,
            &user_id,
            "Override Graduation Test",
            ready_body(),
            Some(r#"[{"criterion":"API returns 200","met":false}]"#),
        )
        .await;

        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &proposal.id,
            kind: "verdict",
            body: "needs-work: scope is too broad",
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: Some("test-judge"),
            source_task_id: None,
            against_revision_seq: proposal.latest_revision_seq,
            round: 1,
            body_metadata: None,
        })
        .await
        .unwrap();

        // Without override, signoff fails.
        let fail_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_signoff",
                        serde_json::json!({ "id": proposal.id, "kind": "technical" }),
                    )
                    .await
            })
            .await
            .unwrap();
        assert!(
            fail_resp.get("error").is_some(),
            "signoff should fail without override"
        );

        // Record a verdict override scoped to the current revision.
        let override_meta = serde_json::json!({
            "override_on_revision_seq": proposal.latest_revision_seq,
            "reason": "PM reviewed scope and approved as-is",
            "override_by_user_id": user_id
        });
        repo.record_refinement_lifecycle(&proposal.id, "verdict_override", Some(&override_meta))
            .await
            .unwrap();

        // Signoff should succeed with override.
        let ok_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_signoff",
                        serde_json::json!({ "id": proposal.id, "kind": "technical" }),
                    )
                    .await
            })
            .await
            .unwrap();
        assert!(
            ok_resp.get("error").is_none(),
            "signoff with override should succeed: {:?}",
            ok_resp.get("error")
        );

        force_approved(&db, &proposal.id).await;

        // Graduation should succeed.
        let grad_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_graduate",
                        serde_json::json!({ "id": proposal.id }),
                    )
                    .await
            })
            .await
            .unwrap();
        assert!(
            grad_resp.get("error").is_none(),
            "graduation with override should succeed: {:?}",
            grad_resp.get("error")
        );

        // Verify the override is recorded in proposal history.
        let revisions = repo.revisions(&proposal.id).await.unwrap();
        let override_event = revisions
            .iter()
            .find(|r| r.event_kind == "verdict_override");
        assert!(
            override_event.is_some(),
            "verdict_override should appear in proposal revisions"
        );
    }

    // ── AC2 (continued): Spike finding visible in proposal_show ─────────────

    /// After a spike closes and its finding is written, proposal_show
    /// includes the finding in the debate trail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spike_finding_visible_in_proposal_show_debate_trail() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_target(
            &repo,
            &project_repo,
            &user_id,
            "Spike Finding Visibility",
            ready_body(),
            Some(r#"[{"criterion":"Works","met":false}]"#),
        )
        .await;

        let targets = repo.targets(&proposal.id).await.unwrap();
        let spike = task_repo
            .create_in_project_with_provenance(
                &targets[0].project_id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&user_id),
                    source_task_id: None,
                    proposal_id: Some(&proposal.id),
                },
                "Spike: Y feasibility",
                "Can Y handle load?",
                "Can Y handle load?",
                "spike",
                djinn_core::models::task::PRIORITY_CRITICAL,
                "planner",
                Some("open"),
                None,
            )
            .await
            .unwrap();

        repo.set_needs_evidence_spike(&proposal.id, &spike.id, "Y handles 10k rps")
            .await
            .unwrap();

        TaskRepository::new(db.clone(), EventBus::noop())
            .set_status(&spike.id, "done")
            .await
            .unwrap();

        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &proposal.id,
            kind: "rebuttal",
            body: "Spike confirms: Y handles 12k rps in benchmarks",
            blocking: false,
            agent_role: "advocate",
            author_kind: "agent",
            author_model: Some("test-advocate"),
            source_task_id: Some(&spike.id),
            against_revision_seq: proposal.latest_revision_seq,
            round: 1,
            body_metadata: None,
        })
        .await
        .unwrap();

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
                    .await
            })
            .await
            .unwrap();

        let entries = response
            .get("debate_trail")
            .and_then(|v| v.as_array())
            .expect("debate_trail should be an array");

        let finding = entries.iter().find(|e| {
            e.get("body")
                .and_then(|b| b.as_str())
                .map(|b| b.contains("12k rps"))
                .unwrap_or(false)
        });
        assert!(
            finding.is_some(),
            "spike finding should be visible in proposal_show debate_trail"
        );
        let finding = finding.unwrap();
        assert_eq!(
            finding.get("agent_role").and_then(|v| v.as_str()),
            Some("advocate")
        );
        assert_eq!(
            finding.get("source_task_id").and_then(|v| v.as_str()),
            Some(spike.id.as_str())
        );
    }

    // ── AC4: Export round-trip after refinement revision ────────────────────

    /// After a refinement checkpoint revision is applied, the proposal
    /// body still exports without parse errors and contains the enriched content.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn export_roundtrip_after_refinement_revision() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_target(
            &repo,
            &project_repo,
            &user_id,
            "Round-trip Test",
            ready_body(),
            Some(r#"[{"criterion":"Works","met":false}]"#),
        )
        .await;

        // Simulate a refinement checkpoint revision.
        let enriched_body = format!(
            "{}\n\n# Error Handling\nAll endpoints return structured errors.",
            ready_body()
        );
        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Round-trip Test",
                body: &enriched_body,
                acceptance_criteria: r#"[{"criterion":"Works","met":false}]"#,
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: Some(&serde_json::json!({
                    "role": "advocate",
                    "round": 1,
                    "checkpoint_status": "approved",
                })),
            },
        )
        .await
        .unwrap();

        // Export the proposal.
        let export_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool("proposal_export", serde_json::json!({ "id": proposal.id }))
                    .await
            })
            .await
            .unwrap();

        assert!(
            export_resp.get("error").is_none(),
            "export should succeed: {:?}",
            export_resp.get("error")
        );
        let mdx = export_resp
            .get("mdx")
            .and_then(|v| v.as_str())
            .expect("export must return mdx field");

        assert!(
            mdx.contains("Error Handling"),
            "exported MDX should contain the refinement revision content"
        );
        assert!(
            mdx.contains("structured errors"),
            "exported MDX should contain enriched body text"
        );
        assert!(
            mdx.starts_with("---\n"),
            "exported MDX should start with YAML frontmatter"
        );
        assert!(
            mdx.contains("title:"),
            "exported MDX frontmatter should contain title"
        );
        assert!(
            mdx.contains("acceptance_criteria:"),
            "exported MDX frontmatter should contain acceptance_criteria"
        );
    }
}
