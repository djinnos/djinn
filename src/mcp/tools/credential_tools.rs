use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::db::repositories::credential::CredentialRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::AnyJson;
use crate::mcp::tools::provider_tools::is_goose_builtin_provider;
use crate::models::provider::{Model, Provider};

fn build_goose_custom_provider_json(
    provider: &Provider,
    models: &[Model],
    api_key_env: &str,
) -> serde_json::Value {
    let model_entries: Vec<serde_json::Value> = models
        .iter()
        .map(|m| {
            serde_json::json!({
                "name": m.id,
                "context_limit": m.context_window.max(1),
            })
        })
        .collect();

    serde_json::json!({
        "name": provider.id,
        "engine": "openai",
        "display_name": provider.name,
        "description": format!("Auto-registered from models.dev catalog ({})", provider.id),
        "api_key_env": api_key_env,
        "base_url": provider.base_url,
        "models": model_entries,
        "supports_streaming": true,
        "requires_auth": true,
        "catalog_provider_id": provider.id,
    })
}

async fn maybe_auto_register_goose_custom_provider(
    server: &DjinnMcpServer,
    provider_id: &str,
    fallback_key_name: &str,
) -> Result<(), String> {
    if is_goose_builtin_provider(provider_id).await {
        return Ok(());
    }
    if provider_id.contains('/') {
        return Ok(());
    }

    let provider = server
        .state
        .catalog()
        .list_providers()
        .into_iter()
        .find(|p| p.id == provider_id);
    let Some(provider) = provider else {
        return Ok(());
    };
    if !provider.is_openai_compatible {
        return Ok(());
    }

    let models = server.state.catalog().list_models(provider_id);
    let api_key_env = provider
        .env_vars
        .first()
        .cloned()
        .unwrap_or_else(|| fallback_key_name.to_string());

    let custom_dir = goose::config::declarative_providers::custom_providers_dir();
    fs::create_dir_all(&custom_dir)
        .await
        .map_err(|e| format!("create {}: {e}", custom_dir.display()))?;

    let payload = build_goose_custom_provider_json(&provider, &models, &api_key_env);
    let json = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
    let file_path = custom_dir.join(format!("{provider_id}.json"));
    fs::write(&file_path, json)
        .await
        .map_err(|e| format!("write {}: {e}", file_path.display()))?;

    goose::providers::refresh_custom_providers()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ── credential_set ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct CredentialSetInput {
    /// Provider ID this key belongs to (e.g. 'anthropic', 'openai').
    pub provider_id: String,
    /// Env-var style key name (e.g. 'ANTHROPIC_API_KEY').
    pub key_name: String,
    /// The raw API key value to store encrypted.
    pub api_key: String,
}

#[derive(Serialize, JsonSchema)]
pub struct CredentialSetResponse {
    pub success: bool,
    pub id: String,
    pub key_name: String,
    pub error: Option<String>,
}

// ── credential_list ───────────────────────────────────────────────────────────

#[derive(Serialize, JsonSchema)]
pub struct CredentialListResponse {
    pub credentials: Vec<AnyJson>,
}

// ── credential_delete ─────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct CredentialDeleteInput {
    /// Env-var style key name of the credential to delete (e.g. 'ANTHROPIC_API_KEY').
    pub key_name: String,
}

#[derive(Serialize, JsonSchema)]
pub struct CredentialDeleteResponse {
    pub success: bool,
    pub deleted: bool,
    pub key_name: String,
    pub error: Option<String>,
}

// ── Tool router ───────────────────────────────────────────────────────────────

#[tool_router(router = credential_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Store or update an encrypted API key in the credential vault.
    /// Use this to save provider API keys for agent dispatch.
    #[tool(
        description = "Store or update an encrypted API key in the credential vault. Use this to save provider API keys for agent dispatch."
    )]
    pub async fn credential_set(
        &self,
        Parameters(input): Parameters<CredentialSetInput>,
    ) -> Json<CredentialSetResponse> {
        if input.key_name.is_empty() {
            return Json(CredentialSetResponse {
                success: false,
                id: String::new(),
                key_name: input.key_name,
                error: Some("key_name must not be empty".into()),
            });
        }
        if input.api_key.is_empty() {
            return Json(CredentialSetResponse {
                success: false,
                id: String::new(),
                key_name: input.key_name,
                error: Some("api_key must not be empty".into()),
            });
        }

        let repo = CredentialRepository::new(self.state.db().clone(), self.state.events().clone());

        match repo
            .set(&input.provider_id, &input.key_name, &input.api_key)
            .await
        {
            Ok(cred) => {
                if let Err(e) =
                    maybe_auto_register_goose_custom_provider(self, &input.provider_id, &input.key_name)
                        .await
                {
                    tracing::warn!(
                        provider_id = %input.provider_id,
                        error = %e,
                        "credential_set: auto-register Goose custom provider failed"
                    );
                }

                Json(CredentialSetResponse {
                    success: true,
                    id: cred.id,
                    key_name: cred.key_name,
                    error: None,
                })
            }
            Err(e) => {
                tracing::warn!(key_name = %input.key_name, error = %e, "credential_set failed");
                Json(CredentialSetResponse {
                    success: false,
                    id: String::new(),
                    key_name: input.key_name,
                    error: Some(e.to_string()),
                })
            }
        }
    }

    /// List all stored credentials. Never returns raw key values.
    #[tool(
        description = "List all stored credentials. Never returns raw key values — only metadata (id, provider_id, key_name, timestamps)."
    )]
    pub async fn credential_list(&self) -> Json<CredentialListResponse> {
        let repo = CredentialRepository::new(self.state.db().clone(), self.state.events().clone());

        let credentials = match repo.list().await {
            Ok(creds) => creds
                .iter()
                .map(|c| {
                    AnyJson::from(serde_json::json!({
                        "id":          c.id,
                        "provider_id": c.provider_id,
                        "key_name":    c.key_name,
                        "created_at":  c.created_at,
                        "updated_at":  c.updated_at,
                    }))
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "credential_list failed");
                vec![]
            }
        };

        Json(CredentialListResponse { credentials })
    }

    /// Delete a stored credential by key_name.
    #[tool(
        description = "Delete a stored credential by key_name (e.g. 'ANTHROPIC_API_KEY'). Returns deleted=true if the credential existed."
    )]
    pub async fn credential_delete(
        &self,
        Parameters(input): Parameters<CredentialDeleteInput>,
    ) -> Json<CredentialDeleteResponse> {
        let repo = CredentialRepository::new(self.state.db().clone(), self.state.events().clone());

        match repo.delete(&input.key_name).await {
            Ok(deleted) => Json(CredentialDeleteResponse {
                success: true,
                deleted,
                key_name: input.key_name,
                error: None,
            }),
            Err(e) => {
                tracing::warn!(key_name = %input.key_name, error = %e, "credential_delete failed");
                Json(CredentialDeleteResponse {
                    success: false,
                    deleted: false,
                    key_name: input.key_name,
                    error: Some(e.to_string()),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goose_custom_provider_json_has_required_fields() {
        let provider = Provider {
            id: "synthetic".to_string(),
            name: "Synthetic".to_string(),
            npm: "@ai-sdk/openai-compatible".to_string(),
            env_vars: vec!["SYNTHETIC_API_KEY".to_string()],
            base_url: "https://api.synthetic.ai/v1".to_string(),
            docs_url: String::new(),
            is_openai_compatible: true,
        };
        let models = vec![Model {
            id: "synth-1".to_string(),
            provider_id: "synthetic".to_string(),
            name: "Synth 1".to_string(),
            tool_call: true,
            reasoning: false,
            attachment: false,
            context_window: 8192,
            output_limit: 4096,
            pricing: crate::models::provider::Pricing::default(),
        }];

        let json = build_goose_custom_provider_json(&provider, &models, "SYNTHETIC_API_KEY");

        assert_eq!(json["name"], "synthetic");
        assert_eq!(json["engine"], "openai");
        assert_eq!(json["api_key_env"], "SYNTHETIC_API_KEY");
        assert_eq!(json["base_url"], "https://api.synthetic.ai/v1");
        assert_eq!(json["models"][0]["name"], "synth-1");
    }
}
