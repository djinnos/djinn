use uuid::Uuid;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Credential;
use djinn_db::crypto;
use djinn_db::{Database, Result, ensure_db};

pub struct CredentialRepository {
    db: Database,
    events: EventBus,
}

impl CredentialRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    /// Upsert a credential by `key_name`. Encrypts `raw_value` before storage.
    ///
    /// Emits `CredentialCreated` on insert, `CredentialUpdated` on update.
    /// The event payload never includes the encrypted value.
    pub async fn set(
        &self,
        provider_id: &str,
        key_name: &str,
        raw_value: &str,
    ) -> Result<Credential> {
        let encrypted = crypto::encrypt(raw_value)?;
        ensure_db!(self.db);
        let existing_id: Option<String> =
            sqlx::query_scalar("SELECT id FROM credentials WHERE key_name = ?")
                .bind(key_name)
                .fetch_optional(self.db.pool())
                .await?;
        let id = existing_id
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        let is_new = existing_id.is_none();

        sqlx::query(
            "INSERT INTO credentials (id, provider_id, key_name, encrypted_value,
                                      created_at, updated_at)
             VALUES (?, ?, ?, ?,
                     DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'),
                     DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'))
             ON DUPLICATE KEY UPDATE
                 provider_id     = VALUES(provider_id),
                 encrypted_value = VALUES(encrypted_value),
                 updated_at      = VALUES(updated_at)",
        )
        .bind(&id)
        .bind(provider_id)
        .bind(key_name)
        .bind(&encrypted)
        .execute(self.db.pool())
        .await?;

        let cred = sqlx::query_as::<_, Credential>(
            "SELECT id, provider_id, key_name, created_at, updated_at
             FROM credentials WHERE key_name = ?",
        )
        .bind(key_name)
        .fetch_one(self.db.pool())
        .await?;

        if is_new {
            self.events
                .send(DjinnEventEnvelope::credential_created(&cred));
        } else {
            self.events
                .send(DjinnEventEnvelope::credential_updated(&cred));
        }

        Ok(cred)
    }

    /// List all credentials. Never returns raw key values.
    pub async fn list(&self) -> Result<Vec<Credential>> {
        ensure_db!(self.db);
        Ok(sqlx::query_as::<_, Credential>(
            "SELECT id, provider_id, key_name, created_at, updated_at
                 FROM credentials
                 ORDER BY provider_id, key_name",
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Delete a credential by `key_name`. Emits `CredentialDeleted` with the ID.
    pub async fn delete(&self, key_name: &str) -> Result<bool> {
        ensure_db!(self.db);
        let deleted_id: Option<String> =
            sqlx::query_scalar("SELECT id FROM credentials WHERE key_name = ?")
                .bind(key_name)
                .fetch_optional(self.db.pool())
                .await?;

        if let Some(ref id) = deleted_id {
            sqlx::query("DELETE FROM credentials WHERE id = ?")
                .bind(id)
                .execute(self.db.pool())
                .await?;
        }

        if let Some(id) = deleted_id {
            self.events
                .send(DjinnEventEnvelope::credential_deleted(&id));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Delete all credentials for a given `provider_id`.
    /// Returns the number of rows deleted. Emits `CredentialDeleted` for each.
    pub async fn delete_by_provider(&self, provider_id: &str) -> Result<u64> {
        ensure_db!(self.db);
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM credentials WHERE provider_id = ?")
                .bind(provider_id)
                .fetch_all(self.db.pool())
                .await?;

        if ids.is_empty() {
            return Ok(0);
        }

        let result = sqlx::query("DELETE FROM credentials WHERE provider_id = ?")
            .bind(provider_id)
            .execute(self.db.pool())
            .await?;

        for id in ids {
            self.events
                .send(DjinnEventEnvelope::credential_deleted(&id));
        }

        Ok(result.rows_affected())
    }

    /// Check whether a credential with the given `key_name` exists (without decrypting).
    pub async fn exists(&self, key_name: &str) -> Result<bool> {
        ensure_db!(self.db);
        let found: Option<String> =
            sqlx::query_scalar("SELECT id FROM credentials WHERE key_name = ?")
                .bind(key_name)
                .fetch_optional(self.db.pool())
                .await?;
        Ok(found.is_some())
    }

    /// Decrypt and return the raw API key for `key_name`.
    ///
    /// Called by `AgentSupervisor` at dispatch time to obtain the key for
    /// provider creation. Never exposed via MCP tools.
    pub async fn get_decrypted(&self, key_name: &str) -> Result<Option<String>> {
        ensure_db!(self.db);
        let blob: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT encrypted_value FROM credentials WHERE key_name = ?")
                .bind(key_name)
                .fetch_optional(self.db.pool())
                .await?;

        match blob {
            Some(b) => Ok(Some(crypto::decrypt(&b)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use tokio::sync::broadcast;

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    fn make_repo() -> CredentialRepository {
        let db = Database::open_in_memory().expect("failed to create test database");
        CredentialRepository::new(db, EventBus::noop())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_and_list() {
        let repo = make_repo();
        let cred = repo
            .set("anthropic", "ANTHROPIC_API_KEY", "sk-test")
            .await
            .unwrap();
        assert_eq!(cred.provider_id, "anthropic");
        assert_eq!(cred.key_name, "ANTHROPIC_API_KEY");

        let list = repo.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].key_name, "ANTHROPIC_API_KEY");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_emits_created_then_updated() {
        let db = Database::open_in_memory().expect("failed to create test database");
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = CredentialRepository::new(db, event_bus_for(&tx));

        repo.set("anthropic", "ANTHROPIC_API_KEY", "key-v1")
            .await
            .unwrap();
        let ev1 = rx.recv().await.unwrap();
        assert_eq!(ev1.entity_type, "credential");
        assert_eq!(ev1.action, "created");

        repo.set("anthropic", "ANTHROPIC_API_KEY", "key-v2")
            .await
            .unwrap();
        let ev2 = rx.recv().await.unwrap();
        assert_eq!(ev2.entity_type, "credential");
        assert_eq!(ev2.action, "updated");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_decrypted_round_trip() {
        let repo = make_repo();
        repo.set("openai", "OPENAI_API_KEY", "sk-secret-value")
            .await
            .unwrap();

        let decrypted = repo.get_decrypted("OPENAI_API_KEY").await.unwrap();
        assert_eq!(decrypted.as_deref(), Some("sk-secret-value"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_decrypted_missing_returns_none() {
        let repo = make_repo();
        let result = repo.get_decrypted("NO_SUCH_KEY").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_removes_and_emits_event() {
        let db = Database::open_in_memory().expect("failed to create test database");
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = CredentialRepository::new(db, event_bus_for(&tx));

        let cred = repo
            .set("anthropic", "ANTHROPIC_API_KEY", "val")
            .await
            .unwrap();
        let _created = rx.recv().await.unwrap(); // consume CredentialCreated

        let deleted = repo.delete("ANTHROPIC_API_KEY").await.unwrap();
        assert!(deleted);

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.entity_type, "credential");
        assert_eq!(ev.action, "deleted");
        assert_eq!(ev.payload["id"].as_str().unwrap(), cred.id);

        let list = repo.list().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_nonexistent_returns_false() {
        let repo = make_repo();
        let deleted = repo.delete("NO_SUCH_KEY").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_keeps_unique_key_name() {
        let repo = make_repo();
        repo.set("anthropic", "ANTHROPIC_API_KEY", "v1")
            .await
            .unwrap();
        repo.set("anthropic", "ANTHROPIC_API_KEY", "v2")
            .await
            .unwrap();

        let list = repo.list().await.unwrap();
        assert_eq!(list.len(), 1, "upsert must not create duplicate rows");

        let v = repo.get_decrypted("ANTHROPIC_API_KEY").await.unwrap();
        assert_eq!(v.as_deref(), Some("v2"));
    }
}
