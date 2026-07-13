//! Resolved GitHub App configuration.
//!
//! Holds the credentials needed to mint App JWTs and complete user-to-server
//! OAuth. The server installs its resolved Secret-or-persisted configuration
//! into a process-wide runtime cache; callers fall back to environment
//! variables before server initialization:
//!
//! - `GITHUB_APP_ID`, `GITHUB_APP_SLUG`,
//!   `GITHUB_APP_CLIENT_ID`, `GITHUB_APP_CLIENT_SECRET`,
//!   `GITHUB_APP_PRIVATE_KEY` (or `_PATH`),
//!   `GITHUB_APP_WEBHOOK_SECRET`, `DJINN_PUBLIC_URL`.
//!
//! Production deployments normally mount these via the Helm chart's
//! `djinn-github-app` Secret. Self-setup deployments persist the same shape in
//! the encrypted credential vault and hot-swap this runtime cache after the
//! manifest exchange, so provider-level JWT and installation-token consumers
//! do not need environment mirroring.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock, RwLock};

use super::{ENV_PRIVATE_KEY, ENV_PRIVATE_KEY_PATH};

/// Env var: GitHub App webhook secret (HMAC key for signed deliveries).
pub const ENV_WEBHOOK_SECRET: &str = "GITHUB_APP_WEBHOOK_SECRET";
/// Env var: public base URL where Djinn is reachable (used to build
/// callback / install URLs).
pub const ENV_PUBLIC_URL: &str = "DJINN_PUBLIC_URL";

/// Default public URL fallback when env doesn't define one.
pub const DEFAULT_PUBLIC_URL: &str = "http://127.0.0.1:8372";

/// Resolved GitHub App credentials + identity.
///
/// Built from process env at startup; never persisted by Djinn itself
/// (operators provision via the `djinn-github-app` Kubernetes Secret).
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
    /// Resolve the active config from the installed runtime snapshot, falling
    /// back to environment variables before server initialization.
    ///
    /// Returns `None` if any required env var (App ID, OAuth client id/secret,
    /// private key) is missing — the server then surfaces a "GitHub App not
    /// configured" status to the UI.
    pub fn load() -> Option<Self> {
        runtime_config().as_deref().cloned().or_else(load_from_env)
    }

    /// Build the install URL for this App's slug. Returns `None` if `slug`
    /// is empty.
    pub fn install_url(&self) -> Option<String> {
        let s = self.slug.trim();
        if s.is_empty() {
            return None;
        }
        Some(format!("https://github.com/apps/{s}/installations/new"))
    }
}

fn runtime_config_slot() -> &'static RwLock<Option<Arc<AppConfig>>> {
    static RUNTIME_CONFIG: OnceLock<RwLock<Option<Arc<AppConfig>>>> = OnceLock::new();
    RUNTIME_CONFIG.get_or_init(|| RwLock::new(None))
}

/// Install the server's resolved credential snapshot for all provider-level
/// GitHub App consumers. Replacing the App invalidates cached installation
/// tokens, which are scoped to the previous App identity.
pub fn install_runtime_config(config: Arc<AppConfig>) {
    if let Ok(mut guard) = runtime_config_slot().write() {
        *guard = Some(config);
    }
    super::installations::invalidate_all_cache();
}

/// Clear the process-wide credential snapshot. Environment fallback remains
/// available for callers that run before `AppState::init_app_config`.
pub fn clear_runtime_config() {
    if let Ok(mut guard) = runtime_config_slot().write() {
        *guard = None;
    }
    super::installations::invalidate_all_cache();
}

/// Return the currently installed runtime credential snapshot, if any.
pub fn runtime_config() -> Option<Arc<AppConfig>> {
    runtime_config_slot().read().ok()?.clone()
}

/// Resolve the active App slug from runtime credentials, then env fallback.
pub fn app_slug() -> Option<String> {
    if let Some(config) = runtime_config() {
        let slug = config.slug.trim();
        return (!slug.is_empty()).then(|| slug.to_string());
    }
    std::env::var(super::ENV_APP_SLUG)
        .ok()
        .map(|slug| slug.trim().to_string())
        .filter(|slug| !slug.is_empty())
}

/// Resolve the GitHub App bot's commit identity from the active App slug.
///
/// The numeric GitHub App ID is not the bot account's user ID, so it must not
/// be used as the numeric prefix of a GitHub no-reply address. Djinn does not
/// currently persist the bot user ID; the login-only no-reply form is the safe
/// non-personal fallback until that distinct ID is available.
pub fn bot_git_identity() -> (String, String) {
    let login = format!(
        "{}[bot]",
        app_slug().unwrap_or_else(|| "djinn-bot".to_string())
    );
    let email = format!("{login}@users.noreply.github.com");
    (login, email)
}

/// Resolve the callback base URL registered for the active GitHub App.
///
/// Before self-setup completes there is no runtime config, so the manifest
/// flow uses `DJINN_PUBLIC_URL` (or the localhost default). After credentials
/// are loaded, the persisted provisioning URL wins to keep OAuth redirect URIs
/// aligned with the callback registered on the App.
pub fn public_url() -> String {
    if let Some(config) = runtime_config() {
        let public_url = config.public_url.trim();
        if !public_url.is_empty() {
            return public_url.to_string();
        }
    }
    std::env::var(ENV_PUBLIC_URL)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_PUBLIC_URL.to_string())
}

/// Resolve OAuth client credentials from runtime credentials, then env
/// fallback. Background refresh paths use this because they do not carry an
/// `AppState` handle.
pub fn oauth_client_credentials() -> Option<(String, String)> {
    if let Some(config) = runtime_config() {
        let client_id = config.client_id.trim();
        let client_secret = config.client_secret.trim();
        return (!client_id.is_empty() && !client_secret.is_empty())
            .then(|| (client_id.to_string(), client_secret.to_string()));
    }
    let client_id = std::env::var(super::ENV_CLIENT_ID)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let client_secret = std::env::var(super::ENV_CLIENT_SECRET)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    Some((client_id, client_secret))
}

#[cfg(test)]
pub(crate) fn github_app_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn load_from_env() -> Option<AppConfig> {
    use super::{ENV_APP_ID, ENV_APP_SLUG, ENV_CLIENT_ID, ENV_CLIENT_SECRET};

    let app_id = std::env::var(ENV_APP_ID).ok()?.trim().parse::<u64>().ok()?;
    if app_id == 0 {
        return None;
    }
    let slug = std::env::var(ENV_APP_SLUG).unwrap_or_default();
    let client_id = std::env::var(ENV_CLIENT_ID).unwrap_or_default();
    let client_secret = std::env::var(ENV_CLIENT_SECRET).unwrap_or_default();
    let pem = read_env_pem()?;
    if super::jwt::validate_rsa_private_key(&pem).is_err() {
        return None;
    }
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

    #[test]
    fn runtime_config_is_first_class_without_env_and_clear_restores_isolation() {
        let _lock = github_app_test_lock();
        clear_runtime_config();
        for key in [
            super::super::ENV_APP_ID,
            super::super::ENV_APP_SLUG,
            super::super::ENV_CLIENT_ID,
            super::super::ENV_CLIENT_SECRET,
            super::super::ENV_PRIVATE_KEY,
            super::super::ENV_PRIVATE_KEY_PATH,
        ] {
            unsafe { std::env::remove_var(key) };
        }

        let cfg = fixture();
        install_runtime_config(Arc::new(cfg.clone()));
        assert_eq!(AppConfig::load(), Some(cfg.clone()));
        assert_eq!(app_slug().as_deref(), Some("djinn-bot"));
        assert_eq!(
            oauth_client_credentials(),
            Some((cfg.client_id.clone(), cfg.client_secret.clone()))
        );

        clear_runtime_config();
        assert!(runtime_config().is_none());
        assert!(AppConfig::load().is_none());
        assert!(app_slug().is_none());
        assert!(oauth_client_credentials().is_none());
    }

    #[test]
    fn bot_git_identity_uses_runtime_slug_without_fabricating_a_user_id() {
        let _lock = github_app_test_lock();
        clear_runtime_config();
        unsafe { std::env::remove_var(super::super::ENV_APP_SLUG) };

        install_runtime_config(Arc::new(fixture()));
        assert_eq!(
            bot_git_identity(),
            (
                "djinn-bot[bot]".to_string(),
                "djinn-bot[bot]@users.noreply.github.com".to_string(),
            )
        );

        clear_runtime_config();
    }

    #[test]
    fn public_url_prefers_runtime_provisioning_url() {
        let _lock = github_app_test_lock();
        clear_runtime_config();
        unsafe { std::env::set_var(ENV_PUBLIC_URL, "https://env.example") };

        let mut cfg = fixture();
        cfg.public_url = "https://provisioned.example".to_string();
        install_runtime_config(Arc::new(cfg));
        assert_eq!(public_url(), "https://provisioned.example");

        clear_runtime_config();
        assert_eq!(public_url(), "https://env.example");
        unsafe { std::env::remove_var(ENV_PUBLIC_URL) };
    }
}
