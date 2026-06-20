//! Shared helpers for verification repositories that are being phased out
//! (epic sehj). Migration 72 drops the verification tables, but the
//! repository modules and their consumers remain temporarily until the
//! remaining verification code is removed. These helpers let the repository
//! methods degrade gracefully — returning empty results instead of erroring
//! — when the backing table no longer exists.

/// Returns `true` when a `sqlx::Error` is a PostgreSQL `undefined_table`
/// error (SQLSTATE `42P01`), meaning the table has been dropped by migration
/// 72 and the repository should degrade to a no-op / empty result.
pub fn ok_if_table_dropped(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = err {
        // PostgreSQL SQLSTATE 42P01 = undefined_table
        return db_err.code().is_some_and(|c| c == "42P01");
    }
    false
}
