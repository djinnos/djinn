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
    PARKABLE_FROM_STATUSES, SOURCE_CHANGED, SOURCE_UNCHANGED, TERMINAL_CLOSE_REASON_CLASS,
    TaskRepository, record_adjudication_source_snapshot,
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
    source_with_escalation_rounds_from(db, repo, epic_id, rounds, "in_lead_intervention").await
}

/// As [`source_with_escalation_rounds`], with the status the producer snapshots
/// the source in made explicit — production has several.
async fn source_with_escalation_rounds_from(
    db: &Database,
    repo: &TaskRepository,
    epic_id: &str,
    rounds: i64,
    snapshot_status: &str,
) -> (String, String) {
    source_with_escalation_rounds_labelled(
        db,
        repo,
        epic_id,
        rounds,
        snapshot_status,
        ESCALATION_LABELS,
    )
    .await
}

async fn source_with_escalation_rounds_labelled(
    db: &Database,
    repo: &TaskRepository,
    epic_id: &str,
    rounds: i64,
    snapshot_status: &str,
    child_labels: &str,
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
        repo.update_labels(&child.id, child_labels).await.unwrap();
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
        repo.set_status(&source.id, snapshot_status).await.unwrap();
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

/// **AC7/AC8.** The scaffolding exclusion must hold for EVERY status a producer
/// can snapshot the source in — not just the Lead-intervention pair.
///
/// An earlier fix collapsed only `needs_lead_intervention`/`in_lead_intervention`,
/// which left the defect alive for every producer that snapshots from a PR
/// status: `poll_pr_review_tasks` escalates straight off a `pr_review` row (the
/// merge-method wedge), and the human-adjudicated tripwire hold is reached from
/// `pr_draft`. Both are the z8i8/zkas family — a source held with an unmerged
/// PR — so those were exactly the cases that still stranded.
///
/// Parameterised over the park's own legal from-set. If the mechanism's body is
/// deleted the round-3 sources stay `open` and every case fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_producer_snapshot_status_reaches_the_exhausted_ladder() {
    for snapshot_status in [
        "in_lead_intervention",
        "needs_lead_intervention",
        "pr_review",
        "pr_draft",
        "in_progress",
        "needs_task_review",
        "in_task_review",
        "approved",
        "open",
    ] {
        let db = Database::open_in_memory().unwrap();
        let repo = TaskRepository::new(db.clone(), silent_bus());
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;

        let (source_id, child_id) = source_with_escalation_rounds_from(
            &db,
            &repo,
            &epic_id,
            MAX_AUTONOMOUS_ESCALATIONS,
            snapshot_status,
        )
        .await;
        repo.set_pr_url(&source_id, "https://github.com/o/r/pull/77")
            .await
            .unwrap();

        close_child(&repo, &child_id).await;

        let source = repo.get(&source_id).await.unwrap().unwrap();
        assert_eq!(
            source.status, "pr_review",
            "a source snapshotted at {snapshot_status:?} and parked to `open` was held by \
             coordinator scaffolding, not disposed of: the exhausted ladder must still hand its \
             unmerged PR to the poller"
        );
        let events = outcome_events(&repo, &source_id).await;
        assert_eq!(
            events[0]["source_status_at_child_creation"], snapshot_status,
            "the comparison baseline is reported as metadata"
        );
        // The reported pair must describe THIS transaction. Asserting the
        // creation snapshot here previously locked in a contradiction: for the
        // `pr_review` producer it claimed `pr_review -> pr_review` — no
        // movement — beside a `source_changed` outcome, for the very handoff
        // the transaction had just performed.
        assert_eq!(
            events[0]["source_status_before"], "open",
            "snapshot status {snapshot_status:?}: the source was parked at `open` before this \
             close transaction ran"
        );
        assert_eq!(
            events[0]["source_status_after"], "pr_review",
            "snapshot status {snapshot_status:?}: and this transaction moved it to the poller"
        );
        // Outcome truth-table row 4: the exhausted handoff IS a disposition
        // change. Asserting only `source.status` above let a real defect
        // through — a source snapshotted at `pr_review` compared `pr_review`
        // against `pr_review` and reported `source_unchanged` for the very
        // handoff it had just performed.
        assert_eq!(
            events[0]["adjudication_outcome"], SOURCE_CHANGED,
            "snapshot status {snapshot_status:?}: the exhausted-ladder handoff must always \
             report source_changed, whatever the creation snapshot happened to hold"
        );
    }
}

/// **AC7.** The already-owned branch, exercised where it is genuinely the thing
/// that decides.
///
/// `an_already_owned_source_is_preserved` sets the source to `pr_review` AFTER
/// an `in_lead_intervention` snapshot, so the *changed* guard short-circuits
/// first and `already_owned` is never consulted — that test passes with
/// `already_owned` hardcoded to `false`. Here the snapshot and the close-time
/// status are both `pr_review`, so the changed guard is false and
/// `already_owned` is the only thing standing between an already-owned source
/// and a spurious force-close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn already_owned_is_the_load_bearing_guard_when_nothing_else_changed() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    // Snapshot at `pr_review` and never park: close-time status is `pr_review`
    // too, and the source has no PR, so without `already_owned` the no-PR
    // branch would force-close a source the PR poller already owns.
    let (source_id, child_id) = source_with_escalation_rounds_from(
        &db,
        &repo,
        &epic_id,
        MAX_AUTONOMOUS_ESCALATIONS,
        "pr_review",
    )
    .await;
    repo.set_status(&source_id, "pr_review").await.unwrap();

    close_child(&repo, &child_id).await;

    let source = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(
        source.status, "pr_review",
        "an already-owned source must be preserved, not re-dispositioned"
    );
    assert_eq!(
        source.close_reason, None,
        "and must not acquire a terminal close reason"
    );
    let events = outcome_events(&repo, &source_id).await;
    assert_eq!(
        events[0]["adjudication_outcome"], SOURCE_UNCHANGED,
        "preserving an already-owned source changes nothing"
    );
}

/// Drift guard. `PARKABLE_FROM_STATUSES` is a hand-written list in `djinn-db`
/// mirroring `TransitionAction::ParkForRemediation`'s legal from-set in
/// `djinn-core`. Nothing else keeps them in step, and the two previous
/// instances of the stranding defect were both caused by exactly that set
/// being wrong. Derive the truth from `compute_transition` and compare.
#[test]
fn parkable_from_statuses_matches_the_park_transition() {
    use djinn_core::models::{TaskStatus, TransitionAction};

    // Enumerated explicitly: `TaskStatus` has no `ALL`, and a hand-listed set
    // here is safe because a NEW variant that is missing from this list simply
    // cannot be derived — which is why the list is asserted exhaustive by the
    // `match` below rather than trusted.
    const EVERY_STATUS: &[TaskStatus] = &[
        TaskStatus::Open,
        TaskStatus::InProgress,
        TaskStatus::NeedsTaskReview,
        TaskStatus::InTaskReview,
        TaskStatus::Approved,
        TaskStatus::PrDraft,
        TaskStatus::PrReview,
        TaskStatus::NeedsLeadIntervention,
        TaskStatus::InLeadIntervention,
        TaskStatus::Closed,
    ];
    // Exhaustiveness: adding a `TaskStatus` variant fails to compile here until
    // it is added to `EVERY_STATUS` above, so the derivation can never silently
    // miss one.
    fn assert_every_status_listed(status: &TaskStatus) {
        match status {
            TaskStatus::Open
            | TaskStatus::InProgress
            | TaskStatus::NeedsTaskReview
            | TaskStatus::InTaskReview
            | TaskStatus::Approved
            | TaskStatus::PrDraft
            | TaskStatus::PrReview
            | TaskStatus::NeedsLeadIntervention
            | TaskStatus::InLeadIntervention
            | TaskStatus::Closed => {}
        }
    }
    assert_eq!(EVERY_STATUS.len(), 10);
    EVERY_STATUS.iter().for_each(assert_every_status_listed);

    let derived: Vec<String> = EVERY_STATUS
        .iter()
        .filter(|status| {
            djinn_core::models::compute_transition(
                &TransitionAction::ParkForRemediation,
                status,
                None,
            )
            .is_ok()
        })
        .map(|status| status.as_str().to_owned())
        .collect();
    let mut declared: Vec<String> = PARKABLE_FROM_STATUSES
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let mut derived_sorted = derived.clone();
    declared.sort();
    derived_sorted.sort();
    assert_eq!(
        declared, derived_sorted,
        "PARKABLE_FROM_STATUSES has drifted from ParkForRemediation's legal from-set. Any status \
         the park is legal from is a status a producer can snapshot the source in, and a missing \
         one silently reinstates the exhausted-ladder stranding."
    );
}

/// **AC8/AC7.** The scaffolding exclusion must be DIRECTIONAL: it collapses
/// `<parkable> -> open` (the coordinator parking the source behind its child)
/// and nothing else. A symmetric "these statuses are equal" rule would read the
/// reverse direction as scaffolding too — and the reverse direction is a real
/// planner disposition.
///
/// The case that discriminates is a round-3 close whose planner moved the
/// source with a STATUS-ONLY disposition, back into an in-flight state. Under
/// the shipped directional rule that is `source_changed`, so the exhausted
/// branch is skipped and the planner's decision survives. Under a symmetric
/// rule it reads as scaffolding, the branch fires, and the planner's decision
/// is destroyed by a force-close.
///
/// An earlier version of this test used the `open -> pr_review` handoff
/// instead, and did not bite: `ownership_applied` forces `source_changed` for
/// the handoff regardless of the comparison, so the test passed with the rule
/// made symmetric. If the mechanism's directionality is removed, THIS test
/// fails on the source's final status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scaffolding_exclusion_is_directional() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    // Snapshot at `open` so the delta under test is purely `open -> in_progress`
    // — the reverse of a park, with no scope or close-reason change to carry it.
    let (source_id, child_id) = source_with_escalation_rounds_from(
        &db,
        &repo,
        &epic_id,
        MAX_AUTONOMOUS_ESCALATIONS,
        "open",
    )
    .await;
    // The planner's disposition: put the source back to work. No PR, so a
    // wrongly-fired exhausted branch would force-close it.
    repo.set_status(&source_id, "in_progress").await.unwrap();

    close_child(&repo, &child_id).await;

    let source = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(
        source.status, "in_progress",
        "`open -> in_progress` is a real disposition, not a park: the exhausted branch must not \
         fire and the planner's decision must survive"
    );
    assert_eq!(
        source.close_reason, None,
        "and the source must not acquire a terminal close reason"
    );
    let events = outcome_events(&repo, &source_id).await;
    assert_eq!(events[0]["adjudication_outcome"], SOURCE_CHANGED);
}

/// **AC7.** The handoff the exclusion must never swallow: `open -> pr_review`.
/// Kept as a separate, honestly-scoped assertion — it is guaranteed by
/// `ownership_applied` rather than by the comparison, so it does NOT prove
/// directionality (see [`the_scaffolding_exclusion_is_directional`] for that).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_exhausted_handoff_is_always_a_disposition() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) =
        source_with_escalation_rounds(&db, &repo, &epic_id, MAX_AUTONOMOUS_ESCALATIONS).await;
    repo.set_pr_url(&source_id, "https://github.com/o/r/pull/78")
        .await
        .unwrap();

    close_child(&repo, &child_id).await;

    let events = outcome_events(&repo, &source_id).await;
    assert_eq!(events[0]["source_status_after"], "pr_review");
    assert_eq!(events[0]["adjudication_outcome"], SOURCE_CHANGED);
}

/// **AC8.** A legacy `human-review-hold` child is an adjudication child too:
/// it is counted by `planner_escalation_count_tx` and therefore participates in
/// the round number and can trigger exhausted ownership. Only the autonomous
/// label was exercised before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_human_review_hold_child_is_an_adjudication_child() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) = source_with_escalation_rounds_labelled(
        &db,
        &repo,
        &epic_id,
        MAX_AUTONOMOUS_ESCALATIONS,
        "in_lead_intervention",
        r#"["human-review-hold"]"#,
    )
    .await;

    close_child(&repo, &child_id).await;

    let source = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(
        source.status, "closed",
        "the legacy human-review hold participates in the terminal-rung round count"
    );
    assert_eq!(
        source.close_reason.as_deref(),
        Some(TERMINAL_CLOSE_REASON_CLASS)
    );
    assert!(!outcome_events(&repo, &source_id).await.is_empty());
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
    // The reported pair is the CLOSE transaction's own before/after, not the
    // child-creation snapshot: the producer's park already landed the source at
    // `open` in an earlier transaction, and this close moved nothing.
    assert_eq!(events[0]["source_status_before"], "open");
    assert_eq!(
        events[0]["source_status_at_child_creation"], "in_lead_intervention",
        "the comparison baseline is still reported, as operational metadata"
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
    // The reported pair is the CLOSE transaction's own before/after, not the
    // child-creation snapshot: the producer's park already landed the source at
    // `open` in an earlier transaction, and this close moved nothing.
    assert_eq!(events[0]["source_status_before"], "open");
    assert_eq!(
        events[0]["source_status_at_child_creation"], "in_lead_intervention",
        "the comparison baseline is still reported, as operational metadata"
    );
    assert_eq!(events[0]["source_status_after"], "open");
}

/// **AC8, truth-table row 3.** "Supersede replaces the source while closing the
/// adjudication child → supersession/replacement relationship changes, normally
/// with source status `superseded` → `source_changed`."
///
/// [`BusinessDisposition`] has NO supersession column, and the module comment
/// argues that `status` + `close_reason` carry the relationship on their own.
/// This test proves that claim from the schema up:
///
/// 1. `tasks` genuinely has no supersession/replacement column, so there is
///    nothing else the snapshot could have read (asserted against
///    `information_schema`, not against a comment);
/// 2. the representation PRODUCTION actually writes — `TransitionAction::
///    ArbiterSupersede` force-closes the source, so `status = closed` and
///    `close_reason = force_closed` — reads `source_changed`;
/// 3. the representation the module comment DESCRIBES — `status = superseded`
///    with `close_reason = superseded` — also reads `source_changed`.
///
/// Both representations are covered because the comment's stated values do not
/// match what `ArbiterSupersede` writes; the claim survives that discrepancy,
/// but only because both forms move `status` away from the parked state.
///
/// **If the whole close transaction's body were deleted** no event would exist
/// at all and the first assertion would fail. What would still pass if only the
/// supersession reasoning were wrong: nothing here — a snapshot comparison that
/// ignored `status` and `close_reason` fails both cases.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supersede_before_the_close_reads_source_changed() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    // (1) The premise of the module comment, asserted against the live schema.
    let supersession_columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
          WHERE table_name = 'tasks' AND column_name LIKE '%supersed%'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert!(
        supersession_columns.is_empty(),
        "the snapshot omits a supersession column because `tasks` has none; if one \
         is ever added, BusinessDisposition must start reading it (found {supersession_columns:?})"
    );

    // (2) What `ArbiterSupersede` actually writes: a force-close.
    let (source_id, child_id) = source_with_escalation_rounds(&db, &repo, &epic_id, 1).await;
    sqlx::query("UPDATE tasks SET status = 'closed', close_reason = $2 WHERE id = $1")
        .bind(&source_id)
        .bind(djinn_core::models::task::CLOSE_REASON_FORCE_CLOSED)
        .execute(db.pool())
        .await
        .unwrap();

    close_child(&repo, &child_id).await;

    let events = outcome_events(&repo, &source_id).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["adjudication_outcome"], SOURCE_CHANGED,
        "a supersede replaces the source: status and close_reason both move, so \
         the fixed snapshot carries the relationship without a dedicated column"
    );
    // The pair describes THIS transaction: the source was parked at `open`
    // before it and the supersede left it `closed`.
    assert_eq!(events[0]["source_status_before"], "closed");
    assert_eq!(events[0]["source_status_after"], "closed");
    assert_eq!(
        events[0]["source_status_at_child_creation"],
        "in_lead_intervention"
    );
    let superseded = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(
        superseded.close_reason.as_deref(),
        Some(djinn_core::models::task::CLOSE_REASON_FORCE_CLOSED),
        "the supersede's own terminal reason must survive the close transaction"
    );

    // (3) The representation the module comment describes.
    let (named_id, named_child) = source_with_escalation_rounds(&db, &repo, &epic_id, 1).await;
    sqlx::query("UPDATE tasks SET status = 'superseded', close_reason = $2 WHERE id = $1")
        .bind(&named_id)
        .bind(djinn_core::models::task::CLOSE_REASON_SUPERSEDED)
        .execute(db.pool())
        .await
        .unwrap();

    close_child(&repo, &named_child).await;

    let named_events = outcome_events(&repo, &named_id).await;
    assert_eq!(named_events.len(), 1);
    assert_eq!(
        named_events[0]["adjudication_outcome"], SOURCE_CHANGED,
        "the `superseded` status form must read as a change too"
    );
    assert_eq!(named_events[0]["source_status_after"], "superseded");
}

/// **AC8.** The applied arbiter directive, ALONE, is a disposition change.
///
/// `applied_directive` is the fifth field of the fixed snapshot and was the
/// only one no test ever moved on its own: a comparison that dropped it would
/// have stayed green everywhere. It is derived from the newest
/// `task_arbitrations` row with `directive_injected = TRUE`, so "the arbiter
/// reopened the source with a new directive" is durable state, not prose.
///
/// Status, close reason and every scope field are deliberately untouched here,
/// so the ONLY thing separating `before` from `after` is the directive.
///
/// **If the applied-directive term were dropped from the comparison**, this
/// close would read `source_unchanged` and this test would fail — while every
/// other close test in this file would still pass.
///
/// **What this does NOT prove.** The fixture reaches `directive_injected = TRUE`
/// by calling `mark_directive_injected` directly. In production that flag is
/// flipped only when a WORKER prompt is built (`prompt_context.rs`), and while
/// an adjudication child is open the source is blocked behind it, so no worker
/// prompt exists — the field therefore cannot actually move between the
/// creation snapshot and the close on this path. The term is wired correctly
/// and is proven so here, but it is inert in production today. That is the
/// conservative direction: a field that never moves can only ever under-report
/// a change, never invent one, so it cannot cause a wrong disposition. Stated
/// explicitly so nobody reads this test as evidence that the arbiter's
/// reopen-with-directive case is exercised end to end — it is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_applied_arbiter_directive_alone_reads_source_changed() {
    use crate::repositories::task_arbitration::{
        CreateArbitrationParams, TaskArbitrationRepository,
    };

    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    let (source_id, child_id) = source_with_escalation_rounds(&db, &repo, &epic_id, 1).await;
    let before = repo.get(&source_id).await.unwrap().unwrap();

    // The arbiter applies a reopen directive to the source. No arbitration row
    // existed when the snapshot was taken, so `applied_directive` moves from
    // `None` to this directive and NOTHING else moves.
    let arb = TaskArbitrationRepository::new(db.clone());
    let empty = serde_json::json!([]);
    arb.try_create(CreateArbitrationParams {
        task_id: &source_id,
        hold_cycle: 0,
        deadline_at: None,
        mirror_head_sha: None,
        github_head_sha: None,
        pr_url: None,
        failing_ci_job_ids: &empty,
        dossier: None,
        directive: Some(&serde_json::json!({
            "decision": "reopen",
            "instructions": "register the service in the router before resubmitting",
        })),
        verification_command: None,
        excluded_models: &empty,
    })
    .await
    .expect("open the arbitration row carrying the directive");
    assert!(
        arb.mark_directive_injected(&source_id, 0).await.unwrap(),
        "the directive must be marked injected — that flag is what makes it APPLIED"
    );

    close_child(&repo, &child_id).await;

    let events = outcome_events(&repo, &source_id).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["adjudication_outcome"], SOURCE_CHANGED,
        "a newly applied arbiter directive is a real disposition change even though \
         the status and every scope field are identical"
    );

    // Prove nothing else moved, so the outcome can only have come from the
    // directive.
    let after = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(after.status, before.status, "status must not have moved");
    assert_eq!(after.close_reason, before.close_reason);
    assert_eq!(after.description, before.description);
    assert_eq!(after.design, before.design);
    assert_eq!(events[0]["source_status_before"], "open");
    assert_eq!(events[0]["source_status_after"], "open");
    assert_eq!(
        events[0]["source_status_at_child_creation"],
        "in_lead_intervention"
    );
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
    // The reported pair is the CLOSE transaction's own before/after, not the
    // child-creation snapshot: the producer's park already landed the source at
    // `open` in an earlier transaction, and this close moved nothing.
    assert_eq!(events[0]["source_status_before"], "open");
    assert_eq!(
        events[0]["source_status_at_child_creation"], "in_lead_intervention",
        "the comparison baseline is still reported, as operational metadata"
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
        Some(TERMINAL_CLOSE_REASON_CLASS),
        "the close_reason COLUMN carries the fixed vocabulary class, matching what \
         `terminally_fail_task` writes on the coordinator-side copy of this contract"
    );
    // ...and the contractual sentence is on the timeline, where the other copy
    // puts it too, so one operator query finds both.
    let transition_reasons: Vec<String> = repo
        .query_activity(crate::repositories::task::ActivityQuery {
            task_id: Some(source_id.clone()),
            event_type: Some("status_changed".to_owned()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).ok())
        .filter(|p| p["to_status"] == "closed")
        .filter_map(|p| p["reason"].as_str().map(str::to_owned))
        .collect();
    assert!(
        transition_reasons
            .iter()
            .any(|reason| reason == LADDER_EXHAUSTED_CLOSE_REASON),
        "the exhausted-ladder disposition must be visible on the timeline with its exact \
         contractual reason; got {transition_reasons:?}"
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

/// **AC8, the "no retroactive change" clause.** The spec is explicit that the
/// outcome "describes ONLY that transaction — it is not a claim that the task
/// eventually merged or converged, and no longitudinal reporting is derived from
/// it". So a LATER merge must leave the already-emitted event exactly as it was:
/// no rewrite, no correcting second event.
///
/// This is a real hazard rather than a hypothetical one. The obvious "fix" for
/// an operator complaining that a `source_changed` handoff later merged anyway
/// is to re-run the close and amend the row — and the amendment would be
/// silently wrong, because the merge is a different transaction with a different
/// cause. The duplicate-delivery guard is what forbids it, and this test pins
/// the guard against the specific input that would tempt someone to bypass it: a
/// source whose `merge_commit_sha` is now set and whose status has moved
/// terminal since the event was written.
///
/// The durable evidence is the `activity_log` ROW ITSELF — id, `payload::text`
/// and `created_at`, read before and after the merge — plus the row count.
///
/// **If `apply_adjudication_child_close_tx`'s body were made inert** the first
/// half of this test fails: no round-3 handoff and no event to preserve. **If
/// only the `outcome_already_emitted` guard were deleted**, the redelivery after
/// the merge would emit a SECOND event — reporting `source_unchanged` off the
/// merged row — and both the count assertion and the returned-`emitted`
/// assertion fail while every other test in this file still passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_merge_does_not_retroactively_change_the_emitted_outcome() {
    let db = Database::open_in_memory().unwrap();
    let repo = TaskRepository::new(db.clone(), silent_bus());
    let project = make_project(&db).await;
    let epic_id = make_epic(&db, &project.id).await;

    /// The full durable outcome rows for a source: (id, canonical payload text,
    /// created_at). Ordered by id so the comparison is position-stable.
    async fn outcome_rows(db: &Database, source_id: &str) -> Vec<(String, String, String)> {
        sqlx::query_as::<_, (String, String, String)>(
            r#"SELECT id, payload::text, created_at FROM activity_log
                WHERE task_id = $1 AND event_type = $2
                ORDER BY id ASC"#,
        )
        .bind(source_id)
        .bind(ADJUDICATION_OUTCOME_EVENT)
        .fetch_all(db.pool())
        .await
        .unwrap()
    }

    let (source_id, child_id) =
        source_with_escalation_rounds(&db, &repo, &epic_id, MAX_AUTONOMOUS_ESCALATIONS).await;
    repo.set_pr_url(&source_id, "https://github.com/o/r/pull/93")
        .await
        .unwrap();

    close_child(&repo, &child_id).await;

    let before_merge = outcome_rows(&db, &source_id).await;
    assert_eq!(
        before_merge.len(),
        1,
        "the round-3 close must have emitted exactly one outcome event"
    );
    let emitted_payload: serde_json::Value = serde_json::from_str(&before_merge[0].1).unwrap();
    assert_eq!(
        emitted_payload["adjudication_outcome"], SOURCE_CHANGED,
        "fixture: the event under test is the terminal handoff"
    );
    assert_eq!(emitted_payload["source_status_after"], "pr_review");
    assert_eq!(
        repo.get(&source_id).await.unwrap().unwrap().status,
        TaskStatus::PrReview.as_str(),
    );

    // ── The later merge ────────────────────────────────────────────────────
    // The poller took the handoff, the PR landed, and the task closed. Both
    // things the exhausted-ladder branch reads — `merge_commit_sha` and the
    // status — now say something different from what they said in the
    // transaction that wrote the event.
    sqlx::query(
        "UPDATE tasks SET merge_commit_sha = $2, status = 'closed', close_reason = $3 WHERE id = $1",
    )
    .bind(&source_id)
    .bind("f00dfacecafe0001")
    .bind(djinn_core::models::task::CLOSE_REASON_COMPLETED)
    .execute(db.pool())
    .await
    .unwrap();

    let after_merge = outcome_rows(&db, &source_id).await;
    assert_eq!(
        after_merge, before_merge,
        "the merge must not rewrite the emitted event: the row's id, payload and created_at all \
         describe the child-close transaction, not the task's eventual fate"
    );

    // ── And a re-delivery after the merge stays a no-op ────────────────────
    // Without this the assertion above would only prove that an UPDATE on
    // `tasks` does not touch `activity_log`, which is trivially true. Re-running
    // the mechanism against the merged row is the input that would produce a
    // second, contradictory event.
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
        "re-delivering the close after the merge must emit nothing"
    );
    assert_eq!(
        outcome_rows(&db, &source_id).await,
        before_merge,
        "no second outcome event, and the first one is still byte-identical: the (source, child) \
         pair is the idempotency key and the merge is not part of it"
    );

    // The merge's own state is untouched too — the re-delivery must not have
    // re-dispositioned a source somebody else already closed.
    let merged = repo.get(&source_id).await.unwrap().unwrap();
    assert_eq!(merged.status, "closed");
    assert_eq!(
        merged.close_reason.as_deref(),
        Some(djinn_core::models::task::CLOSE_REASON_COMPLETED),
        "the merge's own close reason must survive; the exhausted-ladder class must not \
         overwrite it"
    );
    assert_eq!(merged.merge_commit_sha.as_deref(), Some("f00dfacecafe0001"));
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
