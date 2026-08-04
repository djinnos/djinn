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
        let second_pool_id: i64 = sqlx::query_scalar(
            "INSERT INTO model_turn_pools (credential_id, provider_id, model_id) \
             VALUES ('credential-1', 'provider', 'other-model') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("create a distinct scoped pool");
        for binding_pool_id in [pool_id, second_pool_id] {
            sqlx::query(
                "INSERT INTO model_turn_bucket_bindings \
                 (pool_id, bucket_kind, capacity_units, available_units) \
                 VALUES ($1, 'request', 1, 1)",
            )
            .bind(binding_pool_id)
            .execute(&pool)
            .await
            .expect("seed request binding");
        }
        let cross_pool_bucket = sqlx::query(
            "INSERT INTO model_turn_reservation_buckets \
             (reservation_id, pool_id, bucket_kind, reserved_units) \
             VALUES ('00000000-0000-4000-8000-000000000001', $1, 'request', 1)",
        )
        .bind(second_pool_id)
        .execute(&pool)
        .await;
        assert!(
            cross_pool_bucket.is_err(),
            "a reservation debit must use its reservation's pool"
        );
        let cross_pool_lease = sqlx::query(
            "INSERT INTO model_turn_leases \
             (lease_id, generation, pool_id, reservation_id, request_id) \
             VALUES ('00000000-0000-4000-8000-000000000003', 1, $1, \
                     '00000000-0000-4000-8000-000000000001', 'request-1')",
        )
        .bind(second_pool_id)
        .execute(&pool)
        .await;
        assert!(
            cross_pool_lease.is_err(),
            "a lease must use its reservation's pool and request"
        );
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

#[tokio::test]
async fn observation_retention_serializes_concurrent_pool_inserts() {
    with_temp_database("observation_race", |database_url| async move {
        djinn_db::test_support::apply_all_migrations_to_fresh_database(&database_url).await;
        let mut setup = PgConnection::connect(&database_url).await.expect("connect setup");
        setup.execute("INSERT INTO credentials (id, provider_id, key_name, encrypted_value) VALUES ('credential-race', 'provider', 'key-name-race', decode('00', 'hex'))").await.expect("seed credential");
        let pool_id: i64 = sqlx::query_scalar("INSERT INTO model_turn_pools (credential_id, provider_id, model_id) VALUES ('credential-race', 'provider', 'model') RETURNING id").fetch_one(&mut setup).await.expect("seed pool");
        let independent_pool_id: i64 = sqlx::query_scalar("INSERT INTO model_turn_pools (credential_id, provider_id, model_id) VALUES ('credential-race', 'provider', 'independent-model') RETURNING id").fetch_one(&mut setup).await.expect("seed independent pool");
        for sequence in 0_i64..255 {
            sqlx::query("INSERT INTO model_turn_observations (pool_id, sequence, kind) VALUES ($1, $2, 'usage')")
                .bind(pool_id).bind(sequence).execute(&mut setup).await.expect("seed observation");
        }

        // Synchronize both transactions before either can invoke the trigger.
        // The first successful insert keeps the pool-row lock until the test
        // explicitly releases it; the other backend must then report that it
        // is blocked by the lock holder rather than merely losing a scheduling
        // race in this test process.
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(2);
        let (inserted_tx, mut inserted_rx) = tokio::sync::mpsc::channel(2);
        let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release_notification = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut tasks = Vec::new();
        for sequence in [255_i64, 256] {
            let url = database_url.clone();
            let barrier = barrier.clone();
            let ready_tx = ready_tx.clone();
            let inserted_tx = inserted_tx.clone();
            let release = release.clone();
            let release_notification = release_notification.clone();
            tasks.push(tokio::spawn(async move {
                let mut connection = PgConnection::connect(&url).await.expect("connect racer");
                let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut connection)
                    .await
                    .expect("read racer backend pid");
                let mut transaction = connection.begin().await.expect("begin racer");
                ready_tx.send(backend_pid).await.expect("report ready racer");
                barrier.wait().await;
                sqlx::query("INSERT INTO model_turn_observations (pool_id, sequence, kind) VALUES ($1, $2, 'usage')")
                    .bind(pool_id).bind(sequence).execute(&mut *transaction).await.expect("insert racer");
                inserted_tx.send(backend_pid).await.expect("report inserted racer");
                while !release.load(std::sync::atomic::Ordering::SeqCst) {
                    release_notification.notified().await;
                }
                transaction.commit().await.expect("commit racer");
            }));
        }
        let first_pid = ready_rx.recv().await.expect("first ready racer");
        let second_pid = ready_rx.recv().await.expect("second ready racer");
        barrier.wait().await;

        let lock_holder_pid = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            inserted_rx.recv(),
        )
        .await
        .expect("one racer must pass the retention trigger before the timeout")
        .expect("a racer must report its successful insert");
        let blocked_pid = if lock_holder_pid == first_pid {
            second_pid
        } else {
            first_pid
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let blocked: bool = sqlx::query_scalar(
                    "SELECT $1::int = ANY(pg_blocking_pids($2::int))",
                )
                .bind(lock_holder_pid)
                .bind(blocked_pid)
                .fetch_one(&mut setup)
                .await
                .expect("inspect racer lock wait");
                if blocked {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("same-pool peer must remain blocked by the trigger lock holder");
        assert!(
            matches!(inserted_rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)),
            "exactly one racer must pass the retention trigger while its peer is blocked"
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sqlx::query("INSERT INTO model_turn_observations (pool_id, sequence, kind) VALUES ($1, 0, 'usage')")
                .bind(independent_pool_id)
                .execute(&mut setup),
        )
        .await
        .expect("a different pool must not wait on the held per-pool trigger lock")
        .expect("insert an observation for a different pool");
        release.store(true, std::sync::atomic::Ordering::SeqCst);
        release_notification.notify_one();
        for task in tasks { task.await.expect("racer task"); }
        let (count, oldest, newest): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(*), min(sequence), max(sequence) FROM model_turn_observations WHERE pool_id = $1",
        )
        .bind(pool_id)
        .fetch_one(&mut setup)
        .await
        .expect("inspect retained observations");
        assert_eq!(count, 256, "concurrent observation retention must remain bounded");
        assert_eq!((oldest, newest), (1, 256), "retention must preserve exactly the newest 256 observations");
    }).await;
}
