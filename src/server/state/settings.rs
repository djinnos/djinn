use std::collections::HashSet;

use crate::db::repositories::credential::CredentialRepository;
use crate::db::repositories::settings::SettingsRepository;
use crate::models::settings::DjinnSettings;

use super::{AppState, SETTINGS_RAW_KEY};

impl AppState {
    pub async fn apply_settings(&self, settings: &DjinnSettings) -> Result<(), String> {
        self.validate_model_priority_providers_connected(settings)
            .await?;
        let raw =
            serde_json::to_string(settings).map_err(|e| format!("serialize settings: {e}"))?;
        let repo = SettingsRepository::new(self.db().clone(), self.events().clone());
        repo.set(SETTINGS_RAW_KEY, &raw)
            .await
            .map_err(|e| e.to_string())?;
        self.apply_runtime_settings(settings).await;
        Ok(())
    }

    async fn validate_model_priority_providers_connected(
        &self,
        settings: &DjinnSettings,
    ) -> Result<(), String> {
        let priorities = settings.model_priority_or_default();
        if priorities.is_empty() {
            return Ok(());
        }

        let configured_provider_ids: HashSet<String> = priorities
            .values()
            .flat_map(|models| models.iter())
            .map(|model| {
                model
                    .split_once('/')
                    .map(|(provider_id, _)| provider_id)
                    .unwrap_or(model.as_str())
                    .to_string()
            })
            .collect();
        if configured_provider_ids.is_empty() {
            return Ok(());
        }

        let repo = CredentialRepository::new(self.db().clone(), self.events().clone());
        let credentials = repo
            .list()
            .await
            .map_err(|e| format!("list credentials: {e}"))?;
        let mut connected_provider_ids: HashSet<String> =
            credentials.into_iter().map(|c| c.provider_id).collect();

        // Also consider OAuth-connected providers (e.g. chatgpt_codex, github_copilot).
        let goose_entries = crate::mcp::tools::provider_tools::goose_provider_entries().await;
        let catalog_providers = self.catalog().list_providers();
        for provider in &catalog_providers {
            let oauth_keys = crate::mcp::tools::provider_tools::oauth_keys_for_provider(
                &provider.id,
                &goose_entries,
            );
            if !oauth_keys.is_empty()
                && crate::mcp::tools::provider_tools::is_oauth_key_present(&oauth_keys)
            {
                connected_provider_ids.insert(provider.id.clone());
            }
        }

        let mut missing_provider_ids: Vec<String> = configured_provider_ids
            .difference(&connected_provider_ids)
            .cloned()
            .collect();
        missing_provider_ids.sort();

        if missing_provider_ids.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "model_priority references disconnected providers: {}",
                missing_provider_ids.join(", ")
            ))
        }
    }

    pub async fn reset_runtime_settings(&self) {
        if let Some(coordinator) = self.coordinator().await {
            let _ = coordinator.update_dispatch_limit(50).await;
            let _ = coordinator
                .update_model_priorities(std::collections::HashMap::new())
                .await;
        }
        if let Some(supervisor) = self.supervisor().await {
            let _ = supervisor
                .update_session_limits(std::collections::HashMap::new(), 1)
                .await;
        }
    }

    pub(super) async fn apply_runtime_settings_from_db(&self) {
        let repo = SettingsRepository::new(self.db().clone(), self.events().clone());
        let raw = repo
            .get(SETTINGS_RAW_KEY)
            .await
            .ok()
            .flatten()
            .map(|s| s.value);
        let Some(raw) = raw else {
            self.reset_runtime_settings().await;
            return;
        };
        let settings = DjinnSettings::from_db_value(&raw);
        self.apply_runtime_settings(&settings).await;
    }

    async fn apply_runtime_settings(&self, settings: &DjinnSettings) {
        let mut coordinator_handle = None;
        if let Some(coordinator) = self.coordinator().await {
            let _ = coordinator
                .update_dispatch_limit(settings.dispatch_limit_or_default())
                .await;
            let _ = coordinator
                .update_model_priorities(settings.model_priority_or_default())
                .await;
            coordinator_handle = Some(coordinator);
        }

        if let Some(supervisor) = self.supervisor().await {
            let _ = supervisor
                .update_session_limits(settings.max_sessions_or_default(), 1)
                .await;
        }

        // Capacity/model changes can make additional tasks dispatchable immediately.
        // Trigger a dispatch pass now instead of waiting for the next event/tick.
        if let Some(coordinator) = coordinator_handle {
            let _ = coordinator.trigger_dispatch().await;
        }
    }
}
