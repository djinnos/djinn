//! Focused database tests for the transactional `resolve_effective_creator`
//! precedence ladder in `task/writes.rs`.
//!
//! Covers every precedence tier (explicit/session → source-task → parent-epic →
//! proposal build-owner → proposal author), invalid explicit identity, absent /
//! malformed fallback candidates, retained disabled users, transactional FK
//! failure, and structured rollback / no-write on exhaustion.

// Test helper: println used for diagnostics with --nocapture.
#![allow(clippy::print_stdout)]

use djinn_core::events::EventBus;
use djinn_db::EffectiveCreatorProvenance;
use djinn_db::TaskRepository;
use djinn_db::UserRepository;

const EFFECTIVE_CREATOR_UNAVAILABLE: &str = "effective_creator_unavailable";

// ── Helpers ─────────────────────────────────────────────────────────────────

async fn open_db() -> djinn_db::Database {
    let db = djinn_db::Database::open_in_memory().expect("open in-memory db");
    db.ensure_initialized().await.expect("init db");
    db
}

async fn seed_project(db: &djinn_db::Database) -> String {
    let project_id = uuid::Uuid::now_v7().to_string();
    let repo_slug = format!("ecr-resolver-{project_id}");
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "ecr-proj",
        "test-owner",
        repo_slug,
    )
    .execute(db.pool())
    .await
    .expect("insert project");
    project_id
}

/// Seed an epic with an optional `created_by_user_id` and optional
/// `proposal_id` link so the resolver's parent-epic tier can be exercised.
async fn seed_epic(
    db: &djinn_db::Database,
    project_id: &str,
    short_id: &str,
    created_by_user_id: Option<&str>,
    proposal_id: Option<&str>,
) -> String {
    let epic_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs, created_by_user_id, proposal_id)
         VALUES ($1, $2, $3, 'Epic', '', '', '', '', '[]'::jsonb, $4, $5)",
    )
    .bind(&epic_id)
    .bind(project_id)
    .bind(short_id)
    .bind(created_by_user_id)
    .bind(proposal_id)
    .execute(db.pool())
    .await
    .expect("insert epic");
    epic_id
}

async fn seed_user(db: &djinn_db::Database, github_id: i64, login: &str) -> String {
    UserRepository::new(db.clone())
        .upsert_from_github(github_id, login, Some(login), None)
        .await
        .expect("create user")
        .id
}

/// Set `is_member_of_org = false` on a user to simulate a disabled-but-retained
/// user row. The resolver must still accept this user as valid attribution.
async fn disable_user(db: &djinn_db::Database, user_id: &str) {
    sqlx::query("UPDATE users SET is_member_of_org = false WHERE id = $1")
        .bind(user_id)
        .execute(db.pool())
        .await
        .expect("disable user");
}

/// Seed a proposal with optional `build_owner_user_id` and `author_user_id`.
async fn seed_proposal(
    db: &djinn_db::Database,
    short_id: &str,
    build_owner_user_id: Option<&str>,
    author_user_id: Option<&str>,
) -> String {
    let proposal_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO proposals (id, short_id, title, body, acceptance_criteria, status, author_user_id, build_owner_user_id)
         VALUES ($1, $2, 'P', '', '[]'::jsonb, 'draft', $3, $4)",
    )
    .bind(&proposal_id)
    .bind(short_id)
    .bind(author_user_id)
    .bind(build_owner_user_id)
    .execute(db.pool())
    .await
    .expect("insert proposal");
    proposal_id
}

/// Read back the `created_by_user_id` for a task.
async fn created_by(db: &djinn_db::Database, task_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT created_by_user_id FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(db.pool())
        .await
        .expect("read created_by")
}

/// Count all tasks in the DB.
async fn task_count(db: &djinn_db::Database) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tasks")
        .fetch_one(db.pool())
        .await
        .expect("count tasks")
}

/// A provenance that has no explicit or fallback identity — guaranteed to
/// exhaust the resolution ladder.
const fn empty_provenance() -> EffectiveCreatorProvenance<'static> {
    EffectiveCreatorProvenance {
        explicit_user_id: None,
        source_task_id: None,
        proposal_id: None,
    }
}

fn repo(db: &djinn_db::Database) -> TaskRepository {
    TaskRepository::new(db.clone(), EventBus::noop())
}

async fn create(
    db: &djinn_db::Database,
    project_id: &str,
    epic_id: Option<&str>,
    provenance: EffectiveCreatorProvenance<'_>,
) -> Result<String, String> {
    repo(db)
        .create_in_project_with_provenance(
            project_id, epic_id, provenance, "T", "", "", "task", 0, "", None, None,
        )
        .await
        .map(|t| t.id)
        .map_err(|e| e.to_string())
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// Tier 1 — explicit authenticated/selected owner wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_user_id_takes_precedence() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let explicit = seed_user(&db, 1001, "explicit-user").await;
    let fallback = seed_user(&db, 1002, "fallback-user").await;

    // Even when a proposal_id would resolve to `fallback`, the explicit user wins.
    let proposal_id =
        seed_proposal(&db, "p01", Some(fallback.as_str()), Some(fallback.as_str())).await;

    let task_id = create(
        &db,
        &project_id,
        None,
        EffectiveCreatorProvenance {
            explicit_user_id: Some(explicit.as_str()),
            source_task_id: None,
            proposal_id: Some(&proposal_id),
        },
    )
    .await
    .expect("create must succeed");
    assert_eq!(
        created_by(&db, &task_id).await.as_deref(),
        Some(explicit.as_str())
    );
}

/// Tier 2 — source-task creator is used when no explicit user is set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_task_creator_is_used_when_no_explicit() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let source_creator = seed_user(&db, 2001, "source-creator").await;

    // Create the source task with the source_creator as created_by.
    let source_task_id = create(
        &db,
        &project_id,
        None,
        EffectiveCreatorProvenance {
            explicit_user_id: Some(source_creator.as_str()),
            source_task_id: None,
            proposal_id: None,
        },
    )
    .await
    .expect("create source task");

    // Now create a child task pointing at the source task for fallback.
    let child_id = create(
        &db,
        &project_id,
        None,
        EffectiveCreatorProvenance {
            explicit_user_id: None,
            source_task_id: Some(&source_task_id),
            proposal_id: None,
        },
    )
    .await
    .expect("create child task");
    assert_eq!(
        created_by(&db, &child_id).await.as_deref(),
        Some(source_creator.as_str())
    );
}

/// Tier 2 — a missing source task advances to the epic creator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_source_task_advances_to_next_tier() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let epic_creator = seed_user(&db, 3001, "epic-creator").await;
    let epic_id = seed_epic(&db, &project_id, "ep01", Some(epic_creator.as_str()), None).await;
    let missing_source_id = uuid::Uuid::now_v7().to_string();
    let child_id = create(
        &db,
        &project_id,
        Some(&epic_id),
        EffectiveCreatorProvenance {
            explicit_user_id: None,
            source_task_id: Some(&missing_source_id),
            proposal_id: None,
        },
    )
    .await
    .expect("create child task");
    assert_eq!(
        created_by(&db, &child_id).await.as_deref(),
        Some(epic_creator.as_str())
    );
}

/// Tier 3 — parent-epic creator is used when no explicit or source-task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_epic_creator_is_used_when_no_source_task() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let epic_creator = seed_user(&db, 4001, "epic-creator").await;
    let epic_id = seed_epic(&db, &project_id, "ep01", Some(epic_creator.as_str()), None).await;

    let task_id = create(
        &db,
        &project_id,
        Some(&epic_id),
        EffectiveCreatorProvenance {
            explicit_user_id: None,
            source_task_id: None,
            proposal_id: None,
        },
    )
    .await
    .expect("create task");
    assert_eq!(
        created_by(&db, &task_id).await.as_deref(),
        Some(epic_creator.as_str())
    );
}

/// Tier 4 — proposal build-owner is used when no explicit/source/epic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_build_owner_is_used_when_no_epic_creator() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let build_owner = seed_user(&db, 5001, "build-owner").await;
    let author = seed_user(&db, 5002, "author").await;
    // Epic with no creator but linked to a proposal.
    let proposal_id = seed_proposal(
        &db,
        "p01",
        Some(build_owner.as_str()),
        Some(author.as_str()),
    )
    .await;
    let epic_id = seed_epic(&db, &project_id, "ep01", None, Some(&proposal_id)).await;

    let task_id = create(
        &db,
        &project_id,
        Some(&epic_id),
        EffectiveCreatorProvenance {
            explicit_user_id: None,
            source_task_id: None,
            proposal_id: None,
        },
    )
    .await
    .expect("create task");
    assert_eq!(
        created_by(&db, &task_id).await.as_deref(),
        Some(build_owner.as_str()),
        "build-owner must take precedence over author"
    );
}

/// Tier 5 — proposal author is used when build-owner is absent/invalid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_author_is_used_when_build_owner_absent() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let author = seed_user(&db, 6001, "author").await;
    // Proposal with no build_owner but an author.
    let proposal_id = seed_proposal(&db, "p01", None, Some(author.as_str())).await;
    let epic_id = seed_epic(&db, &project_id, "ep01", None, Some(&proposal_id)).await;

    let task_id = create(
        &db,
        &project_id,
        Some(&epic_id),
        EffectiveCreatorProvenance {
            explicit_user_id: None,
            source_task_id: None,
            proposal_id: None,
        },
    )
    .await
    .expect("create task");
    assert_eq!(
        created_by(&db, &task_id).await.as_deref(),
        Some(author.as_str())
    );
}

/// Proposal provenance passed directly (no epic) also resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_id_in_provenance_resolves_build_owner_and_author() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let build_owner = seed_user(&db, 7001, "build-owner").await;
    let proposal_id = seed_proposal(&db, "p01", Some(build_owner.as_str()), None).await;

    // No epic — just a proposal_id in provenance.
    let task_id = create(
        &db,
        &project_id,
        None,
        EffectiveCreatorProvenance {
            explicit_user_id: None,
            source_task_id: None,
            proposal_id: Some(&proposal_id),
        },
    )
    .await
    .expect("create task");
    assert_eq!(
        created_by(&db, &task_id).await.as_deref(),
        Some(build_owner.as_str())
    );
}

/// Invalid explicit identity → immediate error, no task written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_explicit_identity_fails_immediately() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let before = task_count(&db).await;

    let err = create(
        &db,
        &project_id,
        None,
        EffectiveCreatorProvenance {
            explicit_user_id: Some("nonexistent-user-id"),
            source_task_id: None,
            proposal_id: None,
        },
    )
    .await
    .expect_err("must fail");
    assert!(
        err.contains(EFFECTIVE_CREATOR_UNAVAILABLE),
        "error must mention effective_creator_unavailable: {err}"
    );
    assert!(
        err.contains("invalid_explicit_identity"),
        "error must mention invalid_explicit_identity: {err}"
    );
    assert_eq!(
        task_count(&db).await,
        before,
        "no task must be committed on failure"
    );
}

/// Exhausted resolution ladder → structured error, no task written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_resolution_returns_structured_error_no_write() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let before = task_count(&db).await;

    let err = create(&db, &project_id, None, empty_provenance())
        .await
        .expect_err("must fail");
    assert!(
        err.contains(EFFECTIVE_CREATOR_UNAVAILABLE),
        "error must mention effective_creator_unavailable: {err}"
    );
    assert_eq!(
        task_count(&db).await,
        before,
        "no task must be committed on failure"
    );
}

/// Missing / absent fallback candidates: epic has no creator, no proposal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_fallback_candidates_exhaust_resolution() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    // Epic with NULL creator and no proposal.
    let epic_id = seed_epic(&db, &project_id, "ep01", None, None).await;
    let before = task_count(&db).await;

    let err = create(
        &db,
        &project_id,
        Some(&epic_id),
        EffectiveCreatorProvenance {
            explicit_user_id: None,
            source_task_id: None,
            proposal_id: None,
        },
    )
    .await
    .expect_err("must fail");
    assert!(err.contains(EFFECTIVE_CREATOR_UNAVAILABLE));
    assert_eq!(task_count(&db).await, before);
}

/// Malformed fallback: source task id does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_source_task_id_advances_past_tier() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let epic_creator = seed_user(&db, 8001, "epic-creator").await;
    let epic_id = seed_epic(&db, &project_id, "ep01", Some(epic_creator.as_str()), None).await;

    // A source_task_id that doesn't exist in the DB should not crash — it
    // returns None and advances to the next tier (epic creator).
    let task_id = create(
        &db,
        &project_id,
        Some(&epic_id),
        EffectiveCreatorProvenance {
            explicit_user_id: None,
            source_task_id: Some("nonexistent-source-task"),
            proposal_id: None,
        },
    )
    .await
    .expect("must succeed via epic fallback");
    assert_eq!(
        created_by(&db, &task_id).await.as_deref(),
        Some(epic_creator.as_str())
    );
}

/// Malformed fallback: proposal id doesn't exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_proposal_id_advances_past_tier() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;

    // A proposal_id that doesn't exist should return None and fail.
    let err = create(
        &db,
        &project_id,
        None,
        EffectiveCreatorProvenance {
            explicit_user_id: None,
            source_task_id: None,
            proposal_id: Some("nonexistent-proposal"),
        },
    )
    .await
    .expect_err("must fail — no valid fallback");
    assert!(err.contains(EFFECTIVE_CREATOR_UNAVAILABLE));
}

/// Retained disabled users are valid attribution (existence, not org membership).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_disabled_user_is_valid_attribution() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let user_id = seed_user(&db, 9001, "disabled-user").await;
    disable_user(&db, &user_id).await;

    let task_id = create(
        &db,
        &project_id,
        None,
        EffectiveCreatorProvenance {
            explicit_user_id: Some(user_id.as_str()),
            source_task_id: None,
            proposal_id: None,
        },
    )
    .await
    .expect("disabled user must be valid");
    assert_eq!(
        created_by(&db, &task_id).await.as_deref(),
        Some(user_id.as_str())
    );
}

/// Retained disabled user as an epic creator fallback is also valid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_disabled_epic_creator_is_valid_fallback() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let disabled_creator = seed_user(&db, 9002, "disabled-epic-owner").await;
    disable_user(&db, &disabled_creator).await;
    let epic_id = seed_epic(
        &db,
        &project_id,
        "ep01",
        Some(disabled_creator.as_str()),
        None,
    )
    .await;

    let task_id = create(
        &db,
        &project_id,
        Some(&epic_id),
        EffectiveCreatorProvenance {
            explicit_user_id: None,
            source_task_id: None,
            proposal_id: None,
        },
    )
    .await
    .expect("disabled epic creator must be valid fallback");
    assert_eq!(
        created_by(&db, &task_id).await.as_deref(),
        Some(disabled_creator.as_str())
    );
}

/// Transactional FK failure: if the INSERT transaction rolls back (the
/// blocker edge references a non-existent task), no task is committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fk_transaction_failure_rolls_back_no_write() {
    let db = open_db().await;
    let project_id = seed_project(&db).await;
    let user_id = seed_user(&db, 9101, "creator").await;
    let before = task_count(&db).await;

    // The blocker edge references a non-existent task id — the FK constraint on
    // blockers.blocking_task_id → tasks(id) will fail inside the transaction,
    // rolling back the task INSERT that already ran within the same tx.
    let _err = repo(&db)
        .create_in_project_with_blockers(
            &project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(user_id.as_str()),
                source_task_id: None,
                proposal_id: None,
            },
            "FK-fail",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
            &["nonexistent-blocking-task-id".to_owned()],
        )
        .await
        .expect_err("FK violation must fail");

    assert_eq!(
        task_count(&db).await,
        before,
        "FK failure must roll back — no task committed"
    );
}
