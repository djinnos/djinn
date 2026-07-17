use std::collections::{HashMap, HashSet};

use djinn_agent::actors::slot::{ModelSlotConfig, SlotPoolConfig};
use djinn_agent::resource_monitor::MemoryStatus;
use djinn_core::models::DjinnSettings;
use djinn_db::SettingsRepository;
use djinn_provider::repos::CredentialRepository;

use super::{AppState, SETTINGS_RAW_KEY};

/// Maximum auto-detected slots per model to prevent runaway on high-memory machines.
const AUTO_MAX_SLOTS_CAP: u32 = 8;

const ALL_ROLES: &[&str] = &["worker", "reviewer", "lead", "planner", "architect"];

impl AppState {
    fn slot_pool_config_for_settings(settings: &DjinnSettings) -> SlotPoolConfig {
        // The slot pool is now ELASTIC: `dispatch` spawns a slot on demand and
        // admission is gated per-user (each user's per-model cap) by the
        // coordinator — there is no per-model ceiling here. This config only
        // pre-warms a few slots for the global deployment model list;
        // per-user-selected models that aren't in this list spawn on first use.
        let models_list = settings.models_or_default();
        let all_roles: HashSet<String> = ALL_ROLES.iter().map(|r| r.to_string()).collect();
        let model_count = models_list.iter().filter(|id| id.contains('/')).count();
        let auto_default = Self::auto_default_slots(model_count);

        let mut models: Vec<ModelSlotConfig> = models_list
            .iter()
            .filter(|id| id.contains('/'))
            .map(|model_id| ModelSlotConfig {
                max_slots: auto_default,
                roles: all_roles.clone(),
                model_id: model_id.clone(),
            })
            .collect();
        models.sort_by(|a, b| a.model_id.cmp(&b.model_id));

        let role_priorities: HashMap<String, Vec<String>> = ALL_ROLES
            .iter()
            .map(|r| (r.to_string(), models_list.clone()))
            .collect();

        SlotPoolConfig {
            models,
            role_priorities,
        }
    }

    /// Compute auto-detected default max slots per model based on system memory.
    ///
    /// Divides the total suggested sessions across the number of configured models,
    /// caps at [`AUTO_MAX_SLOTS_CAP`], and floors at 1. Returns 1 on non-Linux or
    /// when memory info is unavailable.
    fn auto_default_slots(model_count: usize) -> u32 {
        let Some(mem) = MemoryStatus::read() else {
            return 1;
        };

        let total_suggested = mem.suggested_max_sessions();
        let divisor = (model_count as u32).max(1);
        let per_model = (total_suggested / divisor).clamp(1, AUTO_MAX_SLOTS_CAP);

        tracing::info!(
            total_memory_gib = mem.effective_limit_bytes / (1024 * 1024 * 1024),
            total_suggested_sessions = total_suggested,
            model_count = model_count,
            auto_max_slots_per_model = per_model,
            "auto-detected max_slots from system memory"
        );

        per_model
    }

    pub async fn apply_settings(&self, settings: &DjinnSettings) -> Result<(), String> {
        self.validate_models_providers_connected(settings).await?;
        let raw =
            serde_json::to_string(settings).map_err(|e| format!("serialize settings: {e}"))?;
        let repo = SettingsRepository::new(self.db().clone(), self.event_bus());
        repo.set(SETTINGS_RAW_KEY, &raw)
            .await
            .map_err(|e| e.to_string())?;
        self.apply_runtime_settings(settings).await;
        Ok(())
    }

    async fn validate_models_providers_connected(
        &self,
        settings: &DjinnSettings,
    ) -> Result<(), String> {
        let models = settings.models_or_default();
        if models.is_empty() {
            return Ok(());
        }

        let configured_provider_ids: HashSet<String> = models
            .iter()
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

        let repo = CredentialRepository::new(self.db().clone(), self.event_bus());
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
                "models references disconnected providers: {}",
                missing_provider_ids.join(", ")
            ))
        }
    }

    /// React to a change in some user's per-user model selection or caps. The
    /// elastic slot pool sizes itself on demand and the per-user cap is enforced
    /// at dispatch, so this just re-applies the (idempotent) global runtime
    /// config and triggers a dispatch pass for responsiveness. Called from
    /// `user_settings_set`.
    pub async fn apply_user_model_change(&self) {
        let repo = SettingsRepository::new(self.db().clone(), self.event_bus());
        let settings = match repo.get(SETTINGS_RAW_KEY).await {
            Ok(Some(s)) => DjinnSettings::from_db_value(&s.value),
            _ => DjinnSettings::default(),
        };
        self.apply_runtime_settings(&settings).await;
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
        let repo = SettingsRepository::new(self.db().clone(), self.event_bus());
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

        // If the settings were migrated from a legacy format, re-save them in the current typed
        // schema so subsequent startups parse cleanly without triggering the migration warning.
        if let Ok(canonical) = serde_json::to_string(&settings)
            && canonical != raw
            && let Err(e) = repo.set(SETTINGS_RAW_KEY, &canonical).await
        {
            tracing::warn!(error = %e, "failed to persist migrated settings");
        }

        self.apply_runtime_settings(&settings).await;
    }

    async fn apply_runtime_settings(&self, settings: &DjinnSettings) {
        let mut coordinator_handle = None;
        if let Some(coordinator) = self.coordinator().await {
            let _ = coordinator
                .update_dispatch_limit(settings.dispatch_limit_or_default())
                .await;
            // Build per-role priorities: all roles use the same flat model list.
            let role_priorities: HashMap<String, Vec<String>> = ALL_ROLES
                .iter()
                .map(|r| (r.to_string(), settings.models_or_default()))
                .collect();
            let _ = coordinator.update_model_priorities(role_priorities).await;
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
    }
}
