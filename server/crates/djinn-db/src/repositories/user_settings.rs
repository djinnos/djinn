use djinn_core::models::UserSettings;

use crate::Result;
use crate::database::Database;

pub struct UserSettingsRepository {
    db: Database,
}

impl UserSettingsRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Fetch the row for `user_id`, or `None` if the user has never set anything.
    /// Callers that want a defaults-baseline can use `get_or_default`.
    pub async fn get(&self, user_id: &str) -> Result<Option<UserSettings>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            UserSettings,
            "SELECT user_id, CAST(auto_approve_prs AS UNSIGNED) AS `auto_approve_prs!: bool`,
                    created_at, updated_at
               FROM user_settings WHERE user_id = ?",
            user_id,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Read-side convenience: never returns `None`. Callers downstream of
    /// pr_poller etc. don't want to branch on row existence — a missing row
    /// is semantically "all defaults off".
    pub async fn get_or_default(&self, user_id: &str) -> Result<UserSettings> {
        Ok(self
            .get(user_id)
            .await?
            .unwrap_or_else(|| UserSettings::defaults_for(user_id)))
    }

    /// Return user IDs of every user with `auto_approve_prs = true`, most
    /// recently updated first.
    ///
    /// Used by the PR poller's fallback approver path: when a task has no
    /// `created_by_user_id` (background-agent-spawned) we still want to
    /// auto-approve on behalf of an opted-in human. Phase 0 deployments
    /// typically have a single human user so this list has length 0 or 1.
    pub async fn list_users_with_auto_approve(&self) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query!(
            "SELECT user_id FROM user_settings \
             WHERE auto_approve_prs = TRUE \
             ORDER BY updated_at DESC",
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|r| r.user_id).collect())
    }

    /// Upsert the `auto_approve_prs` toggle. Returns the resulting row.
    pub async fn upsert_auto_approve_prs(
        &self,
        user_id: &str,
        value: bool,
    ) -> Result<UserSettings> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "INSERT INTO user_settings (user_id, auto_approve_prs, created_at, updated_at)
             VALUES (?, ?,
                     DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'),
                     DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'))
             ON DUPLICATE KEY UPDATE
                 auto_approve_prs = VALUES(auto_approve_prs),
                 updated_at = VALUES(updated_at)",
            user_id,
            value,
        )
        .execute(self.db.pool())
        .await?;
        self.get(user_id).await?.ok_or_else(|| {
            crate::Error::Internal(format!("user_settings row missing after upsert for {user_id}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    async fn seed_user(db: &Database, suffix: &str) -> String {
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let github_id: i64 = suffix.bytes().map(i64::from).sum::<i64>() + 1_000_000;
        let login = format!("user-{suffix}");
        sqlx::query!(
            "INSERT INTO users (id, github_id, github_login) VALUES (?, ?, ?)",
            id,
            github_id,
            login,
        )
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_user() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "none").await;
        let repo = UserSettingsRepository::new(db);
        assert!(repo.get(&user_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_or_default_returns_defaults_for_unknown_user() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "default").await;
        let repo = UserSettingsRepository::new(db);
        let s = repo.get_or_default(&user_id).await.unwrap();
        assert_eq!(s.user_id, user_id);
        assert!(!s.auto_approve_prs);
    }

    #[tokio::test]
    async fn upsert_creates_then_updates() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "upsert").await;
        let repo = UserSettingsRepository::new(db);

        let created = repo
            .upsert_auto_approve_prs(&user_id, true)
            .await
            .unwrap();
        assert!(created.auto_approve_prs);

        let updated = repo
            .upsert_auto_approve_prs(&user_id, false)
            .await
            .unwrap();
        assert!(!updated.auto_approve_prs);
        assert_eq!(updated.user_id, user_id);
    }

    #[tokio::test]
    async fn list_users_with_auto_approve_returns_only_opted_in_users() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_off = seed_user(&db, "off").await;
        let user_on_a = seed_user(&db, "on-a").await;
        let user_on_b = seed_user(&db, "on-b").await;
        let repo = UserSettingsRepository::new(db);

        repo.upsert_auto_approve_prs(&user_off, false).await.unwrap();
        repo.upsert_auto_approve_prs(&user_on_a, true).await.unwrap();
        repo.upsert_auto_approve_prs(&user_on_b, true).await.unwrap();

        let ids = repo.list_users_with_auto_approve().await.unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&user_on_a));
        assert!(ids.contains(&user_on_b));
        assert!(!ids.contains(&user_off));
    }

    #[tokio::test]
    async fn fk_cascade_drops_settings_when_user_removed() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "cascade").await;
        let repo = UserSettingsRepository::new(db.clone());
        repo.upsert_auto_approve_prs(&user_id, true).await.unwrap();

        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(&user_id)
            .execute(db.pool())
            .await
            .unwrap();

        assert!(repo.get(&user_id).await.unwrap().is_none());
    }
}
