//! Codex OAuth — Authorization Code + PKCE flow.
//!
//! Ported from Goose's `chatgpt_codex.rs`. Handles the full PKCE browser-redirect
//! flow, token caching, and silent refresh for the ChatGPT Codex provider.

use anyhow::{anyhow, Result};
use base64::Engine as _;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex as TokioMutex};

// ─── Constants ────────────────────────────────────────────────────────────────

/// OAuth client ID for ChatGPT Codex (matches Goose's chatgpt_codex.rs).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// OpenAI auth issuer base URL.
const ISSUER: &str = "https://auth.openai.com";

/// Codex API endpoint (responses sub-path appended at call site).
pub const CODEX_API_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// Default OAuth callback port (canonical per OpenAI docs).
const OAUTH_PORT: u16 = 1455;

/// Browser redirect timeout.
const OAUTH_TIMEOUT_SECS: u64 = 300;

/// OAuth scopes requested.
const OAUTH_SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

/// Default model for ChatGPT Codex provider.
pub const CODEX_DEFAULT_MODEL: &str = "gpt-5.1-codex";

// ─── Token types ─────────────────────────────────────────────────────────────

/// Cached token bundle for the Codex provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexTokens {
    /// Bearer token used in API requests.
    pub access_token: String,
    /// Refresh token for silent renewal.
    pub refresh_token: String,
    /// Optional id_token (may carry account_id claims).
    pub id_token: Option<String>,
    /// UTC timestamp when the access token expires.
    pub expires_at: i64,
    /// ChatGPT account ID extracted from JWT claims.
    pub account_id: Option<String>,
}

impl CodexTokens {
    /// Returns true if the access token has expired (with a 60-second buffer).
    pub fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        now >= self.expires_at - 60
    }

    /// Path where tokens are persisted on disk.
    pub fn cache_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".djinn")
            .join("oauth")
            .join("codex.json")
    }

    /// Load cached tokens from disk, returning `None` on any error.
    pub fn load_cached() -> Option<Self> {
        let path = Self::cache_path();
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Persist tokens to disk.
    pub fn save(&self) -> Result<()> {
        let path = Self::cache_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Remove the cached token file (forces a fresh OAuth flow next time).
    pub fn clear() {
        let _ = std::fs::remove_file(Self::cache_path());
    }
}

/// Attempt a silent token refresh using the cached refresh_token.
/// Returns refreshed `CodexTokens` on success, saving them to disk.
pub async fn refresh_cached_token(cached: &CodexTokens) -> Result<CodexTokens> {
    let tr = refresh_token(ISSUER, &cached.refresh_token).await?;
    let account_id = pick_account_id(&tr).or(cached.account_id.clone());
    let tokens = token_response_to_tokens(tr, account_id);
    let _ = tokens.save();
    tracing::info!("Codex: token refreshed successfully (lifecycle)");
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
    let mut bytes = [0u8; 43]; // 43-byte random = URL-safe base64 ~58 chars, well within 128 limit
    rng.fill(&mut bytes).expect("rng fill");
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    PkceChallenge { verifier, challenge }
}

fn generate_state() -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let rng = SystemRandom::new();
    let mut bytes = [0u8; 24];
    rng.fill(&mut bytes).expect("rng fill");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ─── Auth URL ─────────────────────────────────────────────────────────────────

fn build_authorize_url(redirect_uri: &str, pkce: &PkceChallenge, state: &str) -> Result<String> {
    let scopes = OAUTH_SCOPES.join(" ");
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", &scopes),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
    ];
    let query = serde_urlencoded::to_string(params)?;
    Ok(format!("{}/oauth/authorize?{}", ISSUER, query))
}

// ─── Token exchange / refresh ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
    expires_in: Option<i64>,
}

async fn exchange_code(
    issuer: &str,
    code: &str,
    redirect_uri: &str,
    pkce: &PkceChallenge,
) -> Result<TokenResponse> {
    let client = Client::new();
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", &pkce.verifier),
    ];
    let resp = client
        .post(format!("{}/oauth/token", issuer))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&params)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Token exchange failed ({}): {}", status, text));
    }
    Ok(resp.json().await?)
}

async fn refresh_token(issuer: &str, refresh_token: &str) -> Result<TokenResponse> {
    let client = Client::new();
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ];
    let resp = client
        .post(format!("{}/oauth/token", issuer))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&params)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Token refresh failed ({}): {}", status, text));
    }
    Ok(resp.json().await?)
}

fn token_response_to_tokens(tr: TokenResponse, account_id: Option<String>) -> CodexTokens {
    let expires_at = now_secs() + tr.expires_in.unwrap_or(3600);
    CodexTokens {
        access_token: tr.access_token,
        refresh_token: tr.refresh_token,
        id_token: tr.id_token,
        expires_at,
        account_id,
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ─── JWT account-id extraction ────────────────────────────────────────────────

/// Extract `chatgpt_account_id` (or first org id) from a JWT without verifying signature.
/// This is best-effort; if it fails we proceed without the account ID.
pub fn extract_account_id_unverified(jwt: &str) -> Option<String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    // Try chatgpt_account_id directly
    if let Some(id) = value["chatgpt_account_id"].as_str() {
        return Some(id.to_owned());
    }
    // Try nested auth claims
    if let Some(id) = value["https://api.openai.com/auth"]["chatgpt_account_id"].as_str() {
        return Some(id.to_owned());
    }
    // Fall back to first organization id
    if let Some(orgs) = value["organizations"].as_array()
        && let Some(org) = orgs.first()
        && let Some(id) = org["id"].as_str()
    {
        return Some(id.to_owned());
    }
    None
}

fn pick_account_id(tokens: &TokenResponse) -> Option<String> {
    // Prefer id_token since it has richer claims
    if let Some(id_token) = &tokens.id_token
        && let Some(id) = extract_account_id_unverified(id_token)
    {
        return Some(id);
    }
    extract_account_id_unverified(&tokens.access_token)
}

// ─── Local callback server ────────────────────────────────────────────────────

fn oauth_callback_router(
    expected_state: String,
    tx: Arc<TokioMutex<Option<oneshot::Sender<Result<String>>>>>,
) -> axum::Router {
    use axum::{extract::Query, response::Html, routing::get, Router};
    use std::collections::HashMap;

    Router::new().route(
        "/auth/callback",
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

async fn spawn_callback_server(
    app: axum::Router,
) -> Result<(tokio::task::JoinHandle<()>, u16)> {
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
    r#"<!doctype html><html><head><title>Djinn — Authorization Successful</title></head>
<body style="font-family:system-ui;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#131010;color:#f1ecec">
<div style="text-align:center">
  <h1 style="color:#f1ecec">Authorization Successful</h1>
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
    // If we can't identify the OS, log the URL for manual opening
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    tracing::info!("Please open this URL in your browser: {}", url);
}

// ─── Full PKCE flow ──────────────────────────────────────────────────────────

/// Perform the full Codex PKCE OAuth flow.
///
/// 1. Checks disk cache; returns immediately if unexpired.
/// 2. Attempts a silent token refresh if the cached token is expired.
/// 3. Falls back to a full browser redirect if refresh fails or no cache exists.
pub async fn run_codex_flow() -> Result<CodexTokens> {
    // 1. Check cache
    if let Some(cached) = CodexTokens::load_cached() {
        if !cached.is_expired() {
            tracing::debug!("Codex: using cached access token");
            return Ok(cached);
        }
        tracing::debug!("Codex: cached token expired, attempting refresh");
        match refresh_token(ISSUER, &cached.refresh_token).await {
            Ok(tr) => {
                let account_id = pick_account_id(&tr).or(cached.account_id.clone());
                let tokens = token_response_to_tokens(tr, account_id);
                let _ = tokens.save();
                tracing::info!("Codex: token refreshed successfully");
                return Ok(tokens);
            }
            Err(e) => {
                tracing::warn!("Codex: token refresh failed, starting full flow: {}", e);
                CodexTokens::clear();
            }
        }
    }

    // 2. Full PKCE browser flow
    let pkce = generate_pkce();
    let csrf_state = generate_state();
    let redirect_uri = format!("http://localhost:{}/auth/callback", OAUTH_PORT);
    let auth_url = build_authorize_url(&redirect_uri, &pkce, &csrf_state)?;

    let (tx, rx) = oneshot::channel::<Result<String>>();
    let tx = Arc::new(TokioMutex::new(Some(tx)));
    let app = oauth_callback_router(csrf_state, tx);
    let (server_handle, _port) = spawn_callback_server(app).await?;

    tracing::info!("Codex OAuth: opening browser for authorization");
    open_browser(&auth_url);

    // 3. Wait for callback (5-minute timeout)
    let code = match tokio::time::timeout(
        std::time::Duration::from_secs(OAUTH_TIMEOUT_SECS),
        rx,
    )
    .await
    {
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
            return Err(anyhow!("OAuth flow timed out after {} seconds", OAUTH_TIMEOUT_SECS));
        }
    };

    // 4. Exchange code for tokens
    let tr = exchange_code(ISSUER, &code, &redirect_uri, &pkce).await?;
    let account_id = pick_account_id(&tr);
    let tokens = token_response_to_tokens(tr, account_id);
    let _ = tokens.save();
    tracing::info!("Codex OAuth: authentication successful");
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_is_url_safe() {
        let pkce = generate_pkce();
        assert!(pkce.verifier.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(!pkce.challenge.is_empty());
    }

    #[test]
    fn state_is_url_safe() {
        let state = generate_state();
        assert!(state.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(!state.is_empty());
    }

    #[test]
    fn extract_account_id_from_jwt_payload() {
        // Build a minimal JWT with chatgpt_account_id claim (no signature verification needed)
        let payload = serde_json::json!({ "chatgpt_account_id": "acct_test123", "exp": 9999999999i64 });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(payload.to_string().as_bytes());
        let fake_jwt = format!("header.{}.sig", encoded);
        let id = extract_account_id_unverified(&fake_jwt);
        assert_eq!(id.as_deref(), Some("acct_test123"));
    }

    #[test]
    fn expired_token_detection() {
        let tokens = CodexTokens {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            id_token: None,
            expires_at: now_secs() - 100, // already expired
            account_id: None,
        };
        assert!(tokens.is_expired());

        let tokens_valid = CodexTokens {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            id_token: None,
            expires_at: now_secs() + 3600,
            account_id: None,
        };
        assert!(!tokens_valid.is_expired());
    }
}
