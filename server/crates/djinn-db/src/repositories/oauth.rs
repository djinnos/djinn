//! MCP OAuth 2.1 repository — clients, authorization codes, access tokens.
//!
//! djinn acts as both an OAuth Resource Server and its own Authorization
//! Server (RFC 6749 / 8414 / 7591 / 9728), federating the existing GitHub
//! login. All tokens here are **opaque DB-backed values** (not JWTs), so
//! validation is a row lookup checking expiry + audience — multi-replica safe
//! and trivially revocable, mirroring `user_auth_sessions`.
//!
//! These queries deliberately use the **runtime** `sqlx::query` API (not the
//! compile-time `query!` macros). The OAuth tables are young and their schema
//! still evolves; runtime queries keep them buildable without regenerating the
//! committed `.sqlx` offline cache on every migration. Postgres dialect
//! throughout: `$N` binds, no backticks, JSONB for the array columns.

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::Result;
use crate::database::Database;

// ── Row types ────────────────────────────────────────────────────────────────

/// A registered OAuth client (RFC 7591 dynamic client registration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClient {
    pub client_id: String,
    /// `None` for public PKCE clients (`token_endpoint_auth_method = "none"`).
    pub client_secret: Option<String>,
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
    pub token_endpoint_auth_method: String,
    pub grant_types: Vec<String>,
    pub created_at: String,
}

/// Parameters to register a new client.
pub struct NewOAuthClient<'a> {
    pub client_id: &'a str,
    pub client_secret: Option<&'a str>,
    pub redirect_uris: &'a [String],
    pub client_name: Option<&'a str>,
    pub token_endpoint_auth_method: &'a str,
    pub grant_types: &'a [String],
}

/// A minted single-use authorization code.
#[derive(Debug, Clone)]
pub struct AuthorizationCode {
    pub code: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub resource: String,
    pub scope: Option<String>,
    pub user_id: String,
    pub expires_at: String,
    pub consumed_at: Option<String>,
}

/// Parameters to mint an authorization code.
pub struct NewAuthorizationCode<'a> {
    pub code: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    pub code_challenge_method: &'a str,
    pub resource: &'a str,
    pub scope: Option<&'a str>,
    pub user_id: &'a str,
    pub expires_at: &'a str,
}

/// An issued bearer access token (with optional refresh credential).
#[derive(Debug, Clone)]
pub struct McpAccessToken {
    pub token: String,
    pub refresh_token: Option<String>,
    pub client_id: String,
    pub user_id: String,
    pub resource: String,
    pub scope: Option<String>,
    pub expires_at: String,
    pub refresh_token_expires_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// Parameters to issue an access token.
pub struct NewAccessToken<'a> {
    pub token: &'a str,
    pub refresh_token: Option<&'a str>,
    pub client_id: &'a str,
    pub user_id: &'a str,
    pub resource: &'a str,
    pub scope: Option<&'a str>,
    pub expires_at: &'a str,
    pub refresh_token_expires_at: Option<&'a str>,
}

#[derive(Clone)]
pub struct OAuthRepository {
    db: Database,
}

impl OAuthRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    // ── Clients ──────────────────────────────────────────────────────────────

    pub async fn create_client(&self, params: NewOAuthClient<'_>) -> Result<OAuthClient> {
        self.db.ensure_initialized().await?;
        let redirect_uris = serde_json::to_value(params.redirect_uris)
            .map_err(|e| crate::Error::Internal(e.to_string()))?;
        let grant_types = serde_json::to_value(params.grant_types)
            .map_err(|e| crate::Error::Internal(e.to_string()))?;

        sqlx::query(
            "INSERT INTO oauth_clients
                (client_id, client_secret, redirect_uris, client_name,
                 token_endpoint_auth_method, grant_types)
             VALUES ($1, $2, $3::jsonb, $4, $5, $6::jsonb)",
        )
        .bind(params.client_id)
        .bind(params.client_secret)
        .bind(&redirect_uris)
        .bind(params.client_name)
        .bind(params.token_endpoint_auth_method)
        .bind(&grant_types)
        .execute(self.db.pool())
        .await?;

        self.get_client(params.client_id)
            .await?
            .ok_or_else(|| crate::Error::Internal("client row missing after insert".to_owned()))
    }

    pub async fn get_client(&self, client_id: &str) -> Result<Option<OAuthClient>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            "SELECT client_id, client_secret, redirect_uris::text AS redirect_uris,
                    client_name, token_endpoint_auth_method,
                    grant_types::text AS grant_types, created_at
             FROM oauth_clients WHERE client_id = $1",
        )
        .bind(client_id)
        .fetch_optional(self.db.pool())
        .await?;

        row.map(client_from_row).transpose()
    }

    // ── Authorization codes ────────────────────────────────────────────────────

    pub async fn create_authorization_code(&self, params: NewAuthorizationCode<'_>) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO oauth_authorization_codes
                (code, client_id, redirect_uri, code_challenge, code_challenge_method,
                 resource, scope, user_id, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(params.code)
        .bind(params.client_id)
        .bind(params.redirect_uri)
        .bind(params.code_challenge)
        .bind(params.code_challenge_method)
        .bind(params.resource)
        .bind(params.scope)
        .bind(params.user_id)
        .bind(params.expires_at)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn get_authorization_code(&self, code: &str) -> Result<Option<AuthorizationCode>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            "SELECT code, client_id, redirect_uri, code_challenge, code_challenge_method,
                    resource, scope, user_id, expires_at, consumed_at
             FROM oauth_authorization_codes WHERE code = $1",
        )
        .bind(code)
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row.map(|r| AuthorizationCode {
            code: r.get("code"),
            client_id: r.get("client_id"),
            redirect_uri: r.get("redirect_uri"),
            code_challenge: r.get("code_challenge"),
            code_challenge_method: r.get("code_challenge_method"),
            resource: r.get("resource"),
            scope: r.get("scope"),
            user_id: r.get("user_id"),
            expires_at: r.get("expires_at"),
            consumed_at: r.get("consumed_at"),
        }))
    }

    /// Atomically mark a code consumed. Returns `true` iff this call is the one
    /// that flipped it from unconsumed → consumed (i.e. the row existed and
    /// `consumed_at` was still NULL). A `false` return means the code was
    /// already spent (replay) or unknown — the caller MUST reject in that case.
    pub async fn consume_authorization_code(&self, code: &str, now_rfc3339: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query(
            "UPDATE oauth_authorization_codes
                SET consumed_at = $1
              WHERE code = $2 AND consumed_at IS NULL",
        )
        .bind(now_rfc3339)
        .bind(code)
        .execute(self.db.pool())
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Delete expired/consumed codes whose `expires_at` is at or before `now`.
    pub async fn delete_expired_codes(&self, now_rfc3339: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query("DELETE FROM oauth_authorization_codes WHERE expires_at <= $1")
            .bind(now_rfc3339)
            .execute(self.db.pool())
            .await?;
        Ok(res.rows_affected())
    }

    // ── Access tokens ──────────────────────────────────────────────────────────

    pub async fn create_access_token(&self, params: NewAccessToken<'_>) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO mcp_access_tokens
                (token, refresh_token, client_id, user_id, resource, scope,
                 expires_at, refresh_token_expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(params.token)
        .bind(params.refresh_token)
        .bind(params.client_id)
        .bind(params.user_id)
        .bind(params.resource)
        .bind(params.scope)
        .bind(params.expires_at)
        .bind(params.refresh_token_expires_at)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn get_access_token(&self, token: &str) -> Result<Option<McpAccessToken>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            "SELECT token, refresh_token, client_id, user_id, resource, scope,
                    expires_at, refresh_token_expires_at, revoked_at
             FROM mcp_access_tokens WHERE token = $1",
        )
        .bind(token)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(token_from_row))
    }

    pub async fn get_by_refresh_token(
        &self,
        refresh_token: &str,
    ) -> Result<Option<McpAccessToken>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            "SELECT token, refresh_token, client_id, user_id, resource, scope,
                    expires_at, refresh_token_expires_at, revoked_at
             FROM mcp_access_tokens WHERE refresh_token = $1",
        )
        .bind(refresh_token)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(token_from_row))
    }

    /// Mark a token revoked. Used on refresh-token rotation and explicit
    /// revocation. Idempotent: re-revoking an already-revoked row is a no-op.
    pub async fn revoke_access_token(&self, token: &str, now_rfc3339: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query(
            "UPDATE mcp_access_tokens SET revoked_at = $1
              WHERE token = $2 AND revoked_at IS NULL",
        )
        .bind(now_rfc3339)
        .bind(token)
        .execute(self.db.pool())
        .await?;
        Ok(res.rows_affected())
    }

    /// Delete tokens whose access AND refresh deadlines have both passed.
    /// (Keep rows whose refresh is still live so rotation can find them.)
    pub async fn delete_expired_tokens(&self, now_rfc3339: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query(
            "DELETE FROM mcp_access_tokens
              WHERE expires_at <= $1
                AND (refresh_token_expires_at IS NULL OR refresh_token_expires_at <= $1)",
        )
        .bind(now_rfc3339)
        .execute(self.db.pool())
        .await?;
        Ok(res.rows_affected())
    }
}

fn client_from_row(row: sqlx::postgres::PgRow) -> Result<OAuthClient> {
    let redirect_uris_json: String = row.get("redirect_uris");
    let grant_types_json: String = row.get("grant_types");
    let redirect_uris: Vec<String> = serde_json::from_str(&redirect_uris_json)
        .map_err(|e| crate::Error::InvalidData(format!("bad redirect_uris json: {e}")))?;
    let grant_types: Vec<String> = serde_json::from_str(&grant_types_json)
        .map_err(|e| crate::Error::InvalidData(format!("bad grant_types json: {e}")))?;
    Ok(OAuthClient {
        client_id: row.get("client_id"),
        client_secret: row.get("client_secret"),
        redirect_uris,
        client_name: row.get("client_name"),
        token_endpoint_auth_method: row.get("token_endpoint_auth_method"),
        grant_types,
        created_at: row.get("created_at"),
    })
}

fn token_from_row(row: sqlx::postgres::PgRow) -> McpAccessToken {
    McpAccessToken {
        token: row.get("token"),
        refresh_token: row.get("refresh_token"),
        client_id: row.get("client_id"),
        user_id: row.get("user_id"),
        resource: row.get("resource"),
        scope: row.get("scope"),
        expires_at: row.get("expires_at"),
        refresh_token_expires_at: row.get("refresh_token_expires_at"),
        revoked_at: row.get("revoked_at"),
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
    async fn client_roundtrip_preserves_redirect_uris_and_grants() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = OAuthRepository::new(db);

        let redirects = vec![
            "http://127.0.0.1:9999/callback".to_string(),
            "cursor://anysphere.cursor-retrieval/callback".to_string(),
        ];
        let grants = vec![
            "authorization_code".to_string(),
            "refresh_token".to_string(),
        ];

        let created = repo
            .create_client(NewOAuthClient {
                client_id: "client-abc",
                client_secret: None,
                redirect_uris: &redirects,
                client_name: Some("Claude Code"),
                token_endpoint_auth_method: "none",
                grant_types: &grants,
            })
            .await
            .unwrap();
        assert_eq!(created.client_id, "client-abc");
        assert!(created.client_secret.is_none());
        assert_eq!(created.redirect_uris, redirects);
        assert_eq!(created.grant_types, grants);

        let fetched = repo.get_client("client-abc").await.unwrap().unwrap();
        assert_eq!(fetched.redirect_uris, redirects);
        assert_eq!(fetched.client_name.as_deref(), Some("Claude Code"));

        assert!(repo.get_client("nope").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn confidential_client_stores_secret() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = OAuthRepository::new(db);
        let redirects = vec!["https://app.example.com/cb".to_string()];
        let grants = vec!["authorization_code".to_string()];
        let created = repo
            .create_client(NewOAuthClient {
                client_id: "conf-1",
                client_secret: Some("s3cr3t"),
                redirect_uris: &redirects,
                client_name: None,
                token_endpoint_auth_method: "client_secret_post",
                grant_types: &grants,
            })
            .await
            .unwrap();
        assert_eq!(created.client_secret.as_deref(), Some("s3cr3t"));
        assert_eq!(created.token_endpoint_auth_method, "client_secret_post");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authorization_code_single_use_via_consume() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user_id = seed_user(&db, 1, "octocat").await;
        let repo = OAuthRepository::new(db);

        let redirects = vec!["http://127.0.0.1:9999/cb".to_string()];
        let grants = vec!["authorization_code".to_string()];
        repo.create_client(NewOAuthClient {
            client_id: "c1",
            client_secret: None,
            redirect_uris: &redirects,
            client_name: None,
            token_endpoint_auth_method: "none",
            grant_types: &grants,
        })
        .await
        .unwrap();

        repo.create_authorization_code(NewAuthorizationCode {
            code: "code-xyz",
            client_id: "c1",
            redirect_uri: "http://127.0.0.1:9999/cb",
            code_challenge: "challenge",
            code_challenge_method: "S256",
            resource: "http://127.0.0.1:8372/mcp",
            scope: Some("mcp"),
            user_id: &user_id,
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();

        let fetched = repo
            .get_authorization_code("code-xyz")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.user_id, user_id);
        assert!(fetched.consumed_at.is_none());

        // First consume wins.
        assert!(
            repo.consume_authorization_code("code-xyz", "2025-01-01T00:00:00.000Z")
                .await
                .unwrap()
        );
        // Second consume (replay) loses.
        assert!(
            !repo
                .consume_authorization_code("code-xyz", "2025-01-01T00:00:01.000Z")
                .await
                .unwrap()
        );
        // Unknown code loses.
        assert!(
            !repo
                .consume_authorization_code("no-such-code", "2025-01-01T00:00:01.000Z")
                .await
                .unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn access_token_and_refresh_roundtrip_and_revoke() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user_id = seed_user(&db, 2, "mona").await;
        let repo = OAuthRepository::new(db);

        let redirects = vec!["http://127.0.0.1:9999/cb".to_string()];
        let grants = vec![
            "authorization_code".to_string(),
            "refresh_token".to_string(),
        ];
        repo.create_client(NewOAuthClient {
            client_id: "c2",
            client_secret: None,
            redirect_uris: &redirects,
            client_name: None,
            token_endpoint_auth_method: "none",
            grant_types: &grants,
        })
        .await
        .unwrap();

        repo.create_access_token(NewAccessToken {
            token: "at-1",
            refresh_token: Some("rt-1"),
            client_id: "c2",
            user_id: &user_id,
            resource: "http://127.0.0.1:8372/mcp",
            scope: Some("mcp"),
            expires_at: "2099-01-01T00:00:00.000Z",
            refresh_token_expires_at: Some("2099-02-01T00:00:00.000Z"),
        })
        .await
        .unwrap();

        let by_at = repo.get_access_token("at-1").await.unwrap().unwrap();
        assert_eq!(by_at.user_id, user_id);
        assert_eq!(by_at.resource, "http://127.0.0.1:8372/mcp");
        assert!(by_at.revoked_at.is_none());

        let by_rt = repo.get_by_refresh_token("rt-1").await.unwrap().unwrap();
        assert_eq!(by_rt.token, "at-1");

        let revoked = repo
            .revoke_access_token("at-1", "2025-01-01T00:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(revoked, 1);
        let after = repo.get_access_token("at-1").await.unwrap().unwrap();
        assert!(after.revoked_at.is_some());

        // Re-revoke is a no-op.
        let re = repo
            .revoke_access_token("at-1", "2025-02-01T00:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(re, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_expired_codes_and_tokens_sweep_only_past_rows() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user_id = seed_user(&db, 3, "sweeper").await;
        let repo = OAuthRepository::new(db);

        let redirects = vec!["http://127.0.0.1:9999/cb".to_string()];
        let grants = vec!["authorization_code".to_string()];
        repo.create_client(NewOAuthClient {
            client_id: "c3",
            client_secret: None,
            redirect_uris: &redirects,
            client_name: None,
            token_endpoint_auth_method: "none",
            grant_types: &grants,
        })
        .await
        .unwrap();

        repo.create_authorization_code(NewAuthorizationCode {
            code: "old-code",
            client_id: "c3",
            redirect_uri: "http://127.0.0.1:9999/cb",
            code_challenge: "ch",
            code_challenge_method: "S256",
            resource: "http://127.0.0.1:8372/mcp",
            scope: None,
            user_id: &user_id,
            expires_at: "2000-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();
        let swept = repo
            .delete_expired_codes("2025-01-01T00:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(swept, 1);
        assert!(
            repo.get_authorization_code("old-code")
                .await
                .unwrap()
                .is_none()
        );

        repo.create_access_token(NewAccessToken {
            token: "old-at",
            refresh_token: None,
            client_id: "c3",
            user_id: &user_id,
            resource: "http://127.0.0.1:8372/mcp",
            scope: None,
            expires_at: "2000-01-01T00:00:00.000Z",
            refresh_token_expires_at: None,
        })
        .await
        .unwrap();
        let swept_tokens = repo
            .delete_expired_tokens("2025-01-01T00:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(swept_tokens, 1);
        assert!(repo.get_access_token("old-at").await.unwrap().is_none());
    }
}
