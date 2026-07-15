// Regression tests for the "Current DoR status" string injected into
// tribunal refinement task descriptions.
//
// Split out of `refinement_cap_tests.rs` to keep that file under the
// size-guard byte threshold; shares its fixture helpers.

use super::refinement_cap_tests::{
    build_refinement_actor, seed_refinement_fixture, spawn_test_pool,
};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_db::{ProposalRepository, TaskRepository};

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

    // Drive the real injection path used at dispatch: fold the readiness into
    // the "Current DoR status" string and create the tribunal task.
    let readiness_context = refreshed
        .to_error_string()
        .unwrap_or_else(|| "Proposal currently meets all DoR checks.".to_string());
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

/// An unresolvable durable owner fails closed before task creation. This direct
/// task-builder regression also proves no session/spawn bookkeeping is created
/// by the rejection; the dispatch path performs the same owner gate earlier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_task_missing_owner_creates_no_task_or_spawn() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let actor = build_refinement_actor(&db, &events_tx, pool.clone());

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
        "missing owner must prevent task insertion"
    );
    assert!(
        actor.refinement_sessions.is_empty(),
        "no agent spawn was recorded"
    );
    assert!(
        TaskRepository::new(db.clone(), EventBus::noop())
            .list_by_project(&fixture.project_id)
            .await
            .expect("list project tasks")
            .into_iter()
            .all(|task| task.issue_type != "refinement")
    );
}
