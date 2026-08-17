//! Migration 210 — Phase-C controller-window hardening (epic ai6g, task hb3s).
//!
//! The typed Rust boundary in `djinn-db` already refuses an out-of-contract
//! controller window, but a boundary is only a boundary while every writer goes
//! through it. These regressions prove the *durable ledger itself* refuses the
//! same shapes, so a row that reached Postgres by any other route still cannot
//! become a trainable learner window.
//!
//! Catalog authority is deliberately absent here. The schema knows nothing about
//! models.dev, and the pool label pair is never treated as proof of catalog
//! membership; that revalidation stays at the coordinator seam.

use std::path::{Path, PathBuf};

use djinn_db::{Database, repositories::test_support::seed_scoped_model_turn_admission_fixture};
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 210;
const MIGRATION_FILE: &str = "210_phase_c_controller_window_hardening.sql";

fn base_database_url() -> String {
    djinn_db::test_database_base_url()
}

fn server_prefix(base: &str) -> String {
    base.rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_owned()
}

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres")
}

fn migration_entries(dir: &Path) -> Vec<(u64, PathBuf)> {
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

async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = base_database_url();
    let prefix = server_prefix(&base);
    let db_name = format!(
        "djinn_migration_{}_{}",
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
        .expect("create migration test database");
    drop(admin);

    let db_url = format!("{prefix}/{db_name}");
    let result = f(db_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect postgres admin database");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{db_name}""#).as_str())
        .await
        .expect("drop migration test database");

    result
}

async fn assert_hardened_schema(db_url: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(db_url)
        .await
        .expect("connect migration test database");

    for constraint in [
        "model_turn_controller_windows_aligned_minute",
        "model_turn_controller_windows_sequence_matches_start",
        "model_turn_controller_windows_summary_closed",
    ] {
        let present: Option<String> = sqlx::query_scalar(
            "SELECT conname FROM pg_constraint \
             WHERE conrelid = 'model_turn_controller_windows'::regclass \
               AND contype = 'c' AND conname = $1",
        )
        .bind(constraint)
        .fetch_optional(&pool)
        .await
        .unwrap_or_else(|e| panic!("inspect constraint {constraint}: {e}"));
        assert_eq!(
            present.as_deref(),
            Some(constraint),
            "expected CHECK constraint {constraint}"
        );
    }

    // Exactly one controller-window ledger: the hardening is additive.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name LIKE '%controller_window%' \
         ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await
    .expect("inspect controller-window tables");
    assert_eq!(tables, vec!["model_turn_controller_windows".to_owned()]);

    pool.close().await;
}

#[tokio::test]
async fn migration_210_applies_on_a_fresh_database() {
    with_temp_database("fresh_phase_c_windows", |db_url| async move {
        djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;
        assert_hardened_schema(&db_url).await;
    })
    .await;
}

/// The hardening is additive: it applies on top of the *prior* schema, over a
/// ledger that already holds a controller-window row, and leaves that row alone.
#[tokio::test]
async fn migration_210_applies_additively_over_the_prior_schema() {
    with_temp_database("prior_phase_c_windows", |db_url| async move {
        const OPERATOR: &str = "00000000-0000-7000-8000-000000000210";
        djinn_db::migrations::bootstrap_designated_operator(
            &db_url,
            &djinn_db::migrations::DesignatedOperatorBootstrap {
                user_id: OPERATOR.to_owned(),
                github_id: 9_000_000_210,
                github_login: "hb3s-migration-operator".to_owned(),
                github_name: None,
                github_avatar_url: None,
            },
        )
        .await
        .expect("bootstrap designated operator");

        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        sqlx::query("SELECT set_config('djinn.migration_designated_operator_user_id', $1, false)")
            .bind(OPERATOR)
            .execute(&mut conn)
            .await
            .expect("publish designated operator to the migration session");
        for (version, path) in migration_entries(&migrations_dir()) {
            if version >= MIGRATION_VERSION {
                break;
            }
            // Versions below the creator contract are already applied by the
            // bootstrap above.
            if version == 0 || version < 142 {
                continue;
            }
            let sql = std::fs::read_to_string(&path).expect("read migration sql");
            conn.execute(sql.as_str())
                .await
                .unwrap_or_else(|err| panic!("apply migration {} failed: {err}", path.display()));
        }

        // A pre-existing, in-contract controller-window row must survive the
        // hardening: the new constraints are validated against it, not around it.
        conn.execute(
            "INSERT INTO credentials (id, provider_id, key_name, encrypted_value) \
             VALUES ('hb3s-prior', 'zai', 'hb3s-prior-key', decode('00', 'hex'))",
        )
        .await
        .expect("seed prior credential");
        let pool_id: i64 = sqlx::query_scalar(
            "INSERT INTO model_turn_pools (credential_id, provider_id, model_id) \
             VALUES ('hb3s-prior', 'zai', 'glm-5') RETURNING id",
        )
        .fetch_one(&mut conn)
        .await
        .expect("seed prior pool");
        sqlx::query(
            "INSERT INTO model_turn_controller_windows \
             (pool_id, window_sequence, started_at, ended_at, admitted_turns, completed_turns, summary) \
             VALUES ($1, 2, '1970-01-01T00:02:00Z'::timestamptz, '1970-01-01T00:03:00Z'::timestamptz, 5, 4, $2)",
        )
        .bind(pool_id)
        .bind(TRAINABLE)
        .execute(&mut conn)
        .await
        .expect("seed prior controller window");

        let sql = std::fs::read_to_string(migrations_dir().join(MIGRATION_FILE))
            .expect("read hardening migration sql");
        conn.execute(sql.as_str())
            .await
            .expect("apply hardening migration after prior migrations");

        let surviving: Vec<(i64, String)> = sqlx::query_as(
            "SELECT window_sequence, summary FROM model_turn_controller_windows WHERE pool_id = $1",
        )
        .bind(pool_id)
        .fetch_all(&mut conn)
        .await
        .expect("read back the pre-existing window");
        assert_eq!(surviving, vec![(2, TRAINABLE.to_owned())]);
        drop(conn);

        assert_hardened_schema(&db_url).await;
    })
    .await;
}

#[test]
fn hardening_migration_version_is_unique() {
    let versions: Vec<u64> = migration_entries(&migrations_dir())
        .into_iter()
        .map(|(version, _)| version)
        .filter(|version| *version == MIGRATION_VERSION)
        .collect();
    assert_eq!(
        versions.len(),
        1,
        "migration version {MIGRATION_VERSION} must be owned by exactly one file"
    );
}

/// Write a controller-window row with no Rust-side validation at all, so each
/// assertion below is about what the durable ledger accepts, not about what the
/// typed boundary happens to forward to it.
#[allow(clippy::too_many_arguments)]
async fn raw_insert(
    db: &Database,
    pool_id: i64,
    window_sequence: i64,
    started_at: &str,
    ended_at: &str,
    admitted_turns: i64,
    completed_turns: i64,
    summary: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO model_turn_controller_windows \
         (pool_id, window_sequence, started_at, ended_at, admitted_turns, completed_turns, summary) \
         VALUES ($1, $2, $3::timestamptz, $4::timestamptz, $5, $6, $7) \
         ON CONFLICT (pool_id, window_sequence) DO UPDATE SET \
           started_at = EXCLUDED.started_at, ended_at = EXCLUDED.ended_at, \
           admitted_turns = EXCLUDED.admitted_turns, \
           completed_turns = EXCLUDED.completed_turns, summary = EXCLUDED.summary",
    )
    .bind(pool_id)
    .bind(window_sequence)
    .bind(started_at)
    .bind(ended_at)
    .bind(admitted_turns)
    .bind(completed_turns)
    .bind(summary)
    .execute(db.pool())
    .await
    .map(|_| ())
}

const TRAINABLE: &str =
    r#"{"provider_id":"zai","model_id":"glm-5","trainable":true,"diagnostics":[]}"#;

/// `(label, window_sequence, started_at, ended_at, admitted, completed, summary)`.
type RejectedWindow = (&'static str, i64, String, String, i64, i64, Option<String>);

#[tokio::test]
async fn hardened_ledger_accepts_only_exact_aligned_closed_windows() {
    let db = Database::ephemeral().await.expect("db");
    let pool_id = seed_scoped_model_turn_admission_fixture(
        &db,
        "hb3s-owning",
        "zai",
        "glm-5",
        "shadow",
        "supported",
        1,
    )
    .await;
    let unrelated = seed_scoped_model_turn_admission_fixture(
        &db,
        "hb3s-unrelated",
        "zai",
        "glm-5",
        "shadow",
        "supported",
        1,
    )
    .await;

    // Exactly two shapes are durable: the aligned trainable window, and an
    // aligned diagnostic window carrying only its own pool plus the fixed zero
    // sentinel.
    raw_insert(
        &db,
        pool_id,
        2,
        "1970-01-01T00:02:00Z",
        "1970-01-01T00:03:00Z",
        5,
        4,
        Some(TRAINABLE),
    )
    .await
    .expect("exact aligned trainable window is durable");
    raw_insert(
        &db,
        pool_id,
        3,
        "1970-01-01T00:03:00Z",
        "1970-01-01T00:04:00Z",
        5,
        4,
        Some(&format!(
            r#"{{"provider_id":"zai","model_id":"glm-5","trainable":false,"diagnostics":[{{"pool_id":0,"code":"missing_capability"}},{{"pool_id":{pool_id},"code":"missing_usage"}}]}}"#
        )),
    )
    .await
    .expect("pool-local diagnostic window is durable");

    let overlong = "x".repeat(192);
    let many = (0..65)
        .map(|_| format!(r#"{{"pool_id":{pool_id},"code":"missing_usage"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let rejected: Vec<RejectedWindow> = vec![
        (
            "subsecond start",
            2,
            "1970-01-01T00:02:00.5Z".into(),
            "1970-01-01T00:03:00.5Z".into(),
            0,
            0,
            Some(TRAINABLE.into()),
        ),
        (
            "unaligned start",
            2,
            "1970-01-01T00:02:30Z".into(),
            "1970-01-01T00:03:30Z".into(),
            0,
            0,
            Some(TRAINABLE.into()),
        ),
        (
            "ninety second span",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:30Z".into(),
            0,
            0,
            Some(TRAINABLE.into()),
        ),
        (
            "thirty second span",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:02:30Z".into(),
            0,
            0,
            Some(TRAINABLE.into()),
        ),
        (
            "sequence disagrees with start",
            9,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(TRAINABLE.into()),
        ),
        (
            "negative admitted count",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            -1,
            0,
            Some(TRAINABLE.into()),
        ),
        (
            "negative completed count",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            -1,
            Some(TRAINABLE.into()),
        ),
        (
            "absent summary",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            None,
        ),
        (
            "summary that is not json",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some("not json".into()),
        ),
        (
            "summary that is not an object",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some("[1,2]".into()),
        ),
        (
            "extra top-level key",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(
                r#"{"provider_id":"zai","model_id":"glm-5","trainable":true,"diagnostics":[],"reporter_text":"leak"}"#
                    .into(),
            ),
        ),
        (
            "missing top-level key",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(r#"{"provider_id":"zai","model_id":"glm-5","trainable":true}"#.into()),
        ),
        (
            "blank provider label",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(
                r#"{"provider_id":"   ","model_id":"glm-5","trainable":true,"diagnostics":[]}"#
                    .into(),
            ),
        ),
        (
            "overlong model label",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(format!(
                r#"{{"provider_id":"zai","model_id":"{overlong}","trainable":true,"diagnostics":[]}}"#
            )),
        ),
        (
            "non-boolean trainable",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(
                r#"{"provider_id":"zai","model_id":"glm-5","trainable":"yes","diagnostics":[]}"#
                    .into(),
            ),
        ),
        (
            "diagnostics that are not an array",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(
                r#"{"provider_id":"zai","model_id":"glm-5","trainable":false,"diagnostics":{}}"#
                    .into(),
            ),
        ),
        (
            "trainable with diagnostics",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(format!(
                r#"{{"provider_id":"zai","model_id":"glm-5","trainable":true,"diagnostics":[{{"pool_id":{pool_id},"code":"missing_usage"}}]}}"#
            )),
        ),
        (
            "unbounded diagnostics",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(format!(
                r#"{{"provider_id":"zai","model_id":"glm-5","trainable":false,"diagnostics":[{many}]}}"#
            )),
        ),
        (
            "unknown reason code",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(format!(
                r#"{{"provider_id":"zai","model_id":"glm-5","trainable":false,"diagnostics":[{{"pool_id":{pool_id},"code":"free_text"}}]}}"#
            )),
        ),
        (
            "diagnostic carrying a slot identifier",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(format!(
                r#"{{"provider_id":"zai","model_id":"glm-5","trainable":false,"diagnostics":[{{"pool_id":{pool_id},"code":"missing_usage","slot_pod_uid":"leak"}}]}}"#
            )),
        ),
        (
            "diagnostic missing its pool identity",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(r#"{"provider_id":"zai","model_id":"glm-5","trainable":false,"diagnostics":[{"code":"missing_usage"}]}"#.into()),
        ),
        (
            "another pool's positive identity",
            2,
            "1970-01-01T00:02:00Z".into(),
            "1970-01-01T00:03:00Z".into(),
            0,
            0,
            Some(format!(
                r#"{{"provider_id":"zai","model_id":"glm-5","trainable":false,"diagnostics":[{{"pool_id":{unrelated},"code":"missing_usage"}}]}}"#
            )),
        ),
    ];
    for (label, sequence, started_at, ended_at, admitted, completed, summary) in rejected {
        let outcome = raw_insert(
            &db,
            pool_id,
            sequence,
            &started_at,
            &ended_at,
            admitted,
            completed,
            summary.as_deref(),
        )
        .await;
        assert!(
            outcome.is_err(),
            "{label} must be refused by the durable controller-window ledger"
        );
    }

    // The two accepted rows are still exactly what was written; a refused write
    // never partially replaced them.
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT window_sequence, summary FROM model_turn_controller_windows \
         WHERE pool_id = $1 ORDER BY window_sequence",
    )
    .bind(pool_id)
    .fetch_all(db.pool())
    .await
    .expect("read back durable windows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, 2);
    assert_eq!(rows[0].1, TRAINABLE);
}

/// The persisted summary is privacy-bounded by construction: only opaque pool
/// identities, closed reason codes, and the coordinator's catalog-qualified
/// labels can survive a write.
#[tokio::test]
async fn durable_summaries_carry_no_sensitive_identifier() {
    let db = Database::ephemeral().await.expect("db");
    let pool_id = seed_scoped_model_turn_admission_fixture(
        &db,
        "hb3s-privacy",
        "zai",
        "glm-5",
        "shadow",
        "supported",
        1,
    )
    .await;
    for forbidden in [
        "reporter_text",
        "slot_pod_uid",
        "deployment_revision",
        "attempt_fingerprint",
        "credential_id",
        "user_id",
        "account_id",
        "project_id",
        "request_id",
        "lease_id",
        "request_body",
    ] {
        let top_level = format!(
            r#"{{"provider_id":"zai","model_id":"glm-5","trainable":false,"diagnostics":[],"{forbidden}":"leak"}}"#
        );
        let per_diagnostic = format!(
            r#"{{"provider_id":"zai","model_id":"glm-5","trainable":false,"diagnostics":[{{"pool_id":{pool_id},"code":"missing_usage","{forbidden}":"leak"}}]}}"#
        );
        for summary in [top_level, per_diagnostic] {
            assert!(
                raw_insert(
                    &db,
                    pool_id,
                    2,
                    "1970-01-01T00:02:00Z",
                    "1970-01-01T00:03:00Z",
                    0,
                    0,
                    Some(&summary),
                )
                .await
                .is_err(),
                "a summary carrying {forbidden} must be refused"
            );
        }
    }
    let stored: Option<String> =
        sqlx::query_scalar("SELECT summary FROM model_turn_controller_windows WHERE pool_id = $1")
            .bind(pool_id)
            .fetch_optional(db.pool())
            .await
            .expect("read back durable windows");
    assert!(stored.is_none(), "no forbidden summary reached the ledger");
}
