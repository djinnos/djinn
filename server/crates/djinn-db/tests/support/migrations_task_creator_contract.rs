//! Shared database fixtures for the task-creator contract migration matrix.

use std::path::{Path, PathBuf};

use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};

pub(crate) const MIGRATION_VERSION: u64 = 142;
pub(crate) const MIGRATION_FILE: &str = "142_task_creator_contract.sql";
pub(crate) const DESIGNATED: &str = "00000000-0000-7000-8000-000000000099";

// ═══════════════════════════════════════════════════════════════════════════
// Infrastructure helpers
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) fn base_database_url() -> String {
    djinn_db::test_database_base_url()
}

pub(crate) fn server_prefix(base: &str) -> String {
    base.rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_owned()
}

pub(crate) fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres")
}

pub(crate) fn migration_entries(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut entries: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .expect("read migrations dir")
        .map(|entry| {
            let path = entry.expect("migration dir entry").path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let version = name
                .split_once('_')
                .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
                .unwrap_or(0);
            (version, path)
        })
        .filter(|(_, path)| path.extension().and_then(|e| e.to_str()) == Some("sql"))
        .collect();
    entries.sort_by(|(av, ap), (bv, bp)| {
        av.cmp(bv).then_with(|| {
            ap.file_name()
                .unwrap_or_default()
                .cmp(bp.file_name().unwrap_or_default())
        })
    });
    entries
}

pub(crate) async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = base_database_url();
    let prefix = server_prefix(&base);
    let db_name = format!(
        "djinn_creator_contract_{}_{}",
        suffix,
        uuid::Uuid::now_v7().simple()
    );
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect postgres admin database");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("create creator contract test database");
    drop(admin);

    let db_url = format!("{prefix}/{db_name}");
    let result = f(db_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect postgres admin database");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    let _ = admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{db_name}""#).as_str())
        .await;

    result
}

/// Apply every migration whose version prefix is strictly less than
/// `MIGRATION_VERSION` by reading the SQL files directly.
pub(crate) async fn apply_prior_migrations(conn: &mut PgConnection) {
    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        if version == 0 {
            continue;
        }
        let sql = std::fs::read_to_string(&path).expect("read migration sql");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|err| panic!("apply migration {} failed: {err}", path.display()));
    }
}

pub(crate) fn migration_sql() -> String {
    let migration = migrations_dir().join(MIGRATION_FILE);
    std::fs::read_to_string(&migration).expect("read contract migration sql")
}

/// Extract only the data-backfill CTE+UPDATE statement from the migration,
/// so rollback and idempotence tests can rerun the *real* data step rather
/// than a synthetic substitute.
pub(crate) fn migration_data_step_sql() -> String {
    let sql = migration_sql();
    let start = sql
        .find("WITH valid_source AS (")
        .expect("find data step WITH clause");
    let end = sql
        .find("-- ── 3.")
        .expect("find zero-NULL assertion marker");
    sql[start..end].trim().to_owned()
}

pub(crate) async fn set_operator(conn: &mut PgConnection, operator_id: &str) {
    sqlx::query("SELECT set_config('djinn.migration_designated_operator_user_id', $1, false)")
        .bind(operator_id)
        .execute(&mut *conn)
        .await
        .expect("set designated operator GUC");
}

pub(crate) async fn clear_operator(conn: &mut PgConnection) {
    sqlx::query("RESET djinn.migration_designated_operator_user_id")
        .execute(&mut *conn)
        .await
        .expect("reset designated operator GUC");
}

pub(crate) async fn apply_contract_migration(conn: &mut PgConnection) {
    let sql = migration_sql();
    conn.execute(sql.as_str())
        .await
        .expect("apply contract migration");
}

pub(crate) async fn column_is_nullable(conn: &mut PgConnection) -> bool {
    let nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns \
         WHERE table_name = 'tasks' AND column_name = 'created_by_user_id'",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("check column nullability");
    nullable == "YES"
}

pub(crate) async fn get_task_creator(conn: &mut PgConnection, task_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT created_by_user_id FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(&mut *conn)
        .await
        .expect("fetch task creator")
}

// ═══════════════════════════════════════════════════════════════════════════
// Seeding helpers
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) async fn seed_user(conn: &mut PgConnection, id: &str, disabled: bool) {
    let github_id: i64 = (id.bytes().fold(0i64, |acc, b| acc.wrapping_add(b as i64))).abs() + 1;
    sqlx::query(
        "INSERT INTO users (id, github_id, github_login, is_member_of_org) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(github_id)
    .bind(format!("login-{id}"))
    .bind(!disabled)
    .execute(&mut *conn)
    .await
    .expect("seed user");
}

pub(crate) async fn seed_project(conn: &mut PgConnection, id: &str) {
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ($1, $2, 'djinnos', $3)",
    )
    .bind(id)
    .bind(format!("project-{id}"))
    .bind(format!("djinn-{id}"))
    .execute(&mut *conn)
    .await
    .expect("seed project");
}

/// Minimal task insert: id, project, creator.
pub(crate) async fn seed_task(
    conn: &mut PgConnection,
    id: &str,
    project_id: &str,
    creator: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, \
          labels, acceptance_criteria, memory_refs, created_by_user_id) \
         VALUES ($1, $2, $3, 'title', 'desc', 'design', \
                 '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $4)",
    )
    .bind(id)
    .bind(project_id)
    .bind(format!("sid-{id}"))
    .bind(creator)
    .execute(&mut *conn)
    .await
    .expect("seed task");
}

/// Task insert with epic link and optional labels JSON string.
pub(crate) async fn seed_task_with_epic(
    conn: &mut PgConnection,
    id: &str,
    project_id: &str,
    epic_id: Option<&str>,
    creator: Option<&str>,
    labels_json: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, epic_id, title, description, design, \
          labels, acceptance_criteria, memory_refs, created_by_user_id) \
         VALUES ($1, $2, $3, $4, 'title', 'desc', 'design', \
                 COALESCE($5::jsonb, '[]'::jsonb), '[]'::jsonb, '[]'::jsonb, $6)",
    )
    .bind(id)
    .bind(project_id)
    .bind(format!("sid-{id}"))
    .bind(epic_id)
    .bind(labels_json)
    .bind(creator)
    .execute(&mut *conn)
    .await
    .expect("seed task with epic");
}

pub(crate) async fn seed_epic(
    conn: &mut PgConnection,
    id: &str,
    project_id: &str,
    creator: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO epics \
         (id, project_id, short_id, title, description, memory_refs, created_by_user_id) \
         VALUES ($1, $2, $3, 'epic', 'edesc', '[]'::jsonb, $4)",
    )
    .bind(id)
    .bind(project_id)
    .bind(format!("esid-{id}"))
    .bind(creator)
    .execute(&mut *conn)
    .await
    .expect("seed epic");
}

pub(crate) async fn seed_epic_with_proposal(
    conn: &mut PgConnection,
    id: &str,
    project_id: &str,
    proposal_id: Option<&str>,
    creator: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO epics \
         (id, project_id, short_id, title, description, memory_refs, \
          created_by_user_id, proposal_id) \
         VALUES ($1, $2, $3, 'epic', 'edesc', '[]'::jsonb, $4, $5)",
    )
    .bind(id)
    .bind(project_id)
    .bind(format!("esid-{id}"))
    .bind(creator)
    .bind(proposal_id)
    .execute(&mut *conn)
    .await
    .expect("seed epic with proposal");
}

pub(crate) async fn seed_proposal(
    conn: &mut PgConnection,
    id: &str,
    build_owner: Option<&str>,
    author: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO proposals (id, short_id, title, build_owner_user_id, author_user_id) \
         VALUES ($1, $2, 'proposal', $3, $4)",
    )
    .bind(id)
    .bind(format!("psid-{id}"))
    .bind(build_owner)
    .bind(author)
    .execute(&mut *conn)
    .await
    .expect("seed proposal");
}

pub(crate) async fn seed_audit_source_link(
    conn: &mut PgConnection,
    target_task_id: &str,
    source_task_id: &str,
    suffix: &str,
) {
    let frame_id = format!("frame-{suffix}");
    let mc_id = format!("mc-{suffix}");
    let sel_id = format!("sel-{suffix}");
    let sha = format!("sha-{suffix}");
    let policy_id = format!("policy-{suffix}");

    // Policy (FK target for frame). Use suffix-derived revision to avoid
    // uq_audit_sample_policies_project_rev conflicts when called twice.
    let revision: i32 = suffix.chars().fold(1i32, |acc, c| acc + (c as i32));
    sqlx::query(
        "INSERT INTO audit_sample_policies (id, project_id, revision, policy_json) \
         VALUES ($1, 'project-1', $2, '{}'::jsonb) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&policy_id)
    .bind(revision)
    .execute(&mut *conn)
    .await
    .expect("seed audit policy");

    sqlx::query(
        "INSERT INTO audit_sample_frames \
         (id, project_id, policy_id, window_start, window_end, sealed_at) \
         VALUES ($1, 'project-1', $2, $3, $4, '2020-01-02') \
         ON CONFLICT DO NOTHING",
    )
    .bind(&frame_id)
    .bind(&policy_id)
    .bind(format!("2020-01-0{suffix}"))
    .bind(format!("2020-02-0{suffix}"))
    .execute(&mut *conn)
    .await
    .expect("seed audit frame");

    sqlx::query(
        "INSERT INTO audit_merged_changes \
         (id, project_id, task_id, merge_commit_sha, merged_at) \
         VALUES ($1, 'project-1', $2, $3, '2020-01-01')",
    )
    .bind(&mc_id)
    .bind(source_task_id)
    .bind(&sha)
    .execute(&mut *conn)
    .await
    .expect("seed audit merged change");

    sqlx::query(
        "INSERT INTO audit_selections \
         (id, frame_id, merged_change_id, stratum, selected_position, \
          seed_commitment, audit_task_id) \
         VALUES ($1, $2, $3, 'unflagged_merged', 1, \
                 '0000000000000000000000000000000000000000000000000000000000000000', $4)",
    )
    .bind(&sel_id)
    .bind(&frame_id)
    .bind(&mc_id)
    .bind(target_task_id)
    .execute(&mut *conn)
    .await
    .expect("seed audit selection");
}

pub(crate) async fn seed_hold_blocker_link(
    conn: &mut PgConnection,
    target_task_id: &str,
    source_task_id: &str,
) {
    sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)")
        .bind(source_task_id)
        .bind(target_task_id)
        .execute(&mut *conn)
        .await
        .expect("seed blocker edge");
}
