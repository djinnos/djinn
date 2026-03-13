use std::collections::{HashMap, HashSet};

use crate::actors::slot::{ModelSlotConfig, SlotPoolConfig};
use crate::db::CredentialRepository;
use crate::db::SettingsRepository;
use crate::models::DjinnSettings;

use super::{AppState, SETTINGS_RAW_KEY};

impl AppState {
    fn slot_pool_config_for_settings(settings: &DjinnSettings) -> SlotPoolConfig {
        let role_priorities = settings.model_priority_or_default();
        let max_sessions = settings.max_sessions_or_default();

        let mut model_ids: HashSet<String> = max_sessions.keys().cloned().collect();
        for models in role_priorities.values() {
            for model_id in models {
                if model_id.contains('/') {
                    model_ids.insert(model_id.clone());
                }
            }
        }

        let mut roles_by_model: HashMap<String, HashSet<String>> = HashMap::new();
        for (role, model_ids) in &role_priorities {
            for model_id in model_ids {
                if model_id.contains('/') {
                    roles_by_model
                        .entry(model_id.clone())
                        .or_default()
                        .insert(role.clone());
                }
            }
        }

        let mut models = model_ids
            .into_iter()
            .map(|model_id| ModelSlotConfig {
                max_slots: max_sessions.get(&model_id).copied().unwrap_or(1),
                roles: roles_by_model.remove(&model_id).unwrap_or_default(),
                model_id,
            })
            .collect::<Vec<_>>();
        models.sort_by(|a, b| a.model_id.cmp(&b.model_id));

        SlotPoolConfig {
            models,
            role_priorities,
        }
    }

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
        let connected_provider_ids = self.catalog().connected_provider_ids(&credentials);

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
        if let Some(pool) = self.pool().await {
            let _ = pool
                .reconfigure(SlotPoolConfig {
                    models: Vec::new(),
                    role_priorities: std::collections::HashMap::new(),
                })
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

        if let Some(pool) = self.pool().await {
            let _ = pool
                .reconfigure(Self::slot_pool_config_for_settings(settings))
                .await;
        }

        // Capacity/model changes can make additional tasks dispatchable immediately.
        // Trigger a dispatch pass now instead of waiting for the next event/tick.
        if let Some(coordinator) = coordinator_handle {
            let _ = coordinator.trigger_dispatch().await;
        }

        // Initialize Langfuse/OTLP telemetry if configured.
        if let (Some(pk), Some(sk)) = (&settings.langfuse_public_key, &settings.langfuse_secret_key)
        {
            let endpoint = settings
                .langfuse_endpoint
                .as_deref()
                .unwrap_or("http://localhost:3000/api/public/otel");
            let config = crate::agent::provider::telemetry::LangfuseConfig {
                endpoint: endpoint.to_string(),
                public_key: pk.clone(),
                secret_key: sk.clone(),
            };
            if let Err(e) = crate::agent::provider::telemetry::init(&config) {
                tracing::warn!(error = %e, "failed to initialize Langfuse telemetry");
            }
        }
    }
}
