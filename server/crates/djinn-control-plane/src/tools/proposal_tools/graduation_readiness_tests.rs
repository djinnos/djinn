// Graduation readiness tests — extracted from mod.rs to meet the
// 1500-line file-size guard.  Behavior and expectations are unchanged.

mod graduation_readiness_tests {
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, ProjectRepository, ProposalCreateInput, ProposalRepository, TaskRepository,
        UserRepository,
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
            .upsert_from_github(999_800, "graduate-test-user", None, None)
            .await
            .unwrap();
        UserRepository::new(db.clone())
            .set_role(&user.id, "engineer")
            .await
            .unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db, user.id)
    }

    /// Advance a proposal directly to `approved` via SQL, simulating
    /// legacy data or a proposal that pre-dates the readiness gate.
    async fn force_approved(db: &Database, proposal_id: &str) {
        ProposalRepository::new(db.clone(), EventBus::noop())
            .set_status(proposal_id, "approved")
            .await
            .unwrap();
    }

    /// An approved proposal missing required readiness sections fails
    /// graduation with missing-check names in the error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approved_missing_sections_fails_graduation_with_check_names() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-grad-missing", "test", "svc-grad-missing")
            .await
            .unwrap();

        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Incomplete Graduation",
                    body: incomplete_body(),
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

        force_approved(&db, &proposal.id).await;

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
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
        assert!(
            error.is_some(),
            "expected readiness error, got: {response:?}"
        );
        let error = error.unwrap();
        assert!(
            error.contains("proposal not ready for review"),
            "error should mention readiness: {error}"
        );
        // At least some of the missing-section details should appear.
        assert!(
            error.contains("Missing required coverage"),
            "error should mention missing coverage: {error}"
        );

        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "approved");
        assert!(stored.build_breakdown_task_id.is_none());
        assert!(stored.build_owner_user_id.is_none());
    }

    /// A complete approved proposal graduates: the breakdown planning task
    /// is created, status moves to `building`, and build owner is set.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_approved_proposal_graduates_and_creates_breakdown_task() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-grad-ok", "test", "svc-grad-ok")
            .await
            .unwrap();

        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Complete Graduation",
                    body: ready_body(),
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

        force_approved(&db, &proposal.id).await;

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

        assert!(
            response.get("error").is_none(),
            "graduation should succeed: {:?}",
            response.get("error")
        );

        // Proposal must now be `building`.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "building", "status must advance to building");

        // Build owner must be the caller.
        assert_eq!(
            stored.build_owner_user_id.as_deref(),
            Some(user_id.as_str()),
            "build owner must be the caller"
        );

        // Breakdown task must be set.
        let breakdown_id = stored
            .build_breakdown_task_id
            .as_deref()
            .expect("breakdown task id must be set after graduation");
        let breakdown = task_repo
            .get(breakdown_id)
            .await
            .unwrap()
            .expect("breakdown task must exist");
        assert_eq!(breakdown.issue_type, "epic_breakdown");
        assert!(
            breakdown.title.contains("Complete Graduation"),
            "breakdown title must reference the proposal: {}",
            breakdown.title
        );
    }

    /// Regression: existing guardrails (capability, non-approved status)
    /// still fire before readiness.  A non-approved proposal fails
    /// graduation with the status guardrail error, not the readiness error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_approved_proposal_fails_with_status_guardrail() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-grad-status", "test", "svc-grad-status")
            .await
            .unwrap();

        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Draft Proposal",
                    body: ready_body(),
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

        // Do NOT advance to `approved` — the proposal is still `draft`.
        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
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
        // Must be the status guardrail, NOT the readiness error.
        assert!(
            error.contains("proposal must be approved"),
            "error must be the status guardrail: {error}"
        );
        assert!(
            !error.contains("proposal not ready"),
            "readiness must NOT mask the status guardrail: {error}"
        );
    }

    /// Regression: the no-primary-target guardrail still fires before
    /// readiness.  A proposal without targets fails with the target
    /// guardrail, not the readiness error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_primary_target_fails_with_target_guardrail() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "No Target Proposal",
                    body: ready_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();

        force_approved(&db, &proposal.id).await;

        // Do NOT add any target.
        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
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
        // Must be the primary-target guardrail, NOT the readiness error.
        assert!(
            error.contains("no primary target"),
            "error must be the primary-target guardrail: {error}"
        );
    }

    /// Lifecycle regression: the readiness error format is consistent
    /// across update (review promotion), sign-off, and graduation.
    /// Each path must surface the same "proposal not ready for review"
    /// preamble and missing-section / vague-AC detail structure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readiness_error_format_is_consistent_across_lifecycle_gates() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-grad-format", "test", "svc-grad-format")
            .await
            .unwrap();

        // --- Update path: attempt to promote a draft to in_review ---
        let update_proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Format Check Update",
                    body: incomplete_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();
        repo.add_target(&update_proposal.id, &project.id, "primary")
            .await
            .unwrap();

        let update_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_update",
                        serde_json::json!({
                            "id": update_proposal.id,
                            "status": "in_review"
                        }),
                    )
                    .await
            })
            .await
            .unwrap();

        let update_err = update_resp.get("error").and_then(|v| v.as_str());
        assert!(update_err.is_some(), "update should fail: {update_resp:?}");
        let update_err = update_err.unwrap();
        assert!(
            update_err.starts_with("proposal not ready for review:"),
            "update error must start with readiness preamble: {update_err}"
        );

        // --- Sign-off path: attempt sign-off on a draft ---
        let signoff_proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Format Check Signoff",
                    body: incomplete_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();
        repo.add_target(&signoff_proposal.id, &project.id, "primary")
            .await
            .unwrap();

        let signoff_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_signoff",
                        serde_json::json!({
                            "id": signoff_proposal.id,
                            "kind": "technical"
                        }),
                    )
                    .await
            })
            .await
            .unwrap();

        let signoff_err = signoff_resp.get("error").and_then(|v| v.as_str());
        assert!(
            signoff_err.is_some(),
            "signoff should fail: {signoff_resp:?}"
        );
        let signoff_err = signoff_err.unwrap();
        assert!(
            signoff_err.starts_with("proposal not ready for review:"),
            "signoff error must start with readiness preamble: {signoff_err}"
        );

        // --- Graduation path: attempt graduation on an approved proposal ---
        let grad_proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Format Check Graduate",
                    body: incomplete_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();
        repo.add_target(&grad_proposal.id, &project.id, "primary")
            .await
            .unwrap();
        force_approved(&db, &grad_proposal.id).await;

        let grad_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_graduate",
                        serde_json::json!({ "id": grad_proposal.id }),
                    )
                    .await
            })
            .await
            .unwrap();

        let grad_err = grad_resp.get("error").and_then(|v| v.as_str());
        assert!(grad_err.is_some(), "graduation should fail: {grad_resp:?}");
        let grad_err = grad_err.unwrap();
        assert!(
            grad_err.starts_with("proposal not ready for review:"),
            "graduation error must start with readiness preamble: {grad_err}"
        );

        // All three paths must use the same error format: they should all
        // contain the same set of missing-section details for this body.
        for err in [update_err, signoff_err, grad_err] {
            assert!(
                err.contains("Missing required coverage: problem"),
                "all gates must report missing problem: {err}"
            );
        }
    }
}
