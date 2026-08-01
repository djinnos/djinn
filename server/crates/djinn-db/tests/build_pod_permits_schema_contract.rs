//! The `build_pod_permits` schema contract, asserted against `information_schema`
//! and `pg_indexes` on a database migrated to HEAD.
//!
//! # Why this is not asserted against a migration filename
//!
//! `migrations_build_pod_permits.rs` proves what migrations **162** and **164**
//! do by replaying the schema up to a numbered file. That is the right shape for
//! "this migration is additive and rewrites nothing", and it stays.
//!
//! It is the wrong shape for "the relation the Kueue cutover promised to RETAIN
//! is still here". Migration filenames renumber — a rebase, a squash, or a
//! competing branch landing first moves `162` — and an assertion keyed to a
//! number can start passing against a different file, or start failing for a
//! reason that has nothing to do with the schema. This file asks the database
//! what it actually has.
//!
//! # What it is guarding against
//!
//! The Kueue cutover (`o53p`) deletes the pre-create pods-quota reservation
//! authority — the `admission_journal` relation, the `BuildAdmissionController`
//! and its readiness ladder — while explicitly RETAINING `build_pod_permits`,
//! its write-once identity/CAS API, and the `pod_uid` partial unique index. A
//! future DROP migration (`flc5`) must not take this relation with it. That
//! carve-out is only real if something fails when it stops being true.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

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

/// Every column the pod-permit resize identity (`4acu`) added, plus the
/// original permit identity columns. Names AND types, because a column that
/// silently changed type is a corruption the repository would not notice until
/// a runtime decode failure in production.
const REQUIRED_COLUMNS: &[(&str, &str)] = &[
    // Migration 162 — permit identity and the release event.
    ("task_run_id", "character varying"),
    ("permit_id", "uuid"),
    ("fencing_token", "bigint"),
    ("state", "character varying"),
    ("acquired_at", "timestamp with time zone"),
    ("job_uid", "character varying"),
    ("released_at", "timestamp with time zone"),
    ("released_fencing_token", "bigint"),
    ("release_reason", "character varying"),
    // Migration 164 — the 4acu resize identity.
    ("pod_namespace", "character varying"),
    ("pod_name", "character varying"),
    ("pod_uid", "character varying"),
    ("launcher_container_name", "character varying"),
    ("launcher_container_id", "character varying"),
    ("image_digest", "character varying"),
    ("observed_launcher_protocol", "character varying"),
    ("effective_launcher_protocol", "character varying"),
    ("admitted_cpu_millicores", "bigint"),
    // Migration 170 — how long the row has rested in `state`. The reconciler's
    // strand predicate reads it to tell a drop a live worker is performing from
    // one a dead worker abandoned; without it that distinction is inexpressible
    // and the reconciler steals in-flight drops.
    ("state_changed_at", "timestamp with time zone"),
];

#[tokio::test]
async fn a_database_migrated_to_head_retains_the_build_pod_permit_contract() {
    with_temp_database("build_pod_permits_head", |db_url| async move {
        djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect the migrated database");

        // ── The relation itself survives the cutover ────────────────────────
        let table: Option<String> = sqlx::query_scalar(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = 'build_pod_permits'",
        )
        .fetch_optional(&pool)
        .await
        .expect("query information_schema.tables");
        assert_eq!(
            table.as_deref(),
            Some("build_pod_permits"),
            "the Kueue cutover RETAINS build_pod_permits; a DROP migration that \
             takes it along with the pods-quota reservation ledger is the failure \
             this asserts against"
        );

        // ── Every documented column, with its type ─────────────────────────
        let observed: Vec<(String, String)> = sqlx::query_as(
            "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'build_pod_permits'",
        )
        .fetch_all(&pool)
        .await
        .expect("query information_schema.columns");

        for (name, data_type) in REQUIRED_COLUMNS {
            let found = observed
                .iter()
                .find(|(observed_name, _)| observed_name == name)
                .unwrap_or_else(|| {
                    panic!(
                        "build_pod_permits.{name} is missing; observed columns: {:?}",
                        observed.iter().map(|(n, _)| n).collect::<Vec<_>>()
                    )
                });
            assert_eq!(&found.1, data_type, "build_pod_permits.{name} changed type");
        }

        // ── The pod_uid PARTIAL unique index ───────────────────────────────
        //
        // Partial is load-bearing and is the whole reason this is asserted on
        // the index DEFINITION rather than merely on its existence. A permit
        // that has not yet observed its Pod carries `pod_uid IS NULL`, and
        // several may coexist; a plain UNIQUE index would collapse every
        // unobserved permit into one and reject the second acquisition.
        let index_def: Option<String> = sqlx::query_scalar(
            "SELECT indexdef FROM pg_indexes \
             WHERE schemaname = 'public' AND tablename = 'build_pod_permits' \
               AND indexname = 'build_pod_permits_pod_uid_key'",
        )
        .fetch_optional(&pool)
        .await
        .expect("query pg_indexes");
        let index_def = index_def.expect("build_pod_permits_pod_uid_key exists");
        assert!(
            index_def.contains("UNIQUE"),
            "the pod_uid index must be UNIQUE: {index_def}"
        );
        assert!(
            index_def.contains("(pod_uid)"),
            "the pod_uid index must key on pod_uid: {index_def}"
        );
        assert!(
            index_def.contains("WHERE (pod_uid IS NOT NULL)"),
            "the pod_uid index must be PARTIAL on `pod_uid IS NOT NULL`, or every \
             not-yet-observed permit collides on NULL: {index_def}"
        );

        // ── The canonical global pool row the repository locks ─────────────
        let pool_key: Option<String> = sqlx::query_scalar(
            "SELECT pool_key FROM build_pod_permit_pools WHERE pool_key = 'global'",
        )
        .fetch_optional(&pool)
        .await
        .expect("query build_pod_permit_pools");
        assert_eq!(
            pool_key.as_deref(),
            Some("global"),
            "the canonical pool row is what every permit transaction locks; \
             without it the repository cannot serialize acquisitions"
        );
    })
    .await;
}

/// The assertions above must be reachable from the migrations directory this
/// crate actually embeds, not from a stale copy. Cheap guard against the test
/// silently validating nothing because the runner pointed elsewhere.
#[test]
fn the_embedded_migrations_directory_is_the_one_under_test() {
    let dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres");
    assert!(
        Path::new(&dir).is_dir(),
        "migrations_postgres must exist at {}",
        dir.display()
    );
    let has_permits = std::fs::read_dir(&dir)
        .expect("read migrations dir")
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with("_build_pod_permits.sql")
        });
    assert!(
        has_permits,
        "a *_build_pod_permits.sql migration must exist (matched by SUFFIX, not \
         by number, because migration numbers renumber under rebase)"
    );
}
