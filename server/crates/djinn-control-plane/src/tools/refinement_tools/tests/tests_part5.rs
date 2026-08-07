use super::*;

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
pub(super) fn valid_demand_params(proposal_id: &str) -> serde_json::Value {
    serde_json::json!({
        "proposal_id": proposal_id,
        "round": 1,
        "against_revision_seq": 1,
        "question": "Does module X handle token expiry correctly?",
        "target_subsystem": "auth",
        "spec_unknown_anchor": "anchor-text",
        "insufficient_in_session_research": "No integration tests cover token expiry edge case",
        "expected_findings": "Evidence that token refresh is or is not required",
        "load_bearing_category": "feasibility"
    })
}

/// Create a user in the DB and return the user id.  Each call uses a
/// unique github_id to avoid upsert collisions across parallel tests.
pub(super) async fn create_test_user(db: &Database, login: &str) -> String {
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
pub(super) async fn link_proposal_to_project(
    db: &Database,
    repo: &ProposalRepository,
    proposal_id: &str,
) -> String {
    let project_id = uuid::Uuid::now_v7().to_string();
    // `create_with_id` rather than `create`: the `test-owner/test-repo-<id>`
    // coordinates are how callers address this project through `board_health`.
    djinn_db::ProjectRepository::new(db.clone(), EventBus::noop())
        .create_with_id(
            &project_id,
            &format!("test-project-{project_id}"),
            "test-owner",
            &format!("test-repo-{project_id}"),
        )
        .await
        .unwrap();

    repo.add_target(proposal_id, &project_id, "primary")
        .await
        .unwrap();

    project_id
}

/// Create an active Judge refinement task for the given proposal,
/// attributed to `user_id`.  Returns the task id.
pub(super) async fn create_judge_task(
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
                djinn_db::EffectiveCreatorProvenance {
                    explicit_user_id: Some(user_id),
                    source_task_id: None,
                    proposal_id: None,
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
async fn create_spike_task(db: &Database, project_id: &str, short_label: &str) -> String {
    let event_bus = djinn_core::events::EventBus::noop();
    let task_repo = djinn_db::TaskRepository::new(db.clone(), event_bus);
    let creator_key = uuid::Uuid::now_v7();
    let creator = djinn_db::UserRepository::new(db.clone())
        .upsert_from_github(
            creator_key.as_u128() as i64,
            &format!("refinement-spike-fixture-{creator_key}"),
            None,
            None,
        )
        .await
        .unwrap();
    task_repo
        .create_in_project_with_provenance(
            project_id,
            None,
            djinn_db::EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
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

/// Admit a real running refinement run and return `(run_id, generation)`.
///
/// `refinement.active` is decided by the exact admitted run; the legacy
/// `refinement_start` lifecycle row is display-only. Any test that must reach a
/// check downstream of "refinement active" needs this, not the lifecycle row.
pub(super) async fn admit_refinement_run(
    repo: &ProposalRepository,
    proposal_id: &str,
    label: &str,
) -> (String, i32) {
    let outcome = repo
        .reap_and_admit(djinn_db::AdmitRefinementRunRequest {
            proposal_id: proposal_id.to_owned(),
            idempotency_key: format!("{label}/{proposal_id}"),
            source: djinn_db::RefinementAdmissionSource::Demand {
                demand_id: format!("{label}/{proposal_id}"),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .unwrap();
    match outcome {
        djinn_db::RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        } => (run_id, generation),
        other => panic!("expected admitted refinement run, got {other:?}"),
    }
}

/// Set up a proposal with body containing "anchor-text", start
/// refinement, create a user, project, and Judge task, and return
/// (server, db, proposal, user_id, judge_task_id).
pub(super) async fn setup_demand_test() -> (
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
            // These are deliberately only prose/UI question inventory. The
            // demand path must never synthesize typed evidence from them.
            body: "This spec contains anchor-text for validation.\n\n## Open questions\nIs token expiry safe?\n\n<QuestionForm id=\"questions\">\nDoes the repository need refresh tokens?\n</QuestionForm>",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

    // Keep the legacy lifecycle row for display compatibility, and admit
    // the exact run that is now the sole activity authority for status.
    repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
        .await
        .unwrap();
    let outcome = repo
        .reap_and_admit(djinn_db::AdmitRefinementRunRequest {
            proposal_id: p.id.clone(),
            idempotency_key: format!("demand-validation/{}", p.id),
            source: djinn_db::RefinementAdmissionSource::Demand {
                demand_id: format!("demand-validation/{}", p.id),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .unwrap();
    let (run_id, generation) = match outcome {
        djinn_db::RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        } => (run_id, generation),
        other => panic!("expected admitted refinement run, got {other:?}"),
    };

    // Create user, project, and Judge task for authorization.
    let user_id = create_test_user(&db, "judge-user").await;
    let project_id = link_proposal_to_project(&db, &repo, &p.id).await;
    let judge_task_id = create_judge_task(&db, &project_id, &p.id, &user_id).await;
    // Titles are display-only; materialize the exact Judge authority tuple.
    djinn_db::test_support::materialize_judge_authority_for_test(
        &db,
        &judge_task_id,
        &run_id,
        i64::from(generation),
    )
    .await;

    let updated = repo.get(&p.id).await.unwrap().unwrap();
    // The setup body intentionally contains prose, an Open questions heading,
    // and a QuestionForm. They are UI text only: setup must not synthesize
    // typed evidence or its legacy active-demand projection.
    let finding_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM typed_evidence_findings WHERE proposal_id = $1")
            .bind(&p.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let attempt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM typed_evidence_attempts WHERE finding_id IN \
         (SELECT id FROM typed_evidence_findings WHERE proposal_id = $1)",
    )
    .bind(&p.id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        finding_count, 0,
        "prose and QuestionForm must not create findings"
    );
    assert_eq!(
        attempt_count, 0,
        "prose and QuestionForm must not create attempts"
    );
    assert!(
        updated.linked_spike_task_id.is_none() && updated.needs_evidence_claim.is_none(),
        "prose and QuestionForm must not create legacy evidence state"
    );
    (server, db, updated, user_id, judge_task_id)
}

/// Snapshot the mutation-sensitive fields so we can assert no change
/// after a rejected demand.
pub(super) async fn mutation_snapshot(
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
    let project_id = repo.targets(&p.id).await.unwrap()[0].project_id.clone();
    let snap = atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await;

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

    assert_eq!(
        snap,
        atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await,
        "unauthenticated demand must not mutate any demand-owned relation"
    );
}

// ── AC: Non-Judge caller rejected (wrong user) ──────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_user_rejected() {
    let (server, db, p, _user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let project_id = repo.targets(&p.id).await.unwrap()[0].project_id.clone();
    let snap = atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await;

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
        error.contains("not the active Adversary or Judge"),
        "should mention evidence-authority failure: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    assert_eq!(
        snap,
        atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await,
        "unauthorized demand must not mutate any demand-owned relation"
    );
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

    // Admit a REAL running refinement run, but create no Judge or Adversary
    // task. `refinement.active` is decided by the run, not by the legacy
    // lifecycle row, so the lifecycle row alone would leave the refinement
    // inactive and this test would silently duplicate
    // `inactive_refinement_rejected` instead of exercising its own AC.
    //
    // A live run with no materialized authority task is the one state in
    // which "no authority task in flight" is the true and only reason.
    repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
        .await
        .unwrap();
    let outcome = repo
        .reap_and_admit(djinn_db::AdmitRefinementRunRequest {
            proposal_id: p.id.clone(),
            idempotency_key: format!("no-judge-task/{}", p.id),
            source: djinn_db::RefinementAdmissionSource::Demand {
                demand_id: format!("no-judge-task/{}", p.id),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            djinn_db::RefinementAdmissionOutcome::Admitted { .. }
        ),
        "test needs a live refinement run so the rejection is authority-absence, \
         not refinement-absence: {outcome:?}"
    );

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
        error.contains("no active Adversary or Judge task in flight"),
        "should mention no authority task in flight: {error}"
    );
    // This AC is authority-absence with a LIVE run. It must never collapse
    // into the refinement-absence rejection owned by
    // `inactive_refinement_rejected`, or a regression that merges the two
    // paths would pass both tests.
    assert!(
        !error.contains("refinement is not active"),
        "authority-absence must stay distinct from refinement-absence: {error}"
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
    // No refinement run is admitted here, so the authority lookup would also
    // come back empty. Terminal status is the more specific cause and must
    // win; it must not be reported as missing authority.
    assert!(
        !error.contains("no active Adversary or Judge task in flight"),
        "a terminal proposal must not masquerade as authority-absence: {error}"
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
        error.contains("refinement is not active"),
        "should mention inactive refinement: {error}"
    );
    // A Judge task exists here (see above) but no refinement run does, so the
    // authority lookup would also come back empty. The rejection must name
    // the real cause — no run — and not borrow the authority-absence reason
    // owned by `no_judge_task_in_flight_rejected`.
    assert!(
        !error.contains("no active Adversary or Judge task in flight"),
        "refinement-absence must stay distinct from authority-absence: {error}"
    );
}

// ── AC: Round mismatch rejected ──────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_mismatch_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let project_id = repo.targets(&p.id).await.unwrap()[0].project_id.clone();
    let snap = atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await;

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
        error.contains("does not match the current refinement round"),
        "should mention round mismatch: {error}"
    );
    // `round` is an input to the authority correlation, so a wrong round must
    // report itself as a round mismatch and never as missing authority.
    assert!(
        !error.contains("no active Adversary or Judge task in flight"),
        "a wrong round must not masquerade as authority-absence: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    assert_eq!(
        snap,
        atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await,
        "round mismatch must not mutate any demand-owned relation"
    );
}

// ── AC: future against_revision_seq rejected ─────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn against_revision_seq_exceeds_latest_rejected() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let project_id = repo.targets(&p.id).await.unwrap()[0].project_id.clone();
    let snap = atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await;

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
    // A demand is authority for the EXACT active revision, so a seq beyond
    // `latest_revision_seq` is rejected as a mismatch rather than as an
    // "exceeds" bound — the check is `!=`, not `>`.
    assert!(
        error.contains("does not match the proposal's active revision seq"),
        "should mention revision seq mismatch: {error}"
    );
    // `against_revision_seq` is an input to the authority correlation, so a
    // stale or future seq must report itself and never as missing authority.
    assert!(
        !error.contains("no active Adversary or Judge task in flight"),
        "a stale revision seq must not masquerade as authority-absence: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );

    assert_eq!(
        snap,
        atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await,
        "revision mismatch must not mutate any demand-owned relation"
    );
}

// ── AC: stale against_revision_seq rejected ──────────────────────

/// Advance the persisted proposal head before sending a demand bound to the
/// previous revision. This is deliberately distinct from the future-sequence
/// case above: an older, once-current revision must also fail before the
/// composed demand transaction creates any task or evidence state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_against_revision_seq_rejected_without_demand_mutation() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());

    let advanced = repo
        .update(
            &p.id,
            djinn_db::ProposalUpdateInput {
                title: "Demand Validation Test",
                body: "This spec contains anchor-text for validation after revision advance.",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();
    assert!(
        advanced.latest_revision_seq > p.latest_revision_seq,
        "fixture must advance the active revision: {} > {}",
        advanced.latest_revision_seq,
        p.latest_revision_seq
    );
    let project_id = repo.targets(&p.id).await.unwrap()[0].project_id.clone();
    let before = atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await;

    let mut params = valid_demand_params(&p.id);
    params["against_revision_seq"] = serde_json::json!(p.latest_revision_seq);

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
        error.contains("does not match the proposal's active revision seq"),
        "stale revision must be rejected as an exact revision mismatch: {error}"
    );
    assert!(
        !resp
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    );
    assert_eq!(
        before,
        atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await,
        "stale revision demand must not mutate task, typed, legacy, debate, or lifecycle state"
    );
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
    let project_id = repo.targets(&p.id).await.unwrap()[0].project_id.clone();

    for (pattern, rejection) in [
        ("Please investigate further if this is correct?", "generic"),
        ("Can we improve the token handling?", "generic"),
        ("Should we design more tests for this?", "generic"),
        ("Is blue better than green?", "preference-only"),
        (
            "Which function currently parses this header?",
            "repository-answerable",
        ),
    ] {
        let snap = atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await;
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
            error.contains(rejection),
            "pattern '{pattern}' should be rejected as {rejection}: {error}",
        );
        assert!(
            !resp
                .get("accepted")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
        );
        assert_eq!(
            snap,
            atomic_demand_snapshot(&db, &repo, &p.id, &project_id).await,
            "rejected question '{pattern}' must not mutate any demand-owned relation"
        );
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

// ── AC: Non-empty spec_unknown_anchor is a caller assertion ──────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_empty_spec_unknown_anchor_absent_from_body_accepted() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let project_id = repo.targets(&p.id).await.unwrap()[0].project_id.clone();
    let before =
        djinn_db::test_support::atomic_evidence_demand_counts_for_test(&db, &p.id, &project_id)
            .await;

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

    assert_eq!(resp.get("accepted").and_then(|v| v.as_bool()), Some(true));
    assert!(
        resp.get("error").is_none(),
        "caller-asserted anchor should be accepted: {resp}"
    );
    let result = resp.get("result").expect("accepted response has result");
    assert!(
        result
            .get("spike_task_id")
            .and_then(|v| v.as_str())
            .is_some(),
        "accepted response has spike task: {resp}"
    );

    let after =
        djinn_db::test_support::atomic_evidence_demand_counts_for_test(&db, &p.id, &project_id)
            .await;
    assert_eq!(after.tasks, before.tasks + 1, "exactly one spike task");
    assert_eq!(after.findings, before.findings + 1, "exactly one finding");
    assert_eq!(after.attempts, before.attempts + 1, "exactly one attempt");
    assert_eq!(after.debates, before.debates + 1, "exactly one debate row");
    assert_eq!(
        after.lifecycle_events,
        before.lifecycle_events + 1,
        "exactly one awaiting-evidence lifecycle event"
    );
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
    let spike_id = create_spike_task(&db, &project_id, "existing").await;
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
        error.contains("active_evidence_conflict"),
        "should return the stable active-demand conflict: {error}"
    );
    assert_eq!(
        resp.get("conflict_code").and_then(|v| v.as_str()),
        Some("active_evidence_conflict")
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

/// A second demand issued while a spike is already open must never allocate a
/// second one, whichever way it duplicates:
///
/// * an **identical** re-delivery is a replay of the same allocation — the
///   normalized demand hash matches the open finding, so the caller gets the
///   existing spike back and no demand-owned relation grows;
/// * a **different** claim is an unresolved-evidence conflict — it is rejected
///   with the stable `active_evidence_conflict` code and writes nothing.
///
/// Both branches are asserted here because only counting rows distinguishes a
/// replay from a silent second allocation; an `accepted: true` on its own would
/// not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_demand_after_accepted_creates_no_second_spike() {
    let (server, db, p, user_id, _judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let project_id = repo.targets(&p.id).await.unwrap()[0].project_id.clone();

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
    let result1 = resp1.get("result").expect("accepted response has result");
    let first_spike = result1
        .get("spike_task_id")
        .and_then(|v| v.as_str())
        .expect("first demand should have spike_task_id")
        .to_string();
    let first_finding = result1
        .get("finding_id")
        .and_then(|v| v.as_str())
        .expect("first demand should have finding_id")
        .to_string();
    let first_attempt = result1
        .get("attempt_id")
        .and_then(|v| v.as_str())
        .expect("first demand should have attempt_id")
        .to_string();
    assert_eq!(
        result1.get("replayed").and_then(|v| v.as_bool()),
        Some(false),
        "the first demand allocates rather than replays: {resp1}"
    );

    let after_first =
        djinn_db::test_support::atomic_evidence_demand_counts_for_test(&db, &p.id, &project_id)
            .await;

    // Second, byte-identical demand — a normalized replay of the same claim.
    let resp2 = djinn_core::auth_context::SESSION_USER_ID
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

    let result2 = resp2
        .get("result")
        .expect("replayed demand returns the existing allocation");
    assert_eq!(
        result2.get("spike_task_id").and_then(|v| v.as_str()),
        Some(first_spike.as_str()),
        "a replay must return the FIRST spike, never a new one: {resp2}"
    );
    assert_eq!(
        result2.get("finding_id").and_then(|v| v.as_str()),
        Some(first_finding.as_str()),
        "a replay must return the original typed finding: {resp2}"
    );
    assert_eq!(
        result2.get("attempt_id").and_then(|v| v.as_str()),
        Some(first_attempt.as_str()),
        "a replay must return the original typed attempt: {resp2}"
    );
    assert_eq!(
        result2.get("replayed").and_then(|v| v.as_bool()),
        Some(true),
        "an identical re-delivery must report itself as a replay: {resp2}"
    );
    assert_eq!(
        djinn_db::test_support::atomic_evidence_demand_counts_for_test(&db, &p.id, &project_id)
            .await,
        after_first,
        "a replay must not write any demand-owned relation"
    );

    // Third demand, a DIFFERENT claim while the first spike is still open.
    // Its normalized hash does not match the open finding, so it is an
    // unresolved-evidence conflict rather than a replay.
    let mut divergent = valid_demand_params(&p.id);
    divergent["question"] = serde_json::json!("Does module Y reject expired refresh tokens?");
    let resp3 = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            server
                .dispatch_tool("proposal_refinement_demand_evidence", divergent)
                .await
        })
        .await
        .expect("tool should be registered");

    assert!(
        !resp3
            .get("accepted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        "a divergent demand against an open spike must be rejected: {resp3}"
    );
    assert_eq!(
        resp3.get("conflict_code").and_then(|v| v.as_str()),
        Some("active_evidence_conflict"),
        "should carry the stable active-demand conflict code: {resp3}"
    );
    assert_eq!(
        djinn_db::test_support::atomic_evidence_demand_counts_for_test(&db, &p.id, &project_id)
            .await,
        after_first,
        "a conflicting demand must not write any demand-owned relation"
    );

    // Exactly one spike remains linked, and it is the first one.
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

// ── Atomic authority race fence ───────────────────────────────────

/// Snapshot every relation owned by the atomic-demand boundary. The authority
/// task itself is deliberately excluded because the test changes it after
/// preflight to model a handoff race.
async fn atomic_demand_snapshot(
    db: &Database,
    repo: &ProposalRepository,
    proposal_id: &str,
    project_id: &str,
) -> (
    djinn_db::test_support::AtomicEvidenceDemandCountsForTest,
    Option<String>,
    Option<String>,
) {
    let counts =
        djinn_db::test_support::atomic_evidence_demand_counts_for_test(db, proposal_id, project_id)
            .await;
    let proposal = repo.get(proposal_id).await.unwrap().unwrap();
    (
        counts,
        proposal.linked_spike_task_id,
        proposal.needs_evidence_claim,
    )
}

fn atomic_demand_claim(authority_task_id: &str) -> djinn_core::models::NeedsEvidenceClaim {
    djinn_core::models::NeedsEvidenceClaim {
        question: "Does module X handle token expiry correctly?".to_owned(),
        target_subsystem: "auth".to_owned(),
        spec_unknown_anchor: "anchor-text".to_owned(),
        insufficient_in_session_research: "No integration tests cover token expiry edge case"
            .to_owned(),
        expected_findings: "Evidence that token refresh is or is not required".to_owned(),
        round: 1,
        against_revision_seq: 1,
        created_by_task_id: authority_task_id.to_owned(),
    }
}

/// Preflight can pass and then lose its authority before the repository
/// transaction begins. The repository must reject before allocating any of its
/// task, typed, legacy, debate, or lifecycle rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_demand_rejects_authority_handoff_after_preflight_without_writes() {
    let (_server, db, proposal, user_id, judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let project_id = repo.targets(&proposal.id).await.unwrap()[0]
        .project_id
        .clone();

    let preflight = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id), async {
            crate::tools::refinement_helpers::verify_active_judge_authorization(
                &repo,
                &proposal.id,
                1,
                1,
            )
            .await
        })
        .await;
    assert_eq!(
        preflight.unwrap(),
        judge_task_id,
        "fixture must pass preflight"
    );

    // Model a role handoff between handler validation and transaction entry.
    djinn_db::TaskRepository::new(db.clone(), EventBus::noop())
        .set_status(&judge_task_id, "closed")
        .await
        .unwrap();
    let before = atomic_demand_snapshot(&db, &repo, &proposal.id, &project_id).await;
    let claim = atomic_demand_claim(&judge_task_id);
    let labels = serde_json::json!(["refinement-evidence", "read-only"]);

    let error = repo
        .demand_evidence_atomically(djinn_db::AtomicEvidenceDemandInput {
            proposal_id: &proposal.id,
            project_id: &project_id,
            claim: &claim,
            title: "Evidence spike: token expiry",
            description: "Read-only evidence investigation",
            labels: &labels,
            load_bearing_category: "feasibility",
        })
        .await
        .expect_err("handoff after preflight must be rejected");
    assert!(
        error
            .to_string()
            .contains("stale_evidence_demand_authority"),
        "must return stable stale rejection: {error}"
    );
    assert_eq!(
        before,
        atomic_demand_snapshot(&db, &repo, &proposal.id, &project_id).await,
        "stale demand must not write any demand-owned relation"
    );
}

/// A normalized duplicate still replays after the authority recheck rather
/// than allocating another task, finding, attempt, legacy projection, debate,
/// or lifecycle row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_demand_normalized_replay_survives_authority_fence() {
    let (_server, db, proposal, _user_id, judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let project_id = repo.targets(&proposal.id).await.unwrap()[0]
        .project_id
        .clone();
    let claim = atomic_demand_claim(&judge_task_id);
    let labels = serde_json::json!(["refinement-evidence", "read-only"]);

    let first = repo
        .demand_evidence_atomically(djinn_db::AtomicEvidenceDemandInput {
            proposal_id: &proposal.id,
            project_id: &project_id,
            claim: &claim,
            title: "Evidence spike: token expiry",
            description: "Read-only evidence investigation",
            labels: &labels,
            load_bearing_category: "feasibility",
        })
        .await
        .unwrap();
    assert!(!first.replayed);
    let before_replay = atomic_demand_snapshot(&db, &repo, &proposal.id, &project_id).await;

    let replay = repo
        .demand_evidence_atomically(djinn_db::AtomicEvidenceDemandInput {
            proposal_id: &proposal.id,
            project_id: &project_id,
            claim: &claim,
            title: "Evidence spike: token expiry",
            description: "Read-only evidence investigation",
            labels: &labels,
            load_bearing_category: "feasibility",
        })
        .await
        .unwrap();
    assert!(
        replay.replayed,
        "identical demand must use normalized replay"
    );
    assert_eq!(replay.finding_id, first.finding_id);
    assert_eq!(replay.attempt_id, first.attempt_id);
    assert_eq!(replay.spike_task_id, first.spike_task_id);
    assert_eq!(
        before_replay,
        atomic_demand_snapshot(&db, &repo, &proposal.id, &project_id).await,
        "replay must not allocate duplicate demand-owned rows"
    );
}

/// The accepted categories and authority roles are exercised as data, so a new
/// category cannot accidentally bypass the same atomic persistence boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demand_matrix_persists_every_load_bearing_category_for_each_authority_role() {
    const CATEGORIES: &[&str] = &[
        "feasibility",
        "safety",
        "integrity",
        "compatibility",
        "rollout",
        "core_acceptance_criteria",
    ];

    for role in ["judge", "adversary"] {
        for category in CATEGORIES {
            let (server, db, proposal, user_id, authority_task_id) = setup_demand_test().await;
            let repo = ProposalRepository::new(db.clone(), EventBus::noop());
            let project_id = repo.targets(&proposal.id).await.unwrap()[0]
                .project_id
                .clone();

            // The fixture materializes Judge authority. Switch every persisted
            // role column as one correlated tuple for the Adversary case.
            if role == "adversary" {
                sqlx::query(
                    "UPDATE refinement_dispatch_intents SET role = 'adversary' WHERE task_id = $1",
                )
                .bind(&authority_task_id)
                .execute(db.pool())
                .await
                .unwrap();
                sqlx::query(
                    "UPDATE tasks SET agent_type = 'adversary', refinement_role = 'adversary' WHERE id = $1",
                )
                .bind(&authority_task_id)
                .execute(db.pool())
                .await
                .unwrap();
            }

            let before = atomic_demand_snapshot(&db, &repo, &proposal.id, &project_id).await;
            let mut params = valid_demand_params(&proposal.id);
            params["load_bearing_category"] = serde_json::json!(category);
            params["question"] = serde_json::json!(format!(
                "Does {category} evidence prove the token-expiry boundary?"
            ));
            let response = djinn_core::auth_context::SESSION_USER_ID
                .scope(Some(user_id), async {
                    server
                        .dispatch_tool("proposal_refinement_demand_evidence", params)
                        .await
                })
                .await
                .unwrap();
            assert_eq!(
                response.get("accepted").and_then(|v| v.as_bool()),
                Some(true),
                "{role}/{category} must be accepted: {response}"
            );
            let spike_task_id = response["result"]["spike_task_id"]
                .as_str()
                .expect("accepted matrix response must identify its spike task");
            let spike = djinn_db::TaskRepository::new(db.clone(), EventBus::noop())
                .get(spike_task_id)
                .await
                .unwrap()
                .expect("accepted matrix spike task must persist");
            let labels: Vec<String> = serde_json::from_str(&spike.labels).unwrap();
            assert_eq!(
                spike.agent_type.as_deref(),
                Some("architect"),
                "{role}/{category}"
            );
            assert_eq!(spike.issue_type, "spike", "{role}/{category}");
            assert!(
                labels.contains(&"refinement-evidence".to_owned()),
                "{role}/{category}"
            );
            assert!(
                labels.contains(&"read-only".to_owned()),
                "{role}/{category}"
            );
            assert!(
                labels.contains(&format!("proposal:{}", proposal.short_id)),
                "{role}/{category} must retain its proposal label"
            );
            let after = atomic_demand_snapshot(&db, &repo, &proposal.id, &project_id).await;
            assert_eq!(after.0.tasks, before.0.tasks + 1);
            assert_eq!(after.0.findings, before.0.findings + 1);
            assert_eq!(after.0.attempts, before.0.attempts + 1);
            assert_eq!(after.0.debates, before.0.debates + 1);
            assert_eq!(after.0.lifecycle_events, before.0.lifecycle_events + 1);
            assert!(
                after.1.is_some() && after.2.is_some(),
                "legacy link/claim persist"
            );
            let debate = repo.debate_trail(&proposal.id).await.unwrap();
            assert_eq!(debate.last().unwrap().agent_role, role);
        }
    }
}

/// A database fault at the debate boundary happens after task, typed, and
/// legacy rows have been staged. The composed transaction must roll all of
/// them back rather than leaving a partial evidence demand behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_demand_rolls_back_every_owned_relation_after_late_debate_failure() {
    let (_server, db, proposal, _user_id, judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let project_id = repo.targets(&proposal.id).await.unwrap()[0]
        .project_id
        .clone();
    let before = atomic_demand_snapshot(&db, &repo, &proposal.id, &project_id).await;
    sqlx::query("CREATE FUNCTION reject_atomic_demand_debate_for_test() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected atomic demand debate failure'; END; $$ LANGUAGE plpgsql")
        .execute(db.pool()).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_atomic_demand_debate_for_test BEFORE INSERT ON proposal_debate_trail FOR EACH ROW EXECUTE FUNCTION reject_atomic_demand_debate_for_test()")
        .execute(db.pool()).await.unwrap();

    let claim = atomic_demand_claim(&judge_task_id);
    let labels = serde_json::json!(["refinement-evidence", "read-only"]);
    let error = repo
        .demand_evidence_atomically(djinn_db::AtomicEvidenceDemandInput {
            proposal_id: &proposal.id,
            project_id: &project_id,
            claim: &claim,
            title: "Evidence spike: token expiry",
            description: "Read-only evidence investigation",
            labels: &labels,
            load_bearing_category: "feasibility",
        })
        .await
        .expect_err("late debate fault must abort the atomic demand");
    assert!(
        error
            .to_string()
            .contains("injected atomic demand debate failure")
    );
    assert_eq!(
        before,
        atomic_demand_snapshot(&db, &repo, &proposal.id, &project_id).await,
        "late failure must roll back task, typed, legacy, debate, and lifecycle state"
    );
}
