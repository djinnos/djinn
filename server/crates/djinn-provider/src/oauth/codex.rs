//! Codex OAuth — Device Code flow.
//!
//! OpenAI's first-party Codex OAuth client (`app_EMoamEEZ73f0CkXaXp7hrann`) only
//! registers `http://localhost:1455/auth/callback` as a redirect URI, so the
//! Authorization Code + PKCE browser flow only works when Djinn runs on the
//! user's machine. The **device code flow** (RFC 8628 with OpenAI-specific
//! paths under `/api/accounts/deviceauth/{usercode,token}`) doesn't need any
//! redirect URI — the user visits `https://auth.openai.com/codex/device`,
//! types a short code, and the server polls for tokens. Works identically
//! on localhost and hosted deployments.
//!
//! Public API:
//!   * [`start_codex_device_auth`] — phase 1: request a user code, emit the
//!     `oauth.device_code` SSE event so the UI can display it, and spawn a
//!     background task that polls until tokens arrive (or the 15-minute TTL
//!     expires). Returns immediately; the UI listens for `credential.updated`.
//!   * [`CodexTokens`] / [`refresh_cached_token`] — token storage and silent
//!     refresh, unchanged from the previous PKCE flow.

use anyhow::{Result, anyhow};
use base64::Engine as _;
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use reqwest::{Client, StatusCode};
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::repos::CredentialRepository;

// ─── Constants ────────────────────────────────────────────────────────────────

/// OAuth client ID for ChatGPT Codex (same as the official CLI).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// OpenAI auth issuer base URL — used for the final `/oauth/token` exchange
/// and for silent refresh.
const ISSUER: &str = "https://auth.openai.com";

/// Base URL for the device-code endpoints. The official CLI hits
/// `{issuer}/api/accounts/deviceauth/{usercode,token}`.
const DEVICE_API_BASE: &str = "https://auth.openai.com/api/accounts";

/// URL the user opens in a browser to enter their code.
pub const VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";

/// Redirect URI sent on the final `/oauth/token` exchange. No browser actually
/// redirects here — OpenAI just requires the field to match what their
/// device-flow server recorded when it issued the authorization code.
const DEVICE_AUTH_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";

/// Codex API endpoint (responses sub-path appended at call site).
pub const CODEX_API_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// Default model for ChatGPT Codex provider.
pub const CODEX_DEFAULT_MODEL: &str = "gpt-5.1-codex";

/// Credential key name for DB-stored OAuth tokens.
pub const CODEX_OAUTH_DB_KEY: &str = "__OAUTH_CHATGPT_CODEX";

/// Maximum wall-clock time we'll poll before giving up.
const DEVICE_AUTH_TTL: Duration = Duration::from_secs(15 * 60);

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
        now_secs() >= self.expires_at - 60
    }

    /// Path where tokens are persisted on disk (legacy filesystem cache).
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

    /// Load tokens from the encrypted credential DB.
    /// Falls back to the filesystem cache and migrates it into the DB.
    pub async fn load_from_db(repo: &CredentialRepository) -> Option<Self> {
        if let Ok(Some(json)) = repo.get_decrypted(CODEX_OAUTH_DB_KEY).await {
            if let Ok(tokens) = serde_json::from_str::<Self>(&json) {
                return Some(tokens);
            }
            tracing::warn!("Codex: corrupt token JSON in DB, ignoring");
        }
        if let Some(tokens) = Self::load_cached() {
            tracing::info!("Codex: migrating tokens from filesystem to DB");
            if let Err(e) = tokens.save_to_db(repo).await {
                tracing::warn!("Codex: migration save failed: {e}");
            }
            let _ = std::fs::remove_file(Self::cache_path());
            return Some(tokens);
        }
        None
    }

    /// Persist tokens to the encrypted credential DB.
    pub async fn save_to_db(&self, repo: &CredentialRepository) -> Result<()> {
        let json = serde_json::to_string(self)?;
        repo.set("chatgpt_codex", CODEX_OAUTH_DB_KEY, &json)
            .await
            .map_err(|e| anyhow!("failed to save codex tokens to DB: {e}"))?;
        Ok(())
    }

    /// Remove tokens from the DB (and any lingering filesystem cache).
    pub async fn clear_from_db(repo: &CredentialRepository) {
        let _ = repo.delete(CODEX_OAUTH_DB_KEY).await;
        let _ = std::fs::remove_file(Self::cache_path());
    }
}

/// Attempt a silent token refresh using the cached refresh_token.
/// Returns refreshed `CodexTokens` on success, saving them to the DB.
pub async fn refresh_cached_token(
    cached: &CodexTokens,
    repo: &CredentialRepository,
) -> Result<CodexTokens> {
    let tr = refresh_token(ISSUER, &cached.refresh_token).await?;
    let account_id = pick_account_id(&tr).or(cached.account_id.clone());
    let tokens = token_response_to_tokens(tr, account_id);
    tokens.save_to_db(repo).await?;
    tracing::info!("Codex: token refreshed successfully (lifecycle)");
    Ok(tokens)
}

// ─── Token exchange / refresh ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
    expires_in: Option<i64>,
}

/// Exchange the `authorization_code` returned by the device-code poll for a
/// real access/refresh token pair. The PKCE verifier is provided by the
/// OpenAI server (it mints both the challenge and the verifier during the
/// device-auth phase and hands us the verifier in the poll success response).
async fn exchange_code(
    issuer: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenResponse> {
    let client = Client::new();
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", code_verifier),
    ];
    let resp = client
        .post(format!("{issuer}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&params)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Token exchange failed ({status}): {text}"));
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
        .post(format!("{issuer}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&params)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Token refresh failed ({status}): {text}"));
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
/// Best-effort; returns `None` on any parse failure.
pub fn extract_account_id_unverified(jwt: &str) -> Option<String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    if let Some(id) = value["chatgpt_account_id"].as_str() {
        return Some(id.to_owned());
    }
    if let Some(id) = value["https://api.openai.com/auth"]["chatgpt_account_id"].as_str() {
        return Some(id.to_owned());
    }
    if let Some(orgs) = value["organizations"].as_array()
        && let Some(org) = orgs.first()
        && let Some(id) = org["id"].as_str()
    {
        return Some(id.to_owned());
    }
    None
}

fn pick_account_id(tokens: &TokenResponse) -> Option<String> {
    if let Some(id_token) = &tokens.id_token
        && let Some(id) = extract_account_id_unverified(id_token)
    {
        return Some(id);
    }
    extract_account_id_unverified(&tokens.access_token)
}

// ─── Device-code flow ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct UserCodeReq<'a> {
    client_id: &'a str,
}

#[derive(Deserialize)]
struct UserCodeResp {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default, deserialize_with = "deserialize_interval")]
    interval: u64,
}

// OpenAI's device-code API sometimes returns `interval` as a string (e.g. `"5"`).
// Match the Codex CLI parser to stay bug-compatible.
fn deserialize_interval<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    s.trim().parse::<u64>().map_err(de::Error::custom)
}

#[derive(Serialize)]
struct TokenPollReq<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Deserialize)]
struct CodeSuccessResp {
    authorization_code: String,
    #[allow(dead_code)]
    code_challenge: String,
    code_verifier: String,
}

/// Public payload returned to the MCP tool when a device-code flow kicks off.
/// The caller displays `user_code` + `verification_uri_complete` to the user
/// and waits for the background task to emit `credential.updated` via SSE.
#[derive(Debug, Clone, Serialize)]
pub struct CodexDeviceAuth {
    /// The short human-typable code (e.g. `ABCD-1234`).
    pub user_code: String,
    /// URL the user visits to enter the code manually.
    pub verification_uri: String,
    /// Convenience URL with the code pre-filled in a query string.
    pub verification_uri_complete: String,
    /// Recommended polling interval (seconds).
    pub interval: i64,
    /// Hard cap on how long the user has to complete sign-in.
    pub expires_in: i64,
}

/// Begin a Codex device-code flow.
///
/// Short-circuits and returns `Ok(None)` when valid cached tokens (or a token
/// that can be refreshed silently) are already available. Otherwise hits
/// `/deviceauth/usercode`, emits `oauth.device_code` so the UI can surface the
/// code, and spawns a background task that polls `/deviceauth/token` until
/// tokens arrive or the 15-minute TTL elapses.
pub async fn start_codex_device_auth(
    repo: CredentialRepository,
    events: &EventBus,
) -> Result<Option<CodexDeviceAuth>> {
    // 1. If we already have usable tokens, skip the flow.
    if let Some(cached) = CodexTokens::load_from_db(&repo).await {
        if !cached.is_expired() {
            tracing::debug!("Codex: device-code short-circuit — cached token still valid");
            return Ok(None);
        }
        tracing::debug!("Codex: cached token expired, attempting silent refresh");
        match refresh_token(ISSUER, &cached.refresh_token).await {
            Ok(tr) => {
                let account_id = pick_account_id(&tr).or(cached.account_id.clone());
                let tokens = token_response_to_tokens(tr, account_id);
                let _ = tokens.save_to_db(&repo).await;
                tracing::info!("Codex: token refreshed silently, skipping device flow");
                return Ok(None);
            }
            Err(e) => {
                tracing::warn!("Codex: silent refresh failed, starting device flow: {e}");
            }
        }
    }

    // 2. Phase 1 — request a user code.
    let client = Client::new();
    let url = format!("{DEVICE_API_BASE}/deviceauth/usercode");
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&UserCodeReq { client_id: CLIENT_ID })
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("device code request failed ({status}): {text}"));
    }
    let uc: UserCodeResp = resp.json().await?;
    let interval = uc.interval.max(1);
    let expires_in = DEVICE_AUTH_TTL.as_secs() as i64;

    let verification_uri_complete = format!(
        "{VERIFICATION_URI}?user_code={}",
        urlencoding_inline(&uc.user_code)
    );

    let session = CodexDeviceAuth {
        user_code: uc.user_code.clone(),
        verification_uri: VERIFICATION_URI.to_string(),
        verification_uri_complete: verification_uri_complete.clone(),
        interval: interval as i64,
        expires_in,
    };

    events.send(DjinnEventEnvelope::oauth_device_code(
        "chatgpt_codex",
        VERIFICATION_URI,
        &verification_uri_complete,
        &uc.user_code,
        interval as i64,
        expires_in,
    ));

    // 3. Phase 2 — spawn a detached task that polls until tokens arrive,
    //    the user denies, or the TTL elapses. The SSE `credential.updated`
    //    event fires automatically when `save_to_db` commits.
    let poll_repo = repo.clone();
    let device_auth_id = uc.device_auth_id;
    let user_code = uc.user_code;
    tokio::spawn(async move {
        match poll_codex_device_auth(&device_auth_id, &user_code, interval, &poll_repo).await {
            Ok(_) => tracing::info!("Codex: device-code flow completed, tokens persisted"),
            Err(e) => tracing::warn!(error = %e, "Codex: device-code flow failed"),
        }
    });

    Ok(Some(session))
}

async fn poll_codex_device_auth(
    device_auth_id: &str,
    user_code: &str,
    interval: u64,
    repo: &CredentialRepository,
) -> Result<CodexTokens> {
    let client = Client::new();
    let url = format!("{DEVICE_API_BASE}/deviceauth/token");
    let start = Instant::now();

    let code_resp: CodeSuccessResp = loop {
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&TokenPollReq {
                device_auth_id,
                user_code,
            })
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            break resp.json().await?;
        }
        if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
            let elapsed = start.elapsed();
            if elapsed >= DEVICE_AUTH_TTL {
                return Err(anyhow!("device auth timed out after 15 minutes"));
            }
            let sleep_for = Duration::from_secs(interval).min(DEVICE_AUTH_TTL - elapsed);
            tokio::time::sleep(sleep_for).await;
            continue;
        }
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("device auth failed ({status}): {text}"));
    };

    let tr = exchange_code(
        ISSUER,
        &code_resp.authorization_code,
        DEVICE_AUTH_REDIRECT_URI,
        &code_resp.code_verifier,
    )
    .await?;
    let account_id = pick_account_id(&tr);
    let tokens = token_response_to_tokens(tr, account_id);
    tokens.save_to_db(repo).await?;
    Ok(tokens)
}

// Tiny URL-safe query-string encoder for the `?user_code=` suffix. Keeping it
// inline avoids pulling in `urlencoding` just for one caller.
fn urlencoding_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_account_id_from_jwt_payload() {
        let payload =
            serde_json::json!({ "chatgpt_account_id": "acct_test123", "exp": 9999999999i64 });
        let encoded =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
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
            expires_at: now_secs() - 100,
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

    #[test]
    fn interval_deserializer_handles_string_and_int() {
        let as_int: UserCodeResp =
            serde_json::from_str(r#"{"device_auth_id":"x","user_code":"ABCD","interval":"7"}"#)
                .expect("parses string interval");
        assert_eq!(as_int.interval, 7);

        // The CLI also accepts the `usercode` alias — make sure our copy does too.
        let aliased: UserCodeResp =
            serde_json::from_str(r#"{"device_auth_id":"x","usercode":"ABCD","interval":"3"}"#)
                .expect("parses usercode alias");
        assert_eq!(aliased.user_code, "ABCD");
    }

    #[test]
    fn url_encoding_round_trip() {
        assert_eq!(urlencoding_inline("ABCD-1234"), "ABCD-1234");
        assert_eq!(urlencoding_inline("a b/c"), "a%20b%2Fc");
    }
}
