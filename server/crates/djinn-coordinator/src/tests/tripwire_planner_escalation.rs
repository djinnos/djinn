//! DB-backed tests for the zero-human-hold tripwire path: an arbiter-class
//! tripwire `Held` gate creates an autonomous `planner-park-escalation` (not a
//! `human-review-hold`), leaves NO hold label on the source, keeps the merge
//! blocked via the active-hold state until the escalation closes, and — on
//! close — emits `tripwire.hold.released` for every held head. The
//! org-policy human-adjudication escape hatch still takes the legacy path.

use super::*;
use crate::tripwires::{ActivityEntryRef, TripwireEvaluationInput, TripwirePolicy};
use djinn_core::models::TransitionAction;
use djinn_provider::github_api::PrFile;

const HELD_HEAD: &str = "head-tripwire-escalation-0001";
const PR_NUMBER: u64 = 4242;

/// Build a `Held` tripwire gate result from a migration change (an
/// enforcement-on rule) for the given task/project/head.
fn held_migration_result(
    task_id: &str,
    project_id: &str,
    head_sha: &str,
) -> crate::pr_poller::tripwire_gate::TripwireGateResult {
    let pr_files = vec![PrFile {
        sha: "deadbeef".to_owned(),
        filename: "migrations/20260709_add_column.sql".to_owned(),
        status: "added".to_owned(),
        additions: 12,
        deletions: 0,
        changes: 12,
        patch: None,
    }];
    let input = TripwireEvaluationInput {
        task_id: task_id.to_owned(),
        project_id: project_id.to_owned(),
        pr_number: Some(PR_NUMBER),
        head_sha: head_sha.to_owned(),
        policy: TripwirePolicy::default(),
        allowlist_revision: None,
        changed_files: crate::pr_poller::tripwire_gate::convert_pr_files(&pr_files),
    };
    let result = crate::pr_poller::tripwire_gate::run_gate(&input);
    assert_eq!(
        result.decision.outcome,
        crate::tripwires::GateOutcome::Held,
        "fixture must produce a Held gate"
    );
    result
}

/// Log the `tripwire.gate.held` activity event on the source, exactly as the
/// PR poller does before creating the hold — this establishes the active-hold
/// state that gates the merge independently of any label.
async fn log_gate_held(
    repo: &TaskRepository,
    task_id: &str,
    result: &crate::pr_poller::tripwire_gate::TripwireGateResult,
) {
    let payload_json = serde_json::to_string(&result.payload).unwrap();
    repo.log_activity(
        Some(task_id),
        "coordinator",
        "system",
        result.event_type,
        &payload_json,
    )
    .await
    .unwrap();
}

async fn active_hold_held(repo: &TaskRepository, task_id: &str, head_sha: &str) -> bool {
    let entries = repo.list_activity(task_id).await.unwrap();
    let refs: Vec<ActivityEntryRef> = entries.iter().map(ActivityEntryRef::from_entry).collect();
    crate::tripwires::compute_active_hold_state(&refs, head_sha).held
}

/// Arbiter-class (default policy) tripwire `Held` → autonomous planner-park
/// escalation with the adjudication dossier; NO human-review-hold task, NO hold
/// label on the source; merge stays blocked until the escalation closes, then a
/// release event clears the hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arbiter_tripwire_hold_creates_planner_escalation_no_human_hold() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_status(&task.id, "pr_review").await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();

    let result = held_migration_result(&task.id, &task.project_id, HELD_HEAD);
    log_gate_held(&repo, &task.id, &result).await;

    // Merge is blocked by the active-hold state before any label exists.
    assert!(
        active_hold_held(&repo, &task.id, HELD_HEAD).await,
        "gate.held event must make the active-hold state block the merge"
    );

    actor.create_tripwire_hold(&task, &result, HELD_HEAD).await;

    // Source is parked open and NOT labelled human-review-hold.
    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(parked.status, "open", "tripwire hold parks the source open");
    assert!(
        !parked.labels.contains("human-review-hold"),
        "arbiter tripwire hold must NOT stamp human-review-hold on the source; labels={}",
        parked.labels
    );

    // The blocker is an autonomous planner-park escalation carrying the dossier.
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(blockers.len(), 1, "source must be held by one escalation");
    let escalation = repo.get(&blockers[0].task_id).await.unwrap().unwrap();
    assert_eq!(escalation.issue_type, "review");
    assert_eq!(escalation.status, "open");
    assert!(
        escalation.labels.contains("planner-park-escalation"),
        "hold must be a planner-park escalation; labels={}",
        escalation.labels
    );
    assert!(
        !escalation.labels.contains("human-review-hold"),
        "arbiter tripwire hold must NOT create a human-review hold task; labels={}",
        escalation.labels
    );
    // Dossier content: finding rule, adjudication instructions, evidence path.
    for needle in [
        "Tripwire adjudication",
        "migration_change",
        "migrations/20260709_add_column.sql",
        "CLOSE this task",
        "Reopen the source",
    ] {
        assert!(
            escalation.description.contains(needle),
            "escalation dossier must contain {needle:?}; got: {}",
            escalation.description
        );
    }

    // Merge still blocked (hold active) while the escalation is open.
    assert!(
        active_hold_held(&repo, &task.id, HELD_HEAD).await,
        "hold must stay active until the escalation is resolved"
    );

    // Close the escalation (benign adjudication) and run the release path.
    repo.transition(
        &escalation.id,
        TransitionAction::Close,
        "planner",
        "system",
        Some("Cleared migration_change: additive column, no backfill risk."),
        None,
    )
    .await
    .unwrap();
    let closed_escalation = repo.get(&escalation.id).await.unwrap().unwrap();
    actor
        .emit_tripwire_release_on_hold_close(&closed_escalation)
        .await;

    // A release event was emitted and the active-hold state is now cleared.
    let entries = repo.list_activity(&task.id).await.unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.event_type == crate::tripwires::TRIPWIRE_EVENT_HOLD_RELEASED),
        "closing the escalation must emit tripwire.hold.released on the source"
    );
    assert!(
        !active_hold_held(&repo, &task.id, HELD_HEAD).await,
        "the hold must be released after the escalation closes — merge may proceed"
    );
}

/// Org-policy human-adjudication escape hatch: a rule an operator opted to
/// `Human` still takes the legacy human-review remediation path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_adjudicated_tripwire_hold_keeps_legacy_human_review_path() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_status(&task.id, "pr_review").await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();

    let result = held_migration_result(&task.id, &task.project_id, HELD_HEAD);
    log_gate_held(&repo, &task.id, &result).await;

    // Operator opts the migration rule into human adjudication.
    let mut policy = TripwirePolicy::default();
    policy.migration.adjudication = crate::tripwires::Adjudication::Human;

    actor
        .create_tripwire_hold_with_policy(&task, &result, HELD_HEAD, policy)
        .await;

    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(blockers.len(), 1, "source must be held by one blocker");
    let hold = repo.get(&blockers[0].task_id).await.unwrap().unwrap();
    assert!(
        hold.labels.contains("human-review-hold"),
        "human-adjudicated rule must take the legacy human-review path; labels={}",
        hold.labels
    );
    assert!(
        !hold.labels.contains("planner-park-escalation"),
        "human escape hatch must NOT create a planner-park escalation; labels={}",
        hold.labels
    );
}
