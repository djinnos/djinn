//! Contract coverage for the additive, inert model-turn admission v1 schema.

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = djinn_db::test_database_base_url();
    let prefix = base
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(&base)
        .trim_end_matches('/');
    let db_name = format!(
        "djinn_model_turn_{suffix}_{}",
        uuid::Uuid::now_v7().simple()
    );
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect admin");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("create database");
    drop(admin);

    let database_url = format!("{prefix}/{db_name}");
    let result = f(database_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect admin");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{db_name}""#).as_str())
        .await
        .expect("drop database");
    result
}

#[tokio::test]
async fn fresh_initialization_installs_v1_marker_and_enforces_credential_identity() {
    with_temp_database("fresh", |database_url| async move {
        djinn_db::test_support::apply_all_migrations_to_fresh_database(&database_url).await;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("connect migrated database");

        let marker: i64 = sqlx::query_scalar(
            "SELECT version FROM model_turn_admission_schema \
             WHERE marker = 'model_turn_admission_schema'",
        )
        .fetch_one(&pool)
        .await
        .expect("read v1 marker");
        assert_eq!(marker, djinn_db::MODEL_TURN_ADMISSION_SCHEMA_VERSION);

        let missing_credential = sqlx::query(
            "INSERT INTO model_turn_pools (credential_id, provider_id, model_id) \
             VALUES ('missing', 'provider', 'model')",
        )
        .execute(&pool)
        .await;
        assert!(
            missing_credential.is_err(),
            "pool identity must reference credentials.id"
        );

        sqlx::query(
            "INSERT INTO credentials (id, provider_id, key_name, encrypted_value) \
             VALUES ('credential-1', 'provider', 'key-name-1', decode('00', 'hex'))",
        )
        .execute(&pool)
        .await
        .expect("seed credential without exposing secret material");
        let pool_id: i64 = sqlx::query_scalar(
            "INSERT INTO model_turn_pools (credential_id, provider_id, model_id) \
             VALUES ('credential-1', 'provider', 'model') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("create credential-scoped pool");

        let invalid_phase =
            sqlx::query("UPDATE model_turn_pools SET phase = 'invalid' WHERE id = $1")
                .bind(pool_id)
                .execute(&pool)
                .await;
        assert!(
            invalid_phase.is_err(),
            "phase CHECK must reject invalid lifecycle values"
        );

        let negative_accounting = sqlx::query(
            "INSERT INTO model_turn_bucket_bindings \
             (pool_id, bucket_kind, capacity_units, available_units) \
             VALUES ($1, 'request', 1, -1)",
        )
        .bind(pool_id)
        .execute(&pool)
        .await;
        assert!(
            negative_accounting.is_err(),
            "bucket accounting must remain non-negative"
        );

        // A lease has exactly one terminal row, keyed by its random id.
        sqlx::query(
            "INSERT INTO model_turn_reservations (id, pool_id, request_id) \
             VALUES ('00000000-0000-4000-8000-000000000001', $1, 'request-1')",
        )
        .bind(pool_id)
        .execute(&pool)
        .await
        .expect("seed reservation");
        sqlx::query(
            "INSERT INTO model_turn_leases \
             (lease_id, generation, pool_id, reservation_id, request_id) \
             VALUES ('00000000-0000-4000-8000-000000000002', 1, $1, \
                     '00000000-0000-4000-8000-000000000001', 'request-1')",
        )
        .bind(pool_id)
        .execute(&pool)
        .await
        .expect("seed lease");
        let terminal_sql = "INSERT INTO model_turn_lease_terminals \
                            (lease_id, generation, request_id, outcome) \
                            VALUES ('00000000-0000-4000-8000-000000000002', 1, 'request-1', 'completed')";
        sqlx::query(terminal_sql)
            .execute(&pool)
            .await
            .expect("write terminal record");
        assert!(
            sqlx::query(terminal_sql).execute(&pool).await.is_err(),
            "a lease must not have duplicate terminal records"
        );

        for sequence in 0_i64..257 {
            sqlx::query(
                "INSERT INTO model_turn_observations (pool_id, sequence, kind) \
                 VALUES ($1, $2, 'usage')",
            )
            .bind(pool_id)
            .bind(sequence)
            .execute(&pool)
            .await
            .expect("write bounded observation");
        }
        let observations: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM model_turn_observations WHERE pool_id = $1",
        )
        .bind(pool_id)
        .fetch_one(&pool)
        .await
        .expect("count observations");
        assert_eq!(observations, 256, "observation retention must be bounded");

        pool.close().await;
    })
    .await;
}

#[test]
fn migration_has_bounded_non_secret_observation_contract() {
    let migration = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/migrations_postgres/173_model_turn_admission.sql"
    ))
    .expect("read migration 173");
    for required in [
        "model_turn_observations_bounded",
        "OFFSET 256",
        "model_turn_lease_terminals",
        "REFERENCES credentials(id)",
        "model_turn_admission_schema",
    ] {
        assert!(
            migration.contains(required),
            "migration must contain {required}"
        );
    }
    assert!(
        !migration.contains("owner_user_id"),
        "durable pool schema must not use a user id as its identity"
    );
    assert!(
        !migration.contains("encrypted_value"),
        "admission storage must not duplicate credential material"
    );
}
