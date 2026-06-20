//! Per-commit verification output cache (`verification_cache` table).
//!
//! Verification is being removed (epic sehj). The `verification_cache` table
//! is dropped by migration 72 but this repository module remains temporarily
//! pending the full removal of all verification references. It uses the
//! non-macro `sqlx::query` form (like the other verification repos) so it
//! does not require the table to exist for offline `.sqlx` cache compilation.
//!
//! All methods gracefully degrade when the table no longer exists (post
//! migration 72): reads return empty/`None`, writes are silent no-ops. This
//! lets remaining consumers be removed incrementally without runtime panics.

use sqlx::Row;

use crate::Result;
use crate::database::Database;
use crate::repositories::verification_common::ok_if_table_dropped;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct CachedVerification {
    pub output: String,
    pub duration_ms: i64,
    pub created_at: String,
}

pub struct VerificationCacheRepository {
    db: Database,
}

impl VerificationCacheRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn get(
        &self,
        project_id: &str,
        commit_sha: &str,
    ) -> Result<Option<CachedVerification>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            r#"SELECT output, duration_ms AS "duration_ms!: i64", created_at
                 FROM verification_cache
                WHERE project_id = $1 AND commit_sha = $2"#,
        )
        .bind(project_id)
        .bind(commit_sha)
        .fetch_optional(self.db.pool())
        .await;
        match row {
            Ok(row) => Ok(row.map(|r| CachedVerification {
                output: r.get("output"),
                duration_ms: r.get("duration_ms"),
                created_at: r.get("created_at"),
            })),
            Err(e) => {
                if ok_if_table_dropped(&e) {
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    pub async fn insert(
        &self,
        project_id: &str,
        commit_sha: &str,
        output_json: &str,
        duration_ms: i64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let res: std::result::Result<sqlx::postgres::PgQueryResult, sqlx::Error> = sqlx::query(
            r#"INSERT INTO verification_cache (project_id, commit_sha, output, duration_ms)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (project_id, commit_sha) DO UPDATE
                   SET output=EXCLUDED.output, duration_ms=EXCLUDED.duration_ms"#,
        )
        .bind(project_id)
        .bind(commit_sha)
        .bind(output_json)
        .bind(duration_ms)
        .execute(self.db.pool())
        .await;
        ok_if_table_dropped_or_propagate(res)
    }

    pub async fn invalidate_project(&self, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query("DELETE FROM verification_cache WHERE project_id = $1")
            .bind(project_id)
            .execute(self.db.pool())
            .await;
        ok_if_table_dropped_or_propagate(res)
    }

    pub async fn prune_older_than(&self, days: i64) -> Result<()> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query(
            r#"DELETE FROM verification_cache
                WHERE created_at < to_char((now() at time zone 'utc') - (interval '1 day' * $1),
                                           'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
        )
        .bind(days as f64)
        .execute(self.db.pool())
        .await;
        ok_if_table_dropped_or_propagate(res)
    }
}

fn ok_if_table_dropped_or_propagate(
    res: std::result::Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
) -> Result<()> {
    match res {
        Ok(_) => Ok(()),
        Err(e) => {
            if ok_if_table_dropped(&e) {
                Ok(())
            } else {
                Err(e.into())
            }
        }
    }
}
