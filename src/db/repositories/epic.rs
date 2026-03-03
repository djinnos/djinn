use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::error::{Error, Result};
use crate::events::DjinnEvent;
use crate::models::epic::Epic;

pub struct EpicRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl EpicRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
    }

    pub async fn list(&self) -> Result<Vec<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, Epic>(
                "SELECT id, short_id, title, description, emoji, color, status,
                        owner, created_at, updated_at, closed_at
                 FROM epics ORDER BY created_at",
            )
            .fetch_all(self.db.pool())
            .await?,
        )
    }

    pub async fn get(&self, id: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, Epic>(
                "SELECT id, short_id, title, description, emoji, color, status,
                        owner, created_at, updated_at, closed_at
                 FROM epics WHERE id = ?1",
            )
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?,
        )
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, Epic>(
                "SELECT id, short_id, title, description, emoji, color, status,
                        owner, created_at, updated_at, closed_at
                 FROM epics WHERE short_id = ?1",
            )
            .bind(short_id)
            .fetch_optional(self.db.pool())
            .await?,
        )
    }

    pub async fn create(
        &self,
        title: &str,
        description: &str,
        emoji: &str,
        color: &str,
        owner: &str,
    ) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        sqlx::query(
            "INSERT INTO epics (id, short_id, title, description, emoji, color, owner)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&id)
        .bind(&short_id)
        .bind(title)
        .bind(description)
        .bind(emoji)
        .bind(color)
        .bind(owner)
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as(
            "SELECT id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at
             FROM epics WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self.events.send(DjinnEvent::EpicCreated(epic.clone()));
        Ok(epic)
    }

    pub async fn update(
        &self,
        id: &str,
        title: &str,
        description: &str,
        emoji: &str,
        color: &str,
        owner: &str,
    ) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE epics SET title = ?2, description = ?3, emoji = ?4,
                    color = ?5, owner = ?6,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(title)
        .bind(description)
        .bind(emoji)
        .bind(color)
        .bind(owner)
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as(
            "SELECT id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at
             FROM epics WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self.events.send(DjinnEvent::EpicUpdated(epic.clone()));
        Ok(epic)
    }

    pub async fn close(&self, id: &str) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE epics SET status = 'closed',
                    closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as(
            "SELECT id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at
             FROM epics WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self.events.send(DjinnEvent::EpicUpdated(epic.clone()));
        Ok(epic)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM epics WHERE id = ?1")
            .bind(id)
            .execute(self.db.pool())
            .await?;

        let _ = self
            .events
            .send(DjinnEvent::EpicDeleted { id: id.to_owned() });
        Ok(())
    }

    /// Generate a unique 4-char base36 short ID for the epics table.
    async fn generate_short_id(&self, seed_id: &str) -> Result<String> {
        self.db.ensure_initialized().await?;
        let seed = uuid::Uuid::parse_str(seed_id).map_err(|e| Error::Internal(e.to_string()))?;
        let candidate = short_id_from_uuid(&seed);
        if !short_id_exists(self.db.pool(), "epics", &candidate).await? {
            return Ok(candidate);
        }
        for _ in 0..16 {
            let candidate = short_id_from_uuid(&uuid::Uuid::now_v7());
            if !short_id_exists(self.db.pool(), "epics", &candidate).await? {
                return Ok(candidate);
            }
        }
        Err(Error::Internal("short_id collision after 16 retries".into()))
    }
}

// ── Short ID helpers ─────────────────────────────────────────────────────────

/// Derive a 4-char base36 short ID from the last 4 bytes of a UUIDv7.
fn short_id_from_uuid(id: &uuid::Uuid) -> String {
    let bytes = id.as_bytes();
    let n = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    encode_base36(n % 1_679_616) // 36^4
}

/// Encode `n` (0..1_679_615) as a zero-padded 4-char base36 string.
fn encode_base36(mut n: u32) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [b'0'; 4];
    for i in (0..4).rev() {
        buf[i] = CHARS[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

async fn short_id_exists(pool: &sqlx::SqlitePool, table: &str, short_id: &str) -> Result<bool> {
    // Table name is from internal code only — not user input — so this is safe.
    let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE short_id = ?1)");
    Ok(sqlx::query_scalar::<_, i64>(&sql)
        .bind(short_id)
        .fetch_one(pool)
        .await?
        > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    #[tokio::test]
    async fn create_and_get_epic() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("My Epic", "", "🚀", "#8b5cf6", "user@example.com").await.unwrap();
        assert_eq!(epic.title, "My Epic");
        assert_eq!(epic.status, "open");
        assert_eq!(epic.short_id.len(), 4);

        let fetched = repo.get(&epic.id).await.unwrap().unwrap();
        assert_eq!(fetched.title, "My Epic");
    }

    #[tokio::test]
    async fn short_id_lookup() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Lookup", "", "", "", "").await.unwrap();
        let found = repo.get_by_short_id(&epic.short_id).await.unwrap().unwrap();
        assert_eq!(found.id, epic.id);
    }

    #[tokio::test]
    async fn create_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        repo.create("Event Epic", "", "", "", "").await.unwrap();
        match rx.recv().await.unwrap() {
            DjinnEvent::EpicCreated(e) => assert_eq!(e.title, "Event Epic"),
            _ => panic!("expected EpicCreated"),
        }
    }

    #[tokio::test]
    async fn update_epic() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Old", "", "", "", "").await.unwrap();
        let _ = rx.recv().await.unwrap();

        let updated = repo.update(&epic.id, "New", "desc", "🎯", "#fff", "").await.unwrap();
        assert_eq!(updated.title, "New");

        match rx.recv().await.unwrap() {
            DjinnEvent::EpicUpdated(e) => assert_eq!(e.title, "New"),
            _ => panic!("expected EpicUpdated"),
        }
    }

    #[tokio::test]
    async fn close_epic() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Closeable", "", "", "", "").await.unwrap();
        let closed = repo.close(&epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());
    }

    #[tokio::test]
    async fn delete_epic() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Delete me", "", "", "", "").await.unwrap();
        let _ = rx.recv().await.unwrap();

        repo.delete(&epic.id).await.unwrap();
        assert!(repo.get(&epic.id).await.unwrap().is_none());

        match rx.recv().await.unwrap() {
            DjinnEvent::EpicDeleted { id } => assert_eq!(id, epic.id),
            _ => panic!("expected EpicDeleted"),
        }
    }

    #[tokio::test]
    async fn encode_base36_roundtrip() {
        assert_eq!(encode_base36(0), "0000");
        assert_eq!(encode_base36(1_679_615), "zzzz");
        for s in [encode_base36(12345), encode_base36(999999 % 1_679_616)] {
            assert_eq!(s.len(), 4);
            assert!(s.chars().all(|c| c.is_ascii_alphanumeric() && !c.is_uppercase()));
        }
    }
}
