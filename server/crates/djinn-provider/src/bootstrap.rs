//! Startup bootstrap — read provider credentials from the process environment
//! into the encrypted vault.
//!
//! Deployments (Helm values, docker-compose env, systemd EnvironmentFile, …)
//! provision API keys via the well-known env vars declared on each
//! [`BuiltinProvider`] (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.). At every
//! server start we walk the built-in provider list, read those vars, and upsert
//! them into [`CredentialRepository`] so the dispatch layer's vault lookup
//! resolves the same credentials that appear under Helm.
//!
//! The write is idempotent — a Helm upgrade that changes a key simply
//! overwrites the stored value on the next boot. A deployment that *unsets*
//! an env var does NOT clear the vault entry (removing credentials is an
//! explicit action via the `credential_delete` MCP tool). This matches the
//! GitHub App secret pattern and avoids the footgun of accidentally wiping
//! vault entries when an operator edits unrelated `extraEnv` values.
//!
//! Credentials typed directly into the UI (e.g. Codex OAuth tokens) live in
//! the same vault and are untouched by this pass — only the declared
//! `required_env_vars` are considered.

use anyhow::{Context, Result};

use crate::catalog::builtin::BUILTIN_PROVIDERS;
use crate::repos::CredentialRepository;

/// Walk [`BUILTIN_PROVIDERS`], read each provider's `required_env_vars` from the
/// process environment, and upsert non-empty values into the credential vault.
/// Safe to call multiple times — each call is a straight upsert.
pub async fn bootstrap_env_credentials(repo: &CredentialRepository) -> Result<()> {
    for provider in BUILTIN_PROVIDERS {
        for env_name in provider.required_env_vars {
            let Ok(value) = std::env::var(env_name) else {
                continue;
            };
            let trimmed = value.trim();
            if trimmed.is_empty() {
                continue;
            }
            repo.set(provider.id, env_name, trimmed).await.with_context(|| {
                format!(
                    "failed to bootstrap credential for provider={} key={}",
                    provider.id, env_name
                )
            })?;
            tracing::info!(
                provider = provider.id,
                key = env_name,
                "bootstrapped provider credential from env"
            );
        }
    }
    Ok(())
}
