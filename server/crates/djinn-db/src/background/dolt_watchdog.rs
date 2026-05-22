//! Background task that KILLs long-running Dolt queries.
//!
//! Dolt's lock manager is not as robust as InnoDB's: under concurrent writes
//! to hot tables (sessions / notes / tasks) it can produce lock cycles that
//! neither resolve themselves nor honor `lock_wait_timeout`. The symptom is a
//! cluster of writes hanging for minutes while the application sits idle. We
//! observed it 2026-05-22 — 6 queries hung 4-7 minutes, the coordinator's
//! tick loop froze because its next DB write was queued behind the deadlock,
//! and a human had to KILL the queries by hand.
//!
//! This watchdog automates the manual recovery. Every `tick_interval` it
//! queries `information_schema.processlist`, finds any query that has been in
//! the `Query` state for longer than `kill_after`, logs it at WARN with the
//! statement text, and issues `KILL <id>`. The killed connection's caller
//! sees a normal "connection reset" error and (in most call sites) retries.
//!
//! Scope: this is purely a safety net. Hangs that recur frequently still
//! indicate a real bug — the WARN logs are intentionally noisy so the
//! frequency stays visible.

use std::time::Duration;

use sqlx::mysql::MySqlPool;
use tokio::time::{Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::Database;
use crate::database::DatabaseBackendKind;

const DEFAULT_TICK_INTERVAL_SECS: u64 = 30;
const DEFAULT_KILL_AFTER_SECS: u32 = 60;
const TICK_INTERVAL_ENV: &str = "DJINN_DOLT_WATCHDOG_TICK_SECS";
const KILL_AFTER_ENV: &str = "DJINN_DOLT_WATCHDOG_KILL_AFTER_SECS";
/// Cap how much of a stuck statement we log. Some session/note UPDATEs are
/// long; truncating keeps the log readable without losing the head of the
/// query (which is usually enough to identify the culprit).
const STATEMENT_LOG_TRUNCATE: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WatchdogConfig {
    tick_interval: Duration,
    kill_after_secs: u32,
}

impl WatchdogConfig {
    fn from_env() -> Self {
        let tick_secs = std::env::var(TICK_INTERVAL_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TICK_INTERVAL_SECS);
        let kill_after_secs = std::env::var(KILL_AFTER_ENV)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_KILL_AFTER_SECS);
        Self {
            tick_interval: Duration::from_secs(tick_secs),
            kill_after_secs,
        }
    }
}

/// Spawn the watchdog background task. No-op on non-MySQL backends —
/// SQLite has neither the lock manager nor the `information_schema` plumbing
/// this relies on, and InnoDB has its own deadlock detection so the failure
/// mode this guards against doesn't apply.
pub fn spawn(db: Database, cancel: CancellationToken) {
    if !matches!(db.backend_kind(), DatabaseBackendKind::Mysql) {
        tracing::debug!("dolt_watchdog: skipping spawn — backend is not MySQL/Dolt");
        return;
    }

    let pool = db.pool().clone();
    let config = WatchdogConfig::from_env();
    tracing::info!(
        tick_secs = config.tick_interval.as_secs(),
        kill_after_secs = config.kill_after_secs,
        "dolt_watchdog: spawned"
    );
    tokio::spawn(async move {
        let mut ticker = watchdog_ticker(config.tick_interval);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("dolt_watchdog: cancellation received, exiting");
                    break;
                }
                _ = ticker.tick() => {
                    if let Err(error) = run_tick(&pool, config.kill_after_secs).await {
                        // Don't propagate — a transient watchdog failure
                        // (pool exhaustion, transient network blip) is not
                        // worth crashing the server over. Log + next tick.
                        tracing::warn!(error = %error, "dolt_watchdog: tick failed");
                    }
                }
            }
        }
    });
}

fn watchdog_ticker(interval: Duration) -> Interval {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker
}

/// Single watchdog tick: scan + kill.
async fn run_tick(pool: &MySqlPool, kill_after_secs: u32) -> sqlx::Result<()> {
    // `information_schema.processlist` is the same source as `SHOW PROCESSLIST`
    // but queryable — `SHOW` can't be used with parameters or filtered easily.
    // We filter to `command='Query'` so harmless `Sleep` connections don't
    // light up.
    let rows = sqlx::query_as::<_, StuckRow>(
        "SELECT id, time, info \
         FROM information_schema.processlist \
         WHERE command = 'Query' \
           AND time >= ? \
           AND info IS NOT NULL \
           AND info NOT LIKE '%information_schema.processlist%'",
    )
    .bind(kill_after_secs)
    .fetch_all(pool)
    .await?;

    for row in rows {
        let snippet = row.info.as_deref().map(truncate_statement).unwrap_or("");
        tracing::warn!(
            id = row.id,
            time_secs = row.time,
            statement = %snippet,
            "dolt_watchdog: KILLing stuck query"
        );
        // Sub-failures don't abort the whole tick — the next victim might
        // still be killable, and the cycle is what we want to break.
        let kill_sql = format!("KILL {}", row.id);
        if let Err(e) = sqlx::query(&kill_sql).execute(pool).await {
            tracing::warn!(id = row.id, error = %e, "dolt_watchdog: KILL failed");
        }
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct StuckRow {
    id: u64,
    time: u32,
    info: Option<String>,
}

fn truncate_statement(s: &str) -> &str {
    if s.len() <= STATEMENT_LOG_TRUNCATE {
        s
    } else {
        // Snap to a char boundary so non-ASCII statements don't panic.
        let mut end = STATEMENT_LOG_TRUNCATE;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &s[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_below_limit_returns_input() {
        assert_eq!(truncate_statement("SELECT 1"), "SELECT 1");
    }

    #[test]
    fn truncate_above_limit_clips_to_limit() {
        let long = "x".repeat(500);
        let out = truncate_statement(&long);
        assert_eq!(out.len(), STATEMENT_LOG_TRUNCATE);
    }

    #[test]
    fn truncate_handles_unicode_at_boundary() {
        // 198 ASCII bytes + a 2-byte char — naive slicing at byte 200 would
        // split the multi-byte char; we must back off to a boundary.
        let mut s = "x".repeat(STATEMENT_LOG_TRUNCATE - 2);
        s.push('é'); // 2 bytes
        s.push_str(&"x".repeat(50)); // pad to exceed the cap
        let out = truncate_statement(&s);
        // Must be valid UTF-8 and not panic — that's the real assertion.
        assert!(out.len() <= STATEMENT_LOG_TRUNCATE);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn config_defaults_apply_when_env_missing() {
        // SAFETY: this test is single-threaded by default in `cargo test`
        // unless explicitly parallelized.  Both vars are read-then-cleared.
        // SAFETY: env mutation in tests is single-threaded by cargo default.
        unsafe {
            std::env::remove_var(TICK_INTERVAL_ENV);
            std::env::remove_var(KILL_AFTER_ENV);
        }
        let cfg = WatchdogConfig::from_env();
        assert_eq!(cfg.tick_interval.as_secs(), DEFAULT_TICK_INTERVAL_SECS);
        assert_eq!(cfg.kill_after_secs, DEFAULT_KILL_AFTER_SECS);
    }
}
