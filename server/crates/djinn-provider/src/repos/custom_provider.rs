use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{CustomProvider, SeedModel};
use djinn_db::{Database, Result};

pub struct CustomProviderRepository {
    db: Database,
    events: EventBus,
}

impl CustomProviderRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    /// Return all custom providers, ordered by `created_at`.
    pub async fn list(&self) -> Result<Vec<CustomProvider>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, (String, String, String, String, String, String)>(
            "SELECT id, name, base_url, env_var, seed_models, created_at
             FROM custom_providers
             ORDER BY created_at ASC",
        )
        .fetch_all(self.db.pool())
        .await?;

        let providers = rows
            .into_iter()
            .map(
                |(id, name, base_url, env_var, seed_json, created_at)| CustomProvider {
                    id,
                    name,
                    base_url,
                    env_var,
                    seed_models: serde_json::from_str::<Vec<SeedModel>>(&seed_json)
                        .unwrap_or_default(),
                    created_at,
                },
            )
            .collect();
        Ok(providers)
    }

    /// Insert or replace a custom provider.
    pub async fn upsert(&self, provider: &CustomProvider) -> Result<()> {
        let id = provider.id.clone();
        let name = provider.name.clone();
        let base_url = provider.base_url.clone();
        let env_var = provider.env_var.clone();
        let seed_json =
            serde_json::to_string(&provider.seed_models).unwrap_or_else(|_| "[]".into());

        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO custom_providers (id, name, base_url, env_var, seed_models)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
               name        = excluded.name,
               base_url    = excluded.base_url,
               env_var     = excluded.env_var,
               seed_models = excluded.seed_models",
        )
        .bind(&id)
        .bind(&name)
        .bind(&base_url)
        .bind(&env_var)
        .bind(&seed_json)
        .execute(self.db.pool())
        .await?;
        self.events
            .send(DjinnEventEnvelope::custom_provider_upserted(provider));
        Ok(())
    }

    /// Delete a custom provider by ID. Returns true if a row was removed.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query("DELETE FROM custom_providers WHERE id = ?1")
            .bind(id)
            .execute(self.db.pool())
            .await?;
        let deleted = result.rows_affected() > 0;
        if deleted {
            self.events
                .send(DjinnEventEnvelope::custom_provider_deleted(id));
        }
        Ok(deleted)
    }

    /// Return a single provider by ID, or `None`.
    pub async fn get(&self, id: &str) -> Result<Option<CustomProvider>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as::<_, (String, String, String, String, String, String)>(
            "SELECT id, name, base_url, env_var, seed_models, created_at
             FROM custom_providers WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?;

        Ok(
            row.map(|(id, name, base_url, env_var, seed_json, created_at)| {
                let seed_models: Vec<SeedModel> =
                    serde_json::from_str(&seed_json).unwrap_or_default();
                CustomProvider {
                    id,
                    name,
                    base_url,
                    env_var,
                    seed_models,
                    created_at,
                }
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_and_list() {
        let db = Database::open_in_memory().expect("failed to create test database");
        let repo = CustomProviderRepository::new(db, EventBus::noop());

        let provider = CustomProvider {
            id: "my-llm".to_string(),
            name: "My LLM".to_string(),
            base_url: "https://api.my-llm.com/v1".to_string(),
            env_var: "MY_LLM_API_KEY".to_string(),
            seed_models: vec![SeedModel {
                id: "my-model".to_string(),
                name: "My Model".to_string(),
            }],
            created_at: String::new(),
        };

        repo.upsert(&provider).await.unwrap();

        let list = repo.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "my-llm");
        assert_eq!(list[0].seed_models.len(), 1);
        assert_eq!(list[0].seed_models[0].id, "my-model");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_returns_none_for_missing() {
        let db = Database::open_in_memory().expect("failed to create test database");
        let repo = CustomProviderRepository::new(db, EventBus::noop());
        assert!(repo.get("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_is_idempotent() {
        let db = Database::open_in_memory().expect("failed to create test database");
        let repo = CustomProviderRepository::new(db, EventBus::noop());

        let mut provider = CustomProvider {
            id: "p1".to_string(),
            name: "Provider 1".to_string(),
            base_url: "https://example.com/v1".to_string(),
            env_var: "P1_KEY".to_string(),
            seed_models: vec![],
            created_at: String::new(),
        };

        repo.upsert(&provider).await.unwrap();
        provider.name = "Provider 1 Updated".to_string();
        repo.upsert(&provider).await.unwrap();

        let list = repo.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Provider 1 Updated");
    }
}
