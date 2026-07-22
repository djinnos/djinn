// Regression tests for the "Current DoR status" string injected into
// tribunal refinement task descriptions.
//
// Split out of `refinement_cap_tests.rs` to keep that file under the
// size-guard byte threshold; shares its fixture helpers.

use super::refinement_cap_tests::{
    build_refinement_actor, seed_refinement_fixture, spawn_test_pool,
};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_db::{
    DoctorFindingRepository, NewDoctorFinding, ProposalDebateTrailCreateInput, ProposalRepository,
    ProposalUpdateInput, TaskRepository,
};

/// Regression (proposal 019f0c32): structured acceptance criteria added
/// mid-refinement must be reflected in the DoR readiness evaluation, and
/// therefore in the "Current DoR status" string injected into the next
/// tribunal task.
///
/// The advocate added nine structured `{ "criterion", "met" }` ACs in a
/// revision, but the DoR path parsed the stored JSON with
/// `parse_json_array` (which deserializes to `Vec<String>` and fails
/// wholesale on structured objects), so the injected status kept reporting
/// "At least one acceptance criterion is required". The Judge treats any
/// non-clean injected DoR status as a blocking readiness failure, so it
/// filed reject verdicts for rounds 1–3 against a proposal that already had
/// nine visible ACs. This test drives the real injection path
/// (`evaluate_proposal_readiness` → `create_refinement_task_with_context`)
/// and asserts the missing-AC failure does not survive structured ACs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_acs_added_mid_refinement_clear_missing_ac_dor_status() {
    use djinn_control_plane::tools::proposal_readiness::ReadinessCheck;

    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let actor = build_refinement_actor(&db, &events_tx, pool.clone());

    // Baseline: the fixture proposal is created with empty ACs (`"[]"`), so
    // the DoR evaluator must report the missing-AC failure.
    let baseline = actor
        .evaluate_proposal_readiness(&fixture.proposal_id)
        .await
        .expect("baseline readiness");
    assert!(
        baseline
            .failures
            .iter()
            .any(|f| f.check == ReadinessCheck::AcceptanceCriteriaCount),
        "empty ACs must trip the missing-AC DoR check (baseline sanity)"
    );

    // The advocate adds nine *structured* acceptance criteria mid-refinement,
    // exactly as recorded on the real proposal.
    let structured_acs = serde_json::json!([
        {"criterion": "Injected DoR status is recomputed against the live head", "met": false},
        {"criterion": "Structured ACs are counted by the readiness evaluator", "met": false},
        {"criterion": "Judge no longer rejects on a stale missing-AC status", "met": false},
        {"criterion": "proposal_show and DoR readiness agree on AC count", "met": false},
        {"criterion": "Regression test reproduces the round 1-3 failure", "met": false},
        {"criterion": "parse_json_array is not used on structured ACs", "met": false},
        {"criterion": "Shared tolerant parser is reused across read paths", "met": false},
        {"criterion": "sqlx offline data regenerated if SQL changed", "met": false},
        {"criterion": "clippy and nextest pass on touched crates", "met": false},
    ])
    .to_string();
    ProposalRepository::new(db.clone(), EventBus::noop())
        .set_acceptance_criteria(&fixture.proposal_id, &structured_acs)
        .await
        .expect("attach structured ACs mid-refinement");

    // The readiness evaluator must now see the nine ACs and NOT report the
    // missing-AC failure.
    let refreshed = actor
        .evaluate_proposal_readiness(&fixture.proposal_id)
        .await
        .expect("refreshed readiness");
    assert!(
        !refreshed
            .failures
            .iter()
            .any(|f| f.check == ReadinessCheck::AcceptanceCriteriaCount),
        "structured ACs added mid-refinement must clear the missing-AC DoR check; \
         failures were: {:?}",
        refreshed.failures
    );

    // Drive the real injection path used at dispatch. The coordinator must use
    // the shared latest-head result, including the repository-backed complete
    // lint summary, rather than rebuilding a body-only DoR result.
    assert!(
        refreshed.latest_lint.is_some(),
        "shared latest-head readiness must resolve the current revision lint"
    );
    let readiness_context =
        super::super::actor::CoordinatorActor::format_readiness_context(&refreshed);
    let task_id = actor
        .create_refinement_task_with_context(
            &fixture.proposal_id,
            "judge",
            2,
            2,
            &readiness_context,
            None,
            Some(&fixture.user_id),
        )
        .await
        .expect("create judge refinement task");

    let task = TaskRepository::new(db.clone(), EventBus::noop())
        .get(&task_id)
        .await
        .expect("read task")
        .expect("task exists");
    assert!(
        !task
            .description
            .contains("At least one acceptance criterion is required"),
        "injected DoR status must not claim ACs are missing once structured ACs exist:\n{}",
        task.description
    );
    assert!(
        task.description
            .contains("Latest SpecLintResultV1 summary (errors and warnings):"),
        "judge/refinement context must include the complete latest lint summary:\n{}",
        task.description
    );
}

/// Regression: refinement tribunal tasks must be attributed to the resolved
/// user in the legacy `owner` column (their GitHub login), not the hardcoded
/// "system" placeholder. The coordinator already fail-closed-validates the
/// attributed user before dispatch, so a real login is always available —
/// leaving `owner: "system"` made the Kanban board render tribunal tasks as
/// unassigned and dropped them from owner-based filters (`task_ready owner=…`).
/// `created_by_user_id` (the authoritative ownership field) is unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_task_owner_is_attributed_user_login_not_system() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let task_id = actor
        .create_refinement_task_with_context(
            &fixture.proposal_id,
            "adversary",
            1,
            1,
            "Proposal currently meets all DoR checks.",
            None,
            Some(&fixture.user_id),
        )
        .await
        .expect("create adversary refinement task");

    let task = TaskRepository::new(db.clone(), EventBus::noop())
        .get(&task_id)
        .await
        .expect("read task")
        .expect("task exists");

    // `owner` (legacy display/filter column) is now the user's GitHub login.
    assert_eq!(
        task.owner, "refinement-cap-user",
        "refinement task owner must be the attributed user's login, not \"system\""
    );
    assert_ne!(
        task.owner, "system",
        "refinement task owner must not fall back to the \"system\" placeholder \
         when a real attributed user is in scope"
    );
    // Authoritative ownership field is still stamped with the user id.
    assert_eq!(
        task.created_by_user_id.as_deref(),
        Some(fixture.user_id.as_str()),
        "created_by_user_id must remain the authoritative ownership field"
    );
}

/// Mandatory creator provenance fails closed when the attributed user cannot
/// be resolved. No tribunal task may be inserted with a fabricated `system`
/// owner or an invalid `created_by_user_id`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_task_creation_fails_closed_when_user_unresolvable() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let tasks_before = task_repo
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks before failed creation")
        .len();
    let missing_user_id = "00000000-0000-0000-0000-000000000000";
    let task_id = actor
        .create_refinement_task_with_context(
            &fixture.proposal_id,
            "judge",
            1,
            1,
            "Proposal currently meets all DoR checks.",
            None,
            Some(missing_user_id),
        )
        .await;

    assert!(
        task_id.is_none(),
        "unresolvable mandatory creator provenance must fail closed"
    );
    let tasks_after = task_repo
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks after failed creation")
        .len();
    assert_eq!(
        tasks_after, tasks_before,
        "unresolvable ownership must not insert a tribunal task"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Criterion-4 regression tests: coordinator tribunal readiness and verdict
// gating consume the shared latest-head lint-aware result via
// `lint_for_revision`, not doctor findings or an independent integrity check.
// ─────────────────────────────────────────────────────────────────────────

/// A body that passes all deterministic DoR heuristics (problem, scope,
/// objectives, grounding, dependencies, open-questions) so the only DoR
/// failures are spec-integrity errors from the linter.
const READY_BODY: &str = r#"
# Problem
Users cannot do X.
# Scope
In scope: Y. Out of scope: Z.
# Objectives
Deliver A.
# Dependencies
None.
# Open Questions
What if D fails?
Entry points: src/main.rs.
"#;

/// A body that passes all DoR heuristics AND has spec-integrity errors
/// (duplicate MDX block IDs). Requires `mdx` body format so the linter
/// parses the block tags and reports `DUPLICATE_BLOCK_ID`.
const CORRUPT_MDX_BODY: &str = "\
# Problem\n\
Users cannot do X.\n\
# Scope\n\
In scope: Y. Out of scope: Z.\n\
# Objectives\n\
Deliver A.\n\
# Dependencies\n\
None.\n\
# Open Questions\n\
What if D fails?\n\
Entry points: src/main.rs.\n\
<Callout id=\"dup\">one</Callout>\n\
<Callout id=\"dup\">two</Callout>";

/// Create a ready proposal (all DoR sections + ACs + target) directly in the
/// test DB, returning its id.
async fn seed_ready_proposal(db: &djinn_db::Database) -> (String, String) {
    let project = crate::test_helpers::create_test_project(db).await;
    let user_id = djinn_core::auth_context::SESSION_USER_ID
        .scope(None, async {
            djinn_db::UserRepository::new(db.clone())
                .upsert_from_github(999_001, "dor-status-user", None, None)
                .await
                .expect("create test user")
                .id
        })
        .await;
    let proposal = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            ProposalRepository::new(db.clone(), EventBus::noop())
                .create(djinn_db::ProposalCreateInput {
                    title: "DoR lint gate test",
                    body: READY_BODY,
                    acceptance_criteria: Some(r#"["Result is testable"]"#),
                    status: Some("building"),
                    body_format: None,
                })
                .await
                .expect("create ready proposal")
        })
        .await;
    ProposalRepository::new(db.clone(), EventBus::noop())
        .add_target(&proposal.id, &project.id, "primary")
        .await
        .expect("add proposal target");
    ProposalRepository::new(db.clone(), EventBus::noop())
        .start_refinement_with_owner(&proposal.id, Some(&user_id))
        .await
        .expect("start refinement");
    (proposal.id, project.id)
}

/// Append a judge verdict debate-trail entry at `round`.
async fn add_judge_verdict(
    db: &djinn_db::Database,
    proposal_id: &str,
    round: i32,
    blocking: bool,
    against_revision_seq: i32,
) {
    ProposalRepository::new(db.clone(), EventBus::noop())
        .add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id,
            kind: "verdict",
            body: if blocking { "needs work" } else { "ready" },
            blocking,
            agent_role: "judge",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq,
            round,
            body_metadata: None,
        })
        .await
        .expect("append judge verdict");
}

/// Regression (criterion 4): a reachable legacy corrupt head with no persisted
/// lint row is synchronously recomputed through `lint_for_revision` cache
/// repair by `evaluate_proposal_readiness`. The shared result must include the
/// exact `DUPLICATE_BLOCK_ID` byte-range failure and leave `ready=false`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_legacy_head_recomputed_and_blocks_readiness() {
    use djinn_control_plane::tools::proposal_readiness::ReadinessCheck;

    let db = crate::test_helpers::create_test_db();
    let (proposal_id, _) = seed_ready_proposal(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let actor = build_refinement_actor(&db, &events_tx, pool.clone());

    // Baseline: the ready head has no integrity failures.
    let baseline = actor
        .evaluate_proposal_readiness(&proposal_id)
        .await
        .expect("baseline readiness");
    assert!(
        baseline.ready,
        "ready head must pass DoR before corruption: {:?}",
        baseline.failures
    );

    // Simulate a legacy material head written before repository-boundary linting,
    // with no persisted lint result.
    djinn_db::test_support::replace_legacy_proposal_head_for_test(
        &db,
        &proposal_id,
        CORRUPT_MDX_BODY,
        "mdx",
    )
    .await;
    djinn_db::test_support::delete_proposal_lint_results_for_test(&db, &proposal_id).await;
    let row_count_after_delete = ProposalRepository::new(db.clone(), EventBus::noop())
        .lint_result_count(&proposal_id)
        .await
        .expect("count lint rows after delete");
    assert_eq!(
        row_count_after_delete, 0,
        "legacy head starts without persisted lint"
    );

    // `evaluate_proposal_readiness` must synchronously repair the cache through
    // `lint_for_revision` and surface the `DUPLICATE_BLOCK_ID` integrity failure.
    let result = actor
        .evaluate_proposal_readiness(&proposal_id)
        .await
        .expect("readiness after corrupt head");
    assert!(
        !result.ready,
        "corrupt head must block readiness: {:?}",
        result.failures
    );

    let lint = result
        .latest_lint
        .as_ref()
        .expect("recomputed lint summary must be present");
    assert!(
        !lint.errors.is_empty(),
        "recomputed lint must contain errors"
    );
    assert!(
        lint.errors.iter().any(|v| v.code == "DUPLICATE_BLOCK_ID"),
        "lint must report DUPLICATE_BLOCK_ID: {:?}",
        lint.errors
    );

    // The exact byte-range integrity failure must appear in the DoR result.
    let integrity_failures: Vec<_> = result
        .failures
        .iter()
        .filter(|f| f.check == ReadinessCheck::SpecIntegrity)
        .collect();
    assert!(
        !integrity_failures.is_empty(),
        "corrupt head must produce SpecIntegrity DoR failures"
    );
    let expected_messages: Vec<_> = lint
        .errors
        .iter()
        .map(|v| {
            format!(
                "Spec integrity: {} at bytes {}..{}",
                v.code, v.span.start, v.span.end
            )
        })
        .collect();
    let actual_messages: Vec<_> = integrity_failures
        .iter()
        .map(|f| match &f.detail {
            djinn_control_plane::tools::proposal_readiness::ReadinessFailureDetail::Generic {
                message,
            } => message.clone(),
            detail => panic!("unexpected integrity detail: {detail:?}"),
        })
        .collect();
    assert_eq!(
        actual_messages, expected_messages,
        "integrity failures must match exact byte-range lint errors"
    );

    // `lint_for_revision` recomputes but does NOT persist a lint row (cache
    // repair is read-only). After deleting all lint rows, the count must
    // remain 0 — proving the evaluation does not write lint rows and the
    // historical rows were not disturbed.
    let row_count_after_eval = ProposalRepository::new(db.clone(), EventBus::noop())
        .lint_result_count(&proposal_id)
        .await
        .expect("count lint rows after eval");
    assert_eq!(
        row_count_after_eval, 0,
        "evaluate_proposal_readiness must not persist lint rows during read-only cache repair"
    );
}

/// Regression (criterion 4): an approve/ready verdict cannot be recorded or
/// acted upon when the current head is corrupt. The judge's non-blocking
/// verdict must be converted to blocking by the shared readiness re-evaluation
/// inside `process_judge_outcome`, so the tribunal does NOT park for human
/// review.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_verdict_blocked_when_corrupt_head_recomputed() {
    use super::RefinementSession;
    use crate::refinement::RefinementPhase;
    use std::time::Instant;

    let db = crate::test_helpers::create_test_db();
    let (proposal_id, _) = seed_ready_proposal(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    // Corrupt the head (MDX duplicate block IDs) with no persisted lint row.
    djinn_db::test_support::replace_legacy_proposal_head_for_test(
        &db,
        &proposal_id,
        CORRUPT_MDX_BODY,
        "mdx",
    )
    .await;
    djinn_db::test_support::delete_proposal_lint_results_for_test(&db, &proposal_id).await;

    let head_seq = ProposalRepository::new(db.clone(), EventBus::noop())
        .get(&proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists")
        .latest_revision_seq;

    // Seed an approve (non-blocking) judge verdict — the judge semantically
    // approved, but the machine gate must override it.
    add_judge_verdict(&db, &proposal_id, 1, false, head_seq).await;

    // Park the state machine in JudgeAdjudication so process_refinement_outcome
    // routes to process_judge_outcome.
    actor.active_refinements.insert(
        proposal_id.clone(),
        crate::refinement::RefinementLoopState::new(&proposal_id, head_seq)
            .with_attributed_user(None),
    );

    // Drive the judge outcome through the real coordinator path.
    let session = RefinementSession {
        task_id: "test-judge-task".to_string(),
        phase: RefinementPhase::JudgeAdjudication,
        dispatched_at: Instant::now(),
        session_started_at: Some(Instant::now()),
        model_id: "test/mock".to_string(),
    };
    actor
        .process_refinement_outcome(&proposal_id, &session)
        .await;

    // The tribunal must NOT have parked for human review — the approve verdict
    // was converted to blocking by the readiness re-evaluation.
    let state = actor
        .active_refinements
        .get(&proposal_id)
        .expect("refinement state still active");
    assert_ne!(
        state.phase,
        RefinementPhase::AwaitingHumanReview,
        "corrupt head must prevent approve verdict from parking for human review"
    );
}

/// Regression (criterion 4): after a clean material revision becomes current,
/// the next judge pass resumes ordinary semantic adjudication — a non-blocking
/// verdict parks for human review.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_material_revision_restores_semantic_adjudication() {
    use super::RefinementSession;
    use crate::refinement::RefinementPhase;
    use std::time::Instant;

    let db = crate::test_helpers::create_test_db();
    let (proposal_id, _) = seed_ready_proposal(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    // First corrupt the head so a lint row would exist if persisted.
    djinn_db::test_support::replace_legacy_proposal_head_for_test(
        &db,
        &proposal_id,
        CORRUPT_MDX_BODY,
        "mdx",
    )
    .await;
    djinn_db::test_support::delete_proposal_lint_results_for_test(&db, &proposal_id).await;

    let corrupt_readiness = actor
        .evaluate_proposal_readiness(&proposal_id)
        .await
        .expect("corrupt readiness");
    assert!(!corrupt_readiness.ready, "corrupt head must not be ready");

    // Now create a clean material revision (no duplicate IDs). This bumps
    // latest_revision_seq and writes a new spec_revision row.
    let current = ProposalRepository::new(db.clone(), EventBus::noop())
        .get(&proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    ProposalRepository::new(db.clone(), EventBus::noop())
        .update(
            &proposal_id,
            ProposalUpdateInput {
                title: &current.title,
                body: READY_BODY,
                acceptance_criteria: &current.acceptance_criteria,
                status: &current.status,
                superseded_by: current.superseded_by.as_deref(),
                body_format: Some("markdown"),
                event_metadata: None,
            },
        )
        .await
        .expect("clean material revision");

    let clean_readiness = actor
        .evaluate_proposal_readiness(&proposal_id)
        .await
        .expect("clean readiness");
    assert!(
        clean_readiness.ready,
        "clean material revision must restore readiness: {:?}",
        clean_readiness.failures
    );
    let clean_lint = clean_readiness
        .latest_lint
        .as_ref()
        .expect("clean lint summary");
    assert!(
        clean_lint.errors.is_empty(),
        "clean head must have no lint errors"
    );

    let new_head_seq = ProposalRepository::new(db.clone(), EventBus::noop())
        .get(&proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists")
        .latest_revision_seq;
    assert!(
        new_head_seq > current.latest_revision_seq,
        "clean revision must be material (seq advanced)"
    );

    // A non-blocking judge verdict against the clean head must now park for
    // human review — ordinary semantic adjudication is restored.
    add_judge_verdict(&db, &proposal_id, 1, false, new_head_seq).await;

    actor.active_refinements.insert(
        proposal_id.clone(),
        crate::refinement::RefinementLoopState::new(&proposal_id, new_head_seq)
            .with_attributed_user(None),
    );

    let session = RefinementSession {
        task_id: "test-judge-task-clean".to_string(),
        phase: RefinementPhase::JudgeAdjudication,
        dispatched_at: Instant::now(),
        session_started_at: Some(Instant::now()),
        model_id: "test/mock".to_string(),
    };
    actor
        .process_refinement_outcome(&proposal_id, &session)
        .await;

    let state = actor
        .active_refinements
        .get(&proposal_id)
        .expect("refinement state still active");
    assert_eq!(
        state.phase,
        RefinementPhase::AwaitingHumanReview,
        "clean head must allow approve verdict to park for human review \
         (semantic adjudication restored)"
    );
}

/// Regression (criterion 4): tribunal readiness does not consult doctor
/// findings. A doctor finding claiming the corrupt proposal's integrity is
/// healthy ("info") must NOT suppress the lint-integrity failure surfaced by
/// the shared `lint_for_revision` readiness result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_findings_not_consulted_by_readiness_evaluation() {
    use djinn_control_plane::tools::proposal_readiness::ReadinessCheck;

    let db = crate::test_helpers::create_test_db();
    let (proposal_id, _) = seed_ready_proposal(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let actor = build_refinement_actor(&db, &events_tx, pool.clone());

    // Corrupt the head with no persisted lint row.
    djinn_db::test_support::replace_legacy_proposal_head_for_test(
        &db,
        &proposal_id,
        CORRUPT_MDX_BODY,
        "mdx",
    )
    .await;
    djinn_db::test_support::delete_proposal_lint_results_for_test(&db, &proposal_id).await;

    // Seed a doctor finding that claims the proposal's spec integrity is clean.
    // If the readiness path consulted doctor findings, this would suppress the
    // integrity failure — which it must not.
    djinn_db::test_support::ensure_doctor_findings_schema(&db).await;
    DoctorFindingRepository::new(db.clone())
        .insert(NewDoctorFinding {
            run_id: Some("spec-integrity-doctor-run".to_string()),
            check_name: "spec_integrity".to_string(),
            severity: djinn_db::doctor_severity::INFO.to_string(),
            entity_ids: serde_json::json!([proposal_id]),
            evidence: serde_json::json!({"status": "healthy", "errors": []}),
            resolver_snapshot: None,
            detail: Some("Doctor sweep found no spec integrity issues".to_string()),
        })
        .await
        .expect("seed doctor finding");

    let result = actor
        .evaluate_proposal_readiness(&proposal_id)
        .await
        .expect("readiness with doctor finding present");

    // The shared lint-aware result must still report the corrupt head's
    // DUPLICATE_BLOCK_ID — doctor findings are ignored.
    assert!(
        !result.ready,
        "doctor finding must not suppress lint-integrity failure"
    );
    assert!(
        result
            .failures
            .iter()
            .any(|f| f.check == ReadinessCheck::SpecIntegrity),
        "SpecIntegrity failure must persist despite healthy doctor finding"
    );
    let lint = result.latest_lint.as_ref().expect("lint summary present");
    assert!(
        lint.errors.iter().any(|v| v.code == "DUPLICATE_BLOCK_ID"),
        "lint must report DUPLICATE_BLOCK_ID regardless of doctor findings"
    );
}

/// Regression (criterion 4): `resolve_refinement_review` (human acceptance)
/// also re-evaluates the current head through the shared readiness result and
/// must reject a human accept when the head is corrupt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_accept_rejected_when_corrupt_head_recomputed() {
    let db = crate::test_helpers::create_test_db();
    let (proposal_id, _) = seed_ready_proposal(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let head_seq = ProposalRepository::new(db.clone(), EventBus::noop())
        .get(&proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists")
        .latest_revision_seq;

    // Park the tribunal as if the judge already approved (ready) and the human
    // is about to accept.
    let mut state = crate::refinement::RefinementLoopState::new(&proposal_id, head_seq)
        .with_attributed_user(None);
    state.phase = crate::refinement::RefinementPhase::AwaitingHumanReview;
    actor.active_refinements.insert(proposal_id.clone(), state);

    // Corrupt the head after the tribunal parked — the head changed.
    djinn_db::test_support::replace_legacy_proposal_head_for_test(
        &db,
        &proposal_id,
        CORRUPT_MDX_BODY,
        "mdx",
    )
    .await;
    djinn_db::test_support::delete_proposal_lint_results_for_test(&db, &proposal_id).await;

    // Human acceptance must be rejected because the current head is corrupt.
    let result = actor
        .resolve_refinement_review(&proposal_id, true, None)
        .await;
    assert!(
        result.is_err(),
        "human accept must be rejected when current head is corrupt: {:?}",
        result
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("machine readiness is blocking"),
        "error must reference machine readiness blocking: {err_msg}"
    );

    // The accept was rejected — the refinement was NOT resolved as accepted.
    // `record_judge_verdict(blocking=true)` converts the parked review back
    // into an active round (AdversaryAttack), confirming the corrupt head's
    // blocking readiness was acted upon rather than accepted. The key invariant
    // is that the refinement did NOT complete/resolve as an acceptance.
    let state = actor
        .active_refinements
        .get(&proposal_id)
        .expect("refinement still active after rejected accept");
    assert_ne!(
        state.phase,
        crate::refinement::RefinementPhase::Complete,
        "corrupt head must prevent human accept from completing the refinement"
    );
    // The refinement session must not have been resolved/cleared.
    assert!(
        actor.active_refinements.contains_key(&proposal_id),
        "refinement must remain active (not resolved) after rejected accept"
    );
}
