//! GitHub App OAuth — Authorization Code + PKCE flow.
//!
//! Handles the full PKCE browser-redirect flow, token caching, and silent
//! refresh for GitHub App user authentication.
//!
//! Tokens are stored encrypted in the credentials DB table under
//! `__OAUTH_GITHUB_APP`. The installation ID is stored separately under
//! `__GITHUB_INSTALLATION_ID`.

use anyhow::{Result, anyhow};
use base64::Engine as _;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Mutex as TokioMutex, oneshot};

use crate::repos::CredentialRepository;

// ─── Constants ────────────────────────────────────────────────────────────────

/// GitHub OAuth authorization endpoint.
const GITHUB_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";

/// GitHub OAuth token exchange endpoint.
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// GitHub App client ID. Replace with the actual GitHub App client ID.
/// This is read from the credential vault at runtime when available.
pub const GITHUB_APP_CLIENT_ID_KEY: &str = "__GITHUB_APP_CLIENT_ID";

/// Default GitHub App client ID fallback (override via credential vault).
const DEFAULT_CLIENT_ID: &str = "Iv23liGitHubAppDjinn";

/// OAuth callback port — shared with Codex flow.
const OAUTH_PORT: u16 = 1455;

/// Browser redirect timeout in seconds.
const OAUTH_TIMEOUT_SECS: u64 = 300;

/// OAuth scopes requested for GitHub App user auth.
const OAUTH_SCOPES: &str = "read:user";

/// Credential key for storing GitHub App OAuth tokens.
pub const GITHUB_APP_OAUTH_DB_KEY: &str = "__OAUTH_GITHUB_APP";

/// Credential key for storing the installation ID.
pub const GITHUB_INSTALLATION_ID_KEY: &str = "__GITHUB_INSTALLATION_ID";

// ─── Token types ─────────────────────────────────────────────────────────────

/// Cached GitHub App OAuth token bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubAppTokens {
    /// User access token for authenticating as the user.
    pub access_token: String,
    /// Refresh token for renewing the access token.
    pub refresh_token: String,
    /// UTC timestamp when the access token expires.
    pub expires_at: i64,
    /// UTC timestamp when the refresh token expires (optional).
    pub refresh_token_expires_at: Option<i64>,
    /// GitHub user login extracted after auth (best-effort).
    pub user_login: Option<String>,
}

impl GitHubAppTokens {
    /// Returns true if the access token has expired (with a 60-second buffer).
    pub fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        now >= self.expires_at - 60
    }

    /// Load tokens from the encrypted credential DB.
    pub async fn load_from_db(repo: &CredentialRepository) -> Option<Self> {
        if let Ok(Some(json)) = repo.get_decrypted(GITHUB_APP_OAUTH_DB_KEY).await {
            if let Ok(tokens) = serde_json::from_str::<Self>(&json) {
                return Some(tokens);
            }
            tracing::warn!("GitHubApp: corrupt token JSON in DB, ignoring");
        }
        None
    }

    /// Persist tokens to the encrypted credential DB.
    pub async fn save_to_db(&self, repo: &CredentialRepository) -> Result<()> {
        let json = serde_json::to_string(self)?;
        repo.set("github_app", GITHUB_APP_OAUTH_DB_KEY, &json)
            .await
            .map_err(|e| anyhow!("failed to save GitHub App tokens to DB: {e}"))?;
        Ok(())
    }

    /// Remove tokens from the DB.
    pub async fn clear_from_db(repo: &CredentialRepository) {
        let _ = repo.delete(GITHUB_APP_OAUTH_DB_KEY).await;
    }
}

/// Attempt a silent token refresh using the stored refresh_token.
/// Returns refreshed `GitHubAppTokens` on success, saving them to the DB.
pub async fn refresh_cached_token(
    cached: &GitHubAppTokens,
    client_id: &str,
    repo: &CredentialRepository,
) -> Result<GitHubAppTokens> {
    let tr = refresh_token(client_id, &cached.refresh_token).await?;
    let tokens = token_response_to_tokens(tr, cached.user_login.clone());
    tokens.save_to_db(repo).await?;
    tracing::info!("GitHubApp: token refreshed successfully (lifecycle)");
    Ok(tokens)
}

// ─── PKCE helpers ─────────────────────────────────────────────────────────────

struct PkceChallenge {
    verifier: String,
    challenge: String,
}

fn generate_pkce() -> PkceChallenge {
    use ring::rand::{SecureRandom, SystemRandom};
    use sha2::Digest as _;
    let rng = SystemRandom::new();
    let mut bytes = [0u8; 43];
    rng.fill(&mut bytes).expect("rng fill");
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    PkceChallenge {
        verifier,
        challenge,
    }
}

fn generate_state() -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let rng = SystemRandom::new();
    let mut bytes = [0u8; 24];
    rng.fill(&mut bytes).expect("rng fill");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ─── Auth URL ─────────────────────────────────────────────────────────────────

fn build_authorize_url(
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkceChallenge,
    state: &str,
) -> Result<String> {
    let params = [
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("scope", OAUTH_SCOPES),
        ("state", state),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
    ];
    let query = serde_urlencoded::to_string(params)?;
    Ok(format!("{}?{}", GITHUB_AUTHORIZE_URL, query))
}

// ─── Token exchange / refresh ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: Option<i64>,
    refresh_token_expires_in: Option<i64>,
}

async fn exchange_code(
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    pkce: &PkceChallenge,
) -> Result<TokenResponse> {
    let client = Client::new();
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", &pkce.verifier),
    ];
    let resp = client
        .post(GITHUB_TOKEN_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&params)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub App token exchange failed ({}): {}", status, text));
    }
    Ok(resp.json().await?)
}

async fn refresh_token(client_id: &str, refresh_token_value: &str) -> Result<TokenResponse> {
    let client = Client::new();
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token_value),
        ("client_id", client_id),
    ];
    let resp = client
        .post(GITHUB_TOKEN_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&params)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub App token refresh failed ({}): {}", status, text));
    }
    Ok(resp.json().await?)
}

fn token_response_to_tokens(tr: TokenResponse, user_login: Option<String>) -> GitHubAppTokens {
    let now = now_secs();
    let expires_at = now + tr.expires_in.unwrap_or(28_800); // default 8 hours
    let refresh_token_expires_at = tr.refresh_token_expires_in.map(|secs| now + secs);
    GitHubAppTokens {
        access_token: tr.access_token,
        refresh_token: tr.refresh_token,
        expires_at,
        refresh_token_expires_at,
        user_login,
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ─── Local callback server ────────────────────────────────────────────────────

fn oauth_callback_router(
    expected_state: String,
    tx: Arc<TokioMutex<Option<oneshot::Sender<Result<String>>>>>,
) -> axum::Router {
    use axum::{Router, extract::Query, response::Html, routing::get};
    use std::collections::HashMap;

    Router::new().route(
        "/callback",
        get(move |Query(params): Query<HashMap<String, String>>| {
            let tx = tx.clone();
            let expected = expected_state.clone();
            async move {
                // Check for OAuth error
                if let Some(error) = params.get("error") {
                    let msg = params
                        .get("error_description")
                        .cloned()
                        .unwrap_or_else(|| error.clone());
                    if let Some(sender) = tx.lock().await.take() {
                        let _ = sender.send(Err(anyhow!("{}", msg)));
                    }
                    return Html(html_error(&msg));
                }

                let code = match params.get("code").cloned() {
                    Some(c) => c,
                    None => {
                        let msg = "Missing authorization code";
                        if let Some(sender) = tx.lock().await.take() {
                            let _ = sender.send(Err(anyhow!("{}", msg)));
                        }
                        return Html(html_error(msg));
                    }
                };

                if params.get("state").map(String::as_str) != Some(&expected) {
                    let msg = "Invalid state — potential CSRF attack";
                    if let Some(sender) = tx.lock().await.take() {
                        let _ = sender.send(Err(anyhow!("{}", msg)));
                    }
                    return Html(html_error(msg));
                }

                if let Some(sender) = tx.lock().await.take() {
                    let _ = sender.send(Ok(code));
                }
                Html(html_success())
            }
        }),
    )
}

async fn spawn_callback_server(app: axum::Router) -> Result<(tokio::task::JoinHandle<()>, u16)> {
    use std::net::SocketAddr;
    let addr = SocketAddr::from(([127, 0, 0, 1], OAUTH_PORT));
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            anyhow!(
                "OAuth callback port {} is already in use. Stop the process using it and retry.",
                OAUTH_PORT
            )
        } else {
            anyhow!("Failed to bind OAuth callback port {}: {}", OAUTH_PORT, e)
        }
    })?;
    let port = listener.local_addr()?.port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((handle, port))
}

fn html_success() -> String {
    r#"<!doctype html><html><head><title>Djinn — GitHub App Connected</title></head>
<body style="font-family:system-ui;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#131010;color:#f1ecec">
<div style="text-align:center">
  <h1 style="color:#f1ecec">GitHub App Connected</h1>
  <p style="color:#b7b1b1">You can close this window and return to Djinn.</p>
</div>
<script>setTimeout(()=>window.close(),2000)</script>
</body></html>"#.to_string()
}

fn html_error(error: &str) -> String {
    format!(
        r#"<!doctype html><html><head><title>Djinn — Authorization Failed</title></head>
<body style="font-family:system-ui;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#131010;color:#f1ecec">
<div style="text-align:center">
  <h1 style="color:#fc533a">Authorization Failed</h1>
  <p style="color:#ff917b;font-family:monospace">{}</p>
</div>
</body></html>"#,
        error
    )
}

fn open_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", url])
        .spawn();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    tracing::info!("Please open this URL in your browser: {}", url);
}

// ─── Full PKCE flow ──────────────────────────────────────────────────────────

/// Perform the full GitHub App PKCE OAuth flow.
///
/// 1. Loads client_id from the credential vault (falls back to DEFAULT_CLIENT_ID).
/// 2. Checks DB cache; returns immediately if unexpired.
/// 3. Attempts a silent token refresh if the cached token is expired.
/// 4. Falls back to a full browser redirect if refresh fails or no cache exists.
///
/// Tokens are persisted to the encrypted credential DB on success.
pub async fn run_github_app_flow(repo: &CredentialRepository) -> Result<GitHubAppTokens> {
    // Load client_id from vault or fall back to default.
    let client_id = match repo.get_decrypted(GITHUB_APP_CLIENT_ID_KEY).await {
        Ok(Some(id)) if !id.is_empty() => id,
        _ => DEFAULT_CLIENT_ID.to_string(),
    };

    // 1. Check cache
    if let Some(cached) = GitHubAppTokens::load_from_db(repo).await {
        if !cached.is_expired() {
            tracing::debug!("GitHubApp: using cached access token");
            return Ok(cached);
        }
        tracing::debug!("GitHubApp: cached token expired, attempting refresh");
        match refresh_token(&client_id, &cached.refresh_token).await {
            Ok(tr) => {
                let tokens = token_response_to_tokens(tr, cached.user_login.clone());
                let _ = tokens.save_to_db(repo).await;
                tracing::info!("GitHubApp: token refreshed successfully");
                return Ok(tokens);
            }
            Err(e) => {
                // Do NOT clear tokens — keep the refresh token for future retries.
                tracing::warn!("GitHubApp: token refresh failed, starting full flow: {}", e);
            }
        }
    }

    // 2. Full PKCE browser flow
    let pkce = generate_pkce();
    let csrf_state = generate_state();
    let redirect_uri = format!("http://localhost:{}/callback", OAUTH_PORT);
    let auth_url = build_authorize_url(&client_id, &redirect_uri, &pkce, &csrf_state)?;

    let (tx, rx) = oneshot::channel::<Result<String>>();
    let tx = Arc::new(TokioMutex::new(Some(tx)));
    let app = oauth_callback_router(csrf_state, tx);
    let (server_handle, _port) = spawn_callback_server(app).await?;

    tracing::info!("GitHubApp OAuth: opening browser for authorization");
    open_browser(&auth_url);

    // 3. Wait for callback (5-minute timeout)
    let code =
        match tokio::time::timeout(std::time::Duration::from_secs(OAUTH_TIMEOUT_SECS), rx).await {
            Ok(Ok(result)) => {
                server_handle.abort();
                result?
            }
            Ok(Err(_)) => {
                server_handle.abort();
                return Err(anyhow!("OAuth callback channel closed unexpectedly"));
            }
            Err(_) => {
                server_handle.abort();
                return Err(anyhow!(
                    "OAuth flow timed out after {} seconds",
                    OAUTH_TIMEOUT_SECS
                ));
            }
        };

    // 4. Exchange code for tokens
    let tr = exchange_code(&client_id, &code, &redirect_uri, &pkce).await?;
    let tokens = token_response_to_tokens(tr, None);
    tokens.save_to_db(repo).await?;
    tracing::info!("GitHubApp OAuth: authentication successful");
    Ok(tokens)
}

/// Store the GitHub App installation ID in the credential vault.
pub async fn store_installation_id(
    installation_id: &str,
    repo: &CredentialRepository,
) -> Result<()> {
    repo.set("github_app", GITHUB_INSTALLATION_ID_KEY, installation_id)
        .await
        .map_err(|e| anyhow!("failed to store GitHub installation ID: {e}"))?;
    Ok(())
}

/// Load the stored GitHub App installation ID from the credential vault.
pub async fn load_installation_id(repo: &CredentialRepository) -> Option<String> {
    repo.get_decrypted(GITHUB_INSTALLATION_ID_KEY).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_is_url_safe() {
        let pkce = generate_pkce();
        assert!(
            pkce.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert!(!pkce.challenge.is_empty());
    }

    #[test]
    fn state_is_url_safe() {
        let state = generate_state();
        assert!(
            state
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert!(!state.is_empty());
    }

    #[test]
    fn expired_token_detection() {
        let tokens = GitHubAppTokens {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: now_secs() - 100,
            refresh_token_expires_at: None,
            user_login: None,
        };
        assert!(tokens.is_expired());

        let tokens_valid = GitHubAppTokens {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: now_secs() + 3600,
            refresh_token_expires_at: None,
            user_login: None,
        };
        assert!(!tokens_valid.is_expired());
    }

    #[test]
    fn token_response_sets_expires_at_from_expires_in() {
        let before = now_secs();
        let tr = TokenResponse {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_in: Some(3600),
            refresh_token_expires_in: Some(86400),
        };
        let tokens = token_response_to_tokens(tr, Some("djinn-user".into()));
        let after = now_secs();
        assert!(tokens.expires_at >= before + 3600);
        assert!(tokens.expires_at <= after + 3600);
        assert!(tokens.refresh_token_expires_at.is_some());
        assert_eq!(tokens.user_login.as_deref(), Some("djinn-user"));
    }
}
