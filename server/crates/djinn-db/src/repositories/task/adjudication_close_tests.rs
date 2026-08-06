//! Proposal 4etb: the adjudication child-close transaction.
//!
//! Two guarantees, both of which live or die inside ONE transaction:
//!
//! - **Exhausted-ladder ownership.** When terminal-rung round 3 closes without
//!   changing the source's disposition, the source must land with a NAMED
//!   owner: `pr_review` (the PR poller) when it carries an open unmerged PR,
//!   terminal `closed` with the contractual reason when it has no PR, and
//!   preserved when it is already owned. A fourth escalation is forbidden.
//! - **The adjudication outcome event.** Every adjudication child close emits
//!   `adjudication_outcome` = `source_changed` / `source_unchanged` with
//!   `source_status_before` / `source_status_after`, describing ONLY that
//!   transaction.
//!
//! Every assertion reads the durable row or activity payload the mechanism
//! WRITES, never a log line.

use djinn_core::events::EventBus;
use djinn_core::models::{Project, TaskStatus, TransitionAction};

use crate::database::Database;
use crate::repositories::task::{
    ADJUDICATION_OUTCOME_EVENT, LADDER_EXHAUSTED_CLOSE_REASON, MAX_AUTONOMOUS_ESCALATIONS,
    SOURCE_CHANGED, SOURCE_UNCHANGED, TaskRepository, record_adjudication_source_snapshot,
};

const ESCALATION_LABELS: &str = r#"["planner-park-escalation"]"#;

fn silent_bus() -> EventBus {
    EventBus::new(|_| {})
}

async fn make_project(db: &Database) -> Project {
    db.ensure_initialized().await.unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1,$2,$3,$4)")
        .bind(&id)
        .bind("adjudication-close")
        .bind("test")
        .bind("adjudication-close")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query_as::<_, Project>(
        r#"SELECT id, name, github_owner, github_repo, created_at, target_branch,
                  auto_merge, sync_enabled, sync_remote
             FROM projects WHERE id = $1"#,
    )
    .bind(&id)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

async fn make_epic(db: &Database, project_id: &str) -> String {
    let epic_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
         VALUES ($1,$2,$3,'Epic','','','','','[]'::jsonb)",
    )
    .bind(&epic_id)
    .bind(project_id)
    .bind(&epic_id[..4])
    .execute(db.pool())
    .await
    .unwrap();
    epic_id
}

/// A source task plus `rounds` adjudication children blocking it, the last of
/// which is returned. Mirrors production: each escalation adds exactly one
/// blocker that is never removed, so `rounds` IS the terminal-rung round
/// number — no counter column anywhere.
async fn source_with_escalation_rounds(
    db: &Database,
    repo: &TaskRepository,
    epic_id: &str,
    rounds: i64,
) -> (String, String) {
    let source = repo
        .create_fixture_with_ac(
            epic_id, "Source", "desc", "design", "task", 1, "worker", None, None,
        )
        .await
        .unwrap();
    let mut last_child = String::new();
    for round in 0..rounds {
        let child = repo
            .create_fixture_with_ac(
                epic_id,
                &format!("Planner terminal escalation round {}", round + 1),
                "escalation",
                "",
                "review",
                1,
                "planner",
                None,
                None,
            )
            .await
            .unwrap();
        repo.update_labels(&child.id, ESCALATION_LABELS)
            .await
            .unwrap();
        repo.add_blocker(&source.id, &child.id).await.unwrap();

        // Mirror PRODUCTION ordering exactly. Both producers snapshot BEFORE
        // parking the source: `escalate_to_planner_or_terminally_fail` calls
        // `create_remediation_task` (which snapshots) and then
        // `park_source_open`, and the agent's `execute_arbiter_park_transaction`
        // snapshots before the `ArbiterPark` transition. So the snapshot sees
        // the source mid-adjudication and the close sees it parked at `open`.
        //
        // An earlier version of this fixture left the source `open` throughout,
        // which production never does — and that difference alone hid a defect
        // that made the entire child-close transaction inert in production:
        // every close read `source_changed` from the scaffolding status delta,
        // so the exhausted-ladder branch never ran and a round-3 close left the
        // source `open` with an unmerged PR and no owner.
        repo.set_status(&source.id, "in_lead_intervention")
            .await
            .unwrap();
        record_adjudication_source_snapshot(db.pool(), &source.id, &child.id)
            .await
            .unwrap();
        repo.set_status(&source.id, "open").await.unwrap();

        last_child = child.id;
    }
    (source.id, last_child)
}

async fn outcome_events(repo: &TaskRepository, task_id: &str) -> Vec<serde_json::Value> {
    repo.query_activity(crate::repositories::task::ActivityQuery {
        task_id: Some(task_id.to_owned()),
        event_type: Some(ADJUDICATION_OUTCOME_EVENT.to_owned()),
        limit: 50,
        ..Default::default()
    })
    .await
    .unwrap()
    .into_iter()
    .map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).unwrap())
    .collect()
}

async fn close_child(repo: &TaskRepository, child_id: &str) {
    repo.transition(
        child_id,
        TransitionAction::ForceClose,
        "planner",
        "planner",
        Some("adjudicated"),
        None,
    )
    .await
    .unwrap();
}

// ── AC8: the outcome event ──────────────────────────────────────────────────

/// **AC8.** A round-1 decision failure — close the child, release its blocker,
/// let the next round be created — is pure coordinator scaffolding. It must
/// read `source_unchanged`, because none of the fixed business-disposition
/// fields moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_one_scaffolding_close_reads_source_unchanged() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) = source_with_escalation_rounds(&db, &repo, &epic_id, 1).await;
    close_child(&repo, &child_id).await;

    let events = outcome_events(&repo, &source_id).await;
    assert_eq!(events.len(), 1, "exactly one outcome event per child close");
    assert_eq!(events[0]["adjudication_outcome"], SOURCE_UNCHANGED);
    assert_eq!(
        events[0]["source_status_before"], "in_lead_intervention",
        "the snapshot is taken mid-adjudication, exactly as production does"
    );
    assert_eq!(events[0]["source_status_after"], "open");
    assert_eq!(events[0]["adjudication_child_id"], child_id);

    let source = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(
        source.status, "open",
        "rounds 1 and 2 release the source; only round 3 disposes of it"
    );
}

/// **AC8.** A rescope keeps the status `open` but changes the scope fields, so
/// the SAME close must read `source_changed`. This is the assertion that makes
/// the outcome mean something: a version that only compared status would pass
/// the test above and fail here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rescope_before_the_close_reads_source_changed() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) = source_with_escalation_rounds(&db, &repo, &epic_id, 1).await;

    // The planner rescopes the source through its own MCP calls, BEFORE it
    // closes the child. This is precisely why `before` is the snapshot taken at
    // child creation and not a read inside the close transaction.
    sqlx::query("UPDATE tasks SET description = $2 WHERE id = $1")
        .bind(&source_id)
        .bind("rescoped: the unmet criterion, carved out and made unambiguous")
        .execute(db.pool())
        .await
        .unwrap();

    close_child(&repo, &child_id).await;

    let events = outcome_events(&repo, &source_id).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["adjudication_outcome"], SOURCE_CHANGED,
        "a scope change is a real disposition even though the status did not move"
    );
    assert_eq!(
        events[0]["source_status_before"], "in_lead_intervention",
        "the snapshot is taken mid-adjudication, exactly as production does"
    );
    assert_eq!(events[0]["source_status_after"], "open");
}

/// **AC8.** Duplicate closure delivery emits no second event and re-runs no
/// disposition. The (source, child) pair is the idempotency key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_close_delivery_emits_no_second_event() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) = source_with_escalation_rounds(&db, &repo, &epic_id, 1).await;
    close_child(&repo, &child_id).await;

    // Re-deliver the close transaction directly against the same child.
    let mut tx = db.pool().begin().await.unwrap();
    let emitted = crate::repositories::task::apply_adjudication_child_close_tx(
        &mut tx,
        &child_id,
        ESCALATION_LABELS,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        emitted, 0,
        "a duplicate delivery must be an idempotent no-op"
    );
    assert_eq!(
        outcome_events(&repo, &source_id).await.len(),
        1,
        "exactly one outcome event survives duplicate delivery"
    );
}

// ── AC7: exhausted-ladder ownership ─────────────────────────────────────────

/// **AC7.** The unchanged close of terminal round 3 on a source with an OPEN,
/// UNMERGED PR sends it to `pr_review` — the PR poller's queue — in the SAME
/// transaction. This is the `z8i8`/`zkas` fix: those tasks exhausted the old
/// ladder with unmerged PRs and sat `open`, a status nothing scanned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_three_unchanged_close_sends_an_open_pr_source_to_pr_review() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) =
        source_with_escalation_rounds(&db, &repo, &epic_id, MAX_AUTONOMOUS_ESCALATIONS).await;
    repo.set_pr_url(&source_id, "https://github.com/o/r/pull/91")
        .await
        .unwrap();

    close_child(&repo, &child_id).await;

    let source = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(
        source.status,
        TaskStatus::PrReview.as_str(),
        "an exhausted source with an open unmerged PR belongs to the PR poller"
    );
    let events = outcome_events(&repo, &source_id).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["adjudication_outcome"], SOURCE_CHANGED,
        "the ownership transition IS a disposition change"
    );
    assert_eq!(
        events[0]["source_status_before"], "in_lead_intervention",
        "the snapshot is taken mid-adjudication, exactly as production does"
    );
    assert_eq!(events[0]["source_status_after"], "pr_review");
}

/// **AC7.** No PR means no poller can own it, so the ladder ends terminally with
/// the EXACT contractual reason. Operator queries key on that text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_three_unchanged_close_force_closes_a_no_pr_source() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) =
        source_with_escalation_rounds(&db, &repo, &epic_id, MAX_AUTONOMOUS_ESCALATIONS).await;
    close_child(&repo, &child_id).await;

    let source = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(source.status, "closed");
    assert_eq!(
        source.close_reason.as_deref(),
        Some(LADDER_EXHAUSTED_CLOSE_REASON),
        "the terminal reason is contractual, not free text"
    );
    let events = outcome_events(&repo, &source_id).await;
    assert_eq!(events[0]["adjudication_outcome"], SOURCE_CHANGED);
    assert_eq!(events[0]["source_status_after"], "closed");
}

/// **AC7.** A merged PR does NOT satisfy the open-PR branch: a landed PR gives
/// the poller nothing to do, so the source is terminal instead. Without this
/// the contract would park merged work in `pr_review` forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merged_pr_does_not_satisfy_the_open_pr_branch() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) =
        source_with_escalation_rounds(&db, &repo, &epic_id, MAX_AUTONOMOUS_ESCALATIONS).await;
    repo.set_pr_url(&source_id, "https://github.com/o/r/pull/92")
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET merge_commit_sha = $2 WHERE id = $1")
        .bind(&source_id)
        .bind("deadbeefcafe")
        .execute(db.pool())
        .await
        .unwrap();

    close_child(&repo, &child_id).await;

    let source = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(
        source.status, "closed",
        "a merged PR is not an actionable PR — the source is terminal, not re-queued"
    );
}

/// **AC7.** A round-3 close that DID change the source must be left exactly as
/// the planner left it. Disposing of a freshly rescoped source would destroy
/// the very decision the terminal rung exists to produce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_three_does_not_dispose_of_a_source_the_planner_changed() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) =
        source_with_escalation_rounds(&db, &repo, &epic_id, MAX_AUTONOMOUS_ESCALATIONS).await;
    sqlx::query("UPDATE tasks SET design = $2 WHERE id = $1")
        .bind(&source_id)
        .bind("planner rescoped the design at the terminal round")
        .execute(db.pool())
        .await
        .unwrap();

    close_child(&repo, &child_id).await;

    let source = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(
        source.status, "open",
        "a changed source keeps the planner's disposition; exhausted ownership must not fire"
    );
    let events = outcome_events(&repo, &source_id).await;
    assert_eq!(events[0]["adjudication_outcome"], SOURCE_CHANGED);
}

/// **AC7.** An already-owned source (already `pr_review`) is preserved: the
/// contract never re-dispositions state somebody else owns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_already_owned_source_is_preserved() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) =
        source_with_escalation_rounds(&db, &repo, &epic_id, MAX_AUTONOMOUS_ESCALATIONS).await;
    repo.set_status(&source_id, "pr_review").await.unwrap();

    close_child(&repo, &child_id).await;

    let source = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(source.status, "pr_review");
    assert!(
        source.close_reason.as_deref() != Some(LADDER_EXHAUSTED_CLOSE_REASON),
        "an already-owned source must not acquire the exhausted-ladder close reason"
    );
}

/// **AC1/AC2.** Closing an adjudication child clears the source's evidence
/// epoch, so a LATER trigger stamps a NEW one. Without this every future
/// escalation would measure worker evidence against the first trigger's
/// instant and count attempts belonging to an adjudication already spent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_an_adjudication_child_clears_the_evidence_epoch() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) = source_with_escalation_rounds(&db, &repo, &epic_id, 1).await;
    repo.stamp_escalation_evidence_epoch(&source_id)
        .await
        .unwrap();
    assert!(
        repo.escalation_evidence_at(&source_id)
            .await
            .unwrap()
            .is_some(),
        "fixture must start with a stamped epoch"
    );

    close_child(&repo, &child_id).await;

    assert!(
        repo.escalation_evidence_at(&source_id)
            .await
            .unwrap()
            .is_none(),
        "the epoch must be cleared when the adjudication clears"
    );
}

/// A NON-adjudication child close must not touch any of this. The label is the
/// discriminator, and an ordinary blocker closing is not an adjudication.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_blocker_close_emits_no_outcome_event() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let source = repo
        .create_fixture_with_ac(
            &epic_id, "Source", "d", "d", "task", 1, "worker", None, None,
        )
        .await
        .unwrap();
    let blocker = repo
        .create_fixture_with_ac(
            &epic_id, "Ordinary", "d", "d", "task", 1, "worker", None, None,
        )
        .await
        .unwrap();
    repo.add_blocker(&source.id, &blocker.id).await.unwrap();

    close_child(&repo, &blocker.id).await;

    assert!(
        outcome_events(&repo, &source.id).await.is_empty(),
        "only adjudication children produce an adjudication outcome"
    );
    let refreshed = repo.get(&source.id).await.unwrap().unwrap();
    assert_eq!(refreshed.status, "open");
}
