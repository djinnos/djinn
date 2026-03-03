use crate::db::connection::Database;
use crate::error::Result;
use crate::models::provider::{CustomProvider, SeedModel};

pub struct CustomProviderRepository {
    db: Database,
}

impl CustomProviderRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Return all custom providers, ordered by `created_at`.
    pub async fn list(&self) -> Result<Vec<CustomProvider>> {
        self.db
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, name, base_url, env_var, seed_models, created_at
                     FROM custom_providers
                     ORDER BY created_at ASC",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?;

                let mut providers = Vec::new();
                for row in rows {
                    let (id, name, base_url, env_var, seed_json, created_at) = row?;
                    let seed_models: Vec<SeedModel> =
                        serde_json::from_str(&seed_json).unwrap_or_default();
                    providers.push(CustomProvider {
                        id,
                        name,
                        base_url,
                        env_var,
                        seed_models,
                        created_at,
                    });
                }
                Ok(providers)
            })
            .await
    }

    /// Insert or replace a custom provider.
    pub async fn upsert(&self, provider: &CustomProvider) -> Result<()> {
        let id = provider.id.clone();
        let name = provider.name.clone();
        let base_url = provider.base_url.clone();
        let env_var = provider.env_var.clone();
        let seed_json = serde_json::to_string(&provider.seed_models).unwrap_or_else(|_| "[]".into());

        self.db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO custom_providers (id, name, base_url, env_var, seed_models)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(id) DO UPDATE SET
                       name        = excluded.name,
                       base_url    = excluded.base_url,
                       env_var     = excluded.env_var,
                       seed_models = excluded.seed_models",
                    [&id, &name, &base_url, &env_var, &seed_json],
                )?;
                Ok(())
            })
            .await
    }

    /// Return a single provider by ID, or `None`.
    pub async fn get(&self, id: &str) -> Result<Option<CustomProvider>> {
        let id = id.to_owned();
        self.db
            .call(move |conn| {
                let result = conn.query_row(
                    "SELECT id, name, base_url, env_var, seed_models, created_at
                     FROM custom_providers WHERE id = ?1",
                    [&id],
                    |row| {
                        let seed_json: String = row.get(4)?;
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            seed_json,
                            row.get::<_, String>(5)?,
                        ))
                    },
                );

                match result {
                    Ok((id, name, base_url, env_var, seed_json, created_at)) => {
                        let seed_models: Vec<SeedModel> =
                            serde_json::from_str(&seed_json).unwrap_or_default();
                        Ok(Some(CustomProvider {
                            id,
                            name,
                            base_url,
                            env_var,
                            seed_models,
                            created_at,
                        }))
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    #[tokio::test]
    async fn upsert_and_list() {
        let db = test_helpers::create_test_db();
        let repo = CustomProviderRepository::new(db);

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

    #[tokio::test]
    async fn get_returns_none_for_missing() {
        let db = test_helpers::create_test_db();
        let repo = CustomProviderRepository::new(db);
        assert!(repo.get("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_is_idempotent() {
        let db = test_helpers::create_test_db();
        let repo = CustomProviderRepository::new(db);

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
