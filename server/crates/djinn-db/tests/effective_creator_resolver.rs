//! Exhaustive behavioral tests for the transactional effective-creator
//! resolver and insert transaction.
//!
//! These tests prove the full precedence ladder (explicit/session, source-task
//! creator, parent-epic creator, proposal build owner, proposal author), the
//! failure rules (invalid explicit identity, provenance exhaustion), and the
//! transactional rollback/no-write guarantees — all against real Postgres with
//! real user/task/epic/proposal fixture rows. The created_by_user_id column is
//! still nullable, but every successful production-boundary insert MUST carry a
//! concrete value.

#![allow(clippy::too_many_arguments)]

use djinn_core::events::EventBus;
use djinn_db::repositories::user::UserRepository;
use djinn_db::{Database, EffectiveCreatorProvenance, TaskRepository};

const UNAVAILABLE: &str = "effective_creator_unavailable";

// ── Fixture helpers ───────────────────────────────────────────────────────

async fn seed_project(db: &Database) -> String {
    db.ensure_initialized().await.unwrap();
    let project_id = uuid::Uuid::now_v7().to_string();
    let repo_slug = format!("ecr-{project_id}");
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "ecr-proj",
        "ecr-org",
        repo_slug,
    )
    .execute(db.pool())
    .await
    .unwrap();
    project_id
}

async fn seed_user(db: &Database, github_id: i64, login: &str, is_member: bool) -> String {
    let user_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO users (id, github_id, github_login, is_member_of_org) VALUES ($1, $2, $3, $4)",
    )
    .bind(&user_id)
    .bind(github_id)
    .bind(login)
    .bind(is_member)
    .execute(db.pool())
    .await
    .unwrap();
    user_id
}

async fn seed_epic(
    db: &Database,
    project_id: &str,
    created_by: Option<&str>,
    proposal_id: Option<&str>,
) -> String {
    let epic_id = uuid::Uuid::now_v7().to_string();
    let short = format!("e{}", &epic_id[..12]);
    sqlx::query(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs, created_by_user_id, proposal_id)
         VALUES ($1, $2, $3, 'Epic', '', '', '', '', '[]'::jsonb, $4, $5)",
    )
    .bind(&epic_id)
    .bind(project_id)
    .bind(&short)
    .bind(created_by)
    .bind(proposal_id)
    .execute(db.pool())
    .await
    .unwrap();
    epic_id
}

async fn seed_proposal(db: &Database, build_owner: Option<&str>, author: Option<&str>) -> String {
    let proposal_id = uuid::Uuid::now_v7().to_string();
    let short = format!("p{}", &proposal_id[..12]);
    sqlx::query(
        "INSERT INTO proposals (id, short_id, title, body, status, author_user_id, build_owner_user_id)
         VALUES ($1, $2, 'Proposal', '', 'draft', $3, $4)",
    )
    .bind(&proposal_id)
    .bind(&short)
    .bind(author)
    .bind(build_owner)
    .execute(db.pool())
    .await
    .unwrap();
    proposal_id
}

async fn seed_source_task(repo: &TaskRepository, project_id: &str, creator: &str) -> String {
    repo.create_in_project_with_provenance(
        project_id,
        None,
        EffectiveCreatorProvenance {
            explicit_user_id: Some(creator),
            source_task_id: None,
            proposal_id: None,
        },
        "SourceTask",
        "",
        "",
        "task",
        0,
        "",
        None,
        None,
    )
    .await
    .unwrap()
    .id
}

async fn task_count(db: &Database) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn created_by(db: &Database, task_id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT created_by_user_id FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

// ── Precedence tier tests ─────────────────────────────────────────────────

/// Explicit identity wins over every fallback candidate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_identity_wins_over_all_fallbacks() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let explicit = seed_user(&db, 101, "explicit-wins", true).await;
    let source_creator = seed_user(&db, 102, "source-creator", true).await;
    let epic_creator = seed_user(&db, 103, "epic-creator", true).await;
    let build_owner = seed_user(&db, 104, "build-owner", true).await;
    let author = seed_user(&db, 105, "author", true).await;

    let source_task = seed_source_task(
        &TaskRepository::new(db.clone(), EventBus::noop()),
        &project_id,
        &source_creator,
    )
    .await;
    let proposal_id = seed_proposal(&db, Some(&build_owner), Some(&author)).await;
    let epic_id = seed_epic(&db, &project_id, Some(&epic_creator), Some(&proposal_id)).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&explicit),
                source_task_id: Some(&source_task),
                proposal_id: Some(&proposal_id),
            },
            "ExplicitWins",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(explicit),
        "explicit identity must win over all fallbacks"
    );
}

/// When explicit identity is absent, the source-task creator wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_task_creator_wins_when_no_explicit() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let source_creator = seed_user(&db, 201, "source-only", true).await;
    let epic_creator = seed_user(&db, 202, "epic-fallback", true).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let source_task = seed_source_task(&repo, &project_id, &source_creator).await;
    let epic_id = seed_epic(&db, &project_id, Some(&epic_creator), None).await;

    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: None,
                source_task_id: Some(&source_task),
                proposal_id: None,
            },
            "SourceWins",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(source_creator),
        "source-task creator must win when no explicit identity"
    );
}

/// When explicit and source-task are absent, the parent-epic creator wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_epic_creator_wins_when_no_explicit_or_source() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let epic_creator = seed_user(&db, 301, "epic-creator", true).await;
    let epic_id = seed_epic(&db, &project_id, Some(&epic_creator), None).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance::default(),
            "EpicWins",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(epic_creator),
        "parent-epic creator must win when no explicit/source"
    );
}

/// When explicit/source/epic-creator are absent, proposal build-owner wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_build_owner_wins_when_higher_tiers_absent() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let build_owner = seed_user(&db, 401, "build-owner", true).await;
    let author = seed_user(&db, 402, "author", true).await;
    let proposal_id = seed_proposal(&db, Some(&build_owner), Some(&author)).await;
    let epic_id = seed_epic(&db, &project_id, None, Some(&proposal_id)).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance::default(),
            "BuildOwnerWins",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(build_owner),
        "proposal build-owner must win when higher tiers are absent"
    );
}

/// When all higher tiers are absent and build-owner is missing, proposal author wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_author_wins_last() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let author = seed_user(&db, 501, "author-only", true).await;
    let proposal_id = seed_proposal(&db, None, Some(&author)).await;
    let epic_id = seed_epic(&db, &project_id, None, Some(&proposal_id)).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance::default(),
            "AuthorWins",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(author),
        "proposal author must win when build-owner is absent"
    );
}

/// Missing/absent source-task creator advances to epic creator.
/// The source task exists but its created_by_user_id is NULL (the FK prevents
/// inserting a reference to a non-existent user, so NULL represents the
/// "absent/invalid candidate" case).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_source_creator_advances_to_epic() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let epic_creator = seed_user(&db, 601, "epic-fallback-2", true).await;
    let epic_id = seed_epic(&db, &project_id, Some(&epic_creator), None).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let source_task = seed_source_task(&repo, &project_id, &epic_creator).await;
    // NULL out the source task's creator — the resolver must advance past it.
    sqlx::query("UPDATE tasks SET created_by_user_id = NULL WHERE id = $1")
        .bind(&source_task)
        .execute(db.pool())
        .await
        .unwrap();

    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: None,
                source_task_id: Some(&source_task),
                proposal_id: None,
            },
            "AdvancePastSource",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(epic_creator),
        "missing source creator must advance to epic creator"
    );
}

/// Missing/absent epic creator advances to proposal build-owner.
/// The epic's created_by_user_id is NULL (FK prevents ghost references), so
/// the resolver must advance past it to the proposal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_epic_creator_advances_to_proposal() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let build_owner = seed_user(&db, 701, "build-owner-2", true).await;
    let proposal_id = seed_proposal(&db, Some(&build_owner), None).await;
    // Epic has NO creator but is linked to the proposal.
    let epic_id = seed_epic(&db, &project_id, None, Some(&proposal_id)).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance::default(),
            "AdvancePastEpic",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(build_owner),
        "missing epic creator must advance to proposal build-owner"
    );
}

/// Source-task references a non-existent task id; resolver advances.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_existent_source_task_advances_to_epic() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let epic_creator = seed_user(&db, 801, "epic-creator-3", true).await;
    let epic_id = seed_epic(&db, &project_id, Some(&epic_creator), None).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: None,
                source_task_id: Some("does-not-exist-task-id"),
                proposal_id: None,
            },
            "NonExistentSource",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(epic_creator),
        "non-existent source task must advance to epic creator"
    );
}

// ── Failure rule tests ────────────────────────────────────────────────────

/// Invalid explicit identity fails immediately — no fallback, no write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_explicit_identity_fails_immediately() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let epic_creator = seed_user(&db, 901, "epic-would-work", true).await;
    let epic_id = seed_epic(&db, &project_id, Some(&epic_creator), None).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let before = task_count(&db).await;

    let err = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some("nonexistent-explicit-user"),
                source_task_id: None,
                proposal_id: None,
            },
            "InvalidExplicit",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains(UNAVAILABLE),
        "invalid explicit must return structured failure, got: {err}"
    );
    assert!(
        err.to_string().contains("invalid_explicit_identity"),
        "must flag invalid_explicit_identity, got: {err}"
    );
    assert_eq!(
        before,
        task_count(&db).await,
        "invalid explicit must leave no task row"
    );
}

/// A disabled/retained user (is_member_of_org = false) is a valid attribution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_retained_user_is_valid_attribution() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let retained = seed_user(&db, 1001, "retained-user", false).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&retained),
                source_task_id: None,
                proposal_id: None,
            },
            "RetainedUser",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(retained),
        "a disabled/retained user must be accepted as valid attribution"
    );
}

/// Total provenance exhaustion returns structured effective_creator_unavailable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn total_provenance_exhaustion_returns_structured_failure() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let epic_id = seed_epic(&db, &project_id, None, None).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let before = task_count(&db).await;

    let err = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance::default(),
            "Exhausted",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains(UNAVAILABLE),
        "exhaustion must return structured failure, got: {err}"
    );
    assert_eq!(
        before,
        task_count(&db).await,
        "exhaustion must leave no task row"
    );
}

/// Proposal with NULL build-owner and NULL author; provenance exhaustion.
/// The FK on proposals prevents inserting ghost references, so NULL represents
/// the "no attributable user on the proposal" case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_with_ghost_users_exhausts() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    // Both build_owner and author are NULL — no attributable user.
    let proposal_id = seed_proposal(&db, None, None).await;
    let epic_id = seed_epic(&db, &project_id, None, Some(&proposal_id)).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let err = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance::default(),
            "GhostProposal",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains(UNAVAILABLE),
        "ghost proposal users must exhaust, got: {err}"
    );
}

// ── No-NULL / concrete-creator guarantee ──────────────────────────────────

/// Every successful insert has a concrete created_by_user_id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_successful_insert_has_concrete_created_by() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let user = seed_user(&db, 1101, "concrete-user", true).await;
    let repo = TaskRepository::new(db.clone(), EventBus::noop());

    repo.create_in_project_with_provenance(
        &project_id,
        None,
        EffectiveCreatorProvenance {
            explicit_user_id: Some(&user),
            ..Default::default()
        },
        "ConcreteT1",
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

    let source_task = seed_source_task(&repo, &project_id, &user).await;
    repo.create_in_project_with_provenance(
        &project_id,
        None,
        EffectiveCreatorProvenance {
            source_task_id: Some(&source_task),
            ..Default::default()
        },
        "ConcreteT2",
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

    let null_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tasks WHERE title IN ('ConcreteT1','ConcreteT2') AND created_by_user_id IS NULL",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        null_count, 0,
        "no production-boundary task may have a NULL created_by_user_id"
    );
}

// ── Transactional rollback tests ──────────────────────────────────────────

/// A failed insert (FK violation on blockers) rolls back both the task row and
/// the blocker edge. The creator resolution is NOT retried under a lower-
/// precedence identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fk_failure_rolls_back_task_and_blocker_without_retry() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let explicit_user = seed_user(&db, 1201, "explicit-rollback", true).await;
    let epic_creator = seed_user(&db, 1202, "epic-rollback-fallback", true).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let blocker_id = seed_source_task(&repo, &project_id, &epic_creator).await;

    // Delete the blocker so the blocker-edge INSERT hits an FK violation
    // AFTER creator resolution succeeds.
    sqlx::query("DELETE FROM tasks WHERE id = $1")
        .bind(&blocker_id)
        .execute(db.pool())
        .await
        .unwrap();

    let before = task_count(&db).await;

    let err = repo
        .create_in_project_with_blockers(
            &project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&explicit_user),
                ..Default::default()
            },
            "RollbackTask",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
            std::slice::from_ref(&blocker_id),
        )
        .await
        .unwrap_err();

    assert!(
        !err.to_string().contains(UNAVAILABLE),
        "FK failure must not be masked as creator-unavailable, got: {err}"
    );
    assert_eq!(
        before,
        task_count(&db).await,
        "FK failure must roll back the task row"
    );

    let edge_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM blockers WHERE blocking_task_id = $1")
            .bind(&blocker_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(edge_count, 0, "FK failure must roll back the blocker edge");
}

/// When an explicit identity resolves but the INSERT fails, the resolver does
/// NOT fall back to a lower-precedence identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_insert_does_not_retry_under_lower_identity() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let explicit_user = seed_user(&db, 1301, "explicit-no-retry", true).await;
    let epic_creator = seed_user(&db, 1302, "epic-no-retry", true).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let epic_id = seed_epic(&db, &project_id, Some(&epic_creator), None).await;

    // Ghost project_id forces an INSERT FK failure after resolution succeeds.
    let ghost_project = "00000000-0000-7000-8000-000000000000";
    let before = task_count(&db).await;

    let result = repo
        .create_in_project_with_provenance(
            ghost_project,
            Some(&epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&explicit_user),
                ..Default::default()
            },
            "NoRetryTask",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "insert with ghost project must fail");
    assert_eq!(
        before,
        task_count(&db).await,
        "failed insert must not commit"
    );

    let retry_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE created_by_user_id = $1")
            .bind(&epic_creator)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        retry_count, 0,
        "resolver must not retry under a lower-precedence identity"
    );
}

// ── Proposal provenance via epic link ─────────────────────────────────────

/// Proposal provenance can reach the resolver via the epic's proposal_id link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_provenance_via_epic_link() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let build_owner = seed_user(&db, 1401, "build-owner-via-epic", true).await;
    let proposal_id = seed_proposal(&db, Some(&build_owner), None).await;
    let epic_id = seed_epic(&db, &project_id, None, Some(&proposal_id)).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance::default(),
            "ProposalViaEpic",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(build_owner),
        "proposal provenance must be discovered via the epic's proposal_id link"
    );
}

/// Explicit proposal_id in provenance takes precedence over the epic's link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_proposal_id_overrides_epic_link() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let build_owner_explicit = seed_user(&db, 1501, "explicit-build-owner", true).await;
    let build_owner_epic = seed_user(&db, 1502, "epic-build-owner", true).await;

    let explicit_proposal = seed_proposal(&db, Some(&build_owner_explicit), None).await;
    let epic_proposal = seed_proposal(&db, Some(&build_owner_epic), None).await;
    let epic_id = seed_epic(&db, &project_id, None, Some(&epic_proposal)).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = repo
        .create_in_project_with_provenance(
            &project_id,
            Some(&epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: None,
                source_task_id: None,
                proposal_id: Some(&explicit_proposal),
            },
            "ExplicitProposal",
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

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(build_owner_explicit),
        "explicit proposal_id must win over the epic's proposal link"
    );
}

// ── Session user (SESSION_USER_ID) ────────────────────────────────────────

/// SESSION_USER_ID acts as the explicit identity when no explicit_user_id is passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_user_acts_as_explicit_identity() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let session_user = UserRepository::new(db.clone())
        .upsert_from_github(1601, "session-ecr", Some("Session"), None)
        .await
        .unwrap();
    let epic_creator = seed_user(&db, 1602, "epic-session-fallback", true).await;
    let epic_id = seed_epic(&db, &project_id, Some(&epic_creator), None).await;

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(session_user.id.clone()), async {
            repo.create_in_project_with_provenance(
                &project_id,
                Some(&epic_id),
                EffectiveCreatorProvenance::default(),
                "SessionExplicit",
                "",
                "",
                "task",
                0,
                "",
                None,
                None,
            )
            .await
            .unwrap()
        })
        .await;

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(session_user.id),
        "SESSION_USER_ID must act as the explicit identity"
    );
}

/// Explicit user_id in provenance takes precedence over SESSION_USER_ID.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_overrides_session_user() {
    let db = Database::open_in_memory().unwrap();
    let project_id = seed_project(&db).await;

    let explicit_user = seed_user(&db, 1701, "explicit-over-session", true).await;
    let session_user = UserRepository::new(db.clone())
        .upsert_from_github(1702, "session-override", Some("Session"), None)
        .await
        .unwrap();

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(session_user.id.clone()), async {
            repo.create_in_project_with_provenance(
                &project_id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&explicit_user),
                    ..Default::default()
                },
                "ExplicitOverSession",
                "",
                "",
                "task",
                0,
                "",
                None,
                None,
            )
            .await
            .unwrap()
        })
        .await;

    assert_eq!(
        created_by(&db, &task.id).await,
        Some(explicit_user),
        "explicit_user_id in provenance must override SESSION_USER_ID"
    );
}
