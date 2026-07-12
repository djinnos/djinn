//! Persistent user identity (Phase 1 of ADR "one deployment = one GitHub org").
//!
//! Rows here survive login/logout — unlike `user_auth_sessions`, which only
//! persists while a browser token is live. `github_id` (the immutable numeric
//! GitHub account id) is the natural unique key; `id` is a UUIDv7 surrogate
//! stable across login churn so attribution FKs never need rewriting when a
//! GitHub user renames their login.
//!
//! Phase 1 intentionally does NOT rewire auth; `user_auth_sessions.user_fk`
//! stays nullable until Phase 2 performs the backfill.
//!
//! All queries use compile-time-checked `sqlx::query!` / `sqlx::query_as!`
//! against the Postgres schema (see `migrations_postgres/3_users_and_org_config.sql`).

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;

/// Row materialised from the `users` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub github_id: i64,
    pub github_login: String,
    pub github_name: Option<String>,
    pub github_avatar_url: Option<String>,
    pub is_member_of_org: bool,
    /// Admin privilege bit. The first user to sign in (when no admin exists
    /// yet) is stamped admin by the auth callback; gates the global runtime
    /// settings. See migration 30.
    pub is_admin: bool,
    /// Proposal capability role: `proposer` (default) | `pm` | `engineer`.
    /// `is_admin` is an orthogonal superset.
    pub role: String,
    pub last_seen_at: Option<String>,
    pub created_at: String,
}

#[derive(Clone)]
pub struct UserRepository {
    db: Database,
}

impl UserRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Upsert a user by `github_id`. On first call, inserts a new row with a
    /// freshly-minted UUIDv7 `id`. On subsequent calls, updates the mutable
    /// GitHub attributes (login/name/avatar) and bumps `last_seen_at`, but
    /// keeps the stable surrogate `id` so all FK references remain intact.
    ///
    /// Rationale: GitHub logins are mutable (users can rename). Attribution
    /// chains must not break when that happens.
    pub async fn upsert_from_github(
        &self,
        github_id: i64,
        github_login: &str,
        github_name: Option<&str>,
        github_avatar_url: Option<&str>,
    ) -> Result<User> {
        self.db.ensure_initialized().await?;

        let new_id = uuid::Uuid::now_v7().to_string();

        // MySQL lacks a UUID default, so we generate the id client-side and
        // let ON DUPLICATE KEY UPDATE ignore it on repeat upserts. The
        // VALUES(...) clause feeds new login/name/avatar into the update;
        // last_seen_at is refreshed unconditionally.
        sqlx::query!(
            r#"INSERT INTO users
                (id, github_id, github_login, github_name, github_avatar_url,
                 is_member_of_org, last_seen_at)
             VALUES ($1, $2, $3, $4, $5, TRUE,
                     to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ON CONFLICT (github_id) DO UPDATE SET
                 github_login      = EXCLUDED.github_login,
                 github_name       = EXCLUDED.github_name,
                 github_avatar_url = EXCLUDED.github_avatar_url,
                 last_seen_at      = EXCLUDED.last_seen_at"#,
            new_id,
            github_id,
            github_login,
            github_name,
            github_avatar_url,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as!(
            User,
            r#"SELECT id, github_id, github_login, github_name, github_avatar_url,
                      is_member_of_org AS "is_member_of_org!: bool",
                      is_admin AS "is_admin!: bool", role,
                      last_seen_at, created_at
               FROM users WHERE github_id = $1"#,
            github_id,
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    pub async fn get_by_id(&self, id: &str) -> Result<Option<User>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            User,
            r#"SELECT id, github_id, github_login, github_name, github_avatar_url,
                      is_member_of_org AS "is_member_of_org!: bool",
                      is_admin AS "is_admin!: bool", role,
                      last_seen_at, created_at
               FROM users WHERE id = $1"#,
            id,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_github_id(&self, github_id: i64) -> Result<Option<User>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            User,
            r#"SELECT id, github_id, github_login, github_name, github_avatar_url,
                      is_member_of_org AS "is_member_of_org!: bool",
                      is_admin AS "is_admin!: bool", role,
                      last_seen_at, created_at
               FROM users WHERE github_id = $1"#,
            github_id,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Flip `is_member_of_org` without touching other attributes. Phase 2
    /// will call this from the GitHub org-membership check on login; for now
    /// it's exercised only by tests.
    pub async fn set_member_status(&self, id: &str, is_member: bool) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE users SET is_member_of_org = $1 WHERE id = $2",
            is_member,
            id,
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Flip `is_admin` without touching other attributes. Used by the auth
    /// callback to stamp the bootstrap admin, and available for manual/admin
    /// promotion paths.
    pub async fn set_admin_status(&self, id: &str, is_admin: bool) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!("UPDATE users SET is_admin = $1 WHERE id = $2", is_admin, id,)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Atomically grant the one-time bootstrap-admin role when no admin exists.
    ///
    /// The auth callback can run concurrently for different GitHub users. A
    /// separate `admin_count()` followed by `set_admin_status()` lets both
    /// callbacks observe zero and promote both users. The transaction-scoped
    /// advisory lock serializes only this rare bootstrap decision; normal
    /// user writes and later manual admin changes remain unaffected.
    pub async fn grant_bootstrap_admin_if_none(&self, id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        // Stable, process-independent lock key for "bootstrap the first admin".
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(0x444A_494E_4E41_444Di64)
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query(
            "UPDATE users SET is_admin = TRUE \
             WHERE id = $1 \
               AND NOT EXISTS (SELECT 1 FROM users WHERE is_admin = TRUE)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Set a user's proposal capability `role` (`proposer` | `pm` | `engineer`).
    pub async fn set_role(&self, id: &str, role: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!("UPDATE users SET role = $1 WHERE id = $2", role, id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Count users with `is_admin = TRUE`. The auth callback uses this to
    /// bootstrap the first signer-in as admin: when this returns 0, the
    /// just-upserted user is stamped admin.
    pub async fn admin_count(&self) -> Result<i64> {
        self.db.ensure_initialized().await?;
        let n: i64 =
            sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM users WHERE is_admin = TRUE"#,)
                .fetch_one(self.db.pool())
                .await?;
        Ok(n)
    }

    /// Bump `last_seen_at` to the current server time for a user known to
    /// still be an active org member. Complements [`Self::set_member_status`]
    /// during the periodic membership sync (Phase 3C).
    pub async fn touch_last_seen(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE users
               SET last_seen_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
            id,
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// List every user row. Used by the periodic org-membership sync
    /// (Phase 3C) to diff the local `users` table against the live GitHub
    /// org member list. The table is small (one row per human who has ever
    /// signed in), so we don't bother paginating here.
    pub async fn list_all(&self) -> Result<Vec<User>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            User,
            r#"SELECT id, github_id, github_login, github_name, github_avatar_url,
                      is_member_of_org AS "is_member_of_org!: bool",
                      is_admin AS "is_admin!: bool", role,
                      last_seen_at, created_at
               FROM users
               ORDER BY github_login"#,
        )
        .fetch_all(self.db.pool())
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_creates_then_updates_in_place() {
        let repo = UserRepository::new(test_db());

        let first = repo
            .upsert_from_github(12345, "octocat", Some("Octo Cat"), Some("https://a.png"))
            .await
            .unwrap();
        assert_eq!(first.github_id, 12345);
        assert_eq!(first.github_login, "octocat");
        assert_eq!(first.github_name.as_deref(), Some("Octo Cat"));
        assert!(first.is_member_of_org);
        assert!(first.last_seen_at.is_some());

        // Second upsert with same github_id but renamed login: same `id`,
        // updated login/name. This is the contract Phase 2 relies on.
        let second = repo
            .upsert_from_github(
                12345,
                "octocat-renamed",
                Some("Octo Renamed"),
                Some("https://b.png"),
            )
            .await
            .unwrap();
        assert_eq!(second.id, first.id, "surrogate id must be stable");
        assert_eq!(second.github_login, "octocat-renamed");
        assert_eq!(second.github_name.as_deref(), Some("Octo Renamed"));
        assert_eq!(second.github_avatar_url.as_deref(), Some("https://b.png"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_by_id_and_github_id_roundtrip() {
        let repo = UserRepository::new(test_db());

        assert!(repo.get_by_github_id(999).await.unwrap().is_none());

        let created = repo
            .upsert_from_github(999, "mona", None, None)
            .await
            .unwrap();

        let by_gh = repo.get_by_github_id(999).await.unwrap().unwrap();
        assert_eq!(by_gh.id, created.id);

        let by_id = repo.get_by_id(&created.id).await.unwrap().unwrap();
        assert_eq!(by_id.github_id, 999);

        assert!(repo.get_by_id("no-such-id").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_member_status_flips_flag() {
        let repo = UserRepository::new(test_db());

        let user = repo
            .upsert_from_github(42, "someone", None, None)
            .await
            .unwrap();
        assert!(user.is_member_of_org);

        repo.set_member_status(&user.id, false).await.unwrap();
        let after_revoke = repo.get_by_id(&user.id).await.unwrap().unwrap();
        assert!(!after_revoke.is_member_of_org);

        repo.set_member_status(&user.id, true).await.unwrap();
        let after_grant = repo.get_by_id(&user.id).await.unwrap().unwrap();
        assert!(after_grant.is_member_of_org);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admin_count_and_set_admin_status() {
        let repo = UserRepository::new(test_db());
        assert_eq!(repo.admin_count().await.unwrap(), 0);

        let u = repo
            .upsert_from_github(7, "first", None, None)
            .await
            .unwrap();
        assert!(!u.is_admin, "new users default to non-admin");
        assert_eq!(repo.admin_count().await.unwrap(), 0);

        repo.set_admin_status(&u.id, true).await.unwrap();
        assert_eq!(repo.admin_count().await.unwrap(), 1);
        assert!(repo.get_by_id(&u.id).await.unwrap().unwrap().is_admin);

        repo.set_admin_status(&u.id, false).await.unwrap();
        assert_eq!(repo.admin_count().await.unwrap(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_bootstrap_admin_has_exactly_one_winner() {
        let repo = UserRepository::new(test_db());
        let first = repo
            .upsert_from_github(101, "first", None, None)
            .await
            .unwrap();
        let second = repo
            .upsert_from_github(202, "second", None, None)
            .await
            .unwrap();

        let (first_won, second_won) = tokio::join!(
            repo.grant_bootstrap_admin_if_none(&first.id),
            repo.grant_bootstrap_admin_if_none(&second.id),
        );

        assert_ne!(first_won.unwrap(), second_won.unwrap());
        assert_eq!(repo.admin_count().await.unwrap(), 1);
    }
}
