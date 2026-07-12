//! Deterministic credential-source resolution for the GitHub App.
//!
//! The server may obtain its GitHub App credentials from two sources:
//!
//! 1. **Environment variables / Kubernetes Secret** (`GITHUB_APP_*` env vars).
//!    Highest priority. If *any* required env var is set, the env source is
//!    considered attempted — a partially-configured env is **fatal** and never
//!    silently falls through to persisted credentials.
//!
//! 2. **Encrypted persistence store** (the existing `CredentialRepository`
//!    boundary). Loaded only when no env vars are present. Supports
//!    hot-reload after a manifest exchange without a process restart.
//!
//! The [`CredentialSourceState`] enum makes the resolution outcome explicit so
//! route/UI code can branch on typed states instead of `Option`-only ambiguity.

use std::sync::Arc;

use super::AppConfig;
use crate::repos::CredentialRepository;

/// Well-known credential store identifiers for persisted GitHub App config.
///
/// Stored as a single encrypted JSON blob in the `credentials` table using the
/// existing [`CredentialRepository`] boundary. This avoids creating a new
/// raw-secret table while preserving encryption at rest.
pub const CRED_PROVIDER_ID: &str = "github_app";
pub const CRED_KEY_NAME: &str = "__GITHUB_APP_CONFIG";

/// Source from which the active GitHub App configuration was resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Environment variables / Kubernetes Secret.
    Secret,
    /// Encrypted persistence store.
    Persisted,
}

/// Detailed reason why Secret/env credentials are invalid or incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSecretDetail {
    /// Specific env vars that were present but malformed or missing.
    pub issues: Vec<&'static str>,
}

/// Explicit resolution state for GitHub App credentials.
///
/// Replaces the previous `Option<AppConfig>` ambiguity with typed states that
/// route/UI code can branch on without silent fallthrough. The precedence
/// rules are:
///
/// - `ValidSecret` always wins over persisted credentials.
/// - `InvalidSecret` is **fatal**: the server must NOT silently fall through
///   to persisted credentials, because the operator clearly *intended* to
///   configure via env but made an error.
/// - `ValidPersisted` works regardless of the `DJINN_ENABLE_SELF_SETUP` flag.
/// - `UndecryptablePersisted` means data exists but can't be decrypted
///   (wrong encryption key, corrupt data, etc.).
/// - `Unconfigured` means no credentials are available from any source.
#[derive(Debug, Clone)]
pub enum CredentialSourceState {
    /// Valid credentials loaded from environment variables / Kubernetes Secret.
    /// These take absolute precedence over any persisted credentials.
    ValidSecret(Arc<AppConfig>),

    /// Environment credentials were present but invalid or incomplete.
    /// This is a **fatal** state — the server MUST NOT fall back to persisted
    /// credentials. The operator needs to fix the env configuration.
    InvalidSecret(InvalidSecretDetail),

    /// Valid credentials loaded from the encrypted persistence store.
    /// Works regardless of `DJINN_ENABLE_SELF_SETUP` flag — the flag only
    /// controls whether the *setup UI* is advertised, not whether already-
    /// persisted credentials are usable.
    ValidPersisted(Arc<AppConfig>),

    /// Credentials exist in the persistence store but could not be decrypted.
    /// The operator must re-provision (e.g., re-run the manifest setup flow
    /// or fix the encryption key).
    UndecryptablePersisted,

    /// No credentials available from any source.
    Unconfigured,
}

impl CredentialSourceState {
    /// Extract the inner `AppConfig` if the state represents usable
    /// credentials (either Secret or Persisted).
    pub fn app_config(&self) -> Option<&Arc<AppConfig>> {
        match self {
            Self::ValidSecret(cfg) | Self::ValidPersisted(cfg) => Some(cfg),
            _ => None,
        }
    }

    /// Which source produced the usable config, if any.
    pub fn source(&self) -> Option<ConfigSource> {
        match self {
            Self::ValidSecret(_) => Some(ConfigSource::Secret),
            Self::ValidPersisted(_) => Some(ConfigSource::Persisted),
            _ => None,
        }
    }

    /// Whether this state represents usable credentials.
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::ValidSecret(_) | Self::ValidPersisted(_))
    }
}

/// Detailed result of attempting to load credentials from env vars.
enum EnvLoadResult {
    /// All required env vars present and valid.
    Valid(AppConfig),
    /// Some env vars present but configuration is incomplete/invalid.
    Invalid(InvalidSecretDetail),
    /// No env vars present at all — env source was not attempted.
    Absent,
}

/// Check whether *any* of the core GitHub App env vars are *present in the
/// environment* — even with an empty value.
///
/// "Core" means the vars that are *required* for a valid config:
/// `GITHUB_APP_ID`, `GITHUB_APP_CLIENT_ID`, `GITHUB_APP_CLIENT_SECRET`,
/// and `GITHUB_APP_PRIVATE_KEY`/`GITHUB_APP_PRIVATE_KEY_PATH`.
///
/// Presence is defined as the env var having been **set** in the process
/// environment, *regardless of whether the value is empty*. Empty values are
/// treated as attempted (but invalid) higher-priority Secret/env configuration
/// and must produce the fatal `InvalidSecret` state — they must NOT silently
/// fall through to persisted credentials.
///
/// Only when the var is *completely unset* (not present at all) is it
/// considered absent; in that case we may silently consult persisted
/// credentials.
fn any_core_env_var_set() -> bool {
    use super::{ENV_APP_ID, ENV_APP_SLUG, ENV_CLIENT_ID, ENV_CLIENT_SECRET};
    use super::{ENV_PRIVATE_KEY, ENV_PRIVATE_KEY_PATH};

    // NOTE: Do NOT filter empty strings out here. An operator setting
    // `GITHUB_APP_ID=""` has clearly attempted Secret/env configuration —
    // silently falling through to persisted credentials would hide their
    // mistake. Empty values are reported as `InvalidSecret` issues downstream.
    for key in [
        ENV_APP_ID,
        ENV_CLIENT_ID,
        ENV_CLIENT_SECRET,
        ENV_PRIVATE_KEY,
        ENV_PRIVATE_KEY_PATH,
    ] {
        if std::env::var_os(key).is_some() {
            return true;
        }
    }
    // ENV_APP_SLUG is informational (not required for a valid config)
    // but if it's set alongside nothing else, treat it as an attempted config.
    if std::env::var_os(ENV_APP_SLUG).is_some() {
        return true;
    }
    false
}

/// Try to load credentials from environment variables with detailed error
/// reporting.
///
/// Returns `EnvLoadResult::Absent` only when *no* core env vars are set.
/// Returns `EnvLoadResult::Invalid` when some vars are present but the
/// configuration is incomplete or malformed.
fn try_load_from_env_detailed() -> EnvLoadResult {
    use super::{ENV_APP_ID, ENV_CLIENT_ID, ENV_CLIENT_SECRET};

    // Quick check: if no core env vars are set at all, it's Absent.
    if !any_core_env_var_set() {
        return EnvLoadResult::Absent;
    }

    // At least one core var is set — attempt full load, collecting issues.
    let mut issues: Vec<&'static str> = Vec::new();

    // APP_ID: required, must parse as a non-zero u64. GitHub App IDs are
    // positive identifiers; accepting zero only defers the failure to JWT/API
    // use and incorrectly advertises the Secret as usable.
    let app_id = match std::env::var(ENV_APP_ID) {
        Ok(val) => {
            let trimmed = val.trim();
            if trimmed.is_empty() {
                issues.push("GITHUB_APP_ID is empty");
                None
            } else {
                match trimmed.parse::<u64>() {
                    Ok(0) => {
                        issues.push("GITHUB_APP_ID must be greater than zero");
                        None
                    }
                    Ok(id) => Some(id),
                    Err(_) => {
                        issues.push("GITHUB_APP_ID is not a valid number");
                        None
                    }
                }
            }
        }
        Err(_) => {
            issues.push("GITHUB_APP_ID is not set");
            None
        }
    };

    // CLIENT_ID: required, must be non-empty.
    let client_id = match std::env::var(ENV_CLIENT_ID) {
        Ok(val) => {
            let trimmed = val.trim().to_string();
            if trimmed.is_empty() {
                issues.push("GITHUB_APP_CLIENT_ID is empty");
                None
            } else {
                Some(trimmed)
            }
        }
        Err(_) => {
            issues.push("GITHUB_APP_CLIENT_ID is not set");
            None
        }
    };

    // CLIENT_SECRET: required, must be non-empty.
    let client_secret = match std::env::var(ENV_CLIENT_SECRET) {
        Ok(val) => {
            let trimmed = val.trim().to_string();
            if trimmed.is_empty() {
                issues.push("GITHUB_APP_CLIENT_SECRET is empty");
                None
            } else {
                Some(trimmed)
            }
        }
        Err(_) => {
            issues.push("GITHUB_APP_CLIENT_SECRET is not set");
            None
        }
    };

    // PEM: required, and it must be a usable RSA private key. A non-empty PEM
    // envelope is not enough: EC keys and structurally malformed RSA key data
    // cannot sign the RS256 JWT GitHub requires.
    let pem = match read_env_pem_detailed() {
        Some(p) => match super::jwt::validate_rsa_private_key(&p) {
            Ok(()) => Some(p),
            Err(_) => {
                issues.push("GITHUB_APP_PRIVATE_KEY is not a valid RSA private key");
                None
            }
        },
        None => {
            issues.push("GITHUB_APP_PRIVATE_KEY (or _PATH) is not set or empty");
            None
        }
    };

    // Optional fields (no issue if missing).
    let slug = std::env::var(super::ENV_APP_SLUG).unwrap_or_default();
    let webhook_secret = std::env::var(super::ENV_WEBHOOK_SECRET).unwrap_or_default();
    let public_url = std::env::var(super::ENV_PUBLIC_URL)
        .unwrap_or_else(|_| super::DEFAULT_PUBLIC_URL.to_string());

    if issues.is_empty() {
        // All required fields present and valid.
        EnvLoadResult::Valid(AppConfig {
            app_id: app_id.expect("checked"),
            slug,
            client_id: client_id.expect("checked"),
            client_secret: client_secret.expect("checked"),
            pem: pem.expect("checked"),
            webhook_secret,
            public_url,
        })
    } else {
        EnvLoadResult::Invalid(InvalidSecretDetail { issues })
    }
}

/// Read PEM from env, returning the content if available.
fn read_env_pem_detailed() -> Option<String> {
    use super::{ENV_PRIVATE_KEY, ENV_PRIVATE_KEY_PATH};

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

/// Detailed result of attempting to load persisted credentials.
enum PersistedLoadResult {
    /// Successfully decrypted and deserialized.
    Valid(AppConfig),
    /// Row exists but decryption/deserialization failed.
    Undecryptable,
    /// No persisted row found.
    Absent,
}

/// Try to load credentials from the encrypted persistence store.
async fn try_load_from_persisted(repo: &CredentialRepository) -> PersistedLoadResult {
    let raw = match repo.get_decrypted_for_user(CRED_KEY_NAME, None).await {
        Ok(Some(v)) => v,
        Ok(None) => return PersistedLoadResult::Absent,
        Err(_) => return PersistedLoadResult::Undecryptable,
    };

    match serde_json::from_str::<AppConfig>(&raw) {
        Ok(cfg) => PersistedLoadResult::Valid(cfg),
        Err(_) => PersistedLoadResult::Undecryptable,
    }
}

/// Resolve the credential source state by checking Secret/env first, then
/// the encrypted persistence store.
///
/// # Precedence rules
///
/// 1. If *any* core env var is set, the env source is the sole authority.
///    - All required vars valid → `ValidSecret`
///    - Missing/invalid required vars → `InvalidSecret` (fatal, no fallback)
/// 2. If no core env vars are set, check the persistence store.
///    - Decrypted and deserialised → `ValidPersisted`
///    - Row exists but decryption failed → `UndecryptablePersisted`
///    - No row → `Unconfigured`
///
/// # Arguments
///
/// * `credential_repo` — access to the encrypted credential store. Pass
///   `None` when the DB is not yet available (e.g., early boot); in that
///   case only env/Secret is checked.
pub async fn resolve_credential_source(
    credential_repo: Option<&CredentialRepository>,
) -> CredentialSourceState {
    // Step 1: Check env/Secret.
    match try_load_from_env_detailed() {
        EnvLoadResult::Valid(cfg) => CredentialSourceState::ValidSecret(Arc::new(cfg)),
        EnvLoadResult::Invalid(detail) => CredentialSourceState::InvalidSecret(detail),
        EnvLoadResult::Absent => {
            // Step 2: No env vars — check persisted.
            match credential_repo {
                Some(repo) => match try_load_from_persisted(repo).await {
                    PersistedLoadResult::Valid(cfg) => {
                        CredentialSourceState::ValidPersisted(Arc::new(cfg))
                    }
                    PersistedLoadResult::Undecryptable => {
                        CredentialSourceState::UndecryptablePersisted
                    }
                    PersistedLoadResult::Absent => CredentialSourceState::Unconfigured,
                },
                None => CredentialSourceState::Unconfigured,
            }
        }
    }
}

/// Persist an `AppConfig` into the encrypted credential store and return it
/// wrapped in an `Arc`.
///
/// This is the write-side companion to [`resolve_credential_source`]. After a
/// successful manifest exchange, the callback calls this to persist the new
/// credentials and then calls [`super::reload_app_config`] to hot-swap the
/// in-memory cache.
pub async fn persist_app_config(
    repo: &CredentialRepository,
    config: &AppConfig,
) -> Result<(), String> {
    let json = serde_json::to_string(config)
        .map_err(|e| format!("serialize AppConfig for persistence: {e}"))?;
    repo.set_with_owner(CRED_PROVIDER_ID, CRED_KEY_NAME, &json, None)
        .await
        .map_err(|e| format!("persist AppConfig: {e}"))?;
    Ok(())
}

/// Delete any persisted GitHub App config from the encrypted store.
///
/// Called when the operator wants to clear persisted credentials (e.g., before
/// re-running the setup flow).
pub async fn clear_persisted_app_config(repo: &CredentialRepository) -> Result<bool, String> {
    repo.delete_for_owner(CRED_KEY_NAME, None)
        .await
        .map_err(|e| format!("clear persisted AppConfig: {e}"))
}

#[cfg(test)]
mod tests {
    use super::super::jwt::tests::TEST_RSA_PRIVATE_KEY;
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::Database;

    // ── helpers ──────────────────────────────────────────────────────────

    const TEST_EC_PRIVATE_KEY: &str = "-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIIl4cWgLf9/UBxBiwqKKzrCPdOWOn8DdO8wn7FxCwZ5loAoGCCqGSM49
AwEHoUQDQgAEhLdpVmG8MBC2uexhGwHjWK0yoX9uLH5PAuTXMBdRSck+C5MGYcp0
kveFVBYy/01GtT5ymlBeq5yOk/P8wEI62A==
-----END EC PRIVATE KEY-----";

    /// Set env vars for a valid GitHub App configuration.
    /// Returns a guard that clears them on drop.
    struct EnvGuard {
        vars: Vec<(&'static str, String)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        /// Clear all GitHub App env vars to ensure a clean slate.
        /// Returns a guard that also clears on drop.
        fn clean() -> Self {
            let lock = super::super::config::github_app_test_lock();
            super::super::clear_runtime_config();
            let all_keys = [
                "GITHUB_APP_ID",
                "GITHUB_APP_SLUG",
                "GITHUB_APP_CLIENT_ID",
                "GITHUB_APP_CLIENT_SECRET",
                "GITHUB_APP_PRIVATE_KEY",
                "GITHUB_APP_PRIVATE_KEY_PATH",
                "GITHUB_APP_WEBHOOK_SECRET",
                "DJINN_PUBLIC_URL",
                "DJINN_ENABLE_SELF_SETUP",
            ];
            for k in &all_keys {
                unsafe { std::env::remove_var(k) };
            }
            Self {
                vars: Vec::new(),
                _lock: lock,
            }
        }

        fn set_valid() -> Self {
            let lock = super::super::config::github_app_test_lock();
            super::super::clear_runtime_config();
            let vars = vec![
                ("GITHUB_APP_ID", "12345".to_string()),
                ("GITHUB_APP_SLUG", "djinn-test".to_string()),
                ("GITHUB_APP_CLIENT_ID", "Iv1.abc".to_string()),
                ("GITHUB_APP_CLIENT_SECRET", "shh".to_string()),
                ("GITHUB_APP_PRIVATE_KEY", TEST_RSA_PRIVATE_KEY.to_string()),
                ("GITHUB_APP_WEBHOOK_SECRET", "wsec".to_string()),
                ("DJINN_PUBLIC_URL", "https://djinn.example.com".to_string()),
            ];
            for &(k, ref v) in &vars {
                // SAFETY: test env mutation — EnvGuard::drop cleans up.
                unsafe { std::env::set_var(k, v) };
            }
            Self { vars, _lock: lock }
        }

        fn set_incomplete() -> Self {
            let lock = super::super::config::github_app_test_lock();
            super::super::clear_runtime_config();
            // Only set APP_ID but not CLIENT_ID/SECRET/PEM.
            let vars = vec![("GITHUB_APP_ID", "12345".to_string())];
            for &(k, ref v) in &vars {
                unsafe { std::env::set_var(k, v) };
            }
            Self { vars, _lock: lock }
        }

        fn set_malformed_app_id() -> Self {
            let lock = super::super::config::github_app_test_lock();
            super::super::clear_runtime_config();
            let vars = vec![
                ("GITHUB_APP_ID", "not-a-number".to_string()),
                ("GITHUB_APP_CLIENT_ID", "Iv1.abc".to_string()),
                ("GITHUB_APP_CLIENT_SECRET", "shh".to_string()),
                ("GITHUB_APP_PRIVATE_KEY", TEST_RSA_PRIVATE_KEY.to_string()),
            ];
            for &(k, ref v) in &vars {
                unsafe { std::env::set_var(k, v) };
            }
            Self { vars, _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for &(k, _) in &self.vars {
                unsafe { std::env::remove_var(k) };
            }
            // Also clean up optional vars that might linger.
            for k in [
                "GITHUB_APP_ID",
                "GITHUB_APP_SLUG",
                "GITHUB_APP_CLIENT_ID",
                "GITHUB_APP_CLIENT_SECRET",
                "GITHUB_APP_PRIVATE_KEY",
                "GITHUB_APP_PRIVATE_KEY_PATH",
                "GITHUB_APP_WEBHOOK_SECRET",
                "DJINN_PUBLIC_URL",
                "DJINN_ENABLE_SELF_SETUP",
            ] {
                unsafe { std::env::remove_var(k) };
            }
        }
    }

    fn fixture_config() -> AppConfig {
        AppConfig {
            app_id: 99999,
            slug: "djinn-fixture".into(),
            client_id: "Iv1.fixture".into(),
            client_secret: "fixture-secret".into(),
            pem: TEST_RSA_PRIVATE_KEY.into(),
            webhook_secret: "fixture-wh".into(),
            public_url: "https://fixture.example.com".into(),
        }
    }

    fn cred_repo() -> CredentialRepository {
        let db = Database::open_in_memory().expect("failed to create test database");
        CredentialRepository::new(db, EventBus::noop())
    }

    fn set_complete_env(app_id: &str, private_key: &str) {
        // SAFETY: callers hold the module-wide env lock through `EnvGuard`.
        unsafe {
            std::env::set_var("GITHUB_APP_ID", app_id);
            std::env::set_var("GITHUB_APP_CLIENT_ID", "Iv1.abc");
            std::env::set_var("GITHUB_APP_CLIENT_SECRET", "shh");
            std::env::set_var("GITHUB_APP_PRIVATE_KEY", private_key);
        }
    }

    // ── AC 1: typed state replaces Option-only ambiguity ─────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_secret_produces_valid_secret_state() {
        let _guard = EnvGuard::set_valid();
        let state = resolve_credential_source(None).await;
        assert!(matches!(state, CredentialSourceState::ValidSecret(_)));
        assert!(state.is_usable());
        assert_eq!(state.source(), Some(ConfigSource::Secret));
        let cfg = state.app_config().unwrap();
        assert_eq!(cfg.app_id, 12345);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_env_no_persisted_produces_unconfigured() {
        // Ensure env is clean.
        let _guard = EnvGuard::clean();
        let state = resolve_credential_source(None).await;
        assert!(matches!(state, CredentialSourceState::Unconfigured));
        assert!(!state.is_usable());
        assert!(state.source().is_none());
        assert!(state.app_config().is_none());
    }

    // ── AC 2: Secret takes precedence over persisted ─────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_secret_overrides_persisted() {
        let repo = cred_repo();
        let persisted = fixture_config();
        persist_app_config(&repo, &persisted).await.unwrap();

        let _guard = EnvGuard::set_valid();
        let state = resolve_credential_source(Some(&repo)).await;
        assert!(matches!(state, CredentialSourceState::ValidSecret(_)));
        let cfg = state.app_config().unwrap();
        // Should be the env config, not the persisted one.
        assert_eq!(cfg.app_id, 12345);
        assert_ne!(cfg.app_id, 99999);
    }

    // ── AC 2b: invalid Secret is fatal, no fallback to persisted ─────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_secret_does_not_fall_through_to_persisted() {
        let repo = cred_repo();
        let persisted = fixture_config();
        persist_app_config(&repo, &persisted).await.unwrap();

        let _guard = EnvGuard::set_incomplete();
        let state = resolve_credential_source(Some(&repo)).await;
        assert!(
            matches!(state, CredentialSourceState::InvalidSecret(_)),
            "incomplete Secret must be fatal, not fall through to persisted; got: {state:?}"
        );
        assert!(!state.is_usable());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_app_id_is_fatal() {
        let repo = cred_repo();
        let persisted = fixture_config();
        persist_app_config(&repo, &persisted).await.unwrap();

        let _guard = EnvGuard::set_malformed_app_id();
        let state = resolve_credential_source(Some(&repo)).await;
        assert!(matches!(state, CredentialSourceState::InvalidSecret(_)));
        if let CredentialSourceState::InvalidSecret(detail) = &state {
            assert!(detail.issues.iter().any(|i| i.contains("GITHUB_APP_ID")));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_app_id_is_fatal_over_persisted() {
        let repo = cred_repo();
        persist_app_config(&repo, &fixture_config()).await.unwrap();

        let _env = EnvGuard::clean();
        set_complete_env("0", TEST_RSA_PRIVATE_KEY);

        let state = resolve_credential_source(Some(&repo)).await;
        let CredentialSourceState::InvalidSecret(detail) = state else {
            panic!("zero App ID must be InvalidSecret, not usable or persisted fallback");
        };
        assert!(
            detail
                .issues
                .contains(&"GITHUB_APP_ID must be greater than zero")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_private_key_is_fatal_over_persisted() {
        let repo = cred_repo();
        persist_app_config(&repo, &fixture_config()).await.unwrap();

        let _env = EnvGuard::clean();
        set_complete_env("12345", "not-a-private-key");

        let state = resolve_credential_source(Some(&repo)).await;
        let CredentialSourceState::InvalidSecret(detail) = state else {
            panic!("malformed private key must be InvalidSecret, not ValidSecret");
        };
        assert!(
            detail
                .issues
                .contains(&"GITHUB_APP_PRIVATE_KEY is not a valid RSA private key")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_rsa_private_key_is_fatal_over_persisted() {
        let repo = cred_repo();
        persist_app_config(&repo, &fixture_config()).await.unwrap();

        let _env = EnvGuard::clean();
        set_complete_env("12345", TEST_EC_PRIVATE_KEY);

        let state = resolve_credential_source(Some(&repo)).await;
        let CredentialSourceState::InvalidSecret(detail) = state else {
            panic!("EC private key must be InvalidSecret, not ValidSecret");
        };
        assert!(
            detail
                .issues
                .contains(&"GITHUB_APP_PRIVATE_KEY is not a valid RSA private key")
        );
    }

    /// Regression for the credential-source precedence gap: when an operator
    /// sets a required env var to an *empty* value (e.g. `GITHUB_APP_ID=""`)
    /// while persisted credentials exist, the env source must be treated as
    /// attempted-but-invalid and produce the fatal `InvalidSecret` state —
    /// it must NOT silently fall through to persisted credentials.
    ///
    /// AC1/AC2 require that the typed state distinguishes absent env from
    /// present-but-empty env, so this regression is required.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_env_var_is_fatal_over_persisted() {
        let repo = cred_repo();
        let persisted = fixture_config();
        persist_app_config(&repo, &persisted).await.unwrap();

        // Set ONLY GITHUB_APP_ID to an empty string. All other required vars
        // are unset. Previously this would have been treated as Absent and
        // silently fallen through to persisted credentials.
        let _env = EnvGuard::clean();
        unsafe { std::env::set_var("GITHUB_APP_ID", "") };

        let state = resolve_credential_source(Some(&repo)).await;
        assert!(
            matches!(state, CredentialSourceState::InvalidSecret(_)),
            "empty GITHUB_APP_ID with persisted creds must be fatal; got: {state:?}"
        );
        assert!(!state.is_usable());
        assert!(state.source().is_none());

        if let CredentialSourceState::InvalidSecret(detail) = &state {
            assert!(
                detail.issues.iter().any(|i| i.contains("GITHUB_APP_ID")),
                "InvalidSecret detail should mention GITHUB_APP_ID; got: {:?}",
                detail.issues
            );
        }
    }

    /// Variant of the regression: every other required var is set, but one is
    /// empty. Still must be fatal — and the issue must name the empty var.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_client_id_alongside_other_env_vars_is_fatal() {
        let repo = cred_repo();
        let persisted = fixture_config();
        persist_app_config(&repo, &persisted).await.unwrap();

        let _env = EnvGuard::clean();
        // Set everything but make GITHUB_APP_CLIENT_ID empty.
        unsafe { std::env::set_var("GITHUB_APP_ID", "12345") };
        unsafe { std::env::set_var("GITHUB_APP_CLIENT_ID", "") };
        unsafe { std::env::set_var("GITHUB_APP_CLIENT_SECRET", "shh") };
        unsafe { std::env::set_var("GITHUB_APP_PRIVATE_KEY", TEST_RSA_PRIVATE_KEY) };

        let state = resolve_credential_source(Some(&repo)).await;
        assert!(
            matches!(state, CredentialSourceState::InvalidSecret(_)),
            "empty GITHUB_APP_CLIENT_ID with persisted creds must be fatal; got: {state:?}"
        );
        assert!(!state.is_usable());
        if let CredentialSourceState::InvalidSecret(detail) = &state {
            assert!(
                detail
                    .issues
                    .iter()
                    .any(|i| i.contains("GITHUB_APP_CLIENT_ID"))
            );
        }
    }

    /// Variant: PEM is set to an empty string. Must be fatal, not persisted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_private_key_is_fatal_over_persisted() {
        let repo = cred_repo();
        let persisted = fixture_config();
        persist_app_config(&repo, &persisted).await.unwrap();

        let _env = EnvGuard::clean();
        unsafe { std::env::set_var("GITHUB_APP_ID", "12345") };
        unsafe { std::env::set_var("GITHUB_APP_CLIENT_ID", "Iv1.abc") };
        unsafe { std::env::set_var("GITHUB_APP_CLIENT_SECRET", "shh") };
        unsafe { std::env::set_var("GITHUB_APP_PRIVATE_KEY", "") };

        let state = resolve_credential_source(Some(&repo)).await;
        assert!(
            matches!(state, CredentialSourceState::InvalidSecret(_)),
            "empty GITHUB_APP_PRIVATE_KEY with persisted creds must be fatal; got: {state:?}"
        );
        assert!(!state.is_usable());
    }

    /// Distinguishing case for AC1: a *truly* absent env (unset, not empty)
    /// with persisted credentials must produce `ValidPersisted`, NOT
    /// `InvalidSecret`. This documents the absence/empty distinction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn truly_absent_env_falls_through_to_persisted() {
        let repo = cred_repo();
        let persisted = fixture_config();
        persist_app_config(&repo, &persisted).await.unwrap();

        let _env = EnvGuard::clean();
        // Sanity: no GitHub App env vars are present.
        for k in [
            "GITHUB_APP_ID",
            "GITHUB_APP_CLIENT_ID",
            "GITHUB_APP_CLIENT_SECRET",
            "GITHUB_APP_PRIVATE_KEY",
            "GITHUB_APP_PRIVATE_KEY_PATH",
            "GITHUB_APP_SLUG",
        ] {
            assert!(std::env::var_os(k).is_none(), "env var {k} should be unset");
        }

        let state = resolve_credential_source(Some(&repo)).await;
        assert!(
            matches!(state, CredentialSourceState::ValidPersisted(_)),
            "truly absent env with persisted creds must produce ValidPersisted; got: {state:?}"
        );
    }

    // ── AC 3: valid persisted credentials ────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_persisted_credentials_load() {
        // Clean env.
        let _guard = EnvGuard::clean();
        let repo = cred_repo();
        let config = fixture_config();
        persist_app_config(&repo, &config).await.unwrap();

        let state = resolve_credential_source(Some(&repo)).await;
        assert!(matches!(state, CredentialSourceState::ValidPersisted(_)));
        assert!(state.is_usable());
        assert_eq!(state.source(), Some(ConfigSource::Persisted));
        let cfg = state.app_config().unwrap();
        assert_eq!(cfg.app_id, 99999);
        assert_eq!(cfg.slug, "djinn-fixture");
    }

    // ── AC 3b: persisted credentials with self_setup both on and off ─────
    // The credential resolution itself does not gate on the flag — the flag
    // only controls whether setup *routes* are advertised. We verify that
    // persisted credentials resolve to `ValidPersisted` regardless of the
    // `DJINN_ENABLE_SELF_SETUP` flag's value (so an operator who disables
    // the flag after a successful setup still has working credentials).

    /// Toggle `DJINN_ENABLE_SELF_SETUP` to a concrete value.
    struct SelfSetupFlagGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl SelfSetupFlagGuard {
        fn set(value: &str) -> Self {
            let key = "DJINN_ENABLE_SELF_SETUP";
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for SelfSetupFlagGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persisted_credentials_work_with_self_setup_flag_on() {
        let _env = EnvGuard::clean();
        let _flag = SelfSetupFlagGuard::set("true");
        let repo = cred_repo();
        persist_app_config(&repo, &fixture_config()).await.unwrap();

        let state = resolve_credential_source(Some(&repo)).await;
        assert!(
            matches!(state, CredentialSourceState::ValidPersisted(_)),
            "self_setup=on with persisted creds must produce ValidPersisted; got: {state:?}"
        );
        assert_eq!(state.app_config().unwrap().app_id, 99999);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persisted_credentials_work_with_self_setup_flag_off() {
        let _env = EnvGuard::clean();
        let _flag = SelfSetupFlagGuard::set("false");
        let repo = cred_repo();
        persist_app_config(&repo, &fixture_config()).await.unwrap();

        let state = resolve_credential_source(Some(&repo)).await;
        assert!(
            matches!(state, CredentialSourceState::ValidPersisted(_)),
            "self_setup=off with persisted creds must produce ValidPersisted; got: {state:?}"
        );
        assert_eq!(state.app_config().unwrap().app_id, 99999);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persisted_credentials_work_with_self_setup_flag_unset() {
        // Same as "off" but with the var removed entirely. Operators who never
        // set the flag should still have working persisted credentials.
        let _env = EnvGuard::clean();
        unsafe { std::env::remove_var("DJINN_ENABLE_SELF_SETUP") };
        let repo = cred_repo();
        persist_app_config(&repo, &fixture_config()).await.unwrap();

        let state = resolve_credential_source(Some(&repo)).await;
        assert!(
            matches!(state, CredentialSourceState::ValidPersisted(_)),
            "self_setup=unset with persisted creds must produce ValidPersisted; got: {state:?}"
        );
        assert_eq!(state.app_config().unwrap().app_id, 99999);
    }

    // ── AC 4: Secret removal reveals persisted credentials ───────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removing_secret_reveals_persisted() {
        let repo = cred_repo();
        let persisted = fixture_config();
        persist_app_config(&repo, &persisted).await.unwrap();

        // With Secret present → ValidSecret.
        {
            let _guard = EnvGuard::set_valid();
            let state = resolve_credential_source(Some(&repo)).await;
            assert!(matches!(state, CredentialSourceState::ValidSecret(_)));
        }
        // After env vars are dropped (guard dropped) → ValidPersisted.
        let state = resolve_credential_source(Some(&repo)).await;
        assert!(matches!(state, CredentialSourceState::ValidPersisted(_)));
        assert_eq!(state.app_config().unwrap().app_id, 99999);
    }

    // ── AC 5: undecryptable persisted credentials ────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn undecryptable_persisted_credentials() {
        let _guard = EnvGuard::clean();

        // Manually insert a row with garbage encrypted data.
        let db = Database::open_in_memory().expect("test db");
        let garbage_repo = CredentialRepository::new(db, EventBus::noop());
        // Store garbage as raw bytes directly (bypass encryption).
        garbage_repo
            .set_with_owner(
                CRED_PROVIDER_ID,
                CRED_KEY_NAME,
                "this-is-not-valid-json-after-decryption",
                None,
            )
            .await
            .unwrap();

        // The value *can* be decrypted (it's valid encrypted data), but the
        // JSON deserialization will fail → UndecryptablePersisted.
        let state = resolve_credential_source(Some(&garbage_repo)).await;
        assert!(
            matches!(state, CredentialSourceState::UndecryptablePersisted),
            "non-JSON decrypted value should produce UndecryptablePersisted; got: {state:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn corrupt_encrypted_blob_produces_undecryptable() {
        let _guard = EnvGuard::clean();

        // Create a fresh repo and manually write a corrupt blob.
        let db = Database::open_in_memory().expect("test db");
        db.ensure_initialized().await.unwrap();
        let repo = CredentialRepository::new(db.clone(), EventBus::noop());

        // First store something valid to create the row.
        let valid_json = serde_json::to_string(&fixture_config()).unwrap();
        repo.set_with_owner(CRED_PROVIDER_ID, CRED_KEY_NAME, &valid_json, None)
            .await
            .unwrap();

        // Now corrupt the encrypted value using the djinn-db test-support
        // helper (raw SQL lives inside the djinn-db crate boundary).
        djinn_db::test_support::corrupt_credential_encrypted_value(
            &db,
            CRED_KEY_NAME,
            vec![0u8, 1, 2, 3, 4, 5], // too short / invalid
        )
        .await;

        let state = resolve_credential_source(Some(&repo)).await;
        assert!(
            matches!(state, CredentialSourceState::UndecryptablePersisted),
            "corrupt encrypted blob should produce UndecryptablePersisted; got: {state:?}"
        );
    }

    // ── Persist / clear round-trip ───────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persist_and_clear_round_trip() {
        let _env = EnvGuard::clean();
        let repo = cred_repo();
        let config = fixture_config();

        // Initially absent.
        let state = resolve_credential_source(Some(&repo)).await;
        assert!(matches!(state, CredentialSourceState::Unconfigured));

        // Persist.
        persist_app_config(&repo, &config).await.unwrap();
        let state = resolve_credential_source(Some(&repo)).await;
        assert!(state.is_usable());

        // Clear.
        let deleted = clear_persisted_app_config(&repo).await.unwrap();
        assert!(deleted);
        let state = resolve_credential_source(Some(&repo)).await;
        assert!(matches!(state, CredentialSourceState::Unconfigured));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persist_upserts_on_repeat() {
        let repo = cred_repo();
        let mut config = fixture_config();
        persist_app_config(&repo, &config).await.unwrap();

        // Change and re-persist.
        config.app_id = 77777;
        persist_app_config(&repo, &config).await.unwrap();

        let _guard = EnvGuard::clean();
        let state = resolve_credential_source(Some(&repo)).await;
        assert_eq!(state.app_config().unwrap().app_id, 77777);
    }

    // ── Hot-reload: persist then reload without restart ───────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hot_reload_after_persist() {
        use tokio::sync::RwLock;

        let repo = cred_repo();
        let cache: RwLock<Option<Arc<AppConfig>>> = RwLock::new(None);

        // Initial: no config.
        assert!(cache.read().await.is_none());

        // Manifest callback persists new config.
        let config = fixture_config();
        persist_app_config(&repo, &config).await.unwrap();

        // Hot-reload: re-resolve and update cache.
        let _guard = EnvGuard::clean();
        let state = resolve_credential_source(Some(&repo)).await;
        let new_cfg = state.app_config().cloned();
        *cache.write().await = new_cfg;

        assert!(cache.read().await.is_some());
        assert_eq!(cache.read().await.as_ref().unwrap().app_id, 99999);
    }
}
