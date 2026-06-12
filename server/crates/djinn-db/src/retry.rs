//! Serialization-failure / deadlock retry helper for Postgres transactions.
//!
//! Postgres raises SQLSTATE `40001` (`serialization_failure`) under
//! SERIALIZABLE isolation when two concurrent transactions touch
//! overlapping keys and the engine cannot serialize them; and SQLSTATE
//! `40P01` (`deadlock_detected`) when the deadlock detector breaks a
//! lock cycle. Both are benign outcomes — the loser should re-run the
//! transaction.
//!
//! Historical note: pre–postgres-cutover this helper was wired to Dolt's
//! MySQL error 1213. Dolt routinely emitted 1213 under benign
//! concurrency (the docs describe it as the default MVCC behaviour);
//! Postgres surfaces 40001 / 40P01 only on real contention. The retry
//! is kept because the tx bodies that wrap it (notes, tasks, links,
//! agents) are still hot under concurrent test runs.
//!
//! Call sites that `BEGIN ... COMMIT` on hot tables wrap the transaction
//! body in [`retry_on_serialization_failure`] so the retry is
//! transparent. Every retry opens a fresh `sqlx` transaction — we do not
//! try to resume one that already failed.

use std::time::Duration;

use crate::error::DbError;

/// Default retry cap. Three attempts is enough in practice for both
/// 40001 (`serialization_failure`) and 40P01 (`deadlock_detected`) on
/// Postgres — by attempt 3 the contending writer has almost always
/// finished.
pub const DEFAULT_MAX_TX_RETRIES: usize = 3;

/// Returns `true` iff `err` is a Postgres SQLSTATE that warrants a
/// transaction retry: `40001` (`serialization_failure`) or `40P01`
/// (`deadlock_detected`). Both encode a benign "loser of a concurrent
/// access contest" outcome; rerunning the tx is the canonical fix.
pub fn is_serialization_failure(err: &DbError) -> bool {
    let DbError::Sqlx(sqlx::Error::Database(db_err)) = err else {
        return false;
    };
    matches!(db_err.code().as_deref(), Some("40001") | Some("40P01"))
}

/// Run `op` and retry up to `max_attempts` times when it returns a Postgres
/// serialization failure (`40001`) or deadlock (`40P01`). Backoff is exponential
/// starting at 20 ms so the contending writer has a chance to finish before the
/// next attempt.
///
/// Non-serialization errors short-circuit immediately; any error from the
/// final attempt is returned verbatim.
///
/// `op` is a `FnMut` returning a fresh future per attempt — the caller
/// must re-open the transaction inside the closure because a retried
/// transaction starts clean.
pub async fn retry_on_serialization_failure<T, F, Fut>(
    max_attempts: usize,
    mut op: F,
) -> Result<T, DbError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DbError>>,
{
    debug_assert!(max_attempts >= 1, "retry requires at least one attempt");
    let mut attempt = 0usize;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(err) if attempt + 1 < max_attempts && is_serialization_failure(&err) => {
                attempt += 1;
                tokio::time::sleep(backoff_delay(attempt)).await;
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

fn backoff_delay(attempt: usize) -> Duration {
    // 40ms, 80ms, 160ms, ... capped at a sane upper bound so a runaway
    // retry count never pushes a request past the acquire_timeout.
    let ms = 20u64.saturating_mul(1u64 << attempt).min(500);
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[tokio::test]
    async fn short_circuits_on_non_serialization_error() {
        // Non-serialization errors must return on the first attempt —
        // retrying a constraint violation only hides the bug.
        let attempts = RefCell::new(0usize);
        let result: Result<(), DbError> = retry_on_serialization_failure(3, || async {
            *attempts.borrow_mut() += 1;
            Err(DbError::InvalidData("boom".into()))
        })
        .await;
        assert!(matches!(result, Err(DbError::InvalidData(_))));
        assert_eq!(*attempts.borrow(), 1);
    }

    #[tokio::test]
    async fn succeeds_on_first_attempt_without_retrying() {
        let attempts = RefCell::new(0usize);
        let result: Result<&str, DbError> = retry_on_serialization_failure(3, || async {
            *attempts.borrow_mut() += 1;
            Ok("ok")
        })
        .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(*attempts.borrow(), 1);
    }

    #[test]
    fn is_serialization_failure_matches_dberror_invaliddata_false() {
        assert!(!is_serialization_failure(&DbError::InvalidData(
            "some other error".into()
        )));
    }
}
