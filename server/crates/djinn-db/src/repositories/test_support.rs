use std::path::{Path, PathBuf};

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Project;
use tokio::sync::broadcast;

use crate::database::Database;
use crate::repositories::note::NoteRepository;
use djinn_memory::Note;

/// Replace a material proposal head without its normal write-time validation.
///
/// This is exclusively for legacy-data fixtures: it keeps the current sequence
/// while changing both the live proposal and its immutable `spec_revision`
/// snapshot. Lifecycle audit rows at the same sequence are deliberately left
/// untouched.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn replace_legacy_proposal_head_for_test(
    db: &Database,
    proposal_id: &str,
    body: &str,
    body_format: &str,
) {
    db.ensure_initialized().await.unwrap();
    let mut transaction = db.pool().begin().await.unwrap();
    sqlx::query("UPDATE proposals SET body = $1, body_format = $2 WHERE id = $3")
        .bind(body)
        .bind(body_format)
        .bind(proposal_id)
        .execute(&mut *transaction)
        .await
        .expect("failed to replace legacy proposal head");
    sqlx::query(
        "UPDATE proposal_revisions SET body = $1, body_format = $2 \
         WHERE proposal_id = $3 \
           AND seq = (SELECT latest_revision_seq FROM proposals WHERE id = $3) \
           AND event_kind = 'spec_revision'",
    )
    .bind(body)
    .bind(body_format)
    .bind(proposal_id)
    .execute(&mut *transaction)
    .await
    .expect("failed to replace legacy material head snapshot");
    transaction.commit().await.unwrap();
}

/// Delete a proposal's lint cache rows to model a head written before lint
/// persistence was introduced.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn delete_proposal_lint_results_for_test(db: &Database, proposal_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("DELETE FROM proposal_revision_lint_results WHERE proposal_id = $1")
        .bind(proposal_id)
        .execute(db.pool())
        .await
        .expect("failed to delete proposal lint results");
}

/// Create a fresh FK-valid user for a raw latest-schema fixture.
/// UUIDv7-derived values avoid a repository-wide fixed test identity.
pub async fn seed_test_user(db: &Database) -> String {
    db.ensure_initialized().await.unwrap();
    let uuid = uuid::Uuid::now_v7();
    let id = uuid.to_string();
    let github_id = (uuid.as_u128() & i64::MAX as u128) as i64;
    let github_login = format!("fixture-{}", uuid.simple());
    sqlx::query("INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)")
        .bind(&id)
        .bind(github_id)
        .bind(&github_login)
        .execute(db.pool())
        .await
        .expect("failed to seed fixture user");
    id
}

// ── Seed helpers for usage-analytics route tests ─────────────────────────
// These insert rows directly via raw SQL so integration tests outside
// djinn-db can seed deterministic fixture data without going through the
// full service-layer code paths.

/// Parameters for [`seed_session_row`].
pub struct UsageTestSessionSeed<'a> {
    pub project_id: &'a str,
    pub model_id: &'a str,
    pub agent_type: &'a str,
    pub started_at: &'a str,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: Option<f64>,
    pub cost_basis: &'a str,
    pub task_id: Option<&'a str>,
}

/// Parameters for [`seed_task_row`].
pub struct UsageTestTaskSeed<'a> {
    pub project_id: &'a str,
    pub status: &'a str,
    pub close_reason: Option<&'a str>,
    pub total_reopen_count: i32,
}

/// Seed a task row directly into the database for integration-level
/// contract tests. Returns the generated task id.
pub async fn seed_task_row(db: &Database, seed: UsageTestTaskSeed<'_>) -> String {
    db.ensure_initialized().await.unwrap();
    let creator = seed_test_user(db).await;
    let task_uuid = uuid::Uuid::now_v7();
    let id = task_uuid.to_string();
    let compact_id = task_uuid.simple().to_string();
    let short_id = format!("task-{}-{}", &compact_id[..8], &compact_id[20..32]);
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, epic_id, title, description, design, \
          issue_type, status, priority, owner, labels, acceptance_criteria, \
          reopen_count, continuation_count, verification_failure_count, \
          total_reopen_count, \
          intervention_count, created_at, updated_at, closed_at, close_reason, \
          merge_commit_sha, memory_refs, merge_conflict_metadata, agent_type, pr_url, created_by_user_id) \
         VALUES ($1, $2, $3, NULL, 'test title', 'test desc', 'test design', \
                 'task', $4, 0, '', '[]', '[]', \
                 0, 0, 0, \
                 $5, \
                 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z', NULL, $6, \
                 NULL, '[]', NULL, NULL, NULL, $7)",
    )
    .bind(&id)
    .bind(seed.project_id)
    .bind(&short_id)
    .bind(seed.status)
    .bind(seed.total_reopen_count)
    .bind(seed.close_reason)
    .bind(&creator)
    .execute(db.pool())
    .await
    .expect("failed to seed task row");
    id
}

/// Seed one deterministic board-health mismatch candidate for coordinator
/// integration tests. Raw fixture SQL stays behind the `djinn-db` boundary.
pub async fn seed_board_health_mismatch_candidate(db: &Database, project_id: &str, task_id: &str) {
    db.ensure_initialized().await.unwrap();
    let creator = seed_test_user(db).await;
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, issue_type, status, \
          labels, acceptance_criteria, memory_refs, total_reopen_count, created_by_user_id) \
         VALUES ($1, $2, 'mismatch-storm', 'mismatch storm', 'requires task_create', \
                 '', 'task', 'open', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, 3, $3)",
    )
    .bind(task_id)
    .bind(project_id)
    .bind(&creator)
    .execute(db.pool())
    .await
    .expect("failed to seed board-health mismatch candidate");
}

/// Seed raw session rows directly into the database for integration-level
/// contract tests that need actual query results.
pub async fn seed_session_row(db: &Database, seed: UsageTestSessionSeed<'_>) {
    let id = uuid::Uuid::now_v7().to_string();
    seed_session_row_with_id(db, &id, seed).await;
}

/// Like [`seed_session_row`] but accepts an explicit session id so callers
/// can control the id (e.g. for boundary tests that need a known session id).
pub async fn seed_session_row_with_id(
    db: &Database,
    session_id: &str,
    seed: UsageTestSessionSeed<'_>,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "INSERT INTO sessions \
         (id, project_id, task_id, model_id, agent_type, status, \
          started_at, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, cost_usd, cost_basis) \
         VALUES ($1, $2, $3, $4, $5, 'completed', $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(session_id)
    .bind(seed.project_id)
    .bind(seed.task_id)
    .bind(seed.model_id)
    .bind(seed.agent_type)
    .bind(seed.started_at)
    .bind(seed.tokens_in)
    .bind(seed.tokens_out)
    .bind(seed.cache_read_tokens)
    .bind(seed.cache_write_tokens)
    .bind(seed.cost_usd)
    .bind(seed.cost_basis)
    .execute(db.pool())
    .await
    .expect("failed to seed session row");
}

/// Delete a session row for integration tests that must verify FK cascade
/// behavior without bypassing the database repository boundary.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn delete_session_row(db: &Database, session_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("DELETE FROM sessions WHERE id = $1")
        .bind(session_id)
        .execute(db.pool())
        .await
        .expect("failed to delete session row");
}

/// Seed a projectless global-chat session row for boundary/compaction tests.
///
/// Global chat sessions (`agent_type = 'chat'`) are user-scoped and exist
/// outside any project, so migration 15's `sessions_project_scope_by_agent_type`
/// CHECK requires `project_id IS NULL`. The `UsageTestSessionSeed` helper can
/// only express a non-null project id, so chat-session tests use this dedicated
/// seed instead. `cost_basis` is `'unpriced'` to satisfy migration 83's
/// `sessions_cost_basis_check`.
pub async fn seed_chat_session_row(db: &Database, session_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "INSERT INTO sessions \
         (id, project_id, task_id, model_id, agent_type, status, \
          started_at, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, cost_usd, cost_basis) \
         VALUES ($1, NULL, NULL, 'test-model', 'chat', 'completed', \
                 '2025-01-01T00:00:00Z', 0, 0, 0, 0, NULL, 'unpriced')",
    )
    .bind(session_id)
    .execute(db.pool())
    .await
    .expect("failed to seed chat session row");
}

/// Seed a project so that `project_id` FK constraints pass.
pub async fn seed_project(db: &Database, project_id: &str, name: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ($1, $2, 'test', $2) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(project_id)
    .bind(name)
    .execute(db.pool())
    .await
    .expect("failed to seed project");
}

/// Overwrite a debate-trail entry's `body_metadata`, bypassing the
/// evidence-findings validation enforced by `add_debate_trail_entry`.
/// Recovery tests use this to fabricate legacy/corrupt rows that can no
/// longer be written through the repository API.
///
/// **Not for production use.**  Panics on SQL errors.
pub async fn override_debate_trail_body_metadata(
    db: &Database,
    entry_id: &str,
    body_metadata: &serde_json::Value,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("UPDATE proposal_debate_trail SET body_metadata = $2 WHERE id = $1")
        .bind(entry_id)
        .bind(body_metadata)
        .execute(db.pool())
        .await
        .expect("failed to override debate trail body_metadata");
}

/// Drop a database table if it exists.  Test-fixture helper for
/// failure-injection tests that need to simulate a missing-table error
/// (e.g. the coordinator reentrance `blocker_lookup_error` test).
///
/// **Not for production use.**  Panics on SQL errors.
pub async fn drop_table_for_test(db: &Database, table_name: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(&format!("DROP TABLE IF EXISTS {table_name}"))
        .execute(db.pool())
        .await
        .unwrap();
}

/// Drop a database table if it exists, cascading to dependent constraints.
/// Test-fixture helper for failure-injection tests where the table is
/// referenced by foreign keys (e.g. `images` is referenced by
/// `projects.selected_image_id` with `ON DELETE RESTRICT`).
///
/// **Not for production use.**  Panics on SQL errors.
pub async fn drop_table_cascade_for_test(db: &Database, table_name: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(&format!("DROP TABLE IF EXISTS {table_name} CASCADE"))
        .execute(db.pool())
        .await
        .unwrap();
}

/// Make the `notes.confidence` column nullable and set one note's confidence
/// to NULL. Test-fixture helper for fail-open retrieval tests that need the
/// production note query to exclude one row while a trace/query mapping path
/// observes malformed data.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn nullify_note_confidence_for_test(db: &Database, note_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("ALTER TABLE notes ALTER COLUMN confidence DROP NOT NULL")
        .execute(db.pool())
        .await
        .expect("failed to make notes.confidence nullable");
    sqlx::query("UPDATE notes SET confidence = NULL WHERE id = $1")
        .bind(note_id)
        .execute(db.pool())
        .await
        .expect("failed to nullify note confidence");
}

/// Rename `notes.confidence` so note retrieval queries fail without dropping
/// the dependency-heavy `notes` table. Test-fixture helper for fail-open
/// retrieval tests that need both production and trace-candidate searches to
/// hit a schema error on an ephemeral database.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn rename_note_confidence_column_for_test(db: &Database) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("ALTER TABLE notes RENAME COLUMN confidence TO confidence_for_test")
        .execute(db.pool())
        .await
        .expect("failed to rename notes.confidence for test");
}

/// Add a test-only constraint that leaves existing `task_arbitrations` rows
/// readable but makes subsequent INSERTs fail.  This lets coordinator
/// regressions exercise the real `TaskArbitrationRepository::try_create` error
/// branch after `resolve_current_hold_cycle` has succeeded, without dropping the
/// table and turning the scenario into a read/hold-cycle-resolution failure.
///
/// **Not for production use.**  Panics on SQL errors.
pub async fn reject_new_task_arbitrations_for_test(db: &Database) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "ALTER TABLE task_arbitrations \
         ADD CONSTRAINT task_arbitrations_reject_insert_for_test \
         CHECK (false) NOT VALID",
    )
    .execute(db.pool())
    .await
    .expect("failed to add task_arbitrations reject-insert constraint");
}

/// Toggle a test-only trigger that rejects admission-journal transitions to
/// `create_in_flight`. This lets composition tests exercise an unavailable
/// durable CreateStarted transition through the database-owner boundary.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn reject_admission_create_started_for_test(db: &Database, reject: bool) {
    db.ensure_initialized().await.unwrap();
    if reject {
        sqlx::query(
            "CREATE FUNCTION reject_admission_create_started_for_test() RETURNS trigger AS $$ \
             BEGIN \
               IF NEW.state = 'create_in_flight' THEN \
                 RAISE EXCEPTION 'journal temporarily unavailable'; \
               END IF; \
               RETURN NEW; \
             END; \
             $$ LANGUAGE plpgsql",
        )
        .execute(db.pool())
        .await
        .expect("failed to create admission CreateStarted rejection function");
        sqlx::query(
            "CREATE TRIGGER reject_admission_create_started_for_test \
             BEFORE UPDATE ON admission_journal \
             FOR EACH ROW EXECUTE FUNCTION reject_admission_create_started_for_test()",
        )
        .execute(db.pool())
        .await
        .expect("failed to create admission CreateStarted rejection trigger");
    } else {
        sqlx::query("DROP TRIGGER reject_admission_create_started_for_test ON admission_journal")
            .execute(db.pool())
            .await
            .expect("failed to drop admission CreateStarted rejection trigger");
        sqlx::query("DROP FUNCTION reject_admission_create_started_for_test()")
            .execute(db.pool())
            .await
            .expect("failed to drop admission CreateStarted rejection function");
    }
}

/// Backdate a task's `updated_at` by a PostgreSQL `interval` string
/// (e.g. `'20 minutes'`).
///
/// Test-fixture helper: production `updated_at` is stamped automatically
/// by status transitions.  Used by coordinator orphan / zombie tests to
/// fabricate task timestamps that predate a session's `started_at`.
pub async fn backdate_task_updated_at(db: &Database, task_id: &str, interval: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "UPDATE tasks SET updated_at = to_char(
             now() AT TIME ZONE 'utc' - $1::interval,
             'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         WHERE id = $2",
    )
    .bind(interval)
    .bind(task_id)
    .execute(db.pool())
    .await
    .unwrap();
}

/// Backdate a `task_attempts` row's `created_at` by a PostgreSQL `interval`
/// string (e.g. `'1 hour'`).
///
/// Test-fixture helper for the coordinator's orphaned-pending-attempt reaper,
/// whose age threshold compares against `created_at`.
pub async fn backdate_task_attempt_created_at(db: &Database, attempt_id: &str, interval: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "UPDATE task_attempts SET created_at = to_char(
             now() AT TIME ZONE 'utc' - $1::interval,
             'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         WHERE id = $2",
    )
    .bind(interval)
    .bind(attempt_id)
    .execute(db.pool())
    .await
    .unwrap();
}

/// Close a task at an explicit timestamp. Test-fixture helper: production
/// `closed_at` is stamped automatically by terminal status transitions.
pub async fn close_task_at(db: &Database, task_id: &str, closed_at: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("UPDATE tasks SET status = 'closed', closed_at = $1 WHERE id = $2")
        .bind(closed_at)
        .bind(task_id)
        .execute(db.pool())
        .await
        .expect("failed to close task at timestamp");
}

/// Backdate a `coordinator_incarnations` row's `last_renewed_at` by a
/// PostgreSQL `interval` string (e.g. `'1 hour'`).
///
/// Test-fixture helper for the coordinator's orphaned-pending-attempt reaper,
/// which classifies orphans from the durable owner lease's expiry relative to
/// the orphan threshold.
pub async fn backdate_coordinator_incarnation_lease(
    db: &Database,
    incarnation_id: &str,
    interval: &str,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "UPDATE coordinator_incarnations SET last_renewed_at = to_char(
             now() AT TIME ZONE 'utc' - $1::interval,
             'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         WHERE id = $2",
    )
    .bind(interval)
    .bind(incarnation_id)
    .execute(db.pool())
    .await
    .unwrap();
}

/// Insert a pending `task_attempts` row with an arbitrary (possibly malformed)
/// `dispatch_owner_incarnation_id`, bypassing the repository's UUID validation.
/// Used by reaper evidence tests to seed a malformed-owner orphan.
pub async fn insert_pending_attempt_with_raw_owner(
    db: &Database,
    id: &str,
    task_id: &str,
    role: &str,
    dispatch_key: &str,
    owner_incarnation_id: &str,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        r#"INSERT INTO task_attempts (id, task_id, role, attempt_seq, dispatch_key, outcome,
           dispatch_owner_incarnation_id, dispatch_group_id)
           VALUES ($1, $2, $3, 1, $4, 'pending', $5, NULL)"#,
    )
    .bind(id)
    .bind(task_id)
    .bind(role)
    .bind(dispatch_key)
    .bind(owner_incarnation_id)
    .execute(db.pool())
    .await
    .unwrap();
}

/// Replace the `coordinator_incarnations` table with a view that returns the
/// specified row on the **first** query but returns no rows on all subsequent
/// queries. This simulates the incarnation row vanishing between the reaper's
/// `get()` call (which succeeds) and its `is_live()` call (which returns
/// `None`), exercising the ambiguous-owner classification branch.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn make_coordinator_incarnation_vanish_after_first_read(
    db: &Database,
    incarnation_id: &str,
    registered_at: &str,
    last_renewed_at: &str,
) {
    db.ensure_initialized().await.unwrap();
    // Drop the real table and replace it with a view backed by an access
    // counter sequence. The first SELECT from the view returns the row
    // (counter = 1); every subsequent SELECT returns nothing.
    sqlx::query("DROP TABLE IF EXISTS coordinator_incarnations")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DROP SEQUENCE IF EXISTS ci_access_count")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("CREATE SEQUENCE ci_access_count")
        .execute(db.pool())
        .await
        .unwrap();
    let sql = format!(
        r#"CREATE VIEW coordinator_incarnations AS
           SELECT '{incarnation_id}'::varchar AS id,
                  '{registered_at}'::varchar AS registered_at,
                  '{last_renewed_at}'::varchar AS last_renewed_at
           WHERE nextval('ci_access_count') = 1"#
    );
    sqlx::query(&sql).execute(db.pool()).await.unwrap();
}

/// Replace the `coordinator_incarnations` table with a view that returns the
/// specified row on the **first** query but **raises an error** on all
/// subsequent queries. This simulates the incarnation table becoming
/// unavailable between the reaper's `get()` call (which succeeds) and its
/// `is_live()` call (which returns `Err`), exercising the is_live lookup-error
/// classification branch.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn make_coordinator_incarnation_error_after_first_read(
    db: &Database,
    incarnation_id: &str,
    registered_at: &str,
    last_renewed_at: &str,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS coordinator_incarnations")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DROP SEQUENCE IF EXISTS ci_access_count")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("CREATE SEQUENCE ci_access_count")
        .execute(db.pool())
        .await
        .unwrap();
    // Guard function: admits the first SELECT and raises on every later one.
    sqlx::query(
        r#"CREATE OR REPLACE FUNCTION ci_error_guard() RETURNS boolean AS $$
           BEGIN
             IF nextval('ci_access_count') >= 2 THEN
               RAISE EXCEPTION 'simulated coordinator_incarnations lookup error';
             END IF;
             RETURN true;
           END;
           $$ LANGUAGE plpgsql"#,
    )
    .execute(db.pool())
    .await
    .unwrap();
    let sql = format!(
        r#"CREATE VIEW coordinator_incarnations AS
           SELECT '{incarnation_id}'::varchar AS id,
                  '{registered_at}'::varchar AS registered_at,
                  '{last_renewed_at}'::varchar AS last_renewed_at
           WHERE ci_error_guard()"#
    );
    sqlx::query(&sql).execute(db.pool()).await.unwrap();
}

/// Wire a blocker edge: `blocking_task_id` blocks `task_id`. Test-fixture helper.
pub async fn add_blocker_edge(db: &Database, task_id: &str, blocking_task_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)")
        .bind(task_id)
        .bind(blocking_task_id)
        .execute(db.pool())
        .await
        .expect("failed to add blocker edge");
}

pub fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
    let tx = tx.clone();
    EventBus::new(move |event| {
        let _ = tx.send(event);
    })
}

pub async fn make_project(db: &Database, path: &Path) -> Project {
    db.ensure_initialized().await.unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    let path_slug = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("root");
    let project_name = format!("test-project-{path_slug}-{id}");
    // Synthesize unique (owner, repo) coords; the actual filesystem
    // `path` argument is used only for downstream note fixtures, not
    // persisted.
    let owner = "test";
    let repo_slug = format!("{path_slug}-{id}");
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        id,
        project_name,
        owner,
        repo_slug,
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query_as!(
        Project,
        r#"SELECT id, name,
                  github_owner AS "github_owner!: String",
                  github_repo  AS "github_repo!: String",
                  created_at, target_branch,
                  auto_merge AS "auto_merge!: bool",
                  sync_enabled AS "sync_enabled!: bool",
                  sync_remote
           FROM projects WHERE id = $1"#,
        id
    )
    .fetch_one(db.pool())
    .await
    .unwrap()
}

#[derive(Clone, Debug)]
pub struct HousekeepingFixtureExpectedCounts {
    pub prune_associations: u64,
    pub flag_orphan_notes: u64,
    pub rebuild_missing_content_hashes: u64,
    pub repair_broken_wikilinks: u64,
    /// Number of notes the housekeeping tick's archive sweep is expected to
    /// flip from `active` to `archived` for this project. The shipped
    /// multi-project fixture only uses hand-written `reference` notes, so
    /// this stays at 0 in the default fixture — archive candidates live in
    /// the dedicated single-project archive aggregation test.
    pub archive_audit_candidates: u64,
}

#[derive(Clone, Debug)]
pub struct HousekeepingFixtureProject {
    pub project: Project,
    pub path: PathBuf,
    pub expected: HousekeepingFixtureExpectedCounts,
    pub orphan_note_id: String,
    pub repaired_source_note_id: String,
    pub repaired_target_note_id: String,
    pub legacy_hash_note_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct HousekeepingFixture {
    pub projects: Vec<HousekeepingFixtureProject>,
}

pub async fn build_multi_project_housekeeping_fixture(db: &Database) -> HousekeepingFixture {
    let tmp = crate::database::test_tempdir().unwrap();
    let root = tmp.keep();
    let project_one_path = root.join("project-one");
    let project_two_path = root.join("project-two");
    std::fs::create_dir_all(&project_one_path).unwrap();
    std::fs::create_dir_all(&project_two_path).unwrap();

    let project_one = make_project(db, &project_one_path).await;
    let project_two = make_project(db, &project_two_path).await;

    let (tx, _rx) = broadcast::channel(256);
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let project_one_stale_a = repo
        .create(
            &project_one.id,
            "Project One Stale A",
            "content one",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_stale_b = repo
        .create(
            &project_one.id,
            "Project One Stale B",
            "content two",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_recent_a = repo
        .create(
            &project_one.id,
            "Project One Recent A",
            "content three",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_recent_b = repo
        .create(
            &project_one.id,
            "Project One Recent B",
            "content four",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_orphan = repo
        .create(
            &project_one.id,
            "Project One Orphan",
            "orphan body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_linked_target = repo
        .create(
            &project_one.id,
            "Project One Linked Target",
            "linked body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _project_one_linked_source = repo
        .create(
            &project_one.id,
            "Project One Linked Source",
            &format!("links to [[{}]]", project_one_linked_target.title),
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_canonical_hash = repo
        .create_db_note(
            &project_one.id,
            "Project One Canonical Hash",
            "Alpha\r\nBeta\n",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_legacy_hash = repo
        .create_db_note(
            &project_one.id,
            "Project One Legacy Hash",
            " Alpha\nBeta ",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_repair_target = repo
        .create(
            &project_one.id,
            "Rust Ownership Guide",
            "Rust Ownership. Rust Ownership. Rust Ownership. Rust Ownership. Borrowing and lifetimes details.",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_repair_source = repo
        .create(
            &project_one.id,
            "Project One Broken Link Source",
            "Read [[Rust Ownership]] before editing.",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let project_two_stale_a = repo
        .create(
            &project_two.id,
            "Project Two Stale A",
            "content five",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_stale_b = repo
        .create(
            &project_two.id,
            "Project Two Stale B",
            "content six",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_recent_a = repo
        .create(
            &project_two.id,
            "Project Two Recent A",
            "content seven",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_recent_b = repo
        .create(
            &project_two.id,
            "Project Two Recent B",
            "content eight",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_orphan = repo
        .create(
            &project_two.id,
            "Project Two Orphan",
            "orphan body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_linked_target = repo
        .create(
            &project_two.id,
            "Project Two Linked Target",
            "linked body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _project_two_linked_source = repo
        .create(
            &project_two.id,
            "Project Two Linked Source",
            &format!("links to [[{}]]", project_two_linked_target.title),
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_canonical_hash = repo
        .create_db_note(
            &project_two.id,
            "Project Two Canonical Hash",
            "Gamma\r\nDelta\n",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_legacy_hash = repo
        .create_db_note(
            &project_two.id,
            "Project Two Legacy Hash",
            " Gamma\nDelta ",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_repair_target = repo
        .create(
            &project_two.id,
            "Async Runtime Guide",
            "Async Runtime. Async Runtime. Async Runtime. Async Runtime. Scheduling and executors details.",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_repair_source = repo
        .create(
            &project_two.id,
            "Project Two Broken Link Source",
            "Review [[Async Runtime]] before tuning workers.",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    repo.upsert_association(&project_one_stale_a.id, &project_one_stale_b.id, 1)
        .await
        .unwrap();
    repo.upsert_association(&project_one_recent_a.id, &project_one_recent_b.id, 6)
        .await
        .unwrap();
    repo.upsert_association(&project_two_stale_a.id, &project_two_stale_b.id, 1)
        .await
        .unwrap();
    repo.upsert_association(&project_two_recent_a.id, &project_two_recent_b.id, 6)
        .await
        .unwrap();

    sqlx::query!(
        r#"UPDATE note_associations
         SET last_co_access = to_char((now() at time zone 'utc') - interval '100 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
         WHERE (note_a_id = $1 AND note_b_id = $2)
            OR (note_a_id = $3 AND note_b_id = $4)"#,
        project_one_stale_a.id,
        project_one_stale_b.id,
        project_two_stale_a.id,
        project_two_stale_b.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query!(
        r#"UPDATE note_associations
         SET last_co_access = to_char((now() at time zone 'utc') - interval '1 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
         WHERE (note_a_id = $1 AND note_b_id = $2)
            OR (note_a_id = $3 AND note_b_id = $4)"#,
        project_one_recent_a.id,
        project_one_recent_b.id,
        project_two_recent_a.id,
        project_two_recent_b.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query!(
        r#"UPDATE notes
         SET last_accessed = to_char((now() at time zone 'utc') - interval '31 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), access_count = 0
         WHERE id IN ($1, $2, $3, $4)"#,
        project_one_orphan.id,
        project_one_linked_target.id,
        project_two_orphan.id,
        project_two_linked_target.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query!(
        "UPDATE notes SET content_hash = NULL WHERE id IN ($1, $2, $3, $4)",
        project_one_canonical_hash.id,
        project_one_legacy_hash.id,
        project_two_canonical_hash.id,
        project_two_legacy_hash.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    HousekeepingFixture {
        projects: vec![
            HousekeepingFixtureProject {
                project: project_one,
                path: project_one_path,
                expected: HousekeepingFixtureExpectedCounts {
                    prune_associations: 1,
                    flag_orphan_notes: 1,
                    rebuild_missing_content_hashes: 2,
                    repair_broken_wikilinks: 1,
                    archive_audit_candidates: 0,
                },
                orphan_note_id: project_one_orphan.id,
                repaired_source_note_id: project_one_repair_source.id,
                repaired_target_note_id: project_one_repair_target.id,
                legacy_hash_note_ids: vec![
                    project_one_canonical_hash.id,
                    project_one_legacy_hash.id,
                ],
            },
            HousekeepingFixtureProject {
                project: project_two,
                path: project_two_path,
                expected: HousekeepingFixtureExpectedCounts {
                    prune_associations: 1,
                    flag_orphan_notes: 1,
                    rebuild_missing_content_hashes: 2,
                    repair_broken_wikilinks: 1,
                    archive_audit_candidates: 0,
                },
                orphan_note_id: project_two_orphan.id,
                repaired_source_note_id: project_two_repair_source.id,
                repaired_target_note_id: project_two_repair_target.id,
                legacy_hash_note_ids: vec![
                    project_two_canonical_hash.id,
                    project_two_legacy_hash.id,
                ],
            },
        ],
    }
}

/// Ensure the `doctor_findings` table exists in the database. The test DB is
/// cloned from `djinn_test_template` which may not include the latest migration
/// if the template hasn't been rebuilt. Existing template tables are upgraded
/// with the current additive doctor-finding columns and indexes.
///
/// Placed inside `djinn-db::test_support` so all raw SQL stays within the
/// `djinn-db` crate boundary (enforced by the raw-SQL boundary CI check).
pub async fn ensure_doctor_findings_schema(db: &Database) {
    db.ensure_initialized().await.expect("db initialized");
    // Check if the table exists; create it if not.
    let exists: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'doctor_findings'",
    )
    .fetch_optional(db.pool())
    .await
    .expect("check doctor_findings existence");

    if !matches!(exists, Some((count,)) if count > 0) {
        // Apply the base migration SQL inline. The ALTER/index below bring an
        // older cloned template forward to the current doctor-finding schema.
        sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS doctor_findings (
            id                VARCHAR(36)  NOT NULL PRIMARY KEY,
            run_id            VARCHAR(64)  NULL,
            created_at        VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
            check_name        VARCHAR(255) NOT NULL,
            severity          VARCHAR(16)  NOT NULL,
            entity_ids        JSONB        NOT NULL DEFAULT '[]'::jsonb,
            evidence          JSONB        NOT NULL DEFAULT '{}'::jsonb,
            resolver_snapshot JSONB        NULL,
            detail            TEXT         NULL,
            deduplication_key VARCHAR(255) NULL,
            CONSTRAINT doctor_findings_severity_check
                CHECK (severity IN ('info', 'warn', 'critical'))
        )"#,
        )
        .execute(db.pool())
        .await
        .expect("create doctor_findings table");
    }

    sqlx::query(
        "ALTER TABLE doctor_findings ADD COLUMN IF NOT EXISTS deduplication_key VARCHAR(255) NULL",
    )
    .execute(db.pool())
    .await
    .expect("add doctor_findings.deduplication_key");

    sqlx::query("CREATE INDEX IF NOT EXISTS doctor_findings_created_at_idx ON doctor_findings (created_at DESC)")
        .execute(db.pool())
        .await
        .expect("create doctor_findings_created_at_idx");
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS doctor_findings_check_name_idx ON doctor_findings (check_name)",
    )
    .execute(db.pool())
    .await
    .expect("create doctor_findings_check_name_idx");
    sqlx::query("CREATE INDEX IF NOT EXISTS doctor_findings_check_name_created_at_idx ON doctor_findings (check_name, created_at DESC)")
        .execute(db.pool())
        .await
        .expect("create doctor_findings_check_name_created_at_idx");
    sqlx::query("CREATE INDEX IF NOT EXISTS doctor_findings_entity_ids_gin_idx ON doctor_findings USING GIN (entity_ids jsonb_path_ops)")
        .execute(db.pool())
        .await
        .expect("create doctor_findings_entity_ids_gin_idx");
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS doctor_findings_deduplication_key_unique ON doctor_findings (deduplication_key) WHERE deduplication_key IS NOT NULL")
        .execute(db.pool())
        .await
        .expect("create doctor_findings_deduplication_key_unique");
}

/// Overwrite the `encrypted_value` column of a credential row with arbitrary
/// raw bytes. Used by tests that need to simulate decryption failures (corrupt
/// or truncated ciphertext) without going through the encrypt/decrypt round-trip.
///
/// This is a **test-only** escape hatch — all production writes MUST go through
/// the `CredentialRepository` boundary in `djinn-provider`.
pub async fn corrupt_credential_encrypted_value(db: &Database, key_name: &str, raw_bytes: Vec<u8>) {
    sqlx::query("UPDATE credentials SET encrypted_value = $1 WHERE key_name = $2")
        .bind(raw_bytes)
        .bind(key_name)
        .execute(db.pool())
        .await
        .expect("corrupt_credential_encrypted_value: update failed");
}

// ── Seed helpers for memory-eval fixture loading ────────────────────────
// These insert rows with explicit timestamps, status, and confidence for
// the deterministic memory-eval benchmark.  The eval loader cannot use the
// standard repository `create` methods because those auto-generate
// timestamps and trigger wikilink indexing / event emission.

/// Insert an eval note with explicit timestamps, status, and confidence,
/// then fetch and return the resulting [`Note`] row.
///
/// **Not for production use.**  Used only by the memory-eval fixture loader.
#[allow(clippy::too_many_arguments)]
pub async fn seed_eval_note(
    db: &Database,
    id: &str,
    project_id: &str,
    permalink: &str,
    title: &str,
    note_type: &str,
    folder: &str,
    tags_json: &serde_json::Value,
    content: &str,
    retrieval_anchor: Option<&str>,
    content_hash: &str,
    created_at: &str,
    updated_at: &str,
    last_accessed: &str,
    status: &str,
    confidence: f64,
) -> Note {
    db.ensure_initialized().await.unwrap();
    let empty_scope: serde_json::Value = serde_json::json!([]);

    sqlx::query(
        r#"INSERT INTO notes
            (id, project_id, permalink, title, file_path,
             storage, note_type, folder, tags, content, retrieval_anchor,
             content_hash, scope_paths,
             created_at, updated_at, last_accessed,
             status, confidence, abstract, overview, access_count)
         VALUES ($1, $2, $3, $4, '',
                 'db', $5, $6, $7, $8, $9,
                 $10, $11,
                 $12, $13, $14,
                 $15, $16, NULL, NULL, 0)"#,
    )
    .bind(id)
    .bind(project_id)
    .bind(permalink)
    .bind(title)
    .bind(note_type)
    .bind(folder)
    .bind(tags_json)
    .bind(content)
    .bind(retrieval_anchor)
    .bind(content_hash)
    .bind(&empty_scope)
    .bind(created_at)
    .bind(updated_at)
    .bind(last_accessed)
    .bind(status)
    .bind(confidence)
    .execute(db.pool())
    .await
    .unwrap_or_else(|e| panic!("seed_eval_note: failed to insert note '{permalink}': {e}"));

    // Fetch the note back using the same SELECT projection the rest of
    // djinn-db uses (mirrors the `note_select_where_id!` macro).
    sqlx::query_as::<_, Note>(
        r#"SELECT id, project_id, permalink, title, file_path,
                  storage, note_type, folder, status, tags::text AS tags, content,
                  retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                  access_count, confidence, abstract as abstract_, overview,
                  scope_paths::text AS scope_paths
           FROM notes WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(db.pool())
    .await
    .unwrap_or_else(|e| panic!("seed_eval_note: failed to fetch note '{permalink}': {e}"))
}

/// Insert an eval epic row and return its id.
///
/// **Not for production use.**  Used only by the memory-eval fixture loader.
pub async fn seed_eval_epic(db: &Database, project_id: &str, title: &str) -> String {
    db.ensure_initialized().await.unwrap();
    let epic_id = uuid::Uuid::now_v7().to_string();
    let short_id = format!("ep-{}", &epic_id[epic_id.len() - 12..]);
    sqlx::query(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)\n         VALUES ($1, $2, $3, $4, '', '', '', '', '[]'::jsonb)",
    )
    .bind(&epic_id)
    .bind(project_id)
    .bind(&short_id)
    .bind(title)
    .execute(db.pool())
    .await
    .expect("seed_eval_epic: failed to create epic");
    epic_id
}

/// Insert an eval task with `memory_refs` pointing to note IDs (for
/// task-affinity scoring) and return the generated task id.
///
/// **Not for production use.**  Used only by the memory-eval fixture loader.
pub async fn seed_eval_task_with_memory_refs(
    db: &Database,
    project_id: &str,
    epic_id: &str,
    fixture_task_id: &str,
    memory_refs_json: &str,
) -> String {
    db.ensure_initialized().await.unwrap();
    let task_id = uuid::Uuid::now_v7().to_string();
    let short_id = format!(
        "eval-{}",
        fixture_task_id.chars().take(8).collect::<String>()
    );

    sqlx::query(
        r#"INSERT INTO tasks
            (id, project_id, short_id, epic_id, title, description, design,
             issue_type, priority, owner, status, continuation_count, memory_refs)
         VALUES ($1, $2, $3, $4, $5, '', '', 'task', 0, '', 'open', 0, $6::jsonb)"#,
    )
    .bind(&task_id)
    .bind(project_id)
    .bind(&short_id)
    .bind(epic_id)
    .bind(format!("Eval task {}", fixture_task_id))
    .bind(memory_refs_json)
    .execute(db.pool())
    .await
    .unwrap_or_else(|e| {
        panic!("seed_eval_task_with_memory_refs: failed to create task '{fixture_task_id}': {e}")
    });

    task_id
}
