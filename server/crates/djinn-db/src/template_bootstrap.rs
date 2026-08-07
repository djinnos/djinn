//! djinn-repo-specific test scaffolding ("dogfooding").
//!
//! This module bootstraps djinn's OWN `djinn_test_template` database by running
//! djinn's OWN sqlx migrations, so that djinn agents editing the djinn repo can
//! run djinn's DB-backed tests inside a worker pod whose image provides a bare
//! Postgres sidecar. It is intrinsically coupled to djinn's schema/migrations.
//!
//! # Advisory-lock in-process exception
//!
//! This helper is an **intentional advisory-lock in-process exception** for
//! djinn's own repo.  It runs compiled djinn migrations inside the worker
//! process using `pg_advisory_lock` to serialize concurrent template creation
//! across pods.  It is **not** the generic mechanism for target repos and must
//! not be generalized in place — the `djinn_test_template` name, the
//! `migrations_postgres` path, and the advisory lock id are all hardcoded to
//! djinn's schema on purpose.
//!
//! A future migration could express the equivalent generic shape through
//! `lifecycle.pre_task`, for example:
//!
//! ```yaml
//! # Generic equivalent for any target repo (not djinn itself):
//! lifecycle:
//!   pre_task:
//!     - name: migrate-test-db
//!       command: psql "$TEST_POSTGRES_URL" -f schema.sql
//!       timeout_seconds: 120
//!       failure_policy: blocking
//! ```
//!
//! or a sqlx-migrate / Rails / Django / Prisma equivalent consuming the
//! injected `TEST_POSTGRES_URL` (or framework-specific env var).  Djinn's own
//! `djinn_test_template` bootstrap stays here because it needs in-process
//! advisory locking and the `sqlx::migrate!` macro — neither of which the
//! shell-based `pre_task` runner provides today.
//!
//! # Where the generic path lives
//!
//! The generic, reusable half of this capability lives in `djinn-k8s`: service
//! preset injection (`sidecar::resolve_image_services_with_metadata`) stands up
//! the sidecar and exposes an injected connection env var. The intended reusable
//! path for a non-djinn target repo to bootstrap its own test DB is a
//! project-declared lifecycle / pre-task bootstrap command run against that
//! injected connection env var — NOT code here in `djinn-db`.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::ConnectOptions;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{DbError, DbResult};
use crate::migrations::{self, MigrationContext};

/// Process-wide semaphore guarding `djinn_test_template` bootstrap.
///
/// Multiple worker pods or test processes may hit the bare sidecar at the same
/// time. `CREATE DATABASE` and sqlx migrate both conflict if run concurrently
/// against the same template DB. A single-process semaphore stops intra-process
/// races; across pods, the Postgres advisory lock (below) serializes.
static TEMPLATE_BOOTSTRAP_SEMAPHORE: std::sync::OnceLock<Arc<Semaphore>> =
    std::sync::OnceLock::new();

/// Process-wide latch: set once the template has been created/verified in this
/// process so every later `ensure_test_template` call short-circuits. Without
/// this, bootstrap opens a fresh client connection to `djinn_test_template` on
/// *every* test's first query (to verify migrations), and that connection is
/// what a concurrent `clone_postgres_test_template` `pg_terminate_backend`
/// races with — the `57P01 "terminating connection due to administrator
/// command"` flake. Verifying once per process removes almost all of those
/// template connections; the advisory lock (below) closes the remaining window
/// for the single first-test bootstrap.
static TEMPLATE_READY: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Environment latch for "this template was already built **and verified** by
/// the harness that launched me; do not re-verify".
///
/// [`TEMPLATE_READY`] above is per-**process**, and `cargo nextest` runs every
/// test in its own process. So in CI the fast path above never fires: each of
/// the ~12k test processes re-took the EXCLUSIVE advisory lock, re-ran
/// `UPDATE pg_database SET datistemplate = TRUE` (a shared-catalog write), and
/// re-walked all ~190 embedded migrations against the template — while every
/// peer's `clone_postgres_test_template` queued behind that exclusive lock in
/// SHARED mode. One serialized maintenance window per test, on a lock every
/// test also needs to clone through.
///
/// Setting `DJINN_TEST_TEMPLATE_PREBUILT=1` (or `true`) asserts that the
/// template is already present, marked, and fully migrated, so
/// [`ensure_test_template`] returns immediately and the clone path takes the
/// shared lock uncontended.
///
/// **This is an assertion the setter must prove, not a hint.** It is set by
/// `.github/workflows/quality-gate.yml` only *after* its "Build Postgres test
/// template" step has run `tests/setup_test_template.rs`, which now verifies
/// every embedded migration is applied to the template with a matching
/// checksum and prints a sentinel the workflow greps for before exporting the
/// variable. Nothing sets it locally, so local/worker-pod runs keep today's
/// self-healing bootstrap (create-if-missing, migrate-forward, rebuild on a
/// diverged history).
static TEMPLATE_PREBUILT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Whether the environment asserts a pre-built, pre-verified template.
///
/// Read once per process: `std::env::var` is not free and this is on the hot
/// path of every DB-backed test.
fn template_prebuilt() -> bool {
    *TEMPLATE_PREBUILT.get_or_init(|| {
        std::env::var("DJINN_TEST_TEMPLATE_PREBUILT").is_ok_and(|raw| {
            let raw = raw.trim();
            raw.eq_ignore_ascii_case("1") || raw.eq_ignore_ascii_case("true")
        })
    })
}

fn bootstrap_semaphore() -> Arc<Semaphore> {
    TEMPLATE_BOOTSTRAP_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(1)))
        .clone()
}

/// Advisory lock id used while creating/migrating `djinn_test_template`.
///
/// Chosen as a deterministic 64-bit hash of the string "djinn_test_template".
///
/// `ensure_test_template` holds this in **exclusive** mode for the entire
/// maintenance window (including the template-connected migrate/verify);
/// `clone_postgres_test_template` takes it in **shared** mode around its
/// `pg_terminate_backend`. Exclusive⇄shared conflict means a clone can never
/// terminate a live bootstrap template connection, while concurrent clones
/// (all shared) never block each other.
pub(crate) const TEMPLATE_ADVISORY_LOCK_ID: i64 = 0x6a5b4c3d2e1f0a9b;

/// Upper bound on how long either side may WAIT for the template advisory lock.
///
/// Both `pg_advisory_lock` (exclusive, below) and `pg_advisory_lock_shared`
/// (in `clone_postgres_test_template`) run on raw `PgConnection`s, which do
/// **not** go through the pool's `after_connect` hook — so they inherited none
/// of the pool's `statement_timeout` / `lock_timeout` guards and the wait was
/// unbounded. Under `--test-threads 4` against one Postgres server that is an
/// unbounded wait on a lock held across a full sqlx migrate, and it presents as
/// an unattributable nextest slow-timeout kill rather than as a lock problem
/// (e.g. `batch_completion_suppressed_when_active_planner_on_epic`, ~5% of
/// runs).
///
/// `lock_timeout` does apply to advisory locks — verified against PostgreSQL 16:
/// a contended `SELECT pg_advisory_lock(...)` under `SET lock_timeout = '2s'`
/// returns `55P03 canceling statement due to lock timeout` at 2.01s, where the
/// same statement without it blocks indefinitely.
///
/// 60s matches the local-semaphore budget in [`acquire_bootstrap_permit`] and
/// sits under nextest's hard kill (`slow-timeout` `period = 30s`,
/// `terminate-after = 3` => 90s), so a wedged lock now surfaces as a named
/// Postgres error inside the test instead of a killed process with no cause.
const ADVISORY_LOCK_WAIT_TIMEOUT_MS: u32 = 60_000;

/// Bound how long `conn` will wait for a lock before erroring.
///
/// Session-scoped, and both callers use a short-lived connection they close
/// immediately afterwards, so this can never reach pooled query traffic.
pub(crate) async fn bound_advisory_lock_wait(
    conn: &mut sqlx::postgres::PgConnection,
) -> DbResult<()> {
    sqlx::query(&format!("SET lock_timeout = {ADVISORY_LOCK_WAIT_TIMEOUT_MS}"))
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(())
}

/// Ensure the `djinn_test_template` database exists and is migrated.
///
/// Idempotent: if the template already exists and is marked as a template, this
/// is a no-op. If it exists but is not marked as a template, it is marked and
/// migrations are verified. If it does not exist, it is created and migrated.
///
/// Uses both a local semaphore (per-process) and a Postgres advisory lock
/// (cross-process) so parallel tests / worker pods don't collide.
pub(crate) async fn ensure_test_template(server_prefix: &str) -> DbResult<()> {
    // Fast path: the template was already created/verified in this process, or
    // the harness that launched this process proved it before launching us
    // (see `TEMPLATE_PREBUILT`). The second case is what makes the fast path
    // reachable at all under `cargo nextest`, which forks a process per test.
    if TEMPLATE_READY.get().is_some() || template_prebuilt() {
        return Ok(());
    }

    let _permit = acquire_bootstrap_permit().await;

    // Re-check under the local permit: a peer thread may have completed the
    // bootstrap while we waited on the semaphore.
    if TEMPLATE_READY.get().is_some() {
        return Ok(());
    }

    let admin_url = format!("{server_prefix}/postgres");
    let opts = sqlx::postgres::PgConnectOptions::from_str(&admin_url).map_err(DbError::from)?;
    let mut conn = opts.connect().await.map_err(DbError::from)?;

    // Never wait forever for the lock: this is a raw connection, so it has none
    // of the pool's guards. See `ADVISORY_LOCK_WAIT_TIMEOUT_MS`.
    bound_advisory_lock_wait(&mut conn).await?;

    // Cross-process serialization: take an EXCLUSIVE advisory lock for the
    // duration of our maintenance work on the template — including the
    // template-connected migrate/verify below. `clone_postgres_test_template`
    // takes the same lock in SHARED mode around its `pg_terminate_backend`, so
    // a concurrent clone can never terminate our live template connection
    // (the `57P01` "terminating connection due to administrator command" flake).
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEMPLATE_ADVISORY_LOCK_ID)
        .execute(&mut conn)
        .await
        .map_err(DbError::from)?;

    // Do the maintenance work under the lock, then always release it — even on
    // error — so a failed bootstrap never wedges peers waiting on the lock.
    let result = ensure_test_template_locked(server_prefix, &mut conn).await;
    let _ = unlock_template_bootstrap(&mut conn).await;
    drop(conn);
    result?;

    // Only latch success: a failed bootstrap must be retried by the next test.
    let _ = TEMPLATE_READY.set(());
    Ok(())
}

/// Body of [`ensure_test_template`], run while the exclusive advisory lock is
/// held on `conn`. Any template-connected work (migrate on create, verify on
/// pre-existing) is therefore protected from a concurrent clone's
/// `pg_terminate_backend`.
async fn ensure_test_template_locked(
    server_prefix: &str,
    conn: &mut sqlx::postgres::PgConnection,
) -> DbResult<()> {
    // Does the template exist?
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_database WHERE datname = 'djinn_test_template'",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;

    if exists == 0 {
        create_and_migrate_template(server_prefix, conn).await?;
        return Ok(());
    }

    // Template exists: ensure it is marked as a template and migrations are up
    // to date. This covers the "someone created the DB but didn't run sqlx" case.
    sqlx::query(
        "UPDATE pg_database SET datistemplate = TRUE WHERE datname = 'djinn_test_template'",
    )
    .execute(&mut *conn)
    .await
    .map_err(DbError::from)?;

    // The exclusive advisory lock is still held on `conn`, so migration's own
    // connection to the template cannot be terminated by a racing clone. A
    // long-lived test server may have a template from an older checkout; bring
    // it forward before cloning instead of making every test fail verification.
    if let Err(error) = run_template_migrations(server_prefix).await {
        let incompatible_history = matches!(
            &error,
            DbError::InvalidData(message)
                if message.contains("previously applied but is missing in the resolved migrations")
        );
        if !incompatible_history {
            return Err(error);
        }

        // A shared review server can retain a template produced by another
        // branch whose migration history diverged. It is disposable test data,
        // so rebuild it under the same exclusive lock rather than weakening
        // migration validation for real databases.
        rebuild_test_template(server_prefix, conn).await?;
    }

    Ok(())
}

async fn unlock_template_bootstrap(conn: &mut sqlx::postgres::PgConnection) -> DbResult<()> {
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(TEMPLATE_ADVISORY_LOCK_ID)
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(())
}

async fn acquire_bootstrap_permit() -> Option<OwnedSemaphorePermit> {
    let sem = bootstrap_semaphore();
    // Don't block forever: if the lock is held for an unexpectedly long time,
    // fail loudly rather than hang silently. Postgres advisory lock + migrations
    // take seconds, so a 60s budget is generous.
    match tokio::time::timeout(Duration::from_secs(60), sem.acquire_owned()).await {
        Ok(Ok(permit)) => Some(permit),
        Ok(Err(_)) => None, // semaphore closed; shouldn't happen
        Err(_) => {
            tracing::error!("timeout waiting for local template bootstrap permit");
            None
        }
    }
}

async fn create_and_migrate_template(
    server_prefix: &str,
    conn: &mut sqlx::postgres::PgConnection,
) -> DbResult<()> {
    sqlx::query("CREATE DATABASE \"djinn_test_template\"")
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;

    sqlx::query(
        "UPDATE pg_database SET datistemplate = TRUE WHERE datname = 'djinn_test_template'",
    )
    .execute(&mut *conn)
    .await
    .map_err(DbError::from)?;

    run_template_migrations(server_prefix).await
}

async fn rebuild_test_template(
    server_prefix: &str,
    conn: &mut sqlx::postgres::PgConnection,
) -> DbResult<()> {
    sqlx::query(
        "UPDATE pg_database SET datistemplate = FALSE WHERE datname = 'djinn_test_template'",
    )
    .execute(&mut *conn)
    .await
    .map_err(DbError::from)?;
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = 'djinn_test_template' AND pid <> pg_backend_pid()",
    )
    .execute(&mut *conn)
    .await
    .map_err(DbError::from)?;
    sqlx::query("DROP DATABASE \"djinn_test_template\"")
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    create_and_migrate_template(server_prefix, conn).await
}

async fn run_template_migrations(server_prefix: &str) -> DbResult<()> {
    let template_url = format!("{server_prefix}/djinn_test_template");
    // Reserved exclusively for the clone template; production code has no
    // access to this constant or this module.
    const TEMPLATE_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000001";
    migrations::bootstrap_designated_operator(
        &template_url,
        &migrations::DesignatedOperatorBootstrap {
            user_id: TEMPLATE_OPERATOR_ID.to_owned(),
            github_id: 9_000_000_001,
            github_login: "djinn-test-template-operator".to_owned(),
            github_name: Some("Djinn test template operator".to_owned()),
            github_avatar_url: None,
        },
    )
    .await?;
    migrations::run_postgres_migrations(
        &template_url,
        &MigrationContext {
            designated_operator_user_id: Some(TEMPLATE_OPERATOR_ID.to_owned()),
        },
    )
    .await
}
