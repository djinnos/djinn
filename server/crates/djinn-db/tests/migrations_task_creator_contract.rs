//! Compact, row-auditable live-Postgres proof for the creator-contract migration.
//! All fixture rows are valid under the pre-contract schema; disappearance is
//! modeled only by legal deletes and their documented ON DELETE effects.

use djinn_db::migrations::{
    DesignatedOperatorBootstrap, MigrationContext, bootstrap_designated_operator,
    run_postgres_migrations_on_connection,
};
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};

const OP: &str = "00000000-0000-7000-8000-000000000133";
const DISABLED: &str = "00000000-0000-7000-8000-000000000401";
const EPIC: &str = "00000000-0000-7000-8000-000000000402";
const BUILD: &str = "00000000-0000-7000-8000-000000000403";
const AUTHOR: &str = "00000000-0000-7000-8000-000000000404";
const LIFECYCLE: &str = "00000000-0000-7000-8000-000000000405";
const PROJECT: &str = "00000000-0000-7000-8000-000000000400";
const SENTINEL: &str = "00000000-0000-7000-8000-000000000499";
const CREATOR_CONTRACT_VERSION: i64 = 134;

#[tokio::test]
async fn creator_precedence_and_retained_user_lifecycle_are_schema_valid() {
    let (url, admin_url, db_name) = create_database().await;
    bootstrap_designated_operator(
        &url,
        &DesignatedOperatorBootstrap {
            user_id: OP.into(),
            github_id: 9_000_000_133,
            github_login: "operator".into(),
            github_name: None,
            github_avatar_url: None,
        },
    )
    .await
    .expect("bootstrap pre-contract schema");
    let mut conn = PgConnection::connect(&url).await.expect("connect fixture");
    seed(&mut conn).await;

    run_postgres_migrations_on_connection(
        &mut conn,
        &MigrationContext {
            designated_operator_user_id: Some(OP.into()),
        },
    )
    .await
    .expect("apply creator contract on owned connection");

    let creators: Vec<(String, String)> =
        sqlx::query_as("SELECT short_id, created_by_user_id FROM tasks ORDER BY short_id")
            .fetch_all(&mut conn)
            .await
            .expect("read exact creators");
    assert_eq!(
        creators,
        vec![
            ("ambiguous".into(), EPIC.into()),
            ("audit-deleted-source".into(), EPIC.into()),
            ("audit-source".into(), DISABLED.into()),
            ("build-owner".into(), BUILD.into()),
            ("creatorless".into(), OP.into()),
            ("deleted-epic".into(), OP.into()),
            ("deleted-proposal".into(), OP.into()),
            ("deleted-source-user".into(), EPIC.into()),
            ("epic".into(), EPIC.into()),
            ("generic-prose".into(), EPIC.into()),
            ("lifecycle".into(), LIFECYCLE.into()),
            ("malformed-prose".into(), EPIC.into()),
            ("preserved".into(), DISABLED.into()),
            ("proposal-author".into(), AUTHOR.into()),
            // The all-tasks query also audits these extant fixture helpers.
            ("source-a".into(), DISABLED.into()),
            ("source-b".into(), DISABLED.into()),
            ("source-gone-user".into(), OP.into()),
            ("typed-source".into(), DISABLED.into()),
        ]
    );

    // DISABLED remains a valid provenance user. LIFECYCLE is distinct and
    // proves that membership changes do not weaken the final FK + NOT NULL.
    sqlx::query("UPDATE users SET is_member_of_org = false WHERE id = $1")
        .bind(LIFECYCLE)
        .execute(&mut conn)
        .await
        .expect("disable lifecycle user");
    let enabled: bool = sqlx::query_scalar("SELECT is_member_of_org FROM users WHERE id = $1")
        .bind(LIFECYCLE)
        .fetch_one(&mut conn)
        .await
        .expect("read lifecycle user");
    assert!(!enabled);
    assert!(
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(LIFECYCLE)
            .execute(&mut conn)
            .await
            .is_err()
    );

    let nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns WHERE table_schema = 'public' AND table_name = 'tasks' AND column_name = 'created_by_user_id'",
    ).fetch_one(&mut conn).await.expect("inspect information_schema");
    assert_eq!(nullable, "NO");
    let attnotnull: bool = sqlx::query_scalar(
        "SELECT attnotnull FROM pg_attribute WHERE attrelid = 'tasks'::regclass AND attname = 'created_by_user_id'",
    ).fetch_one(&mut conn).await.expect("inspect pg_attribute");
    assert!(attnotnull);
    assert!(
        sqlx::query("UPDATE tasks SET created_by_user_id = NULL WHERE short_id = 'lifecycle'")
            .execute(&mut conn)
            .await
            .is_err()
    );
    assert!(sqlx::query("INSERT INTO tasks (id, project_id, short_id, title, description, design, issue_type, labels, acceptance_criteria, memory_refs, created_by_user_id) VALUES ('00000000-0000-7000-8000-000000000499', $1, 'direct-null', '', '', '', 'task', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, NULL)").bind(PROJECT).execute(&mut conn).await.is_err());

    conn.close().await.expect("close fixture");
    drop_database(&admin_url, &db_name).await;
}

/// A duplicate temporary constraint is deliberately installed before the
/// owned SQLx run. Migration 134 updates first and then adds this constraint,
/// so the duplicate-name failure is a deterministic post-update,
/// pre-contraction fault.
#[tokio::test]
async fn creator_contract_owned_runner_rolls_back_then_is_idempotent() {
    let (mut conn, admin_url, db_name) = pre_contract_fixture(&["rollback-target"]).await;
    sqlx::query(
        "INSERT INTO users (id, github_id, github_login) VALUES ($1, 9_000_000_499, 'sentinel')",
    )
    .bind(SENTINEL)
    .execute(&mut conn)
    .await
    .expect("seed valid sentinel user");

    assert_eq!(
        contract_snapshot(&mut conn).await,
        expected_snapshot(&[("rollback-target", None)], "YES", false, false, 0)
    );
    sqlx::query(
        "ALTER TABLE tasks ADD CONSTRAINT tasks_created_by_user_id_not_null CHECK (created_by_user_id IS NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("install deterministic post-update fault");

    let failure = run_postgres_migrations_on_connection(
        &mut conn,
        &MigrationContext {
            designated_operator_user_id: Some(OP.into()),
        },
    )
    .await
    .expect_err("temporary constraint must fail after migration UPDATE");
    assert!(
        failure
            .to_string()
            .contains("tasks_created_by_user_id_not_null")
    );
    assert_eq!(
        contract_snapshot(&mut conn).await,
        expected_snapshot(&[("rollback-target", None)], "YES", false, true, 0),
        "the owned SQLx failure rolls back its UPDATE and records no version 134 row"
    );

    sqlx::query("ALTER TABLE tasks DROP CONSTRAINT tasks_created_by_user_id_not_null")
        .execute(&mut conn)
        .await
        .expect("remove test-only fault");
    run_postgres_migrations_on_connection(
        &mut conn,
        &MigrationContext {
            designated_operator_user_id: Some(OP.into()),
        },
    )
    .await
    .expect("rerun owned SQLx migration after removing fault");
    assert_eq!(
        contract_snapshot(&mut conn).await,
        expected_snapshot(&[("rollback-target", Some(OP))], "NO", true, false, 1)
    );

    sqlx::query("UPDATE tasks SET created_by_user_id = $1 WHERE short_id = 'rollback-target'")
        .bind(SENTINEL)
        .execute(&mut conn)
        .await
        .expect("set observable valid sentinel after successful migration");
    run_postgres_migrations_on_connection(
        &mut conn,
        &MigrationContext {
            designated_operator_user_id: Some(OP.into()),
        },
    )
    .await
    .expect("second SQLx invocation is a no-op");
    assert_eq!(
        contract_snapshot(&mut conn).await,
        expected_snapshot(&[("rollback-target", Some(SENTINEL))], "NO", true, false, 1),
        "the recorded version prevents a second UPDATE from overwriting the sentinel"
    );

    conn.close().await.expect("close fixture");
    drop_database(&admin_url, &db_name).await;
}

/// This fixture-only copy is derived from the production migration at runtime:
/// it retains its UPDATE -> residue assertion -> NOT NULL ordering, but replaces
/// only the validated designated-operator fallback with NULL. That makes the
/// otherwise unreachable residue marker observable without weakening production.
#[tokio::test]
async fn creator_contract_residue_assertion_precedes_contraction_and_rolls_back() {
    let (mut conn, admin_url, db_name) =
        pre_contract_fixture(&["residue-one", "residue-two"]).await;
    assert_eq!(
        contract_snapshot(&mut conn).await,
        expected_snapshot(
            &[("residue-one", None), ("residue-two", None)],
            "YES",
            false,
            false,
            0,
        )
    );

    conn.execute("BEGIN")
        .await
        .expect("begin isolated test-only residue transaction");
    let production = include_str!("../migrations_postgres/134_task_creator_contract.sql");
    let update_then_contract = production[production
        .find("WITH source_candidates AS")
        .expect("production migration UPDATE")..]
        .replace(
            "(SELECT id FROM users WHERE id = NULLIF(btrim(current_setting('djinn.migration_designated_operator_user_id', true)), ''))",
            "NULL::TEXT",
        );
    let error = conn
        .execute(update_then_contract.as_str())
        .await
        .expect_err("test-only fallback suppression leaves exactly two NULL residues");
    assert!(
        error
            .to_string()
            .contains("creator_contract_null_residue:2"),
        "exact residue marker: {error}"
    );
    conn.execute("ROLLBACK")
        .await
        .expect("roll back failed residue transaction");
    assert_eq!(
        contract_snapshot(&mut conn).await,
        expected_snapshot(
            &[("residue-one", None), ("residue-two", None)],
            "YES",
            false,
            false,
            0,
        ),
        "residue aborts before NOT NULL contraction and leaves no migration bookkeeping"
    );

    conn.close().await.expect("close fixture");
    drop_database(&admin_url, &db_name).await;
}

#[derive(Debug, PartialEq, Eq)]
struct ContractSnapshot {
    creators: Vec<(String, Option<String>)>,
    is_nullable: String,
    attnotnull: bool,
    temporary_constraint_present: bool,
    migration_rows: i64,
}

fn expected_snapshot(
    creators: &[(&str, Option<&str>)],
    is_nullable: &str,
    attnotnull: bool,
    temporary_constraint_present: bool,
    migration_rows: i64,
) -> ContractSnapshot {
    ContractSnapshot {
        creators: creators
            .iter()
            .map(|(short_id, creator)| {
                (
                    (*short_id).to_owned(),
                    creator.map(std::borrow::ToOwned::to_owned),
                )
            })
            .collect(),
        is_nullable: is_nullable.to_owned(),
        attnotnull,
        temporary_constraint_present,
        migration_rows,
    }
}

async fn contract_snapshot(conn: &mut PgConnection) -> ContractSnapshot {
    ContractSnapshot {
        creators: sqlx::query_as(
            "SELECT short_id, created_by_user_id FROM tasks ORDER BY short_id",
        )
        .fetch_all(&mut *conn)
        .await
        .expect("snapshot task creators"),
        is_nullable: sqlx::query_scalar(
            "SELECT is_nullable FROM information_schema.columns WHERE table_schema = 'public' AND table_name = 'tasks' AND column_name = 'created_by_user_id'",
        )
        .fetch_one(&mut *conn)
        .await
        .expect("snapshot information_schema nullability"),
        attnotnull: sqlx::query_scalar(
            "SELECT attnotnull FROM pg_attribute WHERE attrelid = 'tasks'::regclass AND attname = 'created_by_user_id'",
        )
        .fetch_one(&mut *conn)
        .await
        .expect("snapshot pg_attribute nullability"),
        temporary_constraint_present: sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid = 'tasks'::regclass AND conname = 'tasks_created_by_user_id_not_null')",
        )
        .fetch_one(&mut *conn)
        .await
        .expect("snapshot temporary contract constraint"),
        migration_rows: sqlx::query_scalar(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = $1",
        )
        .bind(CREATOR_CONTRACT_VERSION)
        .fetch_one(&mut *conn)
        .await
        .expect("snapshot creator-contract migration bookkeeping"),
    }
}

async fn pre_contract_fixture(targets: &[&str]) -> (PgConnection, String, String) {
    let (url, admin_url, db_name) = create_database().await;
    bootstrap_designated_operator(
        &url,
        &DesignatedOperatorBootstrap {
            user_id: OP.into(),
            github_id: 9_000_000_133,
            github_login: "operator".into(),
            github_name: None,
            github_avatar_url: None,
        },
    )
    .await
    .expect("bootstrap pre-contract fixture");
    let mut conn = PgConnection::connect(&url)
        .await
        .expect("connect pre-contract fixture");
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, 'rollback fixture', 'rollback-fixture', 'rollback-fixture')",
    )
    .bind(PROJECT)
    .execute(&mut conn)
    .await
    .expect("seed fixture project");
    for (offset, target) in targets.iter().enumerate() {
        sqlx::query("INSERT INTO tasks (id, project_id, short_id, title, description, design, issue_type, labels, acceptance_criteria, memory_refs, created_by_user_id) VALUES ($1, $2, $3, '', '', '', 'task', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, NULL)")
            .bind(format!("00000000-0000-7000-8000-0000000005{:02}", offset))
            .bind(PROJECT)
            .bind(*target)
            .execute(&mut conn)
            .await
            .expect("seed known NULL target");
    }
    (conn, admin_url, db_name)
}

async fn seed(conn: &mut PgConnection) {
    // Every reference is inserted while extant. Source-user/task and parent
    // rows are removed afterwards only where the old schema permits it.
    conn.execute(r#"
INSERT INTO users (id, github_id, github_login, is_member_of_org) VALUES
 ('00000000-0000-7000-8000-000000000401', 9401, 'disabled', false),
 ('00000000-0000-7000-8000-000000000402', 9402, 'epic', true),
 ('00000000-0000-7000-8000-000000000403', 9403, 'build', true),
 ('00000000-0000-7000-8000-000000000404', 9404, 'author', true),
 ('00000000-0000-7000-8000-000000000405', 9405, 'lifecycle', true),
 ('00000000-0000-7000-8000-000000000406', 9406, 'gone-source', true);
INSERT INTO projects (id, name, github_owner, github_repo) VALUES ('00000000-0000-7000-8000-000000000400', 'creator fixture', 'creator-fixture', 'creator-fixture');
INSERT INTO proposals (id, short_id, title, author_user_id, build_owner_user_id) VALUES
 ('00000000-0000-7000-8000-000000000410', 'build', '', '00000000-0000-7000-8000-000000000404', '00000000-0000-7000-8000-000000000403'),
 ('00000000-0000-7000-8000-000000000411', 'author', '', '00000000-0000-7000-8000-000000000404', NULL),
 ('00000000-0000-7000-8000-000000000412', 'gone', '', '00000000-0000-7000-8000-000000000404', NULL);
INSERT INTO epics (id, project_id, short_id, title, description, memory_refs, created_by_user_id, proposal_id) VALUES
 ('00000000-0000-7000-8000-000000000420', '00000000-0000-7000-8000-000000000400', 'epic', '', '', '[]', '00000000-0000-7000-8000-000000000402', NULL),
 ('00000000-0000-7000-8000-000000000421', '00000000-0000-7000-8000-000000000400', 'build', '', '', '[]', NULL, '00000000-0000-7000-8000-000000000410'),
 ('00000000-0000-7000-8000-000000000422', '00000000-0000-7000-8000-000000000400', 'author', '', '', '[]', NULL, '00000000-0000-7000-8000-000000000411'),
 ('00000000-0000-7000-8000-000000000423', '00000000-0000-7000-8000-000000000400', 'gone-epic', '', '', '[]', NULL, NULL),
 ('00000000-0000-7000-8000-000000000424', '00000000-0000-7000-8000-000000000400', 'gone-proposal', '', '', '[]', NULL, '00000000-0000-7000-8000-000000000412');
INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, issue_type, owner, labels, acceptance_criteria, memory_refs, created_by_user_id) VALUES
 ('00000000-0000-7000-8000-000000000430', $P$, 'source-a', NULL, '', '', '', 'task', '', '[]', '[]', '[]', '00000000-0000-7000-8000-000000000401'),
 ('00000000-0000-7000-8000-000000000431', $P$, 'source-b', NULL, '', '', '', 'task', '', '[]', '[]', '[]', '00000000-0000-7000-8000-000000000401'),
 ('00000000-0000-7000-8000-000000000432', $P$, 'source-gone-user', NULL, '', '', '', 'task', '', '[]', '[]', '[]', '00000000-0000-7000-8000-000000000406'),
 ('00000000-0000-7000-8000-000000000433', $P$, 'source-gone-task', NULL, '', '', '', 'task', '', '[]', '[]', '[]', '00000000-0000-7000-8000-000000000401'),
 ('00000000-0000-7000-8000-000000000440', $P$, 'typed-source', '00000000-0000-7000-8000-000000000420', '', '', '', 'review', 'system', '["human-review-hold"]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000441', $P$, 'ambiguous', '00000000-0000-7000-8000-000000000420', '', '', '', 'review', 'system', '["human-review-hold"]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000442', $P$, 'deleted-source-user', '00000000-0000-7000-8000-000000000420', '', '', '', 'review', 'system', '["human-review-hold"]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000443', $P$, 'audit-source', NULL, '', '', '', 'review', 'system', '[]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000444', $P$, 'audit-deleted-source', '00000000-0000-7000-8000-000000000420', '', '', '', 'review', 'system', '[]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000445', $P$, 'epic', '00000000-0000-7000-8000-000000000420', '', '', '', 'task', '', '[]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000446', $P$, 'build-owner', '00000000-0000-7000-8000-000000000421', '', '', '', 'task', '', '[]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000447', $P$, 'proposal-author', '00000000-0000-7000-8000-000000000422', '', '', '', 'task', '', '[]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000448', $P$, 'generic-prose', '00000000-0000-7000-8000-000000000420', '', 'source-a', '', 'task', '', '[]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000449', $P$, 'malformed-prose', '00000000-0000-7000-8000-000000000420', '', 'creator=disabled', '', 'task', '', '[]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000450', $P$, 'deleted-epic', '00000000-0000-7000-8000-000000000423', '', '', '', 'task', '', '[]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000451', $P$, 'deleted-proposal', '00000000-0000-7000-8000-000000000424', '', '', '', 'task', '', '[]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000452', $P$, 'creatorless', NULL, '', '', '', 'task', '', '[]', '[]', '[]', NULL),
 ('00000000-0000-7000-8000-000000000453', $P$, 'preserved', NULL, '', '', '', 'task', '', '[]', '[]', '[]', '00000000-0000-7000-8000-000000000401'),
 ('00000000-0000-7000-8000-000000000454', $P$, 'lifecycle', NULL, '', '', '', 'task', '', '[]', '[]', '[]', '00000000-0000-7000-8000-000000000405');
"#.replace("$P$", format!("'{PROJECT}'").as_str()).as_str()).await.expect("seed valid task rows");
    conn.execute(r#"
INSERT INTO blockers (task_id, blocking_task_id) VALUES
 ('00000000-0000-7000-8000-000000000430', '00000000-0000-7000-8000-000000000440'),
 ('00000000-0000-7000-8000-000000000430', '00000000-0000-7000-8000-000000000441'),
 ('00000000-0000-7000-8000-000000000431', '00000000-0000-7000-8000-000000000441'),
 ('00000000-0000-7000-8000-000000000432', '00000000-0000-7000-8000-000000000442');
INSERT INTO audit_sample_policies (id, project_id, policy_json) VALUES ('00000000-0000-7000-8000-000000000470', $P$, '{}');
INSERT INTO audit_sample_frames (id, project_id, policy_id, window_start, window_end, sealed_at) VALUES ('00000000-0000-7000-8000-000000000471', $P$, '00000000-0000-7000-8000-000000000470', 'start', 'end', 'sealed');
INSERT INTO audit_merged_changes (id, project_id, task_id, merge_commit_sha, merged_at) VALUES
 ('00000000-0000-7000-8000-000000000472', $P$, '00000000-0000-7000-8000-000000000430', 'source', 'merged'),
 ('00000000-0000-7000-8000-000000000473', $P$, '00000000-0000-7000-8000-000000000433', 'deleted', 'merged');
INSERT INTO audit_selections (id, frame_id, merged_change_id, stratum, selected_position, seed_commitment, audit_task_id) VALUES
 ('00000000-0000-7000-8000-000000000474', '00000000-0000-7000-8000-000000000471', '00000000-0000-7000-8000-000000000472', 'unflagged_merged', 1, '0000000000000000000000000000000000000000000000000000000000000000', '00000000-0000-7000-8000-000000000443'),
 ('00000000-0000-7000-8000-000000000475', '00000000-0000-7000-8000-000000000471', '00000000-0000-7000-8000-000000000473', 'unflagged_merged', 2, '0000000000000000000000000000000000000000000000000000000000000000', '00000000-0000-7000-8000-000000000444');
DELETE FROM tasks WHERE id = '00000000-0000-7000-8000-000000000433';
DELETE FROM users WHERE id = '00000000-0000-7000-8000-000000000406';
DELETE FROM epics WHERE id = '00000000-0000-7000-8000-000000000423';
DELETE FROM proposals WHERE id = '00000000-0000-7000-8000-000000000412';
"#.replace("$P$", format!("'{PROJECT}'").as_str()).as_str()).await.expect("seed legal disappearance");
}

fn base_url() -> String {
    std::env::var("DJINN_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TEST_POSTGRES_URL"))
        .expect("live postgres URL")
}
fn prefix(url: &str) -> String {
    url.rsplit_once('/')
        .map(|(p, _)| p)
        .unwrap_or(url)
        .trim_end_matches('/')
        .to_owned()
}
async fn create_database() -> (String, String, String) {
    let base = prefix(&base_url());
    let name = format!("djinn_creator_contract_{}", uuid::Uuid::now_v7().simple());
    let admin_url = format!("{base}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect admin");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create database");
    (format!("{base}/{name}"), admin_url, name)
}
async fn drop_database(admin_url: &str, name: &str) {
    let mut admin = PgConnection::connect(admin_url)
        .await
        .expect("connect admin");
    admin.execute(format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{name}' AND pid <> pg_backend_pid()").as_str()).await.expect("terminate connections");
    admin
        .execute(format!(r#"DROP DATABASE "{name}""#).as_str())
        .await
        .expect("drop database");
}
