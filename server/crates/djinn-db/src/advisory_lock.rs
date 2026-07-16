//! PostgreSQL session advisory-lock operations.
//!
//! These helpers deliberately operate on caller-owned dedicated connections:
//! session-scoped locks cannot safely be acquired through the application pool.

use sqlx::postgres::PgConnection;

/// Try to acquire a session-scoped PostgreSQL advisory lock.
pub async fn try_acquire(
    conn: &mut PgConnection,
    class_id: i32,
    object_id: i32,
) -> std::result::Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1, $2)")
        .bind(class_id)
        .bind(object_id)
        .fetch_one(conn)
        .await
}

/// Release a session-scoped PostgreSQL advisory lock held by `conn`.
pub async fn release(
    conn: &mut PgConnection,
    class_id: i32,
    object_id: i32,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_unlock($1, $2)")
        .bind(class_id)
        .bind(object_id)
        .execute(conn)
        .await
        .map(|_| ())
}

/// Confirm that a dedicated PostgreSQL connection is still usable.
pub async fn ping(conn: &mut PgConnection) -> std::result::Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(conn).await.map(|_| ())
}
