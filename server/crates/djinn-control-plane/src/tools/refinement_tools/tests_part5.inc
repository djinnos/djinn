// ── Demand-evidence validation tests ──────────────────────────────
//
// Focused tests for `validate_demand_evidence`: each test sets up a
// proposal with active refinement and exercises exactly one rejection
// path.  After every rejection the test asserts that no mutation
// occurred (lifecycle count, linked_spike_task_id, debate trail, and
// proposal status are unchanged).

/// Build the common baseline params that would pass all validation
/// checks if the proposal body contains "anchor-text" and refinement
/// is active at round 1.
fn valid_demand_params(proposal_id: &str) -> serde_json::Value {
    serde_json::json!({
        "proposal_id": proposal_id,
        "round": 1,
        "against_revision_seq": 1,
        "question": "Does module X handle token expiry correctly?",
        "target_subsystem": "auth",
        "spec_unknown_anchor": "anchor-text",
        "insufficient_in_session_research": "No integration tests cover token expiry edge case",
        "expected_findings": "Evidence that token refresh is or is not required"
    })
}

/// Create a user in the DB and return the user id.  Each call uses a
/// unique github_id to avoid upsert collisions across parallel tests.
async fn create_test_user(db: &Database, login: &str) -> String {
    let user_repo = djinn_db::UserRepository::new(db.clone());
    let github_id = uuid::Uuid::now_v7().as_u128() as i64;
    let user = user_repo
        .upsert_from_github(github_id, login, None, None)
        .await
        .unwrap();
    user.id
}

/// Create a project row and link the proposal to it so that tasks
/// can be created.  Returns the project_id.
async fn link_proposal_to_project(
    db: &Database,
    repo: &ProposalRepository,
    proposal_id: &str,
) -> String {
    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
             VALUES ($1, $2, $3, $4)",
    )
    .bind(&project_id)
    .bind(format!("test-project-{project_id}"))
    .bind("test-owner")
    .bind(format!("test-repo-{project_id}"))
    .execute(db.pool())
    .await
    .unwrap();

    repo.add_target(proposal_id, &project_id, "primary")
        .await
        .unwrap();

    project_id
}

/// Create an active Judge refinement task for the given proposal,
/// attributed to `user_id`.  Returns the task id.
async fn create_judge_task(
    db: &Database,
    project_id: &str,
    proposal_id: &str,
    user_id: &str,
) -> String {
    let event_bus = djinn_core::events::EventBus::noop();
    let task_repo = djinn_db::TaskRepository::new(db.clone(), event_bus);
    let title = format!("Refinement judge — \"Test Proposal\" (round 1) [{proposal_id}]");
    let task = task_repo
            .create_in_project_with_provenance(
                project_id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(user_id),
                    source_task_id: None,
                    proposal_id: Some(proposal_id),
                },
                &title,
                &format!("Proposal refinement session: judge role for proposal {proposal_id}, round 1, against revision 1."),
                "",
                "refinement",
                0,
                "system",
                Some("open"),
                None,
            )
            .await
            .unwrap();
    task_repo
        .update_agent_type(&task.id, Some("judge"))
        .await
        .unwrap();
    task.id
}

/// Create a real spike task row suitable for storing in
/// proposals.linked_spike_task_id. The column has an FK to tasks, so tests
/// must not use arbitrary UUID strings here.
async fn create_spike_task(
    db: &Database,
    project_id: &str,
    user_id: &str,
    short_label: &str,
) -> String {
    let event_bus = djinn_core::events::EventBus::noop();
    let task_repo = djinn_db::TaskRepository::new(db.clone(), event_bus);
    task_repo
        .create_in_project_with_provenance(
            project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(user_id),
                source_task_id: None,
                proposal_id: None,
            },
            &format!("Evidence spike {short_label}"),
            "Read-only evidence spike fixture",
            "",
            "spike",
            0,
            "system",
            Some("open"),
            None,
        )
        .await
        .unwrap()
        .id
}

/// Set up a proposal with body containing "anchor-text", start
/// refinement, create a user, project, and Judge task, and return
/// (server, db, proposal, user_id, judge_task_id).
async fn setup_demand_test() -> (
    DjinnMcpServer,
    Database,
    djinn_core::models::Proposal,
    String,
    String,
) {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let p = repo
        .create(ProposalCreateInput {
            title: "Demand Validation Test",
            body: "This spec contains anchor-text for validation.",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

    // Start refinement so demand_evidence doesn't reject on inactive.
    repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
        .await
        .unwrap();

    // Create user, project, and Judge task for authorization.
    let user_id = create_test_user(&db, "judge-user").await;
    let project_id = link_proposal_to_project(&db, &repo, &p.id).await;
    let judge_task_id = create_judge_task(&db, &project_id, &p.id, &user_id).await;

    let updated = repo.get(&p.id).await.unwrap().unwrap();
    (server, db, updated, user_id, judge_task_id)
}

/// Snapshot the mutation-sensitive fields so we can assert no change
/// after a rejected demand.
async fn mutation_snapshot(
    repo: &ProposalRepository,
    proposal_id: &str,
) -> (i32, Option<String>, usize) {
    let p = repo.get(proposal_id).await.unwrap().unwrap();
    let revisions = repo.revisions(proposal_id).await.unwrap();
    let lifecycle_count = revisions
        .iter()
        .filter(|r| {
            r.event_kind == "refinement_awaiting_evidence_started"
                || r.event_kind == "refinement_demand_evidence"
                || r.event_kind == "refinement_stop"
        })
        .count();
    let trail = repo.debate_trail(proposal_id).await.unwrap();
    (
        p.latest_revision_seq,
        p.linked_spike_task_id.clone(),
        lifecycle_count + trail.len(),
    )
}

// ── AC: Non-Judge caller rejected (no session identity) ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_session_identity_rejected() {
    let (server, db, p, _user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let snap = mutation_snapshot(&repo, &p.id).await;

    // Call dispatch_tool WITHOUT a SESSION_USER_ID scope — simulates
    // an unauthenticated caller or background path with no session.
    let resp = server
        .dispatch_tool(
            "proposal_refinement_demand_evidence",
            valid_demand_params(&p.id),
        )
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("not authenticated"),
        "should mention auth failure: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Non-Judge caller rejected (wrong user) ──────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_user_rejected() {
    let (server, db, p, _user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let snap = mutation_snapshot(&repo, &p.id).await;

    // Call as a DIFFERENT user — not the one attributed to the Judge task.
    let impostor_id = create_test_user(&db, "impostor-user").await;
    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(impostor_id), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&p.id),
                )
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("not the active Judge"),
        "should mention Judge auth failure: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: No Judge task in flight rejected ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_judge_task_in_flight_rejected() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let p = repo
        .create(ProposalCreateInput {
            title: "No Judge Task Test",
            body: "has anchor-text",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

    // Start refinement but do NOT create a Judge task.
    repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
        .await
        .unwrap();

    let user_id = create_test_user(&db, "no-judge-user").await;
    let snap = mutation_snapshot(&repo, &p.id).await;

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&p.id),
                )
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("no active Judge task"),
        "should mention no Judge task: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Terminal proposal rejected ───────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_proposal_rejected() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let p = repo
        .create(ProposalCreateInput {
            title: "Terminal Test",
            body: "has anchor-text",
            acceptance_criteria: Some("[]"),
            status: Some("done"),
            body_format: None,
        })
        .await
        .unwrap();

    // Even if refinement was started (unusual but possible), terminal
    // status must block.
    repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
        .await
        .unwrap();

    let user_id = create_test_user(&db, "judge-terminal").await;
    let project_id = link_proposal_to_project(&db, &repo, &p.id).await;
    let _judge_task_id = create_judge_task(&db, &project_id, &p.id, &user_id).await;

    let snap = mutation_snapshot(&repo, &p.id).await;

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&p.id),
                )
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("terminal"),
        "should mention terminal status: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    // No mutation.
    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Inactive refinement rejected ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inactive_refinement_rejected() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let p = repo
        .create(ProposalCreateInput {
            title: "No Refinement Test",
            body: "has anchor-text",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

    // Do NOT start refinement.

    // Still need a Judge task for the auth check to pass.
    let user_id = create_test_user(&db, "judge-inactive").await;
    let project_id = link_proposal_to_project(&db, &repo, &p.id).await;
    let _judge_task_id = create_judge_task(&db, &project_id, &p.id, &user_id).await;

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&p.id),
                )
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("not active"),
        "should mention inactive refinement: {error}"
    );
}

// ── AC: Round mismatch rejected ──────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_mismatch_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let snap = mutation_snapshot(&repo, &p.id).await;

    let mut params = valid_demand_params(&p.id);
    params["round"] = serde_json::json!(99); // Wrong round.

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool("proposal_refinement_demand_evidence", params)
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("does not match"),
        "should mention round mismatch: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: against_revision_seq exceeds latest rejected ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn against_revision_seq_exceeds_latest_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let snap = mutation_snapshot(&repo, &p.id).await;

    let mut params = valid_demand_params(&p.id);
    params["against_revision_seq"] = serde_json::json!(999);

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool("proposal_refinement_demand_evidence", params)
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("exceeds"),
        "should mention revision seq exceeds: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Empty question rejected ──────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_question_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let snap = mutation_snapshot(&repo, &p.id).await;

    let mut params = valid_demand_params(&p.id);
    params["question"] = serde_json::json!("");

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool("proposal_refinement_demand_evidence", params)
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("empty"),
        "should mention empty question: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Question without '?' rejected ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn question_without_question_mark_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let snap = mutation_snapshot(&repo, &p.id).await;

    let mut params = valid_demand_params(&p.id);
    params["question"] = serde_json::json!("Tell me about module X and its token handling");

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool("proposal_refinement_demand_evidence", params)
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("falsifiable"),
        "should mention falsifiability: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Generic question rejected ────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_question_pattern_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());

    for pattern in &[
        "Please investigate further if this is correct?",
        "Can we improve the token handling?",
        "Should we design more tests for this?",
    ] {
        let snap = mutation_snapshot(&repo, &p.id).await;

        let mut params = valid_demand_params(&p.id);
        params["question"] = serde_json::json!(pattern);

        let uid = user_id.clone();
        let resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(uid), async {
                server
                    .dispatch_tool("proposal_refinement_demand_evidence", params)
                    .await
            })
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("generic"),
            "pattern '{}' should be rejected as generic: {error}",
            pattern,
        );
        assert!(
            !resp
                .get("accepted")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
        );

        let after = mutation_snapshot(&repo, &p.id).await;
        assert_eq!(snap, after, "rejected demand must not mutate state");
    }
}

// ── AC: Empty target_subsystem rejected ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_target_subsystem_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let snap = mutation_snapshot(&repo, &p.id).await;

    let mut params = valid_demand_params(&p.id);
    params["target_subsystem"] = serde_json::json!("  ");

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool("proposal_refinement_demand_evidence", params)
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("target_subsystem"),
        "should mention target_subsystem: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Missing spec_unknown_anchor in body rejected ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_spec_unknown_anchor_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let snap = mutation_snapshot(&repo, &p.id).await;

    let mut params = valid_demand_params(&p.id);
    params["spec_unknown_anchor"] = serde_json::json!("this-text-does-not-appear-in-body");

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool("proposal_refinement_demand_evidence", params)
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("not found"),
        "should mention anchor not found: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Empty insufficient_in_session_research rejected ──────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_insufficient_research_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let snap = mutation_snapshot(&repo, &p.id).await;

    let mut params = valid_demand_params(&p.id);
    params["insufficient_in_session_research"] = serde_json::json!("");

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool("proposal_refinement_demand_evidence", params)
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("insufficient_in_session_research"),
        "should mention insufficient research: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Cap exhausted rejected ───────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cap_exhausted_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());

    // Exhaust the cap with 2 existing demands in the active Judge round so
    // this test reaches the cap validation path rather than stale-context
    // validation.
    for i in 1..=2 {
        let link = djinn_db::NeedsEvidenceClaimLink {
            kind: djinn_db::NeedsEvidenceClaimLink::KIND_MARKER.to_owned(),
            proposal_id: p.id.clone(),
            judge_task_id: format!("judge-{i}"),
            spike_task_id: uuid::Uuid::now_v7().to_string(),
            round: 1,
            against_revision_seq: 1,
        };
        let meta_value = link.to_value();
        repo.add_debate_trail_entry(djinn_db::ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "needs_evidence",
            body: &format!("existing demand {i}"),
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: Some(&meta_value),
        })
        .await
        .unwrap();
    }

    let snap = mutation_snapshot(&repo, &p.id).await;

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&p.id),
                )
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("cap"),
        "should mention cap exceeded: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Existing open linked spike rejected ──────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_open_linked_spike_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());

    // Simulate an existing open spike by setting linked_spike_task_id to a
    // real task row (the proposals column has an FK constraint).
    let project_id = link_proposal_to_project(&db, &repo, &p.id).await;
    let spike_id = create_spike_task(&db, &project_id, &user_id, "existing").await;
    repo.set_needs_evidence_spike(&p.id, &spike_id, "existing claim")
        .await
        .unwrap();

    let snap = mutation_snapshot(&repo, &p.id).await;

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&p.id),
                )
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("already has an open linked evidence spike"),
        "should mention existing spike: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Awaiting review (converged) refinement rejected ──────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn awaiting_review_refinement_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());

    // Record convergence: awaiting_review lifecycle event after the start.
    repo.record_refinement_lifecycle(
        &p.id,
        "refinement_awaiting_review",
        Some(&serde_json::json!({
            "judge_summary": "Spec is ready",
            "snapshot_revision_seq": p.latest_revision_seq,
        })),
    )
    .await
    .unwrap();

    let snap = mutation_snapshot(&repo, &p.id).await;

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&p.id),
                )
                .await
        })
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("awaiting human review"),
        "should mention awaiting review: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    let after = mutation_snapshot(&repo, &p.id).await;
    assert_eq!(snap, after, "rejected demand must not mutate state");
}

// ── AC: Valid demand accepted ────────────────────────────────────

/// Sanity check: a valid demand with all fields correct and
/// the body containing the anchor text should be accepted.
/// Verifies the full accepted-demand mutation path:
/// - Spike task created with `issue_type = "spike"` and correct labels
/// - `needs_evidence` debate entry written
/// - `linked_spike_task_id` set on the proposal
/// - `refinement_awaiting_evidence_started` lifecycle event recorded
/// - `spike_task_id` returned in the response
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_demand_accepted() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let task_repo = djinn_db::TaskRepository::new(db.clone(), EventBus::noop());

    let resp = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&p.id),
                )
                .await
        })
        .await
        .expect("tool should be registered");

    assert!(
        resp.get("error").and_then(|v| v.as_str()).is_none(),
        "valid demand should not error: {:?}",
        resp.get("error"),
    );
    assert!(
        resp.get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "valid demand should be accepted"
    );

    // Verify spike_task_id is returned (not None).
    let spike_id = resp
        .get("result")
        .and_then(|r| r.get("spike_task_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    assert!(
        spike_id.is_some(),
        "spike_task_id must be Some in accepted response"
    );

    // Verify the spike task was created with correct issue_type and labels.
    let spike_id = spike_id.unwrap();
    let spike = task_repo
        .get(&spike_id)
        .await
        .unwrap()
        .expect("spike task must exist");
    assert_eq!(
        spike.issue_type, "spike",
        "spike task must have issue_type = 'spike'"
    );
    let labels: Vec<String> = serde_json::from_str(&spike.labels).unwrap_or_default();
    assert!(
        labels.contains(&"refinement-evidence".to_string()),
        "must have refinement-evidence label"
    );
    assert!(
        labels.contains(&"read-only".to_string()),
        "must have read-only label"
    );
    assert!(
        labels.iter().any(|l| l.starts_with("proposal:")),
        "must have proposal:<short_id> label"
    );
    assert_eq!(
        spike.agent_type.as_deref(),
        Some("architect"),
        "spike agent_type must be architect"
    );

    // Verify the proposal has linked_spike_task_id set.
    let updated_proposal = repo.get(&p.id).await.unwrap().unwrap();
    assert_eq!(
        updated_proposal.linked_spike_task_id.as_deref(),
        Some(spike_id.as_str()),
        "proposal linked_spike_task_id must point to the spike task"
    );
    assert!(
        updated_proposal.needs_evidence_claim.is_some(),
        "proposal must have needs_evidence_claim set"
    );

    // Verify a needs_evidence debate entry was written.
    let trail = repo.debate_trail(&p.id).await.unwrap();
    let ne_entries: Vec<_> = trail
        .iter()
        .filter(|e| e.kind == "needs_evidence")
        .collect();
    assert_eq!(
        ne_entries.len(),
        1,
        "should record exactly one needs_evidence debate entry"
    );
    assert!(
        ne_entries[0].blocking,
        "needs_evidence entry must be blocking"
    );
    assert_eq!(
        ne_entries[0].agent_role, "judge",
        "needs_evidence entry must be from judge"
    );

    // Verify the refinement_awaiting_evidence_started lifecycle event.
    let revisions = repo.revisions(&p.id).await.unwrap();
    let awaiting_events = revisions
        .iter()
        .filter(|r| r.event_kind == "refinement_awaiting_evidence_started")
        .count();
    assert_eq!(
        awaiting_events, 1,
        "should record exactly one refinement_awaiting_evidence_started lifecycle event"
    );
}

// ── AC: Duplicate demand cannot create two open spikes ───────────

/// Verify that a second valid demand after an accepted first demand
/// is rejected because the proposal already has an open linked spike.
/// This is the sequential duplicate scenario; concurrent races are
/// protected at the DB level by the atomic set_structured_needs_evidence_spike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_demand_after_accepted_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());

    // First demand — should succeed and create a spike.
    let resp1 = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&p.id),
                )
                .await
        })
        .await
        .expect("tool should be registered");

    assert!(
        resp1
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "first demand should be accepted: {:?}",
        resp1.get("error"),
    );
    let first_spike = resp1
        .get("result")
        .and_then(|r| r.get("spike_task_id"))
        .and_then(|v| v.as_str())
        .expect("first demand should have spike_task_id")
        .to_string();

    // Second demand — should be rejected by validation gate (step 11).
    let resp2 = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&p.id),
                )
                .await
        })
        .await
        .expect("tool should be registered");

    assert!(
        !resp2
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        "second demand should be rejected"
    );
    let error = resp2.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        error.contains("already has an open linked evidence spike"),
        "should mention existing spike: {error}"
    );

    // Verify exactly one spike is linked to the proposal.
    let updated = repo.get(&p.id).await.unwrap().unwrap();
    assert_eq!(
        updated.linked_spike_task_id.as_deref(),
        Some(first_spike.as_str()),
        "proposal should still be linked to the first spike only"
    );

    // Verify exactly one needs_evidence debate entry exists.
    let trail = repo.debate_trail(&p.id).await.unwrap();
    let ne_count = trail.iter().filter(|e| e.kind == "needs_evidence").count();
    assert_eq!(
        ne_count, 1,
        "should have exactly one needs_evidence debate entry"
    );
}
