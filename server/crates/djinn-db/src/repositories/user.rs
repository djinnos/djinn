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
//! against the MySQL/Dolt schema (see `migrations_mysql/3_users_and_org_config.sql`).

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
            "INSERT INTO users
                (id, github_id, github_login, github_name, github_avatar_url,
                 is_member_of_org, last_seen_at)
             VALUES (?, ?, ?, ?, ?, TRUE,
                     DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'))
             ON DUPLICATE KEY UPDATE
                 github_login      = VALUES(github_login),
                 github_name       = VALUES(github_name),
                 github_avatar_url = VALUES(github_avatar_url),
                 last_seen_at      = VALUES(last_seen_at)",
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
                      last_seen_at, created_at
               FROM users WHERE github_id = ?"#,
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
                      last_seen_at, created_at
               FROM users WHERE id = ?"#,
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
                      last_seen_at, created_at
               FROM users WHERE github_id = ?"#,
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
            "UPDATE users SET is_member_of_org = ? WHERE id = ?",
            is_member,
            id,
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Bump `last_seen_at` to the current server time for a user known to
    /// still be an active org member. Complements [`Self::set_member_status`]
    /// during the periodic membership sync (Phase 3C).
    pub async fn touch_last_seen(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE users
               SET last_seen_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
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
}
