// Re-export the crate-level builtin registry from djinn-provider.
pub use djinn_provider::catalog::builtin::{
    all_oauth_keys_for_provider, builtin_provider_ids, find_builtin_provider, is_builtin_provider,
    merged_oauth_keys_for, merged_provider_ids, oauth_keys_for_provider, resolve_builtin_name,
    resolve_oauth_provider, BuiltinProvider, BUILTIN_PROVIDERS,
};

// ── Server-specific functions ─────────────────────────────────────────────────
//
// These depend on agent OAuth types and the credential DB, so they live here
// rather than in the crate.

/// Remove OAuth tokens from the DB (and any lingering filesystem cache).
pub async fn clear_oauth_tokens(oauth_keys: &[String], repo: &crate::db::CredentialRepository) {
    use crate::agent::oauth::{codex::CodexTokens, copilot::CopilotTokens};

    for key in oauth_keys {
        match key.as_str() {
            "CHATGPT_CODEX_TOKEN" => CodexTokens::clear_from_db(repo).await,
            "GITHUB_COPILOT_TOKEN" => CopilotTokens::clear_from_db(repo).await,
            _ => {}
        }
    }
}

/// Check whether any of the given OAuth keys have a stored token.
///
/// Checks the vault credential key names (already queried by callers) for the
/// well-known OAuth credential DB keys.  Falls back to the filesystem cache
/// for backward compatibility during migration.
pub fn is_oauth_key_present(
    oauth_keys: &[String],
    credential_key_names: &std::collections::HashSet<String>,
) -> bool {
    use crate::agent::oauth::{
        codex::{CODEX_OAUTH_DB_KEY, CodexTokens},
        copilot::{COPILOT_OAUTH_DB_KEY, CopilotTokens},
    };

    oauth_keys.iter().any(|key| match key.as_str() {
        "CHATGPT_CODEX_TOKEN" => {
            credential_key_names.contains(CODEX_OAUTH_DB_KEY)
                || CodexTokens::load_cached().is_some()
        }
        "GITHUB_COPILOT_TOKEN" => {
            credential_key_names.contains(COPILOT_OAUTH_DB_KEY)
                || CopilotTokens::load_cached().is_some()
        }
        _ => false,
    })
}
