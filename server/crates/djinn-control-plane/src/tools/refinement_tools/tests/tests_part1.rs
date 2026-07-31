use super::*;

pub(super) async fn test_server() -> (DjinnMcpServer, Database) {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
}

/// Terminal status assertions must use the durable run transition, rather
/// than a legacy display-only lifecycle stop.
async fn terminalize_started_run(
    repo: &ProposalRepository,
    proposal_id: &str,
    reason: djinn_core::refinement_liveness::RefinementStopReason,
) {
    let runs = repo
        .load_refinement_run_aggregates(proposal_id, 60_000)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1, "start must admit exactly one exact run");
    let run = &runs[0];
    assert!(
        repo.terminal_refinement_run(djinn_db::TerminalRefinementRunRequest {
            run_id: run.run_id.clone(),
            generation: run.generation,
            reason,
        })
        .await
        .unwrap()
    );
}

fn assert_terminal_refinement(refinement: &serde_json::Value, expected_stop_tag: &str) {
    assert_eq!(refinement["active"], false);
    assert_eq!(refinement["run_state"], "terminal");
    assert_eq!(refinement["liveness"], "terminal");
    assert_eq!(refinement["stop_reason"], expected_stop_tag);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_start_creates_lifecycle_event() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Refinement Test",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: None, // defaults to draft
            body_format: None,
        })
        .await
        .unwrap();
    link_proposal_to_project(&db, &repo, &proposal.id).await;

    let resp = server
        .dispatch_tool(
            "proposal_refinement_start",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .expect("tool should be registered");

    assert!(
        resp.get("error").and_then(|v| v.as_str()).is_none(),
        "expected no error, got: {:?}",
        resp.get("error")
    );
    let refinement = resp.get("refinement").expect("should have refinement");
    assert_eq!(
        refinement.get("active").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        refinement.get("current_round").and_then(|v| v.as_i64()),
        Some(1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_start_rejects_building_proposal() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Building Test",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: Some("building"),
            body_format: None,
        })
        .await
        .unwrap();

    let resp = server
        .dispatch_tool(
            "proposal_refinement_start",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("does not support refinement"),
        "should reject building status: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_status_returns_inactive_when_not_started() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "No Refinement",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

    let resp = server
        .dispatch_tool(
            "proposal_refinement_status",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .expect("tool should be registered");

    let refinement = resp.get("refinement").expect("should have refinement");
    assert_eq!(
        refinement.get("active").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        refinement.get("current_round").and_then(|v| v.as_i64()),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_status_reflects_debate_trail_rounds() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Debate Round Test",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: Some("in_review"),
            body_format: None,
        })
        .await
        .unwrap();
    link_proposal_to_project(&db, &repo, &proposal.id).await;

    // Start refinement.
    server
        .dispatch_tool(
            "proposal_refinement_start",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();

    // Add a non-blocking objection in round 1.
    server
        .dispatch_tool(
            "proposal_debate_append",
            serde_json::json!({
                "proposal_id": proposal.id,
                "kind": "objection",
                "body": "Minor issue",
                "blocking": false,
                "agent_role": "adversary",
                "author_kind": "agent",
                "against_revision_seq": 1,
                "round": 1,
            }),
        )
        .await
        .unwrap();

    // Add a non-blocking objection in round 2.
    server
        .dispatch_tool(
            "proposal_debate_append",
            serde_json::json!({
                "proposal_id": proposal.id,
                "kind": "objection",
                "body": "Another minor issue",
                "blocking": false,
                "agent_role": "adversary",
                "author_kind": "agent",
                "against_revision_seq": 1,
                "round": 2,
            }),
        )
        .await
        .unwrap();

    let resp = server
        .dispatch_tool(
            "proposal_refinement_status",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();

    let refinement = resp.get("refinement").expect("should have refinement");
    assert_eq!(
        refinement.get("active").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        refinement.get("current_round").and_then(|v| v.as_i64()),
        Some(2)
    );
    assert_eq!(
        refinement.get("total_entries").and_then(|v| v.as_i64()),
        Some(2)
    );
    // Both rounds are non-blocking → 2 dry rounds.
    assert_eq!(
        refinement.get("dry_rounds").and_then(|v| v.as_i64()),
        Some(2)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_start_rejects_nonexistent_proposal() {
    let (server, _db) = test_server().await;

    let resp = server
        .dispatch_tool(
            "proposal_refinement_start",
            serde_json::json!({ "proposal_id": "nonexistent" }),
        )
        .await
        .expect("tool should be registered");

    let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
    assert!(
        error.contains("proposal not found"),
        "should mention proposal not found: {error}"
    );
}

/// A failed post-commit wake remains a successful durable admission and
/// never fabricates a lifecycle stop. Replaying the same request id returns
/// the existing run rather than creating a second generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_start_wake_failure_is_pending_and_same_request_is_idempotent() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let state = crate::state::McpState::new(
        db.clone(),
        EventBus::noop(),
        crate::state::stubs::test_mcp_state(db.clone())
            .catalog()
            .clone(),
        crate::state::stubs::test_mcp_state(db.clone())
            .health_tracker()
            .clone(),
        None,
        None,
        None,
        None,
        Arc::new(crate::state::stubs::StubLspOps),
        Arc::new(crate::state::stubs::StubRuntimeOps),
        Arc::new(crate::state::stubs::StubGitOps),
        Arc::new(crate::state::stubs::StubRepoGraphOps),
    );
    let server = DjinnMcpServer::new(state);
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Wake pending",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();
    link_proposal_to_project(&db, &repo, &proposal.id).await;
    // `proposals.refinement_owner_user_id` is FK-constrained to `users`, so
    // durable owner attribution can only be observed against a real row.
    let owner_user_id = create_test_user(&db, "durable-owner-019f").await;
    for _ in 0..2 {
        let response = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({
                    "proposal_id": proposal.id, "owner_user_id": owner_user_id,
                    "request_id": "start-request-019f"
                }),
            )
            .await
            .unwrap();
        assert_eq!(response["error"], "accepted; dispatch pending");
    }
    assert_eq!(
        repo.get(&proposal.id)
            .await
            .unwrap()
            .unwrap()
            .refinement_owner_user_id
            .as_deref(),
        Some(owner_user_id.as_str())
    );
    assert_eq!(
        repo.load_refinement_run_aggregates(&proposal.id, 60_000)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        repo.revisions(&proposal.id)
            .await
            .unwrap()
            .iter()
            .filter(|revision| revision.event_kind == "refinement_stop")
            .count(),
        0
    );
}

// ── Full refinement happy path ────────────────────────────────────────────

/// End-to-end happy path: start refinement → adversary blocking objection
/// (round 1) → adversary dry (round 2) → adversary dry (round 3) →
/// judge verdict → status reports stopped with adversary_dry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_happy_path_start_to_stop() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Happy Path Test",
            body: "## Problem\nThe problem is X.\n## Solution\nWe do Y.",
            acceptance_criteria: Some(r#"["AC1: done", "AC2: done"]"#),
            status: Some("in_review"),
            body_format: None,
        })
        .await
        .unwrap();
    link_proposal_to_project(&db, &repo, &proposal.id).await;

    // 1. Start refinement (checkpoint mode).
    let start_resp = server
        .dispatch_tool(
            "proposal_refinement_start",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();
    assert!(start_resp.get("error").and_then(|v| v.as_str()).is_none());
    let refinement = start_resp.get("refinement").unwrap();
    assert_eq!(
        refinement.get("active").and_then(|v| v.as_bool()),
        Some(true)
    );

    // 2. Round 1: adversary raises a blocking objection.
    server
        .dispatch_tool(
            "proposal_debate_append",
            serde_json::json!({
                "proposal_id": proposal.id,
                "kind": "objection",
                "body": "Missing risk assessment section",
                "blocking": true,
                "agent_role": "adversary",
                "author_kind": "agent",
                "author_model": "openai/gpt-4o",
                "against_revision_seq": 0,
                "round": 1,
            }),
        )
        .await
        .unwrap();

    // 3. Round 2: adversary finds no blocking objections (dry).
    server
        .dispatch_tool(
            "proposal_debate_append",
            serde_json::json!({
                "proposal_id": proposal.id,
                "kind": "objection",
                "body": "Minor formatting concern",
                "blocking": false,
                "agent_role": "adversary",
                "author_kind": "agent",
                "against_revision_seq": 1,
                "round": 2,
            }),
        )
        .await
        .unwrap();

    // 4. Round 3: adversary is dry again.
    // (No debate entries for round 3 = explicit dry.)

    // 5. Judge verdict.
    server
        .dispatch_tool(
            "proposal_debate_append",
            serde_json::json!({
                "proposal_id": proposal.id,
                "kind": "verdict",
                "body": "Proposal meets readiness criteria.",
                "blocking": false,
                "agent_role": "judge",
                "author_kind": "agent",
                "author_model": "anthropic/claude-sonnet-4-20250514",
                "against_revision_seq": 1,
                "round": 3,
            }),
        )
        .await
        .unwrap();

    // 6. The coordinator terminalizes the exact admitted run.
    terminalize_started_run(
        &repo,
        &proposal.id,
        djinn_core::refinement_liveness::RefinementStopReason::AdversaryDry,
    )
    .await;

    // 7. Verify refinement status shows stopped with adversary_dry.
    let status_resp = server
        .dispatch_tool(
            "proposal_refinement_status",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();
    let refinement = status_resp.get("refinement").unwrap();
    assert_terminal_refinement(refinement, "adversary_dry");
    assert_eq!(
        refinement.get("active").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        refinement.get("stop_reason").and_then(|v| v.as_str()),
        Some("adversary_dry")
    );
    // Round should reflect the max round in the debate trail.
    assert_eq!(
        refinement.get("current_round").and_then(|v| v.as_i64()),
        Some(3)
    );
    // Total debate entries: 1 objection + 1 non-blocking + 1 verdict = 3.
    assert_eq!(
        refinement.get("total_entries").and_then(|v| v.as_i64()),
        Some(3)
    );
    // Only round 2 has no blocking adversary objection (round 1 had one).
    // Dry rounds count consecutive from the end: round 3 had no adversary
    // objection at all (verdict only), round 2 had non-blocking → 2 dry.
    let dry = refinement
        .get("dry_rounds")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert!(dry >= 1, "should have at least 1 dry round, got {dry}");
}

// ── Stop reason: round_cap ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_status_shows_round_cap_stop_reason() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Round Cap Test",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();
    link_proposal_to_project(&db, &repo, &proposal.id).await;

    // Start refinement.
    server
        .dispatch_tool(
            "proposal_refinement_start",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();

    terminalize_started_run(
        &repo,
        &proposal.id,
        djinn_core::refinement_liveness::RefinementStopReason::RoundCap,
    )
    .await;

    let status_resp = server
        .dispatch_tool(
            "proposal_refinement_status",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();
    let refinement = status_resp.get("refinement").unwrap();
    assert_terminal_refinement(refinement, "round_cap");
    assert_eq!(
        refinement.get("active").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        refinement.get("stop_reason").and_then(|v| v.as_str()),
        Some("round_cap")
    );
}

// ── Stop reason: spawn_cap ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_status_shows_spawn_cap_stop_reason() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Spawn Cap Test",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();
    link_proposal_to_project(&db, &repo, &proposal.id).await;

    server
        .dispatch_tool(
            "proposal_refinement_start",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();

    terminalize_started_run(
        &repo,
        &proposal.id,
        djinn_core::refinement_liveness::RefinementStopReason::SpawnCap,
    )
    .await;

    let status_resp = server
        .dispatch_tool(
            "proposal_refinement_status",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();
    let refinement = status_resp.get("refinement").unwrap();
    assert_terminal_refinement(refinement, "spawn_cap");
    assert_eq!(
        refinement.get("stop_reason").and_then(|v| v.as_str()),
        Some("spawn_cap")
    );
}

// ── Stop reason: repeated_objection ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_status_shows_repeated_objection_stop_reason() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Repeated Objection Test",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();
    link_proposal_to_project(&db, &repo, &proposal.id).await;

    server
        .dispatch_tool(
            "proposal_refinement_start",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();

    terminalize_started_run(
        &repo,
        &proposal.id,
        djinn_core::refinement_liveness::RefinementStopReason::RepeatedObjection {
            signature: "same objection".into(),
            occurrences: 2,
        },
    )
    .await;

    let status_resp = server
        .dispatch_tool(
            "proposal_refinement_status",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();
    let refinement = status_resp.get("refinement").unwrap();
    assert_terminal_refinement(refinement, "repeated_objection");
    assert_eq!(
        refinement.get("stop_reason").and_then(|v| v.as_str()),
        Some("repeated_objection")
    );
}

// ── Stop reason: agent_failure ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_status_shows_agent_failure_stop_reason() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Agent Failure Test",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();
    link_proposal_to_project(&db, &repo, &proposal.id).await;

    server
        .dispatch_tool(
            "proposal_refinement_start",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();

    terminalize_started_run(
        &repo,
        &proposal.id,
        djinn_core::refinement_liveness::RefinementStopReason::AgentFailure {
            role: djinn_core::refinement_liveness::RefinementRole::Adversary,
            error_code: "test_failure".into(),
            message: "fixture terminal transition".into(),
        },
    )
    .await;

    let status_resp = server
        .dispatch_tool(
            "proposal_refinement_status",
            serde_json::json!({ "proposal_id": proposal.id }),
        )
        .await
        .unwrap();
    let refinement = status_resp.get("refinement").unwrap();
    assert_terminal_refinement(refinement, "agent_failure");
    assert_eq!(
        refinement.get("stop_reason").and_then(|v| v.as_str()),
        Some("agent_failure")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_only_stop_reason_remains_display_only() {
    let (_server, db) = test_server().await;
    let repo = ProposalRepository::new(db, EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Legacy stop display",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();
    repo.record_refinement_lifecycle(&proposal.id, "refinement_start", None)
        .await
        .unwrap();
    repo.record_refinement_lifecycle(
        &proposal.id,
        "refinement_stop",
        Some(&serde_json::json!({ "reason_tag": "round_cap" })),
    )
    .await
    .unwrap();

    let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert!(!status.active);
    assert!(status.run_id.is_none());
    assert!(status.liveness.is_none());
    assert_eq!(status.stop_reason.as_deref(), Some("round_cap"));
}
