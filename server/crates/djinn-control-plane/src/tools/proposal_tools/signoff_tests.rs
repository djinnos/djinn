// Tests for signoff, readiness, composed-gate, and debate blocking.
//
// Split out of mod.rs so the production signoff module stays under the
// size-guard threshold; behavior and expectations are unchanged.

mod signoff_readiness_tests {
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, ProjectRepository, ProposalCreateInput, ProposalRepository, UserRepository,
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

    /// A minimal body that fails most readiness checks (missing problem,
    /// scope, objectives, grounding, dependencies, open questions).
    fn incomplete_body() -> &'static str {
        "Just some random text without any required sections."
    }

    async fn setup_test_server_and_user() -> (DjinnMcpServer, Database, String) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user = UserRepository::new(db.clone())
            .upsert_from_github(999_700, "signoff-test-user", None, None)
            .await
            .unwrap();
        UserRepository::new(db.clone())
            .set_role(&user.id, "engineer")
            .await
            .unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db, user.id)
    }

    /// A draft proposal with incomplete readiness fails on first sign-off
    /// and remains `draft` with no new sign-off.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn draft_incomplete_proposal_fails_signoff_and_remains_draft() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-draft-inc", "test", "svc-draft-inc")
            .await
            .unwrap();

        let proposal = repo
            .create(ProposalCreateInput {
                title: "Incomplete Draft",
                body: incomplete_body(),
                acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        // Add a target so target_count > 0 (one fewer failure to worry about).
        repo.add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_signoff",
                        serde_json::json!({ "id": proposal.id, "kind": "technical" }),
                    )
                    .await
            })
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(
            error.is_some(),
            "expected readiness error, got: {response:?}"
        );
        let error = error.unwrap();
        assert!(
            error.contains("proposal not ready for review"),
            "error should mention readiness: {error}"
        );

        // Proposal must still be `draft` — sign-off was never persisted.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "draft", "status must remain draft");

        // No sign-offs recorded.
        let signoffs = repo.signoffs(&proposal.id).await.unwrap();
        assert!(signoffs.is_empty(), "no sign-offs should be recorded");
    }

    /// A complete draft proposal can receive a sign-off and advance to
    /// `in_review` (one of two required sign-off kinds).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn draft_complete_proposal_accepts_signoff_and_advances() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-draft-ok", "test", "svc-draft-ok")
            .await
            .unwrap();

        let proposal = repo
            .create(ProposalCreateInput {
                title: "Complete Draft",
                body: ready_body(),
                acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        repo.add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
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
            response.get("error").is_none(),
            "sign-off should succeed: {:?}",
            response.get("error")
        );

        // Proposal must have advanced to `in_review` (one of two kinds given).
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(
            stored.status, "in_review",
            "status must advance to in_review"
        );

        // One sign-off recorded.
        let signoffs = repo.signoffs(&proposal.id).await.unwrap();
        assert_eq!(signoffs.len(), 1, "one sign-off should be recorded");
    }
}

mod composed_gate_tests {
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

    async fn setup_test_server_and_user() -> (DjinnMcpServer, Database, String) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user = UserRepository::new(db.clone())
            .upsert_from_github(999_900, "gate-test-user", None, None)
            .await
            .unwrap();
        UserRepository::new(db.clone())
            .set_role(&user.id, "engineer")
            .await
            .unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db, user.id)
    }

    async fn force_approved(db: &Database, proposal_id: &str) {
        ProposalRepository::new(db.clone(), EventBus::noop())
            .set_status(proposal_id, "approved")
            .await
            .unwrap();
    }

    fn incomplete_body() -> &'static str {
        "Just some random text without any required sections."
    }

    async fn create_proposal_with_body(
        repo: &ProposalRepository,
        project_repo: &ProjectRepository,
        user_id: &str,
        title: &str,
        body: &str,
    ) -> djinn_core::models::proposal::Proposal {
        let project = project_repo
            .create(
                &format!("svc-gate-{}", uuid::Uuid::now_v7()),
                "test",
                &format!("svc-gate-{}", uuid::Uuid::now_v7()),
            )
            .await
            .unwrap();
        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.to_string()), async {
                repo.create(ProposalCreateInput {
                    title,
                    body,
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
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

    async fn show_gate_status(
        server: &DjinnMcpServer,
        user_id: &str,
        proposal_id: &str,
    ) -> serde_json::Value {
        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.to_string()), async {
                server
                    .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal_id }))
                    .await
            })
            .await
            .unwrap();
        assert!(
            response.get("error").is_none(),
            "proposal_show should succeed: {:?}",
            response.get("error")
        );
        response
            .get("gate_status")
            .cloned()
            .expect("proposal_show must include gate_status")
    }

    /// Create a complete, ready proposal in draft with a target.
    async fn create_ready_proposal(
        repo: &ProposalRepository,
        project_repo: &ProjectRepository,
        user_id: &str,
        title: &str,
    ) -> djinn_core::models::proposal::Proposal {
        create_proposal_with_body(repo, project_repo, user_id, title, ready_body()).await
    }

    /// A needs-work judge verdict blocks sign-off with a deterministic
    /// message naming the verdict id and missing override.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_work_verdict_blocks_signoff_with_deterministic_message() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal =
            create_ready_proposal(&repo, &project_repo, &user_id, "NW Verdict Block").await;

        // Add a needs-work judge verdict.
        let verdict = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "verdict",
                body: "needs-work: spec is unclear on X",
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

        // Attempt sign-off — should be blocked.
        let response = djinn_core::auth_context::SESSION_USER_ID
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

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(error.is_some(), "expected error, got: {response:?}");
        let error = error.unwrap();
        assert!(
            error.contains("judge returned needs-work"),
            "error should mention needs-work: {error}"
        );
        assert!(
            error.contains(&verdict.id),
            "error should name the verdict id: {error}"
        );
        assert!(
            error.contains("no current human override"),
            "error should mention missing override: {error}"
        );

        // Proposal should still be draft — no sign-off recorded.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "draft");
    }

    /// Regression (gate-verdict-supersession): stale reject verdicts from
    /// earlier tribunal rounds must never count as unresolved blocking rows.
    /// Once a later approve verdict supersedes them, the gate is ready — the
    /// reject verdicts have nothing that resolves them and would otherwise
    /// block the proposal forever ("blocking rows: N" with "Judge verdict:
    /// Ready").
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn superseded_reject_verdicts_do_not_block_gate() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal =
            create_ready_proposal(&repo, &project_repo, &user_id, "Superseded Verdicts").await;

        // Three rounds of blocking reject verdicts (the judge's own REJECTs).
        for (round, body) in [
            (1, "needs-work: round 1"),
            (2, "needs-work: round 2"),
            (3, "needs-work: round 3"),
        ] {
            repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "verdict",
                body,
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: Some("test-judge"),
                source_task_id: None,
                against_revision_seq: proposal.latest_revision_seq,
                round,
                body_metadata: None,
            })
            .await
            .unwrap();
        }

        // Latest verdict is an approve — it supersedes the rejects.
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &proposal.id,
            kind: "verdict",
            body: "Ready",
            blocking: false,
            agent_role: "judge",
            author_kind: "agent",
            author_model: Some("test-judge"),
            source_task_id: None,
            against_revision_seq: proposal.latest_revision_seq,
            round: 4,
            body_metadata: None,
        })
        .await
        .unwrap();

        let gate = show_gate_status(&server, &user_id, &proposal.id).await;
        assert_eq!(
            gate.get("unresolved_blocking_count")
                .and_then(|v| v.as_i64()),
            Some(0),
            "superseded reject verdicts must not count as unresolved blocking: {gate:?}"
        );
        assert_eq!(
            gate.get("judge_needs_work").and_then(|v| v.as_bool()),
            Some(false),
            "latest verdict is approve, so judge_needs_work is false: {gate:?}"
        );
        assert_eq!(
            gate.get("ready").and_then(|v| v.as_bool()),
            Some(true),
            "gate should be ready once the latest verdict approves: {gate:?}"
        );
    }

    /// The latest-verdict channel still gates: when the newest verdict is a
    /// reject, `judge_needs_work` is true and the gate is not ready — even
    /// though verdict rows no longer count as unresolved blocking entries.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_reject_verdict_still_blocks_via_needs_work_channel() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_ready_proposal(&repo, &project_repo, &user_id, "Latest Reject").await;

        // An earlier approve, then a later reject — latest wins.
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &proposal.id,
            kind: "verdict",
            body: "Ready",
            blocking: false,
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
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &proposal.id,
            kind: "verdict",
            body: "needs-work: regression found",
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: Some("test-judge"),
            source_task_id: None,
            against_revision_seq: proposal.latest_revision_seq,
            round: 2,
            body_metadata: None,
        })
        .await
        .unwrap();

        let gate = show_gate_status(&server, &user_id, &proposal.id).await;
        assert_eq!(
            gate.get("unresolved_blocking_count")
                .and_then(|v| v.as_i64()),
            Some(0),
            "verdict rows never count as unresolved blocking: {gate:?}"
        );
        assert_eq!(
            gate.get("judge_needs_work").and_then(|v| v.as_bool()),
            Some(true),
            "latest verdict is a reject — judge_needs_work must be true: {gate:?}"
        );
        assert_eq!(
            gate.get("ready").and_then(|v| v.as_bool()),
            Some(false),
            "gate must not be ready with a latest reject verdict: {gate:?}"
        );
    }

    /// A needs-evidence spike blocks graduation with a deterministic
    /// message naming the spike task id and claim.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_evidence_spike_blocks_graduation_with_deterministic_message() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

        let proposal =
            create_ready_proposal(&repo, &project_repo, &user_id, "NE Spike Block").await;

        // Create a spike task and park the proposal.
        let targets = repo.targets(&proposal.id).await.unwrap();
        let target_project_id = &targets[0].project_id;
        let spike_creator_key = uuid::Uuid::now_v7();
        let spike_creator = UserRepository::new(db.clone())
            .upsert_from_github(
                spike_creator_key.as_u128() as i64,
                &format!("signoff-spike-{spike_creator_key}"),
                None,
                None,
            )
            .await
            .unwrap();
        let spike = task_repo
            .create_in_project_with_provenance(
                target_project_id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&spike_creator.id),
                    source_task_id: None,
                    proposal_id: None,
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

        // Force to approved for graduation test.
        force_approved(&db, &proposal.id).await;

        // Attempt graduation — should be blocked.
        let response = djinn_core::auth_context::SESSION_USER_ID
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

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(error.is_some(), "expected error, got: {response:?}");
        let error = error.unwrap();
        assert!(
            error.contains("proposal parked on needs-evidence spike"),
            "error should mention needs-evidence: {error}"
        );
        assert!(
            error.contains(&spike.id),
            "error should name the spike task id: {error}"
        );
        assert!(
            error.contains("X is load-bearing"),
            "error should name the claim: {error}"
        );

        // Proposal should still be approved — graduation was blocked.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "approved");
    }

    /// An unresolved blocking debate objection blocks sign-off with a
    /// deterministic message naming the entry id(s).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_debate_entry_blocks_signoff_with_entry_ids() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal =
            create_ready_proposal(&repo, &project_repo, &user_id, "Blocking Debate").await;

        // Add a blocking objection from adversary.
        let objection = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "objection",
                body: "Missing error handling section",
                blocking: true,
                agent_role: "adversary",
                author_kind: "agent",
                author_model: Some("test-adversary"),
                source_task_id: None,
                against_revision_seq: proposal.latest_revision_seq,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap();

        // Attempt sign-off — should be blocked.
        let response = djinn_core::auth_context::SESSION_USER_ID
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

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(error.is_some(), "expected error, got: {response:?}");
        let error = error.unwrap();
        assert!(
            error.contains("unresolved blocking debate entries"),
            "error should mention blocking debate: {error}"
        );
        assert!(
            error.contains(&objection.id),
            "error should name the objection id: {error}"
        );

        // After resolving the objection, sign-off should succeed.
        repo.resolve_debate_trail_entry(&objection.id)
            .await
            .unwrap();

        let response2 = djinn_core::auth_context::SESSION_USER_ID
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
            response2.get("error").is_none(),
            "sign-off after resolve should succeed: {:?}",
            response2.get("error")
        );
    }

    /// A current human override allows sign-off despite a needs-work
    /// judge verdict.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_override_allows_signoff_past_needs_work_verdict() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_ready_proposal(&repo, &project_repo, &user_id, "Override Path").await;

        // Add a needs-work judge verdict.
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &proposal.id,
            kind: "verdict",
            body: "needs-work: missing scope detail",
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

        // Without override, sign-off should fail.
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
            "sign-off should fail without override"
        );

        // Record a verdict override at the current revision.
        let override_meta = serde_json::json!({
            "override_on_revision_seq": proposal.latest_revision_seq,
            "reason": "PM reviewed and approved scope as-is",
            "override_by_user_id": user_id
        });
        repo.record_refinement_lifecycle(&proposal.id, "verdict_override", Some(&override_meta))
            .await
            .unwrap();

        // Now sign-off should succeed because the override is current.
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
            "sign-off with current override should succeed: {:?}",
            ok_resp.get("error")
        );

        // Sign-off should be recorded.
        let signoffs = repo.signoffs(&proposal.id).await.unwrap();
        assert_eq!(signoffs.len(), 1, "one sign-off should be recorded");
    }

    /// Without current human authority, deterministic DoR failures still block
    /// sign-off before any sign-off is recorded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dor_failure_blocks_signoff_without_current_human_authority() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_body(
            &repo,
            &project_repo,
            &user_id,
            "DoR Block No Authority",
            incomplete_body(),
        )
        .await;

        let response = djinn_core::auth_context::SESSION_USER_ID
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

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(error.is_some(), "sign-off should fail without authority");
        let error = error.unwrap();
        assert!(
            error.contains("Missing required coverage: problem"),
            "error should keep deterministic DoR details: {error}"
        );
        assert!(repo.signoffs(&proposal.id).await.unwrap().is_empty());
    }

    /// `proposal_show` keeps DoR diagnostics visible under a current human
    /// override but does not report those diagnostics as blocking explanations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gate_status_dor_only_current_override_is_ready_with_diagnostics() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_body(
            &repo,
            &project_repo,
            &user_id,
            "Show DoR Override Ready",
            incomplete_body(),
        )
        .await;

        let override_meta = serde_json::json!({
            "override_on_revision_seq": proposal.latest_revision_seq,
            "reason": "Human reviewer accepted deterministic DoR risk",
            "override_by_user_id": user_id
        });
        repo.record_refinement_lifecycle(&proposal.id, "verdict_override", Some(&override_meta))
            .await
            .unwrap();

        let gate = show_gate_status(&server, &user_id, &proposal.id).await;
        assert_eq!(gate.get("ready").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(gate.get("dor_ready").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            gate.get("human_override_active").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            gate.get("dor_failures")
                .and_then(|v| v.as_array())
                .is_some_and(|failures| !failures.is_empty()),
            "DoR diagnostics should remain visible: {gate:?}"
        );
        assert!(
            gate.get("blocked_explanations")
                .and_then(|v| v.as_array())
                .is_some_and(|explanations| explanations.is_empty()),
            "overridden DoR-only failures should not be blocking explanations: {gate:?}"
        );
    }

    /// Without current authority, `proposal_show` preserves the historical
    /// DoR-only blocking status and explanations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gate_status_dor_only_without_override_blocks_with_explanation() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_body(
            &repo,
            &project_repo,
            &user_id,
            "Show DoR No Override",
            incomplete_body(),
        )
        .await;

        let gate = show_gate_status(&server, &user_id, &proposal.id).await;
        assert_eq!(gate.get("ready").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(gate.get("dor_ready").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            gate.get("human_override_active").and_then(|v| v.as_bool()),
            Some(false)
        );
        let explanations = gate
            .get("blocked_explanations")
            .and_then(|v| v.as_array())
            .expect("blocked_explanations must be present");
        assert!(
            explanations.iter().any(|v| v
                .as_str()
                .is_some_and(|s| s.contains("Missing required coverage: problem"))),
            "DoR block should remain a blocking explanation without override: {gate:?}"
        );
    }

    /// DoR authority is revision-scoped in `proposal_show`; advancing the
    /// proposal revision makes the previous authority stale.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gate_status_stale_dor_override_after_revision_advance_blocks() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_body(
            &repo,
            &project_repo,
            &user_id,
            "Show DoR Override Stale",
            incomplete_body(),
        )
        .await;

        let override_meta = serde_json::json!({
            "override_on_revision_seq": proposal.latest_revision_seq,
            "reason": "Accepted original incomplete draft"
        });
        repo.record_refinement_lifecycle(&proposal.id, "verdict_override", Some(&override_meta))
            .await
            .unwrap();

        let updated = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.update(
                    &proposal.id,
                    djinn_db::ProposalUpdateInput {
                        title: &proposal.title,
                        body: "Different incomplete text after a material edit.",
                        acceptance_criteria: &proposal.acceptance_criteria,
                        status: &proposal.status,
                        superseded_by: proposal.superseded_by.as_deref(),
                        body_format: Some(&proposal.body_format),
                        event_metadata: None,
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert!(updated.latest_revision_seq > proposal.latest_revision_seq);

        let gate = show_gate_status(&server, &user_id, &updated.id).await;
        assert_eq!(gate.get("ready").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            gate.get("human_override_active").and_then(|v| v.as_bool()),
            Some(false)
        );
        let explanations = gate
            .get("blocked_explanations")
            .and_then(|v| v.as_array())
            .expect("blocked_explanations must be present");
        assert!(
            explanations.iter().any(|v| v
                .as_str()
                .is_some_and(|s| s.contains("Missing required coverage: problem"))),
            "stale authority should not suppress DoR blocking explanations: {gate:?}"
        );
    }

    /// A current explicit human override excludes deterministic DoR failures
    /// from both sign-off and graduation composed gates when no tribunal
    /// condition remains blocking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_override_allows_dor_only_signoff_and_graduation() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_body(
            &repo,
            &project_repo,
            &user_id,
            "DoR Override Success",
            incomplete_body(),
        )
        .await;

        let override_meta = serde_json::json!({
            "override_on_revision_seq": proposal.latest_revision_seq,
            "reason": "Human reviewer accepted deterministic DoR risk",
            "override_by_user_id": user_id
        });
        repo.record_refinement_lifecycle(&proposal.id, "verdict_override", Some(&override_meta))
            .await
            .unwrap();

        let signoff_resp = djinn_core::auth_context::SESSION_USER_ID
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
            signoff_resp.get("error").is_none(),
            "current override should allow DoR-only sign-off: {:?}",
            signoff_resp.get("error")
        );

        force_approved(&db, &proposal.id).await;
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
            "current override should allow DoR-only graduation: {:?}",
            grad_resp.get("error")
        );
    }

    /// A current human-accepted refinement stop is also human authority for the
    /// latest revision, so DoR false positives do not block sign-off.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_human_accept_allows_dor_only_signoff() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_body(
            &repo,
            &project_repo,
            &user_id,
            "DoR Human Accept Success",
            incomplete_body(),
        )
        .await;

        let accept_meta = serde_json::json!({
            "source": "human_review",
            "event": "refinement_stop",
            "reason_tag": "human_accepted"
        });
        repo.record_refinement_lifecycle(&proposal.id, "refinement_stop", Some(&accept_meta))
            .await
            .unwrap();

        let response = djinn_core::auth_context::SESSION_USER_ID
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
            response.get("error").is_none(),
            "current human accept should allow DoR-only sign-off: {:?}",
            response.get("error")
        );
        assert_eq!(repo.signoffs(&proposal.id).await.unwrap().len(), 1);
    }

    /// DoR override authority is revision-scoped; after a material edit advances
    /// the proposal revision, the same override no longer suppresses DoR blocks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dor_override_is_stale_after_revision_advances() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal = create_proposal_with_body(
            &repo,
            &project_repo,
            &user_id,
            "DoR Override Stale",
            incomplete_body(),
        )
        .await;

        let override_meta = serde_json::json!({
            "override_on_revision_seq": proposal.latest_revision_seq,
            "reason": "Accepted original incomplete draft"
        });
        repo.record_refinement_lifecycle(&proposal.id, "verdict_override", Some(&override_meta))
            .await
            .unwrap();

        let updated = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.update(
                    &proposal.id,
                    djinn_db::ProposalUpdateInput {
                        title: &proposal.title,
                        body: "Different incomplete text after a material edit.",
                        acceptance_criteria: &proposal.acceptance_criteria,
                        status: &proposal.status,
                        superseded_by: proposal.superseded_by.as_deref(),
                        body_format: Some(&proposal.body_format),
                        event_metadata: None,
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert!(updated.latest_revision_seq > proposal.latest_revision_seq);

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_signoff",
                        serde_json::json!({ "id": updated.id, "kind": "technical" }),
                    )
                    .await
            })
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(
            error.is_some(),
            "stale override should not allow DoR-only sign-off"
        );
        let error = error.unwrap();
        assert!(
            error.contains("Missing required coverage: problem"),
            "stale override should expose DoR block: {error}"
        );
    }

    /// A stale override (different revision) does not unlock a needs-work
    /// verdict.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_override_does_not_unlock_needs_work() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());

        let proposal =
            create_ready_proposal(&repo, &project_repo, &user_id, "Stale Override").await;

        // Add a needs-work judge verdict.
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &proposal.id,
            kind: "verdict",
            body: "needs-work: unclear boundaries",
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

        // Record a stale override at revision 0 (proposal is at revision 1).
        let override_meta = serde_json::json!({
            "override_on_revision_seq": 0,
            "reason": "earlier override before spec changed"
        });
        repo.record_refinement_lifecycle(&proposal.id, "verdict_override", Some(&override_meta))
            .await
            .unwrap();

        // Sign-off should still fail — the override is stale.
        let response = djinn_core::auth_context::SESSION_USER_ID
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

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(error.is_some(), "sign-off should fail with stale override");
        let error = error.unwrap();
        assert!(
            error.contains("no current human override"),
            "error should mention stale/missing override: {error}"
        );
    }
}


// P4 tribunal regression tests — extracted to `tribunal_tests.rs`
// to meet the 1500-line file-size guard.
include!("tribunal_tests.rs");
