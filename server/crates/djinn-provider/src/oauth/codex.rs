//! Codex OAuth — Authorization Code + PKCE flow.
//!
//! The flow is split into two halves that share a [`CodexPendingStore`]:
//!
//!   * [`start_codex_oauth`] generates PKCE + state, stashes the PKCE verifier
//!     in the pending store keyed by state, and returns the `authorize_url`
//!     the browser should be sent to.
//!   * [`finish_codex_oauth`] — invoked from the main axum router when the
//!     browser hits `/auth/callback` — looks up the pending entry
//!     by state, exchanges the authorization code for tokens, and persists
//!     them in the encrypted credentials table.
//!
//! There is no longer a TCP listener on `:1455` — the redirect URL points
//! directly at `djinn-server` on its main port (`DJINN_PUBLIC_URL`), so the
//! same port forward that serves the SPA/API also terminates the OAuth
//! callback. This lets a kind/minikube deployment complete Codex sign-up
//! without exposing a second port.

use anyhow::{Result, anyhow};
use base64::Engine as _;
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::repos::CredentialRepository;

// ─── Constants ────────────────────────────────────────────────────────────────

/// OAuth client ID for ChatGPT Codex.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// OpenAI auth issuer base URL.
const ISSUER: &str = "https://auth.openai.com";

/// Codex API endpoint (responses sub-path appended at call site).
pub const CODEX_API_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// How long a started-but-not-finished OAuth attempt lives in the pending
/// store before being swept. The canonical OpenAI flow gives the user five
/// minutes to complete the browser redirect; we match that.
const OAUTH_PENDING_TTL_SECS: u64 = 300;

/// OAuth scopes requested.
const OAUTH_SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

/// Default model for ChatGPT Codex provider.
pub const CODEX_DEFAULT_MODEL: &str = "gpt-5.1-codex";

/// Credential key name for DB-stored OAuth tokens.
pub const CODEX_OAUTH_DB_KEY: &str = "__OAUTH_CHATGPT_CODEX";

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

    /// Load tokens from the encrypted credential DB.
    /// Falls back to the filesystem cache and migrates it into the DB.
    pub async fn load_from_db(repo: &CredentialRepository) -> Option<Self> {
        // Try DB first.
        if let Ok(Some(json)) = repo.get_decrypted(CODEX_OAUTH_DB_KEY).await {
            if let Ok(tokens) = serde_json::from_str::<Self>(&json) {
                return Some(tokens);
            }
            tracing::warn!("Codex: corrupt token JSON in DB, ignoring");
        }
        // Fallback: migrate from filesystem.
        if let Some(tokens) = Self::load_cached() {
            tracing::info!("Codex: migrating tokens from filesystem to DB");
            if let Err(e) = tokens.save_to_db(repo).await {
                tracing::warn!("Codex: migration save failed: {e}");
            }
            // Remove old file after successful migration.
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

// ─── PKCE helpers ─────────────────────────────────────────────────────────────

#[derive(Clone)]
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

// ─── Pending store ────────────────────────────────────────────────────────────

/// Per-attempt state that `start_codex_oauth` stashes until the browser
/// redirect comes back and `finish_codex_oauth` claims it by `state`.
struct PendingAuth {
    pkce: PkceChallenge,
    redirect_uri: String,
    repo: CredentialRepository,
    created_at: Instant,
}

/// In-memory, process-wide pending-auth table keyed by the OAuth `state`
/// parameter.
///
/// Entries expire after [`OAUTH_PENDING_TTL_SECS`]. Expiry is checked
/// lazily on lookup (and opportunistically on insert) — no background
/// sweep task required.
pub struct CodexPendingStore {
    inner: tokio::sync::Mutex<HashMap<String, PendingAuth>>,
}

impl CodexPendingStore {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn insert(&self, state: String, entry: PendingAuth) {
        let mut guard = self.inner.lock().await;
        // Opportunistic sweep so the map doesn't grow unbounded when flows
        // start but never complete (network failure, user closes the tab).
        let ttl = Duration::from_secs(OAUTH_PENDING_TTL_SECS);
        let now = Instant::now();
        guard.retain(|_, v| now.duration_since(v.created_at) < ttl);
        guard.insert(state, entry);
    }

    async fn take(&self, state: &str) -> Option<PendingAuth> {
        let mut guard = self.inner.lock().await;
        let entry = guard.remove(state)?;
        if entry.created_at.elapsed() >= Duration::from_secs(OAUTH_PENDING_TTL_SECS) {
            tracing::warn!(state, "Codex OAuth: discarded expired pending entry");
            return None;
        }
        Some(entry)
    }

    #[cfg(test)]
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }
}

impl Default for CodexPendingStore {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Split flow: start + finish ───────────────────────────────────────────────

/// Data returned from [`start_codex_oauth`] — the URL to redirect the
/// browser to and the CSRF `state` used as the lookup key.
#[derive(Debug, Clone, Serialize)]
pub struct CodexOAuthStart {
    pub authorize_url: String,
    pub state: String,
}

/// Begin a Codex OAuth flow.
///
/// Short-circuits and returns `Ok(None)` when valid cached tokens (or a
/// token that can be refreshed silently) are already available — the
/// caller should treat that as "already connected". Otherwise stashes a
/// pending entry, emits `oauth.open_browser` so the UI pops the
/// authorisation URL, and returns the URL + state so the MCP tool
/// response can surface them too.
pub async fn start_codex_oauth(
    pending: Arc<CodexPendingStore>,
    repo: CredentialRepository,
    events: &EventBus,
    public_url: &str,
) -> Result<Option<CodexOAuthStart>> {
    // 1. If we already have usable tokens, don't bother starting a new flow.
    if let Some(cached) = CodexTokens::load_from_db(&repo).await {
        if !cached.is_expired() {
            tracing::debug!("Codex: start_codex_oauth short-circuit — cached token still valid");
            return Ok(None);
        }
        tracing::debug!("Codex: cached token expired, attempting silent refresh");
        match refresh_token(ISSUER, &cached.refresh_token).await {
            Ok(tr) => {
                let account_id = pick_account_id(&tr).or(cached.account_id.clone());
                let tokens = token_response_to_tokens(tr, account_id);
                let _ = tokens.save_to_db(&repo).await;
                tracing::info!("Codex: token refreshed silently, skipping browser flow");
                return Ok(None);
            }
            Err(e) => {
                // Keep the refresh token around — a future attempt can retry.
                tracing::warn!("Codex: silent refresh failed, starting browser flow: {}", e);
            }
        }
    }

    // 2. Full browser PKCE flow — stash state, build URL, emit event.
    let pkce = generate_pkce();
    let csrf_state = generate_state();
    // OpenAI's Codex OAuth client (app_EMoamEEZ73f0CkXaXp7hrann, same as the
    // Codex CLI) pins the redirect to exactly http://localhost:1455/auth/callback.
    // The path + port are both whitelisted. The operator has to port-forward
    // :1455 to reach the server — see server/docker/README.md.
    // Override via CODEX_REDIRECT_URI if your deployment uses a different
    // (custom, OpenAI-registered) OAuth client.
    let _ = public_url;
    let redirect_uri = std::env::var("CODEX_REDIRECT_URI")
        .unwrap_or_else(|_| "http://localhost:1455/auth/callback".to_string());
    let authorize_url = build_authorize_url(&redirect_uri, &pkce, &csrf_state)?;

    pending
        .insert(
            csrf_state.clone(),
            PendingAuth {
                pkce,
                redirect_uri,
                repo,
                created_at: Instant::now(),
            },
        )
        .await;

    tracing::info!("Codex OAuth: emitting open_browser event");
    events.send(DjinnEventEnvelope::oauth_open_browser(
        "chatgpt_codex",
        &authorize_url,
    ));

    Ok(Some(CodexOAuthStart {
        authorize_url,
        state: csrf_state,
    }))
}

/// Complete a Codex OAuth flow.
///
/// Looks up the pending entry by `state`, exchanges `code` for tokens,
/// persists the tokens in the encrypted credentials table, and returns
/// them. The `credential.updated` SSE event is emitted as a side-effect
/// of the repository save — no extra event work here.
pub async fn finish_codex_oauth(
    pending: Arc<CodexPendingStore>,
    code: &str,
    state: &str,
) -> Result<CodexTokens> {
    let Some(entry) = pending.take(state).await else {
        return Err(anyhow!(
            "Unknown or expired OAuth state — start the Codex sign-in flow again"
        ));
    };
    let tr = exchange_code(ISSUER, code, &entry.redirect_uri, &entry.pkce).await?;
    let account_id = pick_account_id(&tr);
    let tokens = token_response_to_tokens(tr, account_id);
    tokens.save_to_db(&entry.repo).await?;
    tracing::info!("Codex OAuth: authentication successful");
    Ok(tokens)
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
    fn extract_account_id_from_jwt_payload() {
        // Build a minimal JWT with chatgpt_account_id claim (no signature verification needed)
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

    #[test]
    fn build_authorize_url_includes_redirect_uri_and_state() {
        let pkce = generate_pkce();
        let url = build_authorize_url("http://localhost:3000/auth/callback", &pkce, "st8")
            .expect("build_authorize_url");
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A3000%2Fapi%2Foauth%2Fcodex%2Fcallback"));
        assert!(url.contains("state=st8"));
        assert!(url.contains("code_challenge="));
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
    }

    /// Finish rejects an unknown state without calling OpenAI. This is the
    /// CSRF path: a bogus state must be an error, not an accidental success.
    #[tokio::test]
    async fn finish_codex_oauth_rejects_unknown_state() {
        let store = Arc::new(CodexPendingStore::new());
        let err = finish_codex_oauth(store, "code", "no-such-state")
            .await
            .expect_err("must reject unknown state");
        let msg = err.to_string();
        assert!(
            msg.contains("Unknown or expired"),
            "unexpected error: {msg}"
        );
    }

    /// Expired entries disappear on lookup. We simulate the TTL by
    /// hand-mutating `created_at` because real time-travel tests are
    /// brittle; the behaviour we care about is the lazy sweep.
    #[tokio::test]
    async fn pending_store_evicts_expired_entries_on_take() {
        let store = CodexPendingStore::new();
        let entry = PendingAuth {
            pkce: generate_pkce(),
            redirect_uri: "http://x/auth/callback".into(),
            repo: dummy_credential_repo(),
            // Created 10 minutes ago — well past the 5-minute TTL.
            created_at: Instant::now() - Duration::from_secs(OAUTH_PENDING_TTL_SECS + 60),
        };
        store.inner.lock().await.insert("stale".into(), entry);
        assert!(store.take("stale").await.is_none());
    }

    /// Happy-path insert/take round trip — entry survives lookup once,
    /// and is consumed (so a replay of the same `state` fails).
    #[tokio::test]
    async fn pending_store_insert_take_is_single_use() {
        let store = CodexPendingStore::new();
        let entry = PendingAuth {
            pkce: generate_pkce(),
            redirect_uri: "http://x/auth/callback".into(),
            repo: dummy_credential_repo(),
            created_at: Instant::now(),
        };
        store.insert("live".into(), entry).await;
        assert_eq!(store.len().await, 1);
        assert!(store.take("live").await.is_some());
        assert!(store.take("live").await.is_none());
    }

    /// Minimal helper — the tests that need a `CredentialRepository` only
    /// construct `PendingAuth` to exercise store mechanics; they never hit
    /// the DB. Build one against a dummy in-memory database so `save_to_db`
    /// would explode loudly if it were ever called here.
    fn dummy_credential_repo() -> CredentialRepository {
        let db = djinn_db::Database::open_in_memory().expect("in-memory db");
        CredentialRepository::new(db, EventBus::noop())
    }
}
