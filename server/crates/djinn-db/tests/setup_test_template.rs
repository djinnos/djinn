// Test helper: println used for migration progress output with --nocapture.
#![allow(clippy::print_stdout)]
//! One-shot setup helper for `djinn_test_template` using the owned runner.

#[tokio::test]
#[ignore]
async fn setup_test_template() {
    use djinn_db::migrations::{
        DesignatedOperatorBootstrap, MigrationContext, bootstrap_designated_operator,
        run_postgres_migrations,
    };
    use sqlx::Connection;
    use sqlx::postgres::PgConnection;

    const TEMPLATE_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000001";
    let base = djinn_db::test_database_base_url();
    let server_prefix = base
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(&base);
    let admin_url = format!("{server_prefix}/postgres");
    let mut conn = PgConnection::connect(&admin_url)
        .await
        .expect("connect admin");
    // Clear `datistemplate` BEFORE dropping. A database still marked as a
    // template refuses `DROP DATABASE` with 55006, and because the drop below
    // used to be `let _ =`, that refusal was swallowed and only resurfaced as a
    // baffling 42P04 "already exists" on the CREATE. That made this helper —
    // the one AC that licenses `DJINN_TEST_TEMPLATE_PREBUILT` — fail on every
    // machine that had already built a template once, i.e. every rerun.
    let _ = sqlx::query(
        "UPDATE pg_database SET datistemplate = FALSE WHERE datname = 'djinn_test_template'",
    )
    .execute(&mut conn)
    .await;
    let _ = sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = 'djinn_test_template' AND pid <> pg_backend_pid()")
        .execute(&mut conn).await;
    // Not `let _ =`: a drop that cannot happen must fail here, where the error
    // names the real cause, rather than being re-reported as a create conflict.
    sqlx::query("DROP DATABASE IF EXISTS djinn_test_template")
        .execute(&mut conn)
        .await
        .expect("drop any pre-existing djinn_test_template before rebuilding it");
    sqlx::query("CREATE DATABASE djinn_test_template")
        .execute(&mut conn)
        .await
        .expect("create template");
    drop(conn);

    let template_url = format!("{server_prefix}/djinn_test_template");
    bootstrap_designated_operator(
        &template_url,
        &DesignatedOperatorBootstrap {
            user_id: TEMPLATE_OPERATOR_ID.to_owned(),
            github_id: 9_000_000_001,
            github_login: "djinn-test-template-operator".to_owned(),
            github_name: Some("Djinn test template operator".to_owned()),
            github_avatar_url: None,
        },
    )
    .await
    .expect("provision reserved template operator");
    run_postgres_migrations(
        &template_url,
        &MigrationContext {
            designated_operator_user_id: Some(TEMPLATE_OPERATOR_ID.to_owned()),
        },
    )
    .await
    .expect("apply embedded migrations");

    let mut conn = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect admin");
    sqlx::query(
        "UPDATE pg_database SET datistemplate = TRUE WHERE datname = 'djinn_test_template'",
    )
    .execute(&mut conn)
    .await
    .expect("mark template");
    drop(conn);

    let applied = verify_template_is_fully_migrated(&admin_url, &template_url).await;
    println!("djinn_test_template ready at {template_url}");
    // Sentinel consumed by `.github/workflows/quality-gate.yml`. It is the ONLY
    // thing that licenses the workflow to export `DJINN_TEST_TEMPLATE_PREBUILT=1`,
    // which makes every test process skip `ensure_test_template`. A grep for it
    // is required because `cargo test --test <t> -- --ignored <filter>` exits 0
    // when the filter matches no tests: without the sentinel, a renamed or
    // deleted test would look like a successful template build and every DB test
    // would then run against whatever `CREATE DATABASE` left behind.
    println!("DJINN_TEST_TEMPLATE_VERIFIED migrations={applied}");
}

/// Prove the template is a real, fully-migrated clone source.
///
/// Asserts, against the embedded migrator that production uses:
/// * `datistemplate` is set (otherwise `CREATE DATABASE … TEMPLATE` refuses),
/// * every embedded up-migration is recorded in `_sqlx_migrations`,
/// * each recorded row has `success = true` and a checksum equal to the
///   embedded migration's — so a template built from a different checkout, or
///   one whose migrator died halfway, fails here rather than silently serving
///   an unmigrated schema to ~12k tests,
/// * the reserved designated-operator row exists.
///
/// Returns the number of applied migrations.
async fn verify_template_is_fully_migrated(admin_url: &str, template_url: &str) -> usize {
    use sqlx::Connection;
    use sqlx::postgres::PgConnection;

    const TEMPLATE_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000001";

    let mut admin = PgConnection::connect(admin_url)
        .await
        .expect("connect admin for template verification");
    let is_template: Option<bool> = sqlx::query_scalar(
        "SELECT datistemplate FROM pg_database WHERE datname = 'djinn_test_template'",
    )
    .fetch_optional(&mut admin)
    .await
    .expect("read datistemplate");
    assert_eq!(
        is_template,
        Some(true),
        "djinn_test_template must exist and be marked datistemplate"
    );
    drop(admin);

    let embedded = sqlx::migrate!("./migrations_postgres");
    let expected: Vec<(i64, Vec<u8>)> = embedded
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect();
    assert!(
        !expected.is_empty(),
        "embedded migrator resolved zero up-migrations"
    );

    let mut conn = PgConnection::connect(template_url)
        .await
        .expect("connect template for verification");
    let applied: Vec<(i64, Vec<u8>, bool)> =
        sqlx::query_as("SELECT version, checksum, success FROM _sqlx_migrations")
            .fetch_all(&mut conn)
            .await
            .expect("read _sqlx_migrations from the template");

    for (version, checksum, success) in &applied {
        assert!(*success, "migration {version} is recorded as failed");
        let embedded_checksum = expected
            .iter()
            .find(|(candidate, _)| candidate == version)
            .map(|(_, checksum)| checksum);
        assert_eq!(
            embedded_checksum,
            Some(checksum),
            "migration {version} in the template does not match the embedded migration"
        );
    }
    for (version, _) in &expected {
        assert!(
            applied.iter().any(|(applied, _, _)| applied == version),
            "embedded migration {version} is not applied to djinn_test_template"
        );
    }

    let operators: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = $1")
        .bind(TEMPLATE_OPERATOR_ID)
        .fetch_one(&mut conn)
        .await
        .expect("count reserved template operator");
    assert_eq!(
        operators, 1,
        "reserved designated-operator row is missing from djinn_test_template"
    );

    expected.len()
}
