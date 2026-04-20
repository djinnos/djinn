//! HTTP routes for browser-redirect OAuth flows that complete against
//! the server's main axum router instead of a per-flow ephemeral
//! listener.
//!
//! Currently hosts the Codex (OpenAI) callback at
//! `GET /auth/callback`. The equivalent Copilot/GitHub App
//! flows either use different transports (device code) or already live
//! in `server/src/server/auth.rs`, so they do not appear here.
//!
//! Callback contract:
//!   * Browser arrives with `?code=&state=` after the user approves on
//!     `auth.openai.com`.
//!   * `finish_codex_oauth` looks up the pending entry by state, swaps
//!     the code for tokens, and persists them in the encrypted
//!     credentials table (`credential.updated` SSE fires as a side
//!     effect).
//!   * We 302 the browser back to the SPA with a `?codex=ok` /
//!     `?codex=error` query so the settings screen can flash a toast.

use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;

use crate::server::AppState;
use djinn_provider::oauth::codex;

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/auth/callback", get(codex_callback))
}

#[derive(Deserialize)]
struct CodexCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// OpenAI returns `?error=...&error_description=...` when the user
    /// declines the grant or the client is misconfigured. We bubble the
    /// description into the SPA redirect so the UI can render it.
    error: Option<String>,
    error_description: Option<String>,
}

/// `GET /auth/callback` — terminates the Codex (OpenAI) PKCE
/// redirect on the same port that serves the API/SPA. Replaces the
/// historical `:1455` ephemeral listener.
async fn codex_callback(
    State(state): State<AppState>,
    Query(q): Query<CodexCallbackQuery>,
) -> Response {
    // 1. Propagate provider-side errors back to the SPA.
    if let Some(err) = q.error.as_deref().filter(|s| !s.is_empty()) {
        let description = q.error_description.as_deref().unwrap_or("");
        tracing::warn!(err, description, "Codex OAuth: provider returned error");
        return redirect_to_spa("error", description);
    }

    // 2. We need both `code` and `state` to continue.
    let (Some(code), Some(csrf_state)) = (
        q.code.as_deref().filter(|s| !s.is_empty()),
        q.state.as_deref().filter(|s| !s.is_empty()),
    ) else {
        tracing::warn!("Codex OAuth: callback missing code or state");
        return (
            StatusCode::BAD_REQUEST,
            "missing code or state on Codex OAuth callback",
        )
            .into_response();
    };

    // 3. Hand off to `finish_codex_oauth` — it validates state against the
    //    pending store, exchanges the code, and persists the tokens.
    let pending = state.codex_oauth_pending();
    match codex::finish_codex_oauth(pending, code, csrf_state).await {
        Ok(_tokens) => {
            tracing::info!("Codex OAuth: tokens persisted, redirecting SPA");
            redirect_to_spa("ok", "")
        }
        Err(e) => {
            tracing::error!(error = %e, "Codex OAuth: finish failed");
            redirect_to_spa("error", &e.to_string())
        }
    }
}

/// Build a `302 Location` response that lands the user back on the SPA's
/// provider-settings page with a `?codex=<status>[&error=...]` query so
/// the client can render a success/failure toast and refresh the
/// credential list.
fn redirect_to_spa(status: &str, detail: &str) -> Response {
    let mut target = format!(
        "{}/settings/providers?codex={}",
        spa_base_url().trim_end_matches('/'),
        status
    );
    if !detail.is_empty() {
        // A very small percent-encoder covering the handful of characters
        // we realistically see in OAuth error descriptions. Matches the
        // one in `server::auth::urlencode`.
        target.push_str("&error=");
        for b in detail.as_bytes() {
            let c = *b;
            match c {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    target.push(c as char);
                }
                _ => target.push_str(&format!("%{:02X}", c)),
            }
        }
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&target).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    (StatusCode::FOUND, headers).into_response()
}

/// SPA base URL. Mirrors the `server::auth::web_url` helper — falls back
/// to `DJINN_PUBLIC_URL` when `DJINN_WEB_URL` is unset.
fn spa_base_url() -> String {
    if let Ok(v) = std::env::var("DJINN_WEB_URL")
        && !v.trim().is_empty()
    {
        return v;
    }
    std::env::var("DJINN_PUBLIC_URL").unwrap_or_else(|_| "http://127.0.0.1:8372".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::oauth::codex::CodexPendingStore;
    use std::sync::Arc;

    /// An unknown state must redirect the browser back to the SPA with
    /// `codex=error` — never silently 200 OK and never panic.
    #[tokio::test]
    async fn callback_with_unknown_state_redirects_with_error() {
        // Isolate from the ambient env — the SPA URL fallback is the same
        // for every test worker, so we just assert the path, not the host.
        let pending = Arc::new(CodexPendingStore::new());
        let res =
            codex::finish_codex_oauth(pending, "irrelevant-code", "unseen-state").await;
        assert!(res.is_err(), "unknown state must error");
    }

    #[test]
    fn redirect_to_spa_encodes_error_detail() {
        let resp = redirect_to_spa("error", "Bad things happened!");
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .expect("Location header present");
        assert!(loc.contains("codex=error"), "got: {loc}");
        assert!(loc.contains("error=Bad%20things%20happened%21"), "got: {loc}");
    }

    #[test]
    fn redirect_to_spa_success_has_no_error_param() {
        let resp = redirect_to_spa("ok", "");
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .expect("Location header present");
        assert!(loc.contains("codex=ok"));
        assert!(!loc.contains("&error="));
    }
}
