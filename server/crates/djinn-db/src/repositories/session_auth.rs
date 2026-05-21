//! User-auth session repository backing the web-client GitHub OAuth flow.
//!
//! This is distinct from [`crate::repositories::session`], which tracks
//! agent/task runs. Rows here represent a logged-in human user holding a
//! random 32-byte session token delivered to the browser in the
//! `djinn_session` cookie.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;
use crate::repositories::user::User;

/// Row materialised from `user_auth_sessions` plus the GitHub access token.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct UserAuthSessionRecord {
    pub token: String,
    /// FK into `users.id` — the stable UUID identity surrogate.
    pub user_fk: String,
    pub github_login: String,
    pub github_name: Option<String>,
    pub github_avatar_url: Option<String>,
    pub github_access_token: String,
    pub created_at: String,
    pub expires_at: String,
}

/// Input required to persist a freshly authenticated user.
pub struct CreateUserAuthSession<'a> {
    pub token: &'a str,
    /// FK into `users.id`. The caller is responsible for having upserted
    /// the `users` row before invoking this.
    pub user_fk: &'a str,
    pub github_login: &'a str,
    pub github_name: Option<&'a str>,
    pub github_avatar_url: Option<&'a str>,
    pub github_access_token: &'a str,
    /// RFC3339 timestamp string (the caller computes TTL — typically +30d).
    pub expires_at: &'a str,
}

#[derive(Clone)]
pub struct SessionAuthRepository {
    db: Database,
}

impl SessionAuthRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn create(&self, params: CreateUserAuthSession<'_>) -> Result<UserAuthSessionRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query!(
            "INSERT INTO user_auth_sessions
                (token, github_login, github_name, github_avatar_url,
                 github_access_token, expires_at, user_fk)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params.token,
            params.github_login,
            params.github_name,
            params.github_avatar_url,
            params.github_access_token,
            params.expires_at,
            params.user_fk,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as!(
            UserAuthSessionRecord,
            "SELECT token, github_login, github_name, github_avatar_url, \
                    github_access_token, created_at, expires_at, user_fk \
             FROM user_auth_sessions WHERE token = ?",
            params.token,
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    pub async fn get_by_token(&self, token: &str) -> Result<Option<UserAuthSessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            UserAuthSessionRecord,
            "SELECT token, github_login, github_name, github_avatar_url, \
                    github_access_token, created_at, expires_at, user_fk \
             FROM user_auth_sessions WHERE token = ?",
            token,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Return the most recently issued, non-expired session for `user_fk`,
    /// or `None` when the user has no live session.
    ///
    /// Used by background paths (pr_poller's auto-approve branch) that act
    /// on behalf of a specific user but run outside any HTTP session scope.
    /// `expires_at` is the ISO-8601 string the auth flow wrote — comparing
    /// against `DATE_FORMAT(NOW(3), …)` keeps the comparison in the same
    /// lexicographic ISO format the column was sorted under.
    pub async fn latest_token_for_user(
        &self,
        user_fk: &str,
    ) -> Result<Option<UserAuthSessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            UserAuthSessionRecord,
            "SELECT token, github_login, github_name, github_avatar_url, \
                    github_access_token, created_at, expires_at, user_fk \
             FROM user_auth_sessions \
             WHERE user_fk = ? \
               AND expires_at > DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ') \
             ORDER BY created_at DESC \
             LIMIT 1",
            user_fk,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a session plus its joined `User` row.
    ///
    /// Returns `Ok(None)` when the token is unknown or expired past
    /// deletion. After the migration 22 cut-over, every live session row
    /// has a non-null `user_fk`, so the INNER JOIN never silently filters
    /// otherwise-valid rows.
    pub async fn get_by_token_with_user(
        &self,
        token: &str,
    ) -> Result<Option<(UserAuthSessionRecord, User)>> {
        self.db.ensure_initialized().await?;

        let row = sqlx::query!(
            r#"SELECT
                 s.token               AS s_token,
                 s.github_login        AS s_github_login,
                 s.github_name         AS s_github_name,
                 s.github_avatar_url   AS s_github_avatar_url,
                 s.github_access_token AS s_github_access_token,
                 s.created_at          AS s_created_at,
                 s.expires_at          AS s_expires_at,
                 s.user_fk             AS s_user_fk,
                 u.id                  AS u_id,
                 u.github_id           AS u_github_id,
                 u.github_login        AS u_github_login,
                 u.github_name         AS u_github_name,
                 u.github_avatar_url   AS u_github_avatar_url,
                 u.is_member_of_org    AS `u_is_member_of_org!: bool`,
                 u.last_seen_at        AS u_last_seen_at,
                 u.created_at          AS u_created_at
               FROM user_auth_sessions s
               INNER JOIN users u ON u.id = s.user_fk
               WHERE s.token = ?"#,
            token,
        )
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row.map(|r| {
            let session = UserAuthSessionRecord {
                token: r.s_token,
                github_login: r.s_github_login,
                github_name: r.s_github_name,
                github_avatar_url: r.s_github_avatar_url,
                github_access_token: r.s_github_access_token,
                created_at: r.s_created_at,
                expires_at: r.s_expires_at,
                user_fk: r.s_user_fk,
            };
            let user = User {
                id: r.u_id,
                github_id: r.u_github_id,
                github_login: r.u_github_login,
                github_name: r.u_github_name,
                github_avatar_url: r.u_github_avatar_url,
                is_member_of_org: r.u_is_member_of_org,
                last_seen_at: r.u_last_seen_at,
                created_at: r.u_created_at,
            };
            (session, user)
        }))
    }

    pub async fn delete_by_token(&self, token: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query!("DELETE FROM user_auth_sessions WHERE token = ?", token)
            .execute(self.db.pool())
            .await?;
        Ok(res.rows_affected())
    }

    /// Delete any session rows whose `expires_at` is <= `now` (RFC3339).
    pub async fn delete_expired(&self, now_rfc3339: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query!(
            "DELETE FROM user_auth_sessions WHERE expires_at <= ?",
            now_rfc3339,
        )
        .execute(self.db.pool())
        .await?;
        Ok(res.rows_affected())
    }

    /// Delete every session row linked to `user_fk`. Used by the periodic
    /// org-membership sync (Phase 3C) to revoke browser sessions the moment
    /// a user loses their org membership — their next request will miss the
    /// cookie lookup and bounce back through the OAuth flow, where the
    /// membership re-check in `auth.rs` will reject them.
    ///
    /// Returns the number of rows deleted (0 if the user had no live
    /// sessions).
    pub async fn delete_by_user_fk(&self, user_fk: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query!("DELETE FROM user_auth_sessions WHERE user_fk = ?", user_fk,)
            .execute(self.db.pool())
            .await?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::user::UserRepository;

    async fn seed_user(db: &Database, github_id: i64, login: &str) -> String {
        UserRepository::new(db.clone())
            .upsert_from_github(github_id, login, None, None)
            .await
            .unwrap()
            .id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crud_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user_fk = seed_user(&db, 1234, "octocat").await;
        let repo = SessionAuthRepository::new(db);

        let created = repo
            .create(CreateUserAuthSession {
                token: "tok-abc",
                user_fk: &user_fk,
                github_login: "octocat",
                github_name: Some("Octo Cat"),
                github_avatar_url: Some("https://example/a.png"),
                github_access_token: "gho_x",
                expires_at: "2099-01-01T00:00:00.000Z",
            })
            .await
            .unwrap();
        assert_eq!(created.github_login, "octocat");
        assert_eq!(created.user_fk, user_fk);

        let fetched = repo.get_by_token("tok-abc").await.unwrap().unwrap();
        assert_eq!(fetched.user_fk, user_fk);
        assert_eq!(fetched.github_name.as_deref(), Some("Octo Cat"));

        let missing = repo.get_by_token("nope").await.unwrap();
        assert!(missing.is_none());

        let removed = repo.delete_by_token("tok-abc").await.unwrap();
        assert_eq!(removed, 1);
        assert!(repo.get_by_token("tok-abc").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_expired_sweeps_only_past_rows() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user_a = seed_user(&db, 11, "a").await;
        let user_b = seed_user(&db, 22, "b").await;
        let repo = SessionAuthRepository::new(db);

        repo.create(CreateUserAuthSession {
            token: "past",
            user_fk: &user_a,
            github_login: "a",
            github_name: None,
            github_avatar_url: None,
            github_access_token: "t1",
            expires_at: "2000-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();
        repo.create(CreateUserAuthSession {
            token: "future",
            user_fk: &user_b,
            github_login: "b",
            github_name: None,
            github_avatar_url: None,
            github_access_token: "t2",
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();

        let swept = repo
            .delete_expired("2025-01-01T00:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(swept, 1);
        assert!(repo.get_by_token("past").await.unwrap().is_none());
        assert!(repo.get_by_token("future").await.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_by_token_with_user_returns_the_joined_user_row() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user_fk = seed_user(&db, 777, "joined-user").await;
        let sessions = SessionAuthRepository::new(db);

        sessions
            .create(CreateUserAuthSession {
                token: "linked",
                user_fk: &user_fk,
                github_login: "joined-user",
                github_name: Some("Joined"),
                github_avatar_url: None,
                github_access_token: "gho_linked",
                expires_at: "2099-01-01T00:00:00.000Z",
            })
            .await
            .unwrap();

        let (session, joined) = sessions
            .get_by_token_with_user("linked")
            .await
            .unwrap()
            .expect("linked session should resolve with its user row");
        assert_eq!(session.token, "linked");
        assert_eq!(session.user_fk, user_fk);
        assert_eq!(joined.id, user_fk);
        assert_eq!(joined.github_id, 777);
        assert_eq!(joined.github_login, "joined-user");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_token_for_user_skips_expired_and_returns_most_recent() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user_fk = seed_user(&db, 101, "live-user").await;
        let sessions = SessionAuthRepository::new(db.clone());

        // No sessions yet → None.
        assert!(
            sessions
                .latest_token_for_user(&user_fk)
                .await
                .unwrap()
                .is_none()
        );

        // Expired session — must NOT be returned.
        sessions
            .create(CreateUserAuthSession {
                token: "old",
                user_fk: &user_fk,
                github_login: "live-user",
                github_name: None,
                github_avatar_url: None,
                github_access_token: "gho_old",
                expires_at: "2000-01-01T00:00:00.000Z",
            })
            .await
            .unwrap();
        assert!(
            sessions
                .latest_token_for_user(&user_fk)
                .await
                .unwrap()
                .is_none(),
            "expired sessions must be filtered out"
        );

        // Add a future-dated session — returned.
        sessions
            .create(CreateUserAuthSession {
                token: "fresh",
                user_fk: &user_fk,
                github_login: "live-user",
                github_name: None,
                github_avatar_url: None,
                github_access_token: "gho_fresh",
                expires_at: "2099-01-01T00:00:00.000Z",
            })
            .await
            .unwrap();

        let got = sessions
            .latest_token_for_user(&user_fk)
            .await
            .unwrap()
            .expect("fresh session should resolve");
        assert_eq!(got.token, "fresh");
        assert_eq!(got.github_access_token, "gho_fresh");

        // Different user — must not see this user's token.
        let other = seed_user(&db, 202, "other-user").await;
        assert!(
            sessions
                .latest_token_for_user(&other)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deleting_user_cascades_session() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user_fk = seed_user(&db, 909, "casc").await;
        let sessions = SessionAuthRepository::new(db.clone());

        sessions
            .create(CreateUserAuthSession {
                token: "casc-tok",
                user_fk: &user_fk,
                github_login: "casc",
                github_name: None,
                github_avatar_url: None,
                github_access_token: "gho_casc",
                expires_at: "2099-01-01T00:00:00.000Z",
            })
            .await
            .unwrap();
        assert!(sessions.get_by_token("casc-tok").await.unwrap().is_some());

        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(&user_fk)
            .execute(db.pool())
            .await
            .unwrap();

        assert!(
            sessions.get_by_token("casc-tok").await.unwrap().is_none(),
            "FK ON DELETE CASCADE should have wiped the session row"
        );
    }
}
