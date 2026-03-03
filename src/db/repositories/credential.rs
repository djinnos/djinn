use tokio::sync::broadcast;
use uuid::Uuid;

use crate::crypto;
use crate::db::connection::{Database, OptionalExt};
use crate::error::Result;
use crate::events::DjinnEvent;
use crate::models::credential::Credential;

pub struct CredentialRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl CredentialRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
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
        let provider_id = provider_id.to_owned();
        let key_name = key_name.to_owned();

        let (cred, is_new) = self
            .db
            .write(move |conn| {
                // Check if a row exists for this key_name.
                let existing_id: Option<String> = conn
                    .query_row(
                        "SELECT id FROM credentials WHERE key_name = ?1",
                        [&key_name],
                        |r| r.get(0),
                    )
                    .optional()?;

                let id = existing_id
                    .clone()
                    .unwrap_or_else(|| Uuid::now_v7().to_string());
                let is_new = existing_id.is_none();

                conn.execute(
                    "INSERT INTO credentials (id, provider_id, key_name, encrypted_value,
                                              created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4,
                             strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                             strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                     ON CONFLICT(key_name) DO UPDATE SET
                         provider_id     = excluded.provider_id,
                         encrypted_value = excluded.encrypted_value,
                         updated_at      = excluded.updated_at",
                    rusqlite::params![&id, &provider_id, &key_name, &encrypted],
                )?;

                let cred = conn.query_row(
                    "SELECT id, provider_id, key_name, created_at, updated_at
                     FROM credentials WHERE key_name = ?1",
                    [&key_name],
                    row_to_credential,
                )?;

                Ok((cred, is_new))
            })
            .await?;

        if is_new {
            let _ = self.events.send(DjinnEvent::CredentialCreated(cred.clone()));
        } else {
            let _ = self.events.send(DjinnEvent::CredentialUpdated(cred.clone()));
        }

        Ok(cred)
    }

    /// List all credentials. Never returns raw key values.
    pub async fn list(&self) -> Result<Vec<Credential>> {
        self.db
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, provider_id, key_name, created_at, updated_at
                     FROM credentials
                     ORDER BY provider_id, key_name",
                )?;
                let rows = stmt
                    .query_map([], row_to_credential)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Delete a credential by `key_name`. Emits `CredentialDeleted` with the ID.
    pub async fn delete(&self, key_name: &str) -> Result<bool> {
        let key_name = key_name.to_owned();
        let deleted_id = self
            .db
            .write(move |conn| {
                let id: Option<String> = conn
                    .query_row(
                        "SELECT id FROM credentials WHERE key_name = ?1",
                        [&key_name],
                        |r| r.get(0),
                    )
                    .optional()?;

                if let Some(ref id) = id {
                    conn.execute(
                        "DELETE FROM credentials WHERE id = ?1",
                        [id],
                    )?;
                }
                Ok(id)
            })
            .await?;

        if let Some(id) = deleted_id {
            let _ = self.events.send(DjinnEvent::CredentialDeleted { id });
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Decrypt and return the raw API key for `key_name`.
    ///
    /// Called by `AgentSupervisor` at dispatch time to obtain the key for
    /// Goose provider creation. Never exposed via MCP tools.
    pub async fn get_decrypted(&self, key_name: &str) -> Result<Option<String>> {
        let key_name = key_name.to_owned();
        let blob: Option<Vec<u8>> = self
            .db
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT encrypted_value FROM credentials WHERE key_name = ?1",
                        [&key_name],
                        |r| r.get(0),
                    )
                    .optional()?)
            })
            .await?;

        match blob {
            Some(b) => Ok(Some(crypto::decrypt(&b)?)),
            None => Ok(None),
        }
    }
}

fn row_to_credential(row: &rusqlite::Row<'_>) -> rusqlite::Result<Credential> {
    Ok(Credential {
        id: row.get(0)?,
        provider_id: row.get(1)?,
        key_name: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    fn make_repo() -> CredentialRepository {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        CredentialRepository::new(db, tx)
    }

    #[tokio::test]
    async fn set_and_list() {
        let repo = make_repo();
        let cred = repo.set("anthropic", "ANTHROPIC_API_KEY", "sk-test").await.unwrap();
        assert_eq!(cred.provider_id, "anthropic");
        assert_eq!(cred.key_name, "ANTHROPIC_API_KEY");

        let list = repo.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].key_name, "ANTHROPIC_API_KEY");
    }

    #[tokio::test]
    async fn set_emits_created_then_updated() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = CredentialRepository::new(db, tx);

        repo.set("anthropic", "ANTHROPIC_API_KEY", "key-v1").await.unwrap();
        let ev1 = rx.recv().await.unwrap();
        assert!(matches!(ev1, DjinnEvent::CredentialCreated(_)));

        repo.set("anthropic", "ANTHROPIC_API_KEY", "key-v2").await.unwrap();
        let ev2 = rx.recv().await.unwrap();
        assert!(matches!(ev2, DjinnEvent::CredentialUpdated(_)));
    }

    #[tokio::test]
    async fn get_decrypted_round_trip() {
        let repo = make_repo();
        repo.set("openai", "OPENAI_API_KEY", "sk-secret-value").await.unwrap();

        let decrypted = repo.get_decrypted("OPENAI_API_KEY").await.unwrap();
        assert_eq!(decrypted.as_deref(), Some("sk-secret-value"));
    }

    #[tokio::test]
    async fn get_decrypted_missing_returns_none() {
        let repo = make_repo();
        let result = repo.get_decrypted("NO_SUCH_KEY").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn delete_removes_and_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = CredentialRepository::new(db, tx);

        let cred = repo.set("anthropic", "ANTHROPIC_API_KEY", "val").await.unwrap();
        let _created = rx.recv().await.unwrap(); // consume CredentialCreated

        let deleted = repo.delete("ANTHROPIC_API_KEY").await.unwrap();
        assert!(deleted);

        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev, DjinnEvent::CredentialDeleted { id } if id == cred.id));

        let list = repo.list().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_false() {
        let repo = make_repo();
        let deleted = repo.delete("NO_SUCH_KEY").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn upsert_keeps_unique_key_name() {
        let repo = make_repo();
        repo.set("anthropic", "ANTHROPIC_API_KEY", "v1").await.unwrap();
        repo.set("anthropic", "ANTHROPIC_API_KEY", "v2").await.unwrap();

        let list = repo.list().await.unwrap();
        assert_eq!(list.len(), 1, "upsert must not create duplicate rows");

        let v = repo.get_decrypted("ANTHROPIC_API_KEY").await.unwrap();
        assert_eq!(v.as_deref(), Some("v2"));
    }
}
