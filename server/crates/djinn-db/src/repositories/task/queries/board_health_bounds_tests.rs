//! Regression: `board_health` must stay bounded by live (non-closed) work.
//!
//! The 2026-07-17 restart loop: the mismatch-candidate pull scanned every
//! task on the board (closed history included, ~20 correlated subqueries per
//! row) and the review-queue section fetched every closed task ever — both on
//! every `board_health` call, which the UI polls continuously. As the closed
//! backlog grew the calls reached 6–9 s, starving the coordinator tick and
//! the liveness probe. Closed tasks must appear in NEITHER section.
use crate::database::Database;
use crate::repositories::epic::EpicCreateInput;
use crate::repositories::task::{EffectiveCreatorProvenance, TaskRepository};
use crate::repositories::user::UserRepository;
use djinn_core::events::EventBus;

async fn setup_project(db: &Database) -> (String, String) {
    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "p",
        "test",
        format!("board-health-bounds-{project_id}"),
    )
    .execute(db.pool())
    .await
    .unwrap();

    let epic_repo = crate::repositories::epic::EpicRepository::new(db.clone(), EventBus::noop());
    let epic = epic_repo
        .create_for_project(
            &project_id,
            EpicCreateInput {
                title: "Board health epic",
                description: "",
                emoji: "",
                color: "",
                owner: "",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .unwrap();
    (project_id, epic.id)
}

/// Persist a real fixture user for insertion-time task attribution.
async fn fixture_creator_id(db: &Database) -> String {
    UserRepository::new(db.clone())
        .upsert_from_github(
            9_100_042,
            "board-health-fixture",
            Some("Board Health Fixture"),
            None,
        )
        .await
        .unwrap()
        .id
}

/// A closed task with heavy reopen churn and planner-toolset signals must NOT
/// surface as a role/tool mismatch — the report is advisory about live work,
/// and pulling closed history made the candidate scan unbounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_mismatch_candidates_exclude_closed_tasks() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (project_id, epic_id) = setup_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let creator_id = fixture_creator_id(&db).await;

    // Two identical churn-heavy tasks whose text carries planner signals
    // ("task_create") while the dispatched role for a plain `task` is worker.
    let mut ids = Vec::new();
    for title in ["live mismatch", "closed mismatch"] {
        let task = task_repo
            .create_in_project_with_provenance(
                &project_id,
                Some(&epic_id),
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&creator_id),
                    source_task_id: None,
                    proposal_id: None,
                },
                title,
                "this needs task_create and epic_update to proceed",
                "",
                "task",
                0,
                "",
                None,
                None,
            )
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE tasks SET total_reopen_count = 5 WHERE id = $1",
            task.id
        )
        .execute(db.pool())
        .await
        .unwrap();
        ids.push(task.id);
    }
    sqlx::query!("UPDATE tasks SET status = 'closed' WHERE id = $1", ids[1])
        .execute(db.pool())
        .await
        .unwrap();

    let health = task_repo.board_health(24).await.unwrap();
    let mismatches = health["repeated_reopen_role_tool_mismatches"]
        .as_array()
        .expect("mismatch section is an array");
    let listed: Vec<&str> = mismatches.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(
        listed.contains(&ids[0].as_str()),
        "the live churn-heavy task must be reported; got {listed:?}"
    );
    assert!(
        !listed.contains(&ids[1].as_str()),
        "a closed task must never surface as a mismatch candidate; got {listed:?}"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Focused DB parity/prefilter tests for the narrow mismatch candidate path.
//
// The SQL prefilter in `list_board_health_mismatch_candidates` must be a
// conservative superset of authoritative Rust inference (`infer_expected_role_
// for_task`). These tests systematically prove that:
//   • every signal token at queries.rs:26–48 produces a Rust-positive fixture
//     that the SQL prefilter also returns;
//   • mixed-case text and issue types match in both layers;
//   • the `total_reopen_count >= 3` threshold admits exactly-3 and excludes
//     below-threshold rows;
//   • an impossible-positive row (high reopen count, no signals) is excluded.
// ────────────────────────────────────────────────────────────────────────────

/// Create a task with the given issue type and description, then set its
/// `total_reopen_count` via a raw UPDATE (the create path does not expose it).
#[allow(clippy::too_many_arguments)]
async fn create_signal_fixture(
    task_repo: &TaskRepository,
    db: &Database,
    project_id: &str,
    epic_id: &str,
    title: &str,
    issue_type: &str,
    description: &str,
    reopen_count: i64,
) -> String {
    let creator_id = fixture_creator_id(db).await;
    let task = task_repo
        .create_in_project_with_provenance(
            project_id,
            Some(epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator_id),
                source_task_id: None,
                proposal_id: None,
            },
            title,
            description,
            "",
            issue_type,
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET total_reopen_count = $1 WHERE id = $2")
        .bind(reopen_count)
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();
    task.id
}

/// Every signal token declared at queries.rs:26–48 must produce a Rust-positive
/// fixture that the SQL prefilter also returns. Iterates over issue-type
/// signals, planner text signals, and lead text signals independently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatch_prefilter_returns_every_rust_positive_signal_token() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (project_id, epic_id) = setup_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    // Collect (issue_type, description) fixtures — one per signal token.
    // Every fixture gets total_reopen_count = 3 (exactly at threshold).
    let mut fixtures: Vec<(String, String)> = Vec::new();
    let mut labels: Vec<&'static str> = Vec::new();

    // Issue-type signals (planner): the description intentionally carries no
    // text signal so the issue type alone is the trigger.
    for it in super::PLANNER_ISSUE_TYPE_SIGNALS {
        fixtures.push((it.to_string(), "plain worker description".to_string()));
        labels.push("issue-type");
    }
    // Planner text signals.
    for (needle, _) in super::PLANNER_ROLE_SIGNALS {
        fixtures.push((
            "task".to_string(),
            format!("this task requires {needle} to proceed"),
        ));
        labels.push("planner-text");
    }
    // Lead text signals.
    for (needle, _) in super::LEAD_ROLE_SIGNALS {
        fixtures.push((
            "task".to_string(),
            format!("this task requires {needle} to proceed"),
        ));
        labels.push("lead-text");
    }

    // Insert all fixtures.
    let mut expected_ids: Vec<String> = Vec::new();
    for (idx, (issue_type, description)) in fixtures.iter().enumerate() {
        let id = create_signal_fixture(
            &task_repo,
            &db,
            &project_id,
            &epic_id,
            &format!("{} fixture {idx}", labels[idx]),
            issue_type,
            description,
            3,
        )
        .await;
        expected_ids.push(id);
    }

    // Run the SQL prefilter.
    let candidates = super::list_board_health_mismatch_candidates(&task_repo)
        .await
        .unwrap();
    let candidate_ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();

    // Assert every Rust-positive fixture is returned by the SQL prefilter, and
    // that Rust inference is indeed positive on each returned fixture.
    for expected_id in &expected_ids {
        assert!(
            candidate_ids.contains(&expected_id.as_str()),
            "SQL prefilter must return every Rust-positive fixture; missing {expected_id}"
        );
    }
    for candidate in &candidates {
        if expected_ids.contains(&candidate.id) {
            assert!(
                super::infer_expected_role_for_task(candidate).is_some(),
                "Rust inference must be positive for fixture {} returned by SQL prefilter",
                candidate.id
            );
        }
    }
}

/// Both the SQL prefilter and Rust inference must match role signals regardless
/// of letter case (UPPER, MiXeD, lower).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatch_prefilter_matches_mixed_case_signals() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (project_id, epic_id) = setup_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    // UPPER-case issue type.
    let upper_issue = create_signal_fixture(
        &task_repo,
        &db,
        &project_id,
        &epic_id,
        "upper issue type",
        "PLANNING",
        "no text signals here",
        3,
    )
    .await;

    // MiXeD-cAsE issue type.
    let mixed_issue = create_signal_fixture(
        &task_repo,
        &db,
        &project_id,
        &epic_id,
        "mixed issue type",
        "DeCoMpOsItIoN",
        "no text signals here",
        3,
    )
    .await;

    // UPPER-case text signal.
    let upper_text = create_signal_fixture(
        &task_repo,
        &db,
        &project_id,
        &epic_id,
        "upper text signal",
        "task",
        "this needs TASK_CREATE to proceed",
        3,
    )
    .await;

    // MiXeD-cAsE text signal.
    let mixed_text = create_signal_fixture(
        &task_repo,
        &db,
        &project_id,
        &epic_id,
        "mixed text signal",
        "task",
        "this needs EpIc_UpDaTe to proceed",
        3,
    )
    .await;

    let candidates = super::list_board_health_mismatch_candidates(&task_repo)
        .await
        .unwrap();
    let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();

    for (id, label) in [
        (&upper_issue, "UPPER issue type"),
        (&mixed_issue, "MiXeD issue type"),
        (&upper_text, "UPPER text signal"),
        (&mixed_text, "MiXeD text signal"),
    ] {
        assert!(
            ids.contains(&id.as_str()),
            "mixed-case {label} must match the SQL prefilter"
        );
    }

    // Also verify Rust inference agrees on each mixed-case fixture.
    for candidate in &candidates {
        if [
            upper_issue.as_str(),
            mixed_issue.as_str(),
            upper_text.as_str(),
            mixed_text.as_str(),
        ]
        .contains(&candidate.id.as_str())
        {
            assert!(
                super::infer_expected_role_for_task(candidate).is_some(),
                "Rust inference must be positive for mixed-case fixture {}",
                candidate.id
            );
        }
    }
}

/// The `total_reopen_count >= 3` filter must admit rows at exactly the
/// threshold (3) and exclude rows below it (2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatch_prefilter_enforces_reopen_threshold() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (project_id, epic_id) = setup_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    let at_threshold = create_signal_fixture(
        &task_repo,
        &db,
        &project_id,
        &epic_id,
        "exactly at threshold",
        "task",
        "needs task_create",
        3,
    )
    .await;

    let below_threshold = create_signal_fixture(
        &task_repo,
        &db,
        &project_id,
        &epic_id,
        "below threshold",
        "task",
        "needs task_create",
        2,
    )
    .await;

    let candidates = super::list_board_health_mismatch_candidates(&task_repo)
        .await
        .unwrap();
    let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();

    assert!(
        ids.contains(&at_threshold.as_str()),
        "task at threshold (3 reopens) with a signal must be returned by the SQL prefilter"
    );
    assert!(
        !ids.contains(&below_threshold.as_str()),
        "task below threshold (2 reopens) must be excluded even though it carries a signal"
    );
}

/// A row with high reopen count, non-closed status, but no role-signal text or
/// issue type must NOT be returned — it is an impossible positive for both
/// layers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatch_prefilter_excludes_impossible_positive() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (project_id, epic_id) = setup_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    let impossible = create_signal_fixture(
        &task_repo,
        &db,
        &project_id,
        &epic_id,
        "no signals whatsoever",
        "task",
        "just a regular task with no special keywords at all",
        5,
    )
    .await;

    let candidates = super::list_board_health_mismatch_candidates(&task_repo)
        .await
        .unwrap();
    let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();

    assert!(
        !ids.contains(&impossible.as_str()),
        "impossible-positive row (no signals, high reopen count) must be excluded by the SQL prefilter"
    );

    // Double-check: Rust inference should also be None for this row's shape.
    // Construct a synthetic candidate to verify without needing the row back.
    let synthetic = super::BoardHealthMismatchCandidate {
        id: impossible.clone(),
        short_id: "test".to_string(),
        epic_id: Some(epic_id.clone()),
        title: "no signals whatsoever".to_string(),
        description: "just a regular task with no special keywords at all".to_string(),
        design: String::new(),
        acceptance_criteria: "[]".to_string(),
        issue_type: "task".to_string(),
        status: "open".to_string(),
        total_reopen_count: 5,
    };
    assert!(
        super::infer_expected_role_for_task(&synthetic).is_none(),
        "Rust inference must be None for an impossible-positive row"
    );
}

/// The review queue lists work WAITING for review; closed tasks are history,
/// and including them fetched the entire closed backlog on every call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_review_queue_excludes_closed_tasks() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (project_id, epic_id) = setup_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let creator_id = fixture_creator_id(&db).await;

    let waiting = task_repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator_id),
                source_task_id: None,
                proposal_id: None,
            },
            "waiting for review",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE tasks SET status = 'needs_task_review' WHERE id = $1",
        waiting.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    let done = task_repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator_id),
                source_task_id: None,
                proposal_id: None,
            },
            "already closed",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap();
    sqlx::query!("UPDATE tasks SET status = 'closed' WHERE id = $1", done.id)
        .execute(db.pool())
        .await
        .unwrap();

    let health = task_repo.board_health(24).await.unwrap();
    let queue = health["review_queue"]
        .as_array()
        .expect("review_queue is an array");
    let listed: Vec<&str> = queue.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(
        listed.contains(&waiting.id.as_str()),
        "a needs_task_review task must be in the review queue; got {listed:?}"
    );
    assert!(
        !listed.contains(&done.id.as_str()),
        "closed tasks must not be in the review queue; got {listed:?}"
    );
}
