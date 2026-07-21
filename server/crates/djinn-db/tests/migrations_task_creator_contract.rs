//! Live-Postgres fixture harness for migration 137's creator contract.
//!
//! The owned runner receives a per-connection `MigrationContext`; these tests
//! intentionally never mutate process-global environment for operator input.

use djinn_db::migrations::{
    DesignatedOperatorBootstrap, MigrationContext, bootstrap_designated_operator,
    run_postgres_migrations_on_connection,
};
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};

fn base_database_url() -> String {
    std::env::var("DJINN_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TEST_POSTGRES_URL"))
        .expect("live postgres URL")
}

/// This deliberately uses SQL fixtures rather than task prose: every winning
/// tier below is a durable typed relation consumed by migration 137.
#[tokio::test]
async fn typed_provenance_fixture_matrix_is_deterministic() {
    const OP: &str = "00000000-0000-7000-8000-000000000136";
    const DISABLED: &str = "00000000-0000-7000-8000-000000000401";
    const EPIC: &str = "00000000-0000-7000-8000-000000000402";
    const BUILD: &str = "00000000-0000-7000-8000-000000000403";
    const AUTHOR: &str = "00000000-0000-7000-8000-000000000404";
    const LIFECYCLE: &str = "00000000-0000-7000-8000-000000000405";
    const DELETED: &str = "00000000-0000-7000-8000-000000000406";
    let (db_url, admin_url, db_name) = create_database().await;
    bootstrap_designated_operator(
        &db_url,
        &DesignatedOperatorBootstrap {
            user_id: OP.into(),
            github_id: 9_000_000_136,
            github_login: "operator".into(),
            github_name: None,
            github_avatar_url: None,
        },
    )
    .await
    .unwrap();
    let mut conn = PgConnection::connect(&db_url).await.unwrap();
    for (id, github_id, member) in [
        (DISABLED, 9401, false),
        (EPIC, 9402, true),
        (BUILD, 9403, true),
        (AUTHOR, 9404, true),
        (LIFECYCLE, 9405, true),
        (DELETED, 9406, true),
    ] {
        sqlx::query("INSERT INTO users (id, github_id, github_login, is_member_of_org) VALUES ($1, $2, $1, $3)").bind(id).bind(github_id).bind(member).execute(&mut conn).await.unwrap();
    }
    conn.execute("INSERT INTO projects (id, name, path, verification_rules) VALUES ('00000000-0000-7000-8000-000000000400', 'matrix', '/matrix', '[]'::jsonb);
INSERT INTO proposals (id, short_id, title, author_user_id, build_owner_user_id) VALUES ('00000000-0000-7000-8000-000000000410','pbuild','', '00000000-0000-7000-8000-000000000404','00000000-0000-7000-8000-000000000403'), ('00000000-0000-7000-8000-000000000411','pauthor','', '00000000-0000-7000-8000-000000000404',NULL), ('00000000-0000-7000-8000-000000000412','deleted-proposal','', '00000000-0000-7000-8000-000000000404',NULL);
INSERT INTO epics (id, project_id, short_id, title, description, memory_refs, created_by_user_id, proposal_id) VALUES ('00000000-0000-7000-8000-000000000420','00000000-0000-7000-8000-000000000400','epic','', '', '[]'::jsonb,'00000000-0000-7000-8000-000000000402',NULL), ('00000000-0000-7000-8000-000000000421','00000000-0000-7000-8000-000000000400','build','', '', '[]'::jsonb,NULL,'00000000-0000-7000-8000-000000000410'), ('00000000-0000-7000-8000-000000000422','00000000-0000-7000-8000-000000000400','author','', '', '[]'::jsonb,NULL,'00000000-0000-7000-8000-000000000411'), ('00000000-0000-7000-8000-000000000423','00000000-0000-7000-8000-000000000400','deleted-epic','', '', '[]'::jsonb,NULL,NULL), ('00000000-0000-7000-8000-000000000424','00000000-0000-7000-8000-000000000400','deleted-proposal','', '', '[]'::jsonb,NULL,'00000000-0000-7000-8000-000000000412')").await.unwrap();
    // source, source through retained remediation hold, generic edge, and all fallback tiers
    conn.execute("INSERT INTO tasks (id,project_id,short_id,epic_id,title,description,design,issue_type,owner,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES
('00000000-0000-7000-8000-000000000430','00000000-0000-7000-8000-000000000400','source',NULL,'','','','task','','[]','[]','[]','00000000-0000-7000-8000-000000000401'),
('00000000-0000-7000-8000-000000000431','00000000-0000-7000-8000-000000000400','hold',NULL,'','','','review','system','[\"human-review-hold\"]','[]','[]',NULL),
('00000000-0000-7000-8000-000000000432','00000000-0000-7000-8000-000000000400','generic','00000000-0000-7000-8000-000000000420','free text must not count','','','review','system','[]','[]','[]',NULL),
('00000000-0000-7000-8000-000000000433','00000000-0000-7000-8000-000000000400','epic','00000000-0000-7000-8000-000000000420','','','','task','','[]','[]','[]',NULL),
('00000000-0000-7000-8000-000000000434','00000000-0000-7000-8000-000000000400','build','00000000-0000-7000-8000-000000000421','','','','task','','[]','[]','[]',NULL),
('00000000-0000-7000-8000-000000000435','00000000-0000-7000-8000-000000000400','author','00000000-0000-7000-8000-000000000422','','','','task','','[]','[]','[]',NULL),
('00000000-0000-7000-8000-000000000436','00000000-0000-7000-8000-000000000400','residue',NULL,'creator-less chain','','','task','','[]','[]','[]',NULL);
INSERT INTO blockers (task_id,blocking_task_id) VALUES ('00000000-0000-7000-8000-000000000430','00000000-0000-7000-8000-000000000431'), ('00000000-0000-7000-8000-000000000430','00000000-0000-7000-8000-000000000432')").await.unwrap();
    // Audit selection is a separate durable source relation. Its ledger has
    // no task FK, so the second selection models a deleted source task.
    conn.execute("INSERT INTO proposals (id,short_id,title,author_user_id,build_owner_user_id) VALUES ('00000000-0000-7000-8000-000000000460','pgone','','00000000-0000-7000-8000-000000000404','00000000-0000-7000-8000-000000000403');
INSERT INTO epics (id,project_id,short_id,title,description,memory_refs,created_by_user_id,proposal_id) VALUES ('00000000-0000-7000-8000-000000000461','00000000-0000-7000-8000-000000000400','dangling','','','[]','00000000-0000-7000-8000-000000000402',NULL),('00000000-0000-7000-8000-000000000462','00000000-0000-7000-8000-000000000400','gone-proposal','','','[]','missing','00000000-0000-7000-8000-000000000460');
INSERT INTO tasks (id,project_id,short_id,epic_id,title,description,design,issue_type,owner,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES
('00000000-0000-7000-8000-000000000438','00000000-0000-7000-8000-000000000400','audit',NULL,'','','','review','system','[]','[]','[]',NULL),('00000000-0000-7000-8000-000000000439','00000000-0000-7000-8000-000000000400','audit-missing','00000000-0000-7000-8000-000000000420','','','','review','system','[]','[]','[]',NULL),('00000000-0000-7000-8000-000000000440','00000000-0000-7000-8000-000000000400','ambiguous','00000000-0000-7000-8000-000000000420','','','','review','system','[\"human-review-hold\"]','[]','[]',NULL),('00000000-0000-7000-8000-000000000441','00000000-0000-7000-8000-000000000400','amb-source-a',NULL,'','','','task','','[]','[]','[]','00000000-0000-7000-8000-000000000401'),('00000000-0000-7000-8000-000000000442','00000000-0000-7000-8000-000000000400','amb-source-b',NULL,'','','','task','','[]','[]','[]','00000000-0000-7000-8000-000000000401'),('00000000-0000-7000-8000-000000000443','00000000-0000-7000-8000-000000000400','missing-source',NULL,'','','','task','','[]','[]','[]','00000000-0000-7000-8000-000000000406'),('00000000-0000-7000-8000-000000000444','00000000-0000-7000-8000-000000000400','missing-source-hold','00000000-0000-7000-8000-000000000420','','','','review','system','[\"planner-park-escalation\"]','[]','[]',NULL),('00000000-0000-7000-8000-000000000445','00000000-0000-7000-8000-000000000400','dangling-epic','00000000-0000-7000-8000-000000000461','','','','task','','[]','[]','[]',NULL),('00000000-0000-7000-8000-000000000446','00000000-0000-7000-8000-000000000400','dangling-proposal','00000000-0000-7000-8000-000000000462','','','','task','','[]','[]','[]',NULL),('00000000-0000-7000-8000-000000000447','00000000-0000-7000-8000-000000000400','malformed','00000000-0000-7000-8000-000000000420','creator is disabled','','','task','','[]','[]','[]',NULL),('00000000-0000-7000-8000-000000000448','00000000-0000-7000-8000-000000000400','preserved',NULL,'','','','task','','[]','[]','[]','00000000-0000-7000-8000-000000000405'),('00000000-0000-7000-8000-000000000449','00000000-0000-7000-8000-000000000400','deleted-source',NULL,'','','','task','','[]','[]','[]','00000000-0000-7000-8000-000000000401');
INSERT INTO blockers (task_id,blocking_task_id) VALUES ('00000000-0000-7000-8000-000000000441','00000000-0000-7000-8000-000000000440'),('00000000-0000-7000-8000-000000000442','00000000-0000-7000-8000-000000000440'),('00000000-0000-7000-8000-000000000443','00000000-0000-7000-8000-000000000444');
INSERT INTO audit_sample_policies (id,project_id,policy_json) VALUES ('00000000-0000-7000-8000-000000000470','00000000-0000-7000-8000-000000000400','{}'); INSERT INTO audit_sample_frames (id,project_id,policy_id,window_start,window_end,sealed_at) VALUES ('00000000-0000-7000-8000-000000000471','00000000-0000-7000-8000-000000000400','00000000-0000-7000-8000-000000000470','start','end','sealed');
INSERT INTO audit_merged_changes (id,project_id,task_id,merge_commit_sha,merged_at) VALUES ('00000000-0000-7000-8000-000000000472','00000000-0000-7000-8000-000000000400','00000000-0000-7000-8000-000000000430','audit-source','merged'),('00000000-0000-7000-8000-000000000473','00000000-0000-7000-8000-000000000400','00000000-0000-7000-8000-000000000449','audit-deleted-source','merged');
INSERT INTO audit_selections (id,frame_id,merged_change_id,stratum,selected_position,seed_commitment,audit_task_id) VALUES ('00000000-0000-7000-8000-000000000474','00000000-0000-7000-8000-000000000471','00000000-0000-7000-8000-000000000472','unflagged_merged',1,'0000000000000000000000000000000000000000000000000000000000000000','00000000-0000-7000-8000-000000000438'),('00000000-0000-7000-8000-000000000475','00000000-0000-7000-8000-000000000471','00000000-0000-7000-8000-000000000473','unflagged_merged',2,'0000000000000000000000000000000000000000000000000000000000000000','00000000-0000-7000-8000-000000000439');
DELETE FROM tasks WHERE id = '00000000-0000-7000-8000-000000000449'; DELETE FROM users WHERE id = '00000000-0000-7000-8000-000000000406'; DELETE FROM epics WHERE id = '00000000-0000-7000-8000-000000000461'; DELETE FROM proposals WHERE id = '00000000-0000-7000-8000-000000000460';").await.unwrap();
    run_postgres_migrations_on_connection(
        &mut conn,
        &MigrationContext {
            designated_operator_user_id: Some(OP.into()),
        },
    )
    .await
    .unwrap();
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT short_id, created_by_user_id FROM tasks ORDER BY short_id")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("amb-source-a".into(), DISABLED.into()),
            ("amb-source-b".into(), DISABLED.into()),
            ("ambiguous".into(), EPIC.into()),
            ("audit".into(), DISABLED.into()),
            ("audit-missing".into(), EPIC.into()),
            ("author".into(), AUTHOR.into()),
            ("build".into(), BUILD.into()),
            ("dangling-epic".into(), OP.into()),
            ("dangling-proposal".into(), OP.into()),
            ("epic".into(), EPIC.into()),
            ("generic".into(), EPIC.into()),
            ("hold".into(), DISABLED.into()),
            ("malformed".into(), EPIC.into()),
            ("missing-source".into(), OP.into()),
            ("missing-source-hold".into(), EPIC.into()),
            ("preserved".into(), LIFECYCLE.into()),
            ("residue".into(), OP.into()),
            ("source".into(), DISABLED.into())
        ]
    );
    conn.execute("UPDATE users SET is_member_of_org = false WHERE id = '00000000-0000-7000-8000-000000000405'").await.unwrap();
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT is_member_of_org FROM users WHERE id = '00000000-0000-7000-8000-000000000405'"
        )
        .fetch_one(&mut conn)
        .await
        .unwrap()
    );
    assert!(
        conn.execute("DELETE FROM users WHERE id = '00000000-0000-7000-8000-000000000405'")
            .await
            .is_err()
    );
    conn.close().await.unwrap();
    drop_database(&admin_url, &db_name).await;
}

#[tokio::test]
async fn unset_zero_task_and_forced_post_update_failure_are_atomic() {
    const OP: &str = "00000000-0000-7000-8000-000000000136";
    let (db_url, admin_url, db_name) = create_database().await;
    bootstrap_designated_operator(
        &db_url,
        &DesignatedOperatorBootstrap {
            user_id: OP.into(),
            github_id: 9_000_000_136,
            github_login: "operator".into(),
            github_name: None,
            github_avatar_url: None,
        },
    )
    .await
    .unwrap();
    let mut conn = PgConnection::connect(&db_url).await.unwrap();
    for context in [None, Some("   ".into())] {
        let error = run_postgres_migrations_on_connection(
            &mut conn,
            &MigrationContext {
                designated_operator_user_id: context,
            },
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("creator_contract_designated_operator_unset")
        );
    }
    // A failed attempt can leave a session-scoped setting behind. The next
    // invocation must explicitly clear it when no designated operator is
    // supplied rather than reusing stale authority from this connection.
    let invalid_error = run_postgres_migrations_on_connection(
        &mut conn,
        &MigrationContext {
            designated_operator_user_id: Some("missing-operator".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(
        invalid_error
            .to_string()
            .contains("creator_contract_designated_operator_invalid:missing-operator")
    );
    let unset_error = run_postgres_migrations_on_connection(
        &mut conn,
        &MigrationContext {
            designated_operator_user_id: None,
        },
    )
    .await
    .unwrap_err();
    assert!(
        unset_error
            .to_string()
            .contains("creator_contract_designated_operator_unset")
    );
    conn.execute("INSERT INTO projects (id,name,path,verification_rules) VALUES ('00000000-0000-7000-8000-000000000450','rollback','/rollback','[]'); INSERT INTO tasks (id,project_id,short_id,title,description,design,issue_type,labels,acceptance_criteria,memory_refs) VALUES ('00000000-0000-7000-8000-000000000451','00000000-0000-7000-8000-000000000450','null','','','','task','[]','[]','[]'); ALTER TABLE tasks ADD CONSTRAINT tasks_created_by_user_id_not_null CHECK (true)").await.unwrap();
    assert!(
        run_postgres_migrations_on_connection(
            &mut conn,
            &MigrationContext {
                designated_operator_user_id: Some(OP.into())
            }
        )
        .await
        .is_err()
    );
    let unset_after_valid_error = run_postgres_migrations_on_connection(
        &mut conn,
        &MigrationContext {
            designated_operator_user_id: None,
        },
    )
    .await
    .unwrap_err();
    assert!(
        unset_after_valid_error
            .to_string()
            .contains("creator_contract_designated_operator_unset")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tasks WHERE created_by_user_id IS NULL")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations WHERE version = 136")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        0
    );
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT attnotnull FROM pg_attribute WHERE attrelid = 'tasks'::regclass AND attname = 'created_by_user_id'")
            .fetch_one(&mut conn).await.unwrap(),
        "post-update failure rolls back the schema contract"
    );
    conn.execute("ALTER TABLE tasks DROP CONSTRAINT tasks_created_by_user_id_not_null")
        .await
        .unwrap();
    run_postgres_migrations_on_connection(
        &mut conn,
        &MigrationContext {
            designated_operator_user_id: Some(OP.into()),
        },
    )
    .await
    .unwrap();
    run_postgres_migrations_on_connection(
        &mut conn,
        &MigrationContext {
            designated_operator_user_id: Some(OP.into()),
        },
    )
    .await
    .unwrap();
    conn.close().await.unwrap();
    drop_database(&admin_url, &db_name).await;
}

fn server_prefix(base: &str) -> String {
    base.rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_owned()
}

async fn create_database() -> (String, String, String) {
    let prefix = server_prefix(&base_database_url());
    let db_name = format!("djinn_creator_contract_{}", uuid::Uuid::now_v7().simple());
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect admin");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("create database");
    drop(admin);
    (format!("{prefix}/{db_name}"), admin_url, db_name)
}

async fn drop_database(admin_url: &str, db_name: &str) {
    let mut admin = PgConnection::connect(admin_url)
        .await
        .expect("reconnect admin");
    admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await
        .expect("terminate database connections");
    admin
        .execute(format!(r#"DROP DATABASE "{db_name}""#).as_str())
        .await
        .expect("drop database");
}

#[tokio::test]
async fn designated_operator_preflight_is_exact_and_contracts_null_tasks() {
    const OPERATOR: &str = "00000000-0000-7000-8000-000000000136";
    const EXISTING: &str = "00000000-0000-7000-8000-000000000134";
    let (db_url, admin_url, db_name) = create_database().await;

    // Bootstrap applies only the pre-contract chain, allowing legacy NULL rows
    // to be seeded without bypassing the migration runner.
    bootstrap_designated_operator(
        &db_url,
        &DesignatedOperatorBootstrap {
            user_id: OPERATOR.to_owned(),
            github_id: 9_000_000_136,
            github_login: "creator-contract-operator".to_owned(),
            github_name: None,
            github_avatar_url: None,
        },
    )
    .await
    .expect("bootstrap pre-contract schema and operator");

    let mut seed = PgConnection::connect(&db_url).await.expect("connect seed");
    seed.execute("INSERT INTO users (id, github_id, github_login, is_member_of_org) VALUES ('00000000-0000-7000-8000-000000000134', 9000000134, 'retained-disabled', false)")
        .await
        .expect("insert disabled retained user");
    seed.execute("INSERT INTO projects (id, name, path, verification_rules) VALUES ('00000000-0000-7000-8000-000000000135', 'creator contract', '/creator-contract', '[]'::jsonb)")
        .await
        .expect("insert project");
    seed.execute("INSERT INTO tasks (id, project_id, short_id, title, description, design, issue_type, labels, acceptance_criteria, memory_refs) VALUES ('00000000-0000-7000-8000-000000000136', '00000000-0000-7000-8000-000000000135', 'null', 'unattributed', '', '', 'task', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb), ('00000000-0000-7000-8000-000000000137', '00000000-0000-7000-8000-000000000135', 'kept', 'already attributed', '', '', 'task', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)")
        .await
        .expect("insert legacy tasks");
    seed.execute("UPDATE tasks SET created_by_user_id = '00000000-0000-7000-8000-000000000134' WHERE id = '00000000-0000-7000-8000-000000000137'")
        .await
        .expect("attribute preserved task");
    seed.close().await.expect("close seed connection");

    let mut invalid = PgConnection::connect(&db_url)
        .await
        .expect("connect invalid runner");
    let error = run_postgres_migrations_on_connection(
        &mut invalid,
        &MigrationContext {
            designated_operator_user_id: Some("missing-operator".to_owned()),
        },
    )
    .await
    .expect_err("unknown operator must abort migration");
    assert!(
        error
            .to_string()
            .contains("creator_contract_designated_operator_invalid:missing-operator")
    );
    let nulls: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tasks WHERE created_by_user_id IS NULL")
            .fetch_one(&mut invalid)
            .await
            .expect("inspect unchanged legacy row");
    assert_eq!(nulls, 1, "invalid preflight must not backfill");
    invalid.close().await.expect("close invalid runner");

    let mut migration = PgConnection::connect(&db_url)
        .await
        .expect("connect owned runner");
    run_postgres_migrations_on_connection(
        &mut migration,
        &MigrationContext {
            designated_operator_user_id: Some(OPERATOR.to_owned()),
        },
    )
    .await
    .expect("apply creator contract");
    let creators: Vec<(String, String)> =
        sqlx::query_as("SELECT id, created_by_user_id FROM tasks ORDER BY id")
            .fetch_all(&mut migration)
            .await
            .expect("read creators");
    assert_eq!(
        creators,
        vec![
            (
                "00000000-0000-7000-8000-000000000136".to_owned(),
                OPERATOR.to_owned()
            ),
            (
                "00000000-0000-7000-8000-000000000137".to_owned(),
                EXISTING.to_owned()
            ),
        ]
    );
    let nullable: String = sqlx::query_scalar("SELECT is_nullable FROM information_schema.columns WHERE table_name = 'tasks' AND column_name = 'created_by_user_id'")
        .fetch_one(&mut migration).await.expect("inspect catalog");
    assert_eq!(nullable, "NO");
    assert!(sqlx::query_scalar::<_, bool>("SELECT attnotnull FROM pg_attribute WHERE attrelid = 'tasks'::regclass AND attname = 'created_by_user_id'").fetch_one(&mut migration).await.expect("inspect pg_attribute contract"));
    assert!(sqlx::query("UPDATE tasks SET created_by_user_id = NULL WHERE id = '00000000-0000-7000-8000-000000000136'").execute(&mut migration).await.is_err());
    assert!(sqlx::query("INSERT INTO tasks (id, project_id, short_id, title, description, design, issue_type, labels, acceptance_criteria, memory_refs, created_by_user_id) VALUES ('00000000-0000-7000-8000-000000000138', '00000000-0000-7000-8000-000000000135', 'direct-null', '', '', '', 'task', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, NULL)").execute(&mut migration).await.is_err());
    migration.close().await.expect("close owned runner");

    drop_database(&admin_url, &db_name).await;
}
