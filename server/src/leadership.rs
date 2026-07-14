//! Single-active-coordinator leadership via a Postgres session-scoped
//! advisory lock.
//!
//! ## Why this exists
//!
//! The server is both the HTTP/UI/API plane *and* the coordinator (task
//! dispatch, reaping, maintenance, the worker RPC listener). Those two roles have
//! different correctness requirements during a deploy:
//!
//!   * The HTTP plane wants **zero downtime** — a `Recreate` rollout (kill old
//!     pod, then start new) leaves a multi-second window with no Ready endpoint
//!     and the ingress returns 503.
//!   * The coordinator must be a **singleton** — two pods both dispatching Jobs
//!     and writing Postgres would double-dispatch and race.
//!
//! A `RollingUpdate` (maxSurge=1, maxUnavailable=0) fixes the 503 by bringing
//! the new pod to Ready *before* the old one terminates — but that means two
//! server pods briefly coexist, which is unsafe for the coordinator unless
//! exactly one of them is active.
//!
//! This module is that gate. Every server process serves HTTP immediately
//! (readiness is independent of leadership — otherwise maxUnavailable=0 would
//! deadlock: the old pod won't terminate until the new one is Ready, and the
//! new one can't win the lock until the old one terminates). In parallel, each
//! process races for a Postgres advisory lock; the winner calls
//! [`on_acquire`](run_with_leadership) to start the coordinator and the other
//! active subsystems. The loser stays in HTTP-only standby. When the old
//! (leader) pod terminates it releases the lock and the new pod promotes.
//!
//! ## Why an advisory lock (not a DB lease row)
//!
//! A session-scoped advisory lock is auto-released the instant the holding
//! connection drops — pod crash, OOM-kill, node failure, network partition.
//! There is no TTL to tune and no stale-lease window: the next pod's
//! `pg_try_advisory_lock` simply starts succeeding. The cost is that we must
//! hold a dedicated connection open for the lifetime of leadership, and if that
//! connection dies we have *lost* the lock — so we exit the process rather than
//! risk running active subsystems unlocked (k8s restarts us, and we rejoin as
//! standby or re-acquire).
//!
//! The two-argument lock form (`classid`, `objid`) lives in a separate lock
//! space from the single-argument form sqlx's migrator uses, so there is no
//! possibility of colliding with the migration lock regardless of key values.
//!
//! ## Assumption: a direct connection, not a transaction pooler
//!
//! Session-level advisory locks require a sticky connection. The DSN here must
//! point at Postgres directly (RDS), not a transaction-pooling PgBouncer — in
//! transaction-pooling mode the lock would be released at the end of the
//! implicit transaction and leadership would be meaningless.

use std::time::Duration;

use sqlx::Connection;
use sqlx::postgres::PgConnection;
use tokio_util::sync::CancellationToken;

/// Advisory-lock classid: ASCII "djin" (0x646A696E). Arbitrary-but-fixed
/// namespace for djinn's own process-level locks.
const LOCK_CLASSID: i32 = 0x646A_696E;
/// Advisory-lock objid for the singleton coordinator. Bump only if you ever
/// need a *second* independent singleton role.
const LOCK_OBJID: i32 = 1;

/// How often a standby retries the lock, and how often the leader pings its
/// held connection to detect loss. Kept short so promotion after the old pod
/// exits is sub-deploy-window; the query is a trivial `pg_try_advisory_lock`.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Race for leadership, then run the active subsystems on the winner.
///
/// * `dsn` — Postgres DSN for the dedicated lock connection. `None` means
///   "no advisory lock" (in-process / non-Postgres / dev): we become leader
///   immediately. The caller passes `None` outside the Kubernetes runtime so
///   single-process dev and tests don't depend on a reachable Postgres for the
///   lock.
/// * `cancel` — process-wide shutdown token. On cancel we release the lock (if
///   held) and return.
/// * `on_acquire` — called **exactly once**, when this process becomes leader.
///   Starts the coordinator + slot pool, the worker RPC listener, and the other
///   mutating/periodic subsystems.
///
/// This future runs for the life of the process: it does not return after
/// `on_acquire`; it holds the lock until cancellation (or exits the process if
/// the lock connection is lost). Spawn it as a background task alongside the
/// HTTP server.
pub async fn run_with_leadership<F, Fut>(
    dsn: Option<String>,
    cancel: CancellationToken,
    on_acquire: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let Some(dsn) = dsn else {
        tracing::info!(
            "leadership: no advisory-lock DSN (non-Kubernetes / single-process); \
             running as sole leader"
        );
        on_acquire().await;
        cancel.cancelled().await;
        return;
    };

    // Dedicated connection, deliberately *not* from the app pool: a
    // session-scoped advisory lock must live on a connection we never hand back
    // to the pool (and the app pool's `statement_timeout` / lifetime settings
    // must not apply to a connection we intend to hold idle indefinitely).
    let mut conn = loop {
        if cancel.is_cancelled() {
            return;
        }
        match PgConnection::connect(&dsn).await {
            Ok(conn) => break conn,
            Err(e) => {
                tracing::warn!(error = %e, "leadership: lock connection failed; retrying");
                if wait_or_cancelled(&cancel).await {
                    return;
                }
            }
        }
    };

    // ── Acquisition loop: poll until we win the lock or are cancelled. ──
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match try_acquire(&mut conn).await {
            Ok(true) => {
                tracing::info!(
                    "leadership: acquired coordinator advisory lock — promoting to active"
                );
                break;
            }
            Ok(false) => {
                tracing::debug!(
                    "leadership: another pod holds the coordinator lock; HTTP-only standby"
                );
                if wait_or_cancelled(&cancel).await {
                    return;
                }
            }
            Err(e) => {
                // The lock connection is suspect. Rather than reconnect and
                // risk a subtle double-acquire, exit and let k8s restart us.
                tracing::error!(
                    error = %e,
                    "leadership: advisory-lock query failed — exiting to restart clean"
                );
                std::process::exit(1);
            }
        }
    }

    // ── We are the leader: start the active subsystems exactly once. ──
    on_acquire().await;

    // ── Hold the lock for the rest of the process lifetime. ──
    // Ping the connection each interval; if it has dropped we have lost the
    // lock (another pod can now acquire it), so exit rather than keep running
    // active subsystems unlocked.
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("leadership: shutting down — releasing coordinator advisory lock");
                let _ = sqlx::query("SELECT pg_advisory_unlock($1, $2)")
                    .bind(LOCK_CLASSID)
                    .bind(LOCK_OBJID)
                    .execute(&mut conn)
                    .await;
                return;
            }
            _ = tokio::time::sleep(POLL_INTERVAL) => {
                if let Err(e) = sqlx::query("SELECT 1").execute(&mut conn).await {
                    tracing::error!(
                        error = %e,
                        "leadership: lock connection lost — exiting to restart clean"
                    );
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Try to take the coordinator lock. Returns `Ok(true)` if this connection now
/// holds it, `Ok(false)` if another session does.
async fn try_acquire(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1, $2)")
        .bind(LOCK_CLASSID)
        .bind(LOCK_OBJID)
        .fetch_one(conn)
        .await
}

/// Sleep one poll interval unless cancelled first. Returns `true` if cancelled.
async fn wait_or_cancelled(cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(POLL_INTERVAL) => false,
    }
}
