use super::*;

impl AgentSupervisor {
    pub(super) fn canonical_provider_id(id: &str) -> String {
        id.chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect()
    }

    pub(super) fn resolve_provider_alias(
        provider_id: &str,
        entries: &[(ProviderMetadata, goose::providers::base::ProviderType)],
    ) -> Option<String> {
        if let Some((meta, _)) = entries.iter().find(|(meta, _)| meta.name == provider_id) {
            return Some(meta.name.clone());
        }

        let canonical = Self::canonical_provider_id(provider_id);
        entries
            .iter()
            .find(|(meta, _)| Self::canonical_provider_id(&meta.name) == canonical)
            .map(|(meta, _)| meta.name.clone())
    }

    pub(super) fn provider_secret_key(&self, provider_id: &str) -> Option<String> {
        self.app_state
            .catalog()
            .list_providers()
            .into_iter()
            .find(|p| p.id == provider_id)
            .and_then(|p| p.env_vars.into_iter().next())
    }

    pub(super) async fn load_provider_api_key(
        &self,
        provider_id: &str,
    ) -> Result<(String, String), SupervisorError> {
        let key_name = self
            .provider_secret_key(provider_id)
            .unwrap_or_else(|| format!("{}_API_KEY", provider_id.to_ascii_uppercase()));

        let repo =
            CredentialRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let key = repo
            .get_decrypted(&key_name)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        match key {
            Some(v) => Ok((key_name, v)),
            None => Err(SupervisorError::MissingCredential {
                provider_id: provider_id.to_owned(),
                key_name,
            }),
        }
    }

    pub(super) async fn goose_provider_entries(
        &self,
    ) -> Vec<(ProviderMetadata, goose::providers::base::ProviderType)> {
        providers::providers().await
    }

    pub(super) async fn resolve_goose_provider_id(&self, provider_id: &str) -> String {
        let entries = self.goose_provider_entries().await;
        Self::resolve_provider_alias(provider_id, &entries)
            .unwrap_or_else(|| provider_id.to_string())
    }

    pub(super) async fn provider_supports_oauth(&self, provider_id: &str) -> Option<bool> {
        let entries = self.goose_provider_entries().await;
        let resolved = Self::resolve_provider_alias(provider_id, &entries)?;
        entries
            .iter()
            .find(|(meta, _)| meta.name == resolved)
            .map(|(meta, _)| meta.config_keys.iter().any(|k| k.oauth_flow))
    }
}
