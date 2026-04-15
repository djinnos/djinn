//! User-auth session repository backing the web-client GitHub OAuth flow.
//!
//! This is distinct from [`crate::repositories::session`], which tracks
//! agent/task runs. Rows here represent a logged-in human user holding a
//! random 32-byte session token delivered to the browser in the
//! `djinn_session` cookie.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;

const COLS: &str = "token, user_id, github_login, github_name, github_avatar_url, \
                    github_access_token, created_at, expires_at";

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
}

/// Input required to persist a freshly authenticated user.
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
        self.db.ensure_initialized().await?;

        sqlx::query(
            "INSERT INTO user_auth_sessions
                (token, user_id, github_login, github_name, github_avatar_url,
                 github_access_token, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(params.token)
        .bind(params.user_id)
        .bind(params.github_login)
        .bind(params.github_name)
        .bind(params.github_avatar_url)
        .bind(params.github_access_token)
        .bind(params.expires_at)
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as::<_, UserAuthSessionRecord>(&format!(
            "SELECT {COLS} FROM user_auth_sessions WHERE token = ?"
        ))
        .bind(params.token)
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    pub async fn get_by_token(&self, token: &str) -> Result<Option<UserAuthSessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, UserAuthSessionRecord>(&format!(
            "SELECT {COLS} FROM user_auth_sessions WHERE token = ?"
        ))
        .bind(token)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn delete_by_token(&self, token: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query("DELETE FROM user_auth_sessions WHERE token = ?")
            .bind(token)
            .execute(self.db.pool())
            .await?;
        Ok(res.rows_affected())
    }

    /// Delete any session rows whose `expires_at` is <= `now` (RFC3339).
    pub async fn delete_expired(&self, now_rfc3339: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query("DELETE FROM user_auth_sessions WHERE expires_at <= ?")
            .bind(now_rfc3339)
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
}
