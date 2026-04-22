//! Serialization-failure retry helper for Dolt transactions.
//!
//! Dolt's MVCC engine surfaces MySQL error 1213 (40001) whenever two
//! transactions touch overlapping keys and one of them commits first —
//! the loser gets "serialization failure: this transaction conflicts with
//! a committed transaction from another client, try restarting transaction."
//! This is a normal, benign outcome under concurrent writes; the correct
//! response is to re-run the transaction.
//!
//! Call sites that `BEGIN ... COMMIT` on hot tables (notes, tasks, links,
//! agents) wrap the transaction body in [`retry_on_serialization_failure`]
//! so the retry is transparent. Every retry opens a fresh `sqlx` transaction
//! — we do not try to resume one that already failed.

use std::time::Duration;

use crate::error::DbError;

/// Default retry cap. Three attempts is enough in practice: Dolt commits
/// are serialized on the server, so by attempt 3 the contending writer
/// has almost always finished.
pub const DEFAULT_MAX_TX_RETRIES: usize = 3;

/// Returns `true` iff `err` is MySQL/Dolt error 1213 (serialization failure
/// / deadlock). Both SQLSTATE `40001` and the numeric code are checked
/// because `sqlx` and Dolt have historically shifted which field they
/// populate across versions.
pub fn is_serialization_failure(err: &DbError) -> bool {
    let DbError::Sqlx(sqlx::Error::Database(db_err)) = err else {
        return false;
    };
    let msg = db_err.message();
    db_err.code().as_deref() == Some("40001")
        || msg.contains("1213")
        || msg.contains("serialization failure")
        || msg.contains("Deadlock found")
}

/// Run `op` and retry up to `max_attempts` times when it returns a Dolt
/// serialization failure. Backoff is exponential starting at 20 ms so the
/// contending writer has a chance to finish before the next attempt.
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
