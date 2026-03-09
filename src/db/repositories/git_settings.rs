//! Per-project git settings (CFG-03, GIT-08).
//!
//! Backed by the existing `settings` table using namespaced keys:
//!   - Per-project: `git:{project_id}:target_branch`
//!   - Global default: `git:global:target_branch`  (falls back to "main")

use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::error::Result;
use crate::events::DjinnEvent;
use crate::models::git_settings::GitSettings;

pub struct GitSettingsRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl GitSettingsRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
    }

    /// Load git settings for a project.
    ///
    /// Resolution order:
    ///   1. Per-project override (`git:{project_id}:target_branch`)
    ///   2. Global server default (`git:global:target_branch`)
    ///   3. Hard-coded default ("main")
    pub async fn get(&self, project_id: &str) -> Result<GitSettings> {
        let project_key = format!("git:{project_id}:target_branch");
        let global_key = "git:global:target_branch".to_owned();
        self.db.ensure_initialized().await?;

        if let Some(branch) =
            sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?1")
                .bind(&project_key)
                .fetch_optional(self.db.pool())
                .await?
        {
            return Ok(GitSettings {
                target_branch: branch,
            });
        }

        if let Some(branch) =
            sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?1")
                .bind(&global_key)
                .fetch_optional(self.db.pool())
                .await?
        {
            return Ok(GitSettings {
                target_branch: branch,
            });
        }

        Ok(GitSettings::default())
    }

    /// Set the target branch for a specific project (GIT-08).
    pub async fn set_target_branch(&self, project_id: &str, branch: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let key = format!("git:{project_id}:target_branch");
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at)
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(key) DO UPDATE SET
               value = excluded.value,
               updated_at = excluded.updated_at",
        )
        .bind(&key)
        .bind(branch)
        .execute(self.db.pool())
        .await?;
        let _ = self.events.send(DjinnEvent::GitSettingsUpdated {
            project_id: project_id.to_owned(),
            settings: GitSettings {
                target_branch: branch.to_owned(),
            },
        });
        Ok(())
    }

    /// Set the global default target branch (CFG-03).
    pub async fn set_global_target_branch(&self, branch: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at)
             VALUES ('git:global:target_branch', ?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(key) DO UPDATE SET
               value = excluded.value,
               updated_at = excluded.updated_at",
        )
        .bind(branch)
        .execute(self.db.pool())
        .await?;
        let _ = self.events.send(DjinnEvent::GitSettingsUpdated {
            project_id: "global".into(),
            settings: GitSettings {
                target_branch: branch.to_owned(),
            },
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn defaults_to_main_when_no_settings() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = GitSettingsRepository::new(db, tx);

        let settings = repo.get("some-project-id").await.unwrap();
        assert_eq!(settings.target_branch, "main");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn project_override_takes_precedence() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = GitSettingsRepository::new(db, tx);

        repo.set_target_branch("proj-123", "develop").await.unwrap();
        let settings = repo.get("proj-123").await.unwrap();
        assert_eq!(settings.target_branch, "develop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_default_applies_when_no_project_override() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = GitSettingsRepository::new(db, tx);

        repo.set_global_target_branch("develop").await.unwrap();
        let settings = repo.get("some-other-project").await.unwrap();
        assert_eq!(settings.target_branch, "develop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn project_override_supersedes_global() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = GitSettingsRepository::new(db, tx);

        repo.set_global_target_branch("develop").await.unwrap();
        repo.set_target_branch("proj-123", "feature-base")
            .await
            .unwrap();

        // Project-specific override wins.
        let settings = repo.get("proj-123").await.unwrap();
        assert_eq!(settings.target_branch, "feature-base");

        // Other projects still get the global default.
        let other = repo.get("other-proj").await.unwrap();
        assert_eq!(other.target_branch, "develop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_target_branch_upserts() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = GitSettingsRepository::new(db, tx);

        repo.set_target_branch("proj", "v1").await.unwrap();
        repo.set_target_branch("proj", "v2").await.unwrap();

        let settings = repo.get("proj").await.unwrap();
        assert_eq!(settings.target_branch, "v2");
    }
}
