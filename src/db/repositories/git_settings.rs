//! Per-project git settings (CFG-03, GIT-08).
//!
//! Backed by the existing `settings` table using namespaced keys:
//!   - Per-project: `git:{project_id}:target_branch`
//!   - Global default: `git:global:target_branch`  (falls back to "main")

use crate::db::connection::{Database, OptionalExt};
use crate::error::Result;
use crate::models::git_settings::GitSettings;

pub struct GitSettingsRepository {
    db: Database,
}

impl GitSettingsRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
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

        self.db
            .call(move |conn| {
                // 1. Try project-specific setting.
                if let Some(branch) = conn
                    .query_row(
                        "SELECT value FROM settings WHERE key = ?1",
                        [&project_key],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                {
                    return Ok(GitSettings { target_branch: branch });
                }

                // 2. Try global default.
                if let Some(branch) = conn
                    .query_row(
                        "SELECT value FROM settings WHERE key = ?1",
                        [&global_key],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                {
                    return Ok(GitSettings { target_branch: branch });
                }

                // 3. Hard-coded fallback.
                Ok(GitSettings::default())
            })
            .await
    }

    /// Set the target branch for a specific project (GIT-08).
    pub async fn set_target_branch(&self, project_id: &str, branch: &str) -> Result<()> {
        let key = format!("git:{project_id}:target_branch");
        let branch = branch.to_owned();
        self.db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO settings (key, value, updated_at)
                     VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                     ON CONFLICT(key) DO UPDATE SET
                       value = excluded.value,
                       updated_at = excluded.updated_at",
                    [&key, &branch],
                )?;
                Ok(())
            })
            .await
    }

    /// Set the global default target branch (CFG-03).
    pub async fn set_global_target_branch(&self, branch: &str) -> Result<()> {
        let branch = branch.to_owned();
        self.db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO settings (key, value, updated_at)
                     VALUES ('git:global:target_branch', ?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                     ON CONFLICT(key) DO UPDATE SET
                       value = excluded.value,
                       updated_at = excluded.updated_at",
                    [&branch],
                )?;
                Ok(())
            })
            .await
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    #[tokio::test]
    async fn defaults_to_main_when_no_settings() {
        let db = test_helpers::create_test_db();
        let repo = GitSettingsRepository::new(db);

        let settings = repo.get("some-project-id").await.unwrap();
        assert_eq!(settings.target_branch, "main");
    }

    #[tokio::test]
    async fn project_override_takes_precedence() {
        let db = test_helpers::create_test_db();
        let repo = GitSettingsRepository::new(db);

        repo.set_target_branch("proj-123", "develop").await.unwrap();
        let settings = repo.get("proj-123").await.unwrap();
        assert_eq!(settings.target_branch, "develop");
    }

    #[tokio::test]
    async fn global_default_applies_when_no_project_override() {
        let db = test_helpers::create_test_db();
        let repo = GitSettingsRepository::new(db);

        repo.set_global_target_branch("develop").await.unwrap();
        let settings = repo.get("some-other-project").await.unwrap();
        assert_eq!(settings.target_branch, "develop");
    }

    #[tokio::test]
    async fn project_override_supersedes_global() {
        let db = test_helpers::create_test_db();
        let repo = GitSettingsRepository::new(db);

        repo.set_global_target_branch("develop").await.unwrap();
        repo.set_target_branch("proj-123", "feature-base").await.unwrap();

        // Project-specific override wins.
        let settings = repo.get("proj-123").await.unwrap();
        assert_eq!(settings.target_branch, "feature-base");

        // Other projects still get the global default.
        let other = repo.get("other-proj").await.unwrap();
        assert_eq!(other.target_branch, "develop");
    }

    #[tokio::test]
    async fn set_target_branch_upserts() {
        let db = test_helpers::create_test_db();
        let repo = GitSettingsRepository::new(db);

        repo.set_target_branch("proj", "v1").await.unwrap();
        repo.set_target_branch("proj", "v2").await.unwrap();

        let settings = repo.get("proj").await.unwrap();
        assert_eq!(settings.target_branch, "v2");
    }
}
