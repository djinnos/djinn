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
    pub user_id: String,
    pub github_login: String,
    pub github_name: Option<String>,
    pub github_avatar_url: Option<String>,
    pub github_access_token: String,
    pub created_at: String,
    pub expires_at: String,
    /// Phase 1 nullable FK into `users.id`. `None` for legacy rows written
    /// before Phase 2 backfill; Phase 2 will populate this on every login.
    pub user_fk: Option<String>,
}

/// Input required to persist a freshly authenticated user.
///
/// Phase 1 keeps this struct shape-compatible with existing call sites in
/// `auth.rs`. Phase 2 code paths that have already upserted a `users` row
/// should prefer [`SessionAuthRepository::create_with_user_fk`] so the
/// session is linked to the stable identity surrogate.
pub struct CreateUserAuthSession<'a> {
    pub token: &'a str,
    pub user_id: &'a str,
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
        self.create_internal(params, None).await
    }

    /// Like [`Self::create`] but also records the stable `users.id` surrogate
    /// on the new session row via the `user_fk` column. Phase 2 rewires
    /// `auth.rs` to call this once it has upserted the `users` row.
    pub async fn create_with_user_fk(
        &self,
        params: CreateUserAuthSession<'_>,
        user_fk: &str,
    ) -> Result<UserAuthSessionRecord> {
        self.create_internal(params, Some(user_fk)).await
    }

    async fn create_internal(
        &self,
        params: CreateUserAuthSession<'_>,
        user_fk: Option<&str>,
    ) -> Result<UserAuthSessionRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query!(
            "INSERT INTO user_auth_sessions
                (token, user_id, github_login, github_name, github_avatar_url,
                 github_access_token, expires_at, user_fk)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params.token,
            params.user_id,
            params.github_login,
            params.github_name,
            params.github_avatar_url,
            params.github_access_token,
            params.expires_at,
            user_fk,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as!(
            UserAuthSessionRecord,
            "SELECT token, user_id, github_login, github_name, github_avatar_url, \
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
            "SELECT token, user_id, github_login, github_name, github_avatar_url, \
                    github_access_token, created_at, expires_at, user_fk \
             FROM user_auth_sessions WHERE token = ?",
            token,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a session plus its joined `User` row.
    ///
    /// Returns:
    ///   - `Ok(Some((session, user)))` for Phase 2 sessions that have
    ///     `user_fk` populated.
    ///   - `Ok(None)` when the token is unknown, expired-past-deletion, OR
    ///     when the session row's `user_fk` is NULL (legacy Phase 1 rows).
    ///     Phase 2 backfill eliminates that second case.
    ///
    /// Existing call sites should keep using [`Self::get_by_token`]; this
    /// new method is reserved for code that wants the stable identity
    /// record and must not fall back to the denormalised GitHub columns.
    pub async fn get_by_token_with_user(
        &self,
        token: &str,
    ) -> Result<Option<(UserAuthSessionRecord, User)>> {
        self.db.ensure_initialized().await?;

        let row = sqlx::query!(
            r#"SELECT
                 s.token               AS s_token,
                 s.user_id             AS s_user_id,
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
                user_id: r.s_user_id,
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
        let res = sqlx::query!(
            "DELETE FROM user_auth_sessions WHERE user_fk = ?",
            user_fk,
        )
        .execute(self.db.pool())
        .await?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crud_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = SessionAuthRepository::new(db);

        let created = repo
            .create(CreateUserAuthSession {
                token: "tok-abc",
                user_id: "12345",
                github_login: "octocat",
                github_name: Some("Octo Cat"),
                github_avatar_url: Some("https://example/a.png"),
                github_access_token: "gho_x",
                expires_at: "2099-01-01T00:00:00.000Z",
            })
            .await
            .unwrap();
        assert_eq!(created.github_login, "octocat");
        assert!(created.user_fk.is_none());

        let fetched = repo.get_by_token("tok-abc").await.unwrap().unwrap();
        assert_eq!(fetched.user_id, "12345");
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
        let repo = SessionAuthRepository::new(db);

        repo.create(CreateUserAuthSession {
            token: "past",
            user_id: "1",
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
            user_id: "2",
            github_login: "b",
            github_name: None,
            github_avatar_url: None,
            github_access_token: "t2",
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();

        let swept = repo.delete_expired("2025-01-01T00:00:00.000Z").await.unwrap();
        assert_eq!(swept, 1);
        assert!(repo.get_by_token("past").await.unwrap().is_none());
        assert!(repo.get_by_token("future").await.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_by_token_with_user_joins_when_user_fk_set() {
        use crate::repositories::user::UserRepository;

        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let users = UserRepository::new(db.clone());
        let sessions = SessionAuthRepository::new(db);

        // Session without user_fk: join-path must return None.
        sessions
            .create(CreateUserAuthSession {
                token: "legacy",
                user_id: "legacy-uid",
                github_login: "legacy-login",
                github_name: None,
                github_avatar_url: None,
                github_access_token: "gho_legacy",
                expires_at: "2099-01-01T00:00:00.000Z",
            })
            .await
            .unwrap();
        assert!(
            sessions.get_by_token_with_user("legacy").await.unwrap().is_none(),
            "sessions with NULL user_fk must not resolve via the join"
        );

        // Session with user_fk: join-path must return the joined User row.
        let user = users
            .upsert_from_github(777, "joined-user", Some("Joined"), None)
            .await
            .unwrap();
        sessions
            .create_with_user_fk(
                CreateUserAuthSession {
                    token: "linked",
                    user_id: "linked-uid",
                    github_login: "joined-user",
                    github_name: Some("Joined"),
                    github_avatar_url: None,
                    github_access_token: "gho_linked",
                    expires_at: "2099-01-01T00:00:00.000Z",
                },
                &user.id,
            )
            .await
            .unwrap();

        let (session, joined) = sessions
            .get_by_token_with_user("linked")
            .await
            .unwrap()
            .expect("linked session should resolve with its user row");
        assert_eq!(session.token, "linked");
        assert_eq!(session.user_fk.as_deref(), Some(user.id.as_str()));
        assert_eq!(joined.id, user.id);
        assert_eq!(joined.github_id, 777);
        assert_eq!(joined.github_login, "joined-user");
    }
}
