//! Compatibility and migration-boundary proof for the dormant direct-delivery schema.

use std::borrow::Cow;

use djinn_db::{Database, DirectDeliveryCapabilityRepository, DirectDeliverySchemaCapability};
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};

const DIRECT_DELIVERY_MIGRATION_VERSION: i64 = 203;
const MIGRATION_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000203";

async fn delivery_state_counts(pool: &sqlx::PgPool) -> (i64, i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM tasks), (SELECT count(*) FROM proposal_build_attempts), (SELECT count(*) FROM task_deliveries)",
    )
    .fetch_one(pool)
    .await
    .expect("snapshot direct-delivery state")
}

/// Put a legacy task plus inert direct rows in the fixture so a failed probe
/// has meaningful task, attempt, and ledger state that it must preserve.
async fn seed_probe_state(db: &Database) {
    sqlx::query("INSERT INTO projects (id, name) VALUES ('probe-project', 'probe-project')")
        .execute(db.pool()).await.expect("seed probe project");
    sqlx::query("INSERT INTO tasks (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs) VALUES ('probe-task', 'probe-project', 'probe-task', 'task', '', '', '[]', '[]', '[]')")
        .execute(db.pool()).await.expect("seed probe task");
    sqlx::query("INSERT INTO proposals (id, short_id, title) VALUES ('probe-proposal', 'probe-proposal', 'proposal')")
        .execute(db.pool()).await.expect("seed probe proposal");
    sqlx::query("INSERT INTO proposal_build_attempts (id, proposal_id, short_id, lifecycle, base_sha, branch_name) VALUES ('probe-attempt', 'probe-proposal', 'probe-attempt', 'reserved', 'base', 'proposal/probe/attempt')")
        .execute(db.pool()).await.expect("seed probe attempt");
    sqlx::query("INSERT INTO task_deliveries (build_attempt_id, task_id, delivery_generation, state, candidate_sha, base_sha) VALUES ('probe-attempt', 'probe-task', 1, 'prepared', 'candidate', 'base')")
        .execute(db.pool()).await.expect("seed probe delivery");
}

#[tokio::test]
async fn disabled_epoch_preserves_legacy_task_pr_delivery_and_probe_is_read_only() {
    let db = Database::ephemeral().await.unwrap();
    sqlx::query("INSERT INTO projects (id, name) VALUES ('direct-delivery-project', 'direct-delivery-project')")
        .execute(db.pool()).await.unwrap();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, pr_url) \
         VALUES ('legacy-delivery-task', 'direct-delivery-project', 'legacy-delivery', 'title', 'description', 'design', '[]', '[]', '[]', 'https://example.test/legacy/pr/7')",
    ).execute(db.pool()).await.unwrap();
    let before = delivery_state_counts(db.pool()).await;

    assert!(matches!(
        DirectDeliveryCapabilityRepository::new(db.clone()).probe().await.unwrap(),
        DirectDeliverySchemaCapability::SupportedDisabled { ref epoch }
            if epoch.generation == 0 && !epoch.permits_direct_delivery()
    ));
    assert_eq!(delivery_state_counts(db.pool()).await, before, "the capability probe must not mutate delivery state");
    let pr_url: Option<String> = sqlx::query_scalar("SELECT pr_url FROM tasks WHERE id = 'legacy-delivery-task'")
        .fetch_one(db.pool()).await.unwrap();
    assert_eq!(pr_url.as_deref(), Some("https://example.test/legacy/pr/7"));
}

#[tokio::test]
async fn failing_capability_probes_are_read_only() {
    let missing = Database::ephemeral().await.unwrap();
    seed_probe_state(&missing).await;
    sqlx::query("DROP TABLE direct_delivery_leases").execute(missing.pool()).await.unwrap();
    let before_missing = delivery_state_counts(missing.pool()).await;
    assert!(matches!(
        DirectDeliveryCapabilityRepository::new(missing.clone()).probe().await.unwrap(),
        DirectDeliverySchemaCapability::MissingSchema { missing_relations }
            if missing_relations == ["direct_delivery_leases"]
    ));
    assert_eq!(delivery_state_counts(missing.pool()).await, before_missing, "missing-schema probe must not mutate state");

    let absent = Database::ephemeral().await.unwrap();
    seed_probe_state(&absent).await;
    sqlx::query("DELETE FROM direct_delivery_epochs").execute(absent.pool()).await.unwrap();
    let before_absent = delivery_state_counts(absent.pool()).await;
    assert!(matches!(
        DirectDeliveryCapabilityRepository::new(absent.clone()).probe().await.unwrap(),
        DirectDeliverySchemaCapability::MissingEpoch
    ));
    assert_eq!(delivery_state_counts(absent.pool()).await, before_absent, "missing-epoch probe must not mutate state");

    let unknown = Database::ephemeral().await.unwrap();
    seed_probe_state(&unknown).await;
    sqlx::query("ALTER TABLE direct_delivery_epochs DROP CONSTRAINT direct_delivery_epochs_state_check")
        .execute(unknown.pool()).await.unwrap();
    sqlx::query("UPDATE direct_delivery_epochs SET state = 'unknown'").execute(unknown.pool()).await.unwrap();
    let before_unknown = delivery_state_counts(unknown.pool()).await;
    assert!(matches!(
        DirectDeliveryCapabilityRepository::new(unknown.clone()).probe().await.unwrap(),
        DirectDeliverySchemaCapability::UnknownEpochState { state, generation: 0 }
            if state == "unknown"
    ));
    assert_eq!(delivery_state_counts(unknown.pool()).await, before_unknown, "unknown-epoch probe must not mutate state");
}

fn server_prefix(base: &str) -> String {
    base.rsplit_once('/').map(|(prefix, _)| prefix).unwrap_or(base).trim_end_matches('/').to_owned()
}

async fn with_pre_203_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where Fut: std::future::Future<Output = T>, {
    let prefix = server_prefix(&djinn_db::test_database_base_url());
    let database_name = format!("djinn_direct_delivery_{suffix}_{}", uuid::Uuid::now_v7().simple());
    let admin_url = format!("{prefix}/postgres");
    let database_url = format!("{prefix}/{database_name}");
    let mut admin = PgConnection::connect(&admin_url).await.expect("connect fixture admin");
    admin.execute(format!(r#"CREATE DATABASE "{database_name}""#).as_str()).await.expect("create fixture database");
    admin.close().await.expect("close fixture admin");

    djinn_db::migrations::bootstrap_designated_operator(&database_url, &djinn_db::migrations::DesignatedOperatorBootstrap {
        user_id: MIGRATION_OPERATOR_ID.to_owned(), github_id: 9_000_000_203,
        github_login: "direct-delivery-migration-operator".to_owned(), github_name: None, github_avatar_url: None,
    }).await.expect("bootstrap pre-203 fixture");
    let mut connection = PgConnection::connect(&database_url).await.expect("connect fixture database");
    sqlx::query("SELECT set_config('djinn.migration_designated_operator_user_id', $1, false)")
        .bind(MIGRATION_OPERATOR_ID).execute(&mut connection).await.expect("configure migration operator");
    let embedded = sqlx::migrate!("./migrations_postgres");
    let pre_203 = sqlx::migrate::Migrator {
        migrations: Cow::Owned(embedded.migrations.iter().filter(|migration| migration.version < DIRECT_DELIVERY_MIGRATION_VERSION).cloned().collect()),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    pre_203.run_direct(&mut connection).await.expect("apply migrations through 202");
    connection.close().await.expect("close pre-203 fixture connection");

    let result = f(database_url).await;
    let mut admin = PgConnection::connect(&admin_url).await.expect("reconnect fixture admin");
    let _ = admin.execute(format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{database_name}' AND pid <> pg_backend_pid()").as_str()).await;
    admin.execute(format!(r#"DROP DATABASE IF EXISTS "{database_name}""#).as_str()).await.expect("drop fixture database");
    result
}

async fn seed_project(conn: &mut PgConnection) {
    conn.execute("INSERT INTO projects (id, name, github_owner, github_repo) VALUES ('owner-project', 'owner-project', 'owner', 'direct-delivery')").await.expect("seed project");
}
async fn seed_proposal(conn: &mut PgConnection, id: &str) {
    sqlx::query("INSERT INTO proposals (id, short_id, title) VALUES ($1, $2, 'proposal')")
        .bind(id).bind(format!("short-{id}")).execute(&mut *conn).await.expect("seed proposal");
}
async fn seed_epic(conn: &mut PgConnection, id: &str, proposal_id: &str) {
    sqlx::query("INSERT INTO epics (id, project_id, short_id, title, description, memory_refs, created_by_user_id, proposal_id) VALUES ($1, 'owner-project', $2, 'epic', '', '[]', $3, $4)")
        .bind(id).bind(format!("short-{id}")).bind(MIGRATION_OPERATOR_ID).bind(proposal_id).execute(&mut *conn).await.expect("seed epic");
}
async fn seed_open_task(conn: &mut PgConnection, id: &str, epic_id: Option<&str>) {
    sqlx::query("INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) VALUES ($1, 'owner-project', $2, $3, 'task', '', '', '[]', '[]', '[]', $4)")
        .bind(id).bind(format!("short-{id}")).bind(epic_id).bind(MIGRATION_OPERATOR_ID).execute(&mut *conn).await.expect("seed open task");
}
fn migration_203_sql() -> String {
    std::fs::read_to_string(format!("{}/migrations_postgres/203_direct_delivery_v1.sql", env!("CARGO_MANIFEST_DIR"))).expect("read migration 203")
}

#[tokio::test]
async fn migration_203_accepts_epic_owners_and_breakdown_only_fallbacks() {
    with_pre_203_database("valid_owners", |database_url| async move {
        let mut conn = PgConnection::connect(&database_url).await.expect("connect fixture");
        seed_project(&mut conn).await;
        seed_proposal(&mut conn, "epic-proposal").await;
        seed_proposal(&mut conn, "breakdown-proposal").await;
        seed_epic(&mut conn, "owner-epic", "epic-proposal").await;
        seed_open_task(&mut conn, "epic-owned-task", Some("owner-epic")).await;
        seed_open_task(&mut conn, "breakdown-only-task", None).await;
        sqlx::query("UPDATE proposals SET build_breakdown_task_id = 'breakdown-only-task' WHERE id = 'breakdown-proposal'").execute(&mut conn).await.expect("link breakdown task");

        conn.execute(migration_203_sql().as_str()).await.expect("migration accepts unambiguous owners");
        let owners: Vec<(String, String)> = sqlx::query_as("SELECT t.id, COALESCE(e.proposal_id, p.id) FROM tasks t LEFT JOIN epics e ON e.id = t.epic_id LEFT JOIN proposals p ON p.build_breakdown_task_id = t.id WHERE t.id IN ('epic-owned-task', 'breakdown-only-task') ORDER BY t.id")
            .fetch_all(&mut conn).await.expect("read classified owners");
        assert_eq!(owners, vec![("breakdown-only-task".to_owned(), "breakdown-proposal".to_owned()), ("epic-owned-task".to_owned(), "epic-proposal".to_owned())]);
        let epoch: (String, i64) = sqlx::query_as("SELECT state, generation FROM direct_delivery_epochs WHERE name = 'direct_delivery_v1'").fetch_one(&mut conn).await.expect("read default epoch");
        assert_eq!(epoch, ("disabled".to_owned(), 0));
    }).await;
}

#[tokio::test]
async fn migration_203_reports_every_ambiguous_open_task_and_rolls_back() {
    with_pre_203_database("ambiguous_owners", |database_url| async move {
        let mut conn = PgConnection::connect(&database_url).await.expect("connect fixture");
        seed_project(&mut conn).await;
        for proposal in ["epic-a", "fallback-a", "epic-b", "fallback-b"] { seed_proposal(&mut conn, proposal).await; }
        seed_epic(&mut conn, "epic-a-row", "epic-a").await;
        seed_epic(&mut conn, "epic-b-row", "epic-b").await;
        seed_open_task(&mut conn, "ambiguous-task-a", Some("epic-a-row")).await;
        seed_open_task(&mut conn, "ambiguous-task-b", Some("epic-b-row")).await;
        sqlx::query("UPDATE proposals SET build_breakdown_task_id = $1 WHERE id = $2").bind("ambiguous-task-a").bind("fallback-a").execute(&mut conn).await.expect("link first conflicting fallback");
        sqlx::query("UPDATE proposals SET build_breakdown_task_id = $1 WHERE id = $2").bind("ambiguous-task-b").bind("fallback-b").execute(&mut conn).await.expect("link second conflicting fallback");
        let legacy_task_count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks").fetch_one(&mut conn).await.expect("snapshot legacy tasks");

        let mut transaction = conn.begin().await.expect("start migration transaction");
        let migration = migration_203_sql();
        // Raw SQL is required: this migration contains a DO block and several
        // statements, which PostgreSQL cannot execute as one prepared query.
        let error = transaction
            .execute(migration.as_str())
            .await
            .expect_err("ambiguous owners reject migration");
        let report = error.to_string();
        assert!(report.contains("direct_delivery_v1 migration cannot classify ambiguous open task owner(s)"));
        for task in ["ambiguous-task-a (short-ambiguous-task-a)", "ambiguous-task-b (short-ambiguous-task-b)"] { assert!(report.contains(task), "row-level report omitted {task}: {report}"); }
        transaction.rollback().await.expect("rollback failed migration");
        let relations: i64 = sqlx::query_scalar("SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public' AND table_name IN ('proposal_build_attempts', 'direct_delivery_epochs', 'task_deliveries', 'direct_delivery_leases')").fetch_one(&mut conn).await.expect("inspect rollback schema");
        assert_eq!(relations, 0, "failed migration must atomically roll back every direct-delivery relation");
        let task_count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks").fetch_one(&mut conn).await.expect("read legacy tasks after rollback");
        assert_eq!(task_count, legacy_task_count, "failed migration must preserve legacy rows");
    }).await;
}
