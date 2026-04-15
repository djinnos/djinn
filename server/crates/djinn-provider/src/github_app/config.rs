//! Resolved GitHub App configuration.
//!
//! Holds the credentials needed to mint App JWTs and complete user-to-server
//! OAuth. Loads from two sources, in order:
//!
//! 1. **Persisted DB row** — a single encrypted credential under the key
//!    `__GITHUB_APP_CONFIG`. Written by the manifest auto-provision endpoint
//!    (`/auth/github/app-manifest-callback`). Always wins when present.
//! 2. **Environment variables** — `GITHUB_APP_ID`, `GITHUB_APP_SLUG`,
//!    `GITHUB_APP_CLIENT_ID`, `GITHUB_APP_CLIENT_SECRET`,
//!    `GITHUB_APP_PRIVATE_KEY` (or `_PATH`), `GITHUB_APP_WEBHOOK_SECRET`,
//!    `DJINN_PUBLIC_URL`. Maintains backwards compatibility with operators
//!    that hand-set `.env` files instead of using the manifest flow.
//!
//! The struct is cloned cheaply via `Arc` and held inside the server
//! `AppState` behind a `RwLock`, so a successful manifest provisioning can
//! hot-swap the value without restarting the process.

use serde::{Deserialize, Serialize};

use djinn_core::events::EventBus;
use djinn_db::Database;

use crate::repos::credential::CredentialRepository;

use super::{ENV_APP_ID, ENV_APP_SLUG, ENV_CLIENT_ID, ENV_CLIENT_SECRET, ENV_PRIVATE_KEY, ENV_PRIVATE_KEY_PATH};

/// Credential `key_name` used for the JSON blob storing the full App config.
pub const APP_CONFIG_KEY: &str = "__GITHUB_APP_CONFIG";
/// Credential `provider_id` for the App config row.
pub const APP_CONFIG_PROVIDER: &str = "github-app";

/// Env var: GitHub App webhook secret (HMAC key for signed deliveries).
pub const ENV_WEBHOOK_SECRET: &str = "GITHUB_APP_WEBHOOK_SECRET";
/// Env var: public base URL where Djinn is reachable (used to build
/// callback / install URLs).
pub const ENV_PUBLIC_URL: &str = "DJINN_PUBLIC_URL";

/// Default public URL fallback when neither the DB nor env defines one.
pub const DEFAULT_PUBLIC_URL: &str = "http://127.0.0.1:8372";

/// Persisted GitHub App credentials + identity.
///
/// Serialised as JSON into the encrypted `credentials.encrypted_value`
/// column under [`APP_CONFIG_KEY`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    /// Numeric GitHub App ID (`iss` claim in App JWTs).
    pub app_id: u64,
    /// App slug; used to build `https://github.com/apps/<slug>/installations/new`.
    pub slug: String,
    /// User-to-server OAuth client id.
    pub client_id: String,
    /// User-to-server OAuth client secret.
    pub client_secret: String,
    /// RSA private key PEM (multi-line).
    pub pem: String,
    /// HMAC key for signed webhook deliveries.
    pub webhook_secret: String,
    /// Public base URL the App was provisioned against (e.g. the
    /// `DJINN_PUBLIC_URL` at provisioning time). Used so the app picks the
    /// right callback even if the env var changes later.
    pub public_url: String,
}

impl AppConfig {
    /// Resolve the active config: DB row wins, then env vars.
    ///
    /// Returns `None` if neither source has enough information to mint a JWT.
    pub async fn load(db: &Database, events: EventBus) -> Option<Self> {
        if let Some(cfg) = load_from_db(db.clone(), events).await {
            return Some(cfg);
        }
        load_from_env()
    }

    /// Build the install URL for this App's slug. Returns `None` if `slug`
    /// is empty (shouldn't happen for DB-loaded configs).
    pub fn install_url(&self) -> Option<String> {
        let s = self.slug.trim();
        if s.is_empty() {
            return None;
        }
        Some(format!("https://github.com/apps/{s}/installations/new"))
    }

    /// Mirror this config back into the process environment so existing
    /// callers that still read `std::env::var(...)` (the JWT minter, the
    /// install_url helper, etc.) immediately pick up the new credentials
    /// without a restart.
    ///
    /// Safety: `set_var`/`remove_var` are unsafe in 2024 edition because
    /// they're not thread-safe. We document that this is only called
    /// during the provisioning callback, which is single-flighted by
    /// browser navigation.
    pub fn export_to_env(&self) {
        // SAFETY: see doc comment.
        unsafe {
            std::env::set_var(ENV_APP_ID, self.app_id.to_string());
            std::env::set_var(ENV_APP_SLUG, &self.slug);
            std::env::set_var(ENV_CLIENT_ID, &self.client_id);
            std::env::set_var(ENV_CLIENT_SECRET, &self.client_secret);
            std::env::set_var(ENV_PRIVATE_KEY, &self.pem);
            std::env::set_var(ENV_WEBHOOK_SECRET, &self.webhook_secret);
            std::env::set_var(ENV_PUBLIC_URL, &self.public_url);
        }
    }
}

async fn load_from_db(db: Database, events: EventBus) -> Option<AppConfig> {
    let repo = CredentialRepository::new(db, events);
    match repo.get_github_app_config().await {
        Ok(opt) => opt,
        Err(e) => {
            tracing::warn!(error = %e, "github_app::AppConfig::load: db read failed");
            None
        }
    }
}

fn load_from_env() -> Option<AppConfig> {
    let app_id = std::env::var(ENV_APP_ID).ok()?.trim().parse::<u64>().ok()?;
    let slug = std::env::var(ENV_APP_SLUG).unwrap_or_default();
    let client_id = std::env::var(ENV_CLIENT_ID).unwrap_or_default();
    let client_secret = std::env::var(ENV_CLIENT_SECRET).unwrap_or_default();
    let pem = read_env_pem()?;
    let webhook_secret = std::env::var(ENV_WEBHOOK_SECRET).unwrap_or_default();
    let public_url =
        std::env::var(ENV_PUBLIC_URL).unwrap_or_else(|_| DEFAULT_PUBLIC_URL.to_string());

    if client_id.is_empty() || client_secret.is_empty() {
        // App ID + key alone aren't enough to complete user OAuth.
        return None;
    }

    Some(AppConfig {
        app_id,
        slug,
        client_id,
        client_secret,
        pem,
        webhook_secret,
        public_url,
    })
}

fn read_env_pem() -> Option<String> {
    if let Ok(inline) = std::env::var(ENV_PRIVATE_KEY) {
        let inline = inline.trim();
        if !inline.is_empty() {
            return Some(inline.replace("\\n", "\n"));
        }
    }
    if let Ok(path) = std::env::var(ENV_PRIVATE_KEY_PATH) {
        let p = path.trim();
        if !p.is_empty() {
            return std::fs::read_to_string(p).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> AppConfig {
        AppConfig {
            app_id: 12345,
            slug: "djinn-bot".into(),
            client_id: "Iv1.abc".into(),
            client_secret: "shh".into(),
            pem: "-----BEGIN RSA PRIVATE KEY-----\n...\n-----END RSA PRIVATE KEY-----\n".into(),
            webhook_secret: "wsecret".into(),
            public_url: "https://djinn.example.com".into(),
        }
    }

    #[test]
    fn install_url_uses_slug() {
        let cfg = fixture();
        assert_eq!(
            cfg.install_url().as_deref(),
            Some("https://github.com/apps/djinn-bot/installations/new")
        );
    }

    #[test]
    fn install_url_none_when_slug_empty() {
        let mut cfg = fixture();
        cfg.slug = "  ".into();
        assert!(cfg.install_url().is_none());
    }

    #[test]
    fn json_round_trip() {
        let cfg = fixture();
        let s = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }
}
