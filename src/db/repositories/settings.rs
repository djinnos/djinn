use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::error::Result;
use crate::events::DjinnEvent;
use crate::models::settings::Setting;

pub struct SettingsRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl SettingsRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Setting>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Setting>(
            "SELECT key, value, updated_at FROM settings WHERE key = ?1",
        )
        .bind(key)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Upsert a setting. Returns the full entity and emits `SettingUpdated`.
    pub async fn set(&self, key: &str, value: &str) -> Result<Setting> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at)
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(key) DO UPDATE SET
               value = excluded.value,
               updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .execute(self.db.pool())
        .await?;
        let setting = sqlx::query_as::<_, Setting>(
            "SELECT key, value, updated_at FROM settings WHERE key = ?1",
        )
        .bind(key)
        .fetch_one(self.db.pool())
        .await?;

        // Emit event — ignore send error (no receivers is fine).
        let _ = self
            .events
            .send(DjinnEvent::SettingUpdated(setting.clone()));
        Ok(setting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    #[tokio::test]
    async fn set_and_get_setting() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = SettingsRepository::new(db, tx);

        let setting = repo.set("theme", "dark").await.unwrap();
        assert_eq!(setting.key, "theme");
        assert_eq!(setting.value, "dark");

        let fetched = repo.get("theme").await.unwrap().unwrap();
        assert_eq!(fetched.value, "dark");
    }

    #[tokio::test]
    async fn set_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = SettingsRepository::new(db, tx);

        repo.set("lang", "en").await.unwrap();

        let event = rx.recv().await.unwrap();
        match event {
            DjinnEvent::SettingUpdated(s) => {
                assert_eq!(s.key, "lang");
                assert_eq!(s.value, "en");
            }
            _ => panic!("expected SettingUpdated event"),
        }
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = SettingsRepository::new(db, tx);

        assert!(repo.get("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_upserts() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = SettingsRepository::new(db, tx);

        repo.set("k", "v1").await.unwrap();
        let updated = repo.set("k", "v2").await.unwrap();
        assert_eq!(updated.value, "v2");

        let fetched = repo.get("k").await.unwrap().unwrap();
        assert_eq!(fetched.value, "v2");
    }
}
